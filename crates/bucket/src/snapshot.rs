//! Thread-safe bucket list snapshots for concurrent access.
//!
//! This module provides the infrastructure for taking consistent point-in-time
//! snapshots of the bucket list that can be safely accessed from multiple threads.
//!
//! # Architecture
//!
//! The snapshot system has three main components:
//!
//! - [`BucketSnapshot`]: A read-only wrapper around a single bucket
//! - [`BucketListSnapshot`]: A complete snapshot of the bucket list at a ledger
//! - [`BucketSnapshotManager`]: Manages current and historical snapshots
//!
//! # Thread Safety
//!
//! The [`BucketSnapshotManager`] uses a read-write lock to allow:
//! - Multiple concurrent readers (via `RwLock::read()`)
//! - Exclusive write access when updating snapshots (via `RwLock::write()`)
//!
//! This enables parallel transaction validation while the main thread continues
//! to update the canonical bucket list.
//!
//! # Usage
//!
//! ```ignore
//! use henyey_bucket::snapshot::BucketSnapshotManager;
//!
//! // Create snapshot manager with initial snapshot
//! let manager = BucketSnapshotManager::new(initial_snapshot, hot_archive_snapshot, 5);
//!
//! // Get a searchable snapshot for queries (can be called from any thread)
//! let snapshot = manager.copy_searchable_live_snapshot();
//!
//! // Query the snapshot
//! if let Some(entry) = snapshot.load(&key) {
//!     // Use the entry
//! }
//!
//! // Update snapshot when ledger closes (main thread only)
//! manager.update_current_snapshot(new_live_snapshot, new_hot_archive_snapshot);
//! ```

use crate::{
    bucket_list::BUCKET_LIST_LEVELS,
    entry::{
        get_ttl_key, is_soroban_entry, is_temporary_entry, is_ttl_expired, ledger_entry_data_type,
        ledger_key_type,
    },
    eviction::{
        update_starting_eviction_iterator, EvictionCandidate, EvictionIterator,
        EvictionIteratorExt, EvictionResult,
    },
    index::LiveBucketIndex,
    Bucket, BucketEntry, BucketEntryExt, BucketLevel, BucketList, HotArchiveBucket,
    HotArchiveBucketLevel, HotArchiveBucketList,
};
use henyey_common::protocol::MIN_SOROBAN_PROTOCOL_VERSION;
use parking_lot::RwLock;
use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;
use stellar_xdr::curr::{
    AccountId, Asset, HotArchiveBucketEntry, LedgerEntry, LedgerEntryData, LedgerEntryType,
    LedgerHeader, LedgerKey, LedgerKeyTrustLine, PoolId, StateArchivalSettings, TrustLineAsset,
};

/// A read-only snapshot of a single bucket.
///
/// This wrapper provides thread-safe access to bucket contents without
/// allowing modifications. The underlying bucket data is shared via `Arc`
/// for efficient cloning across threads.
#[derive(Clone)]
pub struct BucketSnapshot {
    bucket: Arc<Bucket>,
}

impl BucketSnapshot {
    /// Creates a new snapshot from an Arc-wrapped bucket.
    pub fn new(bucket: Arc<Bucket>) -> Self {
        Self { bucket }
    }

    /// Creates a snapshot from a cloned bucket.
    ///
    /// This wraps the bucket in an Arc for efficient sharing.
    pub fn from_bucket(bucket: Bucket) -> Self {
        Self {
            bucket: Arc::new(bucket),
        }
    }

    /// Creates a snapshot from a bucket reference by cloning.
    pub fn from_ref(bucket: &Bucket) -> Self {
        Self {
            bucket: Arc::new(bucket.clone()),
        }
    }

    /// Returns true if the bucket is empty.
    pub fn is_empty(&self) -> bool {
        self.bucket.is_empty()
    }

    /// Returns the number of entries in the bucket.
    pub fn len(&self) -> usize {
        self.bucket.len()
    }

    /// Returns the bucket's content hash.
    pub fn hash(&self) -> henyey_common::Hash256 {
        self.bucket.hash()
    }

    /// Gets an entry by key from this bucket.
    ///
    /// Returns the entry if found, or `None` if not present or on error.
    pub fn get(&self, key: &LedgerKey) -> Option<BucketEntry> {
        self.bucket.get(key).ok().flatten()
    }

    /// Gets an entry by key from this bucket, propagating errors.
    ///
    /// Unlike [`get`](Self::get) which swallows errors, this method returns
    /// a `Result` so callers can handle I/O or deserialization failures.
    pub fn get_result(&self, key: &LedgerKey) -> crate::Result<Option<BucketEntry>> {
        self.bucket.get(key)
    }

    /// Gets an entry using pre-serialized key bytes, propagating errors.
    pub fn get_result_by_key_bytes(
        &self,
        key: &LedgerKey,
        key_bytes: &[u8],
    ) -> crate::Result<Option<BucketEntry>> {
        self.bucket.get_by_key_bytes(key, key_bytes)
    }

    /// Returns a reference to the underlying bucket.
    pub fn raw_bucket(&self) -> &Bucket {
        &self.bucket
    }

    /// Loads entries for the given keys.
    ///
    /// For each key found, the entry is added to `result` and the key is removed
    /// from `keys`. This allows efficient multi-key lookups across multiple buckets.
    pub fn load_keys(&self, keys: &mut Vec<LedgerKey>, result: &mut Vec<LedgerEntry>) {
        keys.retain(|key| {
            if let Some(entry) = self.bucket.get(key).ok().flatten() {
                match entry {
                    BucketEntry::Liveentry(e) | BucketEntry::Initentry(e) => {
                        result.push(e);
                        false // Remove from keys
                    }
                    BucketEntry::Deadentry(_) => {
                        false // Key is dead, remove from search but don't add to result
                    }
                    BucketEntry::Metaentry(_) => {
                        true // Keep searching
                    }
                }
            } else {
                true // Keep in keys, continue searching
            }
        });
    }
}

/// A read-only snapshot of a single hot archive bucket.
#[derive(Clone)]
pub struct HotArchiveBucketSnapshot {
    bucket: Arc<HotArchiveBucket>,
}

impl HotArchiveBucketSnapshot {
    /// Creates a new snapshot from an Arc-wrapped hot archive bucket.
    pub fn new(bucket: Arc<HotArchiveBucket>) -> Self {
        Self { bucket }
    }

    /// Creates a snapshot from an owned bucket.
    pub fn from_bucket(bucket: HotArchiveBucket) -> Self {
        Self {
            bucket: Arc::new(bucket),
        }
    }

    /// Creates a snapshot from a bucket reference by cloning.
    pub fn from_ref(bucket: &HotArchiveBucket) -> Self {
        Self {
            bucket: Arc::new(bucket.clone()),
        }
    }

    /// Returns true if the bucket is empty.
    pub fn is_empty(&self) -> bool {
        self.bucket.is_empty()
    }

    /// Returns the number of entries in the bucket.
    pub fn len(&self) -> usize {
        self.bucket.len()
    }

    /// Returns the bucket's content hash.
    pub fn hash(&self) -> henyey_common::Hash256 {
        self.bucket.hash()
    }

    /// Returns a reference to the underlying bucket.
    pub fn raw_bucket(&self) -> &HotArchiveBucket {
        &self.bucket
    }

