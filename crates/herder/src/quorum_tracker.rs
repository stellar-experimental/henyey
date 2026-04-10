//! Quorum tracking utilities for consensus participation monitoring.
//!
//! This module provides two complementary quorum tracking mechanisms used by the
//! Herder to monitor network participation and security.
//!
//! # Background
//!
//! In SCP (Stellar Consensus Protocol), quorum sets define which nodes a validator
//! trusts. A node achieves consensus when it hears from a "quorum" - a set of nodes
//! that satisfies the threshold requirements of its quorum set configuration.
//!
//! # Trackers
//!
//! ## [`SlotQuorumTracker`]
//!
//! Tracks which nodes have sent SCP messages for each slot and determines whether
//! the node has "heard from quorum" or has a "v-blocking set" for a given slot.
//!
//! **Use cases:**
//! - Consensus timing decisions (e.g., when to bump ballot counters)
//! - Determining when enough validators have participated in a slot
//! - V-blocking detection (can prevent consensus if they disagree)
//!
//! ## [`QuorumTracker`]
//!
//! Tracks the transitive quorum set - all nodes reachable through the quorum graph
//! starting from the local node. Uses BFS to explore quorum relationships and
//! maintains distance information.
//!
//! **Use cases:**
//! - **Security validation**: Rejecting EXTERNALIZE messages from nodes not in our
//!   transitive quorum (prevents fast-forward attacks)
//! - **Closest validator tracking**: Identifying which direct validators are on the
//!   shortest path to any given transitive quorum member
//!
//! # Example
//!
//! ```ignore
//! use henyey_herder::quorum_tracker::{SlotQuorumTracker, QuorumTracker};
//!
//! // Track per-slot participation
//! let mut slot_tracker = SlotQuorumTracker::new(Some(quorum_set), 12);
//! slot_tracker.record_envelope(100, node_a);
//! slot_tracker.record_envelope(100, node_b);
//!
//! if slot_tracker.has_quorum(100, |n| get_quorum_set(n)) {
//!     println!("Heard from quorum for slot 100");
//! }
//!
//! // Track transitive quorum membership
//! let mut quorum_tracker = QuorumTracker::new(local_node_id);
//! quorum_tracker.expand(&local_node_id, local_quorum_set);
//!
//! if quorum_tracker.is_node_definitely_in_quorum(&some_node) {
//!     // Accept messages from this node
//! }
//! ```

use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};

use henyey_scp::{is_quorum, is_v_blocking, SlotIndex};
use stellar_xdr::curr::{NodeId, ScpQuorumSet};

/// Tracks quorum participation over recent slots.
///
/// This tracker monitors which nodes have sent SCP messages for each slot,
/// enabling "heard from quorum" and "v-blocking" checks that drive consensus
/// timing decisions.
///
/// The tracker automatically prunes old slots to bound memory usage.
#[derive(Debug, Clone)]
pub struct SlotQuorumTracker {
    /// The local node's quorum set configuration.
    local_quorum_set: Option<ScpQuorumSet>,
    /// Maximum number of slots to track before pruning oldest.
    max_slots: usize,
    /// Map from slot index to the set of nodes heard from for that slot.
    slot_nodes: HashMap<SlotIndex, HashSet<NodeId>>,
}

impl SlotQuorumTracker {
    /// Create a new tracker.
    pub fn new(local_quorum_set: Option<ScpQuorumSet>, max_slots: usize) -> Self {
        Self {
            local_quorum_set,
            max_slots,
            slot_nodes: HashMap::new(),
        }
    }

    /// Update the local quorum set.
    pub fn set_local_quorum_set(&mut self, quorum_set: Option<ScpQuorumSet>) {
        self.local_quorum_set = quorum_set;
    }

    /// Record that we've heard from a node for a slot.
    pub fn record_envelope(&mut self, slot: SlotIndex, node_id: NodeId) {
        self.slot_nodes.entry(slot).or_default().insert(node_id);
        self.prune();
    }

    /// Remove entries for a slot.
    pub fn clear_slot(&mut self, slot: SlotIndex) {
        self.slot_nodes.remove(&slot);
    }

    /// Remove entries for all slots below a given slot.
    pub fn clear_slots_below(&mut self, min_slot: SlotIndex) {
        self.slot_nodes.retain(|&slot, _| slot >= min_slot);
    }

