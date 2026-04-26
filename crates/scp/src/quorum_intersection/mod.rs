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

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
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

/// Error returned when a critical-groups computation is interrupted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Interrupted;

impl fmt::Display for Interrupted {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "quorum intersection analysis interrupted")
    }
}

impl std::error::Error for Interrupted {}

/// Compute intersection-critical groups.
///
/// Mirrors stellar-core's `QuorumIntersectionChecker::getIntersectionCriticalGroups()`
/// (QuorumIntersectionCheckerImpl.cpp:833-955).
///
/// A group is "intersection-critical" if making it "fickle" (willing to go
/// along with anyone) causes the network to lose quorum intersection.
///
/// Returns groups sorted deterministically (BTreeSet ordering = XDR byte
/// ordering). Returns `Err(Interrupted)` if the interrupt flag is set during
/// any sub-check.
pub fn get_intersection_critical_groups(
    quorum_map: &HashMap<NodeId, Option<ScpQuorumSet>>,
    interrupt: &Arc<AtomicBool>,
    seed: u64,
) -> Result<Vec<Vec<NodeId>>, Interrupted> {
    // Step 1: Find criticality candidates.
    let mut candidates: BTreeSet<BTreeSet<NodeId>> = BTreeSet::new();
    for (_node, qset_opt) in quorum_map {
        if let Some(qset) = qset_opt {
            find_criticality_candidates(qset, &mut candidates, true);
        }
    }

    tracing::info!(
        count = candidates.len(),
        "Examining node groups for intersection-criticality"
    );

    // Step 2: Test each candidate by making it fickle.
    let mut critical: BTreeSet<BTreeSet<NodeId>> = BTreeSet::new();
    let mut test_qmap = quorum_map.clone();

    for group in &candidates {
        if interrupt.load(Ordering::Relaxed) {
            return Err(Interrupted);
        }

        // Build the fickle qset: threshold=2, two inner sets.
        // Inner set 1: the group itself (threshold = group size).
        let group_qset = ScpQuorumSet {
            threshold: group.len() as u32,
            validators: group
                .iter()
                .cloned()
                .collect::<Vec<_>>()
                .try_into()
                .unwrap_or_default(),
            inner_sets: Vec::new().try_into().unwrap_or_default(),
        };

        // Inner set 2: all nodes outside the group that point to any group member.
        let mut points_to_group: BTreeSet<NodeId> = BTreeSet::new();
        for candidate in group {
            for (d_node, d_qset_opt) in quorum_map {
                if !group.contains(d_node) {
                    if let Some(d_qset) = d_qset_opt {
                        if points_to_candidate(d_qset, candidate) {
                            points_to_group.insert(d_node.clone());
                        }
                    }
                }
            }
        }

        let dependers_qset = ScpQuorumSet {
            threshold: 1,
            validators: points_to_group
                .iter()
                .cloned()
                .collect::<Vec<_>>()
                .try_into()
                .unwrap_or_default(),
            inner_sets: Vec::new().try_into().unwrap_or_default(),
        };

        let fickle_qset = ScpQuorumSet {
            threshold: 2,
            validators: Vec::new().try_into().unwrap_or_default(),
            inner_sets: vec![group_qset, dependers_qset]
                .try_into()
                .unwrap_or_default(),
        };

        // Install the fickle qset for every member of the group.
        for candidate in group {
            test_qmap.insert(candidate.clone(), Some(fickle_qset.clone()));
        }

        // Check if this modified config loses intersection.
        let result = check_intersection_interruptible(&test_qmap, interrupt, seed);
        match result {
            IntersectionResult::Interrupted => return Err(Interrupted),
            IntersectionResult::Intersects => {
                tracing::debug!(
                    group_size = group.len(),
                    dependers = points_to_group.len(),
                    "group is not intersection-critical"
                );
            }
            IntersectionResult::Split { .. } => {
                tracing::warn!(
                    group_size = group.len(),
                    dependers = points_to_group.len(),
                    "group IS intersection-critical"
                );
                critical.insert(group.clone());
            }
        }

        // Restore original qsets for all group members.
        for candidate in group {
            test_qmap.insert(
                candidate.clone(),
                quorum_map.get(candidate).cloned().unwrap_or(None),
            );
        }
    }

    if critical.is_empty() {
        tracing::info!("No intersection-critical groups found");
    } else {
        tracing::warn!(count = critical.len(), "Found intersection-critical groups");
    }

    // Convert BTreeSet<BTreeSet<NodeId>> → Vec<Vec<NodeId>> preserving ordering.
    Ok(critical
        .into_iter()
        .map(|group| group.into_iter().collect())
        .collect())
}

