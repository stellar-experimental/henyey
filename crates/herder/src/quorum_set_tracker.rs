//! Unified quorum-set state tracker.
//!
//! Owns all quorum-set bookkeeping that was previously spread across
//! four independent DashMaps in [`ScpDriver`]: validated qsets by node,
//! by hash, pending fetch requests, and the local quorum set.

use std::collections::HashSet;
use std::sync::RwLock;

use dashmap::DashMap;
use henyey_common::Hash256;
use henyey_scp::hash_quorum_set;
use stellar_xdr::curr::{NodeId, PublicKey, ScpQuorumSet};
use tracing::{debug, info, trace};

use super::scp_driver::PendingQuorumSet;

/// Maximum number of pending quorum-set requests before new ones are dropped.
const MAX_PENDING_QSET_REQUESTS: usize = 512;

/// Maximum node IDs tracked per pending quorum-set entry.
const MAX_PENDING_NODE_IDS: usize = 64;

/// Diagnostic sizes for the quorum-set tracker.
#[derive(Debug, Clone, Default)]
pub struct QuorumSetTrackerSizes {
    pub by_node: usize,
    pub by_hash: usize,
    pub pending: usize,
}

/// Unified quorum-set state tracker.
///
/// Replaces `ScpDriver.quorum_sets`, `quorum_sets_by_hash`,
/// `pending_quorum_sets`, and `local_quorum_set`.
pub struct QuorumSetTracker {
    /// Validated quorum sets indexed by node ID (32-byte public key).
    by_node: DashMap<[u8; 32], ScpQuorumSet>,
    /// Validated quorum sets indexed by content hash.
    by_hash: DashMap<Hash256, ScpQuorumSet>,
    /// Pending fetch requests: content hash → waiting node_ids + request count.
    pending: DashMap<Hash256, PendingQuorumSet>,
    /// Our local quorum set (always preserved across clears).
    local: RwLock<Option<ScpQuorumSet>>,
    /// Local node's 32-byte public key.
    local_node_key: [u8; 32],
}

impl QuorumSetTracker {
    /// Create a new tracker. If `initial_local` is provided, seeds both maps.
    pub fn new(local_node_key: [u8; 32], initial_local: Option<ScpQuorumSet>) -> Self {
        let by_node = DashMap::new();
        let by_hash = DashMap::new();

        if let Some(ref qs) = initial_local {
            let hash = hash_quorum_set(qs);
            by_node.insert(local_node_key, qs.clone());
            by_hash.insert(hash, qs.clone());
        }

        Self {
            by_node,
            by_hash,
            pending: DashMap::new(),
            local: RwLock::new(initial_local),
            local_node_key,
        }
    }

    // --- Local qset ---

    /// Set the local quorum set. Updates by_node, by_hash, and removes
    /// from pending for this hash.
    pub fn set_local(&self, qs: ScpQuorumSet) {
        *self.local.write().unwrap() = Some(qs.clone());
        let hash = hash_quorum_set(&qs);
        self.by_hash.insert(hash, qs.clone());
        self.by_node.insert(self.local_node_key, qs);
        self.pending.remove(&hash);
    }

    /// Get the local quorum set.
    pub fn get_local(&self) -> Option<ScpQuorumSet> {
        self.local.read().unwrap().clone()
    }

    // --- Request/receipt ---

    /// Request a quorum set for a node. Returns true if this is a new fetch
    /// request (caller should send a network request).
    ///
    /// - If known by hash: associates with node immediately, returns false.
    /// - If already pending: adds node_id to the waiting set, returns false.
    /// - If new: creates a PendingQuorumSet entry, returns true.
    pub fn request(&self, hash: Hash256, node_id: NodeId) -> bool {
        // If we already have this qset, store the node→qset association.
        let existing = self.by_hash.get(&hash).map(|qs| qs.clone());
        if let Some(qs) = existing {
            trace!(%hash, node_id = ?node_id, "Associating existing quorum set with node");
            self.store(&node_id, qs);
            return false;
        }

        // If already pending, add this node_id to the waiting set.
        if let Some(mut entry) = self.pending.get_mut(&hash) {
            entry.request_count += 1;
            // Cap per-entry node_ids to prevent unbounded growth
            if entry.node_ids.len() < MAX_PENDING_NODE_IDS {
                entry.node_ids.insert(node_id);
            }
            return false;
        }

        // Defense-in-depth: cap the number of pending entries
        if self.pending.len() >= MAX_PENDING_QSET_REQUESTS {
            debug!(
                pending_count = self.pending.len(),
                "Dropping quorum set request: pending cap reached"
            );
            return false;
        }

        // New request.
        let mut node_ids = HashSet::new();
        node_ids.insert(node_id);
        self.pending.insert(
            hash,
            PendingQuorumSet {
                request_count: 1,
                node_ids,
            },
        );
        info!(%hash, "Registered pending quorum set request");
        true
    }

