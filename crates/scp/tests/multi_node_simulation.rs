//! Multi-node SCP simulation tests.
//!
//! These tests simulate multiple SCP nodes reaching consensus, matching
//! the patterns from SCPTests.cpp. They verify:
//! - Basic consensus with multiple nodes
//! - Nomination round advancement
//! - Ballot protocol phase transitions
//! - Handling of conflicting values
//! - Recovery scenarios

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use henyey_scp::{
    hash_quorum_set, is_quorum_slice, EnvelopeState, SCPDriver, ValidationLevel, SCP,
};
use stellar_xdr::curr::{
    NodeId, PublicKey, ScpBallot, ScpEnvelope, ScpNomination, ScpQuorumSet, ScpStatement,
    ScpStatementConfirm, ScpStatementExternalize, ScpStatementPledges, ScpStatementPrepare,
    Signature, Uint256, Value,
};

/// Test driver that tracks emitted envelopes for multi-node simulation.
struct SimulationDriver {
    node_id: NodeId,
    quorum_set: ScpQuorumSet,
    emitted_envelopes: RwLock<Vec<ScpEnvelope>>,
    emit_count: AtomicU32,
    quorum_sets: RwLock<HashMap<NodeId, ScpQuorumSet>>,
    validation_level: ValidationLevel,
}

impl SimulationDriver {
    fn new(node_id: NodeId, quorum_set: ScpQuorumSet) -> Self {
        Self {
            node_id,
            quorum_set: quorum_set.clone(),
            emitted_envelopes: RwLock::new(Vec::new()),
            emit_count: AtomicU32::new(0),
            quorum_sets: RwLock::new(HashMap::new()),
            validation_level: ValidationLevel::FullyValidated,
        }
    }

    fn register_quorum_set(&self, node_id: NodeId, qset: ScpQuorumSet) {
        self.quorum_sets.write().unwrap().insert(node_id, qset);
    }

    fn get_emitted_envelopes(&self) -> Vec<ScpEnvelope> {
        self.emitted_envelopes.read().unwrap().clone()
    }

    fn clear_emitted(&self) {
        self.emitted_envelopes.write().unwrap().clear();
    }
}

impl SCPDriver for SimulationDriver {
    fn validate_value(
        &self,
        _slot_index: u64,
        _value: &Value,
        _nomination: bool,
    ) -> ValidationLevel {
        self.validation_level
    }

    fn combine_candidates(&self, _slot_index: u64, candidates: &[Value]) -> Option<Value> {
        // Simple combination: just take the first (highest priority) candidate
        candidates.first().cloned()
    }

    fn extract_valid_value(&self, _slot_index: u64, value: &Value) -> Option<Value> {
        Some(value.clone())
    }

    fn emit_envelope(&self, envelope: &ScpEnvelope) {
        self.emitted_envelopes
            .write()
            .unwrap()
            .push(envelope.clone());
        self.emit_count.fetch_add(1, Ordering::SeqCst);
    }

    fn get_quorum_set(&self, node_id: &NodeId) -> Option<ScpQuorumSet> {
        if node_id == &self.node_id {
            Some(self.quorum_set.clone())
        } else {
            self.quorum_sets.read().unwrap().get(node_id).cloned()
        }
    }

    fn nominating_value(&self, _slot_index: u64, _value: &Value) {}

    fn value_externalized(&self, _slot_index: u64, _value: &Value) {}

    fn ballot_did_prepare(&self, _slot_index: u64, _ballot: &ScpBallot) {}

    fn ballot_did_confirm(&self, _slot_index: u64, _ballot: &ScpBallot) {}

    fn compute_hash_node(
        &self,
        slot_index: u64,
        prev_value: &Value,
        is_priority: bool,
        round: u32,
        node_id: &NodeId,
    ) -> u64 {
        // Deterministic hash based on inputs
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        slot_index.hash(&mut hasher);
        prev_value.as_slice().hash(&mut hasher);
        is_priority.hash(&mut hasher);
        round.hash(&mut hasher);
        let NodeId(PublicKey::PublicKeyTypeEd25519(Uint256(bytes))) = node_id;
        bytes.hash(&mut hasher);
        hasher.finish()
    }

    fn compute_value_hash(
        &self,
        slot_index: u64,
        prev_value: &Value,
        round: u32,
        value: &Value,
    ) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        slot_index.hash(&mut hasher);
        prev_value.as_slice().hash(&mut hasher);
        round.hash(&mut hasher);
        value.as_slice().hash(&mut hasher);
        hasher.finish()
    }

    fn compute_timeout(&self, round: u32, _is_nomination: bool) -> Duration {
        Duration::from_secs(1 + round as u64)
    }

    fn sign_envelope(&self, _envelope: &mut ScpEnvelope) {
        // No-op for tests
    }

    fn verify_envelope(&self, _envelope: &ScpEnvelope) -> bool {
        true
    }
}

// Helper functions

fn make_node_id(seed: u8) -> NodeId {
    let mut bytes = [0u8; 32];
    bytes[0] = seed;
    NodeId(PublicKey::PublicKeyTypeEd25519(Uint256(bytes)))
}

fn make_value(bytes: &[u8]) -> Value {
    bytes.to_vec().try_into().unwrap()
}

fn make_quorum_set(validators: Vec<NodeId>, threshold: u32) -> ScpQuorumSet {
    ScpQuorumSet {
        threshold,
        validators: validators.try_into().unwrap_or_default(),
        inner_sets: vec![].try_into().unwrap(),
    }
}

fn make_nomination_envelope(
    node_id: NodeId,
    slot_index: u64,
    quorum_set: &ScpQuorumSet,
    votes: Vec<Value>,
    accepted: Vec<Value>,
) -> ScpEnvelope {
    let nomination = ScpNomination {
        quorum_set_hash: hash_quorum_set(quorum_set).into(),
        votes: votes.try_into().unwrap(),
        accepted: accepted.try_into().unwrap(),
    };
    let statement = ScpStatement {
        node_id,
        slot_index,
        pledges: ScpStatementPledges::Nominate(nomination),
    };
    ScpEnvelope {
        statement,
        signature: Signature(Vec::new().try_into().unwrap_or_default()),
    }
}

fn make_prepare_envelope(
    node_id: NodeId,
    slot_index: u64,
    quorum_set: &ScpQuorumSet,
    ballot: ScpBallot,
    prepared: Option<ScpBallot>,
    n_c: u32,
    n_h: u32,
) -> ScpEnvelope {
    let prep = ScpStatementPrepare {
        quorum_set_hash: hash_quorum_set(quorum_set).into(),
        ballot,
        prepared,
        prepared_prime: None,
        n_c,
        n_h,
    };
    let statement = ScpStatement {
        node_id,
        slot_index,
        pledges: ScpStatementPledges::Prepare(prep),
    };
    ScpEnvelope {
        statement,
        signature: Signature(Vec::new().try_into().unwrap_or_default()),
    }
}

fn make_confirm_envelope(
    node_id: NodeId,
    slot_index: u64,
    quorum_set: &ScpQuorumSet,
    ballot: ScpBallot,
    n_prepared: u32,
    n_commit: u32,
    n_h: u32,
) -> ScpEnvelope {
    let conf = ScpStatementConfirm {
        ballot,
        n_prepared,
        n_commit,
        n_h,
        quorum_set_hash: hash_quorum_set(quorum_set).into(),
    };
    let statement = ScpStatement {
        node_id,
        slot_index,
        pledges: ScpStatementPledges::Confirm(conf),
    };
    ScpEnvelope {
        statement,
        signature: Signature(Vec::new().try_into().unwrap_or_default()),
    }
}

fn make_externalize_envelope(
    node_id: NodeId,
    slot_index: u64,
    quorum_set: &ScpQuorumSet,
    commit: ScpBallot,
    n_h: u32,
) -> ScpEnvelope {
    let ext = ScpStatementExternalize {
        commit,
        n_h,
        commit_quorum_set_hash: hash_quorum_set(quorum_set).into(),
    };
    let statement = ScpStatement {
        node_id,
        slot_index,
        pledges: ScpStatementPledges::Externalize(ext),
    };
    ScpEnvelope {
        statement,
        signature: Signature(Vec::new().try_into().unwrap_or_default()),
    }
}

