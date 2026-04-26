//! Quorum intersection analysis state for the herder.
//!
//! Tracks the result of periodic quorum intersection checks, matching
//! stellar-core's `QuorumMapIntersectionState` (QuorumIntersectionChecker.h).
//!
//! The state separates completed results from in-flight analysis so that
//! `/info` continues serving previous results while a new check runs.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use henyey_common::Hash256;
use stellar_xdr::curr::NodeId;

/// Result of a completed quorum intersection analysis.
#[derive(Debug, Clone)]
pub(crate) enum QuorumIntersectionResult {
    /// Network enjoys quorum intersection.
    Intersecting {
        /// Ledger at which this check was performed.
        check_ledger: u32,
        /// Number of nodes in the quorum map (including unknown-qset nodes).
        num_nodes: usize,
        /// Hash of the quorum map that was analyzed.
        quorum_map_hash: Hash256,
        /// Intersection-critical node groups.
        ///
        /// Each inner Vec is a group of nodes whose removal would break
        /// quorum intersection. Empty if no critical groups found.
        critical_groups: Vec<Vec<NodeId>>,
    },
    /// Network does NOT enjoy quorum intersection.
    Split {
        /// Ledger at which this check was performed.
        check_ledger: u32,
        /// Number of nodes in the quorum map (including unknown-qset nodes).
        num_nodes: usize,
        /// Hash of the quorum map that was analyzed.
        quorum_map_hash: Hash256,
        /// A pair of non-intersecting quorums (sorted by NodeId XDR).
        potential_split: (Vec<NodeId>, Vec<NodeId>),
    },
}

/// In-flight analysis state as an explicit enum to prevent invalid states.
///
/// Matches stellar-core's interrupt-and-return model.
#[derive(Debug)]
enum AnalysisState {
    /// No analysis in progress.
    Idle,
    /// Analysis is running for the given quorum map hash.
    Analyzing {
        hash: Hash256,
        interrupt_flag: Arc<AtomicBool>,
    },
}

/// Quorum intersection analysis state.
///
/// Tracks both the last completed result and any in-flight analysis,
/// matching stellar-core's `QuorumMapIntersectionState` semantics.
#[derive(Debug)]
pub(crate) struct QuorumIntersectionState {
    /// Last completed analysis result. `None` until first check completes.
    last_result: Option<QuorumIntersectionResult>,
    /// The ledger of the most recent check that found intersection.
    /// Matches stellar-core's `mLastGoodLedger` — 0 until first intersecting result.
    last_good_ledger: u32,
    /// In-flight analysis state.
    analysis: AnalysisState,
}

impl QuorumIntersectionState {
    /// Create a new empty state (no analysis performed yet).
    pub fn new() -> Self {
        Self {
            last_result: None,
            last_good_ledger: 0,
            analysis: AnalysisState::Idle,
        }
    }

    /// Whether any publishable results exist.
    ///
    /// Matches stellar-core's `hasAnyResults()` which returns
    /// `mLastGoodLedger != 0`. This means a first-ever split result
    /// is NOT published — matching stellar-core's behavior.
    pub fn has_any_results(&self) -> bool {
        self.last_good_ledger != 0
    }

    /// Whether the network currently enjoys quorum intersection.
    ///
    /// Only meaningful when `has_any_results()` is true.
    pub fn enjoys_quorum_intersection(&self) -> bool {
        match &self.last_result {
            Some(QuorumIntersectionResult::Intersecting { check_ledger, .. }) => {
                *check_ledger == self.last_good_ledger
            }
            _ => false,
        }
    }

    /// Get the last completed result, if any.
    pub fn last_result(&self) -> Option<&QuorumIntersectionResult> {
        self.last_result.as_ref()
    }

    /// Get the last good ledger (ledger of most recent intersecting check).
    pub fn last_good_ledger(&self) -> u32 {
        self.last_good_ledger
    }

    /// Get the hash of the quorum map currently being analyzed.
    pub fn checking_hash(&self) -> Option<&Hash256> {
        match &self.analysis {
            AnalysisState::Idle => None,
            AnalysisState::Analyzing { hash, .. } => Some(hash),
        }
    }

    /// Whether analysis is currently in progress.
    pub fn is_analyzing(&self) -> bool {
        matches!(self.analysis, AnalysisState::Analyzing { .. })
    }

    /// Get the hash from the last completed result.
    pub fn last_result_hash(&self) -> Option<&Hash256> {
        match &self.last_result {
            Some(QuorumIntersectionResult::Intersecting {
                quorum_map_hash, ..
            }) => Some(quorum_map_hash),
            Some(QuorumIntersectionResult::Split {
                quorum_map_hash, ..
            }) => Some(quorum_map_hash),
            None => None,
        }
    }

