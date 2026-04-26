//! Quorum intersection analysis for SCP networks.
//!
//! Provides a pure analysis function that checks whether all quorums in a
//! network intersect — a critical safety property for SCP.
//!
//! The algorithm enumerates all 2^n subsets of nodes (brute force), identifies
//! valid quorums, and checks all pairs for intersection. This is only practical
//! for small networks (≤ 20 nodes).
//!
//! For larger networks, a SAT-based approach (as in stellar-core v2) is needed
//! but is out of scope here.

use std::collections::{BTreeMap, HashMap, HashSet};

use henyey_common::xdr_to_bytes;
use henyey_crypto::Sha256Hasher;
use stellar_xdr::curr::{NodeId, ScpQuorumSet};

use crate::quorum::{is_quorum, is_quorum_slice};
use crate::Hash256;

/// Maximum number of nodes supported for brute-force intersection analysis.
///
/// The algorithm is O(2^n), so we cap at 20 nodes (2^20 ≈ 1M subsets).
pub const MAX_INTERSECTION_NODES: usize = 20;

/// Result of a quorum intersection check.
#[derive(Debug, Clone)]
pub enum IntersectionResult {
    /// All quorum pairs intersect. The network is safe.
    Intersects,
    /// Found two quorums that do not intersect. The network is unsafe.
    Split {
        /// A pair of non-intersecting quorums (sorted by NodeId XDR for determinism).
        pair: (Vec<NodeId>, Vec<NodeId>),
    },
    /// Too many nodes for brute-force analysis.
    TooLarge {
        /// Number of nodes in the quorum map.
        node_count: usize,
    },
}

/// Check whether all quorums in the network intersect.
///
/// Operates on a quorum map where `None` means the node was observed but its
/// quorum set is unknown. Such nodes are naturally pruned during quorum
/// detection (they have no quorum set so they fail the `is_quorum` check).
///
/// The function tries all nodes with known quorum sets as quorum roots,
/// sorted deterministically by NodeId XDR bytes.
///
/// Returns [`IntersectionResult::TooLarge`] if the map exceeds
/// [`MAX_INTERSECTION_NODES`] nodes.
pub fn check_intersection(
    quorum_map: &HashMap<NodeId, Option<ScpQuorumSet>>,
) -> IntersectionResult {
    if quorum_map.is_empty() {
        return IntersectionResult::Intersects;
    }

    if quorum_map.len() > MAX_INTERSECTION_NODES {
        return IntersectionResult::TooLarge {
            node_count: quorum_map.len(),
        };
    }

    // Sort nodes deterministically by XDR bytes for reproducible enumeration.
    let mut sorted_nodes: Vec<NodeId> = quorum_map.keys().cloned().collect();
    sorted_nodes.sort_by_key(|a| xdr_to_bytes(a));

    // Enumerate all non-empty subsets and find valid quorums.
    let mut quorums: Vec<HashSet<NodeId>> = Vec::new();
    let total = sorted_nodes.len();

    for mask in 1..(1u64 << total) {
        let subset: HashSet<NodeId> = sorted_nodes
            .iter()
            .enumerate()
            .filter(|(idx, _)| (mask >> idx) & 1 == 1)
            .map(|(_, node)| node.clone())
            .collect();

        if is_quorum_for_map(&subset, quorum_map) {
            quorums.push(subset);
        }
    }

    // Check all quorum pairs for intersection.
    for i in 0..quorums.len() {
        for j in (i + 1)..quorums.len() {
            if quorums[i].is_disjoint(&quorums[j]) {
                let mut a: Vec<NodeId> = quorums[i].iter().cloned().collect();
                let mut b: Vec<NodeId> = quorums[j].iter().cloned().collect();
                sort_nodes(&mut a);
                sort_nodes(&mut b);
                // Deterministic ordering: smaller set first, then by first node.
                if a.len() > b.len()
                    || (a.len() == b.len()
                        && !a.is_empty()
                        && !b.is_empty()
                        && xdr_to_bytes(&a[0]) > xdr_to_bytes(&b[0]))
                {
                    std::mem::swap(&mut a, &mut b);
                }
                return IntersectionResult::Split { pair: (a, b) };
            }
        }
    }

    IntersectionResult::Intersects
}

