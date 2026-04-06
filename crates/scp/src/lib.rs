//! Stellar Consensus Protocol (SCP) implementation for rs-stellar-core.
//!
//! SCP is a federated Byzantine agreement protocol that enables nodes to
//! reach consensus without requiring a closed membership or central authority.
//!
//! ## Key Concepts
//!
//! - **Quorum Slices**: Each node defines its own set of trusted nodes
//! - **Quorum**: A set of nodes sufficient for agreement (intersection of slices)
//! - **Blocking Set**: A set that can prevent agreement
//! - **V-Blocking**: A set that intersects all quorum slices of a node
//!
//! ## Protocol Phases
//!
//! 1. **Nomination**: Nodes propose and vote on candidate values
//! 2. **Ballot Protocol**: Nodes vote to prepare and commit ballots
//!    - PREPARE: Vote to prepare a ballot
//!    - CONFIRM: Confirm that a ballot is prepared
//!    - EXTERNALIZE: Commit to a value
//!
//! ## Safety Guarantees
//!
//! SCP guarantees safety (agreement) for any two nodes that share a quorum,
//! and provides liveness when the network is well-behaved.
//!
//! ## Usage for Testnet Sync
//!
//! For testnet sync, SCP is needed only after catchup to track live consensus.
//! During catchup, we skip SCP and just apply historical ledgers using
//! `force_externalize()` to mark slots as externalized without going through
//! the consensus process.
//!
//! ```ignore
//! use henyey_scp::{SCP, SCPDriver, EnvelopeState};
//!
//! // During catchup - just mark slots as externalized
//! scp.force_externalize(ledger_seq, ledger_value);
//!
//! // During live sync - participate in consensus
//! scp.nominate(slot_index, value, &prev_value);
//! let state = scp.receive_envelope(envelope);
//! ```

use std::sync::Arc;

mod ballot;
mod compare;
mod driver;
mod error;
mod format;
mod info;
mod nomination;
mod quorum;
pub mod quorum_config;
mod scp;
mod slot;
#[cfg(test)]
pub(crate) mod test_utils;

// Re-export main types
pub use ballot::{get_working_ballot, BallotPhase};
pub use compare::is_newer_nomination_or_ballot_st;
pub use driver::{SCPDriver, SCPTimerType, ValidationLevel};
pub use error::ScpError;
pub use format::{ballot_to_str, envelope_to_str, node_id_to_short_string, value_to_str};
pub use info::{
    BallotInfo, BallotValue, CommitBounds, NodeInfo, NominationInfo, QuorumInfo, SlotInfo,
};
pub use quorum::{
    find_closest_v_blocking, get_all_nodes, hash_quorum_set, is_quorum, is_quorum_set_sane,
    is_quorum_slice, is_v_blocking, is_valid_quorum_set, normalize_quorum_set,
    normalize_quorum_set_with_remove, simple_quorum_set, singleton_quorum_set,
    SingletonQuorumSetCache, MAXIMUM_QUORUM_NESTING_LEVEL, MAXIMUM_QUORUM_NODES,
};
pub use quorum_config::{
    config_to_quorum_set, node_id_to_strkey, parse_node_id, testnet_quorum_config,
    validate_quorum_config, QuorumConfigError,
};
pub use scp::{SlotState, SCP};
pub use slot::Slot;

/// Result type for SCP operations.
pub type Result<T> = std::result::Result<T, ScpError>;

/// A slot index (typically the ledger sequence number).
pub type SlotIndex = u64;

/// Shared context threaded through ballot and nomination protocol methods.
///
/// Groups the four parameters that nearly every internal SCP function needs:
/// the local node identity, its quorum set, the application driver, and
/// the slot index. Passing a single `&SlotContext<D>` instead of four
/// separate arguments reduces parameter noise across ~40 call sites.
pub(crate) struct SlotContext<'a, D: SCPDriver> {
    pub local_node_id: &'a NodeId,
    pub local_quorum_set: &'a ScpQuorumSet,
    pub driver: &'a Arc<D>,
    pub slot_index: u64,
}

/// Iterate envelopes in sorted node order, skipping self if not fully validated.
///
/// Shared implementation for `NominationProtocol::process_current_state` and
/// `BallotProtocol::process_current_state`.
pub(crate) fn process_envelopes_current_state<F>(
    envelopes: &std::collections::HashMap<NodeId, ScpEnvelope>,
    mut f: F,
    local_node_id: &NodeId,
    fully_validated: bool,
    force_self: bool,
) -> bool
where
    F: FnMut(&ScpEnvelope) -> bool,
{
    let mut nodes: Vec<_> = envelopes.keys().collect();
    nodes.sort();

    for node_id in nodes {
        if !force_self && node_id == local_node_id && !fully_validated {
            continue;
        }

        if let Some(envelope) = envelopes.get(node_id) {
            if !f(envelope) {
                return false;
            }
        }
    }

    true
}

/// The result of processing an SCP envelope.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnvelopeState {
    /// Envelope is invalid (bad signature, malformed, etc.)
    Invalid,
    /// Envelope is valid but not new (duplicate or older state)
    Valid,
    /// Envelope is valid and caused state change
    ValidNew,
}

/// Node state for quorum information reporting.
///
/// This enum represents the state of a node as seen during consensus,
/// used for debugging and monitoring quorum status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuorumInfoNodeState {
    /// Node is missing (no message received)
    Missing,
    /// Node sent a nomination message
    Nominating,
    /// Node is in ballot PREPARE phase
    Preparing,
    /// Node is in ballot CONFIRM phase
    Confirming,
    /// Node has externalized
    Externalized,
}

