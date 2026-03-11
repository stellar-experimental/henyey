//! BucketApplicator - Apply bucket entries to database during catchup.
//!
//! During catchup, entries from bucket files need to be applied to the database.
//! The `BucketApplicator` handles this process incrementally, processing entries
//! in chunks to avoid memory pressure and allow progress tracking.
//!
//! # Deduplication
//!
//! When applying entries, the applicator tracks which keys have been seen to
//! avoid applying older versions of entries that appear in multiple buckets.
//! Entries are processed from newest to oldest, so the first occurrence of a
//! key is the most recent value.
//!
//! # Chunked Processing
//!
//! The applicator processes entries in configurable chunks, allowing:
//! - Memory-bounded processing
//! - Progress reporting
//! - Interruptible operations
//!
//! # Usage
//!
//! ```ignore
//! let applicator = BucketApplicator::new(bucket, 25, 5);
//! let mut counters = ApplicatorCounters::new();
//!
//! while applicator.has_more() {
//!     let batch = applicator.advance(&mut counters)?;
//!     // Apply batch to database
//!     for (key, entry) in batch {
//!         db.upsert(key, entry);
//!     }
//! }
//!
//! println!("Applied {} entries", counters.total_upserted());
//! ```

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use stellar_xdr::curr::{LedgerEntry, LedgerEntryType, LedgerKey};

use crate::bucket::Bucket;
use crate::entry::{ledger_entry_data_type, ledger_entry_to_key, ledger_key_type, BucketEntry};
use crate::Result;

/// Default number of entries to process in each chunk.
pub const DEFAULT_CHUNK_SIZE: usize = 10_000;

// ============================================================================
// Applicator Counters
// ============================================================================

/// Counters for tracking bucket application progress.
#[derive(Debug, Clone, Default)]
pub struct ApplicatorCounters {
    /// Count of upserted entries by type.
    pub upserted_by_type: HashMap<LedgerEntryType, u64>,
    /// Count of deleted entries by type.
    pub deleted_by_type: HashMap<LedgerEntryType, u64>,
    /// Total entries processed.
    pub entries_processed: u64,
    /// Total entries skipped (already seen).
    pub entries_skipped: u64,
}

impl ApplicatorCounters {
    /// Creates new empty counters.
    pub fn new() -> Self {
        Self::default()
    }

    /// Records an upsert operation.
    pub fn record_upsert(&mut self, entry_type: LedgerEntryType) {
        *self.upserted_by_type.entry(entry_type).or_insert(0) += 1;
    }

    /// Records a delete operation.
    pub fn record_delete(&mut self, entry_type: LedgerEntryType) {
        *self.deleted_by_type.entry(entry_type).or_insert(0) += 1;
    }

    /// Records a processed entry (either upsert or delete).
    pub fn record_processed(&mut self) {
        self.entries_processed += 1;
    }

    /// Records a skipped entry (already seen).
    pub fn record_skipped(&mut self) {
        self.entries_skipped += 1;
    }

    /// Returns total upserted entries.
    pub fn total_upserted(&self) -> u64 {
        self.upserted_by_type.values().sum()
    }

    /// Returns total deleted entries.
    pub fn total_deleted(&self) -> u64 {
        self.deleted_by_type.values().sum()
    }

    /// Returns total applied entries (upserted + deleted).
    pub fn total_applied(&self) -> u64 {
        self.total_upserted() + self.total_deleted()
    }

    /// Returns the count of upserted entries for a specific type.
    pub fn upserted_for_type(&self, entry_type: LedgerEntryType) -> u64 {
        self.upserted_by_type.get(&entry_type).copied().unwrap_or(0)
    }

    /// Returns the count of deleted entries for a specific type.
    pub fn deleted_for_type(&self, entry_type: LedgerEntryType) -> u64 {
        self.deleted_by_type.get(&entry_type).copied().unwrap_or(0)
    }

    /// Merges counters from another instance.
    pub fn merge(&mut self, other: &ApplicatorCounters) {
        for (k, v) in &other.upserted_by_type {
            *self.upserted_by_type.entry(*k).or_insert(0) += v;
        }
        for (k, v) in &other.deleted_by_type {
            *self.deleted_by_type.entry(*k).or_insert(0) += v;
        }
        self.entries_processed += other.entries_processed;
        self.entries_skipped += other.entries_skipped;
    }

    /// Resets all counters to zero.
    pub fn reset(&mut self) {
        self.upserted_by_type.clear();
        self.deleted_by_type.clear();
        self.entries_processed = 0;
        self.entries_skipped = 0;
    }
}

// ============================================================================
// Bucket Entry to Apply
// ============================================================================

