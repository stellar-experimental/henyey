//! Generic entry store for managing ledger entry lifecycle.
//!
//! `EntryStore<K, V>` bundles the five parallel collections that every entry type
//! in `LedgerStateManager` requires: a live-entries map, a snapshot map, a
//! created-entries set, a modified-entries tracking vec, and an optional
//! deleted-entries set (for Soroban types).
//!
//! The store handles all store-internal bookkeeping (snapshot management,
//! created/modified tracking, rollback, savepoint, commit). Shared-state
//! operations (delta recording, op-snapshot capture, last-modified tracking,
//! sponsorship metadata) remain on `LedgerStateManager` in thin wrapper methods.

use std::collections::{HashMap, HashSet};
use std::hash::Hash;

/// Per-type entry store bundling the parallel collections needed for snapshot,
/// rollback, commit, and flush lifecycle management.
#[derive(Clone)]
#[allow(dead_code)]
pub struct EntryStore<K: Eq + Hash + Clone, V: Clone> {
    /// Live entries.
    entries: HashMap<K, V>,
    /// Snapshot of each entry's value at the start of the current transaction.
    /// `Some(value)` means the entry existed before the TX; `None` means it did not.
    snapshots: HashMap<K, Option<V>>,
    /// Keys of entries created during this transaction (for rollback: remove rather than restore).
    created: HashSet<K>,
    /// Keys of entries that have been mutated and need flush consideration.
    modified: Vec<K>,
    /// Optional set of deleted entry keys (only for Soroban types that need to prevent
    /// bucket-list reload of deleted entries across transactions in the same ledger).
    deleted: Option<HashSet<K>>,
}

/// Savepoint state for a single entry store, captured by `EntryStore::create_savepoint`.
#[allow(dead_code)]
pub struct EntryStoreSavepoint<K: Eq + Hash + Clone, V: Clone> {
    /// Clone of the snapshot map at savepoint time.
    snapshots: HashMap<K, Option<V>>,
    /// Current live values for all snapshot'd keys at savepoint time (for restoring
    /// entries that were modified before the savepoint and re-modified after).
    pre_values: Vec<(K, Option<V>)>,
    /// Clone of the created set at savepoint time.
    created: HashSet<K>,
    /// Length of the modified vec at savepoint time (for truncation on rollback).
    modified_len: usize,
}

