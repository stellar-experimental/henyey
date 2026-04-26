//! Quorum intersection checker using min-quorum enumeration.
//!
//! Matches stellar-core's `QuorumIntersectionCheckerImpl` (Lachowski, arXiv 1902.06493).
//! Uses SCC decomposition + recursive powerset search with aggressive pruning
//! to determine whether all quorums in a network intersect.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use stellar_xdr::curr::{NodeId, ScpQuorumSet};
use tracing::warn;

use henyey_common::xdr_to_bytes;

use super::bit_set::BitSet;
use super::qbitset::QBitSet;
use super::tarjan::TarjanSCCCalculator;

/// Maximum cached quorum results. Matches stellar-core's MAX_CACHED_QUORUMS_SIZE.
const MAX_CACHED_QUORUMS_SIZE: usize = 0xFFFF;

/// The main quorum intersection checker.
///
/// Built from a quorum map, this structure holds the bitset-based graph
/// representation, SCC decomposition, and all state needed to run the
/// min-quorum enumeration algorithm.
pub(crate) struct QuorumIntersectionChecker {
    /// QBitSet graph: one entry per node with a known qset.
    graph: Vec<QBitSet>,
    /// Maps bit index → NodeId.
    bit_num_pub_keys: Vec<NodeId>,
    /// Maps NodeId → bit index.
    pub_key_bit_nums: HashMap<NodeId, usize>,
    /// Strongly connected components.
    sccs: Vec<BitSet>,
    /// Interrupt flag for cancellation.
    interrupt: Arc<AtomicBool>,
    /// Checker-local PRNG for tie-breaking in pickSplitNode.
    rng: StdRng,
    /// Bounded quorum cache: BitSet → is_quorum.
    cached_quorums: HashMap<u64, bool>,
    cached_quorums_count: usize,
    /// Reusable buffer for in-degree computation.
    in_degrees: Vec<usize>,
    /// Found split witness (if any).
    potential_split: Option<(BitSet, BitSet)>,
}

/// Result of running the checker.
pub(crate) enum CheckerResult {
    /// All quorums intersect.
    Intersects,
    /// Found two disjoint quorums.
    Split { pair: (Vec<NodeId>, Vec<NodeId>) },
    /// Analysis was interrupted.
    Interrupted,
}

/// Exception-like signal for interrupt.
struct InterruptedException;

impl QuorumIntersectionChecker {
    /// Build a checker from a quorum map.
    ///
    /// Nodes with `None` qsets are treated as dead (no bit number assigned).
    /// The `seed` controls tie-breaking in split-node selection.
    pub fn new(
        quorum_map: &HashMap<NodeId, Option<ScpQuorumSet>>,
        interrupt: Arc<AtomicBool>,
        seed: u64,
    ) -> Self {
        let mut checker = Self {
            graph: Vec::new(),
            bit_num_pub_keys: Vec::new(),
            pub_key_bit_nums: HashMap::new(),
            sccs: Vec::new(),
            interrupt,
            rng: StdRng::seed_from_u64(seed),
            cached_quorums: HashMap::new(),
            cached_quorums_count: 0,
            in_degrees: Vec::new(),
            potential_split: None,
        };

        checker.build_graph(quorum_map);
        checker.build_sccs();
        checker
    }

    /// Run the intersection check.
    pub fn check(mut self) -> CheckerResult {
        match self.network_enjoys_quorum_intersection() {
            Ok(true) => CheckerResult::Intersects,
            Ok(false) => {
                let (a_bits, b_bits) = self.potential_split.take().unwrap();
                let a = self.bits_to_nodes(&a_bits);
                let b = self.bits_to_nodes(&b_bits);
                CheckerResult::Split { pair: (a, b) }
            }
            Err(InterruptedException) => CheckerResult::Interrupted,
        }
    }