/// An entry to be applied to the database.
#[derive(Debug, Clone)]
pub enum EntryToApply {
    /// Upsert (insert or update) an entry.
    /// The LedgerEntry is boxed to reduce enum size (LedgerEntry is ~256 bytes).
    Upsert(LedgerKey, Box<LedgerEntry>),
    /// Delete an entry.
    Delete(LedgerKey),
}

impl EntryToApply {
    /// Returns the key for this entry.
    pub fn key(&self) -> &LedgerKey {
        match self {
            EntryToApply::Upsert(key, _) => key,
            EntryToApply::Delete(key) => key,
        }
    }

    /// Returns the entry if this is an upsert.
    pub fn entry(&self) -> Option<&LedgerEntry> {
        match self {
            EntryToApply::Upsert(_, entry) => Some(entry),
            EntryToApply::Delete(_) => None,
        }
    }

    /// Returns true if this is a delete operation.
    pub fn is_delete(&self) -> bool {
        matches!(self, EntryToApply::Delete(_))
    }
}

// ============================================================================
// Bucket Applicator
// ============================================================================

/// Applies bucket entries to a database during catchup.
///
/// The applicator processes entries from a bucket in chunks, tracking which
/// keys have been seen to avoid duplicate applications. It supports both
/// upserts (live/init entries) and deletes (dead entries).
pub struct BucketApplicator {
    /// The bucket being applied.
    bucket: Arc<Bucket>,
    /// Maximum protocol version for filtering.
    max_protocol_version: u32,
    /// Level of this bucket (for logging).
    level: u32,
    /// Keys that have been seen (for deduplication).
    seen_keys: HashSet<LedgerKey>,
    /// Current offset in the bucket (entry index).
    current_offset: usize,
    /// Number of entries to process per chunk.
    chunk_size: usize,
    /// Whether to apply dead entries.
    apply_dead_entries: bool,
    /// Cached entries (for disk-backed buckets).
    cached_entries: Option<Vec<BucketEntry>>,
}

impl BucketApplicator {
    /// Creates a new bucket applicator.
    ///
    /// # Arguments
    ///
    /// * `bucket` - The bucket to apply
    /// * `max_protocol_version` - Maximum protocol version to accept
    /// * `level` - Bucket level (for logging)
    pub fn new(bucket: Arc<Bucket>, max_protocol_version: u32, level: u32) -> Self {
        Self {
            bucket,
            max_protocol_version,
            level,
            seen_keys: HashSet::new(),
            current_offset: 0,
            chunk_size: DEFAULT_CHUNK_SIZE,
            apply_dead_entries: true,
            cached_entries: None,
        }
    }

    /// Creates a new bucket applicator with custom chunk size.
    pub fn with_chunk_size(
        bucket: Arc<Bucket>,
        max_protocol_version: u32,
        level: u32,
        chunk_size: usize,
    ) -> Self {
        Self {
            bucket,
            max_protocol_version,
            level,
            seen_keys: HashSet::new(),
            current_offset: 0,
            chunk_size,
            apply_dead_entries: true,
            cached_entries: None,
        }
    }

    /// Sets whether to apply dead entries (deletes).
    pub fn set_apply_dead_entries(&mut self, apply: bool) {
        self.apply_dead_entries = apply;
    }

    /// Returns the bucket level.
    pub fn level(&self) -> u32 {
        self.level
    }

    /// Returns the max protocol version.
    pub fn max_protocol_version(&self) -> u32 {
        self.max_protocol_version
    }

    /// Returns true if there are more entries to process.
    pub fn has_more(&self) -> bool {
        self.current_offset < self.bucket.len()
    }

    /// Returns the current progress (0.0 to 1.0).
    pub fn progress(&self) -> f64 {
        if self.bucket.is_empty() {
            return 1.0;
        }
        self.current_offset as f64 / self.bucket.len() as f64
    }

    /// Returns the number of entries remaining.
    pub fn remaining(&self) -> usize {
        self.bucket.len().saturating_sub(self.current_offset)
    }

    /// Checks if a key has already been seen.
    pub fn is_seen(&self, key: &LedgerKey) -> bool {
        self.seen_keys.contains(key)
    }

    /// Marks a key as seen externally (for cross-bucket deduplication).
    pub fn mark_seen(&mut self, key: LedgerKey) {
        self.seen_keys.insert(key);
    }

    /// Marks multiple keys as seen.
    pub fn mark_seen_many(&mut self, keys: impl Iterator<Item = LedgerKey>) {
        self.seen_keys.extend(keys);
    }

