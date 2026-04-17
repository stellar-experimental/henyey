//! Ordering and comparison functions for SCP statements and ballots.

use std::cmp::Ordering;

use stellar_xdr::curr::{
    ScpNomination, ScpStatement, ScpStatementConfirm, ScpStatementPledges, ScpStatementPrepare,
};

use crate::ballot::{ballot_compare, cmp_opt_ballot};

/// Compare two nominations or ballot statements for ordering.
///
/// Returns true if `new_st` is newer than `old_st`. Mirrors the three-level
/// check in stellar-core:
/// 1. Identity: node_id + slot_index must match (SCP.cpp:420-423)
/// 2. Phase: nomination ↔ ballot never replaces (Slot.cpp:118-121)
/// 3. Ballot ordering: delegates to [`is_newer_ballot_st`] (BallotProtocol.cpp:55-90)
///
/// Note: stellar-core's slot-existence check (`getSlot`) depends on runtime
/// state and is not replicated in this free function.
pub fn is_newer_nomination_or_ballot_st(old_st: &ScpStatement, new_st: &ScpStatement) -> bool {
    // Identity check: must be same node and slot (SCP.cpp:420-423).
    if old_st.node_id != new_st.node_id || old_st.slot_index != new_st.slot_index {
        return false;
    }

    let is_nomination = |p: &ScpStatementPledges| matches!(p, ScpStatementPledges::Nominate(_));

    // Cross-phase: nomination ↔ ballot never replaces (Slot.cpp:118-121).
    if is_nomination(&old_st.pledges) != is_nomination(&new_st.pledges) {
        return false;
    }

    match (&old_st.pledges, &new_st.pledges) {
        (ScpStatementPledges::Nominate(old), ScpStatementPledges::Nominate(new)) => {
            is_newer_nominate(old, new)
        }
        _ => is_newer_ballot_st(old_st, new_st),
    }
}