    /// Stage 1: Build the bitset graph from the quorum map.
    ///
    /// Matches stellar-core's `buildGraph`: assigns bit numbers only to nodes
    /// with known qsets. Nodes with None qsets are dead — not assigned numbers,
    /// and references to them in qsets reduce the effective threshold.
    fn build_graph(&mut self, quorum_map: &HashMap<NodeId, Option<ScpQuorumSet>>) {
        // Sort nodes by XDR for deterministic bit number assignment.
        let mut ordered: Vec<_> = quorum_map.iter().collect();
        ordered.sort_by_key(|(id, _)| xdr_to_bytes(*id));

        // Pass 1: assign bit numbers to nodes with known qsets.
        for (node_id, qset_opt) in &ordered {
            if qset_opt.is_some() {
                let n = self.bit_num_pub_keys.len();
                self.pub_key_bit_nums.insert((*node_id).clone(), n);
                self.bit_num_pub_keys.push((*node_id).clone());
            }
        }

        // Pass 2: convert qsets to QBitSets.
        for (node_id, qset_opt) in &ordered {
            if let Some(qset) = qset_opt {
                debug_assert!(self.pub_key_bit_nums.contains_key(*node_id));
                let qb = self.convert_quorum_set(qset);
                self.graph.push(qb);
            }
        }

        self.in_degrees = vec![0; self.graph.len()];
    }

    /// Convert an ScpQuorumSet into a QBitSet.
    ///
    /// Nodes not in `pub_key_bit_nums` (dead nodes) are silently skipped,
    /// and the threshold is reduced accordingly — matching stellar-core's
    /// `convertSCPQuorumSet` logic.
    fn convert_quorum_set(&self, qset: &ScpQuorumSet) -> QBitSet {
        let mut threshold = qset.threshold;
        let mut nodes = BitSet::with_capacity(self.bit_num_pub_keys.len());
        let mut dead_count = 0u32;

        for v in qset.validators.iter() {
            if let Some(&bit) = self.pub_key_bit_nums.get(v) {
                nodes.set(bit);
            } else {
                dead_count += 1;
            }
        }

        let mut inner_sets = Vec::new();
        for inner in qset.inner_sets.iter() {
            let inner_qb = self.convert_quorum_set(inner);
            if inner_qb.is_empty() {
                dead_count += 1;
            } else {
                inner_sets.push(inner_qb);
            }
        }

        // Reduce threshold by dead count (saturating to avoid underflow).
        threshold = threshold.saturating_sub(dead_count);

        QBitSet::new(threshold, nodes, inner_sets)
    }

    /// Stage 2: Compute SCCs using Tarjan's algorithm.
    fn build_sccs(&mut self) {
        let graph = &self.graph;
        let calc = TarjanSCCCalculator::calculate(graph.len(), |i| graph[i].all_successors.clone());
        self.sccs = calc.sccs;
    }

    /// Main algorithm: check if network enjoys quorum intersection.
    ///
    /// Stage 1: SCC pre-filtering — if two SCCs each contain quorums,
    /// we have an immediate split.
    /// Stage 2: MinQuorumEnumerator on the single quorum-bearing SCC.
    fn network_enjoys_quorum_intersection(&mut self) -> Result<bool, InterruptedException> {
        let mut found_disjoint = false;
        let mut scan_scc = BitSet::new();

        for scc in &self.sccs {
            let q = self.contract_to_maximal_quorum(scc);
            if !q.empty() {
                if scan_scc.empty() {
                    scan_scc = scc.clone();
                } else {
                    // Two SCCs with quorums → immediate disjoint split.
                    found_disjoint = true;
                    let first_q = self.contract_to_maximal_quorum(&scan_scc);
                    self.note_found_disjoint_quorums(&first_q, &q);
                    break;
                }
            }
        }

        if scan_scc.empty() {
            // No quorums at all: vacuously enjoy intersection.
            warn!("No quorums found in any SCC (quorum intersection vacuously satisfied)");
            return Ok(true);
        }

        if !found_disjoint {
            // Stage 2: enumerate minimal quorums in the scan SCC.
            let committed = BitSet::new();
            let remaining = scan_scc.clone();
            found_disjoint =
                self.any_min_quorum_has_disjoint_quorum(committed, remaining, &scan_scc)?;
        }

        Ok(!found_disjoint)
    }