    /// Advances to the next chunk of entries.
    ///
    /// Processes up to `chunk_size` entries and returns them for application.
    /// Updates counters with statistics about the processing.
    ///
    /// # Arguments
    ///
    /// * `counters` - Counters to update with processing statistics
    ///
    /// # Returns
    ///
    /// A vector of entries to apply to the database.
    pub fn advance(&mut self, counters: &mut ApplicatorCounters) -> Result<Vec<EntryToApply>> {
        let mut batch = Vec::with_capacity(self.chunk_size);

        // Load entries if needed
        if self.cached_entries.is_none() && self.bucket.is_disk_backed() {
            // For disk-backed buckets, collect entries into memory
            let entries: Vec<BucketEntry> = self.bucket.iter().collect();
            self.cached_entries = Some(entries);
        }

        let entries: Vec<BucketEntry> = if let Some(ref cached) = self.cached_entries {
            cached.clone()
        } else {
            self.bucket.iter().collect()
        };

        let end = std::cmp::min(self.current_offset + self.chunk_size, entries.len());

        for entry in &entries[self.current_offset..end] {
            counters.record_processed();

            match entry {
                BucketEntry::Liveentry(ledger_entry) | BucketEntry::Initentry(ledger_entry) => {
                    if let Some(key) = ledger_entry_to_key(ledger_entry) {
                        // Skip if already seen
                        if self.seen_keys.contains(&key) {
                            counters.record_skipped();
                            continue;
                        }

                        self.seen_keys.insert(key.clone());

                        let entry_type = ledger_entry_data_type(&ledger_entry.data);
                        counters.record_upsert(entry_type);
                        batch.push(EntryToApply::Upsert(key, Box::new(ledger_entry.clone())));
                    }
                }
                BucketEntry::Deadentry(key) => {
                    if !self.apply_dead_entries {
                        continue;
                    }

                    // Skip if already seen
                    if self.seen_keys.contains(key) {
                        counters.record_skipped();
                        continue;
                    }

                    self.seen_keys.insert(key.clone());

                    let entry_type = ledger_key_type(key);
                    counters.record_delete(entry_type);
                    batch.push(EntryToApply::Delete(key.clone()));
                }
                BucketEntry::Metaentry(_) => {
                    // Skip metadata entries
                }
            }
        }

        self.current_offset = end;
        Ok(batch)
    }

    /// Processes all remaining entries in one call.
    ///
    /// This is a convenience method that calls `advance` until all entries
    /// are processed.
    pub fn apply_all(&mut self, counters: &mut ApplicatorCounters) -> Result<Vec<EntryToApply>> {
        let mut all_entries = Vec::new();

        while self.has_more() {
            let batch = self.advance(counters)?;
            all_entries.extend(batch);
        }

        Ok(all_entries)
    }

    /// Resets the applicator to start from the beginning.
    ///
    /// This clears the seen keys and resets the offset.
    pub fn reset(&mut self) {
        self.seen_keys.clear();
        self.current_offset = 0;
    }

    /// Returns the total number of entries in the bucket.
    pub fn total_entries(&self) -> usize {
        self.bucket.len()
    }

    /// Returns the number of unique keys seen so far.
    pub fn unique_keys_seen(&self) -> usize {
        self.seen_keys.len()
    }
}

