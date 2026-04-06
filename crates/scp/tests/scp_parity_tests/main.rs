//! SCP parity tests ported from SCPTests.cpp.
//!
//! These tests mirror the exact test scenarios from stellar-core v25's
//! src/scp/test/SCPTests.cpp, ensuring behavioral parity between the
//! stellar-core and Rust implementations of the SCP consensus protocol.
//!
//! The test harness (`TestSCP`) matches the stellar-core `TestSCP` class:
//! - Single-node focus with controllable quorum/v-blocking delivery
//! - Append-only envelope tracking for assertion counting
//! - Simulated timer system
//! - Controllable nomination leader priority

use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use henyey_common::Hash256;
use henyey_scp::{EnvelopeState, SCPDriver, SCPTimerType, ValidationLevel, SCP};
use stellar_xdr::curr::{
    Hash, NodeId, PublicKey, ScpBallot, ScpEnvelope, ScpNomination, ScpQuorumSet, ScpStatement,
    ScpStatementConfirm, ScpStatementExternalize, ScpStatementPledges, ScpStatementPrepare,
    Signature, Uint256, Value,
};

// ---------------------------------------------------------------------------
// Test harness: TestSCP (mirrors stellar-core TestSCP class)
// ---------------------------------------------------------------------------

/// Timer data for simulated timers.
struct TimerData {
    absolute_timeout: u64,
    // We don't store the callback; we just track existence and timing.
}

/// Test SCP driver that mirrors the stellar-core TestSCP class.
///
/// Key differences from the production driver:
/// - Append-only `envs` vector (never cleared) — tests assert on `.len()`
/// - Configurable priority lookup for nomination leader control
/// - Simulated timer system with `bump_timer_offset()`
/// - Pre-settable expected candidates and composite value
#[allow(dead_code)]
struct TestSCPDriver {
    node_id: NodeId,
    quorum_set: ScpQuorumSet,
    quorum_set_hash: Hash256,

    /// All emitted envelopes (append-only, matching stellar-core mEnvs).
    envs: RwLock<Vec<ScpEnvelope>>,

    /// Externalized values per slot.
    externalized_values: RwLock<HashMap<u64, Value>>,

    /// Ballots heard from quorum per slot.
    heard_from_quorums: RwLock<HashMap<u64, Vec<ScpBallot>>>,

    /// Stored quorum sets by hash.
    quorum_sets: RwLock<HashMap<Hash256, ScpQuorumSet>>,

    /// Expected candidates for combine_candidates assertion.
    expected_candidates: RwLock<BTreeSet<Value>>,

    /// Composite value to return from combine_candidates.
    composite_value: RwLock<Value>,

    /// Simulated timers.
    timers: RwLock<HashMap<(u64, SCPTimerType), TimerData>>,

    /// Simulated clock offset in milliseconds.
    current_timer_offset: RwLock<u64>,

    /// Custom priority lookup: returns priority for a given node.
    /// Default: local node gets 1000, others get 1.
    /// Mutable via RwLock so tests can change the priority node at runtime
    /// (matching stellar-core mPriorityLookup which is a mutable std::function).
    priority_node: RwLock<NodeId>,

    /// Timeout parameters (matching stellar-core TestSCP).
    initial_ballot_timeout_ms: u32,
    increment_ballot_timeout_ms: u32,
    initial_nomination_timeout_ms: u32,
    increment_nomination_timeout_ms: u32,
}

impl TestSCPDriver {
    fn new(node_id: NodeId, quorum_set: ScpQuorumSet) -> Self {
        let qs_hash = Hash256::hash_xdr(&quorum_set).unwrap_or(Hash256::ZERO);
        let mut quorum_sets = HashMap::new();
        quorum_sets.insert(qs_hash, quorum_set.clone());
        Self {
            node_id: node_id.clone(),
            quorum_set,
            quorum_set_hash: qs_hash,
            envs: RwLock::new(Vec::new()),
            externalized_values: RwLock::new(HashMap::new()),
            heard_from_quorums: RwLock::new(HashMap::new()),
            quorum_sets: RwLock::new(quorum_sets),
            expected_candidates: RwLock::new(BTreeSet::new()),
            composite_value: RwLock::new(Value(vec![].try_into().unwrap())),
            timers: RwLock::new(HashMap::new()),
            current_timer_offset: RwLock::new(0),
            priority_node: RwLock::new(node_id),
            initial_ballot_timeout_ms: 1000,
            increment_ballot_timeout_ms: 1000,
            initial_nomination_timeout_ms: 1000,
            increment_nomination_timeout_ms: 1000,
        }
    }

