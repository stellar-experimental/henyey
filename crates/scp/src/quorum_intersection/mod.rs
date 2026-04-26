//! Quorum intersection analysis for SCP networks.
//!
//! Provides analysis functions that check whether all quorums in a network
//! intersect — a critical safety property for SCP.
//!
//! Uses SCC decomposition + recursive min-quorum enumeration (Lachowski,
//! arXiv 1902.06493) matching stellar-core's `QuorumIntersectionCheckerImpl`.
//! This handles real-world networks (e.g. mainnet with 30+ validators)
//! efficiently, unlike the brute-force 2^n approach.

mod bit_set;
mod checker;
mod qbitset;
mod tarjan;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use henyey_common::xdr_to_bytes;
use henyey_crypto::Sha256Hasher;
use stellar_xdr::curr::{NodeId, ScpQuorumSet};

use crate::quorum::is_quorum_slice;
use crate::Hash256;

use checker::{CheckerResult, QuorumIntersectionChecker};

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
    /// The analysis was interrupted before completing.
    Interrupted,
}

/// Simple, deterministic quorum intersection check.
///
/// Uses seed=0 for fully deterministic results and a never-set interrupt flag.
/// Suitable for CLI and library callers. Never returns `Interrupted`.
pub fn check_intersection(
    quorum_map: &HashMap<NodeId, Option<ScpQuorumSet>>,
) -> IntersectionResult {
    let interrupt = Arc::new(AtomicBool::new(false));
    let result = check_intersection_interruptible(quorum_map, &interrupt, 0);
    debug_assert!(
        !matches!(result, IntersectionResult::Interrupted),
        "check_intersection with never-set interrupt flag returned Interrupted"
    );
    result
}

