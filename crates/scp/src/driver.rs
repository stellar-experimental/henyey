//! SCP driver trait defining callbacks for the SCP protocol.
//!
//! The [`SCPDriver`] trait is the primary integration point between the SCP
//! consensus protocol and the application layer (typically the Herder component).
//! SCP itself is completely isolated and stateless with respect to application
//! logic - all application-specific behavior is delegated through this trait.
//!
//! # Design Philosophy
//!
//! SCP is designed to be a pure consensus algorithm that:
//! - Does not know how to validate transaction sets
//! - Does not know how to persist data
//! - Does not know how to communicate with peers
//!
//! The driver provides all of this context, making SCP reusable and testable.
//!
//! # Implementation Requirements
//!
//! Implementors must ensure that:
//! - Value validation is deterministic across all nodes
//! - Hash computations match the network's expectations
//! - Timeouts increase appropriately to allow convergence
//!
//! # Example
//!
//! ```ignore
//! struct MyHerder {
//!     // ... application state
//! }
//!
//! impl SCPDriver for MyHerder {
//!     fn validate_value(&self, slot: u64, value: &Value, nomination: bool) -> ValidationLevel {
//!         // Validate transaction set...
//!         ValidationLevel::FullyValidated
//!     }
//!     // ... other methods
//! }
//! ```

use std::time::Duration;

use henyey_common::Hash256;
use stellar_xdr::curr::{NodeId, ScpBallot, ScpEnvelope, ScpQuorumSet, Value};

/// Type of SCP timer.
///
/// SCP uses timers to trigger timeouts during both nomination and
/// ballot protocol phases. This enum identifies the timer type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SCPTimerType {
    /// Timer for the nomination protocol phase.
    ///
    /// Nomination timers trigger round advancement when no progress
    /// is made within the timeout period.
    Nomination,

    /// Timer for the ballot protocol phase.
    ///
    /// Ballot timers trigger ballot bumping (increasing counter) when
    /// the current ballot round times out without externalization.
    Ballot,
}

/// Validation level for SCP values.
///
/// During consensus, values must be validated to ensure nodes agree
/// on valid data. The validation level allows for deferred validation
/// during nomination (when full context may not be available) while
/// requiring full validation before externalization.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValidationLevel {
    /// The value is invalid and should be rejected.
    ///
    /// Invalid values are not voted for and not accepted from peers.
    Invalid,

    /// The value may be valid but has not been fully validated yet.
    ///
    /// This is used during the nomination phase when full validation
    /// may be expensive or require context not yet available. Values
    /// at this level can participate in nomination but will require
    /// full validation before being committed.
    MaybeValid,

    /// The value has been fully validated and is known to be valid.
    ///
    /// Only fully validated values can be externalized (committed).
    FullyValidated,
}

/// Callback interface for the SCP consensus protocol.
///
/// This trait is implemented by the application layer (typically the Herder)
/// to connect SCP to the rest of the system. SCP is designed to be completely
/// isolated from application logic, and all interactions happen through
/// this trait.
///
/// # Thread Safety
///
/// Implementors must be `Send + Sync` as SCP may invoke callbacks from
/// multiple contexts. Internal state should be protected appropriately.
///
/// # Callback Categories
///
/// The trait methods fall into several categories:
///
/// - **Validation**: [`validate_value`](Self::validate_value), [`extract_valid_value`](Self::extract_valid_value)
/// - **Value Composition**: [`combine_candidates`](Self::combine_candidates)
/// - **Quorum Set Lookup**: [`get_quorum_set`](Self::get_quorum_set), [`get_quorum_set_by_hash`](Self::get_quorum_set_by_hash)
/// - **Cryptography**: [`sign_envelope`](Self::sign_envelope), [`verify_envelope`](Self::verify_envelope), [`hash_quorum_set`](Self::hash_quorum_set)
/// - **Hash Computation**: [`compute_hash_node`](Self::compute_hash_node), [`compute_value_hash`](Self::compute_value_hash)
/// - **Network**: [`emit_envelope`](Self::emit_envelope)
/// - **Notifications**: [`nominating_value`](Self::nominating_value), [`value_externalized`](Self::value_externalized), etc.
/// - **Timing**: [`compute_timeout`](Self::compute_timeout)
pub trait SCPDriver: Send + Sync {
    /// Validate a value.
    ///
    /// # Arguments
    /// * `slot_index` - The slot for which this value is being considered
    /// * `value` - The value to validate
    /// * `nomination` - True if this is during nomination phase
    ///
    /// # Returns
    /// The validation level for this value.
    fn validate_value(&self, slot_index: u64, value: &Value, nomination: bool) -> ValidationLevel;