    /// Contract a set to its maximal quorum (greatest fixpoint).
    ///
    /// Repeatedly removes nodes whose quorum slice is not satisfied until
    /// reaching a fixpoint. Returns empty BitSet if no quorum exists.
    fn contract_to_maximal_quorum(&self, nodes: &BitSet) -> BitSet {
        let mut current = nodes.clone();
        loop {
            let mut filtered = current.clone();
            for i in current.iter_set() {
                if !self.contains_quorum_slice_for_node(&filtered, i) {
                    filtered.unset(i);
                }
            }

            if filtered.count() == current.count() || filtered.empty() {
                return filtered;
            }
            current = filtered;
        }
    }

    /// Check if `bs` contains a quorum slice for node `i`'s quorum set.
    fn contains_quorum_slice_for_node(&self, bs: &BitSet, node: usize) -> bool {
        if node >= self.graph.len() {
            return false;
        }
        self.contains_quorum_slice(bs, &self.graph[node])
    }

    /// Check if `bs` satisfies the quorum set `qbs`.
    ///
    /// Three-phase check matching stellar-core's `containsQuorumSlice`:
    /// 1. Count direct node intersections
    /// 2. Overapproximate with all_successors
    /// 3. Selectively test inner sets
    fn contains_quorum_slice(&self, bs: &BitSet, qbs: &QBitSet) -> bool {
        // Phase 1: direct nodes.
        let intersecting = bs.intersection_count(&qbs.nodes);
        if intersecting >= qbs.threshold as usize {
            return true;
        }

        let inner_threshold = qbs.threshold as usize - intersecting;

        // Not enough inner sets to possibly reach threshold.
        if inner_threshold > qbs.inner_sets.len() {
            return false;
        }

        // Phase 2: overapproximation with all_successors.
        if bs.intersection_count(&qbs.all_successors) < qbs.threshold as usize {
            return false;
        }

        // Phase 3: selective inner set testing.
        let mut remaining_needed = inner_threshold;
        let mut fail_limit = qbs.inner_sets.len() - inner_threshold + 1;

        for inner in &qbs.inner_sets {
            if self.contains_quorum_slice(bs, inner) {
                remaining_needed -= 1;
                if remaining_needed == 0 {
                    return true;
                }
            } else {
                fail_limit -= 1;
                if fail_limit == 0 {
                    return false;
                }
            }
        }
        false
    }

    /// Check if nodes form a quorum (with caching).
    fn is_a_quorum(&mut self, nodes: &BitSet) -> bool {
        // Check cache.
        let hash = self.hash_bitset(nodes);
        if let Some(&result) = self.cached_quorums.get(&hash) {
            return result;
        }

        let contracted = self.contract_to_maximal_quorum(nodes);
        let result = !contracted.empty();

        // Cache the result with bounded eviction.
        if self.cached_quorums_count >= MAX_CACHED_QUORUMS_SIZE {
            // Simple eviction: clear and start over.
            // stellar-core uses random eviction; clearing is simpler
            // and functionally equivalent for correctness.
            self.cached_quorums.clear();
            self.cached_quorums_count = 0;
        }
        self.cached_quorums.insert(hash, result);
        self.cached_quorums_count += 1;

        result
    }

