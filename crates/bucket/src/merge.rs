//! Bucket merging implementation.
//!
//! Merging is the fundamental operation that maintains bucket list integrity.
//! When buckets are merged, entries from a newer bucket "shadow" entries from
//! an older bucket with the same key.
//!
//! # Merge Operation
//!
//! The merge operation combines two sorted buckets into one, applying shadowing:
//!
//! ```text
//! Old Bucket: [A=1, C=3, E=5]
//! New Bucket: [B=2, C=30, D=4]
//! Merged:     [A=1, B=2, C=30, D=4, E=5]
//!            (C from new shadows C from old)
//! ```
//!
//! # CAP-0020 INITENTRY Semantics
//!
//! The `Init` entry type (introduced in protocol 11) enables optimizations:
//!
//! - `INIT + DEAD` = Both entries are annihilated (nothing output)
//! - `DEAD + INIT` = Becomes `LIVE` (recreation cancels tombstone)
//! - `INIT + LIVE` = Becomes `INIT` with new value (preserves init status)
//!
//! This prevents tombstones from accumulating when entries are created and
//! deleted within the same merge window.
//!
//! # Dead Entry Handling
//!
//! The `keep_dead_entries` parameter controls tombstone behavior:
//!
//! - `true`: Keep dead entries (needed at lower levels where they may still shadow)
//! - `false`: Remove dead entries (safe at higher levels, reduces bucket size)
//!
//! # Normalization
//!
//! When entries cross level boundaries during a spill, `Init` entries are
//! "normalized" to `Live`. This is because the init status is only relevant
//! within the merge window where the entry was created.

use std::cmp::Ordering;
use std::collections::HashMap;
use std::fs::File;
use std::path::Path;
use std::sync::Arc;

use henyey_common::Hash256;
use sha2::{Digest, Sha256};
use stellar_xdr::curr::{BucketMetadata, BucketMetadataExt, LedgerKey, Limits, WriteXdr};

use crate::bucket::{Bucket, BucketIter};
use crate::entry::{compare_keys, BucketEntry, BucketEntryExt};
use crate::metrics::{EntryCountType, MergeCounters};
use crate::{protocol_version_starts_from, BucketError, ProtocolVersion, Result};

/// Record an entry type in the merge counters.
fn record_entry_type(counters: Option<&MergeCounters>, entry: &BucketEntry) {
    if let Some(c) = counters {
        match entry {
            BucketEntry::Metaentry(_) => c.record_new_entry(EntryCountType::Meta),
            BucketEntry::Initentry(_) => c.record_new_entry(EntryCountType::Init),
            BucketEntry::Liveentry(_) => c.record_new_entry(EntryCountType::Live),
            BucketEntry::Deadentry(_) => c.record_new_entry(EntryCountType::Dead),
        }
    }
}

/// Merge two buckets into a new bucket.
///
/// The `new_bucket` contains newer entries that shadow entries in `old_bucket`.
///
/// # Arguments
/// * `old_bucket` - The older bucket (entries may be shadowed)
/// * `new_bucket` - The newer bucket (entries take precedence)
/// * `keep_dead_entries` - Whether to keep dead entries in the output
/// * `max_protocol_version` - Maximum protocol version allowed for the merge
///
/// # Merge Semantics
/// - When keys match, the newer entry wins
/// - Dead entries shadow live entries (the entry is deleted)
/// - If `keep_dead_entries` is false and a dead entry shadows nothing, it's removed
/// - Init entries are converted to Live entries when crossing level boundaries
///
/// Note: This wrapper always normalizes INIT→LIVE for backward compatibility with
/// tests. For bucket list operations, use `merge_buckets_with_options`.
pub fn merge_buckets(
    old_bucket: &Bucket,
    new_bucket: &Bucket,
    keep_dead_entries: bool,
    max_protocol_version: u32,
) -> Result<Bucket> {
    merge_buckets_with_options(
        old_bucket,
        new_bucket,
        keep_dead_entries,
        max_protocol_version,
        true,
    )
}

/// Merge two buckets into a new bucket with explicit normalization control.
///
/// # Arguments
/// * `old_bucket` - The older bucket (entries may be shadowed)
/// * `new_bucket` - The newer bucket (entries take precedence)
/// * `keep_dead_entries` - Whether to keep dead entries in the output
/// * `max_protocol_version` - Maximum protocol version allowed for the merge
/// * `normalize_init_entries` - Whether to convert INIT entries to LIVE entries.
///   Set to true when merging spills (crossing level boundaries), false for
///   same-level merges (e.g., at level 0).
///
/// # Merge Semantics
/// - When keys match, the newer entry wins
/// - Dead entries shadow live entries (the entry is deleted)
/// - If `keep_dead_entries` is false and a dead entry shadows nothing, it's removed
/// - Init entries are converted to Live entries only if `normalize_init_entries` is true
pub fn merge_buckets_with_options(
    old_bucket: &Bucket,
    new_bucket: &Bucket,
    keep_dead_entries: bool,
    max_protocol_version: u32,
    normalize_init_entries: bool,
) -> Result<Bucket> {
    merge_with_shadows_impl(
        old_bucket,
        new_bucket,
        keep_dead_entries,
        max_protocol_version,
        normalize_init_entries,
        &[],
        false,
        None,
    )
}

/// Core merge implementation with integrated shadow checking.
///
/// Performs a single-pass two-pointer merge with inline shadow filtering.
/// When `shadow_buckets` is empty, shadow checking is a no-op. This matches
/// stellar-core's `BucketOutputIterator::maybePut()` pattern where shadow
/// checking is integrated into the output path rather than as a post-processing
/// filter.
#[allow(clippy::too_many_arguments)]
fn merge_with_shadows_impl(
    old_bucket: &Bucket,
    new_bucket: &Bucket,
    keep_dead_entries: bool,
    max_protocol_version: u32,
    normalize_init_entries: bool,
    shadow_buckets: &[Bucket],
    keep_shadowed_lifecycle_entries: bool,
    counters: Option<&MergeCounters>,
) -> Result<Bucket> {
    // Note: We intentionally do NOT use fast paths for empty buckets here.
    // stellar-core always goes through the full merge process even when
    // one input is empty. This is important because:
    // 1. The output bucket gets new metadata (protocol version)
    // 2. The bucket hash includes metadata
    // 3. Returning input unchanged would preserve old metadata and potentially wrong hash
    //
    // The only optimization is when BOTH inputs are empty.
    if new_bucket.is_empty() && old_bucket.is_empty() {
        return Ok(Bucket::empty());
    }

    // Use streaming merge: iterate one entry at a time from each bucket.
    // For in-memory buckets this iterates over the slice; for disk-backed
    // buckets this streams from disk via BufReader (O(1) memory per input).
    let mut old_iter = old_bucket.iter();
    let mut new_iter = new_bucket.iter();

    // Extract metadata from the first entries of each bucket.
    let mut old_meta: Option<BucketMetadata> = None;
    let mut new_meta: Option<BucketMetadata> = None;
    let mut old_current = advance_skip_metadata(&mut old_iter, &mut old_meta);
    let mut new_current = advance_skip_metadata(&mut new_iter, &mut new_meta);

    tracing::trace!(
        old_hash = %old_bucket.hash(),
        new_hash = %new_bucket.hash(),
        old_has_meta = old_meta.is_some(),
        new_has_meta = new_meta.is_some(),
        "merge_buckets starting (streaming)"
    );

    let (_, output_meta) =
        build_output_metadata(old_meta.as_ref(), new_meta.as_ref(), max_protocol_version)?;

    let mut merged = Vec::new();

    // Create shadow cursors for inline shadow checking.
    // For protocol >= 12 (V12, shadows removed), shadow_buckets is always
    // empty, so no cursors are created.
    let mut shadow_cursors: Vec<ShadowCursor<'_>> =
        shadow_buckets.iter().map(ShadowCursor::new).collect();

    if let Some(ref meta) = output_meta {
        merged.push(meta.clone());
    }

    // Two-pointer merge using streaming iterators
    while old_current.is_some() && new_current.is_some() {
        let old_entry = old_current.as_ref().unwrap();
        let new_entry = new_current.as_ref().unwrap();

        let old_key = old_entry.key();
        let new_key = new_entry.key();

        match (old_key, new_key) {
            (Some(ref ok), Some(ref nk)) => {
                match compare_keys(ok, nk) {
                    Ordering::Less => {
                        // Old entry comes first, no shadow.
                        if should_keep_entry(old_entry, keep_dead_entries) {
                            record_entry_type(counters, old_entry);
                            maybe_put(
                                old_entry.clone(),
                                &mut shadow_cursors,
                                keep_shadowed_lifecycle_entries,
                                &mut merged,
                                counters,
                            );
                        }
                        old_current = next_non_meta(&mut old_iter);
                    }
                    Ordering::Greater => {
                        // New entry comes first
                        if should_keep_entry(new_entry, keep_dead_entries) {
                            let entry =
                                maybe_normalize_entry(new_entry.clone(), normalize_init_entries);
                            record_entry_type(counters, &entry);
                            maybe_put(
                                entry,
                                &mut shadow_cursors,
                                keep_shadowed_lifecycle_entries,
                                &mut merged,
                                counters,
                            );
                        }
                        new_current = next_non_meta(&mut new_iter);
                    }
                    Ordering::Equal => {
                        // Keys match - new entry shadows old entry
                        if let Some(merged_entry) = merge_entries(
                            old_entry,
                            new_entry,
                            keep_dead_entries,
                            normalize_init_entries,
                        ) {
                            record_entry_type(counters, &merged_entry);
                            maybe_put(
                                merged_entry,
                                &mut shadow_cursors,
                                keep_shadowed_lifecycle_entries,
                                &mut merged,
                                counters,
                            );
                        } else {
                            // INIT+DEAD annihilation
                            if let Some(c) = counters {
                                c.record_annihilated();
                            }
                        }
                        old_current = next_non_meta(&mut old_iter);
                        new_current = next_non_meta(&mut new_iter);
                    }
                }
            }
            (None, Some(_)) => {
                old_current = next_non_meta(&mut old_iter);
            }
            (Some(_), None) => {
                new_current = next_non_meta(&mut new_iter);
            }
            (None, None) => {
                old_current = next_non_meta(&mut old_iter);
                new_current = next_non_meta(&mut new_iter);
            }
        }
    }

    // Drain remaining old entries
    while let Some(entry) = old_current {
        if !entry.is_metadata() && should_keep_entry(&entry, keep_dead_entries) {
            record_entry_type(counters, &entry);
            maybe_put(
                entry,
                &mut shadow_cursors,
                keep_shadowed_lifecycle_entries,
                &mut merged,
                counters,
            );
        }
        old_current = old_iter.next();
    }

    // Drain remaining new entries
    while let Some(entry) = new_current {
        if !entry.is_metadata() && should_keep_entry(&entry, keep_dead_entries) {
            let entry = maybe_normalize_entry(entry, normalize_init_entries);
            record_entry_type(counters, &entry);
            maybe_put(
                entry,
                &mut shadow_cursors,
                keep_shadowed_lifecycle_entries,
                &mut merged,
                counters,
            );
        }
        new_current = new_iter.next();
    }

    if merged.is_empty() {
        // In stellar-core, even a merge that results in no data entries still produces a bucket
        // with a metadata entry (for protocol 11+). This ensures that the bucket list
        // hash is consistent. An uninitialized bucket has hash 0, but an initialized
        // empty bucket has the hash of its metadata.
        if let Some(meta) = output_meta {
            return Bucket::from_sorted_entries(vec![meta]);
        }
        return Ok(Bucket::empty());
    }

    // Use from_sorted_entries since the merge algorithm maintains sorted order.
    let result = Bucket::from_sorted_entries(merged)?;

    tracing::trace!(
        result_hash = %result.hash(),
        result_entries = result.len(),
        "merge_buckets complete"
    );
    Ok(result)
}

