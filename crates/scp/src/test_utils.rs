//! Shared test utilities for the SCP crate.
//!
//! Centralises the `MockDriver` implementation and common helpers
//! (`make_node_id`, `make_quorum_set`, `make_value`) that were previously
//! duplicated across every `#[cfg(test)] mod tests` block.

use crate::driver::{SCPDriver, ValidationLevel};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;
use stellar_xdr::curr::{NodeId, PublicKey, ScpBallot, ScpEnvelope, ScpQuorumSet, Uint256, Value};

// ---------------------------------------------------------------------------
// MockDriver — configurable mock for SCPDriver
// ---------------------------------------------------------------------------

/// A configurable mock [`SCPDriver`] used by tests across the crate.
///
/// Behaviour can be tuned via [`MockDriverBuilder`]:
///
/// | Option | Default |
/// |---|---|
/// | `quorum_set` | `None` (returns `None` from `get_quorum_set`) |
/// | `validation_level` | `FullyValidated` |
/// | `value_hash_mode` | `SumBytes` — `compute_value_hash` returns sum of value bytes |
/// | `timeout_mode` | `Fixed(1ms)` |
pub struct MockDriver {
    quorum_set: Option<ScpQuorumSet>,
    validation_level: ValidationLevel,
    value_hash_mode: ValueHashMode,
    timeout_mode: TimeoutMode,
    /// Counts how many times `emit_envelope` was called.
    pub emit_count: AtomicU32,
    /// Counts how many times `ballot_did_hear_from_quorum` was called.
    pub heard_from_quorum: AtomicU32,
    /// If true, `get_quorum_set_by_hash` returns the configured quorum set
    /// regardless of the hash. Useful for testing quorum info methods.
    pub return_qset_by_hash: bool,
    /// If `Some`, `get_node_weight` returns this fixed value for all nodes
    /// instead of the default quorum-position algorithm.
    pub custom_node_weight: Option<u64>,
}

/// How `compute_value_hash` should behave.
#[derive(Clone, Copy)]
pub enum ValueHashMode {
    /// Sum the bytes of the value (default for ballot/nomination tests).
    SumBytes,
    /// Always return a fixed value (default for scp.rs tests).
    Fixed(u64),
}

/// How `compute_timeout` should behave.
#[derive(Clone, Copy)]
pub enum TimeoutMode {
    /// Always return a fixed duration.
    Fixed(Duration),
    /// Return `base + round * step`.
    Linear { base: Duration, step: Duration },
}

// ---- Builder ----

pub struct MockDriverBuilder {
    quorum_set: Option<ScpQuorumSet>,
    validation_level: ValidationLevel,
    value_hash_mode: ValueHashMode,
    timeout_mode: TimeoutMode,
    return_qset_by_hash: bool,
}

impl MockDriverBuilder {
    pub fn new() -> Self {
        Self {
            quorum_set: None,
            validation_level: ValidationLevel::FullyValidated,
            value_hash_mode: ValueHashMode::SumBytes,
            timeout_mode: TimeoutMode::Fixed(Duration::from_millis(1)),
            return_qset_by_hash: false,
        }
    }

    pub fn quorum_set(mut self, qs: ScpQuorumSet) -> Self {
        self.quorum_set = Some(qs);
        self
    }

    pub fn validation_level(mut self, level: ValidationLevel) -> Self {
        self.validation_level = level;
        self
    }

    pub fn value_hash_mode(mut self, mode: ValueHashMode) -> Self {
        self.value_hash_mode = mode;
        self
    }

    pub fn timeout_mode(mut self, mode: TimeoutMode) -> Self {
        self.timeout_mode = mode;
        self
    }

    pub fn return_qset_by_hash(mut self) -> Self {
        self.return_qset_by_hash = true;
        self
    }

    pub fn build(self) -> MockDriver {
        MockDriver {
            quorum_set: self.quorum_set,
            validation_level: self.validation_level,
            value_hash_mode: self.value_hash_mode,
            timeout_mode: self.timeout_mode,
            emit_count: AtomicU32::new(0),
            heard_from_quorum: AtomicU32::new(0),
            return_qset_by_hash: self.return_qset_by_hash,
            custom_node_weight: None,
        }
    }
}

impl MockDriver {
    /// Convenience: fully-validated driver with a quorum set, byte-sum value
    /// hashes, and 1 ms timeouts. This matches the original `MockDriver` used
    /// in `ballot/mod.rs` and `nomination.rs`.
    pub fn with_quorum_set(qs: ScpQuorumSet) -> Self {
        MockDriverBuilder::new().quorum_set(qs).build()
    }

