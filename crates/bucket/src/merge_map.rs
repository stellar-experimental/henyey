//! Merge deduplication for bucket merging.
//!
//! This module provides unified merge dedup state (`BucketMergeMap`) that tracks
//! both completed merge results and in-flight merges, enabling:
//! - Reuse of previously computed merge results (completed-merge cache)
//! - Reattachment to running merges (in-flight dedup, parity with stellar-core
//!   `getMergeFuture`/`putMergeFuture` in `BucketManager.cpp:699–790`)
//!
//! # Merge Key
//!
//! Merges are identified by their [`MergeKey`], which consists of:
//! - The hash of the curr bucket
//! - The hash of the snap bucket
//! - Whether tombstones are kept (affects merge behavior)
//!
//! # In-Flight Dedup
//!
//! When a merge is requested, [`BucketMergeMap::get_or_start`] atomically checks:
//! 1. The completed-merge cache (returns [`MergeSlot::Completed`])
//! 2. The in-flight map (returns [`MergeSlot::InFlight`])
//! 3. Registers a new entry (returns [`MergeSlot::New`])
//!
//! This single atomic operation eliminates races between checking and inserting.
//!
//! # Memory Management
//!
//! The merge map tracks relationships between input and output buckets:
//! - `input_to_output`: Maps input bucket hashes to output hashes
//! - `output_to_merge_key`: Maps output hashes to merge keys for GC
//! - `in_flight`: Maps merge keys to watch channels for running merges
//!
//! GC operations (`retain_outputs`, `forget_all_merges_producing`) only affect
//! the completed submap. In-flight entries are not GC'd — they are removed
//! only when the merge completes, fails, or is abandoned.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use henyey_common::Hash256;
use tokio::sync::watch;

use crate::bucket::Bucket;
use crate::future_bucket::MergeKey;

// ============================================================================
// In-Flight Merge Types
// ============================================================================

/// Result of an in-flight merge — either success or error.
/// Must be cloneable so multiple receivers can access it.
pub type MergeResult = std::result::Result<Arc<Bucket>, Arc<str>>;

/// Outcome of requesting a merge slot via [`BucketMergeMap::get_or_start`].
pub enum MergeSlot {
    /// Completed merge found in cache. Output bucket already available.
    Completed(Arc<Bucket>),
    /// In-flight merge exists. Receiver yields result when done.
    InFlight {
        receiver: watch::Receiver<Option<MergeResult>>,
        metadata: SharedMergeMetadata,
    },
    /// Nothing found. Caller must start the merge.
    /// The guard publishes the result on completion and cleans up on drop.
    New { guard: InFlightGuard },
}

/// Metadata preserved for reattached merges — matches AsyncMergeHandle fields
/// needed for HAS serialization, GC/rooting, and clone behavior.
#[derive(Clone, Debug)]
pub struct SharedMergeMetadata {
    pub merge_key: MergeKey,
    pub input_curr_hash: Hash256,
    pub input_snap_hash: Hash256,
    pub input_file_paths: Vec<PathBuf>,
    pub level: usize,
}

/// Guard that owns an in-flight entry. Owned by the merge producer (spawned task).
///
/// On drop:
/// - If `complete()`/`fail()` was called: result already sent, entry removed.
/// - If NOT called (panic/early return): sends error to all waiters, removes entry.
///
/// This ensures the slot is removed exactly once and all waiters get a terminal value.
pub struct InFlightGuard {
    key: MergeKey,
    sender: Arc<watch::Sender<Option<MergeResult>>>,
    map: Arc<RwLock<BucketMergeMap>>,
    signaled: bool,
}

impl std::fmt::Debug for InFlightGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InFlightGuard")
            .field("key", &self.key)
            .field("signaled", &self.signaled)
            .finish_non_exhaustive()
    }
}

impl InFlightGuard {
    /// Signal successful completion. Records in completed cache if non-empty output.
    pub fn complete(mut self, bucket: Arc<Bucket>) {
        self.signaled = true;
        let output_hash = bucket.hash();
        let _ = self.sender.send(Some(Ok(bucket)));
        // Record in completed cache (if non-empty)
        if !output_hash.is_zero() {
            if let Ok(mut map) = self.map.write() {
                map.record_merge(self.key.clone(), output_hash);
            }
        }
        // Drop will remove from in_flight
    }

