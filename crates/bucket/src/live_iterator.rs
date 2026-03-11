//! Streaming iterator for live bucket list entries.
//!
//! This module provides a memory-efficient iterator over all live entries in a bucket list,
//! matching the stellar-core's iteration pattern from `BucketApplicator`.
//!
//! # Overview
//!
//! The [`LiveEntriesIterator`] iterates through all 11 bucket levels (curr then snap at each level),
//! yielding only live entries that haven't been seen at higher levels. This is a streaming
//! alternative to [`BucketList::live_entries()`] that avoids materializing all entries into memory.
//!
//! # Memory Efficiency
//!
//! Instead of collecting all entries into a `Vec<LedgerEntry>`, this iterator uses a
//! `HashSet<LedgerKey>` for deduplication, matching stellar-core's approach:
//!
//! ```cpp
//! // From BucketApplicator.cpp
//! std::unordered_set<LedgerKey>& mSeenKeys;
//! ```
//!
//! For mainnet scale (~60M entries):
//! - Old approach: ~52 GB (full entry Vec + serialized key HashSet)
//! - New approach: ~8.6 GB (LedgerKey HashSet only)
//!
//! # Example
//!
//! ```ignore
//! use henyey_bucket::BucketList;
//!
//! let bucket_list = BucketList::new();
//! // ... populate bucket list ...
//!
//! // Stream through all live entries without full materialization
//! for entry_result in bucket_list.live_entries_iter() {
//!     let entry = entry_result?;
//!     // Process entry immediately - no need to collect all first
//! }
//! ```
//!
//! # Iteration Order
//!
//! Entries are yielded in the same order as `live_entries()`:
//! - Level 0 curr, Level 0 snap
//! - Level 1 curr, Level 1 snap
//! - ...
//! - Level 10 curr, Level 10 snap
//!
//! The first occurrence of each key shadows later occurrences (including Dead entries
//! which shadow any subsequent Live/Init entries with the same key).

use std::collections::HashSet;

use stellar_xdr::curr::{LedgerEntry, LedgerKey};

use crate::bucket::{Bucket, BucketIter};
use crate::entry::{ledger_entry_to_key, BucketEntry};
use crate::{BucketLevel, BucketList, Result};

/// Streaming iterator over live bucket list entries.
///
/// This iterator yields [`LedgerEntry`] values one at a time, using a `HashSet<LedgerKey>`
/// for deduplication to match stellar-core's approach. It's designed for memory-efficient
/// iteration over large bucket lists (mainnet scale).
///
/// # Deduplication
///
/// The iterator tracks seen keys using `HashSet<LedgerKey>`, matching stellar-core:
///
/// ```cpp
/// auto [_, wasInserted] = mSeenKeys.emplace(LedgerEntryKey(e.liveEntry()));
/// if (!wasInserted) {
///     continue;  // Skip - already seen
/// }
/// ```
///
/// Dead entries are also tracked in the seen set, so they shadow any older Live/Init
/// entries with the same key.
///
/// # Performance
///
/// - Memory: O(unique keys) - approximately 144 bytes per unique key
/// - Time: O(total entries) - each entry is visited once
/// - I/O: Sequential reads for disk-backed buckets
pub struct LiveEntriesIterator<'a> {
    /// Reference to the bucket list levels.
    levels: &'a [BucketLevel],

    /// Current level index (0-10).
    current_level: usize,

    /// Current phase: 0 = curr, 1 = snap.
    current_phase: usize,

    /// Iterator over the current bucket's entries.
    bucket_iter: Option<BucketIter<'a>>,

    /// Set of seen keys for deduplication (matches stellar-core `unordered_set<LedgerKey>`).
    seen_keys: HashSet<LedgerKey>,

    /// Number of entries yielded (for statistics).
    entries_yielded: usize,

    /// Number of entries skipped due to deduplication (for statistics).
    entries_skipped: usize,
}

impl<'a> LiveEntriesIterator<'a> {
    /// Create a new iterator over the bucket list's live entries.
    ///
    /// The iterator starts at level 0 curr and proceeds through all levels.
    pub fn new(bucket_list: &'a BucketList) -> Self {
        let mut iter = Self {
            levels: bucket_list.levels(),
            current_level: 0,
            current_phase: 0,
            bucket_iter: None,
            seen_keys: HashSet::new(),
            entries_yielded: 0,
            entries_skipped: 0,
        };

        // Initialize with the first non-empty bucket
        iter.advance_to_next_bucket();
        iter
    }

    /// Get the number of entries yielded so far.
    pub fn entries_yielded(&self) -> usize {
        self.entries_yielded
    }

    /// Get the number of entries skipped due to deduplication.
    pub fn entries_skipped(&self) -> usize {
        self.entries_skipped
    }

    /// Get the current size of the seen keys set.
    pub fn seen_keys_count(&self) -> usize {
        self.seen_keys.len()
    }

    /// Get the current bucket being iterated.
    fn current_bucket(&self) -> Option<&'a Bucket> {
        if self.current_level >= self.levels.len() {
            return None;
        }