    /// Combine multiple nominated values into one.
    ///
    /// This is called when nomination has produced multiple candidate values
    /// and we need to combine them into a single composite value for the
    /// ballot protocol.
    ///
    /// # Arguments
    /// * `slot_index` - The slot index
    /// * `candidates` - The candidate values to combine
    ///
    /// # Returns
    /// The combined value, or None if combination is not possible.
    fn combine_candidates(&self, slot_index: u64, candidates: &[Value]) -> Option<Value>;

    /// Extract a valid value from a potentially invalid composite.
    ///
    /// When we receive a value from the network that may be partially valid,
    /// this method extracts the valid portion.
    ///
    /// # Arguments
    /// * `slot_index` - The slot index
    /// * `value` - The value to extract from
    ///
    /// # Returns
    /// The extracted valid value, or None if no valid value can be extracted.
    fn extract_valid_value(&self, slot_index: u64, value: &Value) -> Option<Value>;

    /// Emit an envelope to peers.
    ///
    /// Called when SCP needs to broadcast a message to the network.
    fn emit_envelope(&self, envelope: &ScpEnvelope);

    /// Get the quorum set for a node.
    ///
    /// # Arguments
    /// * `node_id` - The node whose quorum set we need
    ///
    /// # Returns
    /// The quorum set, or None if unknown.
    fn get_quorum_set(&self, node_id: &NodeId) -> Option<ScpQuorumSet>;

    /// Get a quorum set by its hash.
    ///
    /// # Arguments
    /// * `hash` - The quorum set hash
    ///
    /// # Returns
    /// The quorum set, or None if unknown.
    fn get_quorum_set_by_hash(&self, _hash: &Hash256) -> Option<ScpQuorumSet> {
        None
    }

    /// Called when we start nominating a value for a slot.
    fn nominating_value(&self, slot_index: u64, value: &Value);

    /// Called when consensus is reached on a value.
    ///
    /// This is the final callback indicating that the slot has been
    /// externalized with the given value.
    fn value_externalized(&self, slot_index: u64, value: &Value);

    /// Called when a ballot is prepared.
    fn ballot_did_prepare(&self, slot_index: u64, ballot: &ScpBallot);

    /// Called when a ballot is confirmed.
    fn ballot_did_confirm(&self, slot_index: u64, ballot: &ScpBallot);

    /// Called when we heard from a quorum for the current ballot.
    fn ballot_did_hear_from_quorum(&self, _slot_index: u64, _ballot: &ScpBallot) {}

    /// Called when the ballot protocol is started for a slot.
    ///
    /// This is called when we transition from nomination to the ballot protocol,
    /// indicating that we have candidate values and are beginning voting.
    fn started_ballot_protocol(&self, _slot_index: u64, _value: &Value) {}

    /// Called when the composite candidate value is updated during nomination.
    ///
    /// This is called during nomination when the composite candidate changes,
    /// allowing the driver to track nomination progress.
    fn updated_candidate_value(&self, _slot_index: u64, _value: &Value) {}

    /// Compute hash for node priority in nomination.
    ///
    /// This is used to deterministically order nodes during nomination
    /// to ensure consistent behavior across the network.
    fn compute_hash_node(
        &self,
        slot_index: u64,
        prev_value: &Value,
        is_priority: bool,
        round: u32,
        node_id: &NodeId,
    ) -> u64;

    /// Compute value hash for nomination ordering.
    fn compute_value_hash(
        &self,
        slot_index: u64,
        prev_value: &Value,
        round: u32,
        value: &Value,
    ) -> u64;

    /// Compute timeout for a nomination or ballot round.
    ///
    /// Timeouts typically increase with round number to allow more
    /// time for consensus when the network is unstable.
    fn compute_timeout(&self, round: u32, is_nomination: bool) -> Duration;