    fn store_quorum_set(&self, qs: &ScpQuorumSet) {
        let hash = Hash256::hash_xdr(qs).unwrap_or(Hash256::ZERO);
        self.quorum_sets.write().unwrap().insert(hash, qs.clone());
    }

    fn envs_len(&self) -> usize {
        self.envs.read().unwrap().len()
    }

    fn get_env(&self, index: usize) -> ScpEnvelope {
        self.envs.read().unwrap()[index].clone()
    }

    fn heard_from_quorum_count(&self, slot: u64) -> usize {
        self.heard_from_quorums
            .read()
            .unwrap()
            .get(&slot)
            .map_or(0, |v| v.len())
    }

    fn heard_from_quorum_ballot(&self, slot: u64, index: usize) -> ScpBallot {
        self.heard_from_quorums.read().unwrap()[&slot][index].clone()
    }

    fn set_expected_candidates(&self, candidates: BTreeSet<Value>) {
        *self.expected_candidates.write().unwrap() = candidates;
    }

    fn set_composite_value(&self, value: Value) {
        *self.composite_value.write().unwrap() = value;
    }

    fn has_ballot_timer(&self) -> bool {
        self.timers
            .read()
            .unwrap()
            .contains_key(&(0, SCPTimerType::Ballot))
    }

    fn has_ballot_timer_upcoming(&self) -> bool {
        let timers = self.timers.read().unwrap();
        let offset = *self.current_timer_offset.read().unwrap();
        if let Some(td) = timers.get(&(0, SCPTimerType::Ballot)) {
            td.absolute_timeout > offset
        } else {
            false
        }
    }

    fn bump_timer_offset(&self) {
        // Advance simulated time by 5 hours (matching stellar-core)
        let mut offset = self.current_timer_offset.write().unwrap();
        *offset += 5 * 3600 * 1000;
    }

    fn externalized_value(&self, slot: u64) -> Option<Value> {
        self.externalized_values.read().unwrap().get(&slot).cloned()
    }

    /// Change which node has highest priority (1000) in compute_hash_node.
    /// Matches stellar-core `scp.mPriorityLookup = [&](NodeID const& n) { ... }`.
    fn set_priority_node(&self, node: NodeId) {
        *self.priority_node.write().unwrap() = node;
    }
}

impl SCPDriver for TestSCPDriver {
    fn validate_value(
        &self,
        _slot_index: u64,
        _value: &Value,
        _nomination: bool,
    ) -> ValidationLevel {
        ValidationLevel::FullyValidated
    }

    fn combine_candidates(&self, _slot_index: u64, candidates: &[Value]) -> Option<Value> {
        let expected = self.expected_candidates.read().unwrap();
        if !expected.is_empty() {
            let actual: BTreeSet<Value> = candidates.iter().cloned().collect();
            assert_eq!(
                *expected, actual,
                "combine_candidates: unexpected candidates"
            );
        }
        Some(self.composite_value.read().unwrap().clone())
    }

    fn extract_valid_value(&self, _slot_index: u64, value: &Value) -> Option<Value> {
        Some(value.clone())
    }

    fn emit_envelope(&self, envelope: &ScpEnvelope) {
        self.envs.write().unwrap().push(envelope.clone());
    }

    fn get_quorum_set(&self, node_id: &NodeId) -> Option<ScpQuorumSet> {
        // All nodes share the same quorum set in these tests
        let _ = node_id;
        Some(self.quorum_set.clone())
    }

    fn get_quorum_set_by_hash(&self, hash: &Hash256) -> Option<ScpQuorumSet> {
        self.quorum_sets.read().unwrap().get(hash).cloned()
    }

    fn nominating_value(&self, _slot_index: u64, _value: &Value) {}