    /// Start analysis for the given quorum map hash.
    ///
    /// Returns a clone of the interrupt flag for the background task.
    /// If analysis is already in progress for a different hash, the old
    /// analysis is interrupted first (matching stellar-core's interrupt-and-return).
    pub fn start_checking(&mut self, hash: Hash256) -> Arc<AtomicBool> {
        // If already analyzing a different hash, interrupt it.
        if let AnalysisState::Analyzing {
            interrupt_flag,
            hash: old_hash,
        } = &self.analysis
        {
            if *old_hash != hash {
                interrupt_flag.store(true, Ordering::Relaxed);
            }
        }

        let flag = Arc::new(AtomicBool::new(false));
        let flag_clone = Arc::clone(&flag);
        self.analysis = AnalysisState::Analyzing {
            hash,
            interrupt_flag: flag,
        };
        flag_clone
    }

    /// Set the interrupt flag on any in-flight analysis.
    ///
    /// Used when the quorum map changes during analysis to signal
    /// the running checker to abort.
    pub fn interrupt_stale(&mut self) {
        if let AnalysisState::Analyzing { interrupt_flag, .. } = &self.analysis {
            interrupt_flag.store(true, Ordering::Relaxed);
        }
    }

    /// Clear the in-progress analysis marker without publishing a result.
    ///
    /// Used when the analysis is interrupted or cannot complete.
    pub fn clear_checking(&mut self) {
        self.analysis = AnalysisState::Idle;
    }

    /// Record a completed analysis result.
    ///
    /// Only publishes if the `expected_hash` matches the current `checking_hash`
    /// (i.e., the quorum map hasn't changed since analysis started). This
    /// prevents stale results from being published.
    ///
    /// Returns `true` if the result was published, `false` if stale.
    pub fn complete_check(
        &mut self,
        expected_hash: &Hash256,
        result: QuorumIntersectionResult,
    ) -> bool {
        if self.checking_hash() != Some(expected_hash) {
            // Quorum map changed during analysis; discard stale result.
            return false;
        }
        self.analysis = AnalysisState::Idle;

        if let QuorumIntersectionResult::Intersecting { check_ledger, .. } = &result {
            self.last_good_ledger = *check_ledger;
        }

        self.last_result = Some(result);
        true
    }
}

impl Default for QuorumIntersectionState {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use henyey_common::Hash256;

    fn make_hash(byte: u8) -> Hash256 {
        Hash256::from([byte; 32])
    }

    #[test]
    fn test_initial_state() {
        let state = QuorumIntersectionState::new();
        assert!(!state.has_any_results());
        assert!(!state.enjoys_quorum_intersection());
        assert!(state.last_result().is_none());
        assert!(state.checking_hash().is_none());
        assert_eq!(state.last_good_ledger(), 0);
    }

    #[test]
    fn test_intersecting_result_published() {
        let mut state = QuorumIntersectionState::new();
        let hash = make_hash(1);

        state.start_checking(hash);
        assert!(state.checking_hash().is_some());

        let result = QuorumIntersectionResult::Intersecting {
            check_ledger: 100,
            num_nodes: 5,
            quorum_map_hash: hash,
            critical_groups: vec![],
        };
        assert!(state.complete_check(&hash, result));

        assert!(state.has_any_results());
        assert!(state.enjoys_quorum_intersection());
        assert_eq!(state.last_good_ledger(), 100);
        assert!(state.checking_hash().is_none());
    }

    #[test]
    fn test_first_split_not_published() {
        // First-ever check finds split → has_any_results() stays false.
        let mut state = QuorumIntersectionState::new();
        let hash = make_hash(1);

        state.start_checking(hash);
        let result = QuorumIntersectionResult::Split {
            check_ledger: 100,
            num_nodes: 4,
            quorum_map_hash: hash,
            potential_split: (vec![], vec![]),
        };
        assert!(state.complete_check(&hash, result));

        // last_good_ledger is still 0, so has_any_results() returns false.
        assert!(!state.has_any_results());
        assert!(!state.enjoys_quorum_intersection());
    }