    /// Sign an envelope before sending.
    ///
    /// The implementation should compute the signature over the
    /// statement and set it on the envelope.
    fn sign_envelope(&self, envelope: &mut ScpEnvelope);

    /// Verify an envelope's signature.
    ///
    /// # Returns
    /// True if the signature is valid.
    fn verify_envelope(&self, envelope: &ScpEnvelope) -> bool;

    /// Get the hash of a quorum set.
    ///
    /// This is used to reference quorum sets by hash in SCP messages.
    fn hash_quorum_set(&self, quorum_set: &ScpQuorumSet) -> Hash256 {
        // Default implementation using XDR serialization
        Hash256::hash_xdr(quorum_set).unwrap_or(Hash256::ZERO)
    }

    /// Get the weight of a node in quorum calculations.
    ///
    /// Returns the weight of `node_id` within `quorum_set`, normalized between
    /// 0 and `u64::MAX`. If `node_id` is the local node, `is_local_node` should
    /// be `true`.
    ///
    /// The default implementation mirrors upstream `SCPDriver::getNodeWeight`:
    /// - Local node returns `u64::MAX`
    /// - Validators found at top level: `computeWeight(UINT64_MAX, d, n)`
    /// - Validators found in inner sets: `computeWeight(leafW, d, n)`
    /// - Not found: 0
    ///
    /// The herder can override this for application-specific weight functions
    /// (e.g., quality-based weights in P22+).
    ///
    /// # Arguments
    /// * `node_id` - The node to get weight for
    /// * `quorum_set` - The quorum set context
    /// * `is_local_node` - Whether `node_id` is the local node
    ///
    /// # Returns
    /// Weight as a u64 between 0 and `u64::MAX`.
    fn get_node_weight(
        &self,
        node_id: &NodeId,
        quorum_set: &ScpQuorumSet,
        is_local_node: bool,
    ) -> u64 {
        base_get_node_weight(node_id, quorum_set, is_local_node)
    }

    /// Get a debug string representation of a value.
    ///
    /// Used for logging and debugging. The default implementation
    /// returns a hex-encoded prefix of the value.
    fn get_value_string(&self, value: &Value) -> String {
        let bytes = value.as_slice();
        hex::encode(&bytes[..8.min(bytes.len())])
    }

    /// Compute a generic hash of arbitrary data.
    ///
    /// This is used for various internal hashing needs.
    /// The default implementation uses SHA-256.
    fn get_hash_of(&self, data: &[u8]) -> Hash256 {
        Hash256::hash(data)
    }

    /// Request the application to set up a timer.
    ///
    /// This callback is invoked when SCP needs a timer to be started.
    /// When the timer fires, the application should call the appropriate
    /// SCP method to handle the timeout (e.g., `nominate_timeout` or
    /// `bump_ballot_on_timeout`).
    ///
    /// # Arguments
    /// * `slot_index` - The slot this timer is for
    /// * `timer_type` - Whether this is a nomination or ballot timer
    /// * `timeout` - The duration after which the timer should fire
    ///
    /// # Default Implementation
    /// The default implementation does nothing. Applications that want
    /// timer-based timeout handling should override this method to spawn
    /// an async task that sleeps for `timeout` and then calls the
    /// appropriate SCP handler (`nominate_timeout` or `bump_ballot_on_timeout`).
    fn setup_timer(&self, _slot_index: u64, _timer_type: SCPTimerType, _timeout: Duration) {
        // Default: do nothing, timers managed externally
    }

    /// Request the application to cancel a previously set timer.
    ///
    /// This callback is invoked when a timer is no longer needed,
    /// typically because the slot has made progress or been externalized.
    ///
    /// # Arguments
    /// * `slot_index` - The slot whose timer should be cancelled
    /// * `timer_type` - Whether this is a nomination or ballot timer
    ///
    /// # Default Implementation
    /// The default implementation does nothing.
    fn stop_timer(&self, _slot_index: u64, _timer_type: SCPTimerType) {
        // Default: do nothing, timers managed externally
    }

    /// Called when a timer expires.
    ///
    /// This is an informational callback to notify the driver that
    /// a timeout occurred. The actual timeout handling is performed
    /// by SCP internally.
    ///
    /// # Arguments
    /// * `slot_index` - The slot that timed out
    /// * `timer_type` - Whether this was a nomination or ballot timer
    fn timer_expired(&self, _slot_index: u64, _timer_type: SCPTimerType) {
        // Default: do nothing
    }