/// Interrupt-aware quorum intersection check.
///
/// Uses caller-provided seed and interrupt flag. Returns `Interrupted` if
/// the interrupt flag is set during analysis.
pub fn check_intersection_interruptible(
    quorum_map: &HashMap<NodeId, Option<ScpQuorumSet>>,
    interrupt: &Arc<AtomicBool>,
    seed: u64,
) -> IntersectionResult {
    if quorum_map.is_empty() {
        return IntersectionResult::Intersects;
    }

    let checker = QuorumIntersectionChecker::new(quorum_map, Arc::clone(interrupt), seed);
    match checker.check() {
        CheckerResult::Intersects => IntersectionResult::Intersects,
        CheckerResult::Split { pair } => IntersectionResult::Split { pair },
        CheckerResult::Interrupted => IntersectionResult::Interrupted,
    }
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
    fn test_large_network_now_works() {
        // Previously returned TooLarge. Now the efficient algorithm handles it.
        let mut map = HashMap::new();
        for i in 0..25 {
            let node = make_node_id(i);
            map.insert(node.clone(), Some(make_qset(vec![node], 1)));
        }
        // Each node only requires itself → each is its own quorum.
        // Any two single-node quorums are disjoint → Split.
        match check_intersection(&map) {
            IntersectionResult::Split { pair: (a, b) } => {
                let a_set: HashSet<_> = a.into_iter().collect();
                let b_set: HashSet<_> = b.into_iter().collect();
                assert!(a_set.is_disjoint(&b_set));
            }
            other => panic!(
                "Expected Split for 25-node self-quorum network, got {:?}",
                other
            ),
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

    // --- Brute-force oracle for cross-validation ---

    /// Brute-force quorum intersection check (retained as test oracle).
    fn brute_force_check_intersection(quorum_map: &HashMap<NodeId, Option<ScpQuorumSet>>) -> bool {
        use crate::quorum::is_quorum;

        let mut sorted_nodes: Vec<NodeId> = quorum_map.keys().cloned().collect();
        sorted_nodes.sort_by_key(|a| xdr_to_bytes(a));

        let total = sorted_nodes.len();
        if total == 0 || total > 20 {
            // Oracle only works for small networks.
            return true;
        }

        let mut quorums: Vec<HashSet<NodeId>> = Vec::new();

        for mask in 1..(1u64 << total) {
            let subset: HashSet<NodeId> = sorted_nodes
                .iter()
                .enumerate()
                .filter(|(idx, _)| (mask >> idx) & 1 == 1)
                .map(|(_, node)| node.clone())
                .collect();

            // Check if this subset is a quorum.
            let mut is_q = false;
            let mut sorted_subset: Vec<&NodeId> = subset.iter().collect();
            sorted_subset.sort_by_key(|a| xdr_to_bytes(*a));
            for root in &sorted_subset {
                if let Some(Some(qset)) = quorum_map.get(*root) {
                    if is_quorum(qset, &subset, |id| {
                        quorum_map.get(id).and_then(|opt| opt.clone())
                    }) {
                        is_q = true;
                        break;
                    }
                }
            }
            if is_q {
                quorums.push(subset);
            }
        }

        for i in 0..quorums.len() {
            for j in (i + 1)..quorums.len() {
                if quorums[i].is_disjoint(&quorums[j]) {
                    return false;
                }
            }
        }
        true
    }

    #[test]
    fn test_oracle_cross_validation_intersecting() {
        // Various small networks: verify new checker agrees with brute-force.
        let cases: Vec<HashMap<NodeId, Option<ScpQuorumSet>>> = vec![
            // Case 1: 3-node 2-of-3
            {
                let nodes: Vec<NodeId> = (1..=3).map(make_node_id).collect();
                let mut map = HashMap::new();
                for n in &nodes {
                    map.insert(n.clone(), Some(make_qset(nodes.clone(), 2)));
                }
                map
            },
            // Case 2: 5-node 3-of-5
            {
                let nodes: Vec<NodeId> = (1..=5).map(make_node_id).collect();
                let mut map = HashMap::new();
                for n in &nodes {
                    map.insert(n.clone(), Some(make_qset(nodes.clone(), 3)));
                }
                map
            },
            // Case 3: 4-node with unknown qset
            {
                let nodes: Vec<NodeId> = (1..=4).map(make_node_id).collect();
                let known = vec![nodes[0].clone(), nodes[1].clone(), nodes[2].clone()];
                let mut map = HashMap::new();
                for n in &known {
                    map.insert(n.clone(), Some(make_qset(known.clone(), 2)));
                }
                map.insert(nodes[3].clone(), None);
                map
            },
        ];

        for (i, map) in cases.iter().enumerate() {
            let oracle = brute_force_check_intersection(map);
            let checker = matches!(check_intersection(map), IntersectionResult::Intersects);
            assert_eq!(
                oracle, checker,
                "Case {}: oracle={}, checker={}",
                i, oracle, checker
            );
        }
    }

    #[test]
    fn test_oracle_cross_validation_split() {
        // Split networks: verify both agree.
        let cases: Vec<HashMap<NodeId, Option<ScpQuorumSet>>> = vec![
            // Case 1: 4-node split (2 groups of 2)
            {
                let mut map = HashMap::new();
                let n1 = make_node_id(1);
                let n2 = make_node_id(2);
                let n3 = make_node_id(3);
                let n4 = make_node_id(4);
                map.insert(n1.clone(), Some(make_qset(vec![n1.clone(), n2.clone()], 1)));
                map.insert(n2.clone(), Some(make_qset(vec![n1.clone(), n2.clone()], 1)));
                map.insert(n3.clone(), Some(make_qset(vec![n3.clone(), n4.clone()], 1)));
                map.insert(n4.clone(), Some(make_qset(vec![n3.clone(), n4.clone()], 1)));
                map
            },
            // Case 2: 6-node split (2 groups of 3, each 1-of-3)
            {
                let group_a: Vec<NodeId> = (1..=3).map(make_node_id).collect();
                let group_b: Vec<NodeId> = (4..=6).map(make_node_id).collect();
                let mut map = HashMap::new();
                for n in &group_a {
                    map.insert(n.clone(), Some(make_qset(group_a.clone(), 1)));
                }
                for n in &group_b {
                    map.insert(n.clone(), Some(make_qset(group_b.clone(), 1)));
                }
                map
            },
        ];

        for (i, map) in cases.iter().enumerate() {
            let oracle = brute_force_check_intersection(map);
            let checker = matches!(check_intersection(map), IntersectionResult::Intersects);
            assert_eq!(
                oracle, checker,
                "Split case {}: oracle={}, checker={}",
                i, oracle, checker
            );
        }
    }

    #[test]
    fn test_interruptible_api() {
        let n1 = make_node_id(1);
        let mut map = HashMap::new();
        map.insert(n1.clone(), Some(make_qset(vec![n1.clone()], 1)));

        // Non-interrupted.
        let interrupt = Arc::new(AtomicBool::new(false));
        assert!(matches!(
            check_intersection_interruptible(&map, &interrupt, 0),
            IntersectionResult::Intersects
        ));

        // Pre-interrupted.
        let interrupt = Arc::new(AtomicBool::new(true));
        assert!(matches!(
            check_intersection_interruptible(&map, &interrupt, 0),
            IntersectionResult::Interrupted
        ));
    }
}