/// Merge two buckets and write the output directly to an uncompressed XDR file.
///
/// This is the fully streaming merge: both inputs and the output are streamed,
/// so memory usage is O(1) per input bucket regardless of size. The output
/// is written as uncompressed XDR with record marks (RFC 5531) suitable for
/// creating a `DiskBacked` bucket.
///
/// # Arguments
/// * `old_bucket` - The older bucket (entries may be shadowed)
/// * `new_bucket` - The newer bucket (entries take precedence)
/// * `output_path` - Path to write the merged bucket file
/// * `keep_dead_entries` - Whether to keep dead entries in the output
/// * `max_protocol_version` - Maximum protocol version allowed
/// * `normalize_init_entries` - Whether to convert INIT entries to LIVE
///
/// # Returns
/// The hash of the output bucket and the number of entries written.
pub fn merge_buckets_to_file(
    old_bucket: &Bucket,
    new_bucket: &Bucket,
    output_path: &Path,
    keep_dead_entries: bool,
    max_protocol_version: u32,
    normalize_init_entries: bool,
) -> Result<(Hash256, usize)> {
    merge_buckets_to_file_with_counters(
        old_bucket,
        new_bucket,
        output_path,
        keep_dead_entries,
        max_protocol_version,
        normalize_init_entries,
        None,
    )
}

/// Merge two buckets to file with optional merge counters.
pub fn merge_buckets_to_file_with_counters(
    old_bucket: &Bucket,
    new_bucket: &Bucket,
    output_path: &Path,
    keep_dead_entries: bool,
    max_protocol_version: u32,
    normalize_init_entries: bool,
    counters: Option<&MergeCounters>,
) -> Result<(Hash256, usize)> {
    use std::io::{BufWriter, Write};

    if new_bucket.is_empty() && old_bucket.is_empty() {
        // Write empty file
        File::create(output_path)?;
        return Ok((Hash256::ZERO, 0));
    }

    let mut old_iter = old_bucket.iter();
    let mut new_iter = new_bucket.iter();

    let mut old_meta: Option<BucketMetadata> = None;
    let mut new_meta: Option<BucketMetadata> = None;
    let mut old_current = advance_skip_metadata(&mut old_iter, &mut old_meta);
    let mut new_current = advance_skip_metadata(&mut new_iter, &mut new_meta);

    let (_, output_meta) =
        build_output_metadata(old_meta.as_ref(), new_meta.as_ref(), max_protocol_version)?;

    let file = File::create(output_path)?;
    let mut writer = BufWriter::new(file);
    let mut hasher = Sha256::new();
    let mut entry_count = 0usize;

    // Helper: serialize and write one entry
    let write_entry = |entry: &BucketEntry,
                       writer: &mut BufWriter<File>,
                       hasher: &mut Sha256,
                       count: &mut usize|
     -> Result<()> {
        let data = entry
            .to_xdr(Limits::none())
            .map_err(|e| BucketError::Serialization(format!("Failed to serialize entry: {}", e)))?;

        // Write XDR record mark + data
        let record_mark = (data.len() as u32) | 0x80000000;
        writer.write_all(&record_mark.to_be_bytes())?;
        writer.write_all(&data)?;

        // Update hash (same format as record mark + data)
        hasher.update(record_mark.to_be_bytes());
        hasher.update(&data);

        *count += 1;
        Ok(())
    };

    // Write metadata first
    if let Some(ref meta) = output_meta {
        record_entry_type(counters, meta);
        write_entry(meta, &mut writer, &mut hasher, &mut entry_count)?;
    }

    // Two-pointer merge
    while old_current.is_some() && new_current.is_some() {
        let old_entry = old_current.as_ref().unwrap();
        let new_entry = new_current.as_ref().unwrap();

        let old_key = old_entry.key();
        let new_key = new_entry.key();

        match (old_key, new_key) {
            (Some(ref ok), Some(ref nk)) => match compare_keys(ok, nk) {
                Ordering::Less => {
                    if should_keep_entry(old_entry, keep_dead_entries) {
                        record_entry_type(counters, old_entry);
                        write_entry(old_entry, &mut writer, &mut hasher, &mut entry_count)?;
                    }
                    old_current = next_non_meta(&mut old_iter);
                }
                Ordering::Greater => {
                    if should_keep_entry(new_entry, keep_dead_entries) {
                        let entry =
                            maybe_normalize_entry(new_entry.clone(), normalize_init_entries);
                        record_entry_type(counters, &entry);
                        write_entry(&entry, &mut writer, &mut hasher, &mut entry_count)?;
                    }
                    new_current = next_non_meta(&mut new_iter);
                }
                Ordering::Equal => {
                    if let Some(merged_entry) = merge_entries(
                        old_entry,
                        new_entry,
                        keep_dead_entries,
                        normalize_init_entries,
                    ) {
                        record_entry_type(counters, &merged_entry);
                        write_entry(&merged_entry, &mut writer, &mut hasher, &mut entry_count)?;
                    } else {
                        // INIT+DEAD annihilation
                        if let Some(c) = counters {
                            c.record_annihilated();
                        }
                    }
                    old_current = next_non_meta(&mut old_iter);
                    new_current = next_non_meta(&mut new_iter);
                }
            },
            (None, Some(_)) => {
                old_current = next_non_meta(&mut old_iter);
            }
            (Some(_), None) => {
                new_current = next_non_meta(&mut new_iter);
            }
            (None, None) => {
                old_current = next_non_meta(&mut old_iter);
                new_current = next_non_meta(&mut new_iter);
            }
        }
    }

    // Drain remaining old entries
    while let Some(entry) = old_current {
        if !entry.is_metadata() && should_keep_entry(&entry, keep_dead_entries) {
            record_entry_type(counters, &entry);
            write_entry(&entry, &mut writer, &mut hasher, &mut entry_count)?;
        }
        old_current = old_iter.next();
    }

    // Drain remaining new entries
    while let Some(entry) = new_current {
        if !entry.is_metadata() && should_keep_entry(&entry, keep_dead_entries) {
            let entry = maybe_normalize_entry(entry, normalize_init_entries);
            record_entry_type(counters, &entry);
            write_entry(&entry, &mut writer, &mut hasher, &mut entry_count)?;
        }
        new_current = new_iter.next();
    }

    // Flush and sync
    writer.flush()?;
    writer
        .into_inner()
        .map_err(|e| {
            BucketError::Io(std::io::Error::other(format!(
                "Failed to flush writer: {}",
                e
            )))
        })?
        .sync_all()?;

    let hash_bytes: [u8; 32] = hasher.finalize().into();
    let hash = Hash256::from_bytes(hash_bytes);

    Ok((hash, entry_count))
}

/// Advance a `BucketIter`, skipping metadata entries and extracting metadata.
fn advance_skip_metadata(
    iter: &mut BucketIter<'_>,
    meta_out: &mut Option<BucketMetadata>,
) -> Option<BucketEntry> {
    for entry in iter.by_ref() {
        if let BucketEntry::Metaentry(m) = &entry {
            *meta_out = Some(m.clone());
            continue;
        }
        return Some(entry);
    }
    None
}