    /// Check whether a value contains protocol upgrades.
    ///
    /// Used during nomination to detect when upgrade-containing values
    /// are causing consensus timeouts. When too many timeouts occur,
    /// upgrades may be stripped to allow consensus to proceed.
    ///
    /// # Default Implementation
    /// Returns false (no upgrades).
    fn has_upgrades(&self, _value: &Value) -> bool {
        false
    }

    /// Strip all upgrades from a value.
    ///
    /// Returns a new value with all upgrades removed, or None if the
    /// value cannot have upgrades stripped (e.g., it has no upgrades).
    ///
    /// # Default Implementation
    /// Returns None.
    fn strip_all_upgrades(&self, _value: &Value) -> Option<Value> {
        None
    }

    /// Get the number of nomination timeouts before upgrades are stripped.
    ///
    /// When the nomination timer expires this many times, values with
    /// upgrades will have their upgrades stripped to help consensus proceed.
    ///
    /// # Default Implementation
    /// Returns `u32::MAX` (effectively never strip).
    fn get_upgrade_nomination_timeout_limit(&self) -> u32 {
        u32::MAX
    }
}

/// Compute weight as `ceil(m * threshold / total)`.
///
/// Matches upstream `computeWeight` in `SCPDriver.cpp`:
/// `bigDivideUnsigned(res, m, threshold, total, ROUND_UP)`.
///
/// Uses `u128` to avoid overflow since `m` can be `u64::MAX`.
// SECURITY: quorum set validated at herder layer before reaching SCP weight computation
pub(crate) fn compute_weight(m: u64, total: u64, threshold: u64) -> u64 {
    if threshold == 0 || total == 0 {
        return 0;
    }
    debug_assert!(threshold <= total);
    let numerator = (m as u128) * (threshold as u128);
    let denominator = total as u128;
    numerator.div_ceil(denominator) as u64
}

