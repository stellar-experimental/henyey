//! Externalize lag tracking for SCP quorum set nodes.
//!
//! Tracks the time difference between when this node first externalizes a slot
//! and when each peer's EXTERNALIZE envelope arrives. This data populates the
//! `lag_ms` field in the `/info` quorum JSON output.
//!
//! # Approximation Note
//!
//! stellar-core uses `medida::Timer` with an `ExponentiallyDecayingReservoir`
//! (~1028 samples, exponential decay weighting recent samples more heavily).
//! This implementation uses a fixed 128-sample FIFO ring buffer per node with
//! equal weighting. This is an **intentional approximation** that achieves:
//!
//! - **Field-level parity**: Same JSON field name, same 75th-percentile semantic
//! - **Behavioral similarity**: Both track per-node externalize lag across slots
//!
//! The practical difference: medida gives more weight to recent samples via
//! exponential decay; our ring buffer gives equal weight to the last 128 samples.
//! Both converge for steady-state lag. The numeric values may differ slightly
//! from stellar-core's output for the same traffic pattern, but the diagnostic
//! utility is equivalent.

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use henyey_scp::SlotIndex;
use stellar_xdr::curr::{NodeId, ScpQuorumSet};

/// Maximum number of lag samples retained per node.
const MAX_LAG_SAMPLES: usize = 128;

/// Tracks per-node externalize lag across SCP slots.
///
/// Records the time between when this node first processes an EXTERNALIZE
/// event for a slot (self-event) and when each peer's EXTERNALIZE envelope
/// is accepted (peer-event). The per-node lag samples are kept in a ring
/// buffer and queried for 75th-percentile statistics.
pub struct ExternalizeLagTracker {
    /// Per-node ring buffer of lag samples (Duration).
    /// Never cleared by slot cleanup — matches stellar-core's `mQSetLag`.
    node_lag: HashMap<NodeId, VecDeque<Duration>>,
    /// First externalize timestamp per slot — set by the first
    /// `record_event` call for each slot (typically the self-event).
    first_externalize: HashMap<SlotIndex, Instant>,
}

impl ExternalizeLagTracker {
    pub fn new() -> Self {
        Self {
            node_lag: HashMap::new(),
            first_externalize: HashMap::new(),
        }
    }

    /// Record an externalize event for a slot.
    ///
    /// - On the first call per slot, sets `first_externalize[slot] = now`.
    /// - For non-self events (peers), records `now - first_externalize[slot]`
    ///   as a lag sample for the node.
    /// - Self events (`is_self = true`) only set the baseline; they don't
    ///   record lag (matching stellar-core: self-node lag is tracked via
    ///   separate `mSelfExternalize` / metric timers, not `mQSetLag`).
    ///
    /// Mirrors `HerderSCPDriver::recordSCPExternalizeEvent` (lines 1141-1201).
    pub fn record_event(&mut self, slot: SlotIndex, node_id: &NodeId, is_self: bool, now: Instant) {
        // Set first_externalize on first call per slot.
        let first = self.first_externalize.entry(slot).or_insert(now);

        if !is_self {
            // Peer event: record lag relative to first externalize.
            let lag = now.duration_since(*first);
            let samples = self.node_lag.entry(node_id.clone()).or_default();
            if samples.len() >= MAX_LAG_SAMPLES {
                samples.pop_front();
            }
            samples.push_back(lag);
        }
    }

    /// Get the 75th percentile lag for a specific node, in milliseconds.
    ///
    /// Returns `None` if no samples exist for this node.
    /// Returns `0` when samples exist but all are zero.
    ///
    /// Mirrors `HerderSCPDriver::getExternalizeLag` which returns
    /// `timer.GetSnapshot().get75thPercentile()`.
    pub fn get_externalize_lag(&self, node_id: &NodeId) -> Option<u64> {
        let samples = self.node_lag.get(node_id)?;
        if samples.is_empty() {
            return None;
        }
        Some(percentile_75_ms(samples))
    }