    fn value_externalized(&self, slot_index: u64, value: &Value) {
        let mut ext = self.externalized_values.write().unwrap();
        assert!(
            !ext.contains_key(&slot_index),
            "value already externalized for slot {}",
            slot_index
        );
        ext.insert(slot_index, value.clone());
    }

    fn ballot_did_prepare(&self, _slot_index: u64, _ballot: &ScpBallot) {}

    fn ballot_did_confirm(&self, _slot_index: u64, _ballot: &ScpBallot) {}

    fn ballot_did_hear_from_quorum(&self, slot_index: u64, ballot: &ScpBallot) {
        self.heard_from_quorums
            .write()
            .unwrap()
            .entry(slot_index)
            .or_default()
            .push(ballot.clone());
    }

    fn compute_hash_node(
        &self,
        _slot_index: u64,
        _prev_value: &Value,
        is_priority: bool,
        _round: u32,
        node_id: &NodeId,
    ) -> u64 {
        if is_priority {
            // Priority node gets highest priority (1000), others get 1
            if node_id == &*self.priority_node.read().unwrap() {
                1000
            } else {
                1
            }
        } else {
            0
        }
    }

    fn compute_value_hash(
        &self,
        _slot_index: u64,
        _prev_value: &Value,
        _round: u32,
        _value: &Value,
    ) -> u64 {
        0
    }

    fn compute_timeout(&self, round: u32, is_nomination: bool) -> Duration {
        let (initial, increment) = if is_nomination {
            (
                self.initial_nomination_timeout_ms,
                self.increment_nomination_timeout_ms,
            )
        } else {
            (
                self.initial_ballot_timeout_ms,
                self.increment_ballot_timeout_ms,
            )
        };
        let ms = initial as u64 + (round.saturating_sub(1) as u64) * increment as u64;
        let max_timeout_ms = 30 * 60 * 1000; // 30 minutes
        Duration::from_millis(ms.min(max_timeout_ms))
    }

    fn sign_envelope(&self, _envelope: &mut ScpEnvelope) {
        // No-op for tests (matching stellar-core)
    }

    fn verify_envelope(&self, _envelope: &ScpEnvelope) -> bool {
        true
    }

    fn setup_timer(&self, slot_index: u64, timer_type: SCPTimerType, timeout: Duration) {
        let offset = *self.current_timer_offset.read().unwrap();
        let absolute = offset + timeout.as_millis() as u64;
        self.timers.write().unwrap().insert(
            (slot_index, timer_type),
            TimerData {
                absolute_timeout: absolute,
            },
        );
    }

    fn stop_timer(&self, slot_index: u64, timer_type: SCPTimerType) {
        self.timers
            .write()
            .unwrap()
            .remove(&(slot_index, timer_type));
    }
}

// ---------------------------------------------------------------------------
// Wrapper for TestSCP that owns SCP + driver
// ---------------------------------------------------------------------------

struct TestSCP {
    scp: SCP<TestSCPDriver>,
}

impl TestSCP {
    fn new(node_id: NodeId, quorum_set: ScpQuorumSet) -> Self {
        let driver = TestSCPDriver::new(node_id.clone(), quorum_set.clone());
        let scp = SCP::new(node_id, true, quorum_set, Arc::new(driver));
        Self { scp }
    }

    fn new_non_validator(node_id: NodeId, quorum_set: ScpQuorumSet) -> Self {
        let driver = TestSCPDriver::new(node_id.clone(), quorum_set.clone());
        let scp = SCP::new(node_id, false, quorum_set, Arc::new(driver));
        Self { scp }
    }

    fn new_with_priority(node_id: NodeId, quorum_set: ScpQuorumSet, priority_node: NodeId) -> Self {
        let driver = TestSCPDriver::new(node_id.clone(), quorum_set.clone());
        driver.set_priority_node(priority_node);
        let scp = SCP::new(node_id, true, quorum_set, Arc::new(driver));
        Self { scp }
    }

    fn driver(&self) -> &TestSCPDriver {
        &self.scp.driver()
    }

    fn bump_state(&self, slot: u64, value: Value) -> bool {
        self.scp.force_bump_state(slot, value)
    }