    /// Convenience: bare-minimum driver with no quorum set. Suitable for
    /// tests that only need a valid `SCPDriver` impl.
    pub fn bare() -> Self {
        MockDriverBuilder::new()
            .value_hash_mode(ValueHashMode::Fixed(1))
            .timeout_mode(TimeoutMode::Linear {
                base: Duration::from_secs(1),
                step: Duration::from_secs(1),
            })
            .build()
    }
}

impl SCPDriver for MockDriver {
    fn validate_value(
        &self,
        _slot_index: u64,
        _value: &Value,
        _nomination: bool,
    ) -> ValidationLevel {
        self.validation_level
    }

    fn combine_candidates(&self, _slot_index: u64, candidates: &[Value]) -> Option<Value> {
        candidates.first().cloned()
    }

    fn extract_valid_value(&self, _slot_index: u64, value: &Value) -> Option<Value> {
        Some(value.clone())
    }

    fn emit_envelope(&self, _envelope: &ScpEnvelope) {
        self.emit_count.fetch_add(1, Ordering::SeqCst);
    }

    fn get_quorum_set(&self, _node_id: &NodeId) -> Option<ScpQuorumSet> {
        self.quorum_set.clone()
    }

    fn get_quorum_set_by_hash(&self, _hash: &henyey_common::Hash256) -> Option<ScpQuorumSet> {
        if self.return_qset_by_hash {
            self.quorum_set.clone()
        } else {
            None
        }
    }

    fn nominating_value(&self, _slot_index: u64, _value: &Value) {}

    fn value_externalized(&self, _slot_index: u64, _value: &Value) {}

    fn ballot_did_prepare(&self, _slot_index: u64, _ballot: &ScpBallot) {}

    fn ballot_did_confirm(&self, _slot_index: u64, _ballot: &ScpBallot) {}

    fn ballot_did_hear_from_quorum(&self, _slot_index: u64, _ballot: &ScpBallot) {
        self.heard_from_quorum.fetch_add(1, Ordering::SeqCst);
    }

    fn compute_hash_node(
        &self,
        _slot_index: u64,
        _prev_value: &Value,
        _is_priority: bool,
        _round: u32,
        _node_id: &NodeId,
    ) -> u64 {
        1
    }

    fn compute_value_hash(
        &self,
        _slot_index: u64,
        _prev_value: &Value,
        _round: u32,
        value: &Value,
    ) -> u64 {
        match self.value_hash_mode {
            ValueHashMode::SumBytes => value.iter().map(|b| *b as u64).sum(),
            ValueHashMode::Fixed(v) => v,
        }
    }

    fn compute_timeout(&self, round: u32, _is_nomination: bool) -> Duration {
        match self.timeout_mode {
            TimeoutMode::Fixed(d) => d,
            TimeoutMode::Linear { base, step } => base + step * round,
        }
    }

    fn sign_envelope(&self, _envelope: &mut ScpEnvelope) {}

    fn get_node_weight(
        &self,
        node_id: &NodeId,
        quorum_set: &ScpQuorumSet,
        is_local_node: bool,
    ) -> u64 {
        if let Some(w) = self.custom_node_weight {
            w
        } else {
            crate::driver::base_get_node_weight(node_id, quorum_set, is_local_node)
        }
    }

    fn verify_envelope(&self, _envelope: &ScpEnvelope) -> bool {
        true
    }
}

// ---------------------------------------------------------------------------
// Common test helpers
// ---------------------------------------------------------------------------

/// Create a deterministic `NodeId` from a single seed byte.
pub fn make_node_id(seed: u8) -> NodeId {
    let mut bytes = [0u8; 32];
    bytes[0] = seed;
    NodeId(PublicKey::PublicKeyTypeEd25519(Uint256(bytes)))
}

/// Create a quorum set with the given validators and threshold.
pub fn make_quorum_set(validators: Vec<NodeId>, threshold: u32) -> ScpQuorumSet {
    ScpQuorumSet {
        threshold,
        validators: validators.try_into().unwrap_or_default(),
        inner_sets: vec![].try_into().unwrap(),
    }
}

/// Create a `Value` from a byte slice.
pub fn make_value(bytes: &[u8]) -> Value {
    bytes.to_vec().try_into().unwrap()
}