    /// Get summary lag info: average of 75th-percentile lags across quorum set
    /// nodes that have lag > 0.
    ///
    /// Returns `None` when no nodes have positive lag.
    ///
    /// Mirrors `HerderSCPDriver::getQsetLagInfo(summary=true)`.
    pub fn get_lag_info_summary(&self, quorum_set: &ScpQuorumSet) -> Option<u64> {
        let mut total_lag: u64 = 0;
        let mut count: u64 = 0;

        for_all_nodes(quorum_set, &mut |node_id| {
            if let Some(lag) = self.get_externalize_lag(node_id) {
                if lag > 0 {
                    total_lag += lag;
                    count += 1;
                }
            }
        });

        total_lag.checked_div(count)
    }

    /// Get per-node lag info for non-summary mode.
    ///
    /// Populates the `LagJsonInfo` with per-node lag values for all quorum set
    /// nodes that have lag > 0.
    ///
    /// Mirrors `HerderSCPDriver::getQsetLagInfo(summary=false)`.
    #[allow(dead_code)]
    pub fn get_lag_info(
        &self,
        quorum_set: &ScpQuorumSet,
        full_keys: bool,
    ) -> crate::json_api::LagJsonInfo {
        let mut nodes = Vec::new();

        for_all_nodes(quorum_set, &mut |node_id| {
            if let Some(lag) = self.get_externalize_lag(node_id) {
                if lag > 0 {
                    let name = crate::json_api::format_node_id(node_id, full_keys);
                    nodes.push(crate::json_api::NodeLagInfo {
                        node: name,
                        lag_ms: lag,
                    });
                }
            }
        });

        crate::json_api::LagJsonInfo {
            nodes,
            summary: None,
        }
    }

    /// Get the first-externalize timestamp for a slot, if recorded.
    pub fn first_externalize_for_slot(&self, slot: SlotIndex) -> Option<Instant> {
        self.first_externalize.get(&slot).copied()
    }

    /// Remove first_externalize entries for a specific slot.
    pub fn cleanup_slot(&mut self, slot: SlotIndex) {
        self.first_externalize.remove(&slot);
    }

    /// Remove first_externalize entries for all slots below the given threshold.
    pub fn cleanup_slots_below(&mut self, slot: SlotIndex) {
        self.first_externalize.retain(|&s, _| s >= slot);
    }

    /// Clear all slot-scoped data (first_externalize map).
    /// Does NOT clear node_lag — matches stellar-core's behavior.
    pub fn clear_slots(&mut self) {
        self.first_externalize.clear();
    }
}

/// Compute the 75th percentile of durations in milliseconds.
///
/// Uses nearest-rank method: index = ceil(0.75 * n) - 1.
fn percentile_75_ms(samples: &VecDeque<Duration>) -> u64 {
    let mut sorted: Vec<u64> = samples.iter().map(|d| d.as_millis() as u64).collect();
    sorted.sort_unstable();

    let n = sorted.len();
    // Nearest-rank: ceil(0.75 * n) - 1
    let rank = ((0.75 * n as f64).ceil() as usize)
        .saturating_sub(1)
        .min(n - 1);
    sorted[rank]
}