    fn receive_envelope(&self, envelope: ScpEnvelope) -> EnvelopeState {
        self.scp.receive_envelope(envelope)
    }

    fn envs_len(&self) -> usize {
        self.driver().envs_len()
    }

    fn get_env(&self, index: usize) -> ScpEnvelope {
        self.driver().get_env(index)
    }

    fn has_ballot_timer(&self) -> bool {
        self.driver().has_ballot_timer()
    }

    fn has_ballot_timer_upcoming(&self) -> bool {
        self.driver().has_ballot_timer_upcoming()
    }

    fn bump_timer_offset(&self) {
        self.driver().bump_timer_offset()
    }

    fn heard_from_quorum_count(&self, slot: u64) -> usize {
        self.driver().heard_from_quorum_count(slot)
    }

    fn heard_from_quorum_ballot(&self, slot: u64, index: usize) -> ScpBallot {
        self.driver().heard_from_quorum_ballot(slot, index)
    }

    fn externalized_value(&self, slot: u64) -> Option<Value> {
        self.driver().externalized_value(slot)
    }

    fn externalized_value_count(&self) -> usize {
        self.driver().externalized_values.read().unwrap().len()
    }

    fn fire_ballot_timer(&self) {
        // Matches stellar-core getBallotProtocolTimer().mCallback() which calls abandonBallot(0)
        self.scp.abandon_ballot(0, 0);
    }

    fn store_quorum_set(&self, qs: &ScpQuorumSet) {
        self.driver().store_quorum_set(qs);
    }

    fn set_priority_node(&self, node: NodeId) {
        self.driver().set_priority_node(node);
    }

    fn nominate(&self, slot: u64, value: Value, prev_value: &Value) -> bool {
        self.scp.nominate(slot, value, prev_value)
    }

    fn nominate_timeout(&self, slot: u64, value: Value, prev_value: &Value) -> bool {
        self.scp.nominate_timeout(slot, value, prev_value)
    }

    /// Get the current envelope for a specific node in a slot, including self
    /// even when not fully validated. Matches stellar-core `getCurrentEnvelope(index, nodeID)`.
    fn get_current_envelope(&self, slot_index: u64, node_id: &NodeId) -> ScpEnvelope {
        let envs = self.scp.get_entire_current_state(slot_index);
        envs.into_iter()
            .find(|e| &e.statement.node_id == node_id)
            .expect("getCurrentEnvelope: envelope not found for node")
    }

    /// Set SCP state from a saved envelope (for crash recovery testing).
    /// Matches stellar-core `mSCP.setStateFromEnvelope(slotIndex, envelope)`.
    fn set_state_from_envelope(&self, envelope: &ScpEnvelope) {
        self.scp.set_state_from_envelope(envelope);
    }

    /// Get the nomination leaders for slot 0.
    /// Matches stellar-core `scp.getNominationLeaders(0)`.
    #[allow(dead_code)]
    fn get_nomination_leaders(&self) -> std::collections::BTreeSet<NodeId> {
        self.scp.get_nomination_leaders(0)
    }

    /// Get the latest composite candidate value for a slot.
    /// Matches stellar-core `scp.getLatestCompositeCandidate(slotIndex)`.
    #[allow(dead_code)]
    fn get_latest_composite_candidate(&self, slot_index: u64) -> Option<Value> {
        self.scp.get_latest_composite_candidate(slot_index)
    }

    /// Check if a nomination timer is currently set for slot 0.
    /// Matches stellar-core `scp.mTimers.find(Slot::NOMINATION_TIMER) != scp.mTimers.end()`.
    #[allow(dead_code)]
    fn has_nomination_timer(&self) -> bool {
        self.driver()
            .timers
            .read()
            .unwrap()
            .contains_key(&(0, SCPTimerType::Nomination))
    }
}

// ---------------------------------------------------------------------------
// Node identity helpers
// ---------------------------------------------------------------------------

/// Create a deterministic node ID from a seed byte.
fn make_node_id(seed: u8) -> NodeId {
    // Matches stellar-core SIMULATION_CREATE_NODE pattern with deterministic bytes
    let mut bytes = [0u8; 32];
    bytes[0] = seed;
    NodeId(PublicKey::PublicKeyTypeEd25519(Uint256(bytes)))
}