#[allow(dead_code)]
impl<K, V> EntryStore<K, V>
where
    K: Eq + Hash + Clone,
    V: Clone,
{
    /// Create a new empty entry store without deleted-entry tracking.
    pub fn new() -> Self {
        Self {
            entries: HashMap::new(),
            snapshots: HashMap::new(),
            created: HashSet::new(),
            modified: Vec::new(),
            deleted: None,
        }
    }

    /// Create a new empty entry store with deleted-entry tracking enabled.
    /// Used for Soroban types (ContractData, ContractCode) that must prevent
    /// bucket-list reload of deleted entries.
    pub fn new_with_deleted_tracking() -> Self {
        Self {
            entries: HashMap::new(),
            snapshots: HashMap::new(),
            created: HashSet::new(),
            modified: Vec::new(),
            deleted: Some(HashSet::new()),
        }
    }

    // ── Storage access ──────────────────────────────────────────────

    /// Get an entry by key (read-only).
    pub fn get(&self, key: &K) -> Option<&V> {
        self.entries.get(key)
    }

    /// Check if the entry exists in the live map.
    pub fn contains(&self, key: &K) -> bool {
        self.entries.contains_key(key)
    }

    /// Check if an entry was loaded/created during this transaction (has a snapshot).
    pub fn is_tracked(&self, key: &K) -> bool {
        self.snapshots.contains_key(key)
    }

    /// Read access to the live entries map.
    pub fn entries(&self) -> &HashMap<K, V> {
        &self.entries
    }

    /// Mutable access to the live entries map (for `apply_entry_no_tracking`,
    /// `load_entry`, and cache clearing).
    pub fn entries_mut(&mut self) -> &mut HashMap<K, V> {
        &mut self.entries
    }

    // ── Snapshot management ─────────────────────────────────────────

    /// Ensure a snapshot exists for the given key, using the `get_mut` semantics:
    /// only save a snapshot if there isn't already a `Some` value in the snapshot map.
    ///
    /// This allows the snapshot to be "upgraded" from `None` (created entry) to the
    /// current value on first `get_mut`, enabling STATE/UPDATED delta pairs for
    /// entries that were created earlier in the same transaction.
    pub fn ensure_snapshot(&mut self, key: &K) {
        if !self.snapshots.get(key).is_some_and(|s| s.is_some()) {
            let snapshot = self.entries.get(key).cloned();
            self.snapshots.insert(key.clone(), snapshot);
        }
    }

    /// Ensure a snapshot exists for the given key, using the `update`/`delete` semantics:
    /// only save a snapshot if the key has never been snapshot'd at all.
    ///
    /// Unlike `ensure_snapshot`, this preserves `None` snapshots for created entries,
    /// which is correct for `update` and `delete` where we want to preserve the original
    /// pre-TX state (None = "didn't exist") rather than upgrading to current value.
    pub fn ensure_snapshot_on_first(&mut self, key: &K) {
        if !self.snapshots.contains_key(key) {
            let snapshot = self.entries.get(key).cloned();
            self.snapshots.insert(key.clone(), snapshot);
        }
    }

    /// Get the snapshot value for a key (for flush comparison).
    pub fn snapshot_value(&self, key: &K) -> Option<&Option<V>> {
        self.snapshots.get(key)
    }

    // ── Modification tracking ───────────────────────────────────────

    /// Track a key as modified (if not already tracked).
    fn track_modified(&mut self, key: &K) {
        if !self.modified.contains(key) {
            self.modified.push(key.clone());
        }
    }

    // ── CRUD (store-internal parts) ─────────────────────────────────

    /// Get a mutable reference to an entry, tracking the modification.
    ///
    /// Returns `None` if the key is not present. The caller must call
    /// `ensure_snapshot` and handle shared-state bookkeeping (op_snapshot,
    /// last_modified) before calling this.
    pub fn get_mut_tracked(&mut self, key: &K) -> Option<&mut V> {
        if self.entries.contains_key(key) {
            self.track_modified(key);
            self.entries.get_mut(key)
        } else {
            None
        }
    }

    /// Insert a newly created entry.
    ///
    /// Records a `None` snapshot (entry didn't exist before), inserts the entry,
    /// marks it as created (for rollback), and tracks it as modified (for flush).
    /// The caller must handle shared-state bookkeeping (delta.record_create,
    /// snapshot_last_modified_key, set_last_modified_key) before calling this.
    pub fn insert_created(&mut self, key: K, value: V) {
        self.snapshots.entry(key.clone()).or_insert(None);
        self.entries.insert(key.clone(), value);
        self.created.insert(key.clone());
        self.track_modified(&key);
    }

    /// Insert an updated entry value.
    ///
    /// The caller must have already called `ensure_snapshot_on_first` and handled
    /// shared-state bookkeeping (capture_op_snapshot, delta.record_update,
    /// snapshot_last_modified_key, set_last_modified_key).
    ///
    /// `track_modified` controls whether the key is added to the modified vec.
    /// Most types pass `false` (the update already recorded directly to delta).
    /// LiquidityPool passes `true`.
    pub fn insert_updated(&mut self, key: K, value: V, track_modified: bool) {
        self.entries.insert(key.clone(), value);
        if track_modified {
            self.track_modified(&key);
        }
    }

    /// Remove a deleted entry.
    ///
    /// The caller must have already called `ensure_snapshot_on_first` and handled
    /// shared-state bookkeeping (capture_op_snapshot, delta.record_delete,
    /// snapshot_last_modified_key, clear_entry_sponsorship_metadata,
    /// remove_last_modified_key).
    ///
    /// If `track_deleted` is true and this store has deleted-entry tracking enabled,
    /// the key is added to the deleted set. If `track_modified` is true, the key is
    /// added to the modified vec (only LiquidityPool uses this).
    pub fn remove_deleted(&mut self, key: &K, track_deleted: bool, track_modified: bool) {
        self.entries.remove(key);
        if track_deleted {
            debug_assert!(
                self.deleted.is_some(),
                "track_deleted=true on store without deleted tracking"
            );
            if let Some(ref mut deleted) = self.deleted {
                deleted.insert(key.clone());
            }
        }
        if track_modified {
            self.track_modified(key);
        }
    }

    // ── Deleted-entry tracking (Soroban) ────────────────────────────

    /// Check if a key has been deleted in this ledger.
    /// Always returns `false` if deleted-entry tracking is not enabled.
    pub fn is_deleted(&self, key: &K) -> bool {
        self.deleted.as_ref().is_some_and(|d| d.contains(key))
    }

    /// Mark a key as deleted without requiring the entry to be in the store.
    /// Used to prevent bucket-list reload across transaction stages.
    pub fn mark_deleted(&mut self, key: K) {
        if let Some(ref mut deleted) = self.deleted {
            deleted.insert(key);
        }
    }

    // ── Savepoint / Rollback / Commit ───────────────────────────────

    /// Create a savepoint capturing the current state of this store's tracking.
    pub fn create_savepoint(&self) -> EntryStoreSavepoint<K, V> {
        let pre_values = self
            .snapshots
            .keys()
            .map(|k| (k.clone(), self.entries.get(k).cloned()))
            .collect();
        EntryStoreSavepoint {
            snapshots: self.snapshots.clone(),
            pre_values,
            created: self.created.clone(),
            modified_len: self.modified.len(),
        }
    }

    /// Rollback to a savepoint, restoring entries to their pre-savepoint state.
    ///
    /// This implements the same two-phase rollback as the existing free functions
    /// `rollback_new_snapshots` and `apply_pre_values`:
    /// - Phase 1: Restore entries snapshot'd *after* the savepoint (new snapshots)
    /// - Phase 2: Restore pre-savepoint values for entries already in snapshot maps
    /// - Phase 3: Restore snapshot map, created set, and truncate modified vec
    pub fn rollback_to_savepoint(&mut self, sp: EntryStoreSavepoint<K, V>) {
        // Phase 1: Restore entries snapshot'd after the savepoint
        for (key, snapshot) in &self.snapshots {
            if !sp.snapshots.contains_key(key) {
                match snapshot {
                    Some(entry) => {
                        self.entries.insert(key.clone(), entry.clone());
                    }
                    None => {
                        self.entries.remove(key);
                    }
                }
            }
        }

        // Phase 2: Restore pre-savepoint values
        for (key, value) in sp.pre_values {
            match value {
                Some(entry) => {
                    self.entries.insert(key, entry);
                }
                None => {
                    self.entries.remove(&key);
                }
            }
        }

        // Phase 3: Restore tracking state
        self.snapshots = sp.snapshots;
        self.created = sp.created;
        self.modified.truncate(sp.modified_len);
    }

    /// Full transaction rollback: remove created entries, restore pre-TX values.
    ///
    /// Equivalent to the existing `rollback_entries` free function.
    pub fn rollback(&mut self) {
        for (key, snapshot) in self.snapshots.drain() {
            if self.created.contains(&key) {
                self.entries.remove(&key);
            } else if let Some(entry) = snapshot {
                self.entries.insert(key, entry);
            }
        }
        self.created.clear();
        self.modified.clear();
    }

    /// Commit: clear all transaction tracking (snapshots, created, modified).
    /// The live entries remain as-is.
    pub fn commit(&mut self) {
        self.snapshots.clear();
        self.created.clear();
        self.modified.clear();
    }

    // ── Flush support ───────────────────────────────────────────────

    /// Take the modified keys vec, replacing it with an empty vec.
    pub fn take_modified(&mut self) -> Vec<K> {
        std::mem::take(&mut self.modified)
    }

    /// Check if a key is in the created set (for flush filtering, e.g., TTL).
    pub fn is_created(&self, key: &K) -> bool {
        self.created.contains(key)
    }

    // ── Cache management ────────────────────────────────────────────

    /// Clear all state (entries, snapshots, created, modified, deleted).
    pub fn clear(&mut self) {
        self.entries.clear();
        self.snapshots.clear();
        self.created.clear();
        self.modified.clear();
        if let Some(ref mut deleted) = self.deleted {
            deleted.clear();
        }
    }

    /// Estimate heap bytes used by this entry store.
    ///
    /// Uses capacity-based estimation for O(1) cost. `key_size` and `value_size`
    /// are the estimated sizes of individual K and V instances.
    pub fn estimate_heap_bytes(&self, key_size: usize, value_size: usize) -> usize {
        use henyey_common::memory::{hashmap_heap_bytes, hashset_heap_bytes, vec_heap_bytes};
        let entries = hashmap_heap_bytes(self.entries.capacity(), key_size, value_size);
        // snapshots: HashMap<K, Option<V>> — Option<V> is same size as V for most types
        let snapshots = hashmap_heap_bytes(self.snapshots.capacity(), key_size, value_size + 1);
        let created = hashset_heap_bytes(self.created.capacity(), key_size);
        let modified = vec_heap_bytes(self.modified.capacity(), key_size);
        let deleted = self
            .deleted
            .as_ref()
            .map(|d| hashset_heap_bytes(d.capacity(), key_size))
            .unwrap_or(0);
        entries + snapshots + created + modified + deleted
    }

    /// Number of live entries.
    pub fn len(&self) -> usize {
        self.entries.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Helper to create a basic store for testing
    fn new_store() -> EntryStore<u32, String> {
        EntryStore::new()
    }

    fn new_store_with_deleted() -> EntryStore<u32, String> {
        EntryStore::new_with_deleted_tracking()
    }

    // ── CRUD basics ─────────────────────────────────────────────────

    #[test]
    fn test_get_empty() {
        let store = new_store();
        assert!(store.get(&1).is_none());
    }

    #[test]
    fn test_contains_empty() {
        let store = new_store();
        assert!(!store.contains(&1));
    }

    #[test]
    fn test_insert_created_and_get() {
        let mut store = new_store();
        store.insert_created(1, "hello".to_string());
        assert_eq!(store.get(&1), Some(&"hello".to_string()));
        assert!(store.contains(&1));
    }

    #[test]
    fn test_insert_created_snapshots_none() {
        let mut store = new_store();
        store.insert_created(1, "hello".to_string());
        // Snapshot should be None (entry didn't exist before creation)
        assert_eq!(store.snapshot_value(&1), Some(&None));
    }

    #[test]
    fn test_insert_created_tracks_modified() {
        let mut store = new_store();
        store.insert_created(1, "hello".to_string());
        let modified = store.take_modified();
        assert_eq!(modified, vec![1]);
    }

    #[test]
    fn test_insert_created_tracks_created() {
        let mut store = new_store();
        store.insert_created(1, "hello".to_string());
        assert!(store.is_created(&1));
    }

    #[test]
    fn test_get_mut_tracked_returns_value() {
        let mut store = new_store();
        store.entries_mut().insert(1, "hello".to_string());
        store.ensure_snapshot(&1);
        let val = store.get_mut_tracked(&1).unwrap();
        assert_eq!(val, "hello");
        *val = "world".to_string();
        assert_eq!(store.get(&1), Some(&"world".to_string()));
    }

    #[test]
    fn test_get_mut_tracked_modifies() {
        let mut store = new_store();
        store.entries_mut().insert(1, "hello".to_string());
        store.ensure_snapshot(&1);
        store.get_mut_tracked(&1);
        let modified = store.take_modified();
        assert_eq!(modified, vec![1]);
    }

    #[test]
    fn test_get_mut_missing_returns_none() {
        let mut store = new_store();
        assert!(store.get_mut_tracked(&1).is_none());
    }

    #[test]
    fn test_insert_updated() {
        let mut store = new_store();
        store.entries_mut().insert(1, "hello".to_string());
        store.ensure_snapshot_on_first(&1);
        store.insert_updated(1, "world".to_string(), false);
        assert_eq!(store.get(&1), Some(&"world".to_string()));
        // modified should be empty since track_modified=false
        assert!(store.take_modified().is_empty());
    }

    #[test]
    fn test_insert_updated_with_tracking() {
        let mut store = new_store();
        store.entries_mut().insert(1, "hello".to_string());
        store.ensure_snapshot_on_first(&1);
        store.insert_updated(1, "world".to_string(), true);
        assert_eq!(store.take_modified(), vec![1]);
    }

    #[test]
    fn test_remove_deleted() {
        let mut store = new_store();
        store.entries_mut().insert(1, "hello".to_string());
        store.ensure_snapshot_on_first(&1);
        store.remove_deleted(&1, false, false);
        assert!(!store.contains(&1));
        assert!(store.get(&1).is_none());
    }

    // ── Snapshot behavior ───────────────────────────────────────────

    #[test]
    fn test_ensure_snapshot_preserves_existing_some() {
        let mut store = new_store();
        store.entries_mut().insert(1, "original".to_string());
        store.ensure_snapshot(&1);
        // Modify the entry
        *store.entries_mut().get_mut(&1).unwrap() = "modified".to_string();
        // Second ensure_snapshot should NOT overwrite since snapshot is already Some
        store.ensure_snapshot(&1);
        assert_eq!(
            store.snapshot_value(&1),
            Some(&Some("original".to_string()))
        );
    }

    #[test]
    fn test_ensure_snapshot_upgrades_none_to_current() {
        let mut store = new_store();
        // Create entry (snapshot is None)
        store.insert_created(1, "created".to_string());
        assert_eq!(store.snapshot_value(&1), Some(&None));
        // ensure_snapshot should upgrade None to current value
        store.ensure_snapshot(&1);
        assert_eq!(store.snapshot_value(&1), Some(&Some("created".to_string())));
    }

    #[test]
    fn test_ensure_snapshot_on_first_preserves_none() {
        let mut store = new_store();
        // Create entry (snapshot is None)
        store.insert_created(1, "created".to_string());
        assert_eq!(store.snapshot_value(&1), Some(&None));
        // ensure_snapshot_on_first should NOT overwrite since key already in snapshots
        store.ensure_snapshot_on_first(&1);
        assert_eq!(store.snapshot_value(&1), Some(&None));
    }

    #[test]
    fn test_snapshot_value_returns_pre_mutation_state() {
        let mut store = new_store();
        store.entries_mut().insert(1, "original".to_string());
        store.ensure_snapshot(&1);
        // Mutate via entries_mut (simulating what get_mut_tracked caller does)
        *store.entries_mut().get_mut(&1).unwrap() = "mutated".to_string();
        // Snapshot still has original
        assert_eq!(
            store.snapshot_value(&1),
            Some(&Some("original".to_string()))
        );
    }

    #[test]
    fn test_multiple_get_mut_preserves_first_snapshot() {
        let mut store = new_store();
        store.entries_mut().insert(1, "v1".to_string());
        store.ensure_snapshot(&1);
        *store.entries_mut().get_mut(&1).unwrap() = "v2".to_string();
        store.ensure_snapshot(&1); // second call
                                   // Snapshot should still be "v1" from the first ensure_snapshot
        assert_eq!(store.snapshot_value(&1), Some(&Some("v1".to_string())));
    }

    #[test]
    fn test_is_tracked_after_create() {
        let mut store = new_store();
        store.insert_created(1, "hello".to_string());
        assert!(store.is_tracked(&1));
    }

    #[test]
    fn test_is_tracked_after_snapshot() {
        let mut store = new_store();
        store.entries_mut().insert(1, "hello".to_string());
        assert!(!store.is_tracked(&1)); // not tracked yet
        store.ensure_snapshot(&1);
        assert!(store.is_tracked(&1)); // now tracked
    }

    // ── Savepoint / Rollback ────────────────────────────────────────

    #[test]
    fn test_create_savepoint_captures_state() {
        let mut store = new_store();
        store.insert_created(1, "hello".to_string());
        let sp = store.create_savepoint();
        assert!(sp.snapshots.contains_key(&1));
        assert!(sp.created.contains(&1));
        assert_eq!(sp.modified_len, 1);
    }

    #[test]
    fn test_rollback_to_savepoint_removes_created_after_sp() {
        let mut store = new_store();
        let sp = store.create_savepoint();
        store.insert_created(1, "hello".to_string());
        assert!(store.contains(&1));
        store.rollback_to_savepoint(sp);
        assert!(!store.contains(&1));
    }

    #[test]
    fn test_rollback_to_savepoint_restores_modified_entry() {
        let mut store = new_store();
        store.entries_mut().insert(1, "original".to_string());
        store.ensure_snapshot(&1);
        // Savepoint after initial load
        let sp = store.create_savepoint();
        // Modify after savepoint
        *store.get_mut_tracked(&1).unwrap() = "modified".to_string();
        assert_eq!(store.get(&1), Some(&"modified".to_string()));
        // Rollback
        store.rollback_to_savepoint(sp);
        assert_eq!(store.get(&1), Some(&"original".to_string()));
    }

    #[test]
    fn test_rollback_to_savepoint_preserves_pre_sp_create() {
        let mut store = new_store();
        store.insert_created(1, "before_sp".to_string());
        let sp = store.create_savepoint();
        store.insert_created(2, "after_sp".to_string());
        store.rollback_to_savepoint(sp);
        // Entry 1 (created before SP) should still exist
        assert!(store.contains(&1));
        assert_eq!(store.get(&1), Some(&"before_sp".to_string()));
        // Entry 2 (created after SP) should be gone
        assert!(!store.contains(&2));
    }

    #[test]
    fn test_rollback_to_savepoint_preserves_pre_sp_modify() {
        let mut store = new_store();
        store.entries_mut().insert(1, "original".to_string());
        store.ensure_snapshot(&1);
        // Modify before savepoint
        *store.get_mut_tracked(&1).unwrap() = "pre_sp_modified".to_string();
        let sp = store.create_savepoint();
        // Modify after savepoint
        *store.get_mut_tracked(&1).unwrap() = "post_sp_modified".to_string();
        store.rollback_to_savepoint(sp);
        // Should restore to pre-savepoint value (the modification before SP)
        assert_eq!(store.get(&1), Some(&"pre_sp_modified".to_string()));
    }

    #[test]
    fn test_rollback_to_savepoint_restores_deleted_entry() {
        let mut store = new_store();
        store.entries_mut().insert(1, "hello".to_string());
        store.ensure_snapshot(&1);
        let sp = store.create_savepoint();
        store.remove_deleted(&1, false, false);
        assert!(!store.contains(&1));
        store.rollback_to_savepoint(sp);
        assert!(store.contains(&1));
        assert_eq!(store.get(&1), Some(&"hello".to_string()));
    }

    #[test]
    fn test_rollback_to_savepoint_truncates_modified() {
        let mut store = new_store();
        store.entries_mut().insert(1, "a".to_string());
        store.ensure_snapshot(&1);
        store.get_mut_tracked(&1); // modified = [1]
        let sp = store.create_savepoint();
        store.entries_mut().insert(2, "b".to_string());
        store.ensure_snapshot(&2);
        store.get_mut_tracked(&2); // modified = [1, 2]
        store.rollback_to_savepoint(sp);
        // Modified should be truncated back to length 1
        let modified = store.take_modified();
        assert_eq!(modified, vec![1]);
    }

    #[test]
    fn test_rollback_full_removes_created() {
        let mut store = new_store();
        store.insert_created(1, "hello".to_string());
        assert!(store.contains(&1));
        store.rollback();
        assert!(!store.contains(&1));
    }

    #[test]
    fn test_rollback_full_restores_pre_tx_values() {
        let mut store = new_store();
        store.entries_mut().insert(1, "original".to_string());
        store.ensure_snapshot(&1);
        *store.get_mut_tracked(&1).unwrap() = "modified".to_string();
        store.rollback();
        assert_eq!(store.get(&1), Some(&"original".to_string()));
    }

    #[test]
    fn test_rollback_full_clears_modified() {
        let mut store = new_store();
        store.entries_mut().insert(1, "a".to_string());
        store.ensure_snapshot(&1);
        store.get_mut_tracked(&1);
        store.rollback();
        assert!(store.take_modified().is_empty());
    }

    #[test]
    fn test_commit_clears_tracking() {
        let mut store = new_store();
        store.entries_mut().insert(1, "original".to_string());
        store.ensure_snapshot(&1);
        store.insert_created(2, "new".to_string());
        store.get_mut_tracked(&1);
        store.commit();
        // Snapshots, created, modified should all be cleared
        assert!(!store.is_tracked(&1));
        assert!(!store.is_tracked(&2));
        assert!(!store.is_created(&2));
        assert!(store.take_modified().is_empty());
        // But entries should still be there
        assert!(store.contains(&1));
        assert!(store.contains(&2));
    }

    #[test]
    fn test_nested_savepoint_rollback() {
        let mut store = new_store();
        store.entries_mut().insert(1, "v0".to_string());
        store.ensure_snapshot(&1);

        // Modify to v1
        *store.get_mut_tracked(&1).unwrap() = "v1".to_string();
        let sp1 = store.create_savepoint();

        // Modify to v2
        *store.get_mut_tracked(&1).unwrap() = "v2".to_string();
        let sp2 = store.create_savepoint();

        // Modify to v3
        *store.get_mut_tracked(&1).unwrap() = "v3".to_string();
        assert_eq!(store.get(&1), Some(&"v3".to_string()));

        // Rollback to sp2 → should restore to v2
        store.rollback_to_savepoint(sp2);
        assert_eq!(store.get(&1), Some(&"v2".to_string()));

        // Rollback to sp1 → should restore to v1
        store.rollback_to_savepoint(sp1);
        assert_eq!(store.get(&1), Some(&"v1".to_string()));
    }

    // ── Flush support ───────────────────────────────────────────────

    #[test]
    fn test_take_modified_returns_and_clears() {
        let mut store = new_store();
        store.entries_mut().insert(1, "a".to_string());
        store.ensure_snapshot(&1);
        store.get_mut_tracked(&1);
        let modified = store.take_modified();
        assert_eq!(modified, vec![1]);
        assert!(store.take_modified().is_empty());
    }

    #[test]
    fn test_flush_unchanged_value() {
        let mut store = new_store();
        store.entries_mut().insert(1, "same".to_string());
        store.ensure_snapshot(&1);
        store.get_mut_tracked(&1); // tracked but not actually changed
        if let Some(Some(snapshot)) = store.snapshot_value(&1) {
            assert_eq!(snapshot, store.get(&1).unwrap());
        }
    }

    #[test]
    fn test_flush_changed_value() {
        let mut store = new_store();
        store.entries_mut().insert(1, "before".to_string());
        store.ensure_snapshot(&1);
        *store.get_mut_tracked(&1).unwrap() = "after".to_string();
        if let Some(Some(snapshot)) = store.snapshot_value(&1) {
            assert_ne!(snapshot, store.get(&1).unwrap());
        }
    }

    // ── Deleted-entry tracking ──────────────────────────────────────

    #[test]
    fn test_deleted_tracking_disabled_by_default() {
        let store = new_store();
        assert!(!store.is_deleted(&1));
    }

    #[test]
    fn test_deleted_tracking_enabled() {
        let mut store = new_store_with_deleted();
        assert!(!store.is_deleted(&1));
        store.mark_deleted(1);
        assert!(store.is_deleted(&1));
    }

    #[test]
    fn test_remove_deleted_tracks_in_deleted_set() {
        let mut store = new_store_with_deleted();
        store.entries_mut().insert(1, "hello".to_string());
        store.ensure_snapshot_on_first(&1);
        store.remove_deleted(&1, true, false);
        assert!(store.is_deleted(&1));
    }

    #[test]
    fn test_remove_deleted_no_tracking_without_flag() {
        let mut store = new_store_with_deleted();
        store.entries_mut().insert(1, "hello".to_string());
        store.ensure_snapshot_on_first(&1);
        store.remove_deleted(&1, false, false);
        assert!(!store.is_deleted(&1));
    }

    #[test]
    fn test_deleted_survives_commit() {
        let mut store = new_store_with_deleted();
        store.mark_deleted(1);
        store.commit();
        // Deleted set is NOT cleared by commit (persistent across TXs in same ledger)
        assert!(store.is_deleted(&1));
    }

    #[test]
    fn test_deleted_survives_savepoint_rollback() {
        let mut store = new_store_with_deleted();
        store.entries_mut().insert(1, "hello".to_string());
        store.ensure_snapshot_on_first(&1);
        let sp = store.create_savepoint();
        // Delete after savepoint
        store.remove_deleted(&1, true, false);
        assert!(store.is_deleted(&1));
        // Rollback should restore the entry but NOT undo the deleted marker
        store.rollback_to_savepoint(sp);
        assert!(store.contains(&1));
        assert!(
            store.is_deleted(&1),
            "deleted set must survive savepoint rollback (cross-TX deletion tracking)"
        );
    }

    #[test]
    fn test_deleted_cleared_on_clear() {
        let mut store = new_store_with_deleted();
        store.mark_deleted(1);
        store.clear();
        assert!(!store.is_deleted(&1));
    }

    // ── Edge cases ──────────────────────────────────────────────────

    #[test]
    fn test_create_idempotent_snapshot() {
        let mut store = new_store();
        store.insert_created(1, "first".to_string());
        assert_eq!(store.snapshot_value(&1), Some(&None));
        // Second create with same key should not overwrite the None snapshot
        store.insert_created(1, "second".to_string());
        assert_eq!(store.snapshot_value(&1), Some(&None));
        assert_eq!(store.get(&1), Some(&"second".to_string()));
    }

    #[test]
    fn test_clear_resets_everything() {
        let mut store = new_store_with_deleted();
        store.insert_created(1, "a".to_string());
        store.mark_deleted(2);
        store.clear();
        assert!(!store.contains(&1));
        assert!(!store.is_tracked(&1));
        assert!(!store.is_created(&1));
        assert!(!store.is_deleted(&2));
        assert!(store.take_modified().is_empty());
    }

    #[test]
    fn test_rollback_after_commit_is_noop() {
        let mut store = new_store();
        store.insert_created(1, "hello".to_string());
        store.commit();
        // Rollback after commit: snapshots/created are empty, so nothing to restore
        store.rollback();
        // Entry should still be there (commit preserves entries, rollback has nothing to undo)
        assert!(store.contains(&1));
    }

    #[test]
    fn test_modified_dedup() {
        let mut store = new_store();
        store.entries_mut().insert(1, "a".to_string());
        store.ensure_snapshot(&1);
        store.get_mut_tracked(&1);
        store.get_mut_tracked(&1); // second call to same key
        let modified = store.take_modified();
        // Should only appear once
        assert_eq!(modified, vec![1]);
    }
}