/// Check if a subset forms a valid quorum using the quorum map.
///
/// Tries all nodes in the subset that have known quorum sets as the root node.
/// This handles the case where some nodes have `None` quorum sets — we need
/// to find at least one known-qset root whose `is_quorum` check passes.
fn is_quorum_for_map(
    nodes: &HashSet<NodeId>,
    quorum_map: &HashMap<NodeId, Option<ScpQuorumSet>>,
) -> bool {
    // Sort for deterministic root selection order.
    let mut sorted: Vec<&NodeId> = nodes.iter().collect();
    sorted.sort_by_key(|a| xdr_to_bytes(*a));

    for root in sorted {
        if let Some(Some(qset)) = quorum_map.get(root) {
            if is_quorum(qset, nodes, |id| {
                quorum_map.get(id).and_then(|opt| opt.clone())
            }) {
                return true;
            }
        }
    }
    false
}

/// Sort a vec of NodeIds by XDR bytes for deterministic output.
fn sort_nodes(nodes: &mut [NodeId]) {
    nodes.sort_by_key(|a| xdr_to_bytes(a));
}

/// Compute a deterministic hash of a quorum map.
///
/// Matches stellar-core's `getQmapHash()` (HerderImpl.cpp:1912-1931):
/// - Entries sorted by NodeId XDR bytes (std::map ordering)
/// - Each entry: hash(node_xdr, qset_xdr) or hash(node_xdr, `\0`) for unknown qsets
/// - `distance` and `closest_validators` are ignored
pub fn compute_quorum_map_hash<Q: QuorumMapEntry>(quorum_map: &HashMap<NodeId, Q>) -> Hash256 {
    let mut hasher = Sha256Hasher::new();

    // Sort by NodeId XDR bytes, matching stellar-core's std::map<NodeID, ...> ordering.
    let mut ordered: BTreeMap<Vec<u8>, (&NodeId, &Q)> = BTreeMap::new();
    for (node_id, info) in quorum_map {
        ordered.insert(xdr_to_bytes(node_id), (node_id, info));
    }

    for (_key, (node_id, info)) in &ordered {
        hasher.update(&xdr_to_bytes(*node_id));
        if let Some(qset) = info.quorum_set_ref() {
            hasher.update(&xdr_to_bytes(qset));
        } else {
            hasher.update(b"\0");
        }
    }

    hasher.finalize()
}

/// Trait to abstract over different quorum map value types.
///
/// The herder uses `NodeInfo { quorum_set: Option<ScpQuorumSet>, ... }` while
/// the checker uses `Option<ScpQuorumSet>` directly. This trait lets
/// `compute_quorum_map_hash` work with both.
pub trait QuorumMapEntry {
    fn quorum_set_ref(&self) -> Option<&ScpQuorumSet>;
}

impl QuorumMapEntry for Option<ScpQuorumSet> {
    fn quorum_set_ref(&self) -> Option<&ScpQuorumSet> {
        self.as_ref()
    }
}

impl QuorumMapEntry for ScpQuorumSet {
    fn quorum_set_ref(&self) -> Option<&ScpQuorumSet> {
        Some(self)
    }
}