/// Merge two buckets using in-memory entries (level 0 optimization).
///
/// This function performs an in-memory merge of two buckets, avoiding disk I/O
/// for reading. The result is a new bucket with entries kept in memory for
/// subsequent fast merges.
///
/// This is the Rust equivalent of stellar-core `LiveBucket::mergeInMemory`.
///
/// # Requirements
///
/// Both input buckets MUST have in-memory entries available
/// (i.e., `has_in_memory_entries()` returns true).
///
/// # Arguments
///
/// * `old_bucket` - The older bucket (entries may be shadowed)
/// * `new_bucket` - The newer bucket (entries take precedence)
/// * `max_protocol_version` - Maximum protocol version for output
///
/// # Returns
///
/// A new bucket with:
/// - All entries merged according to CAP-0020 semantics
/// - In-memory entries populated for the next merge
/// - Proper hash computed
///
/// # Panics
///
/// Panics if either bucket does not have in-memory entries.
pub fn merge_in_memory(
    old_bucket: &Bucket,
    new_bucket: &Bucket,
    max_protocol_version: u32,
) -> Result<Bucket> {
    // Verify both buckets have in-memory entries
    assert!(
        old_bucket.has_in_memory_entries(),
        "old_bucket must have in-memory entries for merge_in_memory"
    );
    assert!(
        new_bucket.has_in_memory_entries(),
        "new_bucket must have in-memory entries for merge_in_memory"
    );

    // Get in-memory entries directly (no disk I/O)
    let old_entries = old_bucket.get_in_memory_entries().unwrap();
    let new_entries = new_bucket.get_in_memory_entries().unwrap();

    // DEBUG: Print merge inputs
    tracing::debug!(
        old_entries_count = old_entries.len(),
        new_entries_count = new_entries.len(),
        max_protocol_version = max_protocol_version,
        "merge_in_memory: starting"
    );

    // Build output metadata using max_protocol_version directly.
    // This matches stellar-core mergeInMemory behavior where meta.ledgerVersion = maxProtocolVersion
    // without calling calculateMergeProtocolVersion.
    let output_meta = if protocol_version_starts_from(max_protocol_version, ProtocolVersion::V11) {
        let mut meta = BucketMetadata {
            ledger_version: max_protocol_version,
            ext: BucketMetadataExt::V0,
        };
        if protocol_version_starts_from(max_protocol_version, ProtocolVersion::V23) {
            meta.ext = BucketMetadataExt::V1(stellar_xdr::curr::BucketListType::Live);
        }
        Some(BucketEntry::Metaentry(meta))
    } else {
        None
    };

    tracing::debug!(
        output_meta = ?output_meta,
        max_protocol_version = max_protocol_version,
        "merge_in_memory: built output metadata"
    );

    // Initialize incremental hash and entry collection
    let mut hasher = Sha256::new();
    let mut key_index = HashMap::new();

    // Pre-allocate output vector
    // all_entries: includes metadata for storage/indexing
    // Note: We derive level_zero_entries at the end using SharedWithStorage
    // which avoids cloning entries
    let capacity =
        old_entries.len() + new_entries.len() + output_meta.as_ref().map(|_| 1).unwrap_or(0);
    let mut all_entries = Vec::with_capacity(capacity);
    let mut entry_idx = 0;

    // Reusable buffer for XDR serialization (avoids repeated allocations)
    let mut entry_buf: Vec<u8> = Vec::with_capacity(4096);
    let mut key_buf: Vec<u8> = Vec::with_capacity(256);

    // Helper to add entry to output with incremental hashing
    // Uses reusable buffers to minimize allocations
    let add_entry = |entry: BucketEntry,
                     hasher: &mut Sha256,
                     key_index: &mut HashMap<LedgerKey, usize>,
                     all_entries: &mut Vec<BucketEntry>,
                     entry_idx: &mut usize,
                     entry_buf: &mut Vec<u8>,
                     _key_buf: &mut Vec<u8>|
     -> Result<()> {
        use stellar_xdr::curr::Limited;

        // Serialize entry for hash using reusable buffer
        entry_buf.clear();
        {
            let mut limited = Limited::new(entry_buf as &mut Vec<u8>, Limits::none());
            entry.write_xdr(&mut limited).map_err(|e| {
                BucketError::Serialization(format!("Failed to serialize entry: {}", e))
            })?;
        }

        // Update hash with XDR Record Marking format
        let size = entry_buf.len() as u32;
        let record_mark = size | 0x80000000;
        hasher.update(record_mark.to_be_bytes());
        hasher.update(entry_buf.as_slice());

        // Build key index for non-metadata entries
        if !entry.is_metadata() {
            if let Some(key) = entry.key() {
                key_index.insert(key, *entry_idx);
            }
        }

        all_entries.push(entry);
        *entry_idx += 1;
        Ok(())
    };

    // Add metadata first if present
    if let Some(ref meta) = output_meta {
        add_entry(
            meta.clone(),
            &mut hasher,
            &mut key_index,
            &mut all_entries,
            &mut entry_idx,
            &mut entry_buf,
            &mut key_buf,
        )?;
    }

    // Set up indices, skipping metadata entries
    let mut old_idx = 0;
    let mut new_idx = 0;

    while old_idx < old_entries.len() && old_entries[old_idx].is_metadata() {
        old_idx += 1;
    }
    while new_idx < new_entries.len() && new_entries[new_idx].is_metadata() {
        new_idx += 1;
    }

    // Level 0 always keeps tombstones (they may shadow entries in deeper levels)
    let keep_dead_entries = true;
    // Level 0 does NOT normalize INIT entries (they stay INIT within the merge window)
    let normalize_init_entries = false;

    // Merge entries with incremental hashing
    while old_idx < old_entries.len() && new_idx < new_entries.len() {
        let old_entry = &old_entries[old_idx];
        let new_entry = &new_entries[new_idx];

        let old_key = old_entry.key();
        let new_key = new_entry.key();

        match (old_key, new_key) {
            (Some(ref ok), Some(ref nk)) => {
                use crate::entry::compare_keys;
                match compare_keys(ok, nk) {
                    std::cmp::Ordering::Less => {
                        if should_keep_entry(old_entry, keep_dead_entries) {
                            add_entry(
                                old_entry.clone(),
                                &mut hasher,
                                &mut key_index,
                                &mut all_entries,
                                &mut entry_idx,
                                &mut entry_buf,
                                &mut key_buf,
                            )?;
                        }
                        old_idx += 1;
                    }
                    std::cmp::Ordering::Greater => {
                        if should_keep_entry(new_entry, keep_dead_entries) {
                            add_entry(
                                maybe_normalize_entry(new_entry.clone(), normalize_init_entries),
                                &mut hasher,
                                &mut key_index,
                                &mut all_entries,
                                &mut entry_idx,
                                &mut entry_buf,
                                &mut key_buf,
                            )?;
                        }
                        new_idx += 1;
                    }
                    std::cmp::Ordering::Equal => {
                        if let Some(merged_entry) = merge_entries(
                            old_entry,
                            new_entry,
                            keep_dead_entries,
                            normalize_init_entries,
                        ) {
                            add_entry(
                                merged_entry,
                                &mut hasher,
                                &mut key_index,
                                &mut all_entries,
                                &mut entry_idx,
                                &mut entry_buf,
                                &mut key_buf,
                            )?;
                        }
                        old_idx += 1;
                        new_idx += 1;
                    }
                }
            }
            (None, Some(_)) => old_idx += 1,
            (Some(_), None) => new_idx += 1,
            (None, None) => {
                old_idx += 1;
                new_idx += 1;
            }
        }
    }

    // Add remaining old entries
    while old_idx < old_entries.len() {
        let entry = &old_entries[old_idx];
        if !entry.is_metadata() && should_keep_entry(entry, keep_dead_entries) {
            add_entry(
                entry.clone(),
                &mut hasher,
                &mut key_index,
                &mut all_entries,
                &mut entry_idx,
                &mut entry_buf,
                &mut key_buf,
            )?;
        }
        old_idx += 1;
    }

    // Add remaining new entries
    while new_idx < new_entries.len() {
        let entry = &new_entries[new_idx];
        if !entry.is_metadata() && should_keep_entry(entry, keep_dead_entries) {
            add_entry(
                maybe_normalize_entry(entry.clone(), normalize_init_entries),
                &mut hasher,
                &mut key_index,
                &mut all_entries,
                &mut entry_idx,
                &mut entry_buf,
                &mut key_buf,
            )?;
        }
        new_idx += 1;
    }

    // Handle empty result
    if all_entries.is_empty() {
        if let Some(meta) = output_meta {
            return Bucket::from_sorted_entries_with_in_memory(vec![meta]);
        }
        return Ok(Bucket::empty());
    }

    // Compute final hash
    let hash_bytes: [u8; 32] = hasher.finalize().into();
    let hash = Hash256::from_bytes(hash_bytes);

    // DEBUG: Print merge output
    tracing::debug!(
        merged_count = all_entries.len(),
        has_meta = all_entries.first().map(|e| e.is_metadata()).unwrap_or(false),
        hash = %hash.to_hex(),
        "merge_in_memory: finished merge"
    );

    // Count metadata entries (typically 0 or 1, always at the start)
    let metadata_count = if output_meta.is_some() { 1 } else { 0 };

    // Create bucket directly with pre-computed hash
    // Use shared level zero state - no cloning needed!
    Ok(Bucket::from_parts(
        hash,
        Arc::new(all_entries),
        Arc::new(key_index),
        metadata_count,
    ))
}

/// Merge two buckets with shadow elimination for pre-shadow-removal protocols.
///
/// Shadows are only used before protocol 12 to avoid reintroducing entries
/// that are already shadowed by newer levels of the bucket list.
pub fn merge_buckets_with_options_and_shadows(
    old_bucket: &Bucket,
    new_bucket: &Bucket,
    keep_dead_entries: bool,
    max_protocol_version: u32,
    normalize_init_entries: bool,
    shadow_buckets: &[Bucket],
) -> Result<Bucket> {
    merge_buckets_with_options_and_shadows_and_counters(
        old_bucket,
        new_bucket,
        keep_dead_entries,
        max_protocol_version,
        normalize_init_entries,
        shadow_buckets,
        None,
    )
}

/// Merge two buckets with shadow elimination and optional merge counters.
pub fn merge_buckets_with_options_and_shadows_and_counters(
    old_bucket: &Bucket,
    new_bucket: &Bucket,
    keep_dead_entries: bool,
    max_protocol_version: u32,
    normalize_init_entries: bool,
    shadow_buckets: &[Bucket],
    counters: Option<&MergeCounters>,
) -> Result<Bucket> {
    // For protocol >= 12 (shadows removed), shadows are always
    // empty in practice. Pass empty slice to skip shadow cursor creation.
    if shadow_buckets.is_empty()
        || protocol_version_starts_from(max_protocol_version, ProtocolVersion::V12)
    {
        return merge_with_shadows_impl(
            old_bucket,
            new_bucket,
            keep_dead_entries,
            max_protocol_version,
            normalize_init_entries,
            &[],
            false,
            counters,
        );
    }

    let keep_shadowed_lifecycle_entries =
        protocol_version_starts_from(max_protocol_version, ProtocolVersion::V11);
    merge_with_shadows_impl(
        old_bucket,
        new_bucket,
        keep_dead_entries,
        max_protocol_version,
        normalize_init_entries,
        shadow_buckets,
        keep_shadowed_lifecycle_entries,
        counters,
    )
}

struct ShadowCursor<'a> {
    iter: BucketIter<'a>,
    current: Option<BucketEntry>,
}

impl<'a> ShadowCursor<'a> {
    fn new(bucket: &'a Bucket) -> Self {
        let mut iter = bucket.iter();
        let current = next_non_meta(&mut iter);
        Self { iter, current }
    }

    fn advance_to_key_or_after(&mut self, key: &LedgerKey) -> bool {
        loop {
            let Some(entry) = self.current.as_ref() else {
                return false;
            };
            let Some(entry_key) = entry.key() else {
                self.current = next_non_meta(&mut self.iter);
                continue;
            };

            match compare_keys(&entry_key, key) {
                Ordering::Less => {
                    self.current = next_non_meta(&mut self.iter);
                }
                Ordering::Equal => return true,
                Ordering::Greater => return false,
            }
        }
    }
}

fn next_non_meta(iter: &mut BucketIter<'_>) -> Option<BucketEntry> {
    iter.by_ref().find(|entry| !entry.is_metadata())
}