    #[test]
    fn test_split_after_intersecting() {
        // First check: intersecting. Second check: split.
        // has_any_results() should still return true.
        let mut state = QuorumIntersectionState::new();

        // First: intersecting
        let hash1 = make_hash(1);
        state.start_checking(hash1);
        state.complete_check(
            &hash1,
            QuorumIntersectionResult::Intersecting {
                check_ledger: 100,
                num_nodes: 5,
                quorum_map_hash: hash1,
                critical_groups: vec![],
            },
        );
        assert!(state.has_any_results());
        assert!(state.enjoys_quorum_intersection());
        assert_eq!(state.last_good_ledger(), 100);

        // Second: split
        let hash2 = make_hash(2);
        state.start_checking(hash2);
        state.complete_check(
            &hash2,
            QuorumIntersectionResult::Split {
                check_ledger: 200,
                num_nodes: 4,
                quorum_map_hash: hash2,
                potential_split: (vec![], vec![]),
            },
        );

        // last_good_ledger is still 100 (from the intersecting check).
        assert!(state.has_any_results());
        assert!(!state.enjoys_quorum_intersection());
        assert_eq!(state.last_good_ledger(), 100);
    }

    #[test]
    fn test_stale_result_discarded() {
        let mut state = QuorumIntersectionState::new();

        let hash1 = make_hash(1);
        let hash2 = make_hash(2);

        // Start checking hash1.
        state.start_checking(hash1);

        // Before hash1 completes, a new check starts for hash2.
        state.start_checking(hash2);

        // hash1's result arrives — should be discarded (stale).
        let result = QuorumIntersectionResult::Intersecting {
            check_ledger: 100,
            num_nodes: 5,
            quorum_map_hash: hash1,
            critical_groups: vec![],
        };
        assert!(!state.complete_check(&hash1, result));

        // No results published.
        assert!(!state.has_any_results());

        // hash2's result arrives — should be published.
        let result2 = QuorumIntersectionResult::Intersecting {
            check_ledger: 101,
            num_nodes: 5,
            quorum_map_hash: hash2,
            critical_groups: vec![],
        };
        assert!(state.complete_check(&hash2, result2));
        assert!(state.has_any_results());
    }

    #[test]
    fn test_result_retained_during_recalculation() {
        let mut state = QuorumIntersectionState::new();

        // Publish an intersecting result.
        let hash1 = make_hash(1);
        state.start_checking(hash1);
        state.complete_check(
            &hash1,
            QuorumIntersectionResult::Intersecting {
                check_ledger: 100,
                num_nodes: 5,
                quorum_map_hash: hash1,
                critical_groups: vec![],
            },
        );

        // Start a new check — previous result should still be available.
        let hash2 = make_hash(2);
        state.start_checking(hash2);

        assert!(state.has_any_results());
        assert!(state.enjoys_quorum_intersection());
        assert_eq!(state.last_good_ledger(), 100);
    }

    #[test]
    fn test_clear_checking_unblocks_future_checks() {
        let mut state = QuorumIntersectionState::new();
        let hash1 = make_hash(1);

        // Publish an intersecting result.
        state.start_checking(hash1);
        state.complete_check(
            &hash1,
            QuorumIntersectionResult::Intersecting {
                check_ledger: 50,
                num_nodes: 3,
                quorum_map_hash: hash1,
                critical_groups: vec![],
            },
        );

        // Start a new check, then clear it (simulates TooLarge).
        let hash2 = make_hash(2);
        state.start_checking(hash2);
        assert!(state.checking_hash().is_some());

        state.clear_checking();
        assert!(state.checking_hash().is_none());

        // Previous result should still be available.
        assert!(state.has_any_results());
        assert!(state.enjoys_quorum_intersection());
        assert_eq!(state.last_good_ledger(), 50);
    }

    #[test]
    fn test_interrupt_stale_analysis() {
        let mut state = QuorumIntersectionState::new();
        let hash1 = make_hash(1);

        // Start analysis.
        let flag = state.start_checking(hash1);
        assert!(!flag.load(Ordering::Relaxed));

        // Interrupt it.
        state.interrupt_stale();
        assert!(flag.load(Ordering::Relaxed));
    }

    #[test]
    fn test_start_checking_interrupts_old_analysis() {
        let mut state = QuorumIntersectionState::new();
        let hash1 = make_hash(1);
        let hash2 = make_hash(2);

        // Start analysis for hash1.
        let flag1 = state.start_checking(hash1);
        assert!(!flag1.load(Ordering::Relaxed));

        // Start analysis for hash2 — should interrupt hash1.
        let _flag2 = state.start_checking(hash2);
        assert!(
            flag1.load(Ordering::Relaxed),
            "old analysis should be interrupted"
        );
    }

    #[test]
    fn test_is_analyzing() {
        let mut state = QuorumIntersectionState::new();
        assert!(!state.is_analyzing());

        state.start_checking(make_hash(1));
        assert!(state.is_analyzing());

        state.clear_checking();
        assert!(!state.is_analyzing());
    }
}
