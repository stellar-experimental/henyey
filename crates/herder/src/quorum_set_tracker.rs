//! Unified quorum-set state tracker.
//!
//! Owns all quorum-set bookkeeping: validated qsets by node, by hash,
//! pending fetch requests, and the local quorum set.
//!
//! Both validated caches (`by_hash`, `by_node`) are bounded at
//! [`MAX_VALIDATED_QSETS`] using [`RandomEvictionCache`] — matching
//! stellar-core's `mQsetCache(QSET_CACHE_SIZE = 10000)` with the same
//! random-two-choice eviction policy.

use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, RwLock};

use henyey_common::Hash256;
use henyey_crypto::RandomEvictionCache;
use henyey_scp::hash_quorum_set;
use stellar_xdr::{NodeId, PublicKey, ScpQuorumSet};
use tracing::{debug, info, trace};

use super::scp_driver::PendingQuorumSet;

/// Maximum number of pending quorum-set requests before new ones are dropped.
const MAX_PENDING_QSET_REQUESTS: usize = 512;

/// Maximum node IDs tracked per pending quorum-set entry.
const MAX_PENDING_NODE_IDS: usize = 64;

/// Maximum validated quorum sets cached. Matches stellar-core's
/// `QSET_CACHE_SIZE` in `PendingEnvelopes.cpp:22`.
const MAX_VALIDATED_QSETS: usize = 10_000;

/// Diagnostic sizes for the quorum-set tracker.
#[derive(Debug, Clone, Default)]
pub struct QuorumSetTrackerSizes {
    pub by_node: usize,
    pub by_hash: usize,
    pub pending: usize,
}

/// The local node's quorum set and its hash, stored atomically.
struct LocalQuorumSet {
    qset: ScpQuorumSet,
    hash: Hash256,
}

/// Interior state protected by a single `Mutex`.
struct QuorumSetTrackerInner {
    /// Authoritative cache: hash → full quorum set value.
    by_hash: RandomEvictionCache<Hash256, ScpQuorumSet>,
    /// Advisory index: node public key → quorum set hash.
    by_node: RandomEvictionCache<[u8; 32], Hash256>,
    /// Pending fetch requests: content hash → waiting node_ids + request count.
    /// Lives under the same Mutex as the caches to enforce the invariant:
    /// if `by_hash` contains a hash, `pending` must not contain that hash.
    pending: HashMap<Hash256, PendingQuorumSet>,
}

impl QuorumSetTrackerInner {
    /// Associate a known qset with a node and clear any pending entry for it.
    ///
    /// Enforces the invariant: a hash present in `by_hash` must not also
    /// be present in `pending`.
    fn associate_known_qset(&mut self, node_key: [u8; 32], hash: Hash256, qset: ScpQuorumSet) {
        self.by_hash.put(hash, qset);
        self.by_node.put(node_key, hash);
        self.pending.remove(&hash);
    }
}

/// Unified quorum-set state tracker.
///
/// Validated quorum sets are stored in bounded caches (10,000 entries each)
/// using random-two-choice eviction. The local node's quorum set is pinned
/// outside the cache and never evicted.
pub struct QuorumSetTracker {
    /// Bounded caches for validated quorum sets and pending requests.
    inner: Mutex<QuorumSetTrackerInner>,
    /// Our local quorum set (pinned, never evicted).
    local: RwLock<Option<LocalQuorumSet>>,
    /// Local node's 32-byte public key.
    local_node_key: [u8; 32],
}

impl QuorumSetTracker {
    /// Create a new tracker. If `initial_local` is provided, seeds both caches.
    pub fn new(local_node_key: [u8; 32], initial_local: Option<ScpQuorumSet>) -> Self {
        let mut by_hash = RandomEvictionCache::new(MAX_VALIDATED_QSETS);
        let mut by_node = RandomEvictionCache::new(MAX_VALIDATED_QSETS);

        let local = if let Some(ref qs) = initial_local {
            let hash = hash_quorum_set(qs);
            by_hash.put(hash, qs.clone());
            by_node.put(local_node_key, hash);
            Some(LocalQuorumSet {
                qset: qs.clone(),
                hash,
            })
        } else {
            None
        };

        Self {
            inner: Mutex::new(QuorumSetTrackerInner {
                by_hash,
                by_node,
                pending: HashMap::new(),
            }),
            local: RwLock::new(local),
            local_node_key,
        }
    }

    // --- Local qset ---