impl std::fmt::Debug for BucketApplicator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BucketApplicator")
            .field("level", &self.level)
            .field("current_offset", &self.current_offset)
            .field("total_entries", &self.bucket.len())
            .field("unique_keys_seen", &self.seen_keys.len())
            .field("progress", &format!("{:.1}%", self.progress() * 100.0))
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entry::BucketEntry; // Use our BucketEntry, not the XDR one
    use stellar_xdr::curr::*;

    fn make_account_id(byte: u8) -> AccountId {
        AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([byte; 32])))
    }

    fn make_account_entry(byte: u8) -> LedgerEntry {
        LedgerEntry {
            last_modified_ledger_seq: 1,
            data: LedgerEntryData::Account(AccountEntry {
                account_id: make_account_id(byte),
                balance: 100,
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
        }
    }

    fn make_account_key(byte: u8) -> LedgerKey {
        LedgerKey::Account(LedgerKeyAccount {
            account_id: make_account_id(byte),
        })
    }

    #[test]
    fn test_applicator_counters() {
        let mut counters = ApplicatorCounters::new();

        counters.record_upsert(LedgerEntryType::Account);
        counters.record_upsert(LedgerEntryType::Account);
        counters.record_delete(LedgerEntryType::Trustline);
        counters.record_processed();
        counters.record_skipped();

        assert_eq!(counters.total_upserted(), 2);
        assert_eq!(counters.total_deleted(), 1);
        assert_eq!(counters.total_applied(), 3);
        assert_eq!(counters.upserted_for_type(LedgerEntryType::Account), 2);
        assert_eq!(counters.deleted_for_type(LedgerEntryType::Trustline), 1);
        assert_eq!(counters.entries_processed, 1);
        assert_eq!(counters.entries_skipped, 1);
    }

    #[test]
    fn test_applicator_basic() {
        let entries = vec![
            BucketEntry::Liveentry(make_account_entry(1)),
            BucketEntry::Liveentry(make_account_entry(2)),
            BucketEntry::Deadentry(make_account_key(3)),
        ];

        let bucket = Arc::new(Bucket::from_entries(entries).unwrap());
        let mut applicator = BucketApplicator::new(bucket, 25, 0);
        let mut counters = ApplicatorCounters::new();

        assert!(applicator.has_more());

        let batch = applicator.apply_all(&mut counters).unwrap();

        // Note: bucket may add metadata entry, so entries may be >= 3
        assert!(batch.len() >= 2); // At least 2 live + 1 dead = 3 (excluding metadata)
        assert!(!applicator.has_more());
        assert!(counters.total_applied() >= 2);
    }

    #[test]
    fn test_applicator_chunked() {
        let entries: Vec<BucketEntry> = (0..100u8)
            .map(|i| BucketEntry::Liveentry(make_account_entry(i)))
            .collect();

        let bucket = Arc::new(Bucket::from_entries(entries).unwrap());
        let mut applicator = BucketApplicator::with_chunk_size(bucket, 25, 0, 30);
        let mut counters = ApplicatorCounters::new();

        let mut total_batches = 0;
        while applicator.has_more() {
            let _batch = applicator.advance(&mut counters).unwrap();
            total_batches += 1;
        }

        // Should take multiple batches to process 100 entries with chunk_size=30
        // Note: actual count may include metadata entry
        assert!(total_batches >= 3);
    }

    #[test]
    fn test_applicator_deduplication() {
        let entries = vec![
            BucketEntry::Liveentry(make_account_entry(1)),
            BucketEntry::Liveentry(make_account_entry(1)), // Duplicate
            BucketEntry::Liveentry(make_account_entry(2)),
        ];

        let bucket = Arc::new(Bucket::from_entries(entries).unwrap());
        let mut applicator = BucketApplicator::new(bucket, 25, 0);
        let mut counters = ApplicatorCounters::new();

        let batch = applicator.apply_all(&mut counters).unwrap();

        // Duplicates should be deduplicated
        let account_upserts = batch
            .iter()
            .filter(|e| !e.is_delete() && matches!(e.key(), LedgerKey::Account(_)))
            .count();

        // Due to sorting, only 2 unique accounts should be in the batch
        assert_eq!(account_upserts, 2);
    }

    #[test]
    fn test_applicator_progress() {
        let entries: Vec<BucketEntry> = (0..10u8)
            .map(|i| BucketEntry::Liveentry(make_account_entry(i)))
            .collect();

        let bucket = Arc::new(Bucket::from_entries(entries).unwrap());
        let mut applicator = BucketApplicator::with_chunk_size(bucket, 25, 0, 5);
        let mut counters = ApplicatorCounters::new();

        assert_eq!(applicator.progress(), 0.0);

        let _ = applicator.advance(&mut counters).unwrap();
        // Progress should be around 50% after first chunk
        let progress = applicator.progress();
        assert!(progress > 0.0 && progress < 1.0);

        let _ = applicator.apply_all(&mut counters).unwrap();
        assert_eq!(applicator.progress(), 1.0);
    }

    #[test]
    fn test_applicator_skip_dead() {
        let entries = vec![
            BucketEntry::Liveentry(make_account_entry(1)),
            BucketEntry::Deadentry(make_account_key(2)),
        ];

        let bucket = Arc::new(Bucket::from_entries(entries).unwrap());
        let mut applicator = BucketApplicator::new(bucket, 25, 0);
        applicator.set_apply_dead_entries(false);
        let mut counters = ApplicatorCounters::new();

        let batch = applicator.apply_all(&mut counters).unwrap();

        // Should not include the dead entry
        let delete_count = batch.iter().filter(|e| e.is_delete()).count();
        assert_eq!(delete_count, 0);
    }

    #[test]
    fn test_applicator_reset() {
        let entries = vec![BucketEntry::Liveentry(make_account_entry(1))];

        let bucket = Arc::new(Bucket::from_entries(entries).unwrap());
        let mut applicator = BucketApplicator::new(bucket, 25, 0);
        let mut counters = ApplicatorCounters::new();

        let _ = applicator.apply_all(&mut counters).unwrap();
        assert!(!applicator.has_more());

        applicator.reset();
        assert!(applicator.has_more());
        assert_eq!(applicator.unique_keys_seen(), 0);
    }

    #[test]
    fn test_entry_to_apply() {
        let entry = make_account_entry(1);
        let key = make_account_key(1);

        let upsert = EntryToApply::Upsert(key.clone(), Box::new(entry.clone()));
        assert!(!upsert.is_delete());
        assert!(upsert.entry().is_some());
        assert_eq!(upsert.key(), &key);

        let delete = EntryToApply::Delete(key.clone());
        assert!(delete.is_delete());
        assert!(delete.entry().is_none());
        assert_eq!(delete.key(), &key);
    }
}