    /// Loads hot archive entries for the given keys.
    ///
    /// For each key found, the entry is added to `result` and the key is removed
    /// from `keys`. This allows efficient multi-key lookups across multiple buckets.
    ///
    /// Returns `HotArchiveBucketEntry` variants:
    /// - `Archived(LedgerEntry)` — entry is in the archive
    /// - `Live(LedgerKey)` — entry was restored (not in archive)
    pub fn load_keys(&self, keys: &mut Vec<LedgerKey>, result: &mut Vec<HotArchiveBucketEntry>) {
        keys.retain(|key| {
            if let Ok(Some(entry)) = self.bucket.get(key) {
                match &entry {
                    HotArchiveBucketEntry::Archived(_) => {
                        result.push(entry);
                        false // Remove from keys
                    }
                    HotArchiveBucketEntry::Live(_) => {
                        // Entry was restored — treat as "not in archive"
                        false // Remove from keys, don't add to result
                    }
                    HotArchiveBucketEntry::Metaentry(_) => {
                        true // Keep searching
                    }
                }
            } else {
                true // Keep in keys, continue searching
            }
        });
    }
}

/// Snapshot of a single bucket list level.
#[derive(Clone)]
pub struct BucketLevelSnapshot {
    /// Current bucket snapshot
    pub curr: BucketSnapshot,
    /// Next bucket snapshot (pending merge result)
    pub next: Option<BucketSnapshot>,
    /// Snapshot bucket snapshot
    pub snap: BucketSnapshot,
}

impl BucketLevelSnapshot {
    /// Creates a new level snapshot from a bucket level.
    ///
    /// Uses `Arc::clone` for curr and snap (zero-cost reference count increment)
    /// instead of deep-cloning the bucket data.
    pub fn from_level(level: &BucketLevel) -> Self {
        Self {
            curr: BucketSnapshot::new(Arc::clone(&level.curr)),
            next: level.next().map(BucketSnapshot::from_ref),
            snap: BucketSnapshot::new(Arc::clone(&level.snap)),
        }
    }
}

/// Snapshot of a single hot archive bucket list level.
#[derive(Clone)]
pub struct HotArchiveBucketLevelSnapshot {
    /// Current bucket snapshot
    pub curr: HotArchiveBucketSnapshot,
    /// Snapshot bucket snapshot
    pub snap: HotArchiveBucketSnapshot,
}

impl HotArchiveBucketLevelSnapshot {
    /// Creates a new level snapshot from a hot archive bucket level.
    pub fn from_level(level: &HotArchiveBucketLevel) -> Self {
        Self {
            curr: HotArchiveBucketSnapshot::from_ref(&level.curr),
            snap: HotArchiveBucketSnapshot::from_ref(&level.snap),
        }
    }
}

/// A complete snapshot of the live bucket list at a specific ledger.
///
/// This captures the state of all 11 levels at the time the snapshot was taken,
/// along with the ledger header that corresponds to this state.
///
/// Per-bucket caches are stored inside each `DiskBucket` and handle caching
/// transparently via `bucket.get()`. No global cache is needed here.
#[derive(Clone)]
pub struct BucketListSnapshot {
    levels: Vec<BucketLevelSnapshot>,
    header: LedgerHeader,
}

impl BucketListSnapshot {
    /// Creates a new snapshot from a bucket list and header.
    ///
    /// Per-bucket caches are stored inside each DiskBucket and shared via Arc,
    /// so snapshot lookups automatically benefit from caching.
    pub fn new(bucket_list: &BucketList, header: LedgerHeader) -> Self {
        let levels = bucket_list
            .levels()
            .iter()
            .map(BucketLevelSnapshot::from_level)
            .collect();
        Self { levels, header }
    }

    /// Returns the ledger sequence number for this snapshot.
    pub fn ledger_seq(&self) -> u32 {
        self.header.ledger_seq
    }

    /// Returns the ledger header for this snapshot.
    pub fn ledger_header(&self) -> &LedgerHeader {
        &self.header
    }

    /// Returns the level snapshots.
    pub fn levels(&self) -> &[BucketLevelSnapshot] {
        &self.levels
    }

    /// Looks up an entry by key in this snapshot.
    ///
    /// Searches from level 0 (most recent) to level 10 (oldest), returning
    /// the first live entry found or `None` if the key is dead or not present.
    /// Per-bucket caches inside each DiskBucket handle caching transparently.
    ///
    /// Returns `None` immediately for OFFER keys (matching stellar-core's
    /// `LiveBucketIndex::typeNotSupported`).
    pub fn get(&self, key: &LedgerKey) -> Option<LedgerEntry> {
        self.get_result(key).ok().flatten()
    }

    /// Looks up an entry by key in this snapshot, propagating errors.
    ///
    /// Like [`get`](Self::get) but returns a `Result` instead of swallowing
    /// I/O or deserialization errors from disk-backed buckets.
    /// Per-bucket caches inside each DiskBucket handle caching transparently.
    ///
    /// Returns `Ok(None)` immediately for OFFER keys.
    pub fn get_result(&self, key: &LedgerKey) -> crate::Result<Option<LedgerEntry>> {
        if LiveBucketIndex::type_not_supported(ledger_key_type(key)) {
            return Ok(None);
        }

        use stellar_xdr::curr::{Limits, WriteXdr};
        let key_bytes = key.to_xdr(Limits::none()).map_err(|e| {
            crate::BucketError::Serialization(format!("Failed to serialize key: {}", e))
        })?;
        for level in &self.levels {
            for bucket in [&level.curr, &level.snap] {
                if let Some(entry) = bucket.get_result_by_key_bytes(key, &key_bytes)? {
                    match entry {
                        BucketEntry::Liveentry(e) | BucketEntry::Initentry(e) => {
                            return Ok(Some(e))
                        }
                        BucketEntry::Deadentry(_) => return Ok(None),
                        BucketEntry::Metaentry(_) => continue,
                    }
                }
            }
        }
        Ok(None)
    }

    /// Batch-loads multiple entries by their keys in a single pass through the bucket list.
    ///
    /// Pre-serializes all keys once and searches through levels from newest to oldest.
    /// Keys are removed from the search set as they are found (or confirmed dead),
    /// allowing early termination when all keys are resolved.
    /// Per-bucket caches inside each DiskBucket handle caching transparently.
    ///
    /// OFFER keys are skipped (matching stellar-core's `typeNotSupported`).
    pub fn load_keys_result(&self, keys: &[LedgerKey]) -> crate::Result<Vec<LedgerEntry>> {
        use stellar_xdr::curr::{Limits, WriteXdr};

        let mut result = Vec::with_capacity(keys.len());

        // Filter out unsupported types (OFFER).
        let keys: Vec<&LedgerKey> = keys
            .iter()
            .filter(|k| !LiveBucketIndex::type_not_supported(ledger_key_type(k)))
            .collect();

        if keys.is_empty() {
            return Ok(result);
        }

        // Pre-serialize all keys once
        let mut remaining: Vec<(&LedgerKey, Vec<u8>)> = keys
            .iter()
            .map(|k| {
                let bytes = k.to_xdr(Limits::none()).map_err(|e| {
                    crate::BucketError::Serialization(format!("Failed to serialize key: {}", e))
                })?;
                Ok((*k, bytes))
            })
            .collect::<crate::Result<Vec<_>>>()?;

        for level in &self.levels {
            if remaining.is_empty() {
                break;
            }
            for bucket in [&level.curr, &level.snap] {
                if remaining.is_empty() {
                    break;
                }
                let mut bucket_err = None;
                remaining.retain(|(key, key_bytes)| {
                    if bucket_err.is_some() {
                        return true;
                    }
                    match bucket.get_result_by_key_bytes(key, key_bytes) {
                        Ok(Some(entry)) => match entry {
                            BucketEntry::Liveentry(ref e) | BucketEntry::Initentry(ref e) => {
                                result.push(e.clone());
                                false
                            }
                            BucketEntry::Deadentry(_) => false,
                            BucketEntry::Metaentry(_) => true,
                        },
                        Ok(None) => true,
                        Err(e) => {
                            bucket_err = Some(e);
                            true
                        }
                    }
                });
                if let Some(e) = bucket_err {
                    return Err(e);
                }
            }
        }

        Ok(result)
    }