// Fixed node IDs for the 5-node test topology
fn v0_id() -> NodeId {
    make_node_id(0)
}
fn v1_id() -> NodeId {
    make_node_id(1)
}
fn v2_id() -> NodeId {
    make_node_id(2)
}
fn v3_id() -> NodeId {
    make_node_id(3)
}
fn v4_id() -> NodeId {
    make_node_id(4)
}

// ---------------------------------------------------------------------------
// Value setup (matching stellar-core setupValues)
// ---------------------------------------------------------------------------

/// Create deterministic test values where x_value < y_value < z_value < zz_value.
fn setup_values() -> (Value, Value, Value, Value) {
    let mut values: Vec<Value> = (0u8..4)
        .map(|i| {
            let mut bytes = vec![0u8; 32];
            bytes[0] = i + 1; // 1, 2, 3, 4 — ensures distinct values
            Value(bytes.try_into().unwrap())
        })
        .collect();
    values.sort();
    let x = values[0].clone();
    let y = values[1].clone();
    let z = values[2].clone();
    let zz = values[3].clone();
    assert!(x < y);
    assert!(y < z);
    assert!(z < zz);
    (x, y, z, zz)
}

// ---------------------------------------------------------------------------
// Quorum set helpers
// ---------------------------------------------------------------------------

/// Create the standard 5-node quorum set with threshold 4.
fn make_core5_quorum_set() -> ScpQuorumSet {
    ScpQuorumSet {
        threshold: 4,
        validators: vec![v0_id(), v1_id(), v2_id(), v3_id(), v4_id()]
            .try_into()
            .unwrap(),
        inner_sets: vec![].try_into().unwrap(),
    }
}

/// Create a 3-node quorum set with threshold 2.
fn make_core3_quorum_set() -> ScpQuorumSet {
    ScpQuorumSet {
        threshold: 2,
        validators: vec![v0_id(), v1_id(), v2_id()].try_into().unwrap(),
        inner_sets: vec![].try_into().unwrap(),
    }
}

fn quorum_set_hash(qs: &ScpQuorumSet) -> Hash256 {
    Hash256::hash_xdr(qs).unwrap_or(Hash256::ZERO)
}

// ---------------------------------------------------------------------------
// Envelope construction helpers (matching stellar-core makePrepare, etc.)
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn make_prepare(
    node_id: &NodeId,
    q_set_hash: Hash256,
    slot_index: u64,
    ballot: &ScpBallot,
    prepared: Option<&ScpBallot>,
    n_c: u32,
    n_h: u32,
    prepared_prime: Option<&ScpBallot>,
) -> ScpEnvelope {
    ScpEnvelope {
        statement: ScpStatement {
            node_id: node_id.clone(),
            slot_index,
            pledges: ScpStatementPledges::Prepare(ScpStatementPrepare {
                quorum_set_hash: Hash(q_set_hash.0),
                ballot: ballot.clone(),
                prepared: prepared.cloned(),
                prepared_prime: prepared_prime.cloned(),
                n_c,
                n_h,
            }),
        },
        signature: Signature(vec![0u8; 64].try_into().unwrap()),
    }
}

fn make_confirm(
    node_id: &NodeId,
    q_set_hash: Hash256,
    slot_index: u64,
    n_prepared: u32,
    ballot: &ScpBallot,
    n_c: u32,
    n_h: u32,
) -> ScpEnvelope {
    ScpEnvelope {
        statement: ScpStatement {
            node_id: node_id.clone(),
            slot_index,
            pledges: ScpStatementPledges::Confirm(ScpStatementConfirm {
                ballot: ballot.clone(),
                n_prepared,
                n_commit: n_c,
                n_h,
                quorum_set_hash: Hash(q_set_hash.0),
            }),
        },
        signature: Signature(vec![0u8; 64].try_into().unwrap()),
    }
}