    /// Check if we have a quorum for a slot.
    pub fn has_quorum<F>(&self, slot: SlotIndex, get_qs: F) -> bool
    where
        F: Fn(&NodeId) -> Option<ScpQuorumSet>,
    {
        let local = match &self.local_quorum_set {
            Some(qs) => qs,
            None => return false,
        };
        let nodes = match self.slot_nodes.get(&slot) {
            Some(nodes) => nodes,
            None => return false,
        };
        is_quorum(local, nodes, get_qs)
    }

    /// Check if we have a v-blocking set for a slot.
    pub fn is_v_blocking(&self, slot: SlotIndex) -> bool {
        let local = match &self.local_quorum_set {
            Some(qs) => qs,
            None => return false,
        };
        let nodes = match self.slot_nodes.get(&slot) {
            Some(nodes) => nodes,
            None => return false,
        };
        is_v_blocking(local, nodes)
    }

    /// Get all slots that have achieved v-blocking status, sorted descending by slot.
    ///
    /// This is used for out-of-sync recovery to find slots to purge.
    pub fn get_v_blocking_slots(&self) -> Vec<SlotIndex> {
        let local = match &self.local_quorum_set {
            Some(qs) => qs,
            None => return vec![],
        };

        let mut v_blocking_slots: Vec<SlotIndex> = self
            .slot_nodes
            .iter()
            .filter_map(|(slot, nodes)| {
                if is_v_blocking(local, nodes) {
                    Some(*slot)
                } else {
                    None
                }
            })
            .collect();

        // Sort descending (highest slot first)
        v_blocking_slots.sort_unstable_by(|a, b| b.cmp(a));
        v_blocking_slots
    }

    fn prune(&mut self) {
        if self.max_slots == 0 || self.slot_nodes.len() <= self.max_slots {
            return;
        }

        let mut slots: Vec<_> = self.slot_nodes.keys().copied().collect();
        slots.sort_unstable();
        let remove_count = self.slot_nodes.len().saturating_sub(self.max_slots);
        for slot in slots.into_iter().take(remove_count) {
            self.slot_nodes.remove(&slot);
        }
    }
}

/// Metadata about a node in the transitive quorum graph.
///
/// Stores information used for quorum security checks and path analysis.
#[derive(Debug, Clone)]
pub struct NodeInfo {
    /// The node's quorum set, if known.
    pub quorum_set: Option<ScpQuorumSet>,
    /// Distance from the local node in the quorum graph (0 = local node).
    pub distance: usize,
    /// The set of direct validators (distance 1) on the shortest path to this node.
    /// Used to identify which direct connections are important for reaching this node.
    pub closest_validators: BTreeSet<NodeId>,
}

/// Errors returned by the transitive quorum tracker.
#[derive(Debug, thiserror::Error)]
pub enum QuorumTrackerError {
    #[error("node missing from quorum map during rebuild")]
    MissingNode,
    #[error("quorum expansion failed during rebuild")]
    ExpandFailed,
}

/// Errors returned by [`TransitiveQuorumTracker::expand`].
#[derive(Debug, thiserror::Error)]
pub enum ExpandError {
    #[error("node not found in quorum map")]
    NodeNotFound,
    #[error("node already has a different quorum set")]
    AlreadyExpanded,
}

/// Tracks the transitive quorum set and path information.
///
/// The transitive quorum set includes all nodes reachable through the quorum
/// graph starting from the local node. This tracker builds and maintains this
/// set incrementally as quorum set information is learned from the network.
///
/// # Security
///
/// The primary security use is validating EXTERNALIZE messages. A node should
/// only accept EXTERNALIZE messages from nodes in its transitive quorum set,
/// preventing attackers outside the trust network from fast-forwarding the node
/// to arbitrary slots.
///
/// # Algorithm
///
/// The tracker uses BFS-style expansion:
/// 1. Start with the local node at distance 0
/// 2. When a node's quorum set is learned, add all its members at distance + 1
/// 3. Track which direct validators (distance 1) are on the path to each node
#[derive(Debug, Clone)]
pub struct QuorumTracker {
    /// The local node's ID.
    local_node_id: NodeId,
    /// Map from node ID to its quorum metadata.
    quorum: HashMap<NodeId, NodeInfo>,
}

impl QuorumTracker {
    /// Create a new tracker for the local node.
    pub fn new(local_node_id: NodeId) -> Self {
        let mut quorum = HashMap::new();
        quorum.insert(
            local_node_id.clone(),
            NodeInfo {
                quorum_set: None,
                distance: 0,
                closest_validators: BTreeSet::new(),
            },
        );
        Self {
            local_node_id,
            quorum,
        }
    }