/// Compare two ballot statements for ordering (PREPARE < CONFIRM < EXTERNALIZE).
///
/// Mirrors stellar-core `BallotProtocol::isNewerStatement` (BallotProtocol.cpp:55-90).
/// Cross-type upgrades return `old_rank < new_rank`; same-type delegates to
/// per-type comparison. Must only be called with ballot (non-nomination) statements.
pub(crate) fn is_newer_ballot_st(old_st: &ScpStatement, new_st: &ScpStatement) -> bool {
    fn ballot_rank(p: &ScpStatementPledges) -> u8 {
        match p {
            ScpStatementPledges::Prepare(_) => 0,
            ScpStatementPledges::Confirm(_) => 1,
            ScpStatementPledges::Externalize(_) => 2,
            ScpStatementPledges::Nominate(_) => {
                debug_assert!(false, "is_newer_ballot_st called with nomination statement");
                0
            }
        }
    }

    let old_rank = ballot_rank(&old_st.pledges);
    let new_rank = ballot_rank(&new_st.pledges);

    if old_rank != new_rank {
        return old_rank < new_rank;
    }

    match (&old_st.pledges, &new_st.pledges) {
        (ScpStatementPledges::Prepare(old), ScpStatementPledges::Prepare(new)) => {
            is_newer_prepare(old, new)
        }
        (ScpStatementPledges::Confirm(old), ScpStatementPledges::Confirm(new)) => {
            is_newer_confirm(old, new)
        }
        (ScpStatementPledges::Externalize(_), ScpStatementPledges::Externalize(_)) => false,
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

pub(crate) fn is_newer_prepare(old: &ScpStatementPrepare, new: &ScpStatementPrepare) -> bool {
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

pub(crate) fn is_newer_confirm(old: &ScpStatementConfirm, new: &ScpStatementConfirm) -> bool {
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
    use crate::test_utils::{make_node_id, make_quorum_set, make_value};
    use stellar_xdr::curr::{ScpBallot, ScpNomination};

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

    /// Regression test for AUDIT-070: nomination ↔ ballot cross-phase statements
    /// must never replace each other. Within-ballot cross-type upgrades
    /// (PREPARE→EXTERNALIZE) are allowed per stellar-core BallotProtocol.cpp:64-66.
    #[test]
    fn test_nomination_ballot_cross_phase_never_replaces() {
        let node = make_node_id(1);
        let quorum_set = make_quorum_set(vec![node.clone()], 1);
        let value = make_value(&[1]);

        let nominate_st = ScpStatement {
            node_id: node.clone(),
            slot_index: 1,
            pledges: ScpStatementPledges::Nominate(ScpNomination {
                quorum_set_hash: crate::quorum::hash_quorum_set(&quorum_set).into(),
                votes: vec![value.clone()].try_into().unwrap(),
                accepted: vec![].try_into().unwrap(),
            }),
        };

        let prepare_st = ScpStatement {
            node_id: node.clone(),
            slot_index: 1,
            pledges: ScpStatementPledges::Prepare(ScpStatementPrepare {
                quorum_set_hash: crate::quorum::hash_quorum_set(&quorum_set).into(),
                ballot: make_ballot(1, &[1]),
                prepared: None,
                prepared_prime: None,
                n_c: 0,
                n_h: 0,
            }),
        };

        let externalize_st = ScpStatement {
            node_id: node.clone(),
            slot_index: 1,
            pledges: ScpStatementPledges::Externalize(stellar_xdr::curr::ScpStatementExternalize {
                commit: make_ballot(1, &[1]),
                n_h: 1,
                commit_quorum_set_hash: crate::quorum::hash_quorum_set(&quorum_set).into(),
            }),
        };

        // Cross-phase: ballot must NOT replace nomination
        assert!(!is_newer_nomination_or_ballot_st(&nominate_st, &prepare_st));
        assert!(!is_newer_nomination_or_ballot_st(
            &nominate_st,
            &externalize_st
        ));

        // Cross-phase: nomination must NOT replace ballot
        assert!(!is_newer_nomination_or_ballot_st(&prepare_st, &nominate_st));
        assert!(!is_newer_nomination_or_ballot_st(
            &externalize_st,
            &nominate_st
        ));

        // Within-ballot cross-type: upgrade is allowed, downgrade is not
        assert!(is_newer_nomination_or_ballot_st(
            &prepare_st,
            &externalize_st
        ));
        assert!(!is_newer_nomination_or_ballot_st(
            &externalize_st,
            &prepare_st
        ));
    }

    /// Test all 6 directed within-ballot cross-type pairs.
    /// Upgrades (PREPARE→CONFIRM, PREPARE→EXTERNALIZE, CONFIRM→EXTERNALIZE)
    /// return true; downgrades return false.
    /// Matches stellar-core BallotProtocol.cpp:64-66: old_type < new_type.
    #[test]
    fn test_cross_type_ballot_upgrades() {
        let node = make_node_id(1);
        let qs = make_quorum_set(vec![node.clone()], 1);
        let qs_hash = crate::quorum::hash_quorum_set(&qs);

        let prepare_st = ScpStatement {
            node_id: node.clone(),
            slot_index: 1,
            pledges: ScpStatementPledges::Prepare(ScpStatementPrepare {
                quorum_set_hash: qs_hash.into(),
                ballot: make_ballot(1, &[1]),
                prepared: None,
                prepared_prime: None,
                n_c: 0,
                n_h: 0,
            }),
        };

        let confirm_st = ScpStatement {
            node_id: node.clone(),
            slot_index: 1,
            pledges: ScpStatementPledges::Confirm(stellar_xdr::curr::ScpStatementConfirm {
                ballot: make_ballot(1, &[1]),
                n_prepared: 1,
                n_commit: 1,
                n_h: 1,
                quorum_set_hash: qs_hash.into(),
            }),
        };

        let externalize_st = ScpStatement {
            node_id: node.clone(),
            slot_index: 1,
            pledges: ScpStatementPledges::Externalize(stellar_xdr::curr::ScpStatementExternalize {
                commit: make_ballot(1, &[1]),
                n_h: 1,
                commit_quorum_set_hash: qs_hash.into(),
            }),
        };

        // Upgrades: true
        assert!(is_newer_nomination_or_ballot_st(&prepare_st, &confirm_st));
        assert!(is_newer_nomination_or_ballot_st(
            &prepare_st,
            &externalize_st
        ));
        assert!(is_newer_nomination_or_ballot_st(
            &confirm_st,
            &externalize_st
        ));

        // Downgrades: false
        assert!(!is_newer_nomination_or_ballot_st(&confirm_st, &prepare_st));
        assert!(!is_newer_nomination_or_ballot_st(
            &externalize_st,
            &prepare_st
        ));
        assert!(!is_newer_nomination_or_ballot_st(
            &externalize_st,
            &confirm_st
        ));

        // Same type (Externalize → Externalize): false
        assert!(!is_newer_nomination_or_ballot_st(
            &externalize_st,
            &externalize_st
        ));
    }

    /// Cross-type upgrade is independent of ballot value: type ordering wins.
    #[test]
    fn test_cross_type_ballot_upgrade_value_independent() {
        let node = make_node_id(1);
        let qs = make_quorum_set(vec![node.clone()], 1);
        let qs_hash = crate::quorum::hash_quorum_set(&qs);

        // Prepare has a "higher" ballot value than Confirm
        let prepare_st = ScpStatement {
            node_id: node.clone(),
            slot_index: 1,
            pledges: ScpStatementPledges::Prepare(ScpStatementPrepare {
                quorum_set_hash: qs_hash.into(),
                ballot: make_ballot(99, &[255]),
                prepared: None,
                prepared_prime: None,
                n_c: 0,
                n_h: 0,
            }),
        };

        let confirm_st = ScpStatement {
            node_id: node.clone(),
            slot_index: 1,
            pledges: ScpStatementPledges::Confirm(stellar_xdr::curr::ScpStatementConfirm {
                ballot: make_ballot(1, &[1]),
                n_prepared: 1,
                n_commit: 1,
                n_h: 1,
                quorum_set_hash: qs_hash.into(),
            }),
        };

        // Type ordering wins: PREPARE→CONFIRM is an upgrade regardless of ballot
        assert!(is_newer_nomination_or_ballot_st(&prepare_st, &confirm_st));
        assert!(!is_newer_nomination_or_ballot_st(&confirm_st, &prepare_st));
    }

    /// Node mismatch returns false (SCP.cpp:420).
    #[test]
    fn test_node_mismatch_returns_false() {
        let node1 = make_node_id(1);
        let node2 = make_node_id(2);
        let qs = make_quorum_set(vec![node1.clone()], 1);
        let qs_hash = crate::quorum::hash_quorum_set(&qs);

        let st1 = ScpStatement {
            node_id: node1,
            slot_index: 1,
            pledges: ScpStatementPledges::Prepare(ScpStatementPrepare {
                quorum_set_hash: qs_hash.into(),
                ballot: make_ballot(1, &[1]),
                prepared: None,
                prepared_prime: None,
                n_c: 0,
                n_h: 0,
            }),
        };

        let st2 = ScpStatement {
            node_id: node2,
            slot_index: 1,
            pledges: ScpStatementPledges::Confirm(stellar_xdr::curr::ScpStatementConfirm {
                ballot: make_ballot(1, &[1]),
                n_prepared: 1,
                n_commit: 1,
                n_h: 1,
                quorum_set_hash: qs_hash.into(),
            }),
        };

        assert!(!is_newer_nomination_or_ballot_st(&st1, &st2));
    }

    /// Slot mismatch returns false (SCP.cpp:420).
    #[test]
    fn test_slot_mismatch_returns_false() {
        let node = make_node_id(1);
        let qs = make_quorum_set(vec![node.clone()], 1);
        let qs_hash = crate::quorum::hash_quorum_set(&qs);

        let st1 = ScpStatement {
            node_id: node.clone(),
            slot_index: 1,
            pledges: ScpStatementPledges::Prepare(ScpStatementPrepare {
                quorum_set_hash: qs_hash.into(),
                ballot: make_ballot(1, &[1]),
                prepared: None,
                prepared_prime: None,
                n_c: 0,
                n_h: 0,
            }),
        };

        let st2 = ScpStatement {
            node_id: node,
            slot_index: 2,
            pledges: ScpStatementPledges::Confirm(stellar_xdr::curr::ScpStatementConfirm {
                ballot: make_ballot(1, &[1]),
                n_prepared: 1,
                n_commit: 1,
                n_h: 1,
                quorum_set_hash: qs_hash.into(),
            }),
        };

        assert!(!is_newer_nomination_or_ballot_st(&st1, &st2));
    }

    /// Regression test mirroring the overlay queue replacement pattern:
    /// same node/slot, stale PREPARE replaced by advancing CONFIRM.
    #[test]
    fn test_queue_replacement_regression() {
        let node = make_node_id(42);
        let qs = make_quorum_set(vec![node.clone()], 1);
        let qs_hash = crate::quorum::hash_quorum_set(&qs);

        let stale_prepare = ScpStatement {
            node_id: node.clone(),
            slot_index: 100,
            pledges: ScpStatementPledges::Prepare(ScpStatementPrepare {
                quorum_set_hash: qs_hash.into(),
                ballot: make_ballot(1, &[1]),
                prepared: None,
                prepared_prime: None,
                n_c: 0,
                n_h: 0,
            }),
        };

        let advancing_confirm = ScpStatement {
            node_id: node,
            slot_index: 100,
            pledges: ScpStatementPledges::Confirm(stellar_xdr::curr::ScpStatementConfirm {
                ballot: make_ballot(5, &[1]),
                n_prepared: 3,
                n_commit: 2,
                n_h: 4,
                quorum_set_hash: qs_hash.into(),
            }),
        };

        // Queue trimming should allow replacing stale PREPARE with CONFIRM
        assert!(is_newer_nomination_or_ballot_st(
            &stale_prepare,
            &advancing_confirm
        ));
        // But not the reverse
        assert!(!is_newer_nomination_or_ballot_st(
            &advancing_confirm,
            &stale_prepare
        ));
    }
}