    /// Signal failure. Sends error to all reattached receivers.
    pub fn fail(mut self, error: &str) {
        self.signaled = true;
        let _ = self.sender.send(Some(Err(Arc::from(error))));
        // Drop will remove from in_flight
    }
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        if !self.signaled {
            // Panic/early-return path: signal stable terminal error
            let _ = self
                .sender
                .send(Some(Err(Arc::from("merge abandoned (guard dropped)"))));
        }
        // Always remove from in_flight (exactly once)
        if let Ok(mut map) = self.map.write() {
            map.in_flight.remove(&self.key);
        }
    }
}

pub(crate) struct InFlightEntry {
    sender: Arc<watch::Sender<Option<MergeResult>>>,
    metadata: SharedMergeMetadata,
}

// ============================================================================
// Bucket Merge Map
// ============================================================================

/// Tracks merge dedup state: both completed results and in-flight merges.
///
/// This is the Rust analogue of stellar-core's combined `mFinishedMerges` +
/// `mLiveBucketFutures` state in `BucketManager`. The single [`get_or_start`]
/// method provides atomic lookup across both submaps.
///
/// **Live bucket list only** — hot-archive dedup is a follow-up.
#[derive(Default)]
pub struct BucketMergeMap {
    /// Maps merge keys to output bucket hashes (completed merges).
    merge_key_to_output: HashMap<MergeKey, Hash256>,
    /// Maps input bucket hashes to the set of output bucket hashes they produced.
    input_to_output: HashMap<Hash256, HashSet<Hash256>>,
    /// Maps output bucket hashes to the merge keys that produced them.
    output_to_merge_key: HashMap<Hash256, Vec<MergeKey>>,
    /// In-flight merges (not yet completed).
    pub(crate) in_flight: HashMap<MergeKey, InFlightEntry>,
}

impl std::fmt::Debug for BucketMergeMap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BucketMergeMap")
            .field("completed", &self.merge_key_to_output.len())
            .field("in_flight", &self.in_flight.len())
            .finish()
    }
}

impl BucketMergeMap {
    /// Creates a new empty merge map.
    pub fn new() -> Self {
        Self::default()
    }

    /// Atomically check completed cache → in-flight map → register new.
    ///
    /// This is a single operation under write lock, eliminating races between
    /// checking and inserting. Callers must hold the write lock when calling this.
    ///
    /// The `map_arc` parameter is the `Arc<RwLock<Self>>` that wraps this map,
    /// needed so the returned `InFlightGuard` can remove its entry on drop.
    /// The `load_bucket` closure loads a bucket from disk by hash (for completed cache hits).
    pub fn get_or_start<F>(
        &mut self,
        key: &MergeKey,
        metadata: SharedMergeMetadata,
        map_arc: Arc<RwLock<BucketMergeMap>>,
        load_bucket: F,
    ) -> MergeSlot
    where
        F: FnOnce(&Hash256) -> Option<Arc<Bucket>>,
    {
        // 1. Check completed cache
        if let Some(output_hash) = self.merge_key_to_output.get(key).copied() {
            if !output_hash.is_zero() {
                if let Some(bucket) = load_bucket(&output_hash) {
                    return MergeSlot::Completed(bucket);
                }
                // Bucket file missing — fall through to start new merge
            }
        }

        // 2. Check in-flight map
        if let Some(entry) = self.in_flight.get(key) {
            return MergeSlot::InFlight {
                receiver: entry.sender.subscribe(),
                metadata: entry.metadata.clone(),
            };
        }

        // 3. Register new in-flight entry
        let (sender, _receiver) = watch::channel(None);
        let sender = Arc::new(sender);
        self.in_flight.insert(
            key.clone(),
            InFlightEntry {
                sender: Arc::clone(&sender),
                metadata: metadata.clone(),
            },
        );

        MergeSlot::New {
            guard: InFlightGuard {
                key: key.clone(),
                sender,
                map: map_arc,
                signaled: false,
            },
        }
    }

    /// Returns the number of in-flight merges.
    pub fn in_flight_count(&self) -> usize {
        self.in_flight.len()
    }

    /// Records a completed merge.
    ///
    /// # Arguments
    ///
    /// * `merge_key` - The key identifying the merge operation
    /// * `output_hash` - The hash of the resulting merged bucket
    pub fn record_merge(&mut self, merge_key: MergeKey, output_hash: Hash256) {
        // Record merge key -> output
        self.merge_key_to_output
            .insert(merge_key.clone(), output_hash);

        // Record input -> output mappings
        self.input_to_output
            .entry(merge_key.curr_hash)
            .or_default()
            .insert(output_hash);
        self.input_to_output
            .entry(merge_key.snap_hash)
            .or_default()
            .insert(output_hash);

        // Record output -> merge key mapping
        self.output_to_merge_key
            .entry(output_hash)
            .or_default()
            .push(merge_key);
    }