    /// Loads multiple entries by their keys.
    ///
    /// Returns a vector of found entries. Keys that are not found or are dead
    /// are not included in the result.
    pub fn load_keys(&self, keys: &[LedgerKey]) -> Vec<LedgerEntry> {
        let mut remaining_keys: Vec<LedgerKey> = keys.to_vec();
        let mut result = Vec::new();

        for level in &self.levels {
            if remaining_keys.is_empty() {
                break;
            }
            level.curr.load_keys(&mut remaining_keys, &mut result);
            if remaining_keys.is_empty() {
                break;
            }
            level.snap.load_keys(&mut remaining_keys, &mut result);
        }

        result
    }

    /// Run an incremental eviction scan on this snapshot.
    ///
    /// This is the snapshot-based equivalent of [`BucketList::scan_for_eviction_incremental`].
    /// It performs the scan phase of eviction, collecting candidates within the byte budget.
    /// The result must be resolved via [`EvictionResult::resolve`] to apply TTL filtering
    /// and max_entries limits.
    ///
    /// Used by the background eviction scan: a snapshot is taken after committing ledger N,
    /// and the scan runs on a background thread targeting ledger N+1. When N+1 arrives,
    /// the result is resolved with that ledger's modified TTL keys.
    pub fn scan_for_eviction_incremental(
        &self,
        mut iter: EvictionIterator,
        current_ledger: u32,
        settings: &StateArchivalSettings,
    ) -> crate::Result<EvictionResult> {
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

            let bucket = if iter.is_curr_bucket {
                self.levels[level].curr.raw_bucket()
            } else {
                self.levels[level].snap.raw_bucket()
            };

            let (_entries_scanned, bytes_used, finished_bucket) = self.scan_bucket_region(
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
    /// This is the snapshot-based equivalent of [`BucketList::scan_bucket_region`].
    /// Uses `self.get_result()` for TTL and entry lookups instead of `BucketList::get()`.
    fn scan_bucket_region(
        &self,
        bucket: &Bucket,
        iter: &mut EvictionIterator,
        max_bytes: u64,
        current_ledger: u32,
        candidates: &mut Vec<EvictionCandidate>,
        seen_keys: &mut HashSet<LedgerKey>,
    ) -> crate::Result<(usize, u64, bool)> {
        let mut entries_scanned = 0;
        let mut bytes_used = 0u64;

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
                        // Parity: stellar-core ignores DEAD entries in scan
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

                // Look up TTL entry from the snapshot (instead of BucketList::get)
                let Some(ttl_entry) = self.get_result(&ttl_key)? else {
                    break 'process;
                };

                let Some(is_expired) = is_ttl_expired(&ttl_entry, current_ledger) else {
                    break 'process;
                };

                if !is_expired {
                    break 'process;
                }

                // Entry is expired — collect as eviction candidate.
                // For persistent entries, archive the NEWEST version from the snapshot.
                let is_temp = is_temporary_entry(live_entry);
                let entry_for_candidate = if !is_temp {
                    if let Some(newest_entry) = self.get_result(&key)? {
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
}

/// A complete snapshot of the hot archive bucket list at a specific ledger.
#[derive(Clone)]
pub struct HotArchiveBucketListSnapshot {
    levels: Vec<HotArchiveBucketLevelSnapshot>,
    header: LedgerHeader,
}

impl HotArchiveBucketListSnapshot {
    /// Creates a new snapshot from a hot archive bucket list and header.
    pub fn new(bucket_list: &HotArchiveBucketList, header: LedgerHeader) -> Self {
        let levels = bucket_list
            .levels()
            .iter()
            .map(HotArchiveBucketLevelSnapshot::from_level)
            .collect();
        Self { levels, header }
    }

    /// Returns the ledger sequence number for this snapshot.
    pub fn ledger_seq(&self) -> u32 {
        self.header.ledger_seq
    }

    /// Returns the ledger header for this snapshot.
    pub fn ledger_header(&self) -> &LedgerHeader {
        &self.header
    }

    /// Returns the level snapshots.
    pub fn levels(&self) -> &[HotArchiveBucketLevelSnapshot] {
        &self.levels
    }

    /// Batch-loads archived entries by their keys.
    ///
    /// Searches from level 0 (most recent) to the highest level, returning
    /// `HotArchiveBucketEntry::Archived` entries found. Keys that resolve to
    /// `Live` (restored) are removed without producing output.
    pub fn load_keys(&self, keys: &[LedgerKey]) -> Vec<HotArchiveBucketEntry> {
        let mut remaining = keys.to_vec();
        let mut result = Vec::new();

        for level in &self.levels {
            if remaining.is_empty() {
                break;
            }
            for bucket in [&level.curr, &level.snap] {
                if remaining.is_empty() {
                    break;
                }
                bucket.load_keys(&mut remaining, &mut result);
            }
        }

        result
    }
}

/// A searchable wrapper around a bucket list snapshot.
///
/// This provides a higher-level interface for querying the snapshot,
/// with support for historical ledger lookups.
pub struct SearchableBucketListSnapshot {
    snapshot: BucketListSnapshot,
    historical_snapshots: BTreeMap<u32, BucketListSnapshot>,
}

impl SearchableBucketListSnapshot {
    /// Creates a new searchable snapshot.
    pub fn new(
        snapshot: BucketListSnapshot,
        historical_snapshots: BTreeMap<u32, BucketListSnapshot>,
    ) -> Self {
        Self {
            snapshot,
            historical_snapshots,
        }
    }

    /// Returns the current ledger sequence number.
    pub fn ledger_seq(&self) -> u32 {
        self.snapshot.ledger_seq()
    }

    /// Returns the current ledger header.
    pub fn ledger_header(&self) -> &LedgerHeader {
        self.snapshot.ledger_header()
    }

    /// Loads a single entry by key from the current snapshot.
    pub fn load(&self, key: &LedgerKey) -> Option<LedgerEntry> {
        self.snapshot.get(key)
    }

    /// Loads multiple entries by their keys from the current snapshot.
    pub fn load_keys(&self, keys: &[LedgerKey]) -> Vec<LedgerEntry> {
        self.snapshot.load_keys(keys)
    }

    /// Loads entries from a specific historical ledger.
    ///
    /// Returns `None` if the requested ledger is not available in the
    /// historical snapshots.
    pub fn load_keys_from_ledger(
        &self,
        keys: &[LedgerKey],
        ledger_seq: u32,
    ) -> Option<Vec<LedgerEntry>> {
        if ledger_seq == self.snapshot.ledger_seq() {
            return Some(self.snapshot.load_keys(keys));
        }

        self.historical_snapshots
            .get(&ledger_seq)
            .map(|snap| snap.load_keys(keys))
    }

    /// Returns the range of available ledger sequences.
    ///
    /// Returns `(oldest, newest)` where `oldest` is the oldest historical
    /// ledger available and `newest` is the current ledger.
    pub fn available_ledger_range(&self) -> (u32, u32) {
        let oldest = self
            .historical_snapshots
            .keys()
            .next()
            .copied()
            .unwrap_or(self.snapshot.ledger_seq());
        (oldest, self.snapshot.ledger_seq())
    }

    /// Scans all entries of a specific type in the bucket list.
    ///
    /// This iterates through all buckets (from level 0 to level 10, curr then snap)
    /// and invokes the callback for each entry matching the specified type.
    ///
    /// # Arguments
    ///
    /// * `entry_type` - The ledger entry type to filter for
    /// * `callback` - Function called for each matching entry. Return `false` to stop iteration.
    ///
    /// # Returns
    ///
    /// `true` if iteration completed, `false` if stopped early by callback.
    ///
    /// # Example
    ///
    /// ```ignore
    /// snapshot.scan_for_entries_of_type(LedgerEntryType::Account, |entry| {
    ///     println!("Found account: {:?}", entry);
    ///     true // continue iteration
    /// });
    /// ```
    pub fn scan_for_entries_of_type<F>(
        &self,
        entry_type: LedgerEntryType,
        mut callback: F,
    ) -> crate::Result<bool>
    where
        F: FnMut(&BucketEntry) -> bool,
    {
        // Track seen keys to avoid processing same key multiple times
        // (older buckets may have outdated versions)
        let mut seen_keys: HashSet<LedgerKey> = HashSet::new();

        for level in &self.snapshot.levels {
            // Process buckets in order (newer to older): curr then snap
            for bucket in [&level.curr, &level.snap] {
                // Iterate through all entries in the bucket
                for entry_result in bucket.raw_bucket().iter()? {
                    let bucket_entry = entry_result?;
                    // Check if this entry matches the requested type
                    if let Some(key) = bucket_entry.key() {
                        // Skip if we've already seen a newer version of this key
                        if seen_keys.contains(&key) {
                            continue;
                        }

                        // Check entry type
                        let matches_type = match &bucket_entry {
                            BucketEntry::Liveentry(e) | BucketEntry::Initentry(e) => {
                                ledger_entry_data_type(&e.data) == entry_type
                            }
                            BucketEntry::Deadentry(k) => ledger_key_type(k) == entry_type,
                            BucketEntry::Metaentry(_) => false,
                        };

                        if matches_type {
                            seen_keys.insert(key);

                            // Skip dead entries for callback
                            if !bucket_entry.is_dead() && !callback(&bucket_entry) {
                                return Ok(false);
                            }
                        }
                    }
                }
            }
        }
        Ok(true)
    }

    /// Finds inflation winners from the bucket list.
    ///
    /// Scans all account entries to find those that have set an inflation destination,
    /// then aggregates the votes (balances) for each destination.
    ///
    /// # Arguments
    ///
    /// * `max_winners` - Maximum number of winners to return
    /// * `min_balance` - Minimum vote count (sum of balances) to be considered a winner
    ///
    /// # Returns
    ///
    /// A vector of inflation winners sorted by vote count (descending).
    ///
    /// # Note
    ///
    /// This is a legacy query method. In modern Stellar (protocol 12+), inflation
    /// has been deprecated. This method is provided for historical compatibility.
    pub fn load_inflation_winners(
        &self,
        max_winners: usize,
        min_balance: i64,
    ) -> crate::Result<Vec<InflationWinner>> {
        use std::collections::HashMap;

        // Track seen accounts to avoid double-counting across bucket levels
        let mut seen_accounts: HashSet<AccountId> = HashSet::new();

        // Map from inflation destination to total votes
        let mut vote_counts: HashMap<AccountId, i64> = HashMap::new();

        // Scan all account entries
        for level in &self.snapshot.levels {
            for bucket in [&level.curr, &level.snap] {
                for entry_result in bucket.raw_bucket().iter()? {
                    let bucket_entry = entry_result?;
                    match &bucket_entry {
                        BucketEntry::Liveentry(entry) | BucketEntry::Initentry(entry) => {
                            if let LedgerEntryData::Account(account) = &entry.data {
                                // Skip if we've already seen this account
                                if seen_accounts.contains(&account.account_id) {
                                    continue;
                                }
                                seen_accounts.insert(account.account_id.clone());

                                // Only count accounts with an inflation destination
                                // and balance >= 100 XLM (1,000,000,000 stroops).
                                // Spec: BUCKETLISTDB_SPEC §10.6 — minimum balance for
                                // vote counting is hardcoded at 100 XLM, independent of
                                // the minBalance parameter.
                                if let Some(dest) = &account.inflation_dest {
                                    if account.balance >= 1_000_000_000 {
                                        *vote_counts.entry(dest.clone()).or_insert(0) +=
                                            account.balance;
                                    }
                                }
                            }
                        }
                        BucketEntry::Deadentry(key) => {
                            // Mark account as seen (it's deleted)
                            if let LedgerKey::Account(k) = key {
                                seen_accounts.insert(k.account_id.clone());
                            }
                        }
                        BucketEntry::Metaentry(_) => {}
                    }
                }
            }
        }

        // Filter and sort winners
        let mut winners: Vec<InflationWinner> = vote_counts
            .into_iter()
            .filter(|(_, votes)| *votes >= min_balance)
            .map(|(account_id, votes)| InflationWinner { account_id, votes })
            .collect();

        // Sort by votes descending
        winners.sort_by(|a, b| b.votes.cmp(&a.votes));

        // Truncate to max_winners
        winners.truncate(max_winners);

        Ok(winners)
    }

    /// Loads pool share trustlines for an account and asset.
    ///
    /// This finds all pool share trustlines that the given account has for
    /// liquidity pools that contain the specified asset. This is useful for
    /// determining which liquidity pools an account participates in for a
    /// given asset.
    ///
    /// # Algorithm
    ///
    /// 1. Scan all liquidity pool entries to find pools containing the asset
    /// 2. Build trustline keys for (account_id, pool_id) pairs
    /// 3. Load those trustlines from the bucket list
    ///
    /// # Arguments
    ///
    /// * `account_id` - The account to query
    /// * `asset` - The asset to find pool share trustlines for
    ///
    /// # Returns
    ///
    /// A vector of trustline entries representing pool shares for pools
    /// containing the given asset.
    pub fn load_pool_share_trustlines_by_account_and_asset(
        &self,
        account_id: &AccountId,
        asset: &Asset,
    ) -> crate::Result<Vec<LedgerEntry>> {
        // First, find all pool IDs containing the asset
        let pool_ids = self.collect_pool_ids_for_asset(asset)?;

        if pool_ids.is_empty() {
            return Ok(Vec::new());
        }

        // Build trustline keys for each pool
        let trustline_keys: Vec<LedgerKey> = pool_ids
            .iter()
            .map(|pool_id| {
                LedgerKey::Trustline(LedgerKeyTrustLine {
                    account_id: account_id.clone(),
                    asset: TrustLineAsset::PoolShare(pool_id.clone()),
                })
            })
            .collect();

        // Load the trustlines
        Ok(self.snapshot.load_keys(&trustline_keys))
    }

    /// Collects all pool IDs that contain a given asset.
    ///
    /// Scans all liquidity pool entries to find pools where either asset_a
    /// or asset_b matches the given asset.
    fn collect_pool_ids_for_asset(&self, asset: &Asset) -> crate::Result<Vec<PoolId>> {
        let mut pool_ids = Vec::new();
        let mut seen_pools: HashSet<PoolId> = HashSet::new();

        for level in &self.snapshot.levels {
            for bucket in [&level.curr, &level.snap] {
                for entry_result in bucket.raw_bucket().iter()? {
                    let bucket_entry = entry_result?;
                    match &bucket_entry {
                        BucketEntry::Liveentry(entry) | BucketEntry::Initentry(entry) => {
                            if let LedgerEntryData::LiquidityPool(pool) = &entry.data {
                                // Skip if already seen
                                if seen_pools.contains(&pool.liquidity_pool_id) {
                                    continue;
                                }

                                // Check if pool contains the asset
                                let stellar_xdr::curr::LiquidityPoolEntryBody::LiquidityPoolConstantProduct(
                                    cp,
                                ) = &pool.body;
                                if &cp.params.asset_a == asset || &cp.params.asset_b == asset {
                                    seen_pools.insert(pool.liquidity_pool_id.clone());
                                    pool_ids.push(pool.liquidity_pool_id.clone());
                                }
                            }
                        }
                        BucketEntry::Deadentry(key) => {
                            // Mark pool as seen if it's deleted
                            if let LedgerKey::LiquidityPool(k) = key {
                                seen_pools.insert(k.liquidity_pool_id.clone());
                            }
                        }
                        BucketEntry::Metaentry(_) => {}
                    }
                }
            }
        }

        Ok(pool_ids)
    }

    /// Loads all trustline entries for an account.
    ///
    /// Scans the bucket list for all trustline entries belonging to the
    /// given account.
    pub fn load_trustlines_for_account(
        &self,
        account_id: &AccountId,
    ) -> crate::Result<Vec<LedgerEntry>> {
        let mut trustlines = Vec::new();
        let mut seen_keys: HashSet<LedgerKey> = HashSet::new();

        for level in &self.snapshot.levels {
            for bucket in [&level.curr, &level.snap] {
                for entry_result in bucket.raw_bucket().iter()? {
                    let bucket_entry = entry_result?;
                    match &bucket_entry {
                        BucketEntry::Liveentry(entry) | BucketEntry::Initentry(entry) => {
                            if let LedgerEntryData::Trustline(tl) = &entry.data {
                                if &tl.account_id == account_id {
                                    let key = LedgerKey::Trustline(LedgerKeyTrustLine {
                                        account_id: tl.account_id.clone(),
                                        asset: tl.asset.clone(),
                                    });
                                    if !seen_keys.contains(&key) {
                                        seen_keys.insert(key);
                                        trustlines.push(entry.clone());
                                    }
                                }
                            }
                        }
                        BucketEntry::Deadentry(key) => {
                            if let LedgerKey::Trustline(k) = key {
                                if &k.account_id == account_id {
                                    seen_keys.insert(key.clone());
                                }
                            }
                        }
                        BucketEntry::Metaentry(_) => {}
                    }
                }
            }
        }

        Ok(trustlines)
    }
}

/// An account that has received inflation votes.
///
/// Used by [`SearchableBucketListSnapshot::load_inflation_winners`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InflationWinner {
    /// The account that is receiving votes (inflation destination).
    pub account_id: AccountId,
    /// Total votes (sum of balances of accounts voting for this destination).
    pub votes: i64,
}

/// A searchable wrapper around a hot archive bucket list snapshot.
pub struct SearchableHotArchiveBucketListSnapshot {
    snapshot: HotArchiveBucketListSnapshot,
    _historical_snapshots: BTreeMap<u32, HotArchiveBucketListSnapshot>,
}

impl SearchableHotArchiveBucketListSnapshot {
    /// Creates a new searchable hot archive snapshot.
    pub fn new(
        snapshot: HotArchiveBucketListSnapshot,
        historical_snapshots: BTreeMap<u32, HotArchiveBucketListSnapshot>,
    ) -> Self {
        Self {
            snapshot,
            _historical_snapshots: historical_snapshots,
        }
    }

    /// Returns the current ledger sequence number.
    pub fn ledger_seq(&self) -> u32 {
        self.snapshot.ledger_seq()
    }

    /// Returns the current ledger header.
    pub fn ledger_header(&self) -> &LedgerHeader {
        self.snapshot.ledger_header()
    }

    /// Loads archived entries by their keys from the current snapshot.
    pub fn load_keys(&self, keys: &[LedgerKey]) -> Vec<HotArchiveBucketEntry> {
        self.snapshot.load_keys(keys)
    }

    /// Loads archived entries from a specific historical ledger.
    ///
    /// Returns `None` if the requested ledger is not available in the
    /// historical snapshots.
    pub fn load_keys_from_ledger(
        &self,
        keys: &[LedgerKey],
        ledger_seq: u32,
    ) -> Option<Vec<HotArchiveBucketEntry>> {
        if ledger_seq == self.snapshot.ledger_seq() {
            return Some(self.snapshot.load_keys(keys));
        }

        self._historical_snapshots
            .get(&ledger_seq)
            .map(|snap| snap.load_keys(keys))
    }
}

/// Manages bucket list snapshots for thread-safe concurrent access.
///
/// This is the main entry point for obtaining snapshots of the bucket list.
/// It maintains:
///
/// - The current canonical snapshot (updated by the main thread)
/// - A configurable number of historical snapshots for point-in-time queries
///
/// # Thread Safety
///
/// - `copy_searchable_*` methods acquire a read lock
/// - `update_current_snapshot` acquires a write lock
///
/// Multiple threads can call `copy_searchable_*` concurrently, but
/// `update_current_snapshot` blocks all readers until complete.
pub struct BucketSnapshotManager {
    /// Current live bucket list snapshot
    current_live: RwLock<Option<BucketListSnapshot>>,
    /// Current hot archive bucket list snapshot
    current_hot_archive: RwLock<Option<HotArchiveBucketListSnapshot>>,
    /// Historical live snapshots (ledger_seq -> snapshot)
    live_historical: RwLock<BTreeMap<u32, BucketListSnapshot>>,
    /// Historical hot archive snapshots (ledger_seq -> snapshot)
    hot_archive_historical: RwLock<BTreeMap<u32, HotArchiveBucketListSnapshot>>,
    /// Maximum number of historical snapshots to retain
    num_historical_snapshots: u32,
}

impl BucketSnapshotManager {
    /// Creates a new snapshot manager with initial snapshots.
    ///
    /// # Arguments
    ///
    /// * `live_snapshot` - Initial live bucket list snapshot
    /// * `hot_archive_snapshot` - Initial hot archive bucket list snapshot
    /// * `num_historical_snapshots` - Number of historical snapshots to retain
    pub fn new(
        live_snapshot: BucketListSnapshot,
        hot_archive_snapshot: HotArchiveBucketListSnapshot,
        num_historical_snapshots: u32,
    ) -> Self {
        Self {
            current_live: RwLock::new(Some(live_snapshot)),
            current_hot_archive: RwLock::new(Some(hot_archive_snapshot)),
            live_historical: RwLock::new(BTreeMap::new()),
            hot_archive_historical: RwLock::new(BTreeMap::new()),
            num_historical_snapshots,
        }
    }

    /// Creates an empty snapshot manager.
    ///
    /// Use `update_current_snapshot` to set the initial snapshots.
    pub fn empty(num_historical_snapshots: u32) -> Self {
        Self {
            current_live: RwLock::new(None),
            current_hot_archive: RwLock::new(None),
            live_historical: RwLock::new(BTreeMap::new()),
            hot_archive_historical: RwLock::new(BTreeMap::new()),
            num_historical_snapshots,
        }
    }

    /// Returns a copy of the current searchable live bucket list snapshot.
    ///
    /// This method acquires a read lock and is safe to call from any thread.
    pub fn copy_searchable_live_snapshot(&self) -> Option<SearchableBucketListSnapshot> {
        let snapshot = self.current_live.read().clone()?;
        let historical = self.live_historical.read().clone();
        Some(SearchableBucketListSnapshot::new(snapshot, historical))
    }

    /// Returns a copy of the current searchable hot archive snapshot.
    ///
    /// This method acquires a read lock and is safe to call from any thread.
    pub fn copy_searchable_hot_archive_snapshot(
        &self,
    ) -> Option<SearchableHotArchiveBucketListSnapshot> {
        let snapshot = self.current_hot_archive.read().clone()?;
        let historical = self.hot_archive_historical.read().clone();
        Some(SearchableHotArchiveBucketListSnapshot::new(
            snapshot, historical,
        ))
    }

    /// Copies both live and hot archive snapshots atomically.
    ///
    /// This ensures both snapshots correspond to the same ledger state,
    /// which is important when querying both snapshot types together.
    pub fn copy_live_and_hot_archive_snapshots(
        &self,
    ) -> Option<(
        SearchableBucketListSnapshot,
        SearchableHotArchiveBucketListSnapshot,
    )> {
        // Acquire both locks to ensure consistency
        let live = self.current_live.read().clone()?;
        let hot_archive = self.current_hot_archive.read().clone()?;
        let live_historical = self.live_historical.read().clone();
        let hot_archive_historical = self.hot_archive_historical.read().clone();

        Some((
            SearchableBucketListSnapshot::new(live, live_historical),
            SearchableHotArchiveBucketListSnapshot::new(hot_archive, hot_archive_historical),
        ))
    }

    /// Updates a snapshot if a newer one is available.
    ///
    /// Returns `true` if the snapshot was updated, `false` otherwise.
    pub fn maybe_update_live_snapshot(
        &self,
        snapshot: &mut Option<SearchableBucketListSnapshot>,
    ) -> bool {
        let current = self.current_live.read();
        let needs_update = match (&*current, snapshot.as_ref()) {
            (Some(curr), Some(snap)) => curr.ledger_seq() > snap.ledger_seq(),
            (Some(_), None) => true,
            _ => false,
        };

        if needs_update {
            drop(current);
            *snapshot = self.copy_searchable_live_snapshot();
            true
        } else {
            false
        }
    }

    /// Updates the current snapshots.
    ///
    /// This method should only be called from the main thread after a ledger
    /// closes. It acquires write locks on both snapshot stores.
    ///
    /// The previous current snapshot is moved to historical storage (if
    /// historical snapshots are enabled).
    pub fn update_current_snapshot(
        &self,
        live_snapshot: BucketListSnapshot,
        hot_archive_snapshot: HotArchiveBucketListSnapshot,
    ) {
        // Update live snapshot
        {
            let mut current = self.current_live.write();
            let mut historical = self.live_historical.write();

            if self.num_historical_snapshots > 0 {
                if let Some(prev) = current.take() {
                    let ledger_seq = prev.ledger_seq();

                    // Remove oldest if at capacity
                    if historical.len() >= self.num_historical_snapshots as usize {
                        if let Some(&oldest) = historical.keys().next() {
                            historical.remove(&oldest);
                        }
                    }

                    historical.insert(ledger_seq, prev);
                }
            }

            *current = Some(live_snapshot);
        }

        // Update hot archive snapshot
        {
            let mut current = self.current_hot_archive.write();
            let mut historical = self.hot_archive_historical.write();

            if self.num_historical_snapshots > 0 {
                if let Some(prev) = current.take() {
                    let ledger_seq = prev.ledger_seq();

                    // Remove oldest if at capacity
                    if historical.len() >= self.num_historical_snapshots as usize {
                        if let Some(&oldest) = historical.keys().next() {
                            historical.remove(&oldest);
                        }
                    }

                    historical.insert(ledger_seq, prev);
                }
            }

            *current = Some(hot_archive_snapshot);
        }
    }

    /// Returns the current ledger sequence, if a snapshot exists.
    pub fn current_ledger_seq(&self) -> Option<u32> {
        self.current_live.read().as_ref().map(|s| s.ledger_seq())
    }

    /// Returns the number of historical snapshots currently stored.
    pub fn historical_snapshot_count(&self) -> usize {
        self.live_historical.read().len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use stellar_xdr::curr::*;

    fn make_test_header(ledger_seq: u32) -> LedgerHeader {
        LedgerHeader {
            ledger_version: 25,
            previous_ledger_hash: Hash([0; 32]),
            scp_value: StellarValue {
                tx_set_hash: Hash([0; 32]),
                close_time: TimePoint(0),
                upgrades: vec![].try_into().unwrap(),
                ext: StellarValueExt::Basic,
            },
            tx_set_result_hash: Hash([0; 32]),
            bucket_list_hash: Hash([0; 32]),
            ledger_seq,
            total_coins: 0,
            fee_pool: 0,
            inflation_seq: 0,
            id_pool: 0,
            base_fee: 100,
            base_reserve: 5000000,
            max_tx_set_size: 100,
            skip_list: [Hash([0; 32]), Hash([0; 32]), Hash([0; 32]), Hash([0; 32])],
            ext: LedgerHeaderExt::V0,
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_bucket_snapshot_manager_creation() {
        let bucket_list = BucketList::new();
        let hot_archive = HotArchiveBucketList::new();
        let header = make_test_header(1);

        let live_snapshot = BucketListSnapshot::new(&bucket_list, header.clone());
        let hot_archive_snapshot = HotArchiveBucketListSnapshot::new(&hot_archive, header);

        let manager = BucketSnapshotManager::new(live_snapshot, hot_archive_snapshot, 5);

        assert_eq!(manager.current_ledger_seq(), Some(1));
        assert_eq!(manager.historical_snapshot_count(), 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_snapshot_update_maintains_history() {
        let bucket_list = BucketList::new();
        let hot_archive = HotArchiveBucketList::new();

        let manager = BucketSnapshotManager::new(
            BucketListSnapshot::new(&bucket_list, make_test_header(1)),
            HotArchiveBucketListSnapshot::new(&hot_archive, make_test_header(1)),
            3,
        );

        // Update to ledger 2
        manager.update_current_snapshot(
            BucketListSnapshot::new(&bucket_list, make_test_header(2)),
            HotArchiveBucketListSnapshot::new(&hot_archive, make_test_header(2)),
        );
        assert_eq!(manager.current_ledger_seq(), Some(2));
        assert_eq!(manager.historical_snapshot_count(), 1);

        // Update to ledger 3
        manager.update_current_snapshot(
            BucketListSnapshot::new(&bucket_list, make_test_header(3)),
            HotArchiveBucketListSnapshot::new(&hot_archive, make_test_header(3)),
        );
        assert_eq!(manager.current_ledger_seq(), Some(3));
        assert_eq!(manager.historical_snapshot_count(), 2);

        // Update to ledger 4
        manager.update_current_snapshot(
            BucketListSnapshot::new(&bucket_list, make_test_header(4)),
            HotArchiveBucketListSnapshot::new(&hot_archive, make_test_header(4)),
        );
        assert_eq!(manager.current_ledger_seq(), Some(4));
        assert_eq!(manager.historical_snapshot_count(), 3);

        // Update to ledger 5 - should evict oldest (ledger 1)
        manager.update_current_snapshot(
            BucketListSnapshot::new(&bucket_list, make_test_header(5)),
            HotArchiveBucketListSnapshot::new(&hot_archive, make_test_header(5)),
        );
        assert_eq!(manager.current_ledger_seq(), Some(5));
        assert_eq!(manager.historical_snapshot_count(), 3);

        // Verify historical snapshots contain ledgers 2, 3, 4
        let snapshot = manager.copy_searchable_live_snapshot().unwrap();
        let (oldest, newest) = snapshot.available_ledger_range();
        assert_eq!(oldest, 2);
        assert_eq!(newest, 5);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_searchable_snapshot_load() {
        let mut bucket_list = BucketList::new();
        let header = make_test_header(1);

        // Add some entries
        let account_id = AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([1; 32])));
        let entry = LedgerEntry {
            last_modified_ledger_seq: 1,
            data: LedgerEntryData::Account(AccountEntry {
                account_id: account_id.clone(),
                balance: 1000,
                seq_num: SequenceNumber(1),
                num_sub_entries: 0,
                inflation_dest: None,
                flags: 0,
                home_domain: String32::default(),
                thresholds: Thresholds([1, 0, 0, 0]),
                signers: vec![].try_into().unwrap(),
                ext: AccountEntryExt::V0,
            }),
            ext: LedgerEntryExt::V0,
        };

        bucket_list
            .add_batch(1, 25, BucketListType::Live, vec![entry], vec![], vec![])
            .unwrap();

        let snapshot = BucketListSnapshot::new(&bucket_list, header);
        let searchable = SearchableBucketListSnapshot::new(snapshot, BTreeMap::new());

        // Query the entry
        let key = LedgerKey::Account(LedgerKeyAccount { account_id });
        let result = searchable.load(&key);
        assert!(result.is_some());

        if let Some(LedgerEntry {
            data: LedgerEntryData::Account(acc),
            ..
        }) = result
        {
            assert_eq!(acc.balance, 1000);
        } else {
            panic!("Expected account entry");
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_concurrent_access() {
        use std::sync::Arc;
        use std::thread;

        let bucket_list = BucketList::new();
        let hot_archive = HotArchiveBucketList::new();
        let header = make_test_header(1);

        let manager = Arc::new(BucketSnapshotManager::new(
            BucketListSnapshot::new(&bucket_list, header.clone()),
            HotArchiveBucketListSnapshot::new(&hot_archive, header),
            5,
        ));

        // Spawn multiple reader threads
        let handles: Vec<_> = (0..4)
            .map(|_| {
                let manager = Arc::clone(&manager);
                thread::spawn(move || {
                    for _ in 0..100 {
                        let snapshot = manager.copy_searchable_live_snapshot();
                        assert!(snapshot.is_some());
                    }
                })
            })
            .collect();

        // Wait for all threads
        for handle in handles {
            handle.join().unwrap();
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_scan_for_entries_of_type() {
        let mut bucket_list = BucketList::new();
        let header = make_test_header(1);

        // Add some account entries
        for i in 0..5u8 {
            let account_id = AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([i; 32])));
            let entry = LedgerEntry {
                last_modified_ledger_seq: 1,
                data: LedgerEntryData::Account(AccountEntry {
                    account_id,
                    balance: i as i64 * 100,
                    seq_num: SequenceNumber(1),
                    num_sub_entries: 0,
                    inflation_dest: None,
                    flags: 0,
                    home_domain: String32::default(),
                    thresholds: Thresholds([1, 0, 0, 0]),
                    signers: vec![].try_into().unwrap(),
                    ext: AccountEntryExt::V0,
                }),
                ext: LedgerEntryExt::V0,
            };
            bucket_list
                .add_batch(
                    1 + i as u32,
                    25,
                    BucketListType::Live,
                    vec![entry],
                    vec![],
                    vec![],
                )
                .unwrap();
        }

        let snapshot = BucketListSnapshot::new(&bucket_list, header);
        let searchable = SearchableBucketListSnapshot::new(snapshot, BTreeMap::new());

        // Count account entries
        let mut count = 0;
        searchable
            .scan_for_entries_of_type(LedgerEntryType::Account, |_| {
                count += 1;
                true // continue
            })
            .unwrap();
        assert_eq!(count, 5);

        // Test early termination
        let mut count = 0;
        let completed = searchable
            .scan_for_entries_of_type(LedgerEntryType::Account, |_| {
                count += 1;
                count < 3 // stop after 3
            })
            .unwrap();
        assert!(!completed);
        assert_eq!(count, 3);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_load_inflation_winners() {
        let mut bucket_list = BucketList::new();
        let header = make_test_header(10);

        // Create inflation destination account
        let dest_account_id = AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([0xFF; 32])));

        // Add accounts that vote for the destination
        for i in 1..=3u8 {
            let account_id = AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([i; 32])));
            let entry = LedgerEntry {
                last_modified_ledger_seq: 1,
                data: LedgerEntryData::Account(AccountEntry {
                    account_id,
                    balance: 1_000_000_000 * i as i64, // 1B, 2B, 3B stroops
                    seq_num: SequenceNumber(1),
                    num_sub_entries: 0,
                    inflation_dest: Some(dest_account_id.clone()),
                    flags: 0,
                    home_domain: String32::default(),
                    thresholds: Thresholds([1, 0, 0, 0]),
                    signers: vec![].try_into().unwrap(),
                    ext: AccountEntryExt::V0,
                }),
                ext: LedgerEntryExt::V0,
            };
            bucket_list
                .add_batch(
                    i as u32,
                    25,
                    BucketListType::Live,
                    vec![entry],
                    vec![],
                    vec![],
                )
                .unwrap();
        }

        // Add an account without inflation destination
        let no_dest_account = AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([0x10; 32])));
        let entry = LedgerEntry {
            last_modified_ledger_seq: 1,
            data: LedgerEntryData::Account(AccountEntry {
                account_id: no_dest_account,
                balance: 10_000_000_000, // 10B stroops
                seq_num: SequenceNumber(1),
                num_sub_entries: 0,
                inflation_dest: None, // No destination
                flags: 0,
                home_domain: String32::default(),
                thresholds: Thresholds([1, 0, 0, 0]),
                signers: vec![].try_into().unwrap(),
                ext: AccountEntryExt::V0,
            }),
            ext: LedgerEntryExt::V0,
        };
        bucket_list
            .add_batch(4, 25, BucketListType::Live, vec![entry], vec![], vec![])
            .unwrap();

        let snapshot = BucketListSnapshot::new(&bucket_list, header);
        let searchable = SearchableBucketListSnapshot::new(snapshot, BTreeMap::new());

        // Load inflation winners
        let winners = searchable.load_inflation_winners(10, 0).unwrap();

        // Should have one winner with total votes = 1B + 2B + 3B = 6B
        assert_eq!(winners.len(), 1);
        assert_eq!(winners[0].account_id, dest_account_id);
        assert_eq!(winners[0].votes, 6_000_000_000);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_inflation_winners_min_balance_filter() {
        let mut bucket_list = BucketList::new();
        let header = make_test_header(5);

        let dest1 = AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([0xAA; 32])));
        let dest2 = AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([0xBB; 32])));

        // Account 1 votes for dest1 with balance below 100 XLM threshold (500M stroops).
        // Per BUCKETLISTDB_SPEC §10.6, accounts with balance < 1B stroops (100 XLM)
        // are excluded from vote counting entirely.
        let entry1 = LedgerEntry {
            last_modified_ledger_seq: 1,
            data: LedgerEntryData::Account(AccountEntry {
                account_id: AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([1; 32]))),
                balance: 500_000_000, // 50 XLM — below 100 XLM threshold
                seq_num: SequenceNumber(1),
                num_sub_entries: 0,
                inflation_dest: Some(dest1.clone()),
                flags: 0,
                home_domain: String32::default(),
                thresholds: Thresholds([1, 0, 0, 0]),
                signers: vec![].try_into().unwrap(),
                ext: AccountEntryExt::V0,
            }),
            ext: LedgerEntryExt::V0,
        };

        // Account 2 votes for dest2 with balance above 100 XLM threshold (2B stroops).
        let entry2 = LedgerEntry {
            last_modified_ledger_seq: 1,
            data: LedgerEntryData::Account(AccountEntry {
                account_id: AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([2; 32]))),
                balance: 2_000_000_000, // 200 XLM — above 100 XLM threshold
                seq_num: SequenceNumber(1),
                num_sub_entries: 0,
                inflation_dest: Some(dest2.clone()),
                flags: 0,
                home_domain: String32::default(),
                thresholds: Thresholds([1, 0, 0, 0]),
                signers: vec![].try_into().unwrap(),
                ext: AccountEntryExt::V0,
            }),
            ext: LedgerEntryExt::V0,
        };

        bucket_list
            .add_batch(
                1,
                25,
                BucketListType::Live,
                vec![entry1, entry2],
                vec![],
                vec![],
            )
            .unwrap();

        let snapshot = BucketListSnapshot::new(&bucket_list, header);
        let searchable = SearchableBucketListSnapshot::new(snapshot, BTreeMap::new());

        // With min_balance=500, only dest2 should qualify (account 1 is below
        // the 100 XLM vote counting threshold so its votes aren't counted at all)
        let winners = searchable.load_inflation_winners(10, 500).unwrap();
        assert_eq!(winners.len(), 1);
        assert_eq!(winners[0].account_id, dest2);
        assert_eq!(winners[0].votes, 2_000_000_000);

        // With min_balance=0, only dest2 qualifies — account 1 is below the
        // hardcoded 100 XLM threshold and its votes are not counted.
        let winners = searchable.load_inflation_winners(10, 0).unwrap();
        assert_eq!(winners.len(), 1);
        assert_eq!(winners[0].account_id, dest2);
        assert_eq!(winners[0].votes, 2_000_000_000);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_snapshot_isolation_from_bucket_list_mutations() {
        let mut bucket_list = BucketList::new();

        // Add entry A
        let account_a = AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([0xAA; 32])));
        let entry_a = LedgerEntry {
            last_modified_ledger_seq: 1,
            data: LedgerEntryData::Account(AccountEntry {
                account_id: account_a.clone(),
                balance: 1000,
                seq_num: SequenceNumber(1),
                num_sub_entries: 0,
                inflation_dest: None,
                flags: 0,
                home_domain: String32::default(),
                thresholds: Thresholds([1, 0, 0, 0]),
                signers: vec![].try_into().unwrap(),
                ext: AccountEntryExt::V0,
            }),
            ext: LedgerEntryExt::V0,
        };
        bucket_list
            .add_batch(
                1,
                25,
                BucketListType::Live,
                vec![entry_a.clone()],
                vec![],
                vec![],
            )
            .unwrap();

        let key_a = LedgerKey::Account(LedgerKeyAccount {
            account_id: account_a,
        });

        // Take snapshot — should see entry A
        let snapshot = BucketListSnapshot::new(&bucket_list, make_test_header(1));
        assert!(snapshot.get(&key_a).is_some());

        // Also verify get_result returns the same entry
        let result = snapshot.get_result(&key_a).unwrap();
        assert!(result.is_some());
        assert_eq!(result.unwrap().last_modified_ledger_seq, 1);

        // Mutate bucket list: add entry B, delete entry A
        let account_b = AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([0xBB; 32])));
        let entry_b = LedgerEntry {
            last_modified_ledger_seq: 2,
            data: LedgerEntryData::Account(AccountEntry {
                account_id: account_b.clone(),
                balance: 2000,
                seq_num: SequenceNumber(1),
                num_sub_entries: 0,
                inflation_dest: None,
                flags: 0,
                home_domain: String32::default(),
                thresholds: Thresholds([1, 0, 0, 0]),
                signers: vec![].try_into().unwrap(),
                ext: AccountEntryExt::V0,
            }),
            ext: LedgerEntryExt::V0,
        };
        let key_b = LedgerKey::Account(LedgerKeyAccount {
            account_id: account_b,
        });
        bucket_list
            .add_batch(
                2,
                25,
                BucketListType::Live,
                vec![entry_b],
                vec![],
                vec![key_a.clone()],
            )
            .unwrap();

        // Verify bucket list itself reflects the mutations
        assert!(
            bucket_list.get(&key_a).unwrap().is_none(),
            "A should be deleted from live bucket list"
        );
        assert!(
            bucket_list.get(&key_b).unwrap().is_some(),
            "B should exist in live bucket list"
        );

        // Verify snapshot is isolated — still sees old state
        assert!(
            snapshot.get(&key_a).is_some(),
            "snapshot should still see entry A"
        );
        assert!(
            snapshot.get(&key_b).is_none(),
            "snapshot should not see entry B"
        );

        // Same via get_result
        assert!(
            snapshot.get_result(&key_a).unwrap().is_some(),
            "get_result should still return entry A"
        );
        assert!(
            snapshot.get_result(&key_b).unwrap().is_none(),
            "get_result should not return entry B"
        );
    }
}