    /// Returns true if the node is definitely in the transitive quorum.
    pub fn is_node_definitely_in_quorum(&self, node_id: &NodeId) -> bool {
        self.quorum.contains_key(node_id)
    }

    /// Expand the quorum map for a node using its quorum set.
    pub fn expand(
        &mut self,
        node_id: &NodeId,
        quorum_set: ScpQuorumSet,
    ) -> Result<(), ExpandError> {
        let (node_distance, closest_validators) = {
            let Some(node_info) = self.quorum.get_mut(node_id) else {
                return Err(ExpandError::NodeNotFound);
            };

            if let Some(ref existing) = node_info.quorum_set {
                if existing == &quorum_set {
                    return Ok(());
                }
                return Err(ExpandError::AlreadyExpanded);
            }

            node_info.quorum_set = Some(quorum_set.clone());
            (node_info.distance, node_info.closest_validators.clone())
        };

        let new_dist = node_distance + 1;

        let mut ok = true;
        for_each_quorum_node(&quorum_set, &mut |qnode| {
            if !ok {
                return;
            }
            let existed = self.quorum.contains_key(qnode);
            let qnode_info = self
                .quorum
                .entry(qnode.clone())
                .or_insert_with(|| NodeInfo {
                    quorum_set: None,
                    distance: new_dist,
                    closest_validators: BTreeSet::new(),
                });

            if existed {
                if qnode_info.distance < new_dist {
                    return;
                }
                if qnode_info.quorum_set.is_some() {
                    ok = false;
                    return;
                }
                if new_dist < qnode_info.distance {
                    qnode_info.closest_validators.clear();
                    qnode_info.distance = new_dist;
                }
            }

            if new_dist == 1 {
                qnode_info.closest_validators.insert(qnode.clone());
            } else {
                qnode_info
                    .closest_validators
                    .extend(closest_validators.iter().cloned());
            }
        });

        if ok {
            Ok(())
        } else {
            Err(ExpandError::AlreadyExpanded)
        }
    }

    /// Rebuild the transitive quorum using a quorum-set lookup function.
    pub fn rebuild<F>(&mut self, lookup: F) -> Result<(), QuorumTrackerError>
    where
        F: Fn(&NodeId) -> Option<ScpQuorumSet>,
    {
        self.quorum.clear();
        self.quorum.insert(
            self.local_node_id.clone(),
            NodeInfo {
                quorum_set: None,
                distance: 0,
                closest_validators: BTreeSet::new(),
            },
        );

        let mut backlog = VecDeque::new();
        backlog.push_back(self.local_node_id.clone());

        while let Some(node) = backlog.pop_front() {
            let Some(info) = self.quorum.get(&node) else {
                return Err(QuorumTrackerError::MissingNode);
            };
            if info.quorum_set.is_none() {
                if let Some(qset) = lookup(&node) {
                    for_each_quorum_node(&qset, &mut |member| {
                        backlog.push_back(member.clone());
                    });
                    if let Err(_) = self.expand(&node, qset) {
                        return Err(QuorumTrackerError::ExpandFailed);
                    }
                }
            }
        }

        Ok(())
    }

    /// Return the currently tracked quorum map.
    pub fn quorum_map(&self) -> &HashMap<NodeId, NodeInfo> {
        &self.quorum
    }

    /// Return the number of nodes being tracked.
    pub fn tracked_node_count(&self) -> usize {
        self.quorum.len()
    }

    /// Return the closest validators in the local quorum set for a node.
    pub fn find_closest_validators(&self, node_id: &NodeId) -> Option<&BTreeSet<NodeId>> {
        self.quorum
            .get(node_id)
            .map(|info| &info.closest_validators)
    }
}

