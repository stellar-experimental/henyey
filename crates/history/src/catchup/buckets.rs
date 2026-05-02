//! Bucket application logic for catchup: restoring bucket lists from HAS.

use crate::{archive_state::HistoryArchiveState, HistoryError, Result};
use henyey_bucket::{
    canonical_bucket_filename, Bucket, BucketList, HasNextState, HotArchiveBucketList,
};
use henyey_common::fs_utils::atomic_write_bytes;
use henyey_common::Hash256;
use std::collections::HashMap;

use tracing::{debug, info, warn};

use super::download::{block_on_async, download_bucket_from_archives};
use super::CatchupManager;

/// Current protocol version used for merge restarts.
pub(super) const CURRENT_PROTOCOL_VERSION: u32 = 25;

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

impl CatchupManager {
    /// Restart pending bucket merges from the HAS (without cache scanning).
    ///
    /// Cache initialization is handled by `LedgerManager::initialize()`.
    pub(super) async fn restart_merges(
        &self,
        bucket_list: &mut BucketList,
        hot_archive_bucket_list: &mut HotArchiveBucketList,
        checkpoint_seq: u32,
        live_next_states: &[HasNextState],
        hot_next_states: &[HasNextState],
    ) -> Result<()> {
        let protocol_version = CURRENT_PROTOCOL_VERSION;

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
                    metrics::counter!("stellar_history_verify_bucket_failure_total").increment(1);
                    warn!(
                        "Existing bucket file has wrong hash: expected {}, got {}",
                        hash,
                        bucket.hash()
                    );
                    let _ = std::fs::remove_file(&bucket_path);
                    // Fall through to download the bucket fresh
                } else {
                    metrics::counter!("stellar_history_verify_bucket_success_total").increment(1);
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
            let xdr_data = if let Some(data) = {
                let mut preloaded = preloaded_buckets.lock().unwrap();
                preloaded.remove(hash)
            } {
                was_preloaded = true;
                data
            } else {
                was_preloaded = false;
                // Download the bucket (blocking - we're in a sync context).
                match block_on_async(download_bucket_from_archives(archives.clone(), *hash)) {
                    Ok(data) => data,
                    Err(e) => {
                        metrics::counter!("stellar_history_download_bucket_failure_total")
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
            if let Err(e) = atomic_write_bytes(&bucket_path, &xdr_data) {
                if !was_preloaded {
                    // Persistence failure on the freshly-downloaded path is a
                    // terminal download-outcome failure — caller bails out.
                    metrics::counter!("stellar_history_download_bucket_failure_total").increment(1);
                }
                return Err(henyey_bucket::BucketError::NotFound(format!(
                    "failed to write bucket to disk: {}",
                    e
                )));
            }
            if !was_preloaded {
                // Successful fetch + persistence — terminal success.
                metrics::counter!("stellar_history_download_bucket_success_total").increment(1);
            }
            // Drop the in-memory XDR data before building the index to free memory
            drop(xdr_data);

            let bucket = Bucket::from_xdr_file_disk_backed(&bucket_path)?;

            // Verify hash matches
            if bucket.hash() != *hash {
                metrics::counter!("stellar_history_verify_bucket_failure_total").increment(1);
                // Clean up the bad file
                let _ = std::fs::remove_file(&bucket_path);
                return Err(henyey_bucket::BucketError::HashMismatch {
                    expected: hash.to_hex(),
                    actual: bucket.hash().to_hex(),
                });
            }
            metrics::counter!("stellar_history_verify_bucket_success_total").increment(1);

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

        // Build hot archive next states (even if no hot archive buckets, for return value).
        // Default to the correct number of levels so restart_merges_from_has gets valid input.
        let hot_next_states: Vec<HasNextState> = {
            let states: Vec<HasNextState> = has
                .hot_archive_next_states()
                .unwrap_or_default()
                .into_iter()
                .map(HasNextState::from)
                .collect();
            if states.is_empty() {
                vec![HasNextState::default(); henyey_bucket::HotArchiveBucketList::NUM_LEVELS]
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
                    let xdr_data: Option<Vec<u8>> = if let Some(data) = {
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
                            Ok(data) => Some(data),
                            Err(e) => {
                                metrics::counter!("stellar_history_download_bucket_failure_total")
                                    .increment(1);
                                return Err(e);
                            }
                        }
                    };

                    // If we downloaded data, save it to disk atomically. A
                    // persistence error here is the terminal download outcome
                    // for this bucket — emit failure before propagating.
                    if let Some(downloaded_data) = xdr_data {
                        if let Err(e) = atomic_write_bytes(&bucket_path, &downloaded_data) {
                            metrics::counter!("stellar_history_download_bucket_failure_total")
                                .increment(1);
                            return Err(henyey_bucket::BucketError::NotFound(format!(
                                "failed to write hot archive bucket to disk: {}",
                                e
                            )));
                        }
                        metrics::counter!("stellar_history_download_bucket_success_total")
                            .increment(1);
                    }

                    // Load hot archive bucket from disk eagerly — builds the index
                    // immediately so it's ready for lookups during live operation.
                    let bucket = HotArchiveBucket::from_xdr_file_disk_backed(&bucket_path)?;

                    // Verify hash matches (same as live bucket verification)
                    if bucket.hash() != *hash {
                        metrics::counter!("stellar_history_verify_bucket_failure_total")
                            .increment(1);
                        let _ = std::fs::remove_file(&bucket_path);
                        return Err(henyey_bucket::BucketError::HashMismatch {
                            expected: hash.to_hex(),
                            actual: bucket.hash().to_hex(),
                        });
                    }
                    metrics::counter!("stellar_history_verify_bucket_success_total").increment(1);

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
                    level.curr().hash().to_hex(),
                    level.snap_bucket().hash().to_hex()
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
}

#[cfg(test)]
mod tests {
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
}
