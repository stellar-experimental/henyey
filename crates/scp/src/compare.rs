//! Ordering and comparison functions for SCP statements and ballots.

use std::cmp::Ordering;

use stellar_xdr::curr::{
    ScpNomination, ScpStatement, ScpStatementConfirm, ScpStatementPledges, ScpStatementPrepare,
};

use crate::ballot::{ballot_compare, cmp_opt_ballot};

/// Compare two nominations or ballot statements for ordering.
///
/// Returns true if `new_st` is newer than `old_st` for the same node.
/// This is used to determine if a statement should replace an existing one.
pub fn is_newer_nomination_or_ballot_st(old_st: &ScpStatement, new_st: &ScpStatement) -> bool {
    use ScpStatementPledges::*;

    let type_rank = |pledges: &ScpStatementPledges| -> u8 {
        match pledges {
            Nominate(_) => 0,
            Prepare(_) => 1,
            Confirm(_) => 2,
            Externalize(_) => 3,
        }
    };

    let old_rank = type_rank(&old_st.pledges);
    let new_rank = type_rank(&new_st.pledges);

    if old_rank != new_rank {
        return new_rank > old_rank;
    }

    match (&old_st.pledges, &new_st.pledges) {
        (Nominate(old), Nominate(new)) => is_newer_nominate(old, new),
        (Prepare(old), Prepare(new)) => is_newer_prepare(old, new),
        (Confirm(old), Confirm(new)) => is_newer_confirm(old, new),
        (Externalize(_), Externalize(_)) => false,
        _ => false,
    }
}

fn is_newer_nominate(old: &ScpNomination, new: &ScpNomination) -> bool {
    let old_votes: std::collections::HashSet<_> = old.votes.iter().collect();
    let old_accepted: std::collections::HashSet<_> = old.accepted.iter().collect();
    let new_votes: std::collections::HashSet<_> = new.votes.iter().collect();
    let new_accepted: std::collections::HashSet<_> = new.accepted.iter().collect();

    if !old_votes.is_subset(&new_votes) || !old_accepted.is_subset(&new_accepted) {
        return false;
    }

    new_votes.len() > old_votes.len() || new_accepted.len() > old_accepted.len()
}

fn is_newer_prepare(old: &ScpStatementPrepare, new: &ScpStatementPrepare) -> bool {
    // Parity: stellar-core BallotProtocol.cpp:104 uses compareBallots which
    // compares counter then value. Must use ballot_compare, not just counter.
    match ballot_compare(&old.ballot, &new.ballot) {
        Ordering::Less => return true,
        Ordering::Greater => return false,
        Ordering::Equal => {}
    }

    match cmp_opt_ballot(&old.prepared, &new.prepared) {
        Ordering::Less => true,
        Ordering::Greater => false,
        Ordering::Equal => match cmp_opt_ballot(&old.prepared_prime, &new.prepared_prime) {
            Ordering::Less => true,
            Ordering::Greater => false,
            Ordering::Equal => new.n_h > old.n_h,
        },
    }
}

fn is_newer_confirm(old: &ScpStatementConfirm, new: &ScpStatementConfirm) -> bool {
    // Parity: stellar-core BallotProtocol.cpp:80 uses compareBallots which
    // compares counter then value. Must use ballot_compare, not just counter.
    match ballot_compare(&old.ballot, &new.ballot) {
        Ordering::Less => return true,
        Ordering::Greater => return false,
        Ordering::Equal => {}
    }
    match new.n_prepared.cmp(&old.n_prepared) {
        Ordering::Greater => return true,
        Ordering::Less => return false,
        Ordering::Equal => {}
    }
    new.n_h > old.n_h
}

#[cfg(test)]
mod tests {
    use super::*;
    use stellar_xdr::curr::{
        NodeId, PublicKey, ScpBallot, ScpNomination, ScpQuorumSet, Uint256, Value,
    };

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
    fn test_is_newer_nomination() {
        let node = make_node_id(1);
        let quorum_set = make_quorum_set(vec![node.clone()], 1);
        let value1 = make_value(&[1]);
        let value2 = make_value(&[2]);

        let nom1 = ScpNomination {
            quorum_set_hash: crate::quorum::hash_quorum_set(&quorum_set).into(),
            votes: vec![value1.clone()].try_into().unwrap(),
            accepted: vec![].try_into().unwrap(),
        };
        let nom2 = ScpNomination {
            quorum_set_hash: crate::quorum::hash_quorum_set(&quorum_set).into(),
            votes: vec![value1.clone(), value2.clone()].try_into().unwrap(),
            accepted: vec![].try_into().unwrap(),
        };

        let st1 = ScpStatement {
            node_id: node.clone(),
            slot_index: 1,
            pledges: ScpStatementPledges::Nominate(nom1),
        };
        let st2 = ScpStatement {
            node_id: node.clone(),
            slot_index: 1,
            pledges: ScpStatementPledges::Nominate(nom2),
        };

        // st2 has more votes, so it's newer
        assert!(is_newer_nomination_or_ballot_st(&st1, &st2));
        assert!(!is_newer_nomination_or_ballot_st(&st2, &st1));
    }

    fn make_ballot(counter: u32, value: &[u8]) -> ScpBallot {
        ScpBallot {
            counter,
            value: value.to_vec().try_into().unwrap(),
        }
    }

    #[test]
    fn test_is_newer_prepare_compares_ballot_value() {
        // Regression test for AUDIT-H1: is_newer_prepare must compare ballot
        // value (not just counter) to match stellar-core's compareBallots.
        let node = make_node_id(1);
        let qs_hash = crate::quorum::hash_quorum_set(&make_quorum_set(vec![node.clone()], 1));

        let prep_a = ScpStatementPrepare {
            quorum_set_hash: qs_hash.into(),
            ballot: make_ballot(5, &[1]),
            prepared: None,
            prepared_prime: None,
            n_c: 0,
            n_h: 0,
        };
        let prep_b = ScpStatementPrepare {
            quorum_set_hash: qs_hash.into(),
            ballot: make_ballot(5, &[2]), // same counter, higher value
            prepared: None,
            prepared_prime: None,
            n_c: 0,
            n_h: 0,
        };

        // Same counter but value [2] > [1], so prep_b is newer
        assert!(is_newer_prepare(&prep_a, &prep_b));
        assert!(!is_newer_prepare(&prep_b, &prep_a));
        // Same ballot: neither is newer
        assert!(!is_newer_prepare(&prep_a, &prep_a));
    }

    #[test]
    fn test_is_newer_confirm_compares_ballot_value() {
        // Regression test for AUDIT-H1: is_newer_confirm must compare ballot
        // value (not just counter) to match stellar-core's compareBallots.
        let conf_a = ScpStatementConfirm {
            ballot: make_ballot(5, &[1]),
            n_prepared: 3,
            n_commit: 1,
            n_h: 4,
            quorum_set_hash: [0u8; 32].into(),
        };
        let conf_b = ScpStatementConfirm {
            ballot: make_ballot(5, &[2]), // same counter, higher value
            n_prepared: 3,
            n_commit: 1,
            n_h: 4,
            quorum_set_hash: [0u8; 32].into(),
        };

        // Same counter but value [2] > [1], so conf_b is newer
        assert!(is_newer_confirm(&conf_a, &conf_b));
        assert!(!is_newer_confirm(&conf_b, &conf_a));
        // Same ballot and fields: neither is newer
        assert!(!is_newer_confirm(&conf_a, &conf_a));
    }
}
