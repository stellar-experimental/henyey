//! Tarjan's Strongly Connected Components algorithm.
//!
//! Matches stellar-core's `TarjanSCCCalculator` (TarjanSCCCalculator.h/cpp).
//! Used to decompose the quorum dependency graph into SCCs for efficient
//! quorum intersection analysis.

use super::bit_set::BitSet;

/// Tarjan SCC calculator.
///
/// Computes strongly connected components of a directed graph where
/// edges are represented as BitSet successors per node.
pub(crate) struct TarjanSCCCalculator {
    nodes: Vec<SCCNode>,
    stack: Vec<usize>,
    index: i64,
    /// Computed SCCs, each as a BitSet of node indices.
    pub sccs: Vec<BitSet>,
}

struct SCCNode {
    index: i64,
    low_link: i64,
    on_stack: bool,
}

impl SCCNode {
    fn new() -> Self {
        Self {
            index: -1,
            low_link: -1,
            on_stack: false,
        }
    }
}

impl TarjanSCCCalculator {
    /// Compute SCCs for a graph of `num_nodes` nodes.
    ///
    /// `successors` returns the successor BitSet for a given node index.
    /// Matches stellar-core's `calculateSCCs(graphSize, getNodeSuccessors)`.
    pub fn calculate<F>(num_nodes: usize, successors: F) -> Self
    where
        F: Fn(usize) -> BitSet,
    {
        let mut calc = Self {
            nodes: (0..num_nodes).map(|_| SCCNode::new()).collect(),
            stack: Vec::new(),
            index: 0,
            sccs: Vec::new(),
        };

        for i in 0..num_nodes {
            if calc.nodes[i].index == -1 {
                calc.scc(i, &successors);
            }
        }

        calc
    }

    fn scc<F>(&mut self, i: usize, successors: &F)
    where
        F: Fn(usize) -> BitSet,
    {
        self.nodes[i].index = self.index;
        self.nodes[i].low_link = self.index;
        self.index += 1;
        self.stack.push(i);
        self.nodes[i].on_stack = true;

        let succ = successors(i);
        for j in succ.iter_set() {
            if self.nodes[j].index == -1 {
                self.scc(j, successors);
                let w_low = self.nodes[j].low_link;
                self.nodes[i].low_link = self.nodes[i].low_link.min(w_low);
            } else if self.nodes[j].on_stack {
                let w_index = self.nodes[j].index;
                self.nodes[i].low_link = self.nodes[i].low_link.min(w_index);
            }
        }

        if self.nodes[i].low_link == self.nodes[i].index {
            let mut scc = BitSet::new();
            loop {
                let j = self.stack.pop().unwrap();
                self.nodes[j].on_stack = false;
                scc.set(j);
                if j == i {
                    break;
                }
            }
            self.sccs.push(scc);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_single_node_self_loop() {
        let calc = TarjanSCCCalculator::calculate(1, |_| {
            let mut bs = BitSet::new();
            bs.set(0);
            bs
        });
        assert_eq!(calc.sccs.len(), 1);
        assert!(calc.sccs[0].get(0));
    }

    #[test]
    fn test_single_node_no_edges() {
        let calc = TarjanSCCCalculator::calculate(1, |_| BitSet::new());
        assert_eq!(calc.sccs.len(), 1);
        assert!(calc.sccs[0].get(0));
    }

    #[test]
    fn test_two_node_cycle() {
        // 0 → 1, 1 → 0: one SCC
        let calc = TarjanSCCCalculator::calculate(2, |i| {
            let mut bs = BitSet::new();
            bs.set(1 - i);
            bs
        });
        assert_eq!(calc.sccs.len(), 1);
        assert!(calc.sccs[0].get(0));
        assert!(calc.sccs[0].get(1));
    }

    #[test]
    fn test_two_separate_nodes() {
        // 0 and 1 with no edges: two SCCs
        let calc = TarjanSCCCalculator::calculate(2, |_| BitSet::new());
        assert_eq!(calc.sccs.len(), 2);
    }

    #[test]
    fn test_chain_three_nodes() {
        // 0 → 1 → 2: three SCCs (no back edges)
        let calc = TarjanSCCCalculator::calculate(3, |i| {
            let mut bs = BitSet::new();
            if i < 2 {
                bs.set(i + 1);
            }
            bs
        });
        assert_eq!(calc.sccs.len(), 3);
    }

    #[test]
    fn test_cycle_three_nodes() {
        // 0 → 1 → 2 → 0: one SCC
        let calc = TarjanSCCCalculator::calculate(3, |i| {
            let mut bs = BitSet::new();
            bs.set((i + 1) % 3);
            bs
        });
        assert_eq!(calc.sccs.len(), 1);
        assert_eq!(calc.sccs[0].count(), 3);
    }

    #[test]
    fn test_two_sccs_with_bridge() {
        // SCC1: {0, 1} (cycle), SCC2: {2, 3} (cycle), bridge: 1 → 2
        let calc = TarjanSCCCalculator::calculate(4, |i| {
            let mut bs = BitSet::new();
            match i {
                0 => bs.set(1),
                1 => {
                    bs.set(0);
                    bs.set(2);
                }
                2 => bs.set(3),
                3 => bs.set(2),
                _ => {}
            }
            bs
        });
        assert_eq!(calc.sccs.len(), 2);
        // Both SCCs should have 2 nodes each
        let sizes: Vec<usize> = calc.sccs.iter().map(|s| s.count()).collect();
        assert!(sizes.contains(&2));
    }

    #[test]
    fn test_empty_graph() {
        let calc = TarjanSCCCalculator::calculate(0, |_| BitSet::new());
        assert!(calc.sccs.is_empty());
    }

    #[test]
    fn test_five_nodes_complex() {
        // 0 → 1, 1 → 2, 2 → 0 (SCC {0,1,2})
        // 3 → 4, 4 → 3 (SCC {3,4})
        // 2 → 3 (bridge)
        let calc = TarjanSCCCalculator::calculate(5, |i| {
            let mut bs = BitSet::new();
            match i {
                0 => bs.set(1),
                1 => bs.set(2),
                2 => {
                    bs.set(0);
                    bs.set(3);
                }
                3 => bs.set(4),
                4 => bs.set(3),
                _ => {}
            }
            bs
        });
        assert_eq!(calc.sccs.len(), 2);
        let mut scc_sizes: Vec<usize> = calc.sccs.iter().map(|s| s.count()).collect();
        scc_sizes.sort();
        assert_eq!(scc_sizes, vec![2, 3]);
    }
}
