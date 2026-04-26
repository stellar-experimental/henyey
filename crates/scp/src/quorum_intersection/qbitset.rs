//! Compact quorum set representation using bitsets.
//!
//! Matches stellar-core's `QBitSet` (QuorumIntersectionCheckerImpl.h).
//! Converts `ScpQuorumSet` into a bitset-based form for efficient
//! quorum slice checking during intersection analysis.

use super::bit_set::BitSet;

/// Compact quorum set: threshold over direct nodes + inner sets.
///
/// All node references are bit indices (not NodeIds). The `all_successors`
/// field is the precomputed union of `nodes` and all recursive successors
/// from `inner_sets`, enabling fast overapproximation checks.
#[derive(Debug, Clone)]
pub(crate) struct QBitSet {
    /// Minimum number of satisfying entries (validators + inner sets).
    pub threshold: u32,
    /// Direct validator nodes in this quorum set (as bit indices).
    pub nodes: BitSet,
    /// Nested inner quorum sets.
    pub inner_sets: Vec<QBitSet>,
    /// Union of `nodes` and all recursive successors from `inner_sets`.
    /// Used for fast overapproximation in `contains_quorum_slice`.
    pub all_successors: BitSet,
}

impl QBitSet {
    /// Create a new QBitSet with precomputed successors.
    pub fn new(threshold: u32, nodes: BitSet, inner_sets: Vec<QBitSet>) -> Self {
        let all_successors = Self::compute_successors(&nodes, &inner_sets);
        Self {
            threshold,
            nodes,
            inner_sets,
            all_successors,
        }
    }

    /// Create an empty QBitSet (no threshold, no members).
    #[cfg(test)]
    pub fn empty() -> Self {
        Self {
            threshold: 0,
            nodes: BitSet::new(),
            inner_sets: Vec::new(),
            all_successors: BitSet::new(),
        }
    }

    /// Whether this quorum set is empty (threshold 0 and no successors).
    pub fn is_empty(&self) -> bool {
        self.threshold == 0 && self.all_successors.empty()
    }

    /// Compute the union of direct nodes and all inner set successors.
    fn compute_successors(nodes: &BitSet, inner_sets: &[QBitSet]) -> BitSet {
        let mut result = nodes.clone();
        for inner in inner_sets {
            result |= &inner.all_successors;
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_empty_qbitset() {
        let qbs = QBitSet::empty();
        assert!(qbs.is_empty());
        assert_eq!(qbs.threshold, 0);
        assert!(qbs.nodes.empty());
        assert!(qbs.all_successors.empty());
    }

    #[test]
    fn test_simple_qbitset() {
        let mut nodes = BitSet::new();
        nodes.set(0);
        nodes.set(1);
        nodes.set(2);

        let qbs = QBitSet::new(2, nodes, Vec::new());
        assert!(!qbs.is_empty());
        assert_eq!(qbs.threshold, 2);
        assert!(qbs.all_successors.get(0));
        assert!(qbs.all_successors.get(1));
        assert!(qbs.all_successors.get(2));
        assert_eq!(qbs.all_successors.count(), 3);
    }

    #[test]
    fn test_nested_qbitset_successors() {
        let mut inner_nodes = BitSet::new();
        inner_nodes.set(2);
        inner_nodes.set(3);
        let inner = QBitSet::new(1, inner_nodes, Vec::new());

        let mut outer_nodes = BitSet::new();
        outer_nodes.set(0);
        outer_nodes.set(1);
        let outer = QBitSet::new(2, outer_nodes, vec![inner]);

        // all_successors should include 0, 1, 2, 3
        assert_eq!(outer.all_successors.count(), 4);
        for i in 0..4 {
            assert!(outer.all_successors.get(i));
        }
    }

    #[test]
    fn test_deeply_nested_successors() {
        let mut deep_nodes = BitSet::new();
        deep_nodes.set(4);
        let deep = QBitSet::new(1, deep_nodes, Vec::new());

        let mut mid_nodes = BitSet::new();
        mid_nodes.set(2);
        mid_nodes.set(3);
        let mid = QBitSet::new(1, mid_nodes, vec![deep]);

        let mut top_nodes = BitSet::new();
        top_nodes.set(0);
        top_nodes.set(1);
        let top = QBitSet::new(2, top_nodes, vec![mid]);

        assert_eq!(top.all_successors.count(), 5);
        for i in 0..5 {
            assert!(top.all_successors.get(i));
        }
    }
}