    /// Hash a bitset for cache lookup (not cryptographic).
    fn hash_bitset(&self, bs: &BitSet) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        bs.hash(&mut hasher);
        hasher.finish()
    }

    /// Check if nodes form a minimal quorum (removing any one node breaks it).
    fn is_minimal_quorum(&mut self, nodes: &BitSet) -> bool {
        if nodes.empty() {
            return false;
        }

        let mut test = nodes.clone();
        for i in nodes.iter_set() {
            test.unset(i);
            if self.is_a_quorum(&test) {
                return false;
            }
            test.set(i);
        }
        true
    }

    /// Record a found pair of disjoint quorums.
    fn note_found_disjoint_quorums(&mut self, a: &BitSet, b: &BitSet) {
        self.potential_split = Some((a.clone(), b.clone()));
    }

    /// Convert a BitSet of node indices back to sorted NodeId vec.
    fn bits_to_nodes(&self, bits: &BitSet) -> Vec<NodeId> {
        let mut nodes: Vec<NodeId> = bits
            .iter_set()
            .filter_map(|i| self.bit_num_pub_keys.get(i).cloned())
            .collect();
        nodes.sort_by_key(|id| xdr_to_bytes(id));
        nodes
    }

    // --- MinQuorumEnumerator (inlined as methods) ---

    /// Pick the split node with highest in-degree (random tie-breaking).
    ///
    /// Matches stellar-core's `pickSplitNode`.
    fn pick_split_node(&mut self, remaining: &BitSet) -> usize {
        self.in_degrees.iter_mut().for_each(|d| *d = 0);

        let mut max_node = remaining.max();
        let mut max_count = 1usize;
        let mut max_degree = 0usize;

        for i in remaining.iter_set() {
            if i >= self.graph.len() {
                continue;
            }
            let avail = &self.graph[i].all_successors & remaining;
            for j in avail.iter_set() {
                if j >= self.in_degrees.len() {
                    continue;
                }
                self.in_degrees[j] += 1;
                let curr_degree = self.in_degrees[j];

                if curr_degree >= max_degree {
                    if curr_degree == max_degree {
                        max_count += 1;
                        // Keep existing max with probability (max_count-1)/max_count.
                        if self.rng.gen_range(0..max_count) == 0 {
                            continue;
                        }
                    } else {
                        max_count = 1;
                    }
                    max_degree = curr_degree;
                    max_node = j;
                }
            }
        }
        max_node
    }

    /// Check if the complement of `nodes` within `scan_scc` contains a quorum.
    fn has_disjoint_quorum(&mut self, nodes: &BitSet, scan_scc: &BitSet) -> bool {
        let complement = scan_scc - nodes;
        let disj = self.contract_to_maximal_quorum(&complement);
        if !disj.empty() {
            self.note_found_disjoint_quorums(nodes, &disj);
            return true;
        }
        false
    }

    /// Recursive min-quorum enumeration with early exits.
    ///
    /// Matches stellar-core's `MinQuorumEnumerator::anyMinQuorumHasDisjointQuorum`.
    fn any_min_quorum_has_disjoint_quorum(
        &mut self,
        committed: BitSet,
        remaining: BitSet,
        scan_scc: &BitSet,
    ) -> Result<bool, InterruptedException> {
        // Interrupt check.
        if self.interrupt.load(Ordering::Relaxed) {
            return Err(InterruptedException);
        }

        let max_commit = scan_scc.count() / 2;

        // Early exit 1: committed too large (symmetry).
        if committed.count() > max_commit {
            return Ok(false);
        }

        // Principal test: check if committed is already a quorum.
        let committed_quorum = self.contract_to_maximal_quorum(&committed);
        if !committed_quorum.empty() {
            if self.is_minimal_quorum(&committed_quorum) {
                // Early exit 3.1: minimal quorum found — check complement.
                return Ok(self.has_disjoint_quorum(&committed_quorum, scan_scc));
            }
            // Early exit 3.2: non-minimal quorum — skip extensions.
            return Ok(false);
        }

        // Early exit 2: check perimeter for extensibility.
        let perimeter = &committed | &remaining;
        let extension_quorum = self.contract_to_maximal_quorum(&perimeter);
        if !extension_quorum.empty() {
            if !committed.is_subset_eq(&extension_quorum) {
                // Early exit 2.2: extension doesn't extend committed.
                return Ok(false);
            }
        } else {
            // Early exit 2.1: no quorum in perimeter.
            return Ok(false);
        }

        // Termination: no remaining nodes.
        if remaining.empty() {
            return Ok(false);
        }

        // Recursion: split on chosen node.
        let mut remaining = remaining;
        let split = self.pick_split_node(&remaining);
        remaining.unset(split);

        // Branch 1: exclude split node.
        if self.any_min_quorum_has_disjoint_quorum(
            committed.clone(),
            remaining.clone(),
            scan_scc,
        )? {
            return Ok(true);
        }

        // Branch 2: include split node.
        let mut committed_with = committed;
        committed_with.set(split);
        self.any_min_quorum_has_disjoint_quorum(committed_with, remaining, scan_scc)
    }
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

    fn check_simple(quorum_map: &HashMap<NodeId, Option<ScpQuorumSet>>) -> CheckerResult {
        let interrupt = Arc::new(AtomicBool::new(false));
        let checker = QuorumIntersectionChecker::new(quorum_map, interrupt, 0);
        checker.check()
    }

    #[test]
    fn test_empty_map() {
        let map = HashMap::new();
        assert!(matches!(check_simple(&map), CheckerResult::Intersects));
    }

    #[test]
    fn test_single_node() {
        let n1 = make_node_id(1);
        let mut map = HashMap::new();
        map.insert(n1.clone(), Some(make_qset(vec![n1.clone()], 1)));
        assert!(matches!(check_simple(&map), CheckerResult::Intersects));
    }

    #[test]
    fn test_three_node_2_of_3() {
        let n1 = make_node_id(1);
        let n2 = make_node_id(2);
        let n3 = make_node_id(3);
        let all = vec![n1.clone(), n2.clone(), n3.clone()];

        let mut map = HashMap::new();
        map.insert(n1.clone(), Some(make_qset(all.clone(), 2)));
        map.insert(n2.clone(), Some(make_qset(all.clone(), 2)));
        map.insert(n3.clone(), Some(make_qset(all.clone(), 2)));

        assert!(matches!(check_simple(&map), CheckerResult::Intersects));
    }

    #[test]
    fn test_split_network() {
        let n1 = make_node_id(1);
        let n2 = make_node_id(2);
        let n3 = make_node_id(3);
        let n4 = make_node_id(4);

        let mut map = HashMap::new();
        map.insert(n1.clone(), Some(make_qset(vec![n1.clone(), n2.clone()], 1)));
        map.insert(n2.clone(), Some(make_qset(vec![n1.clone(), n2.clone()], 1)));
        map.insert(n3.clone(), Some(make_qset(vec![n3.clone(), n4.clone()], 1)));
        map.insert(n4.clone(), Some(make_qset(vec![n3.clone(), n4.clone()], 1)));

        match check_simple(&map) {
            CheckerResult::Split { pair: (a, b) } => {
                use std::collections::HashSet;
                let a_set: HashSet<_> = a.into_iter().collect();
                let b_set: HashSet<_> = b.into_iter().collect();
                assert!(a_set.is_disjoint(&b_set));
            }
            other => panic!("Expected Split, got {:?}", std::mem::discriminant(&other)),
        }
    }

    #[test]
    fn test_none_qset_nodes_treated_as_dead() {
        let n1 = make_node_id(1);
        let n2 = make_node_id(2);
        let n3 = make_node_id(3);

        let mut map = HashMap::new();
        map.insert(n1.clone(), Some(make_qset(vec![n1.clone(), n2.clone()], 2)));
        map.insert(n2.clone(), Some(make_qset(vec![n1.clone(), n2.clone()], 2)));
        map.insert(n3.clone(), None);

        assert!(matches!(check_simple(&map), CheckerResult::Intersects));
    }

    #[test]
    fn test_no_quorum_vacuous() {
        // All nodes require all others, but threshold is impossibly high.
        let n1 = make_node_id(1);
        let n2 = make_node_id(2);

        let mut map = HashMap::new();
        map.insert(
            n1.clone(),
            Some(make_qset(vec![n1.clone(), n2.clone()], 10)),
        );
        map.insert(
            n2.clone(),
            Some(make_qset(vec![n1.clone(), n2.clone()], 10)),
        );

        // No quorums → vacuous intersection.
        assert!(matches!(check_simple(&map), CheckerResult::Intersects));
    }

    #[test]
    fn test_interrupt() {
        let n1 = make_node_id(1);
        let mut map = HashMap::new();
        map.insert(n1.clone(), Some(make_qset(vec![n1.clone()], 1)));

        let interrupt = Arc::new(AtomicBool::new(true));
        let checker = QuorumIntersectionChecker::new(&map, interrupt, 0);
        assert!(matches!(checker.check(), CheckerResult::Interrupted));
    }

    #[test]
    fn test_determinism_with_seed() {
        // Same input + same seed → same result.
        let n1 = make_node_id(1);
        let n2 = make_node_id(2);
        let n3 = make_node_id(3);
        let n4 = make_node_id(4);

        let mut map = HashMap::new();
        map.insert(n1.clone(), Some(make_qset(vec![n1.clone(), n2.clone()], 1)));
        map.insert(n2.clone(), Some(make_qset(vec![n1.clone(), n2.clone()], 1)));
        map.insert(n3.clone(), Some(make_qset(vec![n3.clone(), n4.clone()], 1)));
        map.insert(n4.clone(), Some(make_qset(vec![n3.clone(), n4.clone()], 1)));

        let interrupt1 = Arc::new(AtomicBool::new(false));
        let checker1 = QuorumIntersectionChecker::new(&map, interrupt1, 42);
        let result1 = checker1.check();

        let interrupt2 = Arc::new(AtomicBool::new(false));
        let checker2 = QuorumIntersectionChecker::new(&map, interrupt2, 42);
        let result2 = checker2.check();

        match (result1, result2) {
            (CheckerResult::Split { pair: (a1, b1) }, CheckerResult::Split { pair: (a2, b2) }) => {
                assert_eq!(a1, a2);
                assert_eq!(b1, b2);
            }
            (CheckerResult::Intersects, CheckerResult::Intersects) => {}
            _ => panic!("Determinism violated: different result types"),
        }
    }

    #[test]
    fn test_large_intersecting_network() {
        // 25 nodes, all with 2/3 of all threshold — should still intersect.
        let nodes: Vec<NodeId> = (0..25).map(make_node_id).collect();
        let threshold = (25 * 2 / 3) + 1; // ~17

        let mut map = HashMap::new();
        for n in &nodes {
            map.insert(n.clone(), Some(make_qset(nodes.clone(), threshold)));
        }

        assert!(matches!(check_simple(&map), CheckerResult::Intersects));
    }

    #[test]
    fn test_large_split_network() {
        // 30 nodes: two groups of 15, each with 1-of-own-group threshold.
        let group_a: Vec<NodeId> = (0..15).map(make_node_id).collect();
        let group_b: Vec<NodeId> = (15..30).map(make_node_id).collect();

        let mut map = HashMap::new();
        for n in &group_a {
            map.insert(n.clone(), Some(make_qset(group_a.clone(), 1)));
        }
        for n in &group_b {
            map.insert(n.clone(), Some(make_qset(group_b.clone(), 1)));
        }

        match check_simple(&map) {
            CheckerResult::Split { pair: (a, b) } => {
                use std::collections::HashSet;
                let a_set: HashSet<_> = a.into_iter().collect();
                let b_set: HashSet<_> = b.into_iter().collect();
                assert!(a_set.is_disjoint(&b_set));
            }
            _ => panic!("Expected Split for disjoint groups"),
        }
    }
}