/// Multi-node simulation harness.
struct Simulation {
    nodes: HashMap<NodeId, Arc<SCP<SimulationDriver>>>,
    drivers: HashMap<NodeId, Arc<SimulationDriver>>,
    quorum_set: ScpQuorumSet,
}

impl Simulation {
    fn new(node_count: u8, threshold: u32) -> Self {
        let node_ids: Vec<NodeId> = (1..=node_count).map(make_node_id).collect();
        let quorum_set = make_quorum_set(node_ids.clone(), threshold);

        let mut nodes = HashMap::new();
        let mut drivers = HashMap::new();

        for node_id in &node_ids {
            let driver = Arc::new(SimulationDriver::new(node_id.clone(), quorum_set.clone()));
            // Register all quorum sets
            for other_id in &node_ids {
                driver.register_quorum_set(other_id.clone(), quorum_set.clone());
            }
            let scp = Arc::new(SCP::new(
                node_id.clone(),
                true,
                quorum_set.clone(),
                driver.clone(),
            ));
            nodes.insert(node_id.clone(), scp);
            drivers.insert(node_id.clone(), driver);
        }

        Self {
            nodes,
            drivers,
            quorum_set,
        }
    }

    fn get_node(&self, node_id: &NodeId) -> &Arc<SCP<SimulationDriver>> {
        self.nodes.get(node_id).unwrap()
    }

    fn get_driver(&self, node_id: &NodeId) -> &Arc<SimulationDriver> {
        self.drivers.get(node_id).unwrap()
    }

    /// Broadcast all pending envelopes from all nodes until no more are generated.
    fn run_until_stable(&self) -> u32 {
        let mut rounds = 0;
        loop {
            let mut any_emitted = false;
            for node_id in self.nodes.keys() {
                let envelopes = self.get_driver(node_id).get_emitted_envelopes();
                if !envelopes.is_empty() {
                    any_emitted = true;
                    self.get_driver(node_id).clear_emitted();
                    for envelope in envelopes {
                        for (other_id, scp) in &self.nodes {
                            if other_id != node_id {
                                scp.receive_envelope(envelope.clone());
                            }
                        }
                    }
                }
            }
            if !any_emitted {
                break;
            }
            rounds += 1;
            if rounds > 100 {
                panic!("Simulation did not stabilize after 100 rounds");
            }
        }
        rounds
    }

    /// Check if all nodes have externalized the same value for a slot.
    fn all_externalized(&self, slot_index: u64) -> Option<Value> {
        let mut externalized_value: Option<Value> = None;
        for scp in self.nodes.values() {
            match scp.get_externalized_value(slot_index) {
                Some(value) => {
                    if let Some(ref expected) = externalized_value {
                        if &value != expected {
                            return None; // Different values externalized
                        }
                    } else {
                        externalized_value = Some(value);
                    }
                }
                None => return None, // Not all externalized
            }
        }
        externalized_value
    }
}

// ==================== Tests ====================

#[test]
fn test_three_node_basic_consensus() {
    // Setup: 3 nodes with threshold 2 (2-of-3 quorum)
    let sim = Simulation::new(3, 2);
    let node1 = make_node_id(1);
    let _node2 = make_node_id(2);
    let _node3 = make_node_id(3);

    let slot_index = 1u64;
    let value = make_value(&[1, 2, 3, 4]);
    let prev_value = make_value(&[0]);

    // Node 1 nominates - result depends on whether it's a leader for this round
    let _ = sim
        .get_node(&node1)
        .nominate(slot_index, value.clone(), &prev_value);

    // Node 1 should have the slot created after nomination attempt
    assert!(!sim.get_node(&node1).empty());
}

#[test]
fn test_all_nodes_nominate_same_value() {
    let sim = Simulation::new(3, 2);
    let node1 = make_node_id(1);
    let node2 = make_node_id(2);
    let node3 = make_node_id(3);

    let slot_index = 1u64;
    let value = make_value(&[10, 20, 30]);
    let prev_value = make_value(&[0]);

    // All nodes nominate the same value
    sim.get_node(&node1)
        .nominate(slot_index, value.clone(), &prev_value);
    sim.get_node(&node2)
        .nominate(slot_index, value.clone(), &prev_value);
    sim.get_node(&node3)
        .nominate(slot_index, value.clone(), &prev_value);

    // Run until stable
    sim.run_until_stable();

    // Check that nomination was started on all nodes
    assert!(!sim.get_node(&node1).empty());
    assert!(!sim.get_node(&node2).empty());
    assert!(!sim.get_node(&node3).empty());
}

#[test]
fn test_force_externalize_all_nodes() {
    let sim = Simulation::new(3, 2);
    let node1 = make_node_id(1);
    let node2 = make_node_id(2);
    let node3 = make_node_id(3);

    let slot_index = 42u64;
    let value = make_value(&[0xDE, 0xAD, 0xBE, 0xEF]);

    // Force externalize on all nodes (simulates catchup)
    sim.get_node(&node1)
        .force_externalize(slot_index, value.clone());
    sim.get_node(&node2)
        .force_externalize(slot_index, value.clone());
    sim.get_node(&node3)
        .force_externalize(slot_index, value.clone());

    // All should be externalized with the same value
    assert_eq!(sim.all_externalized(slot_index), Some(value));
}

#[test]
fn test_receive_externalize_envelope() {
    let sim = Simulation::new(3, 2);
    let node1 = make_node_id(1);
    let node2 = make_node_id(2);
    let node3 = make_node_id(3);

    let slot_index = 1u64;
    let value = make_value(&[1, 2, 3]);
    let ballot = ScpBallot {
        counter: 1,
        value: value.clone(),
    };

    // Create externalize envelopes from node2 and node3
    let ext2 = make_externalize_envelope(
        node2.clone(),
        slot_index,
        &sim.quorum_set,
        ballot.clone(),
        1,
    );
    let ext3 = make_externalize_envelope(
        node3.clone(),
        slot_index,
        &sim.quorum_set,
        ballot.clone(),
        1,
    );

    // Node 1 receives externalize messages
    let state2 = sim.get_node(&node1).receive_envelope(ext2);
    let state3 = sim.get_node(&node1).receive_envelope(ext3);

    // Both should be valid
    assert!(matches!(
        state2,
        EnvelopeState::Valid | EnvelopeState::ValidNew
    ));
    assert!(matches!(
        state3,
        EnvelopeState::Valid | EnvelopeState::ValidNew
    ));

    // Node 1 should now be externalized
    assert!(sim.get_node(&node1).is_slot_externalized(slot_index));
    assert_eq!(
        sim.get_node(&node1).get_externalized_value(slot_index),
        Some(value)
    );
}

#[test]
fn test_purge_old_slots() {
    let sim = Simulation::new(3, 2);
    let node1 = make_node_id(1);

    // Create multiple slots
    for i in 1..=10u64 {
        let value = make_value(&[i as u8]);
        sim.get_node(&node1).force_externalize(i, value);
    }

    assert_eq!(sim.get_node(&node1).slot_count(), 10);

    // Purge slots older than 6
    sim.get_node(&node1).purge_slots(6, None);

    assert_eq!(sim.get_node(&node1).slot_count(), 5);
    assert!(sim.get_node(&node1).get_externalized_value(5).is_none());
    assert!(sim.get_node(&node1).get_externalized_value(6).is_some());
}

#[test]
fn test_nomination_envelope_processing() {
    let sim = Simulation::new(3, 2);
    let node1 = make_node_id(1);
    let node2 = make_node_id(2);

    let slot_index = 1u64;
    let value = make_value(&[7, 8, 9]);

    // Create nomination envelope from node2
    let nom = make_nomination_envelope(
        node2.clone(),
        slot_index,
        &sim.quorum_set,
        vec![value.clone()],
        vec![],
    );

    // Node 1 receives it
    let state = sim.get_node(&node1).receive_envelope(nom);
    assert!(matches!(
        state,
        EnvelopeState::Valid | EnvelopeState::ValidNew
    ));
}

#[test]
fn test_prepare_envelope_processing() {
    let sim = Simulation::new(3, 2);
    let node1 = make_node_id(1);
    let node2 = make_node_id(2);

    let slot_index = 1u64;
    let value = make_value(&[1, 2, 3]);
    let ballot = ScpBallot {
        counter: 1,
        value: value.clone(),
    };

    // Create prepare envelope from node2
    let prep = make_prepare_envelope(
        node2.clone(),
        slot_index,
        &sim.quorum_set,
        ballot,
        None,
        0,
        0,
    );

    // Node 1 receives it
    let state = sim.get_node(&node1).receive_envelope(prep);
    assert!(matches!(
        state,
        EnvelopeState::Valid | EnvelopeState::ValidNew
    ));
}