fn make_externalize(
    node_id: &NodeId,
    q_set_hash: Hash256,
    slot_index: u64,
    commit_ballot: &ScpBallot,
    n_h: u32,
) -> ScpEnvelope {
    ScpEnvelope {
        statement: ScpStatement {
            node_id: node_id.clone(),
            slot_index,
            pledges: ScpStatementPledges::Externalize(ScpStatementExternalize {
                commit: commit_ballot.clone(),
                n_h,
                commit_quorum_set_hash: Hash(q_set_hash.0),
            }),
        },
        signature: Signature(vec![0u8; 64].try_into().unwrap()),
    }
}

fn make_nominate(
    node_id: &NodeId,
    q_set_hash: Hash256,
    slot_index: u64,
    votes: Vec<Value>,
    accepted: Vec<Value>,
) -> ScpEnvelope {
    let mut sorted_votes = votes;
    sorted_votes.sort();
    let mut sorted_accepted = accepted;
    sorted_accepted.sort();

    ScpEnvelope {
        statement: ScpStatement {
            node_id: node_id.clone(),
            slot_index,
            pledges: ScpStatementPledges::Nominate(ScpNomination {
                quorum_set_hash: Hash(q_set_hash.0),
                votes: sorted_votes.try_into().unwrap(),
                accepted: sorted_accepted.try_into().unwrap(),
            }),
        },
        signature: Signature(vec![0u8; 64].try_into().unwrap()),
    }
}

// ---------------------------------------------------------------------------
// Verification helpers (matching stellar-core verifyPrepare, etc.)
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn verify_prepare(
    actual: &ScpEnvelope,
    expected_node: &NodeId,
    q_set_hash: Hash256,
    slot_index: u64,
    ballot: &ScpBallot,
    prepared: Option<&ScpBallot>,
    n_c: u32,
    n_h: u32,
    prepared_prime: Option<&ScpBallot>,
) {
    let expected = make_prepare(
        expected_node,
        q_set_hash,
        slot_index,
        ballot,
        prepared,
        n_c,
        n_h,
        prepared_prime,
    );
    assert_eq!(
        actual.statement, expected.statement,
        "PREPARE verification failed.\nActual:   {:?}\nExpected: {:?}",
        actual.statement.pledges, expected.statement.pledges
    );
}

#[allow(clippy::too_many_arguments)]
fn verify_confirm(
    actual: &ScpEnvelope,
    expected_node: &NodeId,
    q_set_hash: Hash256,
    slot_index: u64,
    n_prepared: u32,
    ballot: &ScpBallot,
    n_c: u32,
    n_h: u32,
) {
    let expected = make_confirm(
        expected_node,
        q_set_hash,
        slot_index,
        n_prepared,
        ballot,
        n_c,
        n_h,
    );
    assert_eq!(
        actual.statement, expected.statement,
        "CONFIRM verification failed.\nActual:   {:?}\nExpected: {:?}",
        actual.statement.pledges, expected.statement.pledges
    );
}

fn verify_externalize(
    actual: &ScpEnvelope,
    expected_node: &NodeId,
    q_set_hash: Hash256,
    slot_index: u64,
    commit_ballot: &ScpBallot,
    n_h: u32,
) {
    let expected = make_externalize(expected_node, q_set_hash, slot_index, commit_ballot, n_h);
    assert_eq!(
        actual.statement, expected.statement,
        "EXTERNALIZE verification failed.\nActual:   {:?}\nExpected: {:?}",
        actual.statement.pledges, expected.statement.pledges
    );
}

fn verify_nominate(
    actual: &ScpEnvelope,
    expected_node: &NodeId,
    q_set_hash: Hash256,
    slot_index: u64,
    votes: Vec<Value>,
    accepted: Vec<Value>,
) {
    let expected = make_nominate(expected_node, q_set_hash, slot_index, votes, accepted);
    assert_eq!(
        actual.statement, expected.statement,
        "NOMINATE verification failed.\nActual:   {:?}\nExpected: {:?}",
        actual.statement.pledges, expected.statement.pledges
    );
}

// ---------------------------------------------------------------------------
// Envelope generator type (matching stellar-core genEnvelope)
// ---------------------------------------------------------------------------

/// A generator that takes a node ID and produces an envelope.
/// This mirrors the stellar-core `genEnvelope` typedef.
type GenEnvelope = Box<dyn Fn(&NodeId) -> ScpEnvelope>;

