//! Dead node detection for monitoring network participation.
//!
//! This module tracks nodes that have not participated in SCP consensus over time,
//! providing diagnostic information about potentially offline validators.
//!
//! # How It Works
//!
//! Dead node detection uses a two-interval approach:
//!
//! 1. **Missing nodes**: Nodes that haven't been seen in the current interval
//! 2. **Dead nodes**: Nodes that were missing for two consecutive intervals
//!
//! Every `CHECK_FOR_DEAD_NODES_MINUTES` (15 minutes), the tracker:
//! 1. Moves `missing_nodes` to `dead_nodes`
//! 2. Resets `missing_nodes` with all transitive quorum members
//! 3. As nodes send SCP messages, they are removed from `missing_nodes`
//!
//! This ensures a node must be silent for at least 15-30 minutes before being
//! considered potentially dead.
//!
//! # Usage
//!
//! ```ignore
//! use henyey_herder::dead_node_tracker::DeadNodeTracker;
//!
//! let mut tracker = DeadNodeTracker::new();
//!
//! // Initialize with all transitive quorum members
//! tracker.reset_missing_nodes(&transitive_quorum_members);
//!
//! // When we receive an SCP envelope
//! tracker.record_node_activity(&node_id);
//!
//! // Every 15 minutes
//! tracker.check_interval(&transitive_quorum_members);
//!
//! // Get potentially dead nodes for diagnostics
//! let dead = tracker.get_maybe_dead_nodes();
//! ```

use std::collections::HashSet;
use std::time::{Duration, Instant};

use stellar_xdr::curr::NodeId;

/// Interval for checking dead nodes (15 minutes, matching stellar-core).
pub const CHECK_FOR_DEAD_NODES_MINUTES: u64 = 15;

/// Tracker for detecting nodes that haven't participated in consensus.
///
/// This is a diagnostic tool that helps identify validators that may be
/// offline or having connectivity issues.
#[derive(Debug)]
pub struct DeadNodeTracker {
    /// Nodes that haven't been seen in the current interval.
    /// These are candidates for becoming "dead" if still silent next interval.
    missing_nodes: HashSet<NodeId>,

    /// Nodes that were missing for two consecutive intervals.
    /// These are likely offline or unreachable.
    dead_nodes: HashSet<NodeId>,

    /// When the last check interval started.
    last_check: Instant,

    /// Duration of each check interval.
    check_interval: Duration,
}

impl Default for DeadNodeTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl DeadNodeTracker {
    /// Create a new dead node tracker with the default check interval.
    pub fn new() -> Self {
        Self::with_interval(Duration::from_secs(CHECK_FOR_DEAD_NODES_MINUTES * 60))
    }

    /// Create a new dead node tracker with a custom check interval.
    ///
    /// This is useful for testing with shorter intervals.
    pub fn with_interval(check_interval: Duration) -> Self {
        Self {
            missing_nodes: HashSet::new(),
            dead_nodes: HashSet::new(),
            last_check: Instant::now(),
            check_interval,
        }
    }