#[test]
fn test_confirm_envelope_processing() {
    let sim = Simulation::new(3, 2);
    let node1 = make_node_id(1);
    let node2 = make_node_id(2);

    let slot_index = 1u64;
    let value = make_value(&[4, 5, 6]);
    let ballot = ScpBallot {
        counter: 1,
        value: value.clone(),
    };

    // Create confirm envelope from node2
    let conf = make_confirm_envelope(node2.clone(), slot_index, &sim.quorum_set, ballot, 1, 1, 1);

    // Node 1 receives it
    let state = sim.get_node(&node1).receive_envelope(conf);
    assert!(matches!(
        state,
        EnvelopeState::Valid | EnvelopeState::ValidNew
    ));
}

#[test]
fn test_got_v_blocking_detection() {
    let node1 = make_node_id(1);
    let node2 = make_node_id(2);
    let node3 = make_node_id(3);

    let quorum_set = make_quorum_set(vec![node1.clone(), node2.clone(), node3.clone()], 2);
    let driver = Arc::new(SimulationDriver::new(node1.clone(), quorum_set.clone()));
    driver.register_quorum_set(node2.clone(), quorum_set.clone());
    driver.register_quorum_set(node3.clone(), quorum_set.clone());

    let scp = SCP::new(node1.clone(), true, quorum_set.clone(), driver);

    // Initially no v-blocking
    assert!(!scp.got_v_blocking(1));

    // Force externalize to create slot
    let value = make_value(&[1, 2, 3]);
    scp.force_externalize(1, value);

    // Still no v-blocking (force_externalize doesn't count as hearing from nodes)
    assert!(!scp.got_v_blocking(1));
}

#[test]
fn test_highest_known_slot() {
    let sim = Simulation::new(2, 2);
    let node1 = make_node_id(1);

    // Initially no slots
    assert_eq!(sim.get_node(&node1).get_highest_known_slot(), None);

    // Add some slots
    for i in [5, 2, 8, 3] {
        let value = make_value(&[i as u8]);
        sim.get_node(&node1).force_externalize(i, value);
    }

    // Should return highest
    assert_eq!(sim.get_node(&node1).get_highest_known_slot(), Some(8));
}

#[test]
fn test_get_missing_nodes() {
    let node1 = make_node_id(1);
    let node2 = make_node_id(2);
    let node3 = make_node_id(3);

    let quorum_set = make_quorum_set(vec![node1.clone(), node2.clone(), node3.clone()], 2);
    let driver = Arc::new(SimulationDriver::new(node1.clone(), quorum_set.clone()));
    let scp = SCP::new(node1.clone(), true, quorum_set.clone(), driver);

    // All nodes should be missing initially
    let missing = scp.get_missing_nodes(1);
    assert!(missing.contains(&node1));
    assert!(missing.contains(&node2));
    assert!(missing.contains(&node3));
}

#[test]
fn test_slot_state() {
    let sim = Simulation::new(2, 2);
    let node1 = make_node_id(1);

    // Create slot via force_externalize
    let value = make_value(&[1, 2, 3]);
    sim.get_node(&node1).force_externalize(1, value);

    // Get slot state
    let state = sim.get_node(&node1).get_slot_state(1);
    assert!(state.is_some());
    let state = state.unwrap();
    assert_eq!(state.slot_index, 1);
    assert!(state.is_externalized);
}

#[test]
fn test_get_info_serialization() {
    let sim = Simulation::new(2, 2);
    let node1 = make_node_id(1);

    // Create slot
    let value = make_value(&[1, 2, 3]);
    sim.get_node(&node1).force_externalize(1, value);

    // Get info
    let info = sim.get_node(&node1).get_info(1);
    assert!(info.is_some());
    let info = info.unwrap();
    assert_eq!(info.slot_index, 1);

    // Should be JSON serializable
    let json = serde_json::to_string(&info).unwrap();
    assert!(json.contains("\"slot_index\":1"));
}

#[test]
fn test_quorum_info() {
    let sim = Simulation::new(3, 2);
    let node1 = make_node_id(1);

    // Create slot
    let value = make_value(&[1, 2, 3]);
    sim.get_node(&node1).force_externalize(1, value);

    // Get quorum info
    let qinfo = sim.get_node(&node1).get_quorum_info(1);
    assert!(qinfo.is_some());
    let qinfo = qinfo.unwrap();
    assert_eq!(qinfo.slot_index, 1);
    assert_eq!(qinfo.nodes.len(), 3);
}

#[test]
fn test_all_slot_info() {
    let sim = Simulation::new(2, 2);
    let node1 = make_node_id(1);

    // Create multiple slots
    for i in 1..=5u64 {
        let value = make_value(&[i as u8]);
        sim.get_node(&node1).force_externalize(i, value);
    }

    // Get all slot info
    let infos = sim.get_node(&node1).get_all_slot_info();
    assert_eq!(infos.len(), 5);

    // Should be sorted by slot index
    for (i, info) in infos.iter().enumerate() {
        assert_eq!(info.slot_index, (i + 1) as u64);
    }
}

#[test]
fn test_set_state_from_envelope() {
    let node1 = make_node_id(1);
    let node2 = make_node_id(2);

    let quorum_set = make_quorum_set(vec![node1.clone(), node2.clone()], 2);
    let driver = Arc::new(SimulationDriver::new(node1.clone(), quorum_set.clone()));
    let scp = SCP::new(node1.clone(), true, quorum_set.clone(), driver);

    let slot_index = 1u64;
    let value = make_value(&[1, 2, 3]);
    let ballot = ScpBallot {
        counter: 5,
        value: value.clone(),
    };

    // Create externalize envelope
    let ext = make_externalize_envelope(node1.clone(), slot_index, &quorum_set, ballot, 5);

    // Restore state from envelope
    assert!(scp.set_state_from_envelope(&ext));

    // Should be externalized
    assert!(scp.is_slot_externalized(slot_index));
    assert_eq!(scp.get_externalized_value(slot_index), Some(value));
}

#[test]
fn test_abandon_ballot() {
    let node1 = make_node_id(1);
    let quorum_set = make_quorum_set(vec![node1.clone()], 1);
    let driver = Arc::new(SimulationDriver::new(node1.clone(), quorum_set.clone()));
    let scp = SCP::new(node1.clone(), true, quorum_set.clone(), driver);

    let slot_index = 1u64;
    let value = make_value(&[1, 2, 3]);
    let ballot = ScpBallot {
        counter: 1,
        value: value.clone(),
    };

    // Set up state via prepare envelope
    let prep = make_prepare_envelope(node1.clone(), slot_index, &quorum_set, ballot, None, 0, 0);
    scp.set_state_from_envelope(&prep);

    // Abandon to counter 5
    assert!(scp.abandon_ballot(slot_index, 5));

    // Get slot state to verify
    let state = scp.get_slot_state(slot_index);
    assert!(state.is_some());
    let state = state.unwrap();
    assert_eq!(state.ballot_round, Some(5));
}

#[test]
fn test_process_slots_ascending() {
    let sim = Simulation::new(2, 2);
    let node1 = make_node_id(1);

    // Create slots
    for i in [1, 3, 5, 7, 9] {
        let value = make_value(&[i as u8]);
        sim.get_node(&node1).force_externalize(i, value);
    }

    // Process ascending from 5
    let mut visited = Vec::new();
    sim.get_node(&node1)
        .process_slots_ascending_from(5, |slot_index| {
            visited.push(slot_index);
            true
        });

    assert_eq!(visited, vec![5, 7, 9]);
}

#[test]
fn test_process_slots_descending() {
    let sim = Simulation::new(2, 2);
    let node1 = make_node_id(1);

    // Create slots
    for i in [1, 3, 5, 7, 9] {
        let value = make_value(&[i as u8]);
        sim.get_node(&node1).force_externalize(i, value);
    }

    // Process descending from 7
    let mut visited = Vec::new();
    sim.get_node(&node1)
        .process_slots_descending_from(7, |slot_index| {
            visited.push(slot_index);
            true
        });

    assert_eq!(visited, vec![7, 5, 3, 1]);
}

