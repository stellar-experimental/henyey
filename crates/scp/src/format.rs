//! Display formatting helpers for SCP types (nodes, ballots, envelopes, values).

use stellar_xdr::curr::{NodeId, ScpBallot, ScpEnvelope, ScpStatementPledges, Value};

/// Number of bytes to display for node IDs (8 hex chars).
const NODE_ID_PREFIX_BYTES: usize = 4;
/// Number of bytes to display for ballot values (8 hex chars).
const VALUE_SHORT_BYTES: usize = 4;
/// Number of bytes to display for full values (16 hex chars).
const VALUE_PREFIX_BYTES: usize = 8;

/// Format a NodeId for display as a short string.
///
/// Returns the first 8 hex characters of the public key.
pub fn node_id_to_short_string(node_id: &NodeId) -> String {
    match &node_id.0 {
        stellar_xdr::curr::PublicKey::PublicKeyTypeEd25519(stellar_xdr::curr::Uint256(bytes)) => {
            hex::encode(&bytes[..NODE_ID_PREFIX_BYTES])
        }
    }
}

/// Format a ballot for display.
pub fn ballot_to_str(ballot: &ScpBallot) -> String {
    format!(
        "({},{})",
        ballot.counter,
        hex::encode(&ballot.value.as_slice()[..VALUE_SHORT_BYTES.min(ballot.value.len())])
    )
}

/// Format a Value for display.
pub fn value_to_str(value: &Value) -> String {
    hex::encode(&value.as_slice()[..VALUE_PREFIX_BYTES.min(value.len())])
}

/// Format an envelope for display.
pub fn envelope_to_str(envelope: &ScpEnvelope) -> String {
    let node = node_id_to_short_string(&envelope.statement.node_id);
    let slot = envelope.statement.slot_index;

    match &envelope.statement.pledges {
        ScpStatementPledges::Nominate(nom) => {
            let votes: Vec<_> = nom.votes.iter().map(value_to_str).collect();
            let accepted: Vec<_> = nom.accepted.iter().map(value_to_str).collect();
            format!(
                "NOMINATE<{}, slot={}, votes={:?}, accepted={:?}>",
                node, slot, votes, accepted
            )
        }
        ScpStatementPledges::Prepare(prep) => {
            format!(
                "PREPARE<{}, slot={}, b={}, p={:?}, p'={:?}, c={}, h={}>",
                node,
                slot,
                ballot_to_str(&prep.ballot),
                prep.prepared.as_ref().map(ballot_to_str),
                prep.prepared_prime.as_ref().map(ballot_to_str),
                prep.n_c,
                prep.n_h
            )
        }
        ScpStatementPledges::Confirm(conf) => {
            format!(
                "CONFIRM<{}, slot={}, b={}, p_n={}, c={}, h={}>",
                node,
                slot,
                ballot_to_str(&conf.ballot),
                conf.n_prepared,
                conf.n_commit,
                conf.n_h
            )
        }
        ScpStatementPledges::Externalize(ext) => {
            format!(
                "EXTERNALIZE<{}, slot={}, c={}, h={}>",
                node,
                slot,
                ballot_to_str(&ext.commit),
                ext.n_h
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use stellar_xdr::curr::{PublicKey, ScpNomination, ScpQuorumSet, ScpStatement, Uint256};

    fn make_node_id(seed: u8) -> NodeId {
        let mut bytes = [0u8; 32];
        bytes[0] = seed;
        NodeId(PublicKey::PublicKeyTypeEd25519(Uint256(bytes)))
    }

    fn make_value(data: &[u8]) -> Value {
        data.to_vec().try_into().unwrap()
    }

    #[test]
    fn test_node_id_to_short_string() {
        let node = make_node_id(0xab);
        let short = node_id_to_short_string(&node);
        assert_eq!(short.len(), 8);
        assert!(short.starts_with("ab"));
    }

    #[test]
    fn test_ballot_to_str() {
        let ballot = ScpBallot {
            counter: 5,
            value: make_value(&[0xde, 0xad, 0xbe, 0xef]),
        };
        let s = ballot_to_str(&ballot);
        assert!(s.contains("5"));
        assert!(s.contains("dead"));
    }

    #[test]
    fn test_value_to_str() {
        let value = make_value(&[0x12, 0x34, 0x56, 0x78, 0x9a, 0xbc, 0xde, 0xf0]);
        let s = value_to_str(&value);
        assert_eq!(s, "123456789abcdef0");
    }

    #[test]
    fn test_envelope_to_str() {
        let node = make_node_id(1);
        let value = make_value(&[1, 2, 3, 4]);
        let quorum_set = ScpQuorumSet {
            threshold: 1,
            validators: vec![node.clone()].try_into().unwrap(),
            inner_sets: vec![].try_into().unwrap(),
        };

        let nom = ScpNomination {
            quorum_set_hash: crate::quorum::hash_quorum_set(&quorum_set).into(),
            votes: vec![value.clone()].try_into().unwrap(),
            accepted: vec![].try_into().unwrap(),
        };
        let statement = ScpStatement {
            node_id: node.clone(),
            slot_index: 42,
            pledges: ScpStatementPledges::Nominate(nom),
        };
        let envelope = ScpEnvelope {
            statement,
            signature: stellar_xdr::curr::Signature(Vec::new().try_into().unwrap_or_default()),
        };

        let s = envelope_to_str(&envelope);
        assert!(s.contains("NOMINATE"));
        assert!(s.contains("slot=42"));
    }
}
