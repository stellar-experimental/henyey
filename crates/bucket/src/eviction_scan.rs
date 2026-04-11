//! Shared incremental eviction scan logic.
//!
//! Provides a trait [`EvictionScanSource`] and free functions that implement
//! the incremental eviction scan algorithm. Both [`BucketList`] and
//! [`BucketListSnapshot`] implement this trait, eliminating the previous
//! duplication of ~200 lines of near-identical scan logic.

use std::collections::HashSet;

use stellar_xdr::curr::{LedgerEntry, LedgerKey, StateArchivalSettings};

use crate::bucket::Bucket;
use crate::bucket_list::BUCKET_LIST_LEVELS;
use crate::entry::{get_ttl_key, is_soroban_entry, is_temporary_entry, is_ttl_expired};
use crate::eviction::{
    update_starting_eviction_iterator, EvictionCandidate, EvictionIterator, EvictionIteratorExt,
    EvictionResult,
};
use crate::BucketEntry;
use crate::Result;
use henyey_common::protocol::MIN_SOROBAN_PROTOCOL_VERSION;

/// Abstraction over bucket access and entry lookup for eviction scanning.
///
/// Implemented by both `BucketList` (live) and `BucketListSnapshot` (read-only)
/// so that the scan algorithm can be shared.
pub(crate) trait EvictionScanSource {
    /// Returns the bucket at the given level and position (curr or snap).
    fn eviction_bucket(&self, level: usize, is_curr: bool) -> &Bucket;

    /// Looks up a ledger entry by key (for TTL and newest-version lookups).
    fn eviction_lookup(&self, key: &LedgerKey) -> Result<Option<LedgerEntry>>;
}

/// Perform an incremental eviction scan starting from the given iterator position.
///
/// This matches stellar-core's `scanForEviction` behavior:
/// - Scans entries starting from the iterator's current position
/// - Stops when `settings.eviction_scan_size` bytes have been scanned
/// - Updates the iterator to the new position
/// - Returns evicted entries (archived persistent + deleted temporary)
pub(crate) fn scan_for_eviction_incremental(
    source: &impl EvictionScanSource,
    mut iter: EvictionIterator,
    current_ledger: u32,
    settings: &StateArchivalSettings,
) -> Result<EvictionResult> {
    let mut result = EvictionResult {
        candidates: Vec::new(),
        end_iterator: iter.clone(),
        bytes_scanned: 0,
        scan_complete: false,
    };

    // Update iterator based on spills (reset offset if bucket received new data)
    update_starting_eviction_iterator(
        &mut iter,
        settings.starting_eviction_scan_level,
        current_ledger,
    );

    let start_iter = iter.clone();
    let mut bytes_remaining = settings.eviction_scan_size as u64;

    // Track keys we've seen to avoid duplicates (from shadowed entries)
    let mut seen_keys: HashSet<LedgerKey> = HashSet::new();

    loop {
        let level = iter.bucket_list_level as usize;
        if level >= BUCKET_LIST_LEVELS {
            result.scan_complete = true;
            break;
        }

        let bucket = source.eviction_bucket(level, iter.is_curr_bucket);

        let (_entries_scanned, bytes_used, finished_bucket) = scan_bucket_region(
            source,
            bucket,
            &mut iter,
            bytes_remaining,
            current_ledger,
            &mut result.candidates,
            &mut seen_keys,
        )?;

        result.bytes_scanned += bytes_used;

        if bytes_remaining > bytes_used {
            bytes_remaining -= bytes_used;
        } else {
            bytes_remaining = 0;
        }

        if bytes_remaining == 0 {
            result.scan_complete = true;
            break;
        }

        if finished_bucket {
            iter.advance_to_next_bucket(settings.starting_eviction_scan_level);

            // Check if we've completed a full cycle — only break when we return
            // to the exact starting bucket (same level AND same is_curr).
            if iter.bucket_list_level == start_iter.bucket_list_level
                && iter.is_curr_bucket == start_iter.is_curr_bucket
            {
                result.scan_complete = true;
                break;
            }
        }
    }

    result.end_iterator = iter;

    Ok(result)
}

/// Scan a region of a bucket for evictable entries (scan phase only).
///
/// Returns (entries_scanned, bytes_used, finished_bucket).
///
/// Collects ALL eligible candidates within the byte budget. The
/// `max_entries_to_archive` limit is NOT applied here — it's applied in the
/// resolution phase via `EvictionResult::resolve()`.
///
/// Uses byte-offset-aware iteration: for disk-backed buckets, seeks directly
/// to the start offset and reads record sizes from the file format.
fn scan_bucket_region(
    source: &impl EvictionScanSource,
    bucket: &Bucket,
    iter: &mut EvictionIterator,
    max_bytes: u64,
    current_ledger: u32,
    candidates: &mut Vec<EvictionCandidate>,
    seen_keys: &mut HashSet<LedgerKey>,
) -> Result<(usize, u64, bool)> {
    let mut entries_scanned = 0;
    let mut bytes_used = 0u64;

    // Skip buckets that predate Soroban; they cannot contain evictable entries.
    let bucket_protocol = bucket.protocol_version().unwrap_or(0);
    if bucket_protocol < MIN_SOROBAN_PROTOCOL_VERSION {
        iter.bucket_file_offset = 0;
        return Ok((entries_scanned, bytes_used, true));
    }

    let start_offset = iter.bucket_file_offset;

    for result in bucket.iter_from_offset_with_sizes(start_offset)? {
        let (entry, entry_size) = result?;
        bytes_used += entry_size;
        entries_scanned += 1;

        'process: {
            let live_entry = match &entry {
                BucketEntry::Liveentry(e) | BucketEntry::Initentry(e) => e,
                BucketEntry::Deadentry(_key) => {
                    // Parity: stellar-core ignores DEAD entries entirely
                    // in scanForEvictionInBucket — no key is recorded.
                    break 'process;
                }
                BucketEntry::Metaentry(_) => {
                    break 'process;
                }
            };

            if !is_soroban_entry(live_entry) {
                break 'process;
            }

            let key = henyey_common::entry_to_key(live_entry);

            if !seen_keys.insert(key.clone()) {
                break 'process;
            }

            let Some(ttl_key) = get_ttl_key(&key) else {
                break 'process;
            };

            let Some(ttl_entry) = source.eviction_lookup(&ttl_key)? else {
                break 'process;
            };

            let Some(is_expired) = is_ttl_expired(&ttl_entry, current_ledger) else {
                break 'process;
            };

            if !is_expired {
                break 'process;
            }

            // Entry is expired — collect as eviction candidate.
            // For persistent entries, archive the NEWEST version from the source
            // (not the potentially stale version from the older bucket being scanned).
            let is_temp = is_temporary_entry(live_entry);
            let entry_for_candidate = if !is_temp {
                if let Some(newest_entry) = source.eviction_lookup(&key)? {
                    newest_entry
                } else {
                    live_entry.clone()
                }
            } else {
                live_entry.clone()
            };

            candidates.push(EvictionCandidate {
                entry: entry_for_candidate,
                data_key: key,
                ttl_key,
                is_temporary: is_temp,
                position: EvictionIterator {
                    bucket_list_level: iter.bucket_list_level,
                    is_curr_bucket: iter.is_curr_bucket,
                    bucket_file_offset: start_offset + bytes_used,
                },
            });
        }

        if bytes_used >= max_bytes {
            break;
        }
    }

    let budget_exhausted = bytes_used >= max_bytes;
    iter.bucket_file_offset = start_offset + bytes_used;
    Ok((entries_scanned, bytes_used, !budget_exhausted))
}