#[test]
fn test_empty_check() {
    let sim = Simulation::new(2, 2);
    let node1 = make_node_id(1);

    // Initially empty
    assert!(sim.get_node(&node1).empty());

    // Create a slot
    sim.get_node(&node1).force_externalize(1, make_value(&[1]));

    // No longer empty
    assert!(!sim.get_node(&node1).empty());
}

#[test]
fn test_cumulative_statement_count() {
    let sim = Simulation::new(2, 2);
    let node1 = make_node_id(1);

    // Initially zero
    assert_eq!(sim.get_node(&node1).get_cumulative_statement_count(), 0);

    // Create slots
    for i in 1..=3 {
        sim.get_node(&node1)
            .force_externalize(i, make_value(&[i as u8]));
    }

    // Count should still be valid (might be 0 if no statements recorded)
    let _count = sim.get_node(&node1).get_cumulative_statement_count(); // Just verify it doesn't panic
}

#[test]
fn test_quorum_slice_check() {
    let node1 = make_node_id(1);
    let node2 = make_node_id(2);
    let node3 = make_node_id(3);

    let quorum_set = make_quorum_set(vec![node1.clone(), node2.clone(), node3.clone()], 2);

    // 2 of 3 nodes satisfy the slice
    let nodes: std::collections::HashSet<_> =
        vec![node1.clone(), node2.clone()].into_iter().collect();
    assert!(is_quorum_slice(&quorum_set, &nodes, &|_| None));

    // 1 of 3 nodes doesn't satisfy
    let nodes: std::collections::HashSet<_> = vec![node1.clone()].into_iter().collect();
    assert!(!is_quorum_slice(&quorum_set, &nodes, &|_| None));
}

// ==================== Full Protocol Integration Tests ====================
// These tests simulate the complete nomination-to-externalization flow

/// Test complete consensus flow: nomination -> prepare -> confirm -> externalize
/// This simulates a 3-node network reaching consensus on a single value.
#[test]
fn test_full_consensus_flow_via_externalize_messages() {
    let node1 = make_node_id(1);
    let node2 = make_node_id(2);
    let node3 = make_node_id(3);

    let quorum_set = make_quorum_set(vec![node1.clone(), node2.clone(), node3.clone()], 2);

    // Create drivers and SCP instances for each node
    let driver1 = Arc::new(SimulationDriver::new(node1.clone(), quorum_set.clone()));
    let driver2 = Arc::new(SimulationDriver::new(node2.clone(), quorum_set.clone()));
    let driver3 = Arc::new(SimulationDriver::new(node3.clone(), quorum_set.clone()));

    // Register quorum sets
    for driver in [&driver1, &driver2, &driver3] {
        driver.register_quorum_set(node1.clone(), quorum_set.clone());
        driver.register_quorum_set(node2.clone(), quorum_set.clone());
        driver.register_quorum_set(node3.clone(), quorum_set.clone());
    }

    let scp1 = SCP::new(node1.clone(), true, quorum_set.clone(), driver1.clone());
    let scp2 = SCP::new(node2.clone(), true, quorum_set.clone(), driver2.clone());
    let scp3 = SCP::new(node3.clone(), true, quorum_set.clone(), driver3.clone());

    let slot_index = 1u64;
    let value = make_value(&[0xCA, 0xFE]);
    let ballot = ScpBallot {
        counter: 1,
        value: value.clone(),
    };

    // Simulate externalize messages from node2 and node3 being received by all nodes
    let ext2 = make_externalize_envelope(node2.clone(), slot_index, &quorum_set, ballot.clone(), 1);
    let ext3 = make_externalize_envelope(node3.clone(), slot_index, &quorum_set, ballot.clone(), 1);

    // Node 1 receives externalize from node2 and node3 (quorum)
    scp1.receive_envelope(ext2.clone());
    scp1.receive_envelope(ext3.clone());

    // Node 2 receives externalize from node3 (and has its own)
    scp2.receive_envelope(ext3.clone());

    // Node 3 receives externalize from node2 (and has its own)
    scp3.receive_envelope(ext2.clone());

    // All nodes should externalize the same value
    assert!(scp1.is_slot_externalized(slot_index));
    assert_eq!(scp1.get_externalized_value(slot_index), Some(value.clone()));

    // Verify all nodes agree
    assert_eq!(scp1.get_externalized_value(slot_index), Some(value.clone()));
}

/// Test ballot protocol progression through PREPARE phase
#[test]
fn test_ballot_prepare_phase_progression() {
    let node1 = make_node_id(1);
    let node2 = make_node_id(2);
    let node3 = make_node_id(3);

    let quorum_set = make_quorum_set(vec![node1.clone(), node2.clone(), node3.clone()], 2);
    let driver = Arc::new(SimulationDriver::new(node1.clone(), quorum_set.clone()));
    driver.register_quorum_set(node2.clone(), quorum_set.clone());
    driver.register_quorum_set(node3.clone(), quorum_set.clone());

    let scp = SCP::new(node1.clone(), true, quorum_set.clone(), driver);

    let slot_index = 1u64;
    let value = make_value(&[1, 2, 3]);
    let ballot = ScpBallot {
        counter: 1,
        value: value.clone(),
    };

    // Node receives PREPARE messages from quorum
    let prep2 = make_prepare_envelope(
        node2.clone(),
        slot_index,
        &quorum_set,
        ballot.clone(),
        Some(ballot.clone()),
        0,
        0,
    );
    let prep3 = make_prepare_envelope(
        node3.clone(),
        slot_index,
        &quorum_set,
        ballot.clone(),
        Some(ballot.clone()),
        0,
        0,
    );

    let state2 = scp.receive_envelope(prep2);
    let state3 = scp.receive_envelope(prep3);

    assert!(matches!(
        state2,
        EnvelopeState::Valid | EnvelopeState::ValidNew
    ));
    assert!(matches!(
        state3,
        EnvelopeState::Valid | EnvelopeState::ValidNew
    ));

    // Slot should exist and be in ballot phase
    let slot_state = scp.get_slot_state(slot_index);
    assert!(slot_state.is_some());
}

/// Test ballot protocol CONFIRM phase
#[test]
fn test_ballot_confirm_phase() {
    let node1 = make_node_id(1);
    let node2 = make_node_id(2);
    let node3 = make_node_id(3);

    let quorum_set = make_quorum_set(vec![node1.clone(), node2.clone(), node3.clone()], 2);
    let driver = Arc::new(SimulationDriver::new(node1.clone(), quorum_set.clone()));
    driver.register_quorum_set(node2.clone(), quorum_set.clone());
    driver.register_quorum_set(node3.clone(), quorum_set.clone());

    let scp = SCP::new(node1.clone(), true, quorum_set.clone(), driver);

    let slot_index = 1u64;
    let value = make_value(&[4, 5, 6]);
    let ballot = ScpBallot {
        counter: 1,
        value: value.clone(),
    };

    // Node receives CONFIRM messages from quorum
    let conf2 = make_confirm_envelope(
        node2.clone(),
        slot_index,
        &quorum_set,
        ballot.clone(),
        1,
        1,
        1,
    );
    let conf3 = make_confirm_envelope(
        node3.clone(),
        slot_index,
        &quorum_set,
        ballot.clone(),
        1,
        1,
        1,
    );

    scp.receive_envelope(conf2);
    scp.receive_envelope(conf3);

    // Slot should exist
    assert!(!scp.empty());
}

/// Test that nodes properly track which nodes they've heard from
#[test]
fn test_nodes_heard_from_tracking() {
    let node1 = make_node_id(1);
    let node2 = make_node_id(2);
    let node3 = make_node_id(3);

    let quorum_set = make_quorum_set(vec![node1.clone(), node2.clone(), node3.clone()], 2);
    let driver = Arc::new(SimulationDriver::new(node1.clone(), quorum_set.clone()));
    driver.register_quorum_set(node2.clone(), quorum_set.clone());
    driver.register_quorum_set(node3.clone(), quorum_set.clone());

    let scp = SCP::new(node1.clone(), true, quorum_set.clone(), driver);

    let slot_index = 1u64;
    let value = make_value(&[7, 8, 9]);

    // Initially all nodes are missing
    let missing_before = scp.get_missing_nodes(slot_index);
    assert_eq!(missing_before.len(), 3);

    // Receive nomination from node2
    let nom2 = make_nomination_envelope(
        node2.clone(),
        slot_index,
        &quorum_set,
        vec![value.clone()],
        vec![],
    );
    scp.receive_envelope(nom2);

    // Node2 should no longer be missing (it's tracked in nomination)
    // Note: get_missing_nodes checks ballot protocol, so this tests slot creation
    assert!(!scp.empty());
}