fn is_shadowed(entry: &BucketEntry, cursors: &mut [ShadowCursor<'_>]) -> bool {
    let Some(key) = entry.key() else {
        return false;
    };

    for cursor in cursors.iter_mut() {
        if cursor.advance_to_key_or_after(&key) {
            return true;
        }
    }

    false
}

/// Emit an entry to the output, checking shadows inline.
///
/// This matches stellar-core's `BucketOutputIterator::maybePut()` pattern.
/// If the entry is shadowed by a higher-level bucket, it's silently dropped
/// unless it's a lifecycle entry (INIT/DEAD) and `keep_shadowed_lifecycle_entries`
/// is true (protocol 11+).
///
/// When `shadow_cursors` is empty, this is equivalent to `output.push(entry)`.
fn maybe_put(
    entry: BucketEntry,
    shadow_cursors: &mut [ShadowCursor<'_>],
    keep_shadowed_lifecycle_entries: bool,
    output: &mut Vec<BucketEntry>,
    counters: Option<&MergeCounters>,
) {
    if !shadow_cursors.is_empty() {
        if keep_shadowed_lifecycle_entries && (entry.is_init() || entry.is_dead()) {
            // Lifecycle entries (INIT/DEAD) are preserved even when shadowed
        } else if is_shadowed(&entry, shadow_cursors) {
            if let Some(c) = counters {
                c.record_shadowed();
            }
            return;
        }
    }
    output.push(entry);
}

/// Check if an entry should be kept in the merged output.
fn should_keep_entry(entry: &BucketEntry, keep_dead_entries: bool) -> bool {
    match entry {
        BucketEntry::Deadentry(_) => keep_dead_entries,
        _ => true,
    }
}

/// Normalize an entry (convert Init to Live).
fn normalize_entry(entry: BucketEntry) -> BucketEntry {
    match entry {
        BucketEntry::Initentry(e) => BucketEntry::Liveentry(e),
        other => other,
    }
}

/// Conditionally normalize an entry.
///
/// If `normalize` is true, converts INIT to LIVE. Otherwise returns entry unchanged.
fn maybe_normalize_entry(entry: BucketEntry, normalize: bool) -> BucketEntry {
    if normalize {
        normalize_entry(entry)
    } else {
        entry
    }
}

/// Merge two entries with the same key.
///
/// Returns the merged entry, or None if the entry should be removed.
///
/// Merge semantics per CAP-0020:
/// - INITENTRY + DEADENTRY → Both annihilated (nothing output)
/// - INITENTRY=x + LIVEENTRY=y → Output as INITENTRY=y (preserves INIT status)
/// - DEADENTRY + INITENTRY=x → Output as LIVEENTRY=x
/// - LIVEENTRY + DEADENTRY → Dead (if keep_dead_entries) or nothing
/// - Any + LIVEENTRY → LIVEENTRY wins
fn merge_entries(
    old: &BucketEntry,
    new: &BucketEntry,
    keep_dead_entries: bool,
    _normalize_init_entries: bool,
) -> Option<BucketEntry> {
    match (old, new) {
        // CAP-0020: INITENTRY + DEADENTRY → Both annihilated
        // This is a key optimization: if we created and then deleted in the same
        // merge window, we output nothing at all.
        (BucketEntry::Initentry(_), BucketEntry::Deadentry(_)) => None,

        // CAP-0020: DEADENTRY + INITENTRY=x → Output as LIVEENTRY=x
        // The old tombstone is cancelled by the new creation
        (BucketEntry::Deadentry(_), BucketEntry::Initentry(entry)) => {
            Some(BucketEntry::Liveentry(entry.clone()))
        }

        // CAP-0020: INITENTRY=x + LIVEENTRY=y → Output as INITENTRY=y
        // Preserve the INIT status (entry was created in this merge range)
        (BucketEntry::Initentry(_), BucketEntry::Liveentry(entry)) => {
            Some(BucketEntry::Initentry(entry.clone()))
        }

        // New Live shadows old Live - new wins
        (BucketEntry::Liveentry(_), BucketEntry::Liveentry(entry)) => {
            Some(BucketEntry::Liveentry(entry.clone()))
        }

        // New Live shadows old Dead - live wins
        (BucketEntry::Deadentry(_), BucketEntry::Liveentry(entry)) => {
            Some(BucketEntry::Liveentry(entry.clone()))
        }

        // LIVE + INIT or INIT + INIT: malformed bucket.
        // The only legal old + new-INIT case is DEAD + INIT (handled above).
        // C++ throws "Malformed bucket: old non-DEAD + new INIT."
        (_, BucketEntry::Initentry(_)) => {
            panic!("Malformed bucket: old non-DEAD + new INIT.");
        }

        // LIVEENTRY + DEADENTRY → Dead entry (tombstone) if keeping, else nothing
        (BucketEntry::Liveentry(_), BucketEntry::Deadentry(key)) => {
            if keep_dead_entries {
                Some(BucketEntry::Deadentry(key.clone()))
            } else {
                None
            }
        }

        // Dead shadows Dead - keep newest if needed
        (BucketEntry::Deadentry(_), BucketEntry::Deadentry(key)) => {
            if keep_dead_entries {
                Some(BucketEntry::Deadentry(key.clone()))
            } else {
                None
            }
        }

        // Metadata shouldn't have matching keys
        (BucketEntry::Metaentry(_), _) | (_, BucketEntry::Metaentry(_)) => None,
    }
}

/// Iterator that yields merged entries from two buckets.
///
/// This iterator implements lazy/streaming merge, yielding one entry at a time.
/// It's useful when you want to process merged entries without materializing
/// the full merged bucket in memory.
///
/// # Memory Usage
///
/// For disk-backed buckets, entries are collected upfront (same as `merge_buckets`).
/// For in-memory buckets, the iterator references existing entries without copying.
///
/// # Example
///
/// ```ignore
/// let iter = MergeIterator::new(&old_bucket, &new_bucket, true, 20);
/// for entry in iter {
///     process_entry(entry);
/// }
/// ```
///
/// Note: Always normalizes `Init` entries to `Live` for backward compatibility.
/// For more control, use `merge_buckets_with_options` instead.
pub struct MergeIterator {
    /// Entries from the older bucket (collected upfront).
    old_entries: Vec<BucketEntry>,
    /// Entries from the newer bucket (collected upfront).
    new_entries: Vec<BucketEntry>,
    /// Current index into old_entries.
    old_idx: usize,
    /// Current index into new_entries.
    new_idx: usize,
    /// Whether to keep dead entries in output.
    keep_dead_entries: bool,
    /// Metadata entry to emit first (if any).
    output_metadata: Option<BucketEntry>,
}

impl MergeIterator {
    /// Create a new merge iterator.
    pub fn new(
        old_bucket: &Bucket,
        new_bucket: &Bucket,
        keep_dead_entries: bool,
        max_protocol_version: u32,
    ) -> Self {
        // Collect entries - works for both in-memory and disk-backed buckets
        let old_entries: Vec<BucketEntry> = old_bucket.iter().collect();
        let new_entries: Vec<BucketEntry> = new_bucket.iter().collect();
        let old_meta = extract_metadata(&old_entries);
        let new_meta = extract_metadata(&new_entries);
        let (_, output_metadata) =
            build_output_metadata(old_meta.as_ref(), new_meta.as_ref(), max_protocol_version)
                .unwrap_or((0, None));

        Self {
            old_entries,
            new_entries,
            old_idx: 0,
            new_idx: 0,
            keep_dead_entries,
            output_metadata,
        }
    }
}

impl Iterator for MergeIterator {
    type Item = BucketEntry;

    fn next(&mut self) -> Option<Self::Item> {
        if let Some(meta) = self.output_metadata.take() {
            while self.old_idx < self.old_entries.len()
                && self.old_entries[self.old_idx].is_metadata()
            {
                self.old_idx += 1;
            }
            while self.new_idx < self.new_entries.len()
                && self.new_entries[self.new_idx].is_metadata()
            {
                self.new_idx += 1;
            }
            return Some(meta);
        }

        loop {
            // Check if we're done with both
            if self.old_idx >= self.old_entries.len() && self.new_idx >= self.new_entries.len() {
                return None;
            }

            // Only old entries left
            if self.new_idx >= self.new_entries.len() {
                let entry = self.old_entries[self.old_idx].clone();
                self.old_idx += 1;
                if !entry.is_metadata() {
                    return Some(entry);
                }
                continue;
            }

            // Only new entries left
            if self.old_idx >= self.old_entries.len() {
                let entry = self.new_entries[self.new_idx].clone();
                self.new_idx += 1;
                if !entry.is_metadata() && should_keep_entry(&entry, self.keep_dead_entries) {
                    return Some(normalize_entry(entry));
                }
                continue;
            }

            // Both have entries
            let old_entry = &self.old_entries[self.old_idx];
            let new_entry = &self.new_entries[self.new_idx];

            let old_key = old_entry.key();
            let new_key = new_entry.key();

            match (old_key, new_key) {
                (Some(ref ok), Some(ref nk)) => {
                    match compare_keys(ok, nk) {
                        Ordering::Less => {
                            self.old_idx += 1;
                            return Some(old_entry.clone());
                        }
                        Ordering::Greater => {
                            self.new_idx += 1;
                            if should_keep_entry(new_entry, self.keep_dead_entries) {
                                return Some(normalize_entry(new_entry.clone()));
                            }
                            continue;
                        }
                        Ordering::Equal => {
                            self.old_idx += 1;
                            self.new_idx += 1;
                            // Always normalize in MergeIterator for backward compatibility
                            if let Some(merged) =
                                merge_entries(old_entry, new_entry, self.keep_dead_entries, true)
                            {
                                return Some(merged);
                            }
                            continue;
                        }
                    }
                }
                (None, Some(_)) => {
                    self.old_idx += 1;
                    continue;
                }
                (Some(_), None) => {
                    self.new_idx += 1;
                    continue;
                }
                (None, None) => {
                    self.old_idx += 1;
                    self.new_idx += 1;
                    continue;
                }
            }
        }
    }
}

/// Merge multiple buckets in order (first is oldest).
pub fn merge_multiple(
    buckets: &[&Bucket],
    keep_dead_entries: bool,
    max_protocol_version: u32,
) -> Result<Bucket> {
    if buckets.is_empty() {
        return Ok(Bucket::empty());
    }

    let mut result = buckets[0].clone();

    for bucket in &buckets[1..] {
        result = merge_buckets(&result, bucket, keep_dead_entries, max_protocol_version)?;
    }

    Ok(result)
}

fn extract_metadata(entries: &[BucketEntry]) -> Option<BucketMetadata> {
    entries.iter().find_map(|entry| match entry {
        BucketEntry::Metaentry(meta) => Some(meta.clone()),
        _ => None,
    })
}

/// Build output metadata for a merged bucket.
///
/// Calculates the merge protocol version as max of input bucket versions.
/// This matches stellar-core's `calculateMergeProtocolVersion()` in `BucketBase.cpp`.
///
/// NOTE (BUCKETLISTDB_SPEC §7.2): stellar-core also includes shadow bucket
/// versions in the max calculation. Since shadow filtering was removed in
/// protocol 12, and henyey only supports protocol 24+, shadow versions are
/// never present and can be safely ignored here.
fn build_output_metadata(
    old_meta: Option<&BucketMetadata>,
    new_meta: Option<&BucketMetadata>,
    max_protocol_version: u32,
) -> Result<(u32, Option<BucketEntry>)> {
    let mut protocol_version = 0u32;
    if let Some(meta) = old_meta {
        protocol_version = protocol_version.max(meta.ledger_version);
    }
    if let Some(meta) = new_meta {
        protocol_version = protocol_version.max(meta.ledger_version);
    }

    // Validate that the calculated version doesn't exceed max_protocol_version.
    // max_protocol_version is a constraint, not the output version.
    if max_protocol_version > 0 && protocol_version > max_protocol_version {
        return Err(BucketError::Merge(format!(
            "bucket protocol version {} exceeds max_protocol_version {}",
            protocol_version, max_protocol_version
        )));
    }

    let use_meta = protocol_version_starts_from(protocol_version, ProtocolVersion::V11);
    if !use_meta {
        return Ok((protocol_version, None));
    }

    let mut output = BucketMetadata {
        ledger_version: protocol_version,
        ext: BucketMetadataExt::V0,
    };

    // For Protocol 23+, Live buckets must use V1 extension with BucketListType::LIVE.
    // merge_buckets_with_options is specifically for the Live bucket list.
    if protocol_version_starts_from(protocol_version, ProtocolVersion::V23) {
        output.ext = BucketMetadataExt::V1(stellar_xdr::curr::BucketListType::Live);
    }

    Ok((protocol_version, Some(BucketEntry::Metaentry(output))))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::BucketEntry;
    use stellar_xdr::curr::*; // Re-import to shadow XDR's BucketEntry

    const TEST_PROTOCOL: u32 = 25;

    fn make_account_id(bytes: [u8; 32]) -> AccountId {
        AccountId(PublicKey::PublicKeyTypeEd25519(Uint256(bytes)))
    }

    fn make_account_entry(bytes: [u8; 32], balance: i64) -> LedgerEntry {
        LedgerEntry {
            last_modified_ledger_seq: 1,
            data: LedgerEntryData::Account(AccountEntry {
                account_id: make_account_id(bytes),
                balance,
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
        }
    }

    fn make_account_key(bytes: [u8; 32]) -> LedgerKey {
        LedgerKey::Account(LedgerKeyAccount {
            account_id: make_account_id(bytes),
        })
    }

    #[test]
    fn test_merge_empty_buckets() {
        let empty1 = Bucket::empty();
        let empty2 = Bucket::empty();

        let merged = merge_buckets(&empty1, &empty2, true, 0).unwrap();
        assert!(merged.is_empty());
    }

    #[test]
    fn test_merge_with_empty() {
        let entries = vec![BucketEntry::Liveentry(make_account_entry([1u8; 32], 100))];
        let bucket = Bucket::from_entries(entries).unwrap();
        let empty = Bucket::empty();

        // New is empty
        let merged = merge_buckets(&bucket, &empty, true, 0).unwrap();
        assert_eq!(merged.len(), 1);

        // Old is empty
        let merged = merge_buckets(&empty, &bucket, true, 0).unwrap();
        assert_eq!(merged.len(), 1);
    }

    #[test]
    fn test_merge_no_overlap() {
        let old_entries = vec![BucketEntry::Liveentry(make_account_entry([1u8; 32], 100))];
        let new_entries = vec![BucketEntry::Liveentry(make_account_entry([2u8; 32], 200))];

        let old_bucket = Bucket::from_entries(old_entries).unwrap();
        let new_bucket = Bucket::from_entries(new_entries).unwrap();

        let merged = merge_buckets(&old_bucket, &new_bucket, true, 0).unwrap();
        assert_eq!(merged.len(), 2);
    }

    #[test]
    fn test_merge_shadow() {
        let old_entries = vec![BucketEntry::Liveentry(make_account_entry([1u8; 32], 100))];
        let new_entries = vec![BucketEntry::Liveentry(make_account_entry([1u8; 32], 200))];

        let old_bucket = Bucket::from_entries(old_entries).unwrap();
        let new_bucket = Bucket::from_entries(new_entries).unwrap();

        let merged = merge_buckets(&old_bucket, &new_bucket, true, 0).unwrap();
        assert_eq!(merged.len(), 1);

        // Verify new entry won
        let key = make_account_key([1u8; 32]);
        let entry = merged.get_entry(&key).unwrap().unwrap();
        if let LedgerEntryData::Account(account) = &entry.data {
            assert_eq!(account.balance, 200);
        } else {
            panic!("Expected Account entry");
        }
    }

    #[test]
    fn test_merge_dead_shadows_live() {
        let old_entries = vec![BucketEntry::Liveentry(make_account_entry([1u8; 32], 100))];
        let new_entries = vec![BucketEntry::Deadentry(make_account_key([1u8; 32]))];

        let old_bucket = Bucket::from_entries(old_entries).unwrap();
        let new_bucket = Bucket::from_entries(new_entries).unwrap();

        // With keep_dead_entries = true
        let merged = merge_buckets(&old_bucket, &new_bucket, true, 0).unwrap();
        assert_eq!(merged.len(), 1);
        assert!(merged.entries()[0].is_dead());

        // With keep_dead_entries = false
        let merged = merge_buckets(&old_bucket, &new_bucket, false, 0).unwrap();
        assert_eq!(merged.len(), 0);
    }

    #[test]
    fn test_merge_init_to_live() {
        let entries = vec![BucketEntry::Initentry(make_account_entry([1u8; 32], 100))];
        let bucket = Bucket::from_entries(entries).unwrap();

        let merged = merge_buckets(&Bucket::empty(), &bucket, true, 0).unwrap();
        assert_eq!(merged.len(), 1);

        // Init should be converted to Live
        assert!(merged.entries()[0].is_live());
    }

    #[test]
    fn test_merge_complex() {
        let old_entries = vec![
            BucketEntry::Liveentry(make_account_entry([1u8; 32], 100)),
            BucketEntry::Liveentry(make_account_entry([2u8; 32], 200)),
            BucketEntry::Liveentry(make_account_entry([3u8; 32], 300)),
        ];

        let new_entries = vec![
            BucketEntry::Deadentry(make_account_key([1u8; 32])), // Delete first
            BucketEntry::Liveentry(make_account_entry([2u8; 32], 250)), // Update second
            BucketEntry::Liveentry(make_account_entry([4u8; 32], 400)), // Add new
        ];

        let old_bucket = Bucket::from_entries(old_entries).unwrap();
        let new_bucket = Bucket::from_entries(new_entries).unwrap();

        let merged = merge_buckets(&old_bucket, &new_bucket, true, 0).unwrap();

        // Should have: Dead(1), Live(2, 250), Live(3, 300), Live(4, 400)
        assert_eq!(merged.len(), 4);

        // Verify entries
        let key1 = make_account_key([1u8; 32]);
        assert!(merged.get(&key1).unwrap().unwrap().is_dead());

        let key2 = make_account_key([2u8; 32]);
        let entry2 = merged.get_entry(&key2).unwrap().unwrap();
        if let LedgerEntryData::Account(account) = &entry2.data {
            assert_eq!(account.balance, 250);
        }

        let key3 = make_account_key([3u8; 32]);
        let entry3 = merged.get_entry(&key3).unwrap().unwrap();
        if let LedgerEntryData::Account(account) = &entry3.data {
            assert_eq!(account.balance, 300);
        }

        let key4 = make_account_key([4u8; 32]);
        let entry4 = merged.get_entry(&key4).unwrap().unwrap();
        if let LedgerEntryData::Account(account) = &entry4.data {
            assert_eq!(account.balance, 400);
        }
    }

    #[test]
    fn test_merge_iterator() {
        let old_entries = vec![
            BucketEntry::Liveentry(make_account_entry([1u8; 32], 100)),
            BucketEntry::Liveentry(make_account_entry([3u8; 32], 300)),
        ];

        let new_entries = vec![BucketEntry::Liveentry(make_account_entry([2u8; 32], 200))];

        let old_bucket = Bucket::from_entries(old_entries).unwrap();
        let new_bucket = Bucket::from_entries(new_entries).unwrap();

        let iter = MergeIterator::new(&old_bucket, &new_bucket, true, 0);
        let merged: Vec<_> = iter.collect();

        assert_eq!(merged.len(), 3);
    }

    #[test]
    fn test_merge_multiple() {
        let bucket1 = Bucket::from_entries(vec![BucketEntry::Liveentry(make_account_entry(
            [1u8; 32], 100,
        ))])
        .unwrap();

        let bucket2 = Bucket::from_entries(vec![BucketEntry::Liveentry(make_account_entry(
            [1u8; 32], 200,
        ))])
        .unwrap();

        let bucket3 = Bucket::from_entries(vec![BucketEntry::Liveentry(make_account_entry(
            [1u8; 32], 300,
        ))])
        .unwrap();

        let buckets = vec![&bucket1, &bucket2, &bucket3];
        let merged = merge_multiple(&buckets, true, 0).unwrap();

        assert_eq!(merged.len(), 1);

        let key = make_account_key([1u8; 32]);
        let entry = merged.get_entry(&key).unwrap().unwrap();
        if let LedgerEntryData::Account(account) = &entry.data {
            assert_eq!(account.balance, 300); // Newest wins
        }
    }

    // ============ CAP-0020 INITENTRY Tests ============

    #[test]
    fn test_cap0020_init_plus_dead_annihilation() {
        // CAP-0020: INITENTRY + DEADENTRY → Both annihilated
        let old_entries = vec![BucketEntry::Initentry(make_account_entry([1u8; 32], 100))];
        let new_entries = vec![BucketEntry::Deadentry(make_account_key([1u8; 32]))];

        let old_bucket = Bucket::from_entries(old_entries).unwrap();
        let new_bucket = Bucket::from_entries(new_entries).unwrap();

        // Even with keep_dead_entries = true, INIT + DEAD should annihilate
        let merged = merge_buckets(&old_bucket, &new_bucket, true, 0).unwrap();
        assert_eq!(merged.len(), 0, "INIT + DEAD should produce nothing");
    }

    #[test]
    fn test_cap0020_dead_plus_init_becomes_live() {
        // CAP-0020: DEADENTRY + INITENTRY=x → Output as LIVEENTRY=x
        let old_entries = vec![BucketEntry::Deadentry(make_account_key([1u8; 32]))];
        let new_entries = vec![BucketEntry::Initentry(make_account_entry([1u8; 32], 200))];

        let old_bucket = Bucket::from_entries(old_entries).unwrap();
        let new_bucket = Bucket::from_entries(new_entries).unwrap();

        let merged = merge_buckets(&old_bucket, &new_bucket, true, 0).unwrap();
        assert_eq!(merged.len(), 1);
        assert!(
            merged.entries()[0].is_live(),
            "DEAD + INIT should become LIVE"
        );

        let key = make_account_key([1u8; 32]);
        let entry = merged.get_entry(&key).unwrap().unwrap();
        if let LedgerEntryData::Account(account) = &entry.data {
            assert_eq!(account.balance, 200);
        }
    }

    #[test]
    fn test_cap0020_init_plus_live_preserves_init() {
        // CAP-0020: INITENTRY=x + LIVEENTRY=y → Output as INITENTRY=y
        let old_entries = vec![BucketEntry::Initentry(make_account_entry([1u8; 32], 100))];
        let new_entries = vec![BucketEntry::Liveentry(make_account_entry([1u8; 32], 200))];

        let old_bucket = Bucket::from_entries(old_entries).unwrap();
        let new_bucket = Bucket::from_entries(new_entries).unwrap();

        let merged = merge_buckets(&old_bucket, &new_bucket, true, 0).unwrap();
        assert_eq!(merged.len(), 1);

        // Should preserve INIT status with new value
        let entry = &merged.entries()[0];
        assert!(entry.is_init(), "INIT + LIVE should preserve INIT status");

        let _key = make_account_key([1u8; 32]);
        if let BucketEntry::Initentry(ledger_entry) = entry {
            if let LedgerEntryData::Account(account) = &ledger_entry.data {
                assert_eq!(account.balance, 200, "Should have new value");
            }
        }
    }

    #[test]
    #[should_panic(expected = "Malformed bucket: old non-DEAD + new INIT")]
    fn test_cap0020_init_plus_init_panics() {
        // Two INITs for the same key is malformed — C++ throws
        // "Malformed bucket: old non-DEAD + new INIT."
        let old_entries = vec![BucketEntry::Initentry(make_account_entry([1u8; 32], 100))];
        let new_entries = vec![BucketEntry::Initentry(make_account_entry([1u8; 32], 200))];

        let old_bucket = Bucket::from_entries(old_entries).unwrap();
        let new_bucket = Bucket::from_entries(new_entries).unwrap();

        // This should panic
        let _ = merge_buckets(&old_bucket, &new_bucket, true, 0);
    }

    #[test]
    #[should_panic(expected = "Malformed bucket: old non-DEAD + new INIT")]
    fn test_cap0020_live_plus_init_panics() {
        // LIVE + INIT is malformed — C++ throws
        // "Malformed bucket: old non-DEAD + new INIT."
        // The only legal old + new-INIT case is DEAD + INIT.
        let old_entries = vec![BucketEntry::Liveentry(make_account_entry([1u8; 32], 100))];
        let new_entries = vec![BucketEntry::Initentry(make_account_entry([1u8; 32], 200))];

        let old_bucket = Bucket::from_entries(old_entries).unwrap();
        let new_bucket = Bucket::from_entries(new_entries).unwrap();

        // This should panic
        let _ = merge_buckets(&old_bucket, &new_bucket, true, 0);
    }

    #[test]
    fn test_cap0020_complex_scenario() {
        // Complex scenario testing multiple CAP-0020 rules
        let old_entries = vec![
            BucketEntry::Initentry(make_account_entry([1u8; 32], 100)), // Will be deleted (annihilated)
            BucketEntry::Deadentry(make_account_key([2u8; 32])),        // Will be recreated
            BucketEntry::Initentry(make_account_entry([3u8; 32], 300)), // Will be updated (preserve INIT)
            BucketEntry::Liveentry(make_account_entry([4u8; 32], 400)), // Will be deleted
        ];

        let new_entries = vec![
            BucketEntry::Deadentry(make_account_key([1u8; 32])), // Annihilates with old INIT
            BucketEntry::Initentry(make_account_entry([2u8; 32], 200)), // Recreates, becomes LIVE
            BucketEntry::Liveentry(make_account_entry([3u8; 32], 350)), // Updates, preserves INIT
            BucketEntry::Deadentry(make_account_key([4u8; 32])), // Deletes LIVE
        ];

        let old_bucket = Bucket::from_entries(old_entries).unwrap();
        let new_bucket = Bucket::from_entries(new_entries).unwrap();

        let merged = merge_buckets(&old_bucket, &new_bucket, true, 0).unwrap();

        // Entry 1: INIT + DEAD = nothing (annihilated)
        let key1 = make_account_key([1u8; 32]);
        assert!(
            merged.get(&key1).unwrap().is_none(),
            "Entry 1 should be annihilated"
        );

        // Entry 2: DEAD + INIT = LIVE
        let key2 = make_account_key([2u8; 32]);
        let entry2 = merged.get(&key2).unwrap().unwrap();
        assert!(entry2.is_live(), "Entry 2 should be LIVE");

        // Entry 3: INIT + LIVE = INIT (preserved)
        let key3 = make_account_key([3u8; 32]);
        let entry3 = merged.get(&key3).unwrap().unwrap();
        assert!(entry3.is_init(), "Entry 3 should preserve INIT");

        // Entry 4: LIVE + DEAD = DEAD
        let key4 = make_account_key([4u8; 32]);
        let entry4 = merged.get(&key4).unwrap().unwrap();
        assert!(entry4.is_dead(), "Entry 4 should be DEAD");
    }

    // ============ In-Memory Level 0 Optimization Tests ============

    #[test]
    fn test_merge_in_memory_basic() {
        // Create buckets with in-memory entries enabled
        let old_entries = vec![
            BucketEntry::Liveentry(make_account_entry([1u8; 32], 100)),
            BucketEntry::Liveentry(make_account_entry([2u8; 32], 200)),
        ];
        let new_entries = vec![
            BucketEntry::Liveentry(make_account_entry([2u8; 32], 250)), // Update entry 2
            BucketEntry::Liveentry(make_account_entry([3u8; 32], 300)), // Add new entry
        ];

        let old_bucket = Bucket::from_sorted_entries_with_in_memory(old_entries).unwrap();
        let new_bucket = Bucket::from_sorted_entries_with_in_memory(new_entries).unwrap();

        // Verify both have in-memory entries
        assert!(old_bucket.has_in_memory_entries());
        assert!(new_bucket.has_in_memory_entries());

        // Perform in-memory merge
        let merged = merge_in_memory(&old_bucket, &new_bucket, 25).unwrap();

        // Result should have in-memory entries
        assert!(merged.has_in_memory_entries());

        // Verify entries
        let key1 = make_account_key([1u8; 32]);
        let key2 = make_account_key([2u8; 32]);
        let key3 = make_account_key([3u8; 32]);

        let entry1 = merged.get_entry(&key1).unwrap().unwrap();
        let entry2 = merged.get_entry(&key2).unwrap().unwrap();
        let entry3 = merged.get_entry(&key3).unwrap().unwrap();

        if let LedgerEntryData::Account(a) = &entry1.data {
            assert_eq!(a.balance, 100);
        }
        if let LedgerEntryData::Account(a) = &entry2.data {
            assert_eq!(a.balance, 250); // Updated
        }
        if let LedgerEntryData::Account(a) = &entry3.data {
            assert_eq!(a.balance, 300);
        }
    }

    #[test]
    fn test_merge_in_memory_preserves_init_entries() {
        // INIT entries should NOT be normalized to LIVE in level 0 merges
        let old_entries = vec![BucketEntry::Initentry(make_account_entry([1u8; 32], 100))];
        let new_entries = vec![BucketEntry::Liveentry(make_account_entry([1u8; 32], 200))];

        let old_bucket = Bucket::from_sorted_entries_with_in_memory(old_entries).unwrap();
        let new_bucket = Bucket::from_sorted_entries_with_in_memory(new_entries).unwrap();

        let merged = merge_in_memory(&old_bucket, &new_bucket, 25).unwrap();

        // INIT + LIVE should become INIT with new value
        let key = make_account_key([1u8; 32]);
        let entry = merged.get(&key).unwrap().unwrap();
        assert!(entry.is_init(), "Level 0 merge should preserve INIT status");

        if let BucketEntry::Initentry(le) = entry {
            if let LedgerEntryData::Account(a) = &le.data {
                assert_eq!(a.balance, 200, "Should have updated value");
            }
        }
    }

    #[test]
    fn test_merge_in_memory_keeps_tombstones() {
        // Level 0 merges should always keep tombstones
        let old_entries = vec![BucketEntry::Liveentry(make_account_entry([1u8; 32], 100))];
        let new_entries = vec![BucketEntry::Deadentry(make_account_key([1u8; 32]))];

        let old_bucket = Bucket::from_sorted_entries_with_in_memory(old_entries).unwrap();
        let new_bucket = Bucket::from_sorted_entries_with_in_memory(new_entries).unwrap();

        let merged = merge_in_memory(&old_bucket, &new_bucket, 25).unwrap();

        // Should have the dead entry
        let key = make_account_key([1u8; 32]);
        let entry = merged.get(&key).unwrap().unwrap();
        assert!(entry.is_dead(), "Level 0 merge should keep tombstones");
    }

    #[test]
    fn test_merge_in_memory_annihilation() {
        // INIT + DEAD should annihilate even in in-memory merge
        let old_entries = vec![BucketEntry::Initentry(make_account_entry([1u8; 32], 100))];
        let new_entries = vec![BucketEntry::Deadentry(make_account_key([1u8; 32]))];

        let old_bucket = Bucket::from_sorted_entries_with_in_memory(old_entries).unwrap();
        let new_bucket = Bucket::from_sorted_entries_with_in_memory(new_entries).unwrap();

        let merged = merge_in_memory(&old_bucket, &new_bucket, 25).unwrap();

        // Entry should be annihilated
        let key = make_account_key([1u8; 32]);
        assert!(
            merged.get(&key).unwrap().is_none(),
            "INIT + DEAD should annihilate"
        );
    }

    #[test]
    fn test_fresh_in_memory_only() {
        // Test creating a shell bucket for immediate merging
        let entries = vec![
            BucketEntry::Liveentry(make_account_entry([1u8; 32], 100)),
            BucketEntry::Liveentry(make_account_entry([2u8; 32], 200)),
        ];

        let bucket = Bucket::fresh_in_memory_only(entries.clone());

        // Should have in-memory entries
        assert!(bucket.has_in_memory_entries());

        // Hash should be ZERO (not computed)
        assert!(bucket.hash().is_zero());

        // Should be able to get in-memory entries
        let in_mem = bucket.get_in_memory_entries().unwrap();
        assert_eq!(in_mem.len(), 2);
    }

    // ============ Protocol Version Handling Regression Tests ============
    //
    // These tests verify the fix for bucket list hash divergence caused by
    // incorrect protocol version handling in merges.
    //
    // stellar-core has TWO different merge behaviors:
    // 1. In-memory merge (level 0): Uses maxProtocolVersion directly
    // 2. Disk-based merge (levels 1+): Uses max(old_bucket_version, new_bucket_version)
    //
    // Our Rust code was incorrectly using max_protocol_version as output for ALL merges.

    #[test]
    fn test_build_output_metadata_uses_max_of_inputs() {
        // Regression test: build_output_metadata must use max(old, new) as output version,
        // NOT max_protocol_version. max_protocol_version is only a constraint.

        // Create metadata with different versions
        let old_meta = BucketMetadata {
            ledger_version: 20,
            ext: BucketMetadataExt::V0,
        };
        let new_meta = BucketMetadata {
            ledger_version: 22,
            ext: BucketMetadataExt::V0,
        };

        // max_protocol_version is 25 (current ledger), but output should be max(20, 22) = 22
        let (version, output_meta) =
            build_output_metadata(Some(&old_meta), Some(&new_meta), 25).unwrap();

        assert_eq!(
            version, 22,
            "Output version should be max(old=20, new=22) = 22, NOT max_protocol_version=25"
        );
        assert!(output_meta.is_some());
        if let Some(BucketEntry::Metaentry(meta)) = output_meta {
            assert_eq!(meta.ledger_version, 22);
        }
    }

    #[test]
    fn test_build_output_metadata_validates_constraint() {
        // Regression test: max_protocol_version should be validated as a constraint
        let old_meta = BucketMetadata {
            ledger_version: 20,
            ext: BucketMetadataExt::V0,
        };
        let new_meta = BucketMetadata {
            ledger_version: 26, // Exceeds max_protocol_version
            ext: BucketMetadataExt::V0,
        };

        // Should fail because 26 > 25 (the constraint)
        let result = build_output_metadata(Some(&old_meta), Some(&new_meta), 25);
        assert!(
            result.is_err(),
            "Should fail when bucket version exceeds max_protocol_version constraint"
        );
    }

    #[test]
    fn test_build_output_metadata_with_only_old_meta() {
        // When only old bucket has metadata, output version = old version
        let old_meta = BucketMetadata {
            ledger_version: 18,
            ext: BucketMetadataExt::V0,
        };

        let (version, _) = build_output_metadata(Some(&old_meta), None, 25).unwrap();
        assert_eq!(version, 18, "Output version should be old bucket's version");
    }

    #[test]
    fn test_build_output_metadata_with_only_new_meta() {
        // When only new bucket has metadata, output version = new version
        let new_meta = BucketMetadata {
            ledger_version: 21,
            ext: BucketMetadataExt::V0,
        };

        let (version, _) = build_output_metadata(None, Some(&new_meta), 25).unwrap();
        assert_eq!(version, 21, "Output version should be new bucket's version");
    }

    #[test]
    fn test_build_output_metadata_no_metadata_inputs() {
        // When neither bucket has metadata (pre-protocol 11), output has no metadata
        let (version, output_meta) = build_output_metadata(None, None, 25).unwrap();
        assert_eq!(version, 0, "Version should be 0 when no metadata present");
        assert!(
            output_meta.is_none(),
            "No metadata should be output for pre-protocol-11 buckets"
        );
    }

    #[test]
    fn test_merge_in_memory_uses_max_protocol_version_directly() {
        // Regression test: merge_in_memory (level 0) uses max_protocol_version directly,
        // matching stellar-core LiveBucket::mergeInMemory behavior.

        // Create buckets with protocol version 20 metadata
        let old_meta = BucketMetadata {
            ledger_version: 20,
            ext: BucketMetadataExt::V0,
        };
        let new_meta = BucketMetadata {
            ledger_version: 20,
            ext: BucketMetadataExt::V0,
        };

        let old_entries = vec![
            BucketEntry::Metaentry(old_meta),
            BucketEntry::Liveentry(make_account_entry([1u8; 32], 100)),
        ];
        let new_entries = vec![
            BucketEntry::Metaentry(new_meta),
            BucketEntry::Liveentry(make_account_entry([2u8; 32], 200)),
        ];

        let old_bucket = Bucket::from_sorted_entries_with_in_memory(old_entries).unwrap();
        let new_bucket = Bucket::from_sorted_entries_with_in_memory(new_entries).unwrap();

        // Merge with max_protocol_version = 25 (current ledger's protocol)
        let merged = merge_in_memory(&old_bucket, &new_bucket, 25).unwrap();

        // The output metadata should use 25 (max_protocol_version), NOT 20 (max of inputs)
        let merged_entries: Vec<_> = merged.iter().collect();
        let meta_entry = merged_entries
            .iter()
            .find(|e| e.is_metadata())
            .expect("Merged bucket should have metadata");

        if let BucketEntry::Metaentry(meta) = meta_entry {
            assert_eq!(
                meta.ledger_version, 25,
                "In-memory merge should use max_protocol_version=25 directly, NOT max(old,new)=20"
            );
        } else {
            panic!("Expected metadata entry");
        }
    }

    #[test]
    fn test_disk_merge_uses_max_of_inputs() {
        // Regression test: disk-based merge (merge_buckets) uses max(old, new) for version,
        // matching stellar-core BucketBase::merge/calculateMergeProtocolVersion behavior.

        // Create buckets with different protocol versions
        let old_meta = BucketMetadata {
            ledger_version: 18,
            ext: BucketMetadataExt::V0,
        };
        let new_meta = BucketMetadata {
            ledger_version: 22,
            ext: BucketMetadataExt::V0,
        };

        let old_entries = vec![
            BucketEntry::Metaentry(old_meta),
            BucketEntry::Liveentry(make_account_entry([1u8; 32], 100)),
        ];
        let new_entries = vec![
            BucketEntry::Metaentry(new_meta),
            BucketEntry::Liveentry(make_account_entry([2u8; 32], 200)),
        ];

        let old_bucket = Bucket::from_entries(old_entries).unwrap();
        let new_bucket = Bucket::from_entries(new_entries).unwrap();

        // Merge with max_protocol_version = 25 (current ledger's protocol)
        let merged = merge_buckets(&old_bucket, &new_bucket, true, 25).unwrap();

        // The output metadata should use max(18, 22) = 22, NOT 25
        let merged_entries: Vec<_> = merged.iter().collect();
        let meta_entry = merged_entries
            .iter()
            .find(|e| e.is_metadata())
            .expect("Merged bucket should have metadata");

        if let BucketEntry::Metaentry(meta) = meta_entry {
            assert_eq!(
                meta.ledger_version, 22,
                "Disk merge should use max(old=18, new=22)=22, NOT max_protocol_version=25"
            );
        } else {
            panic!("Expected metadata entry");
        }
    }

    #[test]
    fn test_protocol_version_difference_in_memory_vs_disk() {
        // Regression test: Verify the key difference between in-memory and disk merges.
        // This is the exact scenario that caused bucket list hash divergence.

        let old_meta = BucketMetadata {
            ledger_version: 22,
            ext: BucketMetadataExt::V0,
        };
        let new_meta = BucketMetadata {
            ledger_version: 22,
            ext: BucketMetadataExt::V0,
        };

        let old_entries = vec![
            BucketEntry::Metaentry(old_meta.clone()),
            BucketEntry::Liveentry(make_account_entry([1u8; 32], 100)),
        ];
        let new_entries = vec![
            BucketEntry::Metaentry(new_meta.clone()),
            BucketEntry::Liveentry(make_account_entry([2u8; 32], 200)),
        ];

        // Create two sets of buckets
        let old_bucket_disk = Bucket::from_entries(old_entries.clone()).unwrap();
        let new_bucket_disk = Bucket::from_entries(new_entries.clone()).unwrap();

        let old_bucket_mem = Bucket::from_sorted_entries_with_in_memory(old_entries).unwrap();
        let new_bucket_mem = Bucket::from_sorted_entries_with_in_memory(new_entries).unwrap();

        // max_protocol_version = 25 (ledger's current protocol)
        let max_pv = 25u32;

        // Disk merge: uses max(old, new) = max(22, 22) = 22
        let merged_disk = merge_buckets(&old_bucket_disk, &new_bucket_disk, true, max_pv).unwrap();
        let disk_meta = merged_disk
            .iter()
            .find(|e| e.is_metadata())
            .expect("disk merged should have meta");

        // In-memory merge: uses max_protocol_version = 25 directly
        let merged_mem = merge_in_memory(&old_bucket_mem, &new_bucket_mem, max_pv).unwrap();
        let mem_meta = merged_mem
            .iter()
            .find(|e| e.is_metadata())
            .expect("mem merged should have meta");

        if let (BucketEntry::Metaentry(disk_m), BucketEntry::Metaentry(mem_m)) =
            (disk_meta, mem_meta)
        {
            assert_eq!(
                disk_m.ledger_version, 22,
                "Disk merge version should be max(inputs) = 22"
            );
            assert_eq!(
                mem_m.ledger_version, 25,
                "In-memory merge version should be max_protocol_version = 25"
            );
            assert_ne!(
                disk_m.ledger_version, mem_m.ledger_version,
                "Disk and in-memory merges should produce different versions when max_pv > max(inputs)"
            );
        } else {
            panic!("Expected metadata entries");
        }
    }

    // ============ P0-1: Shadow + Protocol Version Merge Behavior ============
    //
    // stellar-core: BucketTests.cpp "merges proceed old-style despite newer shadows"
    // Tests that shadow filtering only applies pre-protocol-12 and that
    // protocol version constraints are respected.

    #[test]
    fn test_shadow_filtering_pre_protocol_12() {
        // Pre-protocol-12: shadowed LIVE entries should be filtered out
        let shadow_entry = make_account_entry([1u8; 32], 500);
        let shadow_bucket =
            Bucket::from_entries(vec![BucketEntry::Liveentry(shadow_entry)]).unwrap();

        let old_entries = vec![BucketEntry::Liveentry(make_account_entry([1u8; 32], 100))];
        let new_entries = vec![BucketEntry::Liveentry(make_account_entry([2u8; 32], 200))];

        let old_bucket = Bucket::from_entries(old_entries).unwrap();
        let new_bucket = Bucket::from_entries(new_entries).unwrap();

        // Protocol 11 (pre-shadow-removal): entry [1] is in the shadow bucket,
        // so it should be filtered out of the merge result
        let merged = merge_buckets_with_options_and_shadows(
            &old_bucket,
            &new_bucket,
            true, // keep_dead_entries
            ProtocolVersion::V11.as_u32(),
            true, // normalize_init_entries
            &[shadow_bucket],
        )
        .unwrap();

        // Entry [1] should be filtered (shadowed), only entry [2] remains
        let key1 = make_account_key([1u8; 32]);
        let key2 = make_account_key([2u8; 32]);
        assert!(
            merged.get(&key1).unwrap().is_none(),
            "Shadowed entry should be filtered pre-protocol-12"
        );
        assert!(
            merged.get(&key2).unwrap().is_some(),
            "Non-shadowed entry should remain"
        );
    }

    #[test]
    fn test_shadow_filtering_disabled_post_protocol_12() {
        // Post-protocol-12: shadow filtering is disabled
        let shadow_entry = make_account_entry([1u8; 32], 500);
        let shadow_bucket =
            Bucket::from_entries(vec![BucketEntry::Liveentry(shadow_entry)]).unwrap();

        let old_entries = vec![BucketEntry::Liveentry(make_account_entry([1u8; 32], 100))];
        let new_entries = vec![BucketEntry::Liveentry(make_account_entry([2u8; 32], 200))];

        let old_bucket = Bucket::from_entries(old_entries).unwrap();
        let new_bucket = Bucket::from_entries(new_entries).unwrap();

        // Protocol 12 (shadow removal): shadow filtering should NOT apply
        let merged = merge_buckets_with_options_and_shadows(
            &old_bucket,
            &new_bucket,
            true,
            ProtocolVersion::V12.as_u32(), // exactly protocol 12
            true,
            &[shadow_bucket],
        )
        .unwrap();

        // Both entries should remain (no shadow filtering)
        let key1 = make_account_key([1u8; 32]);
        let key2 = make_account_key([2u8; 32]);
        assert!(
            merged.get(&key1).unwrap().is_some(),
            "Entry should NOT be filtered post-protocol-12"
        );
        assert!(merged.get(&key2).unwrap().is_some());
    }

    #[test]
    fn test_shadow_empty_shadows_is_noop() {
        // Empty shadow list should be a no-op regardless of protocol version
        let old_entries = vec![BucketEntry::Liveentry(make_account_entry([1u8; 32], 100))];
        let new_entries = vec![BucketEntry::Liveentry(make_account_entry([2u8; 32], 200))];

        let old_bucket = Bucket::from_entries(old_entries).unwrap();
        let new_bucket = Bucket::from_entries(new_entries).unwrap();

        let merged = merge_buckets_with_options_and_shadows(
            &old_bucket,
            &new_bucket,
            true,
            ProtocolVersion::V11.as_u32(),
            true,
            &[], // no shadows
        )
        .unwrap();

        assert_eq!(merged.len(), 2, "No shadow filtering with empty shadows");
    }

    // ============ P0-2: Init Entry + Shadow Interaction ============
    //
    // stellar-core: BucketTests.cpp "merging bucket entries with initentry with shadows"
    // Tests that INIT and DEAD entries are preserved even when shadowed
    // (keep_shadowed_lifecycle_entries = true for protocol >= 11).

    #[test]
    fn test_shadow_preserves_init_entries_in_init_era() {
        // In protocol 11+ (INITENTRY era), INIT entries should NOT be filtered by shadows
        let shadow_entry = make_account_entry([1u8; 32], 500);
        let shadow_bucket =
            Bucket::from_entries(vec![BucketEntry::Liveentry(shadow_entry)]).unwrap();

        let old_entries = vec![
            BucketEntry::Initentry(make_account_entry([1u8; 32], 100)), // shadowed, but INIT
            BucketEntry::Initentry(make_account_entry([2u8; 32], 200)), // not shadowed
        ];

        let old_bucket = Bucket::from_entries(old_entries).unwrap();
        let new_bucket = Bucket::from_entries(vec![]).unwrap();

        // Protocol 11 supports INITENTRY - shadow filtering should preserve INIT/DEAD
        let merged = merge_buckets_with_options_and_shadows(
            &old_bucket,
            &new_bucket,
            true,
            ProtocolVersion::V11.as_u32(), // protocol 11
            false,                         // don't normalize init entries
            &[shadow_bucket],
        )
        .unwrap();

        // Both INIT entries should be preserved (not filtered by shadows)
        let key1 = make_account_key([1u8; 32]);
        let key2 = make_account_key([2u8; 32]);

        let entry1 = merged.get(&key1).unwrap();
        assert!(
            entry1.is_some(),
            "INIT entry should NOT be filtered by shadow in INITENTRY era"
        );
        assert!(entry1.unwrap().is_init(), "Should remain INIT");

        let entry2 = merged.get(&key2).unwrap();
        assert!(entry2.is_some());
        assert!(entry2.unwrap().is_init());
    }

    #[test]
    fn test_shadow_preserves_dead_entries_in_init_era() {
        // In protocol 11+, DEAD entries should also NOT be filtered by shadows
        let shadow_entry = make_account_entry([1u8; 32], 500);
        let shadow_bucket =
            Bucket::from_entries(vec![BucketEntry::Liveentry(shadow_entry)]).unwrap();

        let old_entries = vec![
            BucketEntry::Deadentry(make_account_key([1u8; 32])), // shadowed, but DEAD
        ];

        let old_bucket = Bucket::from_entries(old_entries).unwrap();
        let new_bucket = Bucket::from_entries(vec![]).unwrap();

        let merged = merge_buckets_with_options_and_shadows(
            &old_bucket,
            &new_bucket,
            true,
            ProtocolVersion::V11.as_u32(),
            false,
            &[shadow_bucket],
        )
        .unwrap();

        let key1 = make_account_key([1u8; 32]);
        let entry = merged.get(&key1).unwrap();
        assert!(
            entry.is_some(),
            "DEAD entry should NOT be filtered by shadow in INITENTRY era"
        );
        assert!(entry.unwrap().is_dead());
    }

    #[test]
    fn test_shadow_filters_live_but_not_lifecycle_entries() {
        // In protocol 11 (pre-shadow-removal): LIVE entries are filtered by
        // shadows but INIT and DEAD entries are preserved.
        let shadow_bucket = Bucket::from_entries(vec![
            BucketEntry::Liveentry(make_account_entry([1u8; 32], 500)),
            BucketEntry::Liveentry(make_account_entry([2u8; 32], 600)),
            BucketEntry::Liveentry(make_account_entry([3u8; 32], 700)),
        ])
        .unwrap();

        let old_entries = vec![
            BucketEntry::Liveentry(make_account_entry([1u8; 32], 100)), // LIVE, shadowed → filtered
            BucketEntry::Initentry(make_account_entry([2u8; 32], 200)), // INIT, shadowed → kept
            BucketEntry::Deadentry(make_account_key([3u8; 32])),        // DEAD, shadowed → kept
        ];

        let old_bucket = Bucket::from_entries(old_entries).unwrap();
        let new_bucket = Bucket::from_entries(vec![]).unwrap();

        let merged = merge_buckets_with_options_and_shadows(
            &old_bucket,
            &new_bucket,
            true,
            ProtocolVersion::V11.as_u32(), // 11
            false,
            &[shadow_bucket],
        )
        .unwrap();

        let key1 = make_account_key([1u8; 32]);
        let key2 = make_account_key([2u8; 32]);
        let key3 = make_account_key([3u8; 32]);

        // LIVE entry [1] should be filtered (shadowed)
        assert!(
            merged.get(&key1).unwrap().is_none(),
            "LIVE entry should be filtered by shadow"
        );
        // INIT entry [2] should be preserved (lifecycle entry)
        assert!(
            merged.get(&key2).unwrap().is_some(),
            "INIT entry should be preserved despite shadow"
        );
        // DEAD entry [3] should be preserved (lifecycle entry)
        assert!(
            merged.get(&key3).unwrap().is_some(),
            "DEAD entry should be preserved despite shadow"
        );
    }

    #[test]
    fn test_shadow_does_not_revive_dead_entries() {
        // stellar-core: "shadowing does not revive dead entries"
        // Multi-level scenario: DEAD + INIT annihilation should still work
        // even with shadows present.

        // Level 5 (oldest): INIT entry for key [1]
        let b5 = Bucket::from_entries(vec![BucketEntry::Initentry(make_account_entry(
            [1u8; 32], 100,
        ))])
        .unwrap();

        // Level 4: DEAD entry for key [1]
        let b4 = Bucket::from_entries(vec![BucketEntry::Deadentry(make_account_key([1u8; 32]))])
            .unwrap();

        // Shadow bucket contains the same key (from a higher level)
        let shadow = Bucket::from_entries(vec![BucketEntry::Liveentry(make_account_entry(
            [1u8; 32], 999,
        ))])
        .unwrap();

        // Merge b4 (newer) with b5 (older) = INIT + DEAD should annihilate
        // regardless of shadow presence
        let merged = merge_buckets_with_options_and_shadows(
            &b5,
            &b4,
            true,
            ProtocolVersion::V11.as_u32(),
            false,
            &[shadow],
        )
        .unwrap();

        // INIT + DEAD should annihilate — shadow should NOT revive the entry
        let key1 = make_account_key([1u8; 32]);
        assert!(
            merged.get(&key1).unwrap().is_none(),
            "Shadow should NOT revive dead entries; INIT+DEAD still annihilates"
        );
    }

    #[test]
    fn test_shadow_does_not_eliminate_init_entries() {
        // stellar-core: shadows don't eliminate INIT entries
        // Even when an INIT entry is "shadowed" by a higher-level entry,
        // it must be kept for correct annihilation behavior at deeper levels.

        let shadow = Bucket::from_entries(vec![BucketEntry::Liveentry(make_account_entry(
            [1u8; 32], 999,
        ))])
        .unwrap();

        // Create two buckets both with INIT for key [1]
        let b1 = Bucket::from_entries(vec![BucketEntry::Initentry(make_account_entry(
            [1u8; 32], 100,
        ))])
        .unwrap();
        let b2 = Bucket::from_entries(vec![BucketEntry::Initentry(make_account_entry(
            [2u8; 32], 200,
        ))])
        .unwrap();

        let merged = merge_buckets_with_options_and_shadows(
            &b1,
            &b2,
            true,
            ProtocolVersion::V11.as_u32(),
            false,
            &[shadow],
        )
        .unwrap();

        let key1 = make_account_key([1u8; 32]);
        let entry = merged.get(&key1).unwrap();
        assert!(entry.is_some(), "Shadow should NOT eliminate INIT entries");
        assert!(entry.unwrap().is_init(), "INIT entry must remain as INIT");
    }

    // ============ P0-3: Output Iterator Version Rejection ============
    //
    // stellar-core: BucketTests.cpp "bucket output iterator rejects wrong-version entries"
    // Tests that pre-protocol-11 merges do not produce INITENTRY or METAENTRY.

    #[test]
    fn test_pre_protocol_11_merge_produces_no_metadata() {
        // Pre-protocol-11 merges should NOT produce metadata entries
        let old_entries = vec![BucketEntry::Liveentry(make_account_entry([1u8; 32], 100))];
        let new_entries = vec![BucketEntry::Liveentry(make_account_entry([2u8; 32], 200))];

        let old_bucket = Bucket::from_entries(old_entries).unwrap();
        let new_bucket = Bucket::from_entries(new_entries).unwrap();

        let merged = merge_buckets_with_options(
            &old_bucket,
            &new_bucket,
            true,
            ProtocolVersion::V10.as_u32(), // protocol 10
            true,
        )
        .unwrap();

        // No metadata in output
        let has_metadata = merged.iter().any(|e| e.is_metadata());
        assert!(
            !has_metadata,
            "Pre-protocol-11 merge should NOT produce METAENTRY"
        );
    }

    #[test]
    fn test_pre_protocol_11_merge_normalizes_init_to_live() {
        // When crossing level boundaries (normalize_init=true), INIT entries from
        // the new (incoming) bucket should be normalized to LIVE.
        // This matches stellar-core BucketOutputIterator behavior.
        let old_entries = vec![BucketEntry::Liveentry(make_account_entry([2u8; 32], 200))];
        let new_entries = vec![BucketEntry::Initentry(make_account_entry([1u8; 32], 100))];

        let old_bucket = Bucket::from_entries(old_entries).unwrap();
        let new_bucket = Bucket::from_entries(new_entries).unwrap();

        let merged = merge_buckets_with_options(
            &old_bucket,
            &new_bucket,
            true,
            25,   // current protocol
            true, // normalize_init = true (spill merge)
        )
        .unwrap();

        // The INIT entry from new_bucket should be normalized to LIVE
        let key1 = make_account_key([1u8; 32]);
        let entry = merged.get(&key1).unwrap().unwrap();
        assert!(
            entry.is_live(),
            "INIT from new bucket should be normalized to LIVE during spill merge"
        );
    }

    #[test]
    fn test_protocol_11_merge_produces_metadata() {
        // Protocol 11+: merges SHOULD produce metadata when inputs have metadata.
        // Note: build_output_metadata uses max(old, new) for version, so inputs
        // must have metadata entries for the output to include them.

        let old_meta = BucketMetadata {
            ledger_version: 11,
            ext: BucketMetadataExt::V0,
        };
        let old_with_meta = Bucket::from_entries(vec![
            BucketEntry::Metaentry(old_meta),
            BucketEntry::Liveentry(make_account_entry([1u8; 32], 100)),
        ])
        .unwrap();
        let new_with_meta = Bucket::from_entries(vec![
            BucketEntry::Metaentry(BucketMetadata {
                ledger_version: 11,
                ext: BucketMetadataExt::V0,
            }),
            BucketEntry::Liveentry(make_account_entry([2u8; 32], 200)),
        ])
        .unwrap();

        let merged = merge_buckets_with_options(
            &old_with_meta,
            &new_with_meta,
            true,
            ProtocolVersion::V11.as_u32(),
            false,
        )
        .unwrap();

        let has_metadata = merged.iter().any(|e| e.is_metadata());
        assert!(
            has_metadata,
            "Protocol-11+ merge should produce METAENTRY when inputs have metadata"
        );
    }

    #[test]
    fn test_in_memory_merge_pre_protocol_11_no_metadata() {
        // In-memory merge with pre-protocol-11 should NOT produce metadata
        let old_entries = vec![BucketEntry::Liveentry(make_account_entry([1u8; 32], 100))];
        let new_entries = vec![BucketEntry::Liveentry(make_account_entry([2u8; 32], 200))];

        let old_bucket = Bucket::from_sorted_entries_with_in_memory(old_entries).unwrap();
        let new_bucket = Bucket::from_sorted_entries_with_in_memory(new_entries).unwrap();

        let merged =
            merge_in_memory(&old_bucket, &new_bucket, ProtocolVersion::V10.as_u32()).unwrap();

        let has_metadata = merged.iter().any(|e| e.is_metadata());
        assert!(
            !has_metadata,
            "In-memory merge with pre-protocol-11 should NOT produce metadata"
        );
    }

    #[test]
    fn test_merge_counters_populated_after_merge() {
        let counters = MergeCounters::new();
        let old = Bucket::from_entries(vec![BucketEntry::Liveentry(make_account_entry(
            [1u8; 32], 100,
        ))])
        .unwrap();
        let new = Bucket::from_entries(vec![
            BucketEntry::Liveentry(make_account_entry([1u8; 32], 200)), // shadows old
            BucketEntry::Liveentry(make_account_entry([2u8; 32], 300)), // new entry
        ])
        .unwrap();

        let _result = merge_buckets_with_options_and_shadows_and_counters(
            &old,
            &new,
            true,
            TEST_PROTOCOL,
            false,
            &[],
            Some(&counters),
        )
        .unwrap();

        let snap = counters.snapshot();
        assert!(
            snap.new_live_entries > 0 || snap.new_meta_entries > 0,
            "should count entries"
        );
    }

    #[test]
    fn test_merge_counters_annihilation() {
        let counters = MergeCounters::new();
        let key = make_account_key([1u8; 32]);
        let old = Bucket::from_entries(vec![BucketEntry::Initentry(make_account_entry(
            [1u8; 32], 100,
        ))])
        .unwrap();
        let new = Bucket::from_entries(vec![BucketEntry::Deadentry(key)]).unwrap();

        let _result = merge_buckets_with_options_and_shadows_and_counters(
            &old,
            &new,
            false,
            TEST_PROTOCOL,
            false,
            &[],
            Some(&counters),
        )
        .unwrap();

        let snap = counters.snapshot();
        assert_eq!(
            snap.entries_annihilated, 1,
            "INIT+DEAD should produce annihilation"
        );
    }

    #[test]
    fn test_merge_counters_shadow_elision() {
        let counters = MergeCounters::new();
        // Create a shadow bucket that contains the key we'll try to merge
        let shadow_bucket = Bucket::from_entries(vec![BucketEntry::Liveentry(make_account_entry(
            [1u8; 32], 999,
        ))])
        .unwrap();
        let old = Bucket::from_entries(vec![BucketEntry::Liveentry(make_account_entry(
            [1u8; 32], 100,
        ))])
        .unwrap();
        let new = Bucket::from_entries(vec![]).unwrap();

        // Use pre-protocol-12 to enable shadow filtering
        let _result = merge_buckets_with_options_and_shadows_and_counters(
            &old,
            &new,
            true,
            ProtocolVersion::V11.as_u32(),
            false,
            &[shadow_bucket],
            Some(&counters),
        )
        .unwrap();

        let snap = counters.snapshot();
        assert_eq!(
            snap.old_entries_shadowed, 1,
            "entry present in shadow should be elided"
        );
    }

    #[test]
    fn test_merge_counters_file_based() {
        let counters = MergeCounters::new();
        // Include metadata so the merge produces protocol-aware output
        let meta = BucketEntry::Metaentry(BucketMetadata {
            ledger_version: TEST_PROTOCOL,
            ext: BucketMetadataExt::V1(BucketListType::Live),
        });
        let old = Bucket::from_sorted_entries(vec![
            meta.clone(),
            BucketEntry::Liveentry(make_account_entry([1u8; 32], 100)),
            BucketEntry::Liveentry(make_account_entry([3u8; 32], 300)),
        ])
        .unwrap();
        let new = Bucket::from_sorted_entries(vec![
            meta,
            BucketEntry::Liveentry(make_account_entry([2u8; 32], 200)),
        ])
        .unwrap();

        let temp_dir = tempfile::tempdir().unwrap();
        let output_path = temp_dir.path().join("merged.xdr");
        let (hash, entry_count) = merge_buckets_to_file_with_counters(
            &old,
            &new,
            &output_path,
            true,
            TEST_PROTOCOL,
            false,
            Some(&counters),
        )
        .unwrap();

        assert!(!hash.is_zero());
        // 1 metadata + 3 live entries = 4
        assert_eq!(entry_count, 4);

        let snap = counters.snapshot();
        assert!(
            snap.new_live_entries >= 3,
            "should count at least 3 live entries, got {}",
            snap.new_live_entries
        );
        assert!(
            snap.new_meta_entries >= 1,
            "should count metadata entry, got {}",
            snap.new_meta_entries
        );
    }
}