        let level = &self.levels[self.current_level];
        match self.current_phase {
            0 => Some(&*level.curr),
            1 => Some(&*level.snap),
            _ => None,
        }
    }

    /// Advance to the next non-empty bucket.
    ///
    /// Returns true if a bucket was found, false if iteration is complete.
    fn advance_to_next_bucket(&mut self) -> bool {
        loop {
            // Check if we've exhausted all levels
            if self.current_level >= self.levels.len() {
                self.bucket_iter = None;
                return false;
            }

            // Try current position
            if let Some(bucket) = self.current_bucket() {
                if !bucket.is_empty() {
                    self.bucket_iter = Some(bucket.iter());
                    return true;
                }
            }

            // Advance to next position
            if self.current_phase == 0 {
                // Move from curr to snap
                self.current_phase = 1;
            } else {
                // Move to next level's curr
                self.current_level += 1;
                self.current_phase = 0;
            }
        }
    }

    /// Move to the next bucket position (curr -> snap -> next level curr).
    fn advance_position(&mut self) {
        if self.current_phase == 0 {
            self.current_phase = 1;
        } else {
            self.current_level += 1;
            self.current_phase = 0;
        }
        self.bucket_iter = None;
    }
}

impl<'a> Iterator for LiveEntriesIterator<'a> {
    type Item = Result<LedgerEntry>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            // If we don't have a bucket iterator, try to get one
            if self.bucket_iter.is_none() && !self.advance_to_next_bucket() {
                return None; // No more buckets
            }

            // Try to get the next entry from the current bucket
            let iter = self.bucket_iter.as_mut()?;

            match iter.next() {
                Some(entry) => {
                    match entry {
                        BucketEntry::Liveentry(ref e) | BucketEntry::Initentry(ref e) => {
                            // Get the key for this entry
                            let key = match ledger_entry_to_key(e) {
                                Some(k) => k,
                                None => continue, // Skip entries without valid keys
                            };

                            // stellar-core pattern: mSeenKeys.emplace(key).second
                            // insert() returns true if the key was newly inserted
                            if !self.seen_keys.insert(key) {
                                self.entries_skipped += 1;
                                continue; // Already seen, skip
                            }

                            self.entries_yielded += 1;
                            return Some(Ok(e.clone()));
                        }
                        BucketEntry::Deadentry(ref key) => {
                            // Mark dead keys as seen (shadows older live entries)
                            self.seen_keys.insert(key.clone());
                            continue;
                        }
                        BucketEntry::Metaentry(_) => {
                            // Skip metadata entries
                            continue;
                        }
                    }
                }
                None => {
                    // Current bucket exhausted, move to next
                    self.advance_position();
                }
            }
        }
    }
}

/// Statistics from a completed iteration.
#[derive(Debug, Clone)]
pub struct LiveEntriesStats {
    /// Total number of live entries yielded.
    pub entries_yielded: usize,
    /// Number of entries skipped due to deduplication.
    pub entries_skipped: usize,
    /// Number of unique keys seen (including dead entries).
    pub unique_keys: usize,
}