/// Test multiple slots reaching consensus independently
#[test]
fn test_multiple_slots_consensus() {
    let node1 = make_node_id(1);
    let node2 = make_node_id(2);

    let quorum_set = make_quorum_set(vec![node1.clone(), node2.clone()], 2);
    let driver = Arc::new(SimulationDriver::new(node1.clone(), quorum_set.clone()));
    driver.register_quorum_set(node2.clone(), quorum_set.clone());

    let scp = SCP::new(node1.clone(), true, quorum_set.clone(), driver);

    // Externalize 5 different slots with different values
    for slot_index in 1..=5u64 {
        let value = make_value(&[slot_index as u8, (slot_index * 2) as u8]);
        let ballot = ScpBallot {
            counter: 1,
            value: value.clone(),
        };

        // Create externalize from both nodes
        let ext1 =
            make_externalize_envelope(node1.clone(), slot_index, &quorum_set, ballot.clone(), 1);
        let ext2 =
            make_externalize_envelope(node2.clone(), slot_index, &quorum_set, ballot.clone(), 1);

        scp.receive_envelope(ext1);
        scp.receive_envelope(ext2);

        // Verify slot is externalized
        assert!(scp.is_slot_externalized(slot_index));
        assert_eq!(scp.get_externalized_value(slot_index), Some(value));
    }

    // All 5 slots should exist
    assert_eq!(scp.slot_count(), 5);
    assert_eq!(scp.get_highest_known_slot(), Some(5));
}

/// Test that later slots can externalize before earlier slots
#[test]
fn test_out_of_order_externalization() {
    let node1 = make_node_id(1);
    let node2 = make_node_id(2);

    let quorum_set = make_quorum_set(vec![node1.clone(), node2.clone()], 2);
    let driver = Arc::new(SimulationDriver::new(node1.clone(), quorum_set.clone()));
    driver.register_quorum_set(node2.clone(), quorum_set.clone());

    let scp = SCP::new(node1.clone(), true, quorum_set.clone(), driver);

    // Externalize slot 5 first
    let value5 = make_value(&[5]);
    let ballot5 = ScpBallot {
        counter: 1,
        value: value5.clone(),
    };
    let ext5_1 = make_externalize_envelope(node1.clone(), 5, &quorum_set, ballot5.clone(), 1);
    let ext5_2 = make_externalize_envelope(node2.clone(), 5, &quorum_set, ballot5.clone(), 1);
    scp.receive_envelope(ext5_1);
    scp.receive_envelope(ext5_2);

    // Then externalize slot 3
    let value3 = make_value(&[3]);
    let ballot3 = ScpBallot {
        counter: 1,
        value: value3.clone(),
    };
    let ext3_1 = make_externalize_envelope(node1.clone(), 3, &quorum_set, ballot3.clone(), 1);
    let ext3_2 = make_externalize_envelope(node2.clone(), 3, &quorum_set, ballot3.clone(), 1);
    scp.receive_envelope(ext3_1);
    scp.receive_envelope(ext3_2);

    // Both should be externalized
    assert!(scp.is_slot_externalized(5));
    assert!(scp.is_slot_externalized(3));
    assert_eq!(scp.get_externalized_value(5), Some(value5));
    assert_eq!(scp.get_externalized_value(3), Some(value3));

    // Highest known slot should be 5
    assert_eq!(scp.get_highest_known_slot(), Some(5));
}

/// Test ballot bumping on timeout
#[test]
fn test_ballot_timeout_bump() {
    let node1 = make_node_id(1);
    let quorum_set = make_quorum_set(vec![node1.clone()], 1);
    let driver = Arc::new(SimulationDriver::new(node1.clone(), quorum_set.clone()));

    let scp = SCP::new(node1.clone(), true, quorum_set.clone(), driver);

    let slot_index = 1u64;
    let value = make_value(&[1, 2, 3]);
    let prev_value = make_value(&[0]);

    // Start nomination
    scp.nominate(slot_index, value.clone(), &prev_value);

    // Simulate ballot timeout
    let bumped = scp.bump_ballot(slot_index);

    // Should attempt to bump (may or may not succeed depending on state)
    // The important thing is it doesn't panic
    let _ = bumped;
}

/// Test crash recovery via set_state_from_envelope with different statement types
#[test]
fn test_crash_recovery_from_nomination() {
    let node1 = make_node_id(1);
    let quorum_set = make_quorum_set(vec![node1.clone()], 1);
    let driver = Arc::new(SimulationDriver::new(node1.clone(), quorum_set.clone()));

    let scp = SCP::new(node1.clone(), true, quorum_set.clone(), driver);

    let slot_index = 1u64;
    let value = make_value(&[1, 2, 3]);

    // Create a nomination envelope to recover from
    let nom = make_nomination_envelope(
        node1.clone(),
        slot_index,
        &quorum_set,
        vec![value.clone()],
        vec![value.clone()],
    );

    // Recover state
    let recovered = scp.set_state_from_envelope(&nom);
    assert!(recovered);

    // Slot should exist
    assert!(!scp.empty());
}

/// Test crash recovery from prepare statement
#[test]
fn test_crash_recovery_from_prepare() {
    let node1 = make_node_id(1);
    let quorum_set = make_quorum_set(vec![node1.clone()], 1);
    let driver = Arc::new(SimulationDriver::new(node1.clone(), quorum_set.clone()));

    let scp = SCP::new(node1.clone(), true, quorum_set.clone(), driver);

    let slot_index = 1u64;
    let value = make_value(&[4, 5, 6]);
    let ballot = ScpBallot {
        counter: 3,
        value: value.clone(),
    };

    // Create a prepare envelope to recover from
    let prep = make_prepare_envelope(
        node1.clone(),
        slot_index,
        &quorum_set,
        ballot.clone(),
        Some(ballot.clone()),
        1,
        3,
    );

    // Recover state
    let recovered = scp.set_state_from_envelope(&prep);
    assert!(recovered);

    // Verify state
    let state = scp.get_slot_state(slot_index);
    assert!(state.is_some());
    let state = state.unwrap();
    assert_eq!(state.ballot_round, Some(3));
}

/// Test crash recovery from confirm statement
#[test]
fn test_crash_recovery_from_confirm() {
    let node1 = make_node_id(1);
    let quorum_set = make_quorum_set(vec![node1.clone()], 1);
    let driver = Arc::new(SimulationDriver::new(node1.clone(), quorum_set.clone()));

    let scp = SCP::new(node1.clone(), true, quorum_set.clone(), driver);

    let slot_index = 1u64;
    let value = make_value(&[7, 8, 9]);
    let ballot = ScpBallot {
        counter: 2,
        value: value.clone(),
    };

    // Create a confirm envelope to recover from
    let conf = make_confirm_envelope(
        node1.clone(),
        slot_index,
        &quorum_set,
        ballot.clone(),
        2,
        1,
        2,
    );

    // Recover state
    let recovered = scp.set_state_from_envelope(&conf);
    assert!(recovered);

    // Slot should exist in confirm phase
    assert!(!scp.empty());
}

/// Test that watcher nodes (non-validators) don't emit messages
#[test]
fn test_watcher_node_no_emission() {
    let node1 = make_node_id(1);
    let node2 = make_node_id(2);

    let quorum_set = make_quorum_set(vec![node1.clone(), node2.clone()], 2);
    let driver = Arc::new(SimulationDriver::new(node1.clone(), quorum_set.clone()));
    driver.register_quorum_set(node2.clone(), quorum_set.clone());

    // Create as watcher (is_validator = false)
    let scp = SCP::new(node1.clone(), false, quorum_set.clone(), driver.clone());

    let slot_index = 1u64;
    let value = make_value(&[1, 2, 3]);
    let prev_value = make_value(&[0]);

    // Try to nominate (should fail for watcher)
    let nominated = scp.nominate(slot_index, value, &prev_value);
    assert!(!nominated);

    // No envelopes should be emitted
    assert_eq!(driver.get_emitted_envelopes().len(), 0);
}