/// Recursively find criticality candidates from a quorum set.
///
/// Mirrors stellar-core's `findCriticalityCandidates()`.
fn find_criticality_candidates(
    qset: &ScpQuorumSet,
    candidates: &mut BTreeSet<BTreeSet<NodeId>>,
    root: bool,
) {
    // Always add each validator as a singleton.
    for v in qset.validators.iter() {
        let singleton: BTreeSet<NodeId> = [v.clone()].into_iter().collect();
        candidates.insert(singleton);
    }

    // Non-root with no inner sets => leaf group; record the whole validator set.
    if !root && qset.inner_sets.is_empty() {
        let group: BTreeSet<NodeId> = qset.validators.iter().cloned().collect();
        if !group.is_empty() {
            candidates.insert(group);
        }
    }

    // Recurse into inner sets.
    for inner in qset.inner_sets.iter() {
        find_criticality_candidates(inner, candidates, false);
    }
}

/// Check if a quorum set references a specific node (directly or in inner sets).
///
/// Mirrors stellar-core's `pointsToCandidate()`.
fn points_to_candidate(qset: &ScpQuorumSet, candidate: &NodeId) -> bool {
    for v in qset.validators.iter() {
        if v == candidate {
            return true;
        }
    }
    for inner in qset.inner_sets.iter() {
        if points_to_candidate(inner, candidate) {
            return true;
        }
    }
    false
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

    /// Regression test: missing qsets must NOT reduce thresholds.
    ///
    /// A depends on B with threshold 2. B has no qset (dead).
    /// With correct handling (option #1 from stellar-core), A cannot form a
    /// quorum because it needs 2 validators but only has itself (B is dead).
    /// With incorrect threshold reduction, A's threshold would drop to 1
    /// and {A} would be a quorum, producing a false "intersects" result.
    #[test]
    fn test_missing_qset_does_not_reduce_threshold() {
        let a = make_node_id(1);
        let b = make_node_id(2);

        let mut map: HashMap<NodeId, Option<ScpQuorumSet>> = HashMap::new();
        // A's qset: threshold=2, validators=[A, B]
        map.insert(a.clone(), Some(make_qset(vec![a.clone(), b.clone()], 2)));
        // B has no qset (dead node)
        map.insert(b.clone(), None);

        // Neither A nor B can form a quorum: A needs 2 votes but B is dead.
        // The only possible quorum set {A} doesn't satisfy threshold=2.
        // So no quorums exist → intersection is trivially satisfied.
        let result = check_intersection(&map);
        assert!(
            matches!(result, IntersectionResult::Intersects),
            "Expected Intersects (no quorums exist), got {:?}",
            result
        );
    }

    /// Regression test: dead nodes don't make otherwise-healthy networks split.
    ///
    /// A, B, C are 2-of-3 with each other plus dead node D. Without D, they
    /// intersect. D's absence should not change the result (threshold stays 2,
    /// the dead slot just makes it harder, but the 3 live nodes still satisfy it).
    #[test]
    fn test_missing_qset_preserves_intersection() {
        let a = make_node_id(1);
        let b = make_node_id(2);
        let c = make_node_id(3);
        let d = make_node_id(4);

        let mut map: HashMap<NodeId, Option<ScpQuorumSet>> = HashMap::new();
        let all = vec![a.clone(), b.clone(), c.clone(), d.clone()];
        // Each live node has threshold=2, validators=[A,B,C,D]
        // D is dead → threshold stays 2, but A,B,C can still reach it.
        map.insert(a.clone(), Some(make_qset(all.clone(), 2)));
        map.insert(b.clone(), Some(make_qset(all.clone(), 2)));
        map.insert(c.clone(), Some(make_qset(all.clone(), 2)));
        map.insert(d.clone(), None);

        let result = check_intersection(&map);
        assert!(
            matches!(result, IntersectionResult::Intersects),
            "Expected Intersects, got {:?}",
            result
        );
    }

    // ---- Critical groups tests ----

    #[test]
    fn test_points_to_candidate_direct() {
        let n1 = make_node_id(1);
        let n2 = make_node_id(2);
        let qset = make_qset(vec![n1.clone(), n2.clone()], 2);
        assert!(points_to_candidate(&qset, &n1));
        assert!(points_to_candidate(&qset, &n2));
        assert!(!points_to_candidate(&qset, &make_node_id(3)));
    }

    #[test]
    fn test_points_to_candidate_nested() {
        let n1 = make_node_id(1);
        let n2 = make_node_id(2);
        let inner = make_qset(vec![n2.clone()], 1);
        let qset = make_qset_with_inner(vec![n1.clone()], vec![inner], 2);
        assert!(points_to_candidate(&qset, &n1));
        assert!(points_to_candidate(&qset, &n2));
        assert!(!points_to_candidate(&qset, &make_node_id(3)));
    }

    #[test]
    fn test_find_criticality_candidates_flat() {
        let n1 = make_node_id(1);
        let n2 = make_node_id(2);
        let qset = make_qset(vec![n1.clone(), n2.clone()], 2);

        let mut candidates = BTreeSet::new();
        find_criticality_candidates(&qset, &mut candidates, true);

        // Root: should add singletons for each validator but NOT the group itself.
        assert!(candidates.contains(&[n1.clone()].into_iter().collect::<BTreeSet<_>>()));
        assert!(candidates.contains(&[n2.clone()].into_iter().collect::<BTreeSet<_>>()));
        // Root with no inner sets → not a leaf group, so no group entry.
        assert_eq!(candidates.len(), 2);
    }

    #[test]
    fn test_find_criticality_candidates_leaf_group() {
        let n1 = make_node_id(1);
        let n2 = make_node_id(2);
        let qset = make_qset(vec![n1.clone(), n2.clone()], 2);

        let mut candidates = BTreeSet::new();
        // Non-root with no inner sets → leaf group.
        find_criticality_candidates(&qset, &mut candidates, false);

        assert!(candidates.contains(&[n1.clone()].into_iter().collect::<BTreeSet<_>>()));
        assert!(candidates.contains(&[n2.clone()].into_iter().collect::<BTreeSet<_>>()));
        // Also contains the full group {n1, n2}.
        let group: BTreeSet<NodeId> = [n1, n2].into_iter().collect();
        assert!(candidates.contains(&group));
        assert_eq!(candidates.len(), 3);
    }

    #[test]
    fn test_find_criticality_candidates_with_inner_sets() {
        let n1 = make_node_id(1);
        let n2 = make_node_id(2);
        let n3 = make_node_id(3);
        // Inner set with n2, n3 (leaf group when non-root).
        let inner = make_qset(vec![n2.clone(), n3.clone()], 1);
        let qset = make_qset_with_inner(vec![n1.clone()], vec![inner], 2);

        let mut candidates = BTreeSet::new();
        find_criticality_candidates(&qset, &mut candidates, true);

        // Singletons: n1, n2, n3
        assert!(candidates.contains(&[n1].into_iter().collect::<BTreeSet<_>>()));
        assert!(candidates.contains(&[n2.clone()].into_iter().collect::<BTreeSet<_>>()));
        assert!(candidates.contains(&[n3.clone()].into_iter().collect::<BTreeSet<_>>()));
        // Inner set is non-root with no inner sets → leaf group {n2, n3}.
        let group: BTreeSet<NodeId> = [n2, n3].into_iter().collect();
        assert!(candidates.contains(&group));
        assert_eq!(candidates.len(), 4);
    }

    #[test]
    fn test_critical_groups_empty_map() {
        let map = HashMap::new();
        let interrupt = Arc::new(AtomicBool::new(false));
        let result = get_intersection_critical_groups(&map, &interrupt, 0).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn test_critical_groups_no_critical_fully_connected() {
        // 3 nodes, 2-of-3 quorum. Fully connected — no single node or group
        // is critical because any 2 nodes still intersect.
        let n1 = make_node_id(1);
        let n2 = make_node_id(2);
        let n3 = make_node_id(3);
        let all = vec![n1.clone(), n2.clone(), n3.clone()];

        let mut map = HashMap::new();
        map.insert(n1.clone(), Some(make_qset(all.clone(), 2)));
        map.insert(n2.clone(), Some(make_qset(all.clone(), 2)));
        map.insert(n3.clone(), Some(make_qset(all.clone(), 2)));

        let interrupt = Arc::new(AtomicBool::new(false));
        let result = get_intersection_critical_groups(&map, &interrupt, 0).unwrap();
        assert!(
            result.is_empty(),
            "Expected no critical groups in fully connected 2-of-3, got: {:?}",
            result
        );
    }

    #[test]
    fn test_critical_groups_bridge_node() {
        // Replicates stellar-core's "quorum intersection criticality" test.
        // 7 nodes with org3 (n3) as a bridge between two groups:
        //   - Group A: {n0, n1, n2} connected in a chain, all depending on n3
        //   - Group B: {n4, n5, n6} fully connected, n4 and n6 depending on n3
        //   - n3 depends on n0, n1, n2, n4, n6 (5 of 6 slots = bridge)
        //
        // The network enjoys intersection because n3 bridges both groups.
        // Making n3 fickle splits the network.

        let n0 = make_node_id(0);
        let n1 = make_node_id(1);
        let n2 = make_node_id(2);
        let n3 = make_node_id(3);
        let n4 = make_node_id(4);
        let n5 = make_node_id(5);
        let n6 = make_node_id(6);

        let inner = |node: &NodeId| -> ScpQuorumSet { make_qset(vec![node.clone()], 1) };

        // n0: needs n0 + n1 + n3 (threshold 3 of {n0, inner(n1), inner(n3)})
        let q0 = make_qset_with_inner(vec![n0.clone()], vec![inner(&n1), inner(&n3)], 3);
        // n1: needs n1 + 2 of {n0, n2, n3} (threshold 3 of 4 slots)
        let q1 = make_qset_with_inner(
            vec![n1.clone()],
            vec![inner(&n0), inner(&n2), inner(&n3)],
            3,
        );
        // n2: needs n2 + n1 + n3 (threshold 3 of 3)
        let q2 = make_qset_with_inner(vec![n2.clone()], vec![inner(&n1), inner(&n3)], 3);
        // n3: needs n3 + 4 of {n0, n1, n2, n4, n6} (threshold 5 of 6 slots)
        let q3 = make_qset_with_inner(
            vec![n3.clone()],
            vec![inner(&n0), inner(&n1), inner(&n2), inner(&n4), inner(&n6)],
            5,
        );
        // n4: needs n4 + 2 of {n3, n5, n6} (threshold 3 of 4)
        let q4 = make_qset_with_inner(
            vec![n4.clone()],
            vec![inner(&n3), inner(&n5), inner(&n6)],
            3,
        );
        // n5: needs n5 + n4 + n6 (threshold 3 of 3)
        let q5 = make_qset_with_inner(vec![n5.clone()], vec![inner(&n4), inner(&n6)], 3);
        // n6: needs n6 + 2 of {n3, n4, n5} (threshold 3 of 4)
        let q6 = make_qset_with_inner(
            vec![n6.clone()],
            vec![inner(&n3), inner(&n4), inner(&n5)],
            3,
        );

        let mut map = HashMap::new();
        map.insert(n0.clone(), Some(q0));
        map.insert(n1.clone(), Some(q1));
        map.insert(n2.clone(), Some(q2));
        map.insert(n3.clone(), Some(q3));
        map.insert(n4.clone(), Some(q4));
        map.insert(n5.clone(), Some(q5));
        map.insert(n6.clone(), Some(q6));

        // Verify intersection holds.
        assert!(matches!(
            check_intersection(&map),
            IntersectionResult::Intersects
        ));

        let interrupt = Arc::new(AtomicBool::new(false));
        let groups = get_intersection_critical_groups(&map, &interrupt, 0).unwrap();

        // n3 (the bridge) should be the only critical group (as a singleton).
        assert_eq!(
            groups.len(),
            1,
            "Expected exactly 1 critical group, got: {:?}",
            groups
        );
        assert_eq!(
            groups[0],
            vec![n3.clone()],
            "Expected n3 to be the critical group"
        );
    }

    #[test]
    fn test_critical_groups_interrupted() {
        let n1 = make_node_id(1);
        let n2 = make_node_id(2);
        let all = vec![n1.clone(), n2.clone()];

        let mut map = HashMap::new();
        map.insert(n1.clone(), Some(make_qset(all.clone(), 2)));
        map.insert(n2.clone(), Some(make_qset(all.clone(), 2)));

        // Set interrupt flag before calling.
        let interrupt = Arc::new(AtomicBool::new(true));
        let result = get_intersection_critical_groups(&map, &interrupt, 0);
        assert!(result.is_err());
        assert_eq!(result.unwrap_err(), Interrupted);
    }
}