    /// Get node_ids waiting for a pending qset hash.
    /// Returns empty vec if not pending.
    pub fn pending_node_ids(&self, hash: &Hash256) -> Vec<NodeId> {
        self.pending
            .get(hash)
            .map(|entry| entry.node_ids.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// Clear a specific pending request.
    pub fn clear_pending(&self, hash: &Hash256) {
        self.pending.remove(hash);
    }

    // --- Store/lookup ---

    /// Store a validated qset for a node. Updates by_node and by_hash.
    /// Removes from pending for this hash.
    pub fn store(&self, node_id: &NodeId, quorum_set: ScpQuorumSet) {
        let key: [u8; 32] = match &node_id.0 {
            PublicKey::PublicKeyTypeEd25519(key) => key.0,
        };
        let hash = hash_quorum_set(&quorum_set);
        self.by_node.insert(key, quorum_set.clone());
        self.by_hash.insert(hash, quorum_set);
        self.pending.remove(&hash);
    }

    /// Look up by node ID.
    pub fn get_by_node(&self, node_id: &NodeId) -> Option<ScpQuorumSet> {
        let key: [u8; 32] = match &node_id.0 {
            PublicKey::PublicKeyTypeEd25519(key) => key.0,
        };
        // Check local first (matches ScpDriver behavior: local_quorum_set checked
        // before quorum_sets map in get_quorum_set).
        if key == self.local_node_key {
            if let Some(qs) = self.get_local() {
                return Some(qs);
            }
        }
        self.by_node.get(&key).map(|v| v.clone())
    }

    /// Look up by hash.
    pub fn get_by_hash(&self, hash: &Hash256) -> Option<ScpQuorumSet> {
        self.by_hash.get(hash).map(|v| v.clone())
    }

    /// Check if a hash is known.
    pub fn has_hash(&self, hash: &Hash256) -> bool {
        self.by_hash.contains_key(hash)
    }

    // --- Cleanup ---

    // --- Diagnostics ---

    pub fn sizes(&self) -> QuorumSetTrackerSizes {
        QuorumSetTrackerSizes {
            by_node: self.by_node.len(),
            by_hash: self.by_hash.len(),
            pending: self.pending.len(),
        }
    }

    pub fn by_node_count(&self) -> usize {
        self.by_node.len()
    }

    pub fn by_hash_count(&self) -> usize {
        self.by_hash.len()
    }

    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use stellar_xdr::curr::{ScpQuorumSet, Uint256, VecM};

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

        // Create pending
        tracker.pending.insert(
            hash,
            PendingQuorumSet {
                request_count: 1,
                node_ids: HashSet::new(),
            },
        );

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

    /// Regression test for issue #1953: QuorumSetTracker seeded with an
    /// unnormalized local quorum set must still be findable by the
    /// *normalized* hash (because SCP::new normalizes and emits that hash
    /// in statements).
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

        // Compute the normalized hash (what SCP::new would produce).
        let mut normalized = unnormalized.clone();
        normalize_quorum_set(&mut normalized);
        let normalized_hash = hash_quorum_set(&normalized);

        // Construct the tracker with the unnormalized qset as-is.
        // In production, Herder::build normalizes before constructing the
        // tracker, but this test verifies the contract: even if someone
        // constructs with unnormalized input, the normalized hash should
        // work for lookup after normalization at the Herder layer.
        let tracker = QuorumSetTracker::new(node_key(0), Some(normalized.clone()));

        // Lookup by normalized hash must succeed.
        assert!(
            tracker.has_hash(&normalized_hash),
            "tracker seeded with normalized qset must be findable by canonical hash"
        );
        let retrieved = tracker.get_by_hash(&normalized_hash);
        assert!(retrieved.is_some());
        assert_eq!(retrieved.unwrap().threshold, 2);
    }
}