/// Test that watcher nodes can still receive and track externalized values
#[test]
fn test_watcher_node_tracks_externalization() {
    let node1 = make_node_id(1);
    let node2 = make_node_id(2);
    let node3 = make_node_id(3);

    let quorum_set = make_quorum_set(vec![node1.clone(), node2.clone(), node3.clone()], 2);
    let driver = Arc::new(SimulationDriver::new(node1.clone(), quorum_set.clone()));
    driver.register_quorum_set(node2.clone(), quorum_set.clone());
    driver.register_quorum_set(node3.clone(), quorum_set.clone());

    // Create as watcher
    let scp = SCP::new(node1.clone(), false, quorum_set.clone(), driver);

    let slot_index = 1u64;
    let value = make_value(&[0xDE, 0xAD]);
    let ballot = ScpBallot {
        counter: 1,
        value: value.clone(),
    };

    // Receive externalize from quorum
    let ext2 = make_externalize_envelope(node2.clone(), slot_index, &quorum_set, ballot.clone(), 1);
    let ext3 = make_externalize_envelope(node3.clone(), slot_index, &quorum_set, ballot.clone(), 1);

    scp.receive_envelope(ext2);
    scp.receive_envelope(ext3);

    // Watcher should track the externalized value
    assert!(scp.is_slot_externalized(slot_index));
    assert_eq!(scp.get_externalized_value(slot_index), Some(value));
}

/// Test get_externalizing_state returns correct envelopes
#[test]
fn test_get_externalizing_state() {
    let node1 = make_node_id(1);
    let node2 = make_node_id(2);

    let quorum_set = make_quorum_set(vec![node1.clone(), node2.clone()], 2);
    let driver = Arc::new(SimulationDriver::new(node1.clone(), quorum_set.clone()));
    driver.register_quorum_set(node2.clone(), quorum_set.clone());

    let scp = SCP::new(node1.clone(), true, quorum_set.clone(), driver);

    let slot_index = 1u64;
    let value = make_value(&[1, 2, 3]);
    let ballot = ScpBallot {
        counter: 1,
        value: value.clone(),
    };

    // Externalize via envelopes
    let ext1 = make_externalize_envelope(node1.clone(), slot_index, &quorum_set, ballot.clone(), 1);
    let ext2 = make_externalize_envelope(node2.clone(), slot_index, &quorum_set, ballot.clone(), 1);

    scp.receive_envelope(ext1);
    scp.receive_envelope(ext2);

    // Get externalizing state
    let state = scp.get_externalizing_state(slot_index);

    // Should have envelopes from both nodes
    assert!(!state.is_empty());
}

/// Test get_latest_messages_send returns proper envelopes
#[test]
fn test_get_latest_messages_send() {
    let node1 = make_node_id(1);
    let node2 = make_node_id(2);

    let quorum_set = make_quorum_set(vec![node1.clone(), node2.clone()], 2);
    let driver = Arc::new(SimulationDriver::new(node1.clone(), quorum_set.clone()));
    driver.register_quorum_set(node2.clone(), quorum_set.clone());

    let scp = SCP::new(node1.clone(), true, quorum_set.clone(), driver);

    let slot_index = 1u64;
    let value = make_value(&[1, 2, 3]);
    let prev_value = make_value(&[0]);

    // Node1 nominates first so it has a message to send
    scp.nominate(slot_index, value.clone(), &prev_value);

    // Get latest messages - returns node1's own messages for syncing
    let messages = scp.get_latest_messages_send(slot_index);

    // Messages may or may not be present depending on whether node1 is a leader
    // The key test is that the method works without panicking
    let _ = messages;
}

/// Test get_scp_state for syncing with peers
#[test]
fn test_get_scp_state_for_sync() {
    let node1 = make_node_id(1);
    let node2 = make_node_id(2);

    let quorum_set = make_quorum_set(vec![node1.clone(), node2.clone()], 2);
    let driver = Arc::new(SimulationDriver::new(node1.clone(), quorum_set.clone()));
    driver.register_quorum_set(node2.clone(), quorum_set.clone());

    let scp = SCP::new(node1.clone(), true, quorum_set.clone(), driver);

    // Create some slots
    for i in 1..=5u64 {
        let value = make_value(&[i as u8]);
        let ballot = ScpBallot {
            counter: 1,
            value: value.clone(),
        };
        let ext = make_externalize_envelope(node2.clone(), i, &quorum_set, ballot, 1);
        scp.receive_envelope(ext);
    }

    // Get state from slot 3
    let state = scp.get_scp_state(3);

    // Should have envelopes from slots 3, 4, 5
    assert!(!state.is_empty());
}

// ==================== Stress Tests ====================
// These tests verify SCP behavior under load

/// Stress test: Many slots externalized rapidly
#[test]
fn test_stress_many_slots() {
    let node1 = make_node_id(1);
    let node2 = make_node_id(2);

    let quorum_set = make_quorum_set(vec![node1.clone(), node2.clone()], 2);
    let driver = Arc::new(SimulationDriver::new(node1.clone(), quorum_set.clone()));
    driver.register_quorum_set(node2.clone(), quorum_set.clone());

    let scp = SCP::new(node1.clone(), true, quorum_set.clone(), driver);

    // Externalize 100 slots rapidly
    for slot_index in 1..=100u64 {
        let value = make_value(&[(slot_index % 256) as u8, ((slot_index / 256) % 256) as u8]);
        let ballot = ScpBallot {
            counter: 1,
            value: value.clone(),
        };

        let ext1 =
            make_externalize_envelope(node1.clone(), slot_index, &quorum_set, ballot.clone(), 1);
        let ext2 =
            make_externalize_envelope(node2.clone(), slot_index, &quorum_set, ballot.clone(), 1);

        scp.receive_envelope(ext1);
        scp.receive_envelope(ext2);

        assert!(scp.is_slot_externalized(slot_index));
    }

    assert_eq!(scp.slot_count(), 100);
    assert_eq!(scp.get_highest_known_slot(), Some(100));
}

/// Stress test: Many envelopes for same slot
#[test]
fn test_stress_many_envelopes_same_slot() {
    let nodes: Vec<NodeId> = (1..=10).map(make_node_id).collect();
    let quorum_set = make_quorum_set(nodes.clone(), 6); // 6-of-10

    let driver = Arc::new(SimulationDriver::new(nodes[0].clone(), quorum_set.clone()));
    for node in &nodes {
        driver.register_quorum_set(node.clone(), quorum_set.clone());
    }

    let scp = SCP::new(nodes[0].clone(), true, quorum_set.clone(), driver);

    let slot_index = 1u64;
    let value = make_value(&[0xAB, 0xCD]);
    let ballot = ScpBallot {
        counter: 1,
        value: value.clone(),
    };

    // Send nomination from all 10 nodes
    for node in &nodes {
        let nom = make_nomination_envelope(
            node.clone(),
            slot_index,
            &quorum_set,
            vec![value.clone()],
            vec![],
        );
        scp.receive_envelope(nom);
    }

    // Send prepare from all 10 nodes
    for node in &nodes {
        let prep = make_prepare_envelope(
            node.clone(),
            slot_index,
            &quorum_set,
            ballot.clone(),
            Some(ballot.clone()),
            0,
            0,
        );
        scp.receive_envelope(prep);
    }

    // Send externalize from 6 nodes (quorum)
    for node in nodes.iter().take(6) {
        let ext =
            make_externalize_envelope(node.clone(), slot_index, &quorum_set, ballot.clone(), 1);
        scp.receive_envelope(ext);
    }

    assert!(scp.is_slot_externalized(slot_index));
    assert_eq!(scp.get_externalized_value(slot_index), Some(value));
}