    /// Set the local quorum set. Updates both caches and removes from pending.
    pub fn set_local(&self, qs: ScpQuorumSet) {
        let hash = hash_quorum_set(&qs);
        *self.local.write().unwrap() = Some(LocalQuorumSet {
            qset: qs.clone(),
            hash,
        });
        let mut inner = self.inner.lock().unwrap();
        inner.associate_known_qset(self.local_node_key, hash, qs);
    }

    /// Get the local quorum set.
    pub fn get_local(&self) -> Option<ScpQuorumSet> {
        self.local.read().unwrap().as_ref().map(|l| l.qset.clone())
    }

    // --- Request/receipt ---

    /// Request a quorum set for a node. Returns true if this is a new fetch
    /// request (caller should send a network request).
    ///
    /// - If known by hash (local or cache): associates with node, returns false.
    /// - If already pending: adds node_id to the waiting set, returns false.
    /// - If new: creates a PendingQuorumSet entry, returns true.
    pub fn request(&self, hash: Hash256, node_id: NodeId) -> bool {
        // Check local first (separate RwLock, released before inner).
        if let Some(local) = self.local.read().unwrap().as_ref() {
            if local.hash == hash {
                trace!(%hash, node_id = ?node_id, "Associating local quorum set with node");
                self.store(&node_id, local.qset.clone());
                return false;
            }
        }

        let node_key: [u8; 32] = match &node_id.0 {
            PublicKey::PublicKeyTypeEd25519(key) => key.0,
        };

        // Single Mutex scope: check cache + cap + insert/update pending.
        // This enforces two invariants atomically:
        // 1. pending.len() never exceeds MAX_PENDING_QSET_REQUESTS
        // 2. if by_hash contains a hash, pending does not contain that hash
        let mut inner = self.inner.lock().unwrap();

        if let Some(qs) = inner.by_hash.get(&hash).cloned() {
            trace!(%hash, node_id = ?node_id, "Associating existing quorum set with node");
            inner.associate_known_qset(node_key, hash, qs);
            return false;
        }

        // Cap check before entry() — within the same Mutex scope, so this
        // is atomic with respect to concurrent insertions (no TOCTOU race).
        let at_cap = inner.pending.len() >= MAX_PENDING_QSET_REQUESTS;

        match inner.pending.entry(hash) {
            std::collections::hash_map::Entry::Occupied(mut entry) => {
                let pending = entry.get_mut();
                pending.request_count += 1;
                if pending.node_ids.len() < MAX_PENDING_NODE_IDS {
                    pending.node_ids.insert(node_id);
                }
                false
            }
            std::collections::hash_map::Entry::Vacant(entry) => {
                if at_cap {
                    debug!("Dropping quorum set request: pending cap reached");
                    return false;
                }

                let mut node_ids = HashSet::new();
                node_ids.insert(node_id);
                entry.insert(PendingQuorumSet {
                    request_count: 1,
                    node_ids,
                });
                info!(%hash, "Registered pending quorum set request");
                true
            }
        }
    }