/// Iterate over all nodes in a quorum set (validators + inner sets).
///
/// Mirrors stellar-core's `LocalNode::forAllNodes`.
fn for_all_nodes<F>(quorum_set: &ScpQuorumSet, f: &mut F)
where
    F: FnMut(&NodeId),
{
    for node in quorum_set.validators.iter() {
        f(node);
    }
    for inner in quorum_set.inner_sets.iter() {
        for_all_nodes(inner, f);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use stellar_xdr::curr::PublicKey;

    fn make_node(byte: u8) -> NodeId {
        NodeId(PublicKey::PublicKeyTypeEd25519(stellar_xdr::curr::Uint256(
            [byte; 32],
        )))
    }

    fn make_qset(nodes: &[NodeId]) -> ScpQuorumSet {
        ScpQuorumSet {
            threshold: nodes.len() as u32,
            validators: nodes.to_vec().try_into().unwrap(),
            inner_sets: Vec::new().try_into().unwrap(),
        }
    }

    #[test]
    fn test_self_only_no_lag() {
        let mut tracker = ExternalizeLagTracker::new();
        let self_node = make_node(1);
        let t0 = Instant::now();

        tracker.record_event(100, &self_node, true, t0);

        let qset = make_qset(&[self_node.clone()]);
        assert_eq!(tracker.get_lag_info_summary(&qset), None);
    }

    #[test]
    fn test_peer_after_self() {
        let mut tracker = ExternalizeLagTracker::new();
        let self_node = make_node(1);
        let peer = make_node(2);
        let t0 = Instant::now();
        let t100 = t0 + Duration::from_millis(100);

        // Self event sets baseline
        tracker.record_event(100, &self_node, true, t0);
        // Peer event records lag
        tracker.record_event(100, &peer, false, t100);

        let lag = tracker.get_externalize_lag(&peer).unwrap();
        assert_eq!(lag, 100);

        let qset = make_qset(&[self_node, peer]);
        let summary = tracker.get_lag_info_summary(&qset).unwrap();
        assert_eq!(summary, 100);
    }

    #[test]
    fn test_peer_first_self_later() {
        let mut tracker = ExternalizeLagTracker::new();
        let self_node = make_node(1);
        let peer = make_node(2);
        let t0 = Instant::now();

        // Peer arrives first — sets first_externalize AND records lag = 0
        tracker.record_event(100, &peer, false, t0);
        // Self arrives later — doesn't affect peer lag
        tracker.record_event(100, &self_node, true, t0 + Duration::from_millis(50));

        let lag = tracker.get_externalize_lag(&peer).unwrap();
        assert_eq!(lag, 0); // peer arrived at same time as first_externalize

        // Summary excludes nodes with lag = 0
        let qset = make_qset(&[self_node, peer]);
        assert_eq!(tracker.get_lag_info_summary(&qset), None);
    }

    #[test]
    fn test_multiple_peers_summary_average() {
        let mut tracker = ExternalizeLagTracker::new();
        let self_node = make_node(1);
        let peer_a = make_node(2);
        let peer_b = make_node(3);
        let t0 = Instant::now();

        tracker.record_event(100, &self_node, true, t0);
        tracker.record_event(100, &peer_a, false, t0 + Duration::from_millis(100));
        tracker.record_event(100, &peer_b, false, t0 + Duration::from_millis(200));

        let qset = make_qset(&[self_node, peer_a, peer_b]);
        let summary = tracker.get_lag_info_summary(&qset).unwrap();
        // avg(100, 200) = 150
        assert_eq!(summary, 150);
    }

    #[test]
    fn test_ring_buffer_overflow() {
        let mut tracker = ExternalizeLagTracker::new();
        let self_node = make_node(1);
        let peer = make_node(2);
        let t0 = Instant::now();

        // Fill with 128 + 10 samples (different slots)
        for i in 0..(MAX_LAG_SAMPLES + 10) {
            let slot = i as u64;
            let base = t0 + Duration::from_secs(i as u64 * 10);
            tracker.record_event(slot, &self_node, true, base);
            tracker.record_event(slot, &peer, false, base + Duration::from_millis(50));
        }

        // All samples are 50ms, so 75th percentile is 50ms
        let lag = tracker.get_externalize_lag(&peer).unwrap();
        assert_eq!(lag, 50);

        // Verify we only kept MAX_LAG_SAMPLES
        assert_eq!(tracker.node_lag[&peer].len(), MAX_LAG_SAMPLES);
    }

    #[test]
    fn test_percentile_75() {
        let mut tracker = ExternalizeLagTracker::new();
        let self_node = make_node(1);
        let peer = make_node(2);
        let t0 = Instant::now();

        // Insert 4 samples with different lags: 10, 20, 30, 40
        for (i, lag_ms) in [10u64, 20, 30, 40].iter().enumerate() {
            let slot = i as u64;
            let base = t0 + Duration::from_secs(i as u64 * 10);
            tracker.record_event(slot, &self_node, true, base);
            tracker.record_event(slot, &peer, false, base + Duration::from_millis(*lag_ms));
        }

        // 75th percentile of [10, 20, 30, 40]: ceil(0.75 * 4) - 1 = 2 → sorted[2] = 30
        let lag = tracker.get_externalize_lag(&peer).unwrap();
        assert_eq!(lag, 30);
    }

    #[test]
    fn test_slot_cleanup_preserves_node_lag() {
        let mut tracker = ExternalizeLagTracker::new();
        let self_node = make_node(1);
        let peer = make_node(2);
        let t0 = Instant::now();

        tracker.record_event(100, &self_node, true, t0);
        tracker.record_event(100, &peer, false, t0 + Duration::from_millis(50));

        // Cleanup slot 100
        tracker.cleanup_slots_below(101);

        // first_externalize is gone
        assert!(!tracker.first_externalize.contains_key(&100));
        // node_lag survives
        assert_eq!(tracker.get_externalize_lag(&peer).unwrap(), 50);
    }

    #[test]
    fn test_clear_slots_preserves_node_lag() {
        let mut tracker = ExternalizeLagTracker::new();
        let self_node = make_node(1);
        let peer = make_node(2);
        let t0 = Instant::now();

        tracker.record_event(100, &self_node, true, t0);
        tracker.record_event(100, &peer, false, t0 + Duration::from_millis(75));

        tracker.clear_slots();

        assert!(tracker.first_externalize.is_empty());
        assert_eq!(tracker.get_externalize_lag(&peer).unwrap(), 75);
    }

    #[test]
    fn test_duplicate_peer_event_records_two_samples() {
        let mut tracker = ExternalizeLagTracker::new();
        let self_node = make_node(1);
        let peer = make_node(2);
        let t0 = Instant::now();

        tracker.record_event(100, &self_node, true, t0);
        // Same peer, same slot, two arrivals
        tracker.record_event(100, &peer, false, t0 + Duration::from_millis(50));
        tracker.record_event(100, &peer, false, t0 + Duration::from_millis(150));

        assert_eq!(tracker.node_lag[&peer].len(), 2);
        // 75th percentile of [50, 150]: ceil(0.75 * 2) - 1 = 1 → sorted[1] = 150
        let lag = tracker.get_externalize_lag(&peer).unwrap();
        assert_eq!(lag, 150);
    }

    #[test]
    fn test_get_lag_info_non_summary() {
        let mut tracker = ExternalizeLagTracker::new();
        let self_node = make_node(1);
        let peer_a = make_node(2);
        let peer_b = make_node(3);
        let t0 = Instant::now();

        tracker.record_event(100, &self_node, true, t0);
        tracker.record_event(100, &peer_a, false, t0 + Duration::from_millis(100));
        tracker.record_event(100, &peer_b, false, t0 + Duration::from_millis(200));

        let qset = make_qset(&[self_node, peer_a, peer_b]);
        let info = tracker.get_lag_info(&qset, false);

        assert_eq!(info.nodes.len(), 2);
        // Verify both peers are present with correct lag values
        let lags: Vec<u64> = info.nodes.iter().map(|n| n.lag_ms).collect();
        assert!(lags.contains(&100));
        assert!(lags.contains(&200));
    }

    #[test]
    fn test_lag_ms_json_serialization() {
        use crate::json_api::InfoQuorumSetSnapshot;

        let with_lag = InfoQuorumSetSnapshot {
            phase: "EXTERNALIZE".to_string(),
            hash: Some("aabbcc".to_string()),
            fail_at: Some(1),
            validated: None,
            agree: 3,
            disagree: 0,
            missing: 0,
            delayed: 0,
            ledger: 100,
            lag_ms: Some(450),
        };
        let json = serde_json::to_value(&with_lag).unwrap();
        assert_eq!(json["lag_ms"], 450);

        let without_lag = InfoQuorumSetSnapshot {
            phase: "EXTERNALIZE".to_string(),
            hash: Some("aabbcc".to_string()),
            fail_at: Some(1),
            validated: None,
            agree: 3,
            disagree: 0,
            missing: 0,
            delayed: 0,
            ledger: 100,
            lag_ms: None,
        };
        let json = serde_json::to_value(&without_lag).unwrap();
        assert!(json["lag_ms"].is_null());
    }
}