    /// Gets the output hash for a merge if it was previously completed.
    pub fn get_output(&self, merge_key: &MergeKey) -> Option<&Hash256> {
        self.merge_key_to_output.get(merge_key)
    }

    /// Checks if a merge was previously completed.
    pub fn has_output(&self, merge_key: &MergeKey) -> bool {
        self.merge_key_to_output.contains_key(merge_key)
    }

    /// Gets all outputs that used a given input bucket.
    pub fn get_outputs_for_input(&self, input_hash: &Hash256) -> Option<&HashSet<Hash256>> {
        self.input_to_output.get(input_hash)
    }

    /// Removes a merge record.
    ///
    /// This should be called when the output bucket is no longer needed.
    pub fn remove_merge(&mut self, merge_key: &MergeKey) {
        if let Some(output_hash) = self.merge_key_to_output.remove(merge_key) {
            // Remove from input mappings
            if let Some(outputs) = self.input_to_output.get_mut(&merge_key.curr_hash) {
                outputs.remove(&output_hash);
                if outputs.is_empty() {
                    self.input_to_output.remove(&merge_key.curr_hash);
                }
            }
            if let Some(outputs) = self.input_to_output.get_mut(&merge_key.snap_hash) {
                outputs.remove(&output_hash);
                if outputs.is_empty() {
                    self.input_to_output.remove(&merge_key.snap_hash);
                }
            }

            // Remove from output mapping
            if let Some(keys) = self.output_to_merge_key.get_mut(&output_hash) {
                keys.retain(|k| k != merge_key);
                if keys.is_empty() {
                    self.output_to_merge_key.remove(&output_hash);
                }
            }
        }
    }

    /// Removes all merge records for outputs not in the given set.
    ///
    /// Spec: BUCKETLISTDB_SPEC §8.2 (analogue) — housekeeping for the merge map.
    ///
    /// This is called by `retain_buckets()` after bucket files are deleted, to
    /// remove stale merge-map entries that reference deleted outputs. This is a
    /// consistency/memory-hygiene measure, NOT a GC safety mechanism — GC
    /// correctness depends solely on the keep-set being complete (see
    /// `retain_buckets` doc comment for the safety contract).
    pub fn retain_outputs(&mut self, keep: &HashSet<Hash256>) {
        // Find merge keys to remove
        let keys_to_remove: Vec<MergeKey> = self
            .merge_key_to_output
            .iter()
            .filter(|(_, output)| !keep.contains(*output))
            .map(|(key, _)| key.clone())
            .collect();

        // Remove them
        for key in keys_to_remove {
            self.remove_merge(&key);
        }
    }

    /// Removes all merge records that produce the given output hash.
    ///
    /// Spec: BUCKETLISTDB_SPEC §8.2 (analogue) — removes merge map entries producing
    /// a given output hash.
    ///
    /// Returns the set of merge keys that were removed.
    /// This is the Rust equivalent of stellar-core `forgetAllMergesProducing`.
    pub fn forget_all_merges_producing(&mut self, output_hash: &Hash256) -> HashSet<MergeKey> {
        // Get all merge keys that produced this output
        let keys_to_remove: Vec<MergeKey> = self
            .output_to_merge_key
            .get(output_hash)
            .cloned()
            .unwrap_or_default();

        let mut removed = HashSet::new();
        for key in keys_to_remove {
            if self.merge_key_to_output.get(&key) == Some(output_hash) {
                self.remove_merge(&key);
                removed.insert(key);
            }
        }
        removed
    }

    /// Returns the number of recorded merges.
    pub fn len(&self) -> usize {
        self.merge_key_to_output.len()
    }

    /// Checks if the map is empty.
    pub fn is_empty(&self) -> bool {
        self.merge_key_to_output.is_empty()
    }