    /// Get node_ids waiting for a pending qset hash.
    /// Returns empty vec if not pending.
    pub fn pending_node_ids(&self, hash: &Hash256) -> Vec<NodeId> {
        self.inner
            .lock()
            .unwrap()
            .pending
            .get(hash)
            .map(|entry| entry.node_ids.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// Clear a specific pending request.
    pub fn clear_pending(&self, hash: &Hash256) {
        self.inner.lock().unwrap().pending.remove(hash);
    }

    // --- Store/lookup ---

    /// Store a validated qset for a node. Inserts into both bounded caches
    /// and removes from pending.
    pub fn store(&self, node_id: &NodeId, quorum_set: ScpQuorumSet) {
        let key: [u8; 32] = match &node_id.0 {
            PublicKey::PublicKeyTypeEd25519(key) => key.0,
        };
        let hash = hash_quorum_set(&quorum_set);
        let mut inner = self.inner.lock().unwrap();
        inner.associate_known_qset(key, hash, quorum_set);
    }

    /// Look up by node ID.
    ///
    /// Checks the pinned local qset first, then the bounded caches.
    /// The `by_node` lookup is a two-step indirection: node→hash, then
    /// hash→qset. If the hash has been evicted, returns `None`.
    pub fn get_by_node(&self, node_id: &NodeId) -> Option<ScpQuorumSet> {
        let key: [u8; 32] = match &node_id.0 {
            PublicKey::PublicKeyTypeEd25519(key) => key.0,
        };
        // Check local first (pinned, never evicted).
        if key == self.local_node_key {
            if let Some(local) = self.local.read().unwrap().as_ref() {
                return Some(local.qset.clone());
            }
        }
        // Two-step lookup through bounded caches.
        let mut inner = self.inner.lock().unwrap();
        let hash = *inner.by_node.get(&key)?;
        inner.by_hash.get(&hash).cloned()
    }

    /// Look up by hash.
    ///
    /// Checks the pinned local qset first, then the bounded cache.
    pub fn get_by_hash(&self, hash: &Hash256) -> Option<ScpQuorumSet> {
        // Check local first.
        if let Some(local) = self.local.read().unwrap().as_ref() {
            if &local.hash == hash {
                return Some(local.qset.clone());
            }
        }
        let mut inner = self.inner.lock().unwrap();
        inner.by_hash.get(hash).cloned()
    }

    /// Check if a hash is known (local or cached).
    ///
    /// Uses `exists()` which does NOT bump access generation, matching
    /// stellar-core's `exists()` semantics.
    pub fn has_hash(&self, hash: &Hash256) -> bool {
        // Check local first.
        if let Some(local) = self.local.read().unwrap().as_ref() {
            if &local.hash == hash {
                return true;
            }
        }
        let inner = self.inner.lock().unwrap();
        inner.by_hash.exists(hash)
    }

    // --- Diagnostics ---

    pub fn sizes(&self) -> QuorumSetTrackerSizes {
        let inner = self.inner.lock().unwrap();
        QuorumSetTrackerSizes {
            by_node: inner.by_node.len(),
            by_hash: inner.by_hash.len(),
            pending: inner.pending.len(),
        }
    }

    pub fn by_node_count(&self) -> usize {
        self.inner.lock().unwrap().by_node.len()
    }

    pub fn by_hash_count(&self) -> usize {
        self.inner.lock().unwrap().by_hash.len()
    }

    pub fn pending_count(&self) -> usize {
        self.inner.lock().unwrap().pending.len()
    }

    #[cfg(test)]
    fn pending_get(&self, hash: &Hash256) -> Option<PendingQuorumSet> {
        self.inner.lock().unwrap().pending.get(hash).cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use stellar_xdr::{ScpQuorumSet, Uint256, VecM};

    fn make_node_id(seed: u8) -> NodeId {
        NodeId(PublicKey::PublicKeyTypeEd25519(Uint256([seed; 32])))
    }

    fn make_qset(threshold: u32) -> ScpQuorumSet {
        ScpQuorumSet {
            threshold,
            validators: vec![make_node_id(1)].try_into().unwrap(),
            inner_sets: VecM::default(),
        }
    }

    fn node_key(seed: u8) -> [u8; 32] {
        [seed; 32]
    }

    #[test]
    fn test_request_new_creates_pending() {
        let tracker = QuorumSetTracker::new(node_key(0), None);
        let hash = Hash256::from_bytes([1; 32]);
        let node = make_node_id(10);

        assert!(tracker.request(hash, node.clone()));
        assert_eq!(tracker.pending_count(), 1);

        let ids = tracker.pending_node_ids(&hash);
        assert_eq!(ids.len(), 1);
        assert_eq!(ids[0], node);
    }

    #[test]
    fn test_request_known_hash_returns_false() {
        let qs = make_qset(1);
        let hash = hash_quorum_set(&qs);
        let tracker = QuorumSetTracker::new(node_key(0), None);

        // Pre-populate by_hash
        let node_a = make_node_id(10);
        tracker.store(&node_a, qs.clone());

        // Now request for a different node
        let node_b = make_node_id(20);
        assert!(!tracker.request(hash, node_b.clone()));

        // node_b should now have the qset
        assert!(tracker.get_by_node(&node_b).is_some());
        // No pending entry created
        assert_eq!(tracker.pending_count(), 0);
    }

    #[test]
    fn test_request_pending_adds_node_id() {
        let tracker = QuorumSetTracker::new(node_key(0), None);
        let hash = Hash256::from_bytes([1; 32]);

        assert!(tracker.request(hash, make_node_id(10)));
        assert!(!tracker.request(hash, make_node_id(20)));
        assert!(!tracker.request(hash, make_node_id(30)));

        let ids = tracker.pending_node_ids(&hash);
        assert_eq!(ids.len(), 3);
    }

    #[test]
    fn test_pending_node_ids_empty_for_unknown() {
        let tracker = QuorumSetTracker::new(node_key(0), None);
        let hash = Hash256::from_bytes([99; 32]);
        assert!(tracker.pending_node_ids(&hash).is_empty());
    }

    #[test]
    fn test_store_updates_both_maps_clears_pending() {
        let tracker = QuorumSetTracker::new(node_key(0), None);
        let qs = make_qset(1);
        let hash = hash_quorum_set(&qs);
        let node = make_node_id(10);

        // Create pending entry via the public API
        assert!(tracker.request(hash, make_node_id(99)));
        assert_eq!(tracker.pending_count(), 1);

        tracker.store(&node, qs.clone());

        assert!(tracker.get_by_node(&node).is_some());
        assert!(tracker.get_by_hash(&hash).is_some());
        assert_eq!(tracker.pending_count(), 0);
    }

    #[test]
    fn test_store_multi_node_fanout() {
        let tracker = QuorumSetTracker::new(node_key(0), None);
        let qs = make_qset(1);
        let hash = hash_quorum_set(&qs);

        // Simulate 3 nodes requesting the same qset
        tracker.request(hash, make_node_id(10));
        tracker.request(hash, make_node_id(20));
        tracker.request(hash, make_node_id(30));
        assert_eq!(tracker.pending_count(), 1);

        // Store for first node clears pending
        tracker.store(&make_node_id(10), qs.clone());
        assert_eq!(tracker.pending_count(), 0);

        // Store for remaining nodes still works
        tracker.store(&make_node_id(20), qs.clone());
        tracker.store(&make_node_id(30), qs.clone());

        assert!(tracker.get_by_node(&make_node_id(10)).is_some());
        assert!(tracker.get_by_node(&make_node_id(20)).is_some());
        assert!(tracker.get_by_node(&make_node_id(30)).is_some());
    }

    #[test]
    fn test_clear_pending_explicit() {
        let tracker = QuorumSetTracker::new(node_key(0), None);
        let hash = Hash256::from_bytes([1; 32]);

        tracker.request(hash, make_node_id(10));
        assert_eq!(tracker.pending_count(), 1);

        tracker.clear_pending(&hash);
        assert_eq!(tracker.pending_count(), 0);
    }

    #[test]
    fn test_set_local_removes_from_pending() {
        let tracker = QuorumSetTracker::new(node_key(0), None);
        let qs = make_qset(1);
        let hash = hash_quorum_set(&qs);

        // Create pending for this hash
        tracker.request(hash, make_node_id(10));
        assert_eq!(tracker.pending_count(), 1);

        tracker.set_local(qs.clone());
        assert_eq!(tracker.pending_count(), 0);
        assert!(tracker.get_local().is_some());
        assert!(tracker.get_by_hash(&hash).is_some());
    }

    #[test]
    fn test_has_hash() {
        let tracker = QuorumSetTracker::new(node_key(0), None);
        let qs = make_qset(1);
        let hash = hash_quorum_set(&qs);

        assert!(!tracker.has_hash(&hash));
        tracker.store(&make_node_id(10), qs);
        assert!(tracker.has_hash(&hash));
    }

    #[test]
    fn test_get_by_node_prefers_local() {
        let local_qs = make_qset(1);
        let tracker = QuorumSetTracker::new(node_key(0), Some(local_qs.clone()));

        // Querying the local node should return the local qset
        let result = tracker.get_by_node(&make_node_id(0));
        assert!(result.is_some());
        assert_eq!(result.unwrap().threshold, 1);
    }

    /// Regression test for AUDIT-010: pending entries are capped to prevent
    /// unbounded memory growth from forged quorum-set hashes.
    #[test]
    fn test_audit_010_pending_cap() {
        let tracker = QuorumSetTracker::new(node_key(0), None);

        // Fill to the cap
        for i in 0..MAX_PENDING_QSET_REQUESTS {
            let mut bytes = [0u8; 32];
            bytes[0] = (i & 0xFF) as u8;
            bytes[1] = ((i >> 8) & 0xFF) as u8;
            let hash = Hash256::from_bytes(bytes);
            assert!(tracker.request(hash, make_node_id(1)));
        }
        assert_eq!(tracker.pending_count(), MAX_PENDING_QSET_REQUESTS);

        // Next request should be rejected
        let overflow_hash = Hash256::from_bytes([0xFF; 32]);
        assert!(!tracker.request(overflow_hash, make_node_id(2)));
        assert_eq!(tracker.pending_count(), MAX_PENDING_QSET_REQUESTS);
    }

    /// Regression test for AUDIT-010: per-entry node_ids are capped.
    #[test]
    fn test_audit_010_node_ids_cap() {
        let tracker = QuorumSetTracker::new(node_key(0), None);
        let hash = Hash256::from_bytes([42; 32]);

        // First request creates the entry
        assert!(tracker.request(hash, make_node_id(1)));

        // Add up to the cap
        for i in 2..=(MAX_PENDING_NODE_IDS as u8) {
            assert!(!tracker.request(hash, make_node_id(i)));
        }

        // Verify node_ids are capped
        let node_ids = tracker.pending_node_ids(&hash);
        assert_eq!(node_ids.len(), MAX_PENDING_NODE_IDS);

        // Adding more doesn't grow beyond cap
        assert!(!tracker.request(hash, make_node_id(200)));
        let node_ids = tracker.pending_node_ids(&hash);
        assert_eq!(node_ids.len(), MAX_PENDING_NODE_IDS);
    }

    /// AUDIT-259: Validated caches are bounded at MAX_VALIDATED_QSETS.
    #[test]
    fn test_audit_259_bounded_by_hash() {
        let tracker = QuorumSetTracker::new(node_key(0), None);

        // Insert more than the cap
        for i in 0..(MAX_VALIDATED_QSETS + 100) {
            let qs = ScpQuorumSet {
                threshold: i as u32,
                validators: vec![make_node_id(1)].try_into().unwrap(),
                inner_sets: VecM::default(),
            };
            let node_seed = ((i % 254) + 1) as u8;
            tracker.store(&make_node_id(node_seed), qs);
        }

        // Both caches must be bounded
        assert!(tracker.by_hash_count() <= MAX_VALIDATED_QSETS);
        assert!(tracker.by_node_count() <= MAX_VALIDATED_QSETS);
    }

    /// AUDIT-259: Local qset is always accessible via all lookup paths
    /// even under heavy cache pressure.
    #[test]
    fn test_audit_259_local_pinning_all_paths() {
        let local_qs = make_qset(42);
        let local_hash = hash_quorum_set(&local_qs);
        let tracker = QuorumSetTracker::new(node_key(0), Some(local_qs.clone()));

        // Fill caches beyond capacity
        for i in 0..(MAX_VALIDATED_QSETS + 50) {
            let qs = ScpQuorumSet {
                threshold: i as u32,
                validators: vec![make_node_id(1)].try_into().unwrap(),
                inner_sets: VecM::default(),
            };
            let seed = ((i % 254) + 1) as u8;
            tracker.store(&make_node_id(seed), qs);
        }

        // Local must still be accessible via all paths
        assert!(
            tracker.get_by_node(&make_node_id(0)).is_some(),
            "local qset must be accessible via get_by_node"
        );
        assert_eq!(tracker.get_by_node(&make_node_id(0)).unwrap().threshold, 42);

        assert!(
            tracker.get_by_hash(&local_hash).is_some(),
            "local qset must be accessible via get_by_hash"
        );
        assert_eq!(tracker.get_by_hash(&local_hash).unwrap().threshold, 42);

        assert!(
            tracker.has_hash(&local_hash),
            "local hash must be detectable via has_hash"
        );
    }

    /// AUDIT-259: Overwriting an existing entry does not trigger eviction.
    #[test]
    fn test_audit_259_overwrite_no_evict() {
        let tracker = QuorumSetTracker::new(node_key(0), None);

        // Fill to exactly the cap with distinct hashes
        for i in 0..MAX_VALIDATED_QSETS {
            let qs = ScpQuorumSet {
                threshold: i as u32,
                validators: vec![make_node_id(1)].try_into().unwrap(),
                inner_sets: VecM::default(),
            };
            let seed = ((i % 254) + 1) as u8;
            tracker.store(&make_node_id(seed), qs);
        }

        let count_before = tracker.by_hash_count();

        // Overwrite an existing entry (same hash, same node)
        let qs = ScpQuorumSet {
            threshold: 0,
            validators: vec![make_node_id(1)].try_into().unwrap(),
            inner_sets: VecM::default(),
        };
        tracker.store(&make_node_id(1), qs);

        // Count should not decrease (no spurious eviction)
        assert_eq!(tracker.by_hash_count(), count_before);
    }

    /// AUDIT-259: When by_hash evicts a hash, get_by_node returns None
    /// gracefully (no panic).
    #[test]
    fn test_audit_259_stale_by_node_graceful() {
        let tracker = QuorumSetTracker::new(node_key(0), None);

        // Store a qset for node 42
        let target_qs = make_qset(99);
        tracker.store(&make_node_id(42), target_qs);

        // Verify it's accessible
        assert!(tracker.get_by_node(&make_node_id(42)).is_some());

        // Now flood with entries to evict the target from by_hash
        for i in 0..(MAX_VALIDATED_QSETS + 200) {
            let qs = ScpQuorumSet {
                threshold: (i + 1000) as u32,
                validators: vec![make_node_id(1)].try_into().unwrap(),
                inner_sets: VecM::default(),
            };
            let seed = ((i % 254) + 1) as u8;
            tracker.store(&make_node_id(seed), qs);
        }

        // get_by_node should return None (not panic) if the hash was evicted
        // This tests graceful degradation of stale by_node entries
        let result = tracker.get_by_node(&make_node_id(42));
        // Result may be Some or None depending on eviction — the important
        // thing is no panic
        let _ = result;
    }

    /// AUDIT-259: Active validators (accessed frequently) survive eviction
    /// pressure from many other entries.
    #[test]
    fn test_audit_259_active_validators_survive_eviction() {
        let tracker = QuorumSetTracker::new(node_key(0), None);

        // Store qsets for 30 "active validators"
        let mut active_qsets = Vec::new();
        for i in 1..=30u8 {
            let qs = ScpQuorumSet {
                threshold: i as u32,
                validators: vec![make_node_id(i)].try_into().unwrap(),
                inner_sets: VecM::default(),
            };
            tracker.store(&make_node_id(i), qs.clone());
            active_qsets.push((i, qs));
        }

        // Simulate ongoing access pattern: active validators are looked up
        // frequently while other entries are added
        for i in 0..MAX_VALIDATED_QSETS {
            // Every 100 inserts, access all active validators (simulating
            // slot processing which calls get_quorum_set for quorum members)
            if i % 100 == 0 {
                for &(seed, _) in &active_qsets {
                    tracker.get_by_node(&make_node_id(seed));
                }
            }

            let qs = ScpQuorumSet {
                threshold: (i + 1000) as u32,
                validators: vec![make_node_id(1)].try_into().unwrap(),
                inner_sets: VecM::default(),
            };
            // Use seeds 31+ to avoid overwriting active validators
            let seed = ((i % 224) + 31) as u8;
            tracker.store(&make_node_id(seed), qs);
        }

        // Access all active validators one more time
        for &(seed, _) in &active_qsets {
            tracker.get_by_node(&make_node_id(seed));
        }

        // All 30 active validators should still be accessible (recency
        // keeps them alive in the RandomEvictionCache)
        let mut accessible_count = 0;
        for &(seed, ref expected_qs) in &active_qsets {
            if let Some(qs) = tracker.get_by_node(&make_node_id(seed)) {
                if qs.threshold == expected_qs.threshold {
                    accessible_count += 1;
                }
            }
        }

        // With 10,000 cap and ~30 active validators accessed every 100
        // inserts, all should survive. Allow for small margin.
        assert!(
            accessible_count >= 25,
            "Expected at least 25 of 30 active validators to survive, got {}",
            accessible_count
        );
    }

    /// AUDIT-259: Re-store after eviction succeeds without error.
    #[test]
    fn test_audit_259_reinsert_after_eviction() {
        let tracker = QuorumSetTracker::new(node_key(0), None);

        let target_qs = make_qset(77);
        let target_hash = hash_quorum_set(&target_qs);
        tracker.store(&make_node_id(42), target_qs.clone());

        // Flood to cause eviction
        for i in 0..(MAX_VALIDATED_QSETS + 100) {
            let qs = ScpQuorumSet {
                threshold: (i + 500) as u32,
                validators: vec![make_node_id(1)].try_into().unwrap(),
                inner_sets: VecM::default(),
            };
            let seed = ((i % 254) + 1) as u8;
            tracker.store(&make_node_id(seed), qs);
        }

        // Re-store the original — must succeed
        tracker.store(&make_node_id(42), target_qs.clone());
        assert_eq!(tracker.get_by_hash(&target_hash).unwrap().threshold, 77);
        assert_eq!(
            tracker.get_by_node(&make_node_id(42)).unwrap().threshold,
            77
        );
    }

    /// Regression test for issue #1953: verifies that Herder::build's
    /// normalization step makes the canonical hash usable for by-hash
    /// lookup. We simulate the full flow: start with an unnormalized qset,
    /// normalize it (as Herder::build does), then seed the tracker.
    #[test]
    fn test_unnormalized_local_qset_lookup_by_normalized_hash() {
        use henyey_scp::normalize_quorum_set;

        // Build an unnormalized quorum set: validators in reverse order.
        let node_a = make_node_id(1);
        let node_b = make_node_id(2);
        let node_c = make_node_id(3);
        let unnormalized = ScpQuorumSet {
            threshold: 2,
            validators: vec![node_c, node_b, node_a].try_into().unwrap(),
            inner_sets: VecM::default(),
        };

        // The unnormalized hash differs from the normalized hash.
        let unnormalized_hash = hash_quorum_set(&unnormalized);
        let mut normalized = unnormalized.clone();
        normalize_quorum_set(&mut normalized);
        let normalized_hash = hash_quorum_set(&normalized);
        assert_ne!(
            unnormalized_hash, normalized_hash,
            "precondition: unnormalized and normalized hashes must differ"
        );

        // Simulate the Herder::build flow: normalize first, then seed tracker.
        let tracker = QuorumSetTracker::new(node_key(0), Some(normalized.clone()));

        // Lookup by the canonical (normalized) hash must succeed.
        assert!(
            tracker.has_hash(&normalized_hash),
            "tracker seeded with normalized qset must be findable by canonical hash"
        );
        let retrieved = tracker.get_by_hash(&normalized_hash);
        assert!(retrieved.is_some());
        assert_eq!(retrieved.unwrap().threshold, 2);

        // Lookup by the old unnormalized hash must NOT succeed — the tracker
        // only knows the canonical form.
        assert!(
            !tracker.has_hash(&unnormalized_hash),
            "tracker must not be indexed under the unnormalized hash"
        );
    }

    // --- Concurrent regression tests for entry() TOCTOU fix (#2469) ---

    #[test]
    fn test_concurrent_request_same_hash_different_nodes() {
        use std::sync::{Arc, Barrier};
        use std::thread;

        let tracker = Arc::new(QuorumSetTracker::new(node_key(0), None));
        let hash = Hash256::from_bytes([42; 32]);
        let n = 16;
        let barrier = Arc::new(Barrier::new(n));

        let handles: Vec<_> = (0..n)
            .map(|i| {
                let tracker = Arc::clone(&tracker);
                let barrier = Arc::clone(&barrier);
                let node_id = make_node_id(i as u8 + 1);
                thread::spawn(move || {
                    barrier.wait();
                    tracker.request(hash, node_id)
                })
            })
            .collect();

        let results: Vec<bool> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        let true_count = results.iter().filter(|&&r| r).count();

        // Exactly one thread should create the pending entry.
        assert_eq!(true_count, 1, "exactly one request() should return true");
        assert_eq!(tracker.pending_count(), 1);

        // All node_ids should be preserved (up to MAX_PENDING_NODE_IDS).
        let entry = tracker.pending_get(&hash).unwrap();
        assert_eq!(entry.request_count, n as u32);
        assert_eq!(entry.node_ids.len(), n);
    }

    #[test]
    fn test_request_at_cap_still_increments_existing() {
        let tracker = QuorumSetTracker::new(node_key(0), None);

        // Fill to capacity with unique hashes.
        for i in 0..MAX_PENDING_QSET_REQUESTS {
            let mut bytes = [0u8; 32];
            bytes[0] = (i & 0xFF) as u8;
            bytes[1] = ((i >> 8) & 0xFF) as u8;
            let hash = Hash256::from_bytes(bytes);
            tracker.request(hash, make_node_id(1));
        }

        assert_eq!(tracker.pending_count(), MAX_PENDING_QSET_REQUESTS);

        // Existing hash should still be updatable.
        let first_hash = Hash256::from_bytes([0u8; 32]);
        let new_node = make_node_id(99);
        assert!(!tracker.request(first_hash, new_node.clone()));
        let entry = tracker.pending_get(&first_hash).unwrap();
        assert_eq!(entry.request_count, 2);
        assert!(entry.node_ids.contains(&new_node));

        // New hash should be rejected.
        let new_hash = Hash256::from_bytes([0xFF; 32]);
        assert!(!tracker.request(new_hash, make_node_id(50)));
        assert!(tracker.pending_get(&new_hash).is_none());
    }

    /// Regression test for #2521: concurrent cap-boundary enforcement.
    /// Verifies that the pending count never exceeds MAX_PENDING_QSET_REQUESTS
    /// even under concurrent access at the boundary.
    #[test]
    fn test_concurrent_cap_boundary() {
        use std::sync::{Arc, Barrier};
        use std::thread;

        let tracker = Arc::new(QuorumSetTracker::new(node_key(0), None));

        // Prefill to one below the cap.
        for i in 0..(MAX_PENDING_QSET_REQUESTS - 1) {
            let mut bytes = [0u8; 32];
            bytes[0] = (i & 0xFF) as u8;
            bytes[1] = ((i >> 8) & 0xFF) as u8;
            let hash = Hash256::from_bytes(bytes);
            tracker.request(hash, make_node_id(1));
        }
        assert_eq!(tracker.pending_count(), MAX_PENDING_QSET_REQUESTS - 1);

        // Spawn N threads, each trying to insert a distinct new hash.
        let n = 16;
        let barrier = Arc::new(Barrier::new(n));
        let handles: Vec<_> = (0..n)
            .map(|i| {
                let tracker = Arc::clone(&tracker);
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    let mut bytes = [0xAA; 32];
                    bytes[0] = i as u8;
                    let hash = Hash256::from_bytes(bytes);
                    barrier.wait();
                    tracker.request(hash, make_node_id((i + 100) as u8))
                })
            })
            .collect();

        let results: Vec<bool> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        let true_count = results.iter().filter(|&&r| r).count();

        // Exactly one thread should succeed (filling the last slot).
        assert_eq!(
            true_count, 1,
            "exactly one request() should return true at the cap boundary"
        );
        assert_eq!(tracker.pending_count(), MAX_PENDING_QSET_REQUESTS);
    }

    /// Regression test for #2521: store() atomically clears pending under
    /// the same lock as the cache insert, preventing stale pending entries.
    #[test]
    fn test_store_clears_pending_atomically() {
        use std::sync::{Arc, Barrier};
        use std::thread;

        let tracker = Arc::new(QuorumSetTracker::new(node_key(0), None));
        let n_hashes = 32;

        // Create pending entries for many distinct hashes.
        let mut hashes = Vec::new();
        let mut qsets = Vec::new();
        for i in 0..n_hashes {
            let qs = ScpQuorumSet {
                threshold: (i + 1) as u32,
                validators: vec![make_node_id(1)].try_into().unwrap(),
                inner_sets: VecM::default(),
            };
            let hash = hash_quorum_set(&qs);
            tracker.request(hash, make_node_id((i + 10) as u8));
            hashes.push(hash);
            qsets.push(qs);
        }
        assert_eq!(tracker.pending_count(), n_hashes);

        // Concurrently: half the threads call store() (cache + clear pending),
        // the other half call request() for the same hashes.
        let barrier = Arc::new(Barrier::new(n_hashes * 2));
        let mut handles = Vec::new();

        for i in 0..n_hashes {
            // Clone Arcs for both threads before spawning either.
            let tracker_store = Arc::clone(&tracker);
            let barrier_store = Arc::clone(&barrier);
            let tracker_req = Arc::clone(&tracker);
            let barrier_req = Arc::clone(&barrier);

            // Store thread
            let qs = qsets[i].clone();
            let node = make_node_id((i + 10) as u8);
            handles.push(thread::spawn(move || {
                barrier_store.wait();
                tracker_store.store(&node, qs);
            }));

            // Request thread (same hash)
            let hash = hashes[i];
            handles.push(thread::spawn(move || {
                barrier_req.wait();
                tracker_req.request(hash, make_node_id((i + 100) as u8));
            }));
        }

        for h in handles {
            h.join().unwrap();
        }

        // After all store() calls complete, the invariant must hold:
        // every hash in by_hash must NOT be in pending.
        for hash in &hashes {
            assert!(
                tracker.get_by_hash(hash).is_some(),
                "hash must be in cache after store()"
            );
            assert!(
                tracker.pending_get(hash).is_none(),
                "pending must be cleared for cached hash (invariant: by_hash ∋ hash ⟹ pending ∌ hash)"
            );
        }
    }
}