fn for_each_quorum_node<F>(quorum_set: &ScpQuorumSet, f: &mut F)
where
    F: FnMut(&NodeId),
{
    for validator in quorum_set.validators.iter() {
        f(validator);
    }
    for inner in quorum_set.inner_sets.iter() {
        for_each_quorum_node(inner, f);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use stellar_xdr::curr::{PublicKey, Uint256};

    fn make_node_id(seed: u8) -> NodeId {
        let mut bytes = [0u8; 32];
        bytes[0] = seed;
        NodeId(PublicKey::PublicKeyTypeEd25519(Uint256(bytes)))
    }

    fn make_quorum_set(validators: Vec<NodeId>, threshold: u32) -> ScpQuorumSet {
        ScpQuorumSet {
            threshold,
            validators: validators.try_into().unwrap_or_default(),
            inner_sets: vec![].try_into().unwrap(),
        }
    }

    fn make_quorum_set_with_inners(
        validators: Vec<NodeId>,
        inner_sets: Vec<ScpQuorumSet>,
        threshold: u32,
    ) -> ScpQuorumSet {
        ScpQuorumSet {
            threshold,
            validators: validators.try_into().unwrap_or_default(),
            inner_sets: inner_sets.try_into().unwrap_or_default(),
        }
    }

    #[test]
    fn test_slot_quorum_tracker_quorum_and_vblocking() {
        let local = make_node_id(1);
        let node_b = make_node_id(2);
        let node_c = make_node_id(3);
        let qset = make_quorum_set(vec![local.clone(), node_b.clone(), node_c.clone()], 2);

        let mut tracker = SlotQuorumTracker::new(Some(qset), 4);
        tracker.record_envelope(7, local.clone());
        tracker.record_envelope(7, node_b.clone());

        assert!(tracker.has_quorum(7, |node| {
            if node == &local || node == &node_b || node == &node_c {
                Some(make_quorum_set(
                    vec![local.clone(), node_b.clone(), node_c.clone()],
                    2,
                ))
            } else {
                None
            }
        }));
        assert!(tracker.is_v_blocking(7));

        tracker.record_envelope(7, node_c.clone());
        assert!(tracker.has_quorum(7, |node| {
            if node == &local || node == &node_b || node == &node_c {
                Some(make_quorum_set(
                    vec![local.clone(), node_b.clone(), node_c.clone()],
                    2,
                ))
            } else {
                None
            }
        }));
        assert!(tracker.is_v_blocking(7));
    }

    #[test]
    fn test_slot_quorum_tracker_prunes_old_slots() {
        let local = make_node_id(1);
        let qset = make_quorum_set(vec![local.clone()], 1);

        let mut tracker = SlotQuorumTracker::new(Some(qset), 2);
        tracker.record_envelope(1, local.clone());
        tracker.record_envelope(2, local.clone());
        tracker.record_envelope(3, local);

        assert!(!tracker.has_quorum(1, |_| Some(make_quorum_set(vec![make_node_id(1)], 1))));
        assert!(tracker.has_quorum(2, |_| Some(make_quorum_set(vec![make_node_id(1)], 1))));
        assert!(tracker.has_quorum(3, |_| Some(make_quorum_set(vec![make_node_id(1)], 1))));
    }

    #[test]
    fn test_slot_quorum_tracker_clear_and_set_local_qset() {
        let local = make_node_id(1);
        let qset = make_quorum_set(vec![local.clone()], 1);

        let mut tracker = SlotQuorumTracker::new(None, 2);
        tracker.record_envelope(5, local.clone());
        assert!(!tracker.has_quorum(5, |_| Some(qset.clone())));

        tracker.set_local_quorum_set(Some(qset.clone()));
        assert!(tracker.has_quorum(5, |_| Some(qset.clone())));

        tracker.clear_slot(5);
        assert!(!tracker.has_quorum(5, |_| Some(qset.clone())));
    }

    #[test]
    fn test_expand_tracks_closest_validators() {
        let local = make_node_id(1);
        let node_b = make_node_id(2);
        let node_c = make_node_id(3);
        let qset = make_quorum_set(vec![local.clone(), node_b.clone(), node_c.clone()], 2);

        let mut tracker = QuorumTracker::new(local.clone());
        tracker.expand(&local, qset).unwrap();

        assert!(tracker.is_node_definitely_in_quorum(&local));
        assert!(tracker.is_node_definitely_in_quorum(&node_b));
        assert!(tracker.is_node_definitely_in_quorum(&node_c));

        let closest_b = tracker.find_closest_validators(&node_b).unwrap();
        assert!(closest_b.contains(&node_b));
        let closest_c = tracker.find_closest_validators(&node_c).unwrap();
        assert!(closest_c.contains(&node_c));
    }

    #[test]
    fn test_rebuild_tracks_transitive_quorum() {
        let local = make_node_id(1);
        let node_b = make_node_id(2);
        let node_c = make_node_id(3);

        let qset_a = make_quorum_set(vec![local.clone(), node_b.clone()], 2);
        let qset_b = make_quorum_set(vec![node_b.clone(), node_c.clone()], 2);

        let mut tracker = QuorumTracker::new(local.clone());
        tracker
            .rebuild(|node| {
                if node == &local {
                    Some(qset_a.clone())
                } else if node == &node_b {
                    Some(qset_b.clone())
                } else {
                    None
                }
            })
            .expect("rebuild");

        assert!(tracker.is_node_definitely_in_quorum(&local));
        assert!(tracker.is_node_definitely_in_quorum(&node_b));
        assert!(tracker.is_node_definitely_in_quorum(&node_c));

        let closest_c = tracker.find_closest_validators(&node_c).unwrap();
        assert!(closest_c.contains(&node_b));
    }

    #[test]
    fn test_expand_returns_false_on_conflicting_expansion() {
        let local = make_node_id(1);
        let node_b = make_node_id(2);
        let node_c = make_node_id(3);

        let qset_local = make_quorum_set(vec![local.clone(), node_b.clone(), node_c.clone()], 2);
        let qset_b = make_quorum_set(vec![node_b.clone(), node_c.clone()], 2);
        let qset_conflict = make_quorum_set(vec![node_b.clone()], 1);

        let mut tracker = QuorumTracker::new(local.clone());
        tracker.expand(&local, qset_local).unwrap();
        tracker.expand(&node_b, qset_b).unwrap();
        assert!(tracker.expand(&node_b, qset_conflict).is_err());
    }

    #[test]
    fn test_expand_returns_true_on_same_quorum_set() {
        let local = make_node_id(1);
        let node_b = make_node_id(2);

        let qset_local = make_quorum_set(vec![local.clone(), node_b.clone()], 2);
        let qset_b = make_quorum_set(vec![node_b.clone()], 1);

        let mut tracker = QuorumTracker::new(local.clone());
        tracker.expand(&local, qset_local).unwrap();
        tracker.expand(&node_b, qset_b.clone()).unwrap();
        tracker.expand(&node_b, qset_b).unwrap();
    }

    #[test]
    fn test_rebuild_merges_closest_validators_for_shared_nodes() {
        let local = make_node_id(1);
        let node_b = make_node_id(2);
        let node_c = make_node_id(3);
        let node_d = make_node_id(4);

        let qset_a = make_quorum_set(vec![local.clone(), node_b.clone(), node_c.clone()], 2);
        let qset_b = make_quorum_set(vec![node_b.clone(), node_d.clone()], 2);
        let qset_c = make_quorum_set(vec![node_c.clone(), node_d.clone()], 2);

        let mut tracker = QuorumTracker::new(local.clone());
        tracker
            .rebuild(|node| {
                if node == &local {
                    Some(qset_a.clone())
                } else if node == &node_b {
                    Some(qset_b.clone())
                } else if node == &node_c {
                    Some(qset_c.clone())
                } else {
                    None
                }
            })
            .expect("rebuild");

        let closest_d = tracker.find_closest_validators(&node_d).unwrap();
        assert!(closest_d.contains(&node_b));
        assert!(closest_d.contains(&node_c));
    }

    #[test]
    fn test_expand_tracks_inner_set_nodes() {
        let local = make_node_id(1);
        let node_b = make_node_id(2);
        let node_c = make_node_id(3);

        let inner = make_quorum_set(vec![node_b.clone(), node_c.clone()], 1);
        let qset_local = make_quorum_set_with_inners(vec![local.clone()], vec![inner], 1);

        let mut tracker = QuorumTracker::new(local.clone());
        tracker.expand(&local, qset_local).unwrap();

        assert!(tracker.is_node_definitely_in_quorum(&node_b));
        assert!(tracker.is_node_definitely_in_quorum(&node_c));

        let closest_b = tracker.find_closest_validators(&node_b).unwrap();
        assert!(closest_b.contains(&node_b));
    }

    #[test]
    fn test_rebuild_closest_validators_via_inner_sets() {
        let local = make_node_id(1);
        let node_b = make_node_id(2);
        let node_c = make_node_id(3);
        let node_d = make_node_id(4);

        let inner = make_quorum_set(vec![node_b.clone(), node_c.clone()], 1);
        let qset_local = make_quorum_set_with_inners(vec![local.clone()], vec![inner], 1);
        let qset_b = make_quorum_set(vec![node_b.clone(), node_d.clone()], 2);

        let mut tracker = QuorumTracker::new(local.clone());
        tracker
            .rebuild(|node| {
                if node == &local {
                    Some(qset_local.clone())
                } else if node == &node_b {
                    Some(qset_b.clone())
                } else {
                    None
                }
            })
            .expect("rebuild");

        let closest_d = tracker.find_closest_validators(&node_d).unwrap();
        assert!(closest_d.contains(&node_b));
        assert!(!closest_d.contains(&node_c));
    }
}