    /// Reset the missing nodes set with all transitive quorum members.
    ///
    /// Call this when starting tracking or when the quorum configuration changes.
    pub fn reset_missing_nodes<'a>(
        &mut self,
        transitive_quorum: impl IntoIterator<Item = &'a NodeId>,
    ) {
        self.missing_nodes.clear();
        self.missing_nodes
            .extend(transitive_quorum.into_iter().cloned());
    }

    /// Record that a node has participated in SCP.
    ///
    /// Call this when receiving any SCP envelope from a node.
    /// The node will be removed from the missing set.
    pub fn record_node_activity(&mut self, node_id: &NodeId) {
        self.missing_nodes.remove(node_id);
    }

    /// Check if it's time to run the dead node check interval.
    pub fn should_check(&self) -> bool {
        self.last_check.elapsed() >= self.check_interval
    }

    /// Perform the dead node check interval.
    ///
    /// This moves missing nodes to dead nodes and resets the missing set
    /// with all transitive quorum members.
    ///
    /// Call this every `CHECK_FOR_DEAD_NODES_MINUTES` (15 minutes).
    ///
    /// # Arguments
    ///
    /// * `transitive_quorum` - All nodes in the transitive quorum
    pub fn check_interval<'a>(&mut self, transitive_quorum: impl IntoIterator<Item = &'a NodeId>) {
        // Nodes that were missing before and are still missing are now dead
        self.dead_nodes = std::mem::take(&mut self.missing_nodes);

        // Reset missing nodes to all transitive quorum members
        self.missing_nodes
            .extend(transitive_quorum.into_iter().cloned());

        self.last_check = Instant::now();

        tracing::info!(
            dead_count = self.dead_nodes.len(),
            missing_count = self.missing_nodes.len(),
            "Dead node check interval completed"
        );
    }

    /// Get the set of nodes that may be dead (missing for two intervals).
    pub fn get_maybe_dead_nodes(&self) -> &HashSet<NodeId> {
        &self.dead_nodes
    }

    /// Get the set of nodes currently missing (not seen in current interval).
    pub fn get_missing_nodes(&self) -> &HashSet<NodeId> {
        &self.missing_nodes
    }

    /// Check if a specific node is potentially dead.
    pub fn is_maybe_dead(&self, node_id: &NodeId) -> bool {
        self.dead_nodes.contains(node_id)
    }

    /// Check if a specific node is currently missing.
    pub fn is_missing(&self, node_id: &NodeId) -> bool {
        self.missing_nodes.contains(node_id)
    }

    /// Get the number of potentially dead nodes.
    pub fn dead_count(&self) -> usize {
        self.dead_nodes.len()
    }

    /// Get the number of currently missing nodes.
    pub fn missing_count(&self) -> usize {
        self.missing_nodes.len()
    }

    /// Clear all tracking state.
    pub fn clear(&mut self) {
        self.missing_nodes.clear();
        self.dead_nodes.clear();
        self.last_check = Instant::now();
    }

    /// Get time since last check interval.
    pub fn time_since_last_check(&self) -> Duration {
        self.last_check.elapsed()
    }

    /// Get time until next check interval should run.
    pub fn time_until_next_check(&self) -> Duration {
        self.check_interval
            .checked_sub(self.last_check.elapsed())
            .unwrap_or(Duration::ZERO)
    }

    /// Get the configured check interval duration.
    pub fn check_interval_duration(&self) -> Duration {
        self.check_interval
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use stellar_xdr::curr::PublicKey;

    fn make_node_id(n: u8) -> NodeId {
        let mut bytes = [0u8; 32];
        bytes[0] = n;
        NodeId(PublicKey::PublicKeyTypeEd25519(stellar_xdr::curr::Uint256(
            bytes,
        )))
    }

    #[test]
    fn test_new_tracker() {
        let tracker = DeadNodeTracker::new();
        assert_eq!(tracker.dead_count(), 0);
        assert_eq!(tracker.missing_count(), 0);
        assert_eq!(
            tracker.check_interval_duration(),
            Duration::from_secs(15 * 60)
        );
    }

    #[test]
    fn test_custom_interval() {
        let tracker = DeadNodeTracker::with_interval(Duration::from_secs(60));
        assert_eq!(tracker.check_interval_duration(), Duration::from_secs(60));
    }

    #[test]
    fn test_reset_missing_nodes() {
        let mut tracker = DeadNodeTracker::new();
        let nodes: Vec<NodeId> = (0..5).map(make_node_id).collect();

        tracker.reset_missing_nodes(&nodes);
        assert_eq!(tracker.missing_count(), 5);
        assert_eq!(tracker.dead_count(), 0);
    }

    #[test]
    fn test_record_activity_removes_from_missing() {
        let mut tracker = DeadNodeTracker::new();
        let nodes: Vec<NodeId> = (0..5).map(make_node_id).collect();

        tracker.reset_missing_nodes(&nodes);
        assert_eq!(tracker.missing_count(), 5);

        tracker.record_node_activity(&nodes[0]);
        tracker.record_node_activity(&nodes[2]);

        assert_eq!(tracker.missing_count(), 3);
        assert!(!tracker.is_missing(&nodes[0]));
        assert!(tracker.is_missing(&nodes[1]));
        assert!(!tracker.is_missing(&nodes[2]));
    }

    #[test]
    fn test_check_interval_moves_missing_to_dead() {
        let mut tracker = DeadNodeTracker::new();
        let nodes: Vec<NodeId> = (0..5).map(make_node_id).collect();

        // First interval: all nodes missing
        tracker.reset_missing_nodes(&nodes);
        assert_eq!(tracker.missing_count(), 5);
        assert_eq!(tracker.dead_count(), 0);

        // Second interval: nodes still missing become dead
        tracker.check_interval(&nodes);
        assert_eq!(tracker.dead_count(), 5);
        assert_eq!(tracker.missing_count(), 5); // Reset with all nodes again

        // Node 0 and 2 participate
        tracker.record_node_activity(&nodes[0]);
        tracker.record_node_activity(&nodes[2]);

        // Third interval
        tracker.check_interval(&nodes);

        // Now only nodes 1, 3, 4 should be dead (were missing in previous interval)
        assert_eq!(tracker.dead_count(), 3);
        assert!(!tracker.is_maybe_dead(&nodes[0]));
        assert!(tracker.is_maybe_dead(&nodes[1]));
        assert!(!tracker.is_maybe_dead(&nodes[2]));
        assert!(tracker.is_maybe_dead(&nodes[3]));
        assert!(tracker.is_maybe_dead(&nodes[4]));
    }

    #[test]
    fn test_node_recovery() {
        let mut tracker = DeadNodeTracker::new();
        let nodes: Vec<NodeId> = (0..3).map(make_node_id).collect();

        // All nodes missing
        tracker.reset_missing_nodes(&nodes);

        // Move to dead
        tracker.check_interval(&nodes);
        assert_eq!(tracker.dead_count(), 3);

        // All nodes become active
        for node in &nodes {
            tracker.record_node_activity(node);
        }

        // Next interval: no dead nodes
        tracker.check_interval(&nodes);
        assert_eq!(tracker.dead_count(), 0);
    }

    #[test]
    fn test_clear() {
        let mut tracker = DeadNodeTracker::new();
        let nodes: Vec<NodeId> = (0..3).map(make_node_id).collect();

        tracker.reset_missing_nodes(&nodes);
        tracker.check_interval(&nodes);
        assert!(tracker.dead_count() > 0);
        assert!(tracker.missing_count() > 0);

        tracker.clear();
        assert_eq!(tracker.dead_count(), 0);
        assert_eq!(tracker.missing_count(), 0);
    }

    #[test]
    fn test_should_check() {
        let interval = Duration::from_secs(60);
        let mut tracker = DeadNodeTracker::with_interval(interval);

        // Freshly constructed — should not need to check
        assert!(!tracker.should_check());

        // Backdate last_check past the interval
        tracker.last_check = Instant::now() - interval;
        assert!(tracker.should_check());
    }

    #[test]
    fn test_time_until_next_check() {
        let mut tracker = DeadNodeTracker::with_interval(Duration::from_secs(60));
        // Backdate last_check by exactly 10 seconds
        tracker.last_check = Instant::now() - Duration::from_secs(10);
        let remaining = tracker.time_until_next_check();
        // Should be close to 50s (60 - 10).
        // Generous 2s tolerance absorbs any scheduling jitter.
        assert!(
            remaining <= Duration::from_secs(51),
            "remaining={remaining:?}"
        );
        assert!(
            remaining >= Duration::from_secs(49),
            "remaining={remaining:?}"
        );
    }
}