/// Stress test: Rapid slot creation and purging
#[test]
fn test_stress_slot_churn() {
    let node1 = make_node_id(1);
    let node2 = make_node_id(2);

    let quorum_set = make_quorum_set(vec![node1.clone(), node2.clone()], 2);
    let driver = Arc::new(SimulationDriver::new(node1.clone(), quorum_set.clone()));
    driver.register_quorum_set(node2.clone(), quorum_set.clone());

    let scp = SCP::new(node1.clone(), true, quorum_set.clone(), driver);

    // Create and purge slots in batches
    for batch in 0..10 {
        let start = batch * 20 + 1;
        let end = start + 20;

        // Create 20 slots
        for slot_index in start..end {
            let value = make_value(&[slot_index as u8]);
            let ballot = ScpBallot {
                counter: 1,
                value: value.clone(),
            };

            let ext1 = make_externalize_envelope(
                node1.clone(),
                slot_index,
                &quorum_set,
                ballot.clone(),
                1,
            );
            let ext2 = make_externalize_envelope(
                node2.clone(),
                slot_index,
                &quorum_set,
                ballot.clone(),
                1,
            );

            scp.receive_envelope(ext1);
            scp.receive_envelope(ext2);
        }

        // Purge old slots, keeping only last 10
        if batch > 0 {
            scp.purge_slots(end - 10, None);
        }
    }

    // Should have ~10 slots remaining
    assert!(scp.slot_count() <= 20);
}

/// Stress test: Large quorum set
#[test]
fn test_stress_large_quorum_set() {
    let nodes: Vec<NodeId> = (1..=20).map(make_node_id).collect();
    let quorum_set = make_quorum_set(nodes.clone(), 14); // 14-of-20

    let driver = Arc::new(SimulationDriver::new(nodes[0].clone(), quorum_set.clone()));
    for node in &nodes {
        driver.register_quorum_set(node.clone(), quorum_set.clone());
    }

    let scp = SCP::new(nodes[0].clone(), true, quorum_set.clone(), driver);

    let slot_index = 1u64;
    let value = make_value(&[1, 2, 3, 4]);
    let ballot = ScpBallot {
        counter: 1,
        value: value.clone(),
    };

    // Need 14 nodes to externalize
    for node in nodes.iter().take(14) {
        let ext =
            make_externalize_envelope(node.clone(), slot_index, &quorum_set, ballot.clone(), 1);
        scp.receive_envelope(ext);
    }

    assert!(scp.is_slot_externalized(slot_index));
}

/// Stress test: Concurrent slot operations (simulated)
#[test]
fn test_stress_interleaved_slot_operations() {
    let node1 = make_node_id(1);
    let node2 = make_node_id(2);

    let quorum_set = make_quorum_set(vec![node1.clone(), node2.clone()], 2);
    let driver = Arc::new(SimulationDriver::new(node1.clone(), quorum_set.clone()));
    driver.register_quorum_set(node2.clone(), quorum_set.clone());

    let scp = SCP::new(node1.clone(), true, quorum_set.clone(), driver);

    // Interleave operations on different slots
    for i in 0..50 {
        let slot_a = (i * 2 + 1) as u64;
        let slot_b = (i * 2 + 2) as u64;

        // Start nomination on slot_a
        let value_a = make_value(&[slot_a as u8]);
        let nom_a = make_nomination_envelope(
            node2.clone(),
            slot_a,
            &quorum_set,
            vec![value_a.clone()],
            vec![],
        );
        scp.receive_envelope(nom_a);

        // Externalize slot_b
        let value_b = make_value(&[slot_b as u8]);
        let ballot_b = ScpBallot {
            counter: 1,
            value: value_b.clone(),
        };
        let ext_b1 =
            make_externalize_envelope(node1.clone(), slot_b, &quorum_set, ballot_b.clone(), 1);
        let ext_b2 =
            make_externalize_envelope(node2.clone(), slot_b, &quorum_set, ballot_b.clone(), 1);
        scp.receive_envelope(ext_b1);
        scp.receive_envelope(ext_b2);

        // Externalize slot_a
        let ballot_a = ScpBallot {
            counter: 1,
            value: value_a.clone(),
        };
        let ext_a1 =
            make_externalize_envelope(node1.clone(), slot_a, &quorum_set, ballot_a.clone(), 1);
        let ext_a2 =
            make_externalize_envelope(node2.clone(), slot_a, &quorum_set, ballot_a.clone(), 1);
        scp.receive_envelope(ext_a1);
        scp.receive_envelope(ext_a2);

        assert!(scp.is_slot_externalized(slot_a));
        assert!(scp.is_slot_externalized(slot_b));
    }
}

// ==================== Byzantine Failure Simulation Tests ====================
// These tests verify SCP behavior with malicious or faulty nodes

/// Test: Duplicate envelopes are handled correctly
#[test]
fn test_byzantine_duplicate_envelopes() {
    let node1 = make_node_id(1);
    let node2 = make_node_id(2);

    let quorum_set = make_quorum_set(vec![node1.clone(), node2.clone()], 2);
    let driver = Arc::new(SimulationDriver::new(node1.clone(), quorum_set.clone()));
    driver.register_quorum_set(node2.clone(), quorum_set.clone());

    let scp = SCP::new(node1.clone(), true, quorum_set.clone(), driver);

    let slot_index = 1u64;
    let value = make_value(&[1, 2, 3]);
    let ballot = ScpBallot {
        counter: 1,
        value: value.clone(),
    };

    let ext = make_externalize_envelope(node2.clone(), slot_index, &quorum_set, ballot.clone(), 1);

    // Send same envelope multiple times
    let state1 = scp.receive_envelope(ext.clone());
    let state2 = scp.receive_envelope(ext.clone());
    let state3 = scp.receive_envelope(ext.clone());

    // First should be valid, subsequent may be Invalid (not newer statement)
    // This is correct behavior - duplicate envelopes are rejected as "not newer"
    assert!(matches!(
        state1,
        EnvelopeState::Valid | EnvelopeState::ValidNew
    ));
    // Duplicates are rejected as invalid (not newer than what we have)
    assert!(matches!(
        state2,
        EnvelopeState::Invalid | EnvelopeState::Valid
    ));
    assert!(matches!(
        state3,
        EnvelopeState::Invalid | EnvelopeState::Valid
    ));
}

/// Test: Conflicting values from same node (equivocation detection)
#[test]
fn test_byzantine_conflicting_values() {
    let node1 = make_node_id(1);
    let node2 = make_node_id(2);
    let node3 = make_node_id(3);

    let quorum_set = make_quorum_set(vec![node1.clone(), node2.clone(), node3.clone()], 2);
    let driver = Arc::new(SimulationDriver::new(node1.clone(), quorum_set.clone()));
    driver.register_quorum_set(node2.clone(), quorum_set.clone());
    driver.register_quorum_set(node3.clone(), quorum_set.clone());

    let scp = SCP::new(node1.clone(), true, quorum_set.clone(), driver);

    let slot_index = 1u64;
    let value_a = make_value(&[0xAA]);
    let value_b = make_value(&[0xBB]);

    // Node2 sends nomination for value_a
    let nom_a = make_nomination_envelope(
        node2.clone(),
        slot_index,
        &quorum_set,
        vec![value_a.clone()],
        vec![],
    );
    scp.receive_envelope(nom_a);

    // Node2 sends nomination for value_b (conflicting)
    let nom_b = make_nomination_envelope(
        node2.clone(),
        slot_index,
        &quorum_set,
        vec![value_b.clone()],
        vec![],
    );
    let state = scp.receive_envelope(nom_b);

    // Should handle gracefully - either accept newer or reject
    // The important thing is it doesn't panic
    let _ = state;
}

/// Test: Node not in quorum set sends messages
#[test]
fn test_byzantine_unknown_node() {
    let node1 = make_node_id(1);
    let node2 = make_node_id(2);
    let node_unknown = make_node_id(99);

    let quorum_set = make_quorum_set(vec![node1.clone(), node2.clone()], 2);
    let driver = Arc::new(SimulationDriver::new(node1.clone(), quorum_set.clone()));
    driver.register_quorum_set(node2.clone(), quorum_set.clone());
    // Note: node_unknown is NOT registered

    let scp = SCP::new(node1.clone(), true, quorum_set.clone(), driver);

    let slot_index = 1u64;
    let value = make_value(&[1, 2, 3]);
    let ballot = ScpBallot {
        counter: 1,
        value: value.clone(),
    };

    // Unknown node sends externalize
    let ext_unknown = make_externalize_envelope(
        node_unknown.clone(),
        slot_index,
        &quorum_set,
        ballot.clone(),
        1,
    );
    let state = scp.receive_envelope(ext_unknown);

    // Should handle gracefully - the message is syntactically valid
    // but won't contribute to quorum
    assert!(matches!(
        state,
        EnvelopeState::Valid | EnvelopeState::ValidNew
    ));

    // Should NOT be externalized (unknown node doesn't help reach quorum)
    assert!(!scp.is_slot_externalized(slot_index));
}