/// Validate that each node's quorum slice is satisfiable by the network.
///
/// Returns the first node whose quorum set cannot be satisfied, or `None`
/// if all are satisfiable.
pub fn find_unsatisfiable_node(
    quorum_map: &HashMap<NodeId, Option<ScpQuorumSet>>,
) -> Option<NodeId> {
    let all_nodes: HashSet<NodeId> = quorum_map.keys().cloned().collect();
    for (node, qset_opt) in quorum_map {
        if let Some(qset) = qset_opt {
            if !is_quorum_slice(qset, &all_nodes, &|id| {
                quorum_map.get(id).and_then(|opt| opt.clone())
            }) {
                return Some(node.clone());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::make_node_id;

    fn make_qset(validators: Vec<NodeId>, threshold: u32) -> ScpQuorumSet {
        ScpQuorumSet {
            threshold,
            validators: validators.try_into().unwrap_or_default(),
            inner_sets: Vec::new().try_into().unwrap_or_default(),
        }
    }

    fn make_qset_with_inner(
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
    fn test_empty_map_intersects() {
        let map = HashMap::new();
        assert!(matches!(
            check_intersection(&map),
            IntersectionResult::Intersects
        ));
    }

    #[test]
    fn test_single_node_intersects() {
        let n1 = make_node_id(1);
        let mut map = HashMap::new();
        map.insert(n1.clone(), Some(make_qset(vec![n1.clone()], 1)));
        assert!(matches!(
            check_intersection(&map),
            IntersectionResult::Intersects
        ));
    }

    #[test]
    fn test_three_node_2_of_3_intersects() {
        let n1 = make_node_id(1);
        let n2 = make_node_id(2);
        let n3 = make_node_id(3);
        let all = vec![n1.clone(), n2.clone(), n3.clone()];

        let mut map = HashMap::new();
        map.insert(n1.clone(), Some(make_qset(all.clone(), 2)));
        map.insert(n2.clone(), Some(make_qset(all.clone(), 2)));
        map.insert(n3.clone(), Some(make_qset(all.clone(), 2)));

        assert!(matches!(
            check_intersection(&map),
            IntersectionResult::Intersects
        ));
    }

    #[test]
    fn test_split_network() {
        // Two disjoint groups that each form their own quorum but don't overlap.
        let n1 = make_node_id(1);
        let n2 = make_node_id(2);
        let n3 = make_node_id(3);
        let n4 = make_node_id(4);

        let mut map = HashMap::new();
        // Group 1: n1, n2 require 1-of-{n1, n2}
        map.insert(n1.clone(), Some(make_qset(vec![n1.clone(), n2.clone()], 1)));
        map.insert(n2.clone(), Some(make_qset(vec![n1.clone(), n2.clone()], 1)));
        // Group 2: n3, n4 require 1-of-{n3, n4}
        map.insert(n3.clone(), Some(make_qset(vec![n3.clone(), n4.clone()], 1)));
        map.insert(n4.clone(), Some(make_qset(vec![n3.clone(), n4.clone()], 1)));

        match check_intersection(&map) {
            IntersectionResult::Split { pair: (a, b) } => {
                // The split should be between the two groups.
                let a_set: HashSet<_> = a.into_iter().collect();
                let b_set: HashSet<_> = b.into_iter().collect();
                assert!(a_set.is_disjoint(&b_set));
            }
            other => panic!("Expected Split, got {:?}", other),
        }
    }

    #[test]
    fn test_too_large() {
        let mut map = HashMap::new();
        for i in 0..21 {
            let node = make_node_id(i);
            map.insert(node.clone(), Some(make_qset(vec![node], 1)));
        }
        match check_intersection(&map) {
            IntersectionResult::TooLarge { node_count } => {
                assert_eq!(node_count, 21);
            }
            other => panic!("Expected TooLarge, got {:?}", other),
        }
    }

    #[test]
    fn test_none_qset_nodes_pruned() {
        // 3 nodes where one has unknown qset. The remaining 2 form quorums
        // with 2-of-2 threshold, so they always intersect (both must participate).
        let n1 = make_node_id(1);
        let n2 = make_node_id(2);
        let n3 = make_node_id(3);

        let mut map = HashMap::new();
        map.insert(n1.clone(), Some(make_qset(vec![n1.clone(), n2.clone()], 2)));
        map.insert(n2.clone(), Some(make_qset(vec![n1.clone(), n2.clone()], 2)));
        map.insert(n3.clone(), None); // Unknown qset — pruned during quorum check

        assert!(matches!(
            check_intersection(&map),
            IntersectionResult::Intersects
        ));
    }

    #[test]
    fn test_nested_inner_sets() {
        // Test that nested inner sets are handled correctly.
        let n1 = make_node_id(1);
        let n2 = make_node_id(2);
        let n3 = make_node_id(3);

        // Inner set: 1-of-{n2, n3}
        let inner = make_qset(vec![n2.clone(), n3.clone()], 1);
        // Outer: threshold 2 of {n1, inner_set} — requires n1 + at least 1 of {n2,n3}
        let outer = make_qset_with_inner(vec![n1.clone()], vec![inner], 2);

        let mut map = HashMap::new();
        map.insert(n1.clone(), Some(outer.clone()));
        map.insert(
            n2.clone(),
            Some(make_qset(vec![n1.clone(), n2.clone(), n3.clone()], 2)),
        );
        map.insert(
            n3.clone(),
            Some(make_qset(vec![n1.clone(), n2.clone(), n3.clone()], 2)),
        );

        assert!(matches!(
            check_intersection(&map),
            IntersectionResult::Intersects
        ));
    }

    #[test]
    fn test_hash_determinism() {
        let n1 = make_node_id(1);
        let n2 = make_node_id(2);
        let qset1 = make_qset(vec![n1.clone(), n2.clone()], 1);
        let qset2 = make_qset(vec![n1.clone(), n2.clone()], 1);

        // Build two maps with same data but potentially different iteration order.
        let mut map1: HashMap<NodeId, Option<ScpQuorumSet>> = HashMap::new();
        map1.insert(n1.clone(), Some(qset1.clone()));
        map1.insert(n2.clone(), Some(qset2.clone()));

        let mut map2: HashMap<NodeId, Option<ScpQuorumSet>> = HashMap::new();
        map2.insert(n2.clone(), Some(qset2));
        map2.insert(n1.clone(), Some(qset1));

        assert_eq!(
            compute_quorum_map_hash(&map1),
            compute_quorum_map_hash(&map2)
        );
    }

    #[test]
    fn test_hash_differs_with_none_vs_some() {
        let n1 = make_node_id(1);
        let qset = make_qset(vec![n1.clone()], 1);

        let mut map_some: HashMap<NodeId, Option<ScpQuorumSet>> = HashMap::new();
        map_some.insert(n1.clone(), Some(qset));

        let mut map_none: HashMap<NodeId, Option<ScpQuorumSet>> = HashMap::new();
        map_none.insert(n1.clone(), None);

        assert_ne!(
            compute_quorum_map_hash(&map_some),
            compute_quorum_map_hash(&map_none)
        );
    }

    #[test]
    fn test_hash_with_scpquorumset_directly() {
        // Test that compute_quorum_map_hash works with HashMap<NodeId, ScpQuorumSet> too.
        let n1 = make_node_id(1);
        let qset = make_qset(vec![n1.clone()], 1);

        let mut map: HashMap<NodeId, ScpQuorumSet> = HashMap::new();
        map.insert(n1.clone(), qset.clone());

        let mut map_opt: HashMap<NodeId, Option<ScpQuorumSet>> = HashMap::new();
        map_opt.insert(n1.clone(), Some(qset));

        // Both should produce the same hash.
        assert_eq!(
            compute_quorum_map_hash(&map),
            compute_quorum_map_hash(&map_opt)
        );
    }

    #[test]
    fn test_find_unsatisfiable_node() {
        let n1 = make_node_id(1);
        let n2 = make_node_id(2);
        // n1 requires n3 which doesn't exist.
        let n3 = make_node_id(3);

        let mut map = HashMap::new();
        map.insert(n1.clone(), Some(make_qset(vec![n1.clone(), n3.clone()], 2)));
        map.insert(n2.clone(), Some(make_qset(vec![n1.clone(), n2.clone()], 1)));

        let unsatisfiable = find_unsatisfiable_node(&map);
        assert_eq!(unsatisfiable, Some(n1));
    }

    #[test]
    fn test_find_unsatisfiable_none_returns_none() {
        let n1 = make_node_id(1);
        let n2 = make_node_id(2);

        let mut map = HashMap::new();
        map.insert(n1.clone(), Some(make_qset(vec![n1.clone(), n2.clone()], 1)));
        map.insert(n2.clone(), Some(make_qset(vec![n1.clone(), n2.clone()], 1)));

        assert_eq!(find_unsatisfiable_node(&map), None);
    }
}