    /// Clears all recorded merges.
    pub fn clear(&mut self) {
        self.merge_key_to_output.clear();
        self.input_to_output.clear();
        self.output_to_merge_key.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::merge::DeadEntryPolicy;

    fn make_hash(byte: u8) -> Hash256 {
        Hash256::from_bytes([byte; 32])
    }

    fn make_merge_key(curr: u8, snap: u8, keep_tombstones: DeadEntryPolicy) -> MergeKey {
        MergeKey::new(keep_tombstones, make_hash(curr), make_hash(snap))
    }

    #[test]
    fn test_bucket_merge_map_basic() {
        let mut map = BucketMergeMap::new();
        assert!(map.is_empty());

        let key = make_merge_key(1, 2, DeadEntryPolicy::Keep);
        let output = make_hash(3);

        map.record_merge(key.clone(), output);
        assert_eq!(map.len(), 1);
        assert!(map.has_output(&key));
        assert_eq!(map.get_output(&key), Some(&output));

        // Check input -> output mapping
        let outputs = map.get_outputs_for_input(&make_hash(1)).unwrap();
        assert!(outputs.contains(&output));
    }

    #[test]
    fn test_bucket_merge_map_remove() {
        let mut map = BucketMergeMap::new();

        let key1 = make_merge_key(1, 2, DeadEntryPolicy::Keep);
        let key2 = make_merge_key(3, 4, DeadEntryPolicy::Keep);
        let output1 = make_hash(10);
        let output2 = make_hash(11);

        map.record_merge(key1.clone(), output1);
        map.record_merge(key2.clone(), output2);
        assert_eq!(map.len(), 2);

        map.remove_merge(&key1);
        assert_eq!(map.len(), 1);
        assert!(!map.has_output(&key1));
        assert!(map.has_output(&key2));
    }

    #[test]
    fn test_bucket_merge_map_retain() {
        let mut map = BucketMergeMap::new();

        let key1 = make_merge_key(1, 2, DeadEntryPolicy::Keep);
        let key2 = make_merge_key(3, 4, DeadEntryPolicy::Keep);
        let output1 = make_hash(10);
        let output2 = make_hash(11);

        map.record_merge(key1.clone(), output1);
        map.record_merge(key2.clone(), output2);

        // Keep only output2
        let keep: HashSet<Hash256> = [output2].into_iter().collect();
        map.retain_outputs(&keep);

        assert_eq!(map.len(), 1);
        assert!(!map.has_output(&key1));
        assert!(map.has_output(&key2));
    }

    #[test]
    fn test_merge_key_equality() {
        let key1 = make_merge_key(1, 2, DeadEntryPolicy::Keep);
        let key2 = make_merge_key(1, 2, DeadEntryPolicy::Keep);
        let key3 = make_merge_key(1, 2, DeadEntryPolicy::Remove);
        let key4 = make_merge_key(1, 3, DeadEntryPolicy::Keep);

        assert_eq!(key1, key2);
        assert_ne!(key1, key3); // Different keep_tombstones
        assert_ne!(key1, key4); // Different snap_hash
    }

    fn make_metadata(key: &MergeKey, level: usize) -> SharedMergeMetadata {
        SharedMergeMetadata {
            merge_key: key.clone(),
            input_curr_hash: key.curr_hash,
            input_snap_hash: key.snap_hash,
            input_file_paths: Vec::new(),
            level,
        }
    }

    #[test]
    fn test_get_or_start_returns_new() {
        let map = Arc::new(RwLock::new(BucketMergeMap::new()));
        let key = make_merge_key(1, 2, DeadEntryPolicy::Keep);
        let metadata = make_metadata(&key, 1);

        let slot = map
            .write()
            .unwrap()
            .get_or_start(&key, metadata, Arc::clone(&map), |_| None);
        assert!(matches!(slot, MergeSlot::New { .. }));
        assert_eq!(map.read().unwrap().in_flight_count(), 1);
    }

    #[test]
    fn test_get_or_start_returns_in_flight() {
        let map = Arc::new(RwLock::new(BucketMergeMap::new()));
        let key = make_merge_key(1, 2, DeadEntryPolicy::Keep);
        let metadata = make_metadata(&key, 1);

        // First call: New
        let slot =
            map.write()
                .unwrap()
                .get_or_start(&key, metadata.clone(), Arc::clone(&map), |_| None);
        let _guard = match slot {
            MergeSlot::New { guard } => guard,
            _ => panic!("expected New"),
        };

        // Second call: InFlight
        let slot2 = map
            .write()
            .unwrap()
            .get_or_start(&key, metadata, Arc::clone(&map), |_| None);
        assert!(matches!(slot2, MergeSlot::InFlight { .. }));
    }

    #[test]
    fn test_get_or_start_returns_completed() {
        let map = Arc::new(RwLock::new(BucketMergeMap::new()));
        let key = make_merge_key(1, 2, DeadEntryPolicy::Keep);
        let output_hash = make_hash(99);
        let metadata = make_metadata(&key, 1);

        // Pre-record a completed merge
        map.write().unwrap().record_merge(key.clone(), output_hash);

        let bucket = Arc::new(Bucket::empty()); // placeholder
        let bucket_clone = Arc::clone(&bucket);
        let slot =
            map.write()
                .unwrap()
                .get_or_start(&key, metadata, Arc::clone(&map), move |_hash| {
                    Some(bucket_clone.clone())
                });
        assert!(matches!(slot, MergeSlot::Completed(_)));
    }

    #[test]
    fn test_guard_complete_records_in_completed_cache() {
        let map = Arc::new(RwLock::new(BucketMergeMap::new()));
        let key = make_merge_key(1, 2, DeadEntryPolicy::Keep);
        let metadata = make_metadata(&key, 1);

        let slot = map
            .write()
            .unwrap()
            .get_or_start(&key, metadata, Arc::clone(&map), |_| None);
        let guard = match slot {
            MergeSlot::New { guard } => guard,
            _ => panic!("expected New"),
        };

        // Create a non-empty bucket (use a known non-zero hash)
        let bucket = Arc::new(
            Bucket::from_entries(vec![stellar_xdr::curr::BucketEntry::Liveentry(
                stellar_xdr::curr::LedgerEntry {
                    last_modified_ledger_seq: 1,
                    data: stellar_xdr::curr::LedgerEntryData::Ttl(stellar_xdr::curr::TtlEntry {
                        key_hash: stellar_xdr::curr::Hash([1; 32]),
                        live_until_ledger_seq: 100,
                    }),
                    ext: stellar_xdr::curr::LedgerEntryExt::V0,
                },
            )])
            .unwrap(),
        );
        let output_hash = bucket.hash();
        guard.complete(bucket);

        // Should be in completed cache now
        let map_read = map.read().unwrap();
        assert!(map_read.has_output(&key));
        assert_eq!(map_read.get_output(&key), Some(&output_hash));
        assert_eq!(map_read.in_flight_count(), 0);
    }

    #[test]
    fn test_guard_complete_empty_does_not_record() {
        let map = Arc::new(RwLock::new(BucketMergeMap::new()));
        let key = make_merge_key(1, 2, DeadEntryPolicy::Keep);
        let metadata = make_metadata(&key, 1);

        let slot = map
            .write()
            .unwrap()
            .get_or_start(&key, metadata, Arc::clone(&map), |_| None);
        let guard = match slot {
            MergeSlot::New { guard } => guard,
            _ => panic!("expected New"),
        };

        // Complete with empty bucket
        guard.complete(Arc::new(Bucket::empty()));

        // Should NOT be in completed cache (empty output)
        let map_read = map.read().unwrap();
        assert!(!map_read.has_output(&key));
        assert_eq!(map_read.in_flight_count(), 0);
    }

    #[test]
    fn test_guard_drop_signals_error() {
        let map = Arc::new(RwLock::new(BucketMergeMap::new()));
        let key = make_merge_key(1, 2, DeadEntryPolicy::Keep);
        let metadata = make_metadata(&key, 1);

        let slot =
            map.write()
                .unwrap()
                .get_or_start(&key, metadata.clone(), Arc::clone(&map), |_| None);
        let guard = match slot {
            MergeSlot::New { guard } => guard,
            _ => panic!("expected New"),
        };

        // Get a receiver before dropping
        let slot2 = map
            .write()
            .unwrap()
            .get_or_start(&key, metadata, Arc::clone(&map), |_| None);
        let receiver = match slot2 {
            MergeSlot::InFlight { receiver, .. } => receiver,
            _ => panic!("expected InFlight"),
        };

        // Drop guard without completing
        drop(guard);

        // Receiver should have an error
        let value = receiver.borrow().clone();
        assert!(value.is_some());
        let result = value.unwrap();
        assert!(result.is_err());
        assert_eq!(map.read().unwrap().in_flight_count(), 0);
    }

    #[test]
    fn test_retain_outputs_does_not_affect_in_flight() {
        let map = Arc::new(RwLock::new(BucketMergeMap::new()));
        let key = make_merge_key(1, 2, DeadEntryPolicy::Keep);
        let metadata = make_metadata(&key, 1);

        // Start an in-flight merge
        let slot = map
            .write()
            .unwrap()
            .get_or_start(&key, metadata, Arc::clone(&map), |_| None);
        let _guard = match slot {
            MergeSlot::New { guard } => guard,
            _ => panic!("expected New"),
        };

        // GC with empty keep set
        let keep: HashSet<Hash256> = HashSet::new();
        map.write().unwrap().retain_outputs(&keep);

        // In-flight should still be there
        assert_eq!(map.read().unwrap().in_flight_count(), 1);
    }
}