/// Base implementation of `get_node_weight`, matching upstream `SCPDriver::getNodeWeight`.
///
/// - If `is_local_node` is true, returns `u64::MAX`.
/// - Searches validators at each level; if found, applies `compute_weight`.
/// - Recursively searches inner sets; if found in an inner set, the inner set's
///   weight is further scaled by the outer level's `compute_weight`.
/// - Returns 0 if the node is not found.
// SECURITY: quorum set validated at herder layer before reaching SCP weight computation
pub(crate) fn base_get_node_weight(
    node_id: &NodeId,
    quorum_set: &ScpQuorumSet,
    is_local_node: bool,
) -> u64 {
    if is_local_node {
        return u64::MAX;
    }

    let total = (quorum_set.validators.len() + quorum_set.inner_sets.len()) as u64;
    let threshold = quorum_set.threshold as u64;
    if threshold == 0 || total == 0 {
        return 0;
    }

    // Check top-level validators
    for validator in quorum_set.validators.iter() {
        if validator == node_id {
            return compute_weight(u64::MAX, total, threshold);
        }
    }

    // Recursively check inner sets
    for inner in quorum_set.inner_sets.iter() {
        let leaf_w = base_get_node_weight(node_id, inner, is_local_node);
        if leaf_w > 0 {
            return compute_weight(leaf_w, total, threshold);
        }
    }

    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::{make_node_id, make_quorum_set};

    fn is_near_weight(weight: u64, target: f64) -> bool {
        let ratio = weight as f64 / u64::MAX as f64;
        (ratio - target).abs() < 0.01
    }

    #[test]
    fn test_compute_weight_basic() {
        // threshold=3, total=4: ceil(UINT64_MAX * 3 / 4) approx 0.75 * UINT64_MAX
        let w = compute_weight(u64::MAX, 4, 3);
        assert!(is_near_weight(w, 0.75));
    }

    #[test]
    fn test_compute_weight_full() {
        // threshold=total: ceil(m * 1) = m
        let w = compute_weight(u64::MAX, 3, 3);
        assert_eq!(w, u64::MAX);
    }

    #[test]
    fn test_compute_weight_zero_total() {
        assert_eq!(compute_weight(u64::MAX, 0, 0), 0);
    }

    #[test]
    fn test_compute_weight_zero_threshold() {
        assert_eq!(compute_weight(u64::MAX, 4, 0), 0);
    }

    #[test]
    fn test_compute_weight_rounds_up() {
        // 10 * 3 / 4 = 7.5 -> should round up to 8
        let w = compute_weight(10, 4, 3);
        assert_eq!(w, 8);
    }

    #[test]
    fn test_base_get_node_weight_local_node() {
        let node0 = make_node_id(0);
        let qset = make_quorum_set(vec![node0.clone()], 1);
        assert_eq!(base_get_node_weight(&node0, &qset, true), u64::MAX);
    }

    #[test]
    fn test_base_get_node_weight_flat_qset() {
        let node0 = make_node_id(0);
        let node1 = make_node_id(1);
        let node2 = make_node_id(2);
        let node3 = make_node_id(3);

        // 4 validators, threshold 3 -> weight = 3/4 = 0.75
        let qset = make_quorum_set(
            vec![node0.clone(), node1.clone(), node2.clone(), node3.clone()],
            3,
        );

        let weight = base_get_node_weight(&node2, &qset, false);
        assert!(is_near_weight(weight, 0.75));
    }

    #[test]
    fn test_base_get_node_weight_not_in_qset() {
        let node0 = make_node_id(0);
        let node1 = make_node_id(1);
        let node2 = make_node_id(2);
        let node3 = make_node_id(3);
        let node4 = make_node_id(4);

        let qset = make_quorum_set(
            vec![node0.clone(), node1.clone(), node2.clone(), node3.clone()],
            3,
        );

        assert_eq!(base_get_node_weight(&node4, &qset, false), 0);
    }

    #[test]
    fn test_base_get_node_weight_inner_set() {
        let node0 = make_node_id(0);
        let node1 = make_node_id(1);
        let node2 = make_node_id(2);
        let node3 = make_node_id(3);
        let node4 = make_node_id(4);
        let node5 = make_node_id(5);

        let inner = make_quorum_set(vec![node4.clone(), node5.clone()], 1);

        // 4 validators + 1 inner set = 5 entries, threshold 3
        let qset = ScpQuorumSet {
            threshold: 3,
            validators: vec![node0.clone(), node1.clone(), node2.clone(), node3.clone()]
                .try_into()
                .unwrap(),
            inner_sets: vec![inner].try_into().unwrap(),
        };

        // node4 in inner set: inner weight = UINT64_MAX * 1/2 = 0.5
        // outer weight = innerWeight * 3/5 = 0.5 * 0.6 = 0.3
        let weight = base_get_node_weight(&node4, &qset, false);
        assert!(is_near_weight(weight, 0.6 * 0.5));
    }

    #[test]
    fn test_base_get_node_weight_empty_qset() {
        let node0 = make_node_id(0);
        let qset = ScpQuorumSet {
            threshold: 0,
            validators: Vec::new().try_into().unwrap(),
            inner_sets: Vec::new().try_into().unwrap(),
        };
        assert_eq!(base_get_node_weight(&node0, &qset, false), 0);
    }

    #[test]
    fn test_base_get_node_weight_matches_upstream_test_vectors() {
        // Upstream test from SCPUnitTests.cpp:
        // 4 validators (v0..v3), threshold 3
        // v2 weight approx 0.75
        // v4 not found -> 0
        // Add inner set [v4, v5] threshold 1 -> v4 approx 0.6 * 0.5

        let node0 = make_node_id(0);
        let node1 = make_node_id(1);
        let node2 = make_node_id(2);
        let node3 = make_node_id(3);
        let node4 = make_node_id(4);
        let node5 = make_node_id(5);

        let mut qset = make_quorum_set(
            vec![node0.clone(), node1.clone(), node2.clone(), node3.clone()],
            3,
        );

        let result = base_get_node_weight(&node2, &qset, false);
        assert!(is_near_weight(result, 0.75));

        let result = base_get_node_weight(&node4, &qset, false);
        assert_eq!(result, 0);

        let inner = make_quorum_set(vec![node4.clone(), node5.clone()], 1);
        qset.inner_sets = vec![inner].try_into().unwrap();

        let result = base_get_node_weight(&node4, &qset, false);
        assert!(is_near_weight(result, 0.6 * 0.5));
    }
}