impl LiveEntriesStats {
    /// Create stats from a completed iterator.
    pub fn from_iterator(iter: &LiveEntriesIterator<'_>) -> Self {
        Self {
            entries_yielded: iter.entries_yielded(),
            entries_skipped: iter.entries_skipped(),
            unique_keys: iter.seen_keys_count(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::BucketList;
    use stellar_xdr::curr::*;

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

    const TEST_PROTOCOL: u32 = 25;

    #[tokio::test(flavor = "multi_thread")]
    async fn test_empty_bucket_list() {
        let bucket_list = BucketList::new();
        let iter = LiveEntriesIterator::new(&bucket_list);
        let entries: Vec<_> = iter.collect();
        assert!(entries.is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_single_entry() {
        let mut bucket_list = BucketList::new();
        let entry = make_account_entry([1u8; 32], 100);

        bucket_list
            .add_batch(
                1,
                TEST_PROTOCOL,
                BucketListType::Live,
                vec![entry.clone()],
                vec![],
                vec![],
            )
            .unwrap();

        let entries: Vec<_> = LiveEntriesIterator::new(&bucket_list)
            .collect::<Result<Vec<_>>>()
            .unwrap();

        assert_eq!(entries.len(), 1);
        if let LedgerEntryData::Account(account) = &entries[0].data {
            assert_eq!(account.balance, 100);
        } else {
            panic!("Expected account entry");
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_multiple_entries() {
        let mut bucket_list = BucketList::new();

        for i in 1..=10u32 {
            let mut id = [0u8; 32];
            id[0..4].copy_from_slice(&i.to_be_bytes());
            let entry = make_account_entry(id, i as i64 * 100);

            bucket_list
                .add_batch(
                    i,
                    TEST_PROTOCOL,
                    BucketListType::Live,
                    vec![entry],
                    vec![],
                    vec![],
                )
                .unwrap();
        }

        let entries: Vec<_> = LiveEntriesIterator::new(&bucket_list)
            .collect::<Result<Vec<_>>>()
            .unwrap();

        assert_eq!(entries.len(), 10);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_deduplication() {
        let mut bucket_list = BucketList::new();

        // Add entry in ledger 1
        let entry1 = make_account_entry([1u8; 32], 100);
        bucket_list
            .add_batch(
                1,
                TEST_PROTOCOL,
                BucketListType::Live,
                vec![entry1],
                vec![],
                vec![],
            )
            .unwrap();

        // Update same entry in ledger 2 (should shadow the first)
        let entry2 = make_account_entry([1u8; 32], 200);
        bucket_list
            .add_batch(
                2,
                TEST_PROTOCOL,
                BucketListType::Live,
                vec![],
                vec![entry2],
                vec![],
            )
            .unwrap();

        let mut iter = LiveEntriesIterator::new(&bucket_list);
        let entries: Vec<_> = iter.by_ref().collect::<Result<Vec<_>>>().unwrap();

        // Should only see the newer entry (balance 200)
        assert_eq!(entries.len(), 1);
        if let LedgerEntryData::Account(account) = &entries[0].data {
            assert_eq!(account.balance, 200);
        } else {
            panic!("Expected account entry");
        }

        // Stats should show one skipped
        assert_eq!(iter.entries_yielded(), 1);
        assert!(iter.entries_skipped() >= 1); // At least the old entry was skipped
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_dead_entry_shadows() {
        let mut bucket_list = BucketList::new();

        // Add entry in ledger 1
        let entry = make_account_entry([1u8; 32], 100);
        bucket_list
            .add_batch(
                1,
                TEST_PROTOCOL,
                BucketListType::Live,
                vec![entry],
                vec![],
                vec![],
            )
            .unwrap();

        // Delete entry in ledger 2
        let key = make_account_key([1u8; 32]);
        bucket_list
            .add_batch(
                2,
                TEST_PROTOCOL,
                BucketListType::Live,
                vec![],
                vec![],
                vec![key],
            )
            .unwrap();

        let entries: Vec<_> = LiveEntriesIterator::new(&bucket_list)
            .collect::<Result<Vec<_>>>()
            .unwrap();

        // Deleted entry should not appear
        assert!(entries.is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_matches_live_entries() {
        // Verify that LiveEntriesIterator produces the same results as live_entries()
        let mut bucket_list = BucketList::new();

        // Add various entries over multiple ledgers
        for i in 1..=20u32 {
            let mut id = [0u8; 32];
            id[0..4].copy_from_slice(&i.to_be_bytes());
            let entry = make_account_entry(id, i as i64 * 100);

            if i % 3 == 0 {
                // Update existing entry
                bucket_list
                    .add_batch(
                        i,
                        TEST_PROTOCOL,
                        BucketListType::Live,
                        vec![],
                        vec![entry],
                        vec![],
                    )
                    .unwrap();
            } else {
                // New entry
                bucket_list
                    .add_batch(
                        i,
                        TEST_PROTOCOL,
                        BucketListType::Live,
                        vec![entry],
                        vec![],
                        vec![],
                    )
                    .unwrap();
            }
        }

        // Delete some entries
        for i in [5u32, 10, 15] {
            let mut id = [0u8; 32];
            id[0..4].copy_from_slice(&i.to_be_bytes());
            let key = make_account_key(id);
            bucket_list
                .add_batch(
                    21 + i,
                    TEST_PROTOCOL,
                    BucketListType::Live,
                    vec![],
                    vec![],
                    vec![key],
                )
                .unwrap();
        }

        // Get entries via both methods
        #[allow(deprecated)]
        let old_entries = bucket_list.live_entries().unwrap();
        let new_entries: Vec<_> = LiveEntriesIterator::new(&bucket_list)
            .collect::<Result<Vec<_>>>()
            .unwrap();

        // Should have the same count
        assert_eq!(
            old_entries.len(),
            new_entries.len(),
            "Entry counts differ: old={}, new={}",
            old_entries.len(),
            new_entries.len()
        );

        // Convert to sets of keys for comparison (order may differ slightly)
        let old_keys: HashSet<_> = old_entries
            .iter()
            .filter_map(|e| ledger_entry_to_key(e))
            .collect();
        let new_keys: HashSet<_> = new_entries
            .iter()
            .filter_map(|e| ledger_entry_to_key(e))
            .collect();

        assert_eq!(old_keys, new_keys, "Key sets differ");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_stats() {
        let mut bucket_list = BucketList::new();

        // Add 5 unique entries
        for i in 1..=5u32 {
            let mut id = [0u8; 32];
            id[0..4].copy_from_slice(&i.to_be_bytes());
            let entry = make_account_entry(id, i as i64 * 100);
            bucket_list
                .add_batch(
                    i,
                    TEST_PROTOCOL,
                    BucketListType::Live,
                    vec![entry],
                    vec![],
                    vec![],
                )
                .unwrap();
        }

        let mut iter = LiveEntriesIterator::new(&bucket_list);
        let _entries: Vec<_> = iter.by_ref().collect::<Result<Vec<_>>>().unwrap();

        let stats = LiveEntriesStats::from_iterator(&iter);
        assert_eq!(stats.entries_yielded, 5);
        assert_eq!(stats.unique_keys, 5);
    }
}