/// Create a PREPARE envelope generator with pre-bound parameters.
fn make_prepare_gen(
    q_set_hash: Hash256,
    ballot: ScpBallot,
    prepared: Option<ScpBallot>,
    n_c: u32,
    n_h: u32,
    prepared_prime: Option<ScpBallot>,
) -> GenEnvelope {
    Box::new(move |node_id: &NodeId| {
        make_prepare(
            node_id,
            q_set_hash,
            0,
            &ballot,
            prepared.as_ref(),
            n_c,
            n_h,
            prepared_prime.as_ref(),
        )
    })
}

/// Create a CONFIRM envelope generator with pre-bound parameters.
fn make_confirm_gen(
    q_set_hash: Hash256,
    n_prepared: u32,
    ballot: ScpBallot,
    n_c: u32,
    n_h: u32,
) -> GenEnvelope {
    Box::new(move |node_id: &NodeId| {
        make_confirm(node_id, q_set_hash, 0, n_prepared, &ballot, n_c, n_h)
    })
}

/// Create an EXTERNALIZE envelope generator with pre-bound parameters.
fn make_externalize_gen(q_set_hash: Hash256, commit_ballot: ScpBallot, n_h: u32) -> GenEnvelope {
    Box::new(move |node_id: &NodeId| make_externalize(node_id, q_set_hash, 0, &commit_ballot, n_h))
}

// ---------------------------------------------------------------------------
// Multi-node message delivery helpers (matching stellar-core recvQuorum/recvVBlocking)
// ---------------------------------------------------------------------------

/// Deliver envelopes from a v-blocking set (v1, v2) to the test SCP node.
/// In a 5-node quorum with threshold=4, any 2 nodes form a v-blocking set.
///
/// After delivery, asserts exactly 1 new envelope was emitted (if with_checks).
fn recv_v_blocking_checks(scp: &TestSCP, gen: &GenEnvelope, with_checks: bool) {
    let e1 = gen(&v1_id());
    let e2 = gen(&v2_id());

    scp.bump_timer_offset();

    let i = scp.envs_len();
    scp.receive_envelope(e1);
    if with_checks {
        assert_eq!(
            scp.envs_len(),
            i,
            "v-blocking: first message should not emit"
        );
    }
    scp.receive_envelope(e2);
    if with_checks {
        assert_eq!(
            scp.envs_len(),
            i + 1,
            "v-blocking: second message should emit exactly 1"
        );
    }
}

fn recv_v_blocking(scp: &TestSCP, gen: &GenEnvelope) {
    recv_v_blocking_checks(scp, gen, true);
}

/// Deliver envelopes from a quorum (v1, v2, v3, v4) to the test SCP node.
/// In a 5-node quorum with threshold=4: v0 (self) + v1 + v2 + v3 = quorum after 3 messages.
///
/// Asserts exactly 1 new envelope was emitted after quorum is reached (if with_checks).
fn recv_quorum_checks_ex(
    scp: &TestSCP,
    gen: &GenEnvelope,
    with_checks: bool,
    delayed_quorum: bool,
    check_upcoming: bool,
) {
    let e1 = gen(&v1_id());
    let e2 = gen(&v2_id());
    let e3 = gen(&v3_id());
    let e4 = gen(&v4_id());

    scp.bump_timer_offset();

    scp.receive_envelope(e1);
    scp.receive_envelope(e2);
    let i = scp.envs_len() + 1;
    scp.receive_envelope(e3);
    if with_checks && !delayed_quorum {
        assert_eq!(
            scp.envs_len(),
            i,
            "quorum: 3rd message should produce exactly 1 new envelope"
        );
    }
    if check_upcoming && !delayed_quorum {
        assert!(
            scp.has_ballot_timer_upcoming(),
            "quorum: ballot timer should be upcoming"
        );
    }
    // 4th message: nothing extra (unless delayed quorum)
    scp.receive_envelope(e4);
    if with_checks && delayed_quorum {
        assert_eq!(
            scp.envs_len(),
            i,
            "delayed quorum: 4th message should produce exactly 1 new envelope"
        );
    }
    if check_upcoming && delayed_quorum {
        assert!(
            scp.has_ballot_timer_upcoming(),
            "delayed quorum: ballot timer should be upcoming"
        );
    }
}