impl QuorumInfoNodeState {
    /// Get state from an envelope's pledges.
    pub fn from_pledges(pledges: &ScpStatementPledges) -> Self {
        match pledges {
            ScpStatementPledges::Nominate(_) => QuorumInfoNodeState::Nominating,
            ScpStatementPledges::Prepare(_) => QuorumInfoNodeState::Preparing,
            ScpStatementPledges::Confirm(_) => QuorumInfoNodeState::Confirming,
            ScpStatementPledges::Externalize(_) => QuorumInfoNodeState::Externalized,
        }
    }

    /// Check if this state represents active participation in ballot protocol.
    pub fn is_in_ballot(&self) -> bool {
        matches!(
            self,
            QuorumInfoNodeState::Preparing
                | QuorumInfoNodeState::Confirming
                | QuorumInfoNodeState::Externalized
        )
    }

    /// Check if this state represents completed consensus.
    pub fn is_externalized(&self) -> bool {
        matches!(self, QuorumInfoNodeState::Externalized)
    }
}

impl EnvelopeState {
    /// Check if the envelope was valid.
    pub fn is_valid(&self) -> bool {
        matches!(self, EnvelopeState::Valid | EnvelopeState::ValidNew)
    }

    /// Check if the envelope caused a state change.
    pub fn is_new(&self) -> bool {
        matches!(self, EnvelopeState::ValidNew)
    }
}

// Re-export XDR types commonly used with SCP
pub use stellar_xdr::curr::{
    NodeId, ScpBallot, ScpEnvelope, ScpNomination, ScpQuorumSet, ScpStatement, ScpStatementConfirm,
    ScpStatementExternalize, ScpStatementPledges, ScpStatementPrepare, Value,
};

// Re-export Hash256 for quorum set hashing
pub use henyey_common::Hash256;

#[cfg(test)]
mod tests {
    use super::*;
    use stellar_xdr::curr::{PublicKey, Uint256};

    fn make_node_id(seed: u8) -> NodeId {
        let mut bytes = [0u8; 32];
        bytes[0] = seed;
        NodeId(PublicKey::PublicKeyTypeEd25519(Uint256(bytes)))
    }

    fn make_value(data: &[u8]) -> Value {
        data.to_vec().try_into().unwrap()
    }

    fn make_quorum_set(nodes: Vec<NodeId>, threshold: u32) -> ScpQuorumSet {
        ScpQuorumSet {
            threshold,
            validators: nodes.try_into().unwrap(),
            inner_sets: vec![].try_into().unwrap(),
        }
    }

    #[test]
    fn test_envelope_state() {
        assert!(!EnvelopeState::Invalid.is_valid());
        assert!(EnvelopeState::Valid.is_valid());
        assert!(EnvelopeState::ValidNew.is_valid());

        assert!(!EnvelopeState::Invalid.is_new());
        assert!(!EnvelopeState::Valid.is_new());
        assert!(EnvelopeState::ValidNew.is_new());
    }

    #[test]
    fn test_quorum_info_node_state() {
        // Test from_pledges
        let value = make_value(&[1, 2, 3]);
        let node = make_node_id(1);
        let quorum_set = make_quorum_set(vec![node.clone()], 1);

        let nom = ScpNomination {
            quorum_set_hash: hash_quorum_set(&quorum_set).into(),
            votes: vec![value.clone()].try_into().unwrap(),
            accepted: vec![].try_into().unwrap(),
        };
        assert_eq!(
            QuorumInfoNodeState::from_pledges(&ScpStatementPledges::Nominate(nom)),
            QuorumInfoNodeState::Nominating
        );

        let ballot = ScpBallot {
            counter: 1,
            value: value.clone(),
        };
        let prep = ScpStatementPrepare {
            quorum_set_hash: hash_quorum_set(&quorum_set).into(),
            ballot: ballot.clone(),
            prepared: None,
            prepared_prime: None,
            n_c: 0,
            n_h: 0,
        };
        assert_eq!(
            QuorumInfoNodeState::from_pledges(&ScpStatementPledges::Prepare(prep)),
            QuorumInfoNodeState::Preparing
        );

        let conf = ScpStatementConfirm {
            ballot: ballot.clone(),
            n_prepared: 1,
            n_commit: 1,
            n_h: 1,
            quorum_set_hash: hash_quorum_set(&quorum_set).into(),
        };
        assert_eq!(
            QuorumInfoNodeState::from_pledges(&ScpStatementPledges::Confirm(conf)),
            QuorumInfoNodeState::Confirming
        );

        let ext = ScpStatementExternalize {
            commit: ballot.clone(),
            n_h: 1,
            commit_quorum_set_hash: hash_quorum_set(&quorum_set).into(),
        };
        assert_eq!(
            QuorumInfoNodeState::from_pledges(&ScpStatementPledges::Externalize(ext)),
            QuorumInfoNodeState::Externalized
        );

        // Test helper methods
        assert!(!QuorumInfoNodeState::Missing.is_in_ballot());
        assert!(!QuorumInfoNodeState::Nominating.is_in_ballot());
        assert!(QuorumInfoNodeState::Preparing.is_in_ballot());
        assert!(QuorumInfoNodeState::Confirming.is_in_ballot());
        assert!(QuorumInfoNodeState::Externalized.is_in_ballot());

        assert!(!QuorumInfoNodeState::Missing.is_externalized());
        assert!(!QuorumInfoNodeState::Preparing.is_externalized());
        assert!(QuorumInfoNodeState::Externalized.is_externalized());
    }
}