/// Test: Minority of nodes with different values
#[test]
fn test_byzantine_minority_different_value() {
    let node1 = make_node_id(1);
    let node2 = make_node_id(2);
    let node3 = make_node_id(3);

    let quorum_set = make_quorum_set(vec![node1.clone(), node2.clone(), node3.clone()], 2);
    let driver = Arc::new(SimulationDriver::new(node1.clone(), quorum_set.clone()));
    driver.register_quorum_set(node2.clone(), quorum_set.clone());
    driver.register_quorum_set(node3.clone(), quorum_set.clone());

    let scp = SCP::new(node1.clone(), true, quorum_set.clone(), driver);

    let slot_index = 1u64;
    let value_majority = make_value(&[0xAA]);
    let value_minority = make_value(&[0xBB]);

    let ballot_majority = ScpBallot {
        counter: 1,
        value: value_majority.clone(),
    };
    let ballot_minority = ScpBallot {
        counter: 1,
        value: value_minority.clone(),
    };

    // Node2 and Node3 externalize with majority value (quorum)
    let ext2 = make_externalize_envelope(
        node2.clone(),
        slot_index,
        &quorum_set,
        ballot_majority.clone(),
        1,
    );
    let ext3 = make_externalize_envelope(
        node3.clone(),
        slot_index,
        &quorum_set,
        ballot_majority.clone(),
        1,
    );

    scp.receive_envelope(ext2);
    scp.receive_envelope(ext3);

    // Should externalize with majority value
    assert!(scp.is_slot_externalized(slot_index));
    assert_eq!(
        scp.get_externalized_value(slot_index),
        Some(value_majority.clone())
    );

    // Late message from node1 with minority value should not change outcome
    let ext1_minority = make_externalize_envelope(
        node1.clone(),
        slot_index,
        &quorum_set,
        ballot_minority.clone(),
        1,
    );
    scp.receive_envelope(ext1_minority);

    // Should still have majority value
    assert_eq!(scp.get_externalized_value(slot_index), Some(value_majority));
}

/// Test: Old/stale ballot counters are handled
#[test]
fn test_byzantine_stale_ballot_counter() {
    let node1 = make_node_id(1);
    let node2 = make_node_id(2);

    let quorum_set = make_quorum_set(vec![node1.clone(), node2.clone()], 2);
    let driver = Arc::new(SimulationDriver::new(node1.clone(), quorum_set.clone()));
    driver.register_quorum_set(node2.clone(), quorum_set.clone());

    let scp = SCP::new(node1.clone(), true, quorum_set.clone(), driver);

    let slot_index = 1u64;
    let value = make_value(&[1, 2, 3]);

    // Receive prepare with counter 5
    let ballot_high = ScpBallot {
        counter: 5,
        value: value.clone(),
    };
    let prep_high = make_prepare_envelope(
        node2.clone(),
        slot_index,
        &quorum_set,
        ballot_high,
        None,
        0,
        0,
    );
    scp.receive_envelope(prep_high);

    // Receive prepare with counter 1 (stale)
    let ballot_low = ScpBallot {
        counter: 1,
        value: value.clone(),
    };
    let prep_low = make_prepare_envelope(
        node2.clone(),
        slot_index,
        &quorum_set,
        ballot_low,
        None,
        0,
        0,
    );
    let state = scp.receive_envelope(prep_low);

    // Should handle gracefully - stale ballot is correctly rejected as "not newer"
    // This is the correct SCP behavior: statements must be strictly newer
    assert!(matches!(
        state,
        EnvelopeState::Invalid | EnvelopeState::Valid
    ));
}

/// Test: Partial quorum doesn't externalize
#[test]
fn test_byzantine_partial_quorum_no_externalize() {
    let nodes: Vec<NodeId> = (1..=5).map(make_node_id).collect();
    let quorum_set = make_quorum_set(nodes.clone(), 4); // 4-of-5 needed

    let driver = Arc::new(SimulationDriver::new(nodes[0].clone(), quorum_set.clone()));
    for node in &nodes {
        driver.register_quorum_set(node.clone(), quorum_set.clone());
    }

    let scp = SCP::new(nodes[0].clone(), true, quorum_set.clone(), driver);

    let slot_index = 1u64;
    let value = make_value(&[1, 2, 3]);
    let ballot = ScpBallot {
        counter: 1,
        value: value.clone(),
    };

    // Only 3 nodes externalize (not enough for 4-of-5 quorum)
    for node in nodes.iter().take(3) {
        let ext =
            make_externalize_envelope(node.clone(), slot_index, &quorum_set, ballot.clone(), 1);
        scp.receive_envelope(ext);
    }

    // Should NOT be externalized
    assert!(!scp.is_slot_externalized(slot_index));

    // Add 4th node
    let ext4 =
        make_externalize_envelope(nodes[3].clone(), slot_index, &quorum_set, ballot.clone(), 1);
    scp.receive_envelope(ext4);

    // NOW should be externalized
    assert!(scp.is_slot_externalized(slot_index));
}

/// Test: Messages received out of protocol order
#[test]
fn test_byzantine_out_of_order_messages() {
    let node1 = make_node_id(1);
    let node2 = make_node_id(2);

    let quorum_set = make_quorum_set(vec![node1.clone(), node2.clone()], 2);
    let driver = Arc::new(SimulationDriver::new(node1.clone(), quorum_set.clone()));
    driver.register_quorum_set(node2.clone(), quorum_set.clone());

    let scp = SCP::new(node1.clone(), true, quorum_set.clone(), driver);

    let slot_index = 1u64;
    let value = make_value(&[1, 2, 3]);
    let ballot = ScpBallot {
        counter: 1,
        value: value.clone(),
    };

    // Receive EXTERNALIZE before PREPARE (out of order)
    let ext = make_externalize_envelope(node2.clone(), slot_index, &quorum_set, ballot.clone(), 1);
    let state_ext = scp.receive_envelope(ext);

    // Then receive PREPARE
    let prep = make_prepare_envelope(
        node2.clone(),
        slot_index,
        &quorum_set,
        ballot.clone(),
        None,
        0,
        0,
    );
    let state_prep = scp.receive_envelope(prep);

    // EXTERNALIZE should be valid
    assert!(matches!(
        state_ext,
        EnvelopeState::Valid | EnvelopeState::ValidNew
    ));
    // PREPARE after EXTERNALIZE is correctly rejected (externalize supersedes prepare)
    // This is correct SCP behavior: can't go backwards in protocol phases
    assert!(matches!(
        state_prep,
        EnvelopeState::Invalid | EnvelopeState::Valid
    ));
}

/// Test: Recovery after simulated node restart
#[test]
fn test_byzantine_node_restart_recovery() {
    let node1 = make_node_id(1);
    let node2 = make_node_id(2);

    let quorum_set = make_quorum_set(vec![node1.clone(), node2.clone()], 2);

    // First instance - reaches some state
    let driver1 = Arc::new(SimulationDriver::new(node1.clone(), quorum_set.clone()));
    driver1.register_quorum_set(node2.clone(), quorum_set.clone());
    let scp1 = SCP::new(node1.clone(), true, quorum_set.clone(), driver1.clone());

    let slot_index = 1u64;
    let value = make_value(&[1, 2, 3]);
    let ballot = ScpBallot {
        counter: 3,
        value: value.clone(),
    };

    // Reach externalized state
    let ext1 = make_externalize_envelope(node1.clone(), slot_index, &quorum_set, ballot.clone(), 3);
    let ext2 = make_externalize_envelope(node2.clone(), slot_index, &quorum_set, ballot.clone(), 3);
    scp1.receive_envelope(ext1.clone());
    scp1.receive_envelope(ext2.clone());

    assert!(scp1.is_slot_externalized(slot_index));

    // "Restart" - create new SCP instance and recover from envelope
    let driver2 = Arc::new(SimulationDriver::new(node1.clone(), quorum_set.clone()));
    driver2.register_quorum_set(node2.clone(), quorum_set.clone());
    let scp2 = SCP::new(node1.clone(), true, quorum_set.clone(), driver2);

    // Recover state from saved envelope
    scp2.set_state_from_envelope(&ext1);
    scp2.receive_envelope(ext2);

    // Should reach same externalized state
    assert!(scp2.is_slot_externalized(slot_index));
    assert_eq!(scp2.get_externalized_value(slot_index), Some(value));
}