fn recv_quorum_ex(scp: &TestSCP, gen: &GenEnvelope, check_upcoming: bool) {
    recv_quorum_checks_ex(scp, gen, true, false, check_upcoming);
}

fn recv_quorum(scp: &TestSCP, gen: &GenEnvelope) {
    recv_quorum_checks_ex(scp, gen, true, false, false);
}

// ---------------------------------------------------------------------------
// Common test setup: nodesAllPledgeToCommit
// ---------------------------------------------------------------------------

/// Drive the ballot protocol through the sequence where all nodes pledge to
/// commit value x. After this, `scp.envs` has 3 entries:
/// 0: PREPARE(1,x)
/// 1: PREPARE(1,x) prepared=(1,x)
/// 2: PREPARE(1,x) prepared=(1,x) nC=1 nH=1
fn nodes_all_pledge_to_commit(scp: &TestSCP, x_value: &Value, qs_hash: Hash256) {
    let qs_hash0 = quorum_set_hash(&scp.scp.local_quorum_set());
    let b = ScpBallot {
        counter: 1,
        value: x_value.clone(),
    };

    assert!(scp.bump_state(0, x_value.clone()));
    assert_eq!(scp.envs_len(), 1);

    verify_prepare(&scp.get_env(0), &v0_id(), qs_hash0, 0, &b, None, 0, 0, None);

    // Receive PREPARE from v1, v2, v3 (quorum)
    let prepare1 = make_prepare(&v1_id(), qs_hash, 0, &b, None, 0, 0, None);
    let prepare2 = make_prepare(&v2_id(), qs_hash, 0, &b, None, 0, 0, None);
    let prepare3 = make_prepare(&v3_id(), qs_hash, 0, &b, None, 0, 0, None);
    let prepare4 = make_prepare(&v4_id(), qs_hash, 0, &b, None, 0, 0, None);

    scp.receive_envelope(prepare1);
    assert_eq!(scp.envs_len(), 1);
    assert_eq!(scp.heard_from_quorum_count(0), 0);

    scp.receive_envelope(prepare2);
    assert_eq!(scp.envs_len(), 1);
    assert_eq!(scp.heard_from_quorum_count(0), 0);

    scp.receive_envelope(prepare3);
    assert_eq!(scp.envs_len(), 2);
    assert_eq!(scp.heard_from_quorum_count(0), 1);
    assert_eq!(scp.heard_from_quorum_ballot(0, 0), b);

    // Quorum including us → emits PREPARE with prepared set
    verify_prepare(
        &scp.get_env(1),
        &v0_id(),
        qs_hash0,
        0,
        &b,
        Some(&b),
        0,
        0,
        None,
    );

    scp.receive_envelope(prepare4);
    assert_eq!(scp.envs_len(), 2);

    // Now receive PREPARE-with-prepared from quorum to confirm prepared
    let prepared2 = make_prepare(&v2_id(), qs_hash, 0, &b, Some(&b), 0, 0, None);
    let prepared3 = make_prepare(&v3_id(), qs_hash, 0, &b, Some(&b), 0, 0, None);
    let prepared4 = make_prepare(&v4_id(), qs_hash, 0, &b, Some(&b), 0, 0, None);

    scp.receive_envelope(prepared4);
    scp.receive_envelope(prepared3);
    assert_eq!(scp.envs_len(), 2);

    scp.receive_envelope(prepared2);
    assert_eq!(scp.envs_len(), 3);

    // Confirms prepared: nC=1, nH=1
    verify_prepare(
        &scp.get_env(2),
        &v0_id(),
        qs_hash0,
        0,
        &b,
        Some(&b),
        b.counter,
        b.counter,
        None,
    );
}

mod ballot_core5;
pub(crate) use ballot_core5::setup_confirm_prepared_a2;
mod ballot_deep;
mod ballot_scenarios;
mod ballot_z_branch;
mod core3;
mod nomination;
mod pristine_ballot;
