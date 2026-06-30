//! Bucket application logic for catchup: restoring bucket lists from HAS.

use crate::archive_state::{
    validate_bucket_list_structure, BucketLevelVersionInfo, HASBucketLevel,
};
use crate::{archive_state::HistoryArchiveState, HistoryError, Result};
use henyey_bucket::{
    canonical_bucket_filename, Bucket, BucketLevel, BucketList, HotArchiveBucketLevel,
    HotArchiveBucketList, PendingMergeState,
};
use henyey_common::fs_utils::atomic_write_bytes;
use henyey_common::Hash256;
use std::collections::HashMap;

use tracing::{debug, info, warn};

use super::download::{block_on_async, download_bucket_from_archives};
use super::CatchupManager;

/// Read the current process RSS (Resident Set Size) in MB from `/proc/self/status`.
/// Returns `None` on non-Linux platforms or if the file can't be read.
pub(super) fn rss_mb() -> Option<u64> {
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

/// Create a closure that loads live buckets from disk using streaming I/O.
///
/// This uses `Bucket::from_xdr_file_disk_backed()` which streams through the file
/// in two passes (hash computation + index building) without loading the entire file
/// Create a closure that loads live buckets from disk with hash verification.
///
/// Uses streaming I/O (disk-backed) for memory efficiency — O(index_size)
/// instead of O(file_size). Critical for mainnet where buckets can be tens of GB.
/// Verifies the loaded bucket's hash matches the expected hash to prevent
/// silent divergence from corrupted files.
pub(super) fn verified_bucket_loader(
    bucket_manager: std::sync::Arc<henyey_bucket::BucketManager>,
) -> impl FnMut(&Hash256) -> henyey_bucket::Result<Bucket> {
    move |hash: &Hash256| bucket_manager.load_bucket_for_merge(hash)
}

/// Create a closure that loads hot archive buckets from disk with hash verification.
///
/// Same memory optimization and hash verification as [`verified_bucket_loader`]
/// but for hot archive buckets which use `HotArchiveBucketEntry` format.
pub(super) fn verified_hot_archive_bucket_loader(
    bucket_manager: std::sync::Arc<henyey_bucket::BucketManager>,
) -> impl FnMut(&Hash256) -> henyey_bucket::Result<henyey_bucket::HotArchiveBucket> {
    move |hash: &Hash256| bucket_manager.load_hot_archive_bucket_for_merge(hash)
}

/// Build per-level version info for a restored live `BucketList`, suitable
/// for passing to [`validate_bucket_list_structure`].
///
/// The `levels` and `has_levels` slices are zipped — any length mismatch is
/// surfaced by the validator's own size check, not by an indexing panic, so
/// passing mismatched lengths still yields the validator's structured error.
///
/// Each bucket's protocol version is taken from the cached
/// `Bucket::protocol_version()` (O(1) after load); empty / metaentry-less
/// buckets map to version 0, matching stellar-core's
/// `bucket->isEmpty() ? 0 : bucket->getBucketVersion()` branch.
pub(crate) fn build_live_level_version_infos(
    levels: &[BucketLevel],
    has_levels: &[HASBucketLevel],
) -> Result<Vec<BucketLevelVersionInfo>> {
    let mut out = Vec::with_capacity(levels.len().max(has_levels.len()));
    for (level, has_level) in levels.iter().zip(has_levels.iter()) {
        let curr_version = level
            .curr
            .protocol_version()
            .map_err(|e| {
                HistoryError::CatchupFailed(format!(
                    "failed to read curr bucket protocol version: {}",
                    e
                ))
            })?
            .unwrap_or(0);
        let snap_version = level
            .snap
            .protocol_version()
            .map_err(|e| {
                HistoryError::CatchupFailed(format!(
                    "failed to read snap bucket protocol version: {}",
                    e
                ))
            })?
            .unwrap_or(0);
        out.push(BucketLevelVersionInfo {
            snap_version,
            curr_version,
            next: has_level.next.clone(),
        });
    }
    Ok(out)
}

/// Build per-level version info for a restored hot-archive bucket list,
/// suitable for passing to [`validate_bucket_list_structure`].
///
/// Same contract as [`build_live_level_version_infos`] but for hot-archive
/// levels — `HotArchiveBucket::get_protocol_version()` returns `Ok(0)` for
/// empty buckets directly, so no `unwrap_or(0)` is needed.
pub(super) fn build_hot_archive_level_version_infos(
    levels: &[HotArchiveBucketLevel],
    has_levels: &[HASBucketLevel],
) -> Result<Vec<BucketLevelVersionInfo>> {
    let mut out = Vec::with_capacity(levels.len().max(has_levels.len()));
    for (level, has_level) in levels.iter().zip(has_levels.iter()) {
        let curr_version = level.curr().get_protocol_version().map_err(|e| {
            HistoryError::CatchupFailed(format!(
                "failed to read hot-archive curr bucket protocol version: {}",
                e
            ))
        })?;
        let snap_version = level.snap_bucket().get_protocol_version().map_err(|e| {
            HistoryError::CatchupFailed(format!(
                "failed to read hot-archive snap bucket protocol version: {}",
                e
            ))
        })?;
        out.push(BucketLevelVersionInfo {
            snap_version,
            curr_version,
            next: has_level.next.clone(),
        });
    }
    Ok(out)
}

/// Translate a `validate_bucket_list_structure` error into the catchup error
/// class so the catchup orchestrator's existing handling kicks in unchanged.
fn map_validation_error(scope: &str, e: HistoryError) -> HistoryError {
    match e {
        HistoryError::VerificationFailed(msg) => {
            HistoryError::CatchupFailed(format!("invalid {} bucket list structure: {}", scope, msg))
        }
        other => other,
    }
}

impl CatchupManager {
    /// Restart pending bucket merges from the HAS (without cache scanning).
    ///
    /// Cache initialization is handled by `LedgerManager::initialize()`.
    pub(super) async fn restart_merges(
        &self,
        bucket_list: &mut BucketList,
        hot_archive_bucket_list: &mut HotArchiveBucketList,
        checkpoint_seq: u32,
        live_next_states: &[Option<PendingMergeState>],
        hot_next_states: &[Option<PendingMergeState>],
        protocol_version: u32,
    ) -> Result<()> {
        // Run live bucket list merge restarts in parallel (all levels concurrently).
        let load_bucket_for_merge = verified_bucket_loader(self.bucket_manager.clone());

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
            let load_hot_bucket_for_merge =
                verified_hot_archive_bucket_loader(self.bucket_manager.clone());
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
    pub(super) async fn apply_buckets(
        &self,
        has: &HistoryArchiveState,
        buckets: &[(Hash256, Vec<u8>)],
    ) -> Result<(
        BucketList,
        HotArchiveBucketList,
        Vec<Option<PendingMergeState>>,
        Vec<Option<PendingMergeState>>,
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

        // Helper to load a bucket - downloads on-demand, saves to disk, and caches
        let load_bucket = |hash: &Hash256| -> henyey_bucket::Result<Bucket> {
            // Sentinel hashes (zero and empty-file) don't need files on disk.
            if let Some(bucket) = Bucket::for_sentinel_hash(hash) {
                return Ok(bucket);
            }

            // Check cache first
            {
                let cache = bucket_cache.lock().unwrap();
                if let Some(bucket) = cache.get(hash) {
                    return Ok(bucket.clone());
                }
            }

            // Construct path for this bucket
            let bucket_path = bucket_dir.join(canonical_bucket_filename(hash));

            // Check if bucket already exists on disk as an XDR file.
            // Build the index eagerly so it's ready for lookups during live
            // ledger closing — deferring index construction to the first get()
            // would cause multi-second stalls when closing the first few ledgers.
            if bucket_path.exists() {
                debug!("Loading existing bucket {} from disk", hash);
                let bucket = Bucket::from_xdr_file_disk_backed(&bucket_path)?;
                // Verify hash matches (protects against corrupt files on disk)
                if bucket.hash() != *hash {
                    metrics::counter!(
                        "stellar_history_verify_bucket_failure_total",
                        "archive" => "local",
                    )
                    .increment(1);
                    warn!(
                        "Existing bucket file has wrong hash: expected {}, got {}",
                        hash,
                        bucket.hash()
                    );
                    let _ = std::fs::remove_file(&bucket_path);
                    // Fall through to download the bucket fresh
                } else {
                    metrics::counter!(
                        "stellar_history_verify_bucket_success_total",
                        "archive" => "local",
                    )
                    .increment(1);
                    let mut cache = bucket_cache.lock().unwrap();
                    cache.insert(*hash, bucket.clone());
                    return Ok(bucket);
                }
            }

            // Use preloaded bucket data if available, otherwise download.
            // The download path (block_on_async + download_bucket_from_archives)
            // bypasses `download_buckets`, so emit per-bucket download
            // success/failure here too — matching the pre-download stream.
            //
            // Stage E counter coverage: success is only emitted once the bytes
            // are safely persisted to disk. If the network fetch succeeds but
            // `atomic_write_bytes` fails (e.g., ENOSPC), we increment the
            // failure counter and bail out, so dashboards see one terminal
            // outcome per bucket.
            let was_preloaded;
            let download_archive_name: String;
            let xdr_data = if let Some(data) = {
                let mut preloaded = preloaded_buckets.lock().unwrap();
                preloaded.remove(hash)
            } {
                was_preloaded = true;
                download_archive_name = String::new();
                data
            } else {
                was_preloaded = false;
                // Download the bucket (blocking - we're in a sync context).
                match block_on_async(download_bucket_from_archives(archives.clone(), *hash)) {
                    Ok((data, archive_name)) => {
                        download_archive_name = archive_name;
                        data
                    }
                    Err(e) => {
                        // Use last archive as the failure label (all archives exhausted).
                        let last_name = archives
                            .last()
                            .map(|a| a.name().to_owned())
                            .unwrap_or_default();
                        metrics::counter!(
                            "stellar_history_download_bucket_failure_total",
                            "archive" => last_name,
                        )
                        .increment(1);
                        return Err(e);
                    }
                }
            };

            info!(
                "Downloaded bucket {}: {} bytes, saving to disk",
                hash,
                xdr_data.len()
            );

            // Save XDR data to disk first, then build the disk-backed bucket by
            // streaming through the file. This avoids holding the full file in memory
            // while also building the index — critical for multi-GB buckets on mainnet.
            //
            // NOTE (#3686): this `atomic_write_bytes` runs under
            // `block_on_async`/`block_in_place` (synchronous context, not a
            // tokio-worker-polled async fn), so it is intentionally NOT wrapped
            // in `spawn_blocking` — the worker is already released via
            // `block_in_place`. Only the async `.buffer_unordered` download
            // call sites (historywork/download.rs, catchup/download.rs) need the
            // off-worker wrap.
            if let Err(e) = atomic_write_bytes(&bucket_path, &xdr_data) {
                if !was_preloaded {
                    // Persistence failure on the freshly-downloaded path is a
                    // terminal download-outcome failure — caller bails out.
                    metrics::counter!(
                        "stellar_history_download_bucket_failure_total",
                        "archive" => download_archive_name.clone(),
                    )
                    .increment(1);
                }
                return Err(henyey_bucket::BucketError::NotFound(format!(
                    "failed to write bucket to disk: {}",
                    e
                )));
            }
            if !was_preloaded {
                // Successful fetch + persistence — terminal success.
                metrics::counter!(
                    "stellar_history_download_bucket_success_total",
                    "archive" => download_archive_name.clone(),
                )
                .increment(1);
            }
            // Drop the in-memory XDR data before building the index to free memory
            drop(xdr_data);

            let bucket = Bucket::from_xdr_file_disk_backed(&bucket_path)?;

            // Verify hash matches — attribute to the archive that served the
            // download, or "local" if the data was preloaded/provided.
            let verify_archive = if was_preloaded {
                "local".to_owned()
            } else {
                download_archive_name
            };
            if bucket.hash() != *hash {
                metrics::counter!(
                    "stellar_history_verify_bucket_failure_total",
                    "archive" => verify_archive,
                )
                .increment(1);
                // Clean up the bad file
                let _ = std::fs::remove_file(&bucket_path);
                return Err(henyey_bucket::BucketError::HashMismatch {
                    expected: hash.to_hex(),
                    actual: bucket.hash().to_hex(),
                });
            }
            metrics::counter!(
                "stellar_history_verify_bucket_success_total",
                "archive" => verify_archive,
            )
            .increment(1);

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
        let live_next_states: Vec<Option<PendingMergeState>> = has.live_next_states()?;

        for (level_idx, (curr, snap)) in live_hash_pairs.iter().enumerate() {
            info!(
                "HAS level {} hashes: curr={}, snap={}",
                level_idx, curr, snap
            );
        }

        // Restore the live bucket list with FutureBucket states
        let mut bucket_list = BucketList::restore_from_has_parallel(
            &live_hash_pairs,
            &live_next_states,
            load_bucket,
            self.restore_apply_fan_out,
        )
        .map_err(|e| {
            HistoryError::CatchupFailed(format!("Failed to restore live bucket list: {}", e))
        })?;
        bucket_list.set_bucket_dir(bucket_dir.to_path_buf());

        // Validate live bucket-list structure post-restore.
        //
        // This mirrors stellar-core's `containsValidBuckets` invocation in
        // `CatchupWork.cpp:217` between `DownloadBucketsWork` and
        // `ApplyBucketsWork`. Henyey runs the check inside `apply_buckets`
        // (the structurally analogous post-restore point), still before any
        // DB mutation so the observable effect is identical: a malformed HAS
        // aborts catchup with the same error class.
        let live_infos =
            build_live_level_version_infos(bucket_list.levels(), &has.current_buckets)?;
        validate_bucket_list_structure(&live_infos, BucketList::NUM_LEVELS)
            .map_err(|e| map_validation_error("live", e))?;

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

        // Build hot archive next states (even if no hot archive buckets, for return value).
        // Default to the correct number of levels so restart_merges_from_has gets valid input.
        let hot_next_states: Vec<Option<PendingMergeState>> = {
            let states: Vec<Option<PendingMergeState>> =
                has.hot_archive_next_states()?.unwrap_or_default();
            if states.is_empty() {
                vec![None; henyey_bucket::HotArchiveBucketList::NUM_LEVELS]
            } else {
                states
            }
        };

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

            // NOTE (#3686): the `atomic_write_bytes` calls inside this closure
            // (and the live-bucket loader above) run synchronously under the
            // `apply_buckets` `block_in_place` region — not on a tokio worker
            // polling an async fn — so they are intentionally NOT
            // `spawn_blocking`-wrapped. Only the async `.buffer_unordered`
            // download call sites need the off-worker wrap.
            let load_hot_archive_bucket =
                |hash: &Hash256| -> henyey_bucket::Result<HotArchiveBucket> {
                    // Short-circuit sentinel hashes (zero hash → empty bucket)
                    if let Some(bucket) = HotArchiveBucket::for_sentinel_hash(hash) {
                        return Ok(bucket);
                    }

                    // Check cache first (same hash can appear at multiple levels)
                    {
                        let cache = hot_archive_bucket_cache.lock().unwrap();
                        if let Some(bucket) = cache.get(hash) {
                            return Ok(bucket.clone());
                        }
                    }

                    // Check if we have the XDR data in the pre-downloaded cache
                    let bucket_path = bucket_dir_clone.join(canonical_bucket_filename(hash));

                    // Stage E counter coverage: download outcomes are only
                    // counted on the network-fetch fallback path. Success is
                    // emitted once bytes are persisted; persistence-error on
                    // the freshly-fetched path counts as a download failure.
                    let xdr_data: Option<(Vec<u8>, String)> = if let Some(data) = {
                        let mut preloaded = preloaded_buckets.lock().unwrap();
                        preloaded.remove(hash)
                    } {
                        // Save preloaded data to disk atomically, then load via streaming
                        atomic_write_bytes(&bucket_path, &data).map_err(|e| {
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
                        // Download if needed (shouldn't happen if download_buckets was called).
                        // Stage E: count as a per-bucket download outcome —
                        // matches the per-bucket counters emitted by
                        // `download_buckets()` so dashboards see one event
                        // per bucket file regardless of which path fetched it.
                        warn!(
                            "Hot archive bucket {} not found in cache, downloading",
                            hash
                        );
                        match block_on_async(download_bucket_from_archives(
                            archives_clone.clone(),
                            *hash,
                        )) {
                            Ok((data, _archive_name)) => Some((data, _archive_name)),
                            Err(e) => {
                                let last_name = archives_clone
                                    .last()
                                    .map(|a| a.name().to_owned())
                                    .unwrap_or_default();
                                metrics::counter!(
                                    "stellar_history_download_bucket_failure_total",
                                    "archive" => last_name,
                                )
                                .increment(1);
                                return Err(e);
                            }
                        }
                    };

                    // If we downloaded data, save it to disk atomically. A
                    // persistence error here is the terminal download outcome
                    // for this bucket — emit failure before propagating.
                    let verify_archive_name: String;
                    if let Some((downloaded_data, archive_name)) = xdr_data {
                        if let Err(e) = atomic_write_bytes(&bucket_path, &downloaded_data) {
                            metrics::counter!(
                                "stellar_history_download_bucket_failure_total",
                                "archive" => archive_name,
                            )
                            .increment(1);
                            return Err(henyey_bucket::BucketError::NotFound(format!(
                                "failed to write hot archive bucket to disk: {}",
                                e
                            )));
                        }
                        metrics::counter!(
                            "stellar_history_download_bucket_success_total",
                            "archive" => archive_name.clone(),
                        )
                        .increment(1);
                        verify_archive_name = archive_name;
                    } else {
                        verify_archive_name = "local".to_owned();
                    }

                    // Load hot archive bucket from disk eagerly — builds the index
                    // immediately so it's ready for lookups during live operation.
                    let bucket = HotArchiveBucket::from_xdr_file_disk_backed(&bucket_path)?;

                    // Verify hash matches (same as live bucket verification)
                    if bucket.hash() != *hash {
                        metrics::counter!(
                            "stellar_history_verify_bucket_failure_total",
                            "archive" => verify_archive_name,
                        )
                        .increment(1);
                        let _ = std::fs::remove_file(&bucket_path);
                        return Err(henyey_bucket::BucketError::HashMismatch {
                            expected: hash.to_hex(),
                            actual: bucket.hash().to_hex(),
                        });
                    }
                    metrics::counter!(
                        "stellar_history_verify_bucket_success_total",
                        "archive" => verify_archive_name,
                    )
                    .increment(1);

                    // Cache for reuse (same hash can appear at multiple levels)
                    {
                        let mut cache = hot_archive_bucket_cache.lock().unwrap();
                        cache.insert(*hash, bucket.clone());
                    }

                    Ok(bucket)
                };

            let hot_bucket_list = HotArchiveBucketList::restore_from_has_parallel(
                &hot_hash_pairs,
                &hot_next_states,
                load_hot_archive_bucket,
                self.restore_apply_fan_out,
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
                    level.curr().hash().to_hex(),
                    level.snap_bucket().hash().to_hex()
                );
            }

            // Validate hot-archive bucket-list structure post-restore.
            // Mirrors stellar-core's second `validateBucketListHelper` call
            // inside `containsValidBuckets` for the hot-archive list.
            let hot_has_levels = has.hot_archive_levels();
            let hot_infos =
                build_hot_archive_level_version_infos(hot_bucket_list.levels(), hot_has_levels)?;
            validate_bucket_list_structure(&hot_infos, HotArchiveBucketList::NUM_LEVELS)
                .map_err(|e| map_validation_error("hot archive", e))?;

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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::archive_state::HASBucketNext;
    use henyey_bucket::{BucketEntry, HOT_ARCHIVE_BUCKET_LIST_LEVELS};
    use stellar_xdr::{BucketMetadata, BucketMetadataExt};

    fn meta_entry(version: u32) -> BucketEntry {
        BucketEntry::Metaentry(BucketMetadata {
            ledger_version: version,
            ext: BucketMetadataExt::V0,
        })
    }

    fn bucket_with_version(version: u32) -> Bucket {
        Bucket::from_entries(vec![meta_entry(version)]).expect("metaentry-only bucket valid")
    }

    fn empty_has_level() -> HASBucketLevel {
        HASBucketLevel {
            curr: Hash256::ZERO.to_hex(),
            snap: Hash256::ZERO.to_hex(),
            next: HASBucketNext::default(),
        }
    }

    fn cleared_levels(n: usize) -> Vec<HASBucketLevel> {
        (0..n).map(|_| empty_has_level()).collect()
    }

    /// Stage E: pin the metric literals emitted from this module so a typo
    /// can't silently detach this crate from the central catalog.
    #[test]
    fn test_stage_e_buckets_metric_literals_present() {
        let src = include_str!("buckets.rs");
        for literal in &[
            "\"stellar_history_verify_bucket_success_total\"",
            "\"stellar_history_verify_bucket_failure_total\"",
            "\"stellar_history_download_bucket_success_total\"",
            "\"stellar_history_download_bucket_failure_total\"",
        ] {
            assert!(
                src.contains(literal),
                "expected metric literal {literal} in catchup/buckets.rs",
            );
        }
    }

    /// Stage E: verify and download counters must carry the `"archive"` label.
    #[test]
    fn test_stage_e_buckets_archive_label_present() {
        let src = include_str!("buckets.rs");
        let main_code = src.split("#[cfg(test)]").next().unwrap_or(src);
        for metric in &[
            "stellar_history_verify_bucket_success_total",
            "stellar_history_verify_bucket_failure_total",
            "stellar_history_download_bucket_success_total",
            "stellar_history_download_bucket_failure_total",
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
                     in catchup/buckets.rs",
                );
                search_from = idx + metric.len();
            }
            assert!(found_any, "metric {metric} not found in catchup/buckets.rs",);
        }
    }

    #[test]
    fn test_build_level_version_infos_pairs_buckets_and_has_levels() {
        let mut bucket_list = BucketList::new();
        // Level 0: curr=v24, snap=empty(0); higher levels: empty.
        let level0 = bucket_list.level_mut(0).unwrap();
        level0.set_curr(bucket_with_version(24));
        // snap stays empty.

        let mut has_levels = cleared_levels(BucketList::NUM_LEVELS);
        // Default `next` (state=CLEAR) is fine for level 0.
        has_levels[0].next = HASBucketNext::default();

        let infos = build_live_level_version_infos(bucket_list.levels(), &has_levels).unwrap();
        assert_eq!(infos.len(), BucketList::NUM_LEVELS);
        assert_eq!(infos[0].curr_version, 24);
        assert_eq!(infos[0].snap_version, 0);
        for level_info in infos.iter().skip(1) {
            assert_eq!(level_info.curr_version, 0);
            assert_eq!(level_info.snap_version, 0);
        }
    }

    #[test]
    fn test_build_live_then_validator_accepts_well_formed_list() {
        // All-empty bucket list: every level has version 0; the validator
        // skips the next-field check via `non_empty_seen=false` until j==0,
        // where the only check is `next.is_clear()` — satisfied by the
        // default `HASBucketNext`.
        let bucket_list = BucketList::new();
        let has_levels = cleared_levels(BucketList::NUM_LEVELS);
        let infos = build_live_level_version_infos(bucket_list.levels(), &has_levels).unwrap();
        validate_bucket_list_structure(&infos, BucketList::NUM_LEVELS)
            .expect("well-formed (all-empty) list must validate");
    }

    #[test]
    fn test_build_live_then_validator_rejects_non_monotonic_versions() {
        // Walking from deepest to level 0, snap then curr must be
        // non-decreasing. Put the LOWER version at a deeper level than
        // a HIGHER version → triggers the monotonicity error.
        //
        // Place v24 at the deepest curr and v20 at the next-deeper snap. Use
        // version 24 at the prev-snap (level deep-1 snap) so the validator's
        // `prev_snap_version >= FIRST_PROTOCOL_SHADOWS_REMOVED` branch
        // accepts the cleared `next` field; the failure must come from the
        // version monotonicity check, not the next-field check.
        let mut bucket_list = BucketList::new();
        let deep = BucketList::NUM_LEVELS - 1;
        bucket_list
            .level_mut(deep)
            .unwrap()
            .set_curr(bucket_with_version(24));
        bucket_list
            .level_mut(deep - 1)
            .unwrap()
            .set_snap(bucket_with_version(20));

        let has_levels = cleared_levels(BucketList::NUM_LEVELS);
        let infos = build_live_level_version_infos(bucket_list.levels(), &has_levels).unwrap();
        let err = validate_bucket_list_structure(&infos, BucketList::NUM_LEVELS)
            .expect_err("non-monotonic versions must be rejected");
        match err {
            HistoryError::VerificationFailed(msg) => {
                assert!(
                    msg.contains("incompatible bucket versions"),
                    "unexpected error: {msg}"
                );
            }
            other => panic!("expected VerificationFailed, got {other:?}"),
        }
    }

    #[test]
    fn test_build_live_then_validator_rejects_level0_next_not_clear() {
        // All-empty list (so prev-level next-field checks are skipped via
        // `non_empty_seen=false`) but with a non-clear `next` at level 0 →
        // the level-0 next-clear check fires unconditionally.
        let bucket_list = BucketList::new();
        let mut has_levels = cleared_levels(BucketList::NUM_LEVELS);
        has_levels[0].next = HASBucketNext {
            state: 1, // FB_HASH_OUTPUT
            output: Some(Hash256::ZERO.to_hex()),
            ..HASBucketNext::default()
        };

        let infos = build_live_level_version_infos(bucket_list.levels(), &has_levels).unwrap();
        let err = validate_bucket_list_structure(&infos, BucketList::NUM_LEVELS)
            .expect_err("non-clear next at level 0 must be rejected");
        match err {
            HistoryError::VerificationFailed(msg) => {
                assert!(
                    msg.contains("next must be clear at level 0"),
                    "unexpected error: {msg}"
                );
            }
            other => panic!("expected VerificationFailed, got {other:?}"),
        }
    }

    #[test]
    fn test_build_hot_archive_then_validator_rejects_level0_next_not_clear() {
        // All-empty hot archive list — level 0 next-clear check fires
        // unconditionally regardless of versions (matches validator semantics).
        let hot_list = HotArchiveBucketList::new();

        let mut has_levels = cleared_levels(HOT_ARCHIVE_BUCKET_LIST_LEVELS);
        has_levels[0].next = HASBucketNext {
            state: 1,
            output: Some(Hash256::ZERO.to_hex()),
            ..HASBucketNext::default()
        };

        let infos = build_hot_archive_level_version_infos(hot_list.levels(), &has_levels).unwrap();
        // Sanity: helper produced one info per level, all version 0.
        assert_eq!(infos.len(), HotArchiveBucketList::NUM_LEVELS);
        assert!(infos
            .iter()
            .all(|i| i.curr_version == 0 && i.snap_version == 0));

        let err = validate_bucket_list_structure(&infos, HotArchiveBucketList::NUM_LEVELS)
            .expect_err("non-clear next at level 0 must be rejected for hot archive");
        match err {
            HistoryError::VerificationFailed(msg) => {
                assert!(
                    msg.contains("next must be clear at level 0"),
                    "unexpected error: {msg}"
                );
            }
            other => panic!("expected VerificationFailed, got {other:?}"),
        }
    }

    #[test]
    fn test_map_validation_error_translates_to_catchup_failed() {
        let v = HistoryError::VerificationFailed("boom".to_string());
        match map_validation_error("live", v) {
            HistoryError::CatchupFailed(msg) => {
                assert!(msg.contains("invalid live bucket list structure"));
                assert!(msg.contains("boom"));
            }
            other => panic!("expected CatchupFailed, got {other:?}"),
        }
        // Non-VerificationFailed errors pass through unchanged.
        let other = HistoryError::CatchupFailed("untouched".to_string());
        match map_validation_error("hot archive", other) {
            HistoryError::CatchupFailed(msg) => assert_eq!(msg, "untouched"),
            other => panic!("unexpected error variant: {other:?}"),
        }
    }

    // ----------------------------------------------------------------------
    // #3268 — restore_apply_fan_out wired into the cold-catchup restore path.
    //
    // These tests exercise the seam that `apply_buckets` now uses:
    // `BucketList::restore_from_has_parallel(.., self.restore_apply_fan_out)`
    // (swapped from the un-capped, sequential `restore_from_has`).
    // ----------------------------------------------------------------------

    use crate::archive_state::HistoryArchiveState;
    use henyey_bucket::BucketManager;
    use henyey_db::Database;
    use stellar_xdr::{
        AccountEntry, AccountEntryExt, AccountId, LedgerEntry, LedgerEntryData, LedgerEntryExt,
        PublicKey, SequenceNumber, String32, Thresholds, Uint256,
    };

    fn account_liveentry(seed: u8) -> BucketEntry {
        BucketEntry::Liveentry(LedgerEntry {
            last_modified_ledger_seq: 1,
            data: LedgerEntryData::Account(AccountEntry {
                account_id: AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([seed; 32]))),
                balance: seed as i64 * 100,
                seq_num: SequenceNumber(1),
                num_sub_entries: 0,
                inflation_dest: None,
                flags: 0,
                home_domain: String32::default(),
                thresholds: Thresholds([1, 0, 0, 0]),
                signers: Vec::new().try_into().unwrap(),
                ext: AccountEntryExt::V0,
            }),
            ext: LedgerEntryExt::V0,
        })
    }

    /// A distinct, well-versioned live bucket: a leading metaentry (protocol
    /// `version`) followed by a single account liveentry keyed on `seed`, so
    /// every bucket has a unique hash while sharing the same protocol version
    /// (keeps the bucket-list structure validation happy).
    fn versioned_bucket(version: u32, seed: u8) -> Bucket {
        Bucket::from_entries(vec![meta_entry(version), account_liveentry(seed)])
            .expect("metaentry + liveentry bucket valid")
    }

    /// Build a HAS whose live bucket list places distinct curr+snap buckets on
    /// the shallowest `n` levels (all protocol v24, contiguous from level 0 with
    /// every `next` clear, so the structure validator accepts it), no hot
    /// archive. Returns the HAS plus the preloaded `(hash, xdr_bytes)` list that
    /// `apply_buckets` consumes (mirroring the post-`download_buckets` state).
    ///
    /// Each level carries TWO distinct non-empty buckets (curr + snap) so that
    /// `n` non-empty levels yield `n` concurrent restore worker threads (one per
    /// level), exercising the fan-out cap.
    fn build_live_has_fixture(n: usize) -> (HistoryArchiveState, Vec<(Hash256, Vec<u8>)>) {
        assert!(n >= 1 && n <= BucketList::NUM_LEVELS);
        let mut levels = cleared_levels(BucketList::NUM_LEVELS);
        let mut preloaded: Vec<(Hash256, Vec<u8>)> = Vec::new();
        let mut seed: u8 = 1;
        for level in levels.iter_mut().take(n) {
            let curr = versioned_bucket(24, seed);
            seed += 1;
            let snap = versioned_bucket(24, seed);
            seed += 1;
            let (hc, hs) = (curr.hash(), snap.hash());
            level.curr = hc.to_hex();
            level.snap = hs.to_hex();
            preloaded.push((hc, curr.to_xdr_bytes().expect("serialize bucket xdr")));
            preloaded.push((hs, snap.to_xdr_bytes().expect("serialize bucket xdr")));
        }
        let has = HistoryArchiveState::new_for_testing(64, levels);
        (has, preloaded)
    }

    /// Construct a `CatchupManager` over a fresh temp bucket dir + in-memory DB
    /// with no archives (the preloaded path never hits the network).
    fn catchup_manager_for(dir: &std::path::Path) -> CatchupManager {
        let bucket_manager =
            BucketManager::new(dir.to_path_buf()).expect("bucket manager over temp dir");
        let db = Database::open_in_memory().expect("in-memory db");
        CatchupManager::new(Vec::new(), bucket_manager, db)
    }

    /// Byte-identical restored state across fan-out values on the cold-catchup
    /// path: capping concurrency must NOT change the restored `bucketListHash`.
    #[tokio::test]
    async fn test_apply_buckets_byte_identical_across_fan_out() {
        let (has, preloaded) = build_live_has_fixture(4);

        let mut hashes = Vec::new();
        for fan_out in [None, Some(1usize), Some(2), Some(8)] {
            let dir = tempfile::tempdir().expect("tempdir");
            let mut mgr = catchup_manager_for(dir.path());
            mgr.set_restore_apply_fan_out(fan_out);
            let (live_bl, hot_bl, _, _) = mgr
                .apply_buckets(&has, &preloaded)
                .await
                .expect("apply_buckets succeeds");
            hashes.push((fan_out, live_bl.hash(), hot_bl.hash()));
        }

        let (_, live0, hot0) = hashes[0];
        for (fan_out, live, hot) in &hashes {
            assert_eq!(
                *live, live0,
                "live bucketListHash differs for fan_out={fan_out:?} — capping \
                 cold-catchup concurrency must not change the restored state"
            );
            assert_eq!(
                *hot, hot0,
                "hot-archive bucketListHash differs for fan_out={fan_out:?}"
            );
        }
    }

    /// #3282 fan-out rule-in/out DIAGNOSTIC: the FULL ledger-header hash
    /// (not just the `bucketListHash`) must be identical across fan-out values
    /// `{None, 1, 2, 6}`.
    ///
    /// **Why this is here:** build `384b4f6e` (the #3282 incident build) is the
    /// #3269 fan-out-wiring commit, and `set_restore_apply_fan_out` is wired
    /// into the catchup manager. #3269 only proved **bucketListHash** equality
    /// across fan-out; the *full* ledger-header hash also folds in the
    /// `bucket_list_hash` field (and, downstream, tx-result / upgrades / idpool
    /// — all archive-sourced and therefore fan-out-independent). This test
    /// rules fan-out IN or OUT as a contributor to the #3282 divergence by
    /// hashing a representative `LedgerHeader` whose only fan-out-dependent
    /// field — `bucket_list_hash` — is set from the restored live bucket list.
    ///
    /// **Diagnostic, not a blocker:** if this is GREEN, fan-out is exonerated
    /// for #3282 (consistent with the timeline evidence that #2886/#2931
    /// predate ALL fan-out work). If it ever goes RED, that is a SECOND,
    /// separately-tracked parallel-restore-determinism bug (#3234 lineage) —
    /// file a follow-up, do NOT widen the #3282 PR into a restore-determinism
    /// fix.
    #[tokio::test]
    async fn test_apply_buckets_full_header_hash_identical_across_fan_out() {
        use stellar_xdr::{Hash, LedgerHeader};

        // Six distinct non-empty live levels so unbounded fan-out overlaps
        // multiple concurrent restore workers (maximizes any ordering
        // sensitivity the diagnostic would surface).
        let (has, preloaded) = build_live_has_fixture(6);

        let mut header_hashes = Vec::new();
        let mut hot_hashes = Vec::new();
        for fan_out in [None, Some(1usize), Some(2), Some(6)] {
            let dir = tempfile::tempdir().expect("tempdir");
            let mut mgr = catchup_manager_for(dir.path());
            mgr.set_restore_apply_fan_out(fan_out);
            let (live_bl, hot_bl, _, _) = mgr
                .apply_buckets(&has, &preloaded)
                .await
                .expect("apply_buckets succeeds");

            // Build a representative ledger header whose only
            // fan-out-dependent field is the restored live bucketListHash, and
            // hash the WHOLE header (canonical XDR hash), not just the BL hash.
            // The remaining header fields are archive-sourced constants, so any
            // fan-out-induced divergence can ONLY enter via bucket_list_hash.
            let header = LedgerHeader {
                ledger_seq: has.current_ledger,
                bucket_list_hash: Hash(live_bl.hash().0),
                ..LedgerHeader::default()
            };
            header_hashes.push((fan_out, Hash256::hash_xdr(&header)));
            hot_hashes.push((fan_out, hot_bl.hash()));
        }

        let (_, header0) = header_hashes[0];
        for (fan_out, hh) in &header_hashes {
            assert_eq!(
                *hh, header0,
                "FULL ledger-header hash differs for fan_out={fan_out:?} — \
                 parallel restore concurrency must not change the derived \
                 ledger header (if RED: file a separate #3234-lineage \
                 follow-up; do NOT block #3282 on it)"
            );
        }
        let (_, hot0) = hot_hashes[0];
        for (fan_out, hot) in &hot_hashes {
            assert_eq!(
                *hot, hot0,
                "hot-archive bucketListHash differs for fan_out={fan_out:?}"
            );
        }
    }

    /// Default cap (`None`) reproduces the un-capped restore exactly: the
    /// restored `bucketListHash` equals a direct sequential `restore_from_has`
    /// over the same HAS (the pre-#3268 behavior). No-op-at-default guarantee.
    #[tokio::test]
    async fn test_apply_buckets_default_unbounded_matches_current() {
        let (has, preloaded) = build_live_has_fixture(3);

        // Cold-catchup path with the DEFAULT cap (None — what a default config
        // yields via restore_apply_fan_out_cap()).
        let dir = tempfile::tempdir().expect("tempdir");
        let mgr = catchup_manager_for(dir.path());
        assert_eq!(
            mgr.restore_apply_fan_out, None,
            "CatchupManager must default to unbounded fan-out"
        );
        let (live_bl, _, _, _) = mgr
            .apply_buckets(&has, &preloaded)
            .await
            .expect("apply_buckets succeeds at default cap");

        // Independent reference: sequential restore_from_has over the same HAS.
        let by_hash: HashMap<Hash256, Bucket> = preloaded
            .iter()
            .map(|(h, bytes)| {
                (
                    *h,
                    Bucket::from_xdr_bytes(bytes).expect("decode preloaded bucket"),
                )
            })
            .collect();
        let live_hash_pairs = has.bucket_hash_pairs();
        let live_next_states = has.live_next_states().expect("live next states");
        let ref_loader = |h: &Hash256| -> henyey_bucket::Result<Bucket> {
            if let Some(b) = Bucket::for_sentinel_hash(h) {
                return Ok(b);
            }
            by_hash
                .get(h)
                .cloned()
                .ok_or_else(|| henyey_bucket::BucketError::NotFound(h.to_hex()))
        };
        let ref_bl =
            BucketList::restore_from_has(&live_hash_pairs, &live_next_states, ref_loader).unwrap();

        assert_eq!(
            live_bl.hash(),
            ref_bl.hash(),
            "default (None) cold-catchup restore must equal the sequential \
             restore_from_has result (no-op at default)"
        );
    }

    /// The cold-catchup restore honors the fan-out cap.
    ///
    /// This drives the exact seam `apply_buckets` now uses — feeding the
    /// `CatchupManager`'s stored `restore_apply_fan_out` into
    /// `restore_from_has_parallel` — with a load-instrumented loader that
    /// records the max number of concurrent in-flight loads.
    ///
    /// **Why this fails on `origin/main`:** there, `apply_buckets` calls the
    /// sequential `restore_from_has`, which (a) has no `fan_out` parameter, so
    /// `CatchupManager` has no field to store a cap and `set_restore_apply_fan_out`
    /// does not exist (compile error), and (b) loads buckets strictly one at a
    /// time, so concurrency can never reach 2. Using `Some(2)` (not `Some(1)`)
    /// makes the failure unambiguous: main's max concurrency is always 1.
    #[test]
    fn test_apply_buckets_respects_fan_out_cap() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc as StdArc;

        // Stored cap flows through the setter exactly as catchup_impl wires it.
        let dir = tempfile::tempdir().unwrap();
        let mut mgr = catchup_manager_for(dir.path());
        mgr.set_restore_apply_fan_out(Some(2));
        assert_eq!(mgr.restore_apply_fan_out, Some(2), "setter round-trips");

        // Six distinct non-empty levels so unbounded would overlap >2 loads.
        let (has, preloaded) = build_live_has_fixture(6);
        let by_hash: HashMap<Hash256, Bucket> = preloaded
            .iter()
            .map(|(h, bytes)| (*h, Bucket::from_xdr_bytes(bytes).unwrap()))
            .collect();
        let live_hash_pairs = has.bucket_hash_pairs();
        let live_next_states = has.live_next_states().unwrap();

        let current = StdArc::new(AtomicUsize::new(0));
        let max_seen = StdArc::new(AtomicUsize::new(0));
        let current_c = StdArc::clone(&current);
        let max_seen_c = StdArc::clone(&max_seen);

        let loader = move |h: &Hash256| -> henyey_bucket::Result<Bucket> {
            if let Some(b) = Bucket::for_sentinel_hash(h) {
                return Ok(b);
            }
            let now = current_c.fetch_add(1, Ordering::SeqCst) + 1;
            max_seen_c.fetch_max(now, Ordering::SeqCst);
            std::thread::sleep(std::time::Duration::from_millis(20));
            let b = by_hash.get(h).cloned().expect("known hash");
            current_c.fetch_sub(1, Ordering::SeqCst);
            Ok(b)
        };

        // Exactly what apply_buckets does: feed the manager's stored cap.
        let bl = BucketList::restore_from_has_parallel(
            &live_hash_pairs,
            &live_next_states,
            loader,
            mgr.restore_apply_fan_out,
        )
        .unwrap();

        assert_eq!(current.load(Ordering::SeqCst), 0, "all loads completed");
        let max = max_seen.load(Ordering::SeqCst);
        assert!(
            max <= 2,
            "max concurrent cold-catchup loads {max} exceeded the fan_out cap 2"
        );
        assert_eq!(
            max, 2,
            "concurrency must actually reach 2 (proving the cap is threaded and \
             parallelism active) — unreachable on main's sequential restore"
        );
        // Sanity: the curr buckets were materialized into the right levels
        // (preloaded is [curr0, snap0, curr1, snap1, ...]).
        for (i, pair) in preloaded.chunks(2).enumerate() {
            assert_eq!(bl.levels()[i].curr.hash(), pair[0].0);
            assert_eq!(bl.levels()[i].snap.hash(), pair[1].0);
        }
    }
}
