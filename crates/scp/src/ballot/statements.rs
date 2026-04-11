//! Ballot statement comparison, quorum set resolution, and federated accept/ratify logic.

use std::cmp::Ordering;

use super::*;

impl BallotProtocol {
    pub fn is_newer_statement(&self, node_id: &NodeId, statement: &ScpStatement) -> bool {
        match self.latest_envelopes.get(node_id) {
            None => true,
            Some(existing) => Self::is_newer_statement_pair(&existing.statement, statement),
        }
    }

    fn is_newer_statement_pair(old: &ScpStatement, new: &ScpStatement) -> bool {
        let old_rank = Self::pledge_rank(&old.pledges);
        let new_rank = Self::pledge_rank(&new.pledges);

        if old_rank != new_rank {
            return old_rank < new_rank;
        }

        match (&old.pledges, &new.pledges) {
            (ScpStatementPledges::Externalize(_), ScpStatementPledges::Externalize(_)) => false,
            (ScpStatementPledges::Confirm(old_c), ScpStatementPledges::Confirm(new_c)) => {
                crate::compare::is_newer_confirm(old_c, new_c)
            }
            (ScpStatementPledges::Prepare(old_p), ScpStatementPledges::Prepare(new_p)) => {
                crate::compare::is_newer_prepare(old_p, new_p)
            }
            _ => false,
        }
    }

    fn pledge_rank(pledges: &ScpStatementPledges) -> u8 {
        match pledges {
            ScpStatementPledges::Prepare(_) => 0,
            ScpStatementPledges::Confirm(_) => 1,
            ScpStatementPledges::Externalize(_) => 2,
            _ => 3,
        }
    }

    pub(crate) fn is_statement_sane<D: SCPDriver>(
        &self,
        statement: &ScpStatement,
        ctx: &SlotContext<'_, D>,
    ) -> bool {
        let quorum_set = match self.statement_quorum_set(statement, ctx) {
            Some(qset) => qset,
            None => return false,
        };

        if is_quorum_set_sane(&quorum_set, false).is_err() {
            return false;
        }

        match &statement.pledges {
            ScpStatementPledges::Prepare(prep) => {
                let is_self = statement.node_id == *ctx.local_node_id;
                if !is_self && prep.ballot.counter == 0 {
                    return false;
                }

                if let (Some(prepared_prime), Some(prepared)) =
                    (&prep.prepared_prime, &prep.prepared)
                {
                    if ballot_compare(prepared_prime, prepared) != Ordering::Less
                        || ballot_compatible(prepared_prime, prepared)
                    {
                        return false;
                    }
                }

                if prep.n_h != 0 {
                    if let Some(prepared) = &prep.prepared {
                        if prep.n_h > prepared.counter {
                            return false;
                        }
                    } else {
                        return false;
                    }
                }

                if prep.n_c != 0 {
                    if prep.n_h == 0 {
                        return false;
                    }
                    if prep.ballot.counter < prep.n_h || prep.n_h < prep.n_c {
                        return false;
                    }
                }
            }
            ScpStatementPledges::Confirm(conf) => {
                if conf.ballot.counter == 0 {
                    return false;
                }
                if conf.n_h > conf.ballot.counter {
                    return false;
                }
                if conf.n_commit > conf.n_h {
                    return false;
                }
            }
            ScpStatementPledges::Externalize(ext) => {
                if ext.commit.counter == 0 {
                    return false;
                }
                if ext.n_h < ext.commit.counter {
                    return false;
                }
            }
            _ => return false,
        }

        true
    }

    pub(crate) fn validate_statement_values<D: SCPDriver>(
        &self,
        statement: &ScpStatement,
        driver: &Arc<D>,
        slot_index: u64,
    ) -> ValidationLevel {
        let values = Self::statement_values(statement);
        if values.is_empty() {
            return ValidationLevel::Invalid;
        }

        let mut level = ValidationLevel::FullyValidated;
        for value in values {
            let next = driver.validate_value(slot_index, &value, false);
            level = min_validation_level(level, next);
            if level == ValidationLevel::Invalid {
                break;
            }
        }
        level
    }

    fn statement_quorum_set<D: SCPDriver>(
        &self,
        statement: &ScpStatement,
        ctx: &SlotContext<'_, D>,
    ) -> Option<ScpQuorumSet> {
        match &statement.pledges {
            ScpStatementPledges::Externalize(_) => {
                Some(simple_quorum_set(1, vec![statement.node_id.clone()]))
            }
            ScpStatementPledges::Prepare(prep) => {
                let provided = henyey_common::Hash256::from(prep.quorum_set_hash.clone());
                self.resolve_quorum_set(&provided, &statement.node_id, ctx)
            }
            ScpStatementPledges::Confirm(conf) => {
                let provided = henyey_common::Hash256::from(conf.quorum_set_hash.clone());
                self.resolve_quorum_set(&provided, &statement.node_id, ctx)
            }
            _ => None,
        }
    }

    /// Resolve a quorum set from its hash, checking local, hash cache, then node lookup.
    fn resolve_quorum_set<D: SCPDriver>(
        &self,
        provided: &henyey_common::Hash256,
        node_id: &NodeId,
        ctx: &SlotContext<'_, D>,
    ) -> Option<ScpQuorumSet> {
        if node_id == ctx.local_node_id {
            let expected = hash_quorum_set(ctx.local_quorum_set);
            if expected == *provided {
                return Some(ctx.local_quorum_set.clone());
            }
        }
        if let Some(qset) = ctx.driver.get_quorum_set_by_hash(provided) {
            return Some(qset);
        }
        ctx.driver.get_quorum_set(node_id).and_then(|qset| {
            let expected = hash_quorum_set(&qset);
            if expected == *provided {
                Some(qset)
            } else {
                None
            }
        })
    }

    fn statement_values(statement: &ScpStatement) -> Vec<Value> {
        crate::slot::Slot::get_statement_values(statement)
    }

    pub(super) fn get_prepare_candidates(&self, hint: &ScpStatement) -> Vec<ScpBallot> {
        let mut hint_ballots = Self::collect_hint_ballots(hint);
        let mut candidates: Vec<ScpBallot> = Vec::new();
        let mut seen = std::collections::HashSet::new();

        hint_ballots.sort_by(ballot_compare);

        for top_vote in hint_ballots.iter().rev() {
            for envelope in self.latest_envelopes.values() {
                match &envelope.statement.pledges {
                    ScpStatementPledges::Prepare(prep) => {
                        if are_ballots_less_and_compatible(&prep.ballot, top_vote) {
                            Self::push_candidate(&mut candidates, &mut seen, prep.ballot.clone());
                        }
                        if let Some(prepared) = &prep.prepared {
                            if are_ballots_less_and_compatible(prepared, top_vote) {
                                Self::push_candidate(&mut candidates, &mut seen, prepared.clone());
                            }
                        }
                        if let Some(prepared_prime) = &prep.prepared_prime {
                            if are_ballots_less_and_compatible(prepared_prime, top_vote) {
                                Self::push_candidate(
                                    &mut candidates,
                                    &mut seen,
                                    prepared_prime.clone(),
                                );
                            }
                        }
                    }
                    ScpStatementPledges::Confirm(conf) => {
                        if ballot_compatible(top_vote, &conf.ballot) {
                            Self::push_candidate(&mut candidates, &mut seen, top_vote.clone());
                            if conf.n_prepared < top_vote.counter {
                                Self::push_candidate(
                                    &mut candidates,
                                    &mut seen,
                                    ScpBallot {
                                        counter: conf.n_prepared,
                                        value: top_vote.value.clone(),
                                    },
                                );
                            }
                        }
                    }
                    ScpStatementPledges::Externalize(ext) => {
                        if ballot_compatible(top_vote, &ext.commit) {
                            Self::push_candidate(&mut candidates, &mut seen, top_vote.clone());
                        }
                    }
                    _ => {}
                }
            }
        }

        candidates.sort_by(ballot_compare);
        candidates
    }

    fn push_candidate(
        candidates: &mut Vec<ScpBallot>,
        seen: &mut std::collections::HashSet<(u32, Vec<u8>)>,
        ballot: ScpBallot,
    ) {
        let key = (
            ballot.counter,
            ballot.value.to_xdr(Limits::none()).unwrap_or_default(),
        );
        if seen.insert(key) {
            candidates.push(ballot);
        }
    }

    pub(super) fn commit_predicate(
        &self,
        ballot: &ScpBallot,
        interval: (u32, u32),
        statement: &ScpStatement,
    ) -> bool {
        match &statement.pledges {
            ScpStatementPledges::Confirm(conf) => {
                if ballot_compatible(ballot, &conf.ballot) {
                    conf.n_commit <= interval.0 && interval.1 <= conf.n_h
                } else {
                    false
                }
            }
            ScpStatementPledges::Externalize(ext) => {
                if ballot_compatible(ballot, &ext.commit) {
                    ext.commit.counter <= interval.0
                } else {
                    false
                }
            }
            _ => false,
        }
    }

    pub(super) fn statement_ballot_counter(statement: &ScpStatement) -> u32 {
        match &statement.pledges {
            ScpStatementPledges::Prepare(prep) => prep.ballot.counter,
            ScpStatementPledges::Confirm(conf) => conf.ballot.counter,
            ScpStatementPledges::Externalize(_) => u32::MAX,
            _ => 0,
        }
    }

    pub(super) fn has_vblocking_subset_strictly_ahead_of<D: SCPDriver>(
        &self,
        counter: u32,
        ctx: &SlotContext<'_, D>,
    ) -> bool {
        let mut nodes = HashSet::new();
        for (node_id, envelope) in &self.latest_envelopes {
            if Self::statement_ballot_counter(&envelope.statement) > counter {
                nodes.insert(node_id.clone());
            }
        }
        is_v_blocking(ctx.local_quorum_set, &nodes)
            && !self.statement_quorum_set_map(ctx).is_empty()
    }

    fn statement_quorum_set_map<D: SCPDriver>(
        &self,
        ctx: &SlotContext<'_, D>,
    ) -> HashMap<NodeId, ScpQuorumSet> {
        let mut map = HashMap::new();
        for (node_id, envelope) in &self.latest_envelopes {
            if let Some(qset) = self.statement_quorum_set(&envelope.statement, ctx) {
                map.insert(node_id.clone(), qset);
            }
        }
        if !map.contains_key(ctx.local_node_id) {
            map.insert(ctx.local_node_id.clone(), ctx.local_quorum_set.clone());
        }
        map
    }

    pub(super) fn federated_accept<D: SCPDriver, V, A>(
        &self,
        voted: V,
        accepted: A,
        ctx: &SlotContext<'_, D>,
    ) -> bool
    where
        V: Fn(&ScpStatement) -> bool,
        A: Fn(&ScpStatement) -> bool,
    {
        let mut accepted_nodes = HashSet::new();
        let mut supporters = HashSet::new();
        for (node_id, envelope) in &self.latest_envelopes {
            let statement = &envelope.statement;
            if accepted(statement) {
                accepted_nodes.insert(node_id.clone());
                supporters.insert(node_id.clone());
            } else if voted(statement) {
                supporters.insert(node_id.clone());
            }
        }

        if is_v_blocking(ctx.local_quorum_set, &accepted_nodes) {
            return true;
        }

        let qsets = self.statement_quorum_set_map(ctx);
        let get_qs = |node_id: &NodeId| -> Option<ScpQuorumSet> { qsets.get(node_id).cloned() };
        is_quorum(ctx.local_quorum_set, &supporters, get_qs)
    }

    pub(super) fn federated_ratify<D: SCPDriver, V>(
        &self,
        voted: V,
        ctx: &SlotContext<'_, D>,
    ) -> bool
    where
        V: Fn(&ScpStatement) -> bool,
    {
        let mut supporters = HashSet::new();
        for (node_id, envelope) in &self.latest_envelopes {
            if voted(&envelope.statement) {
                supporters.insert(node_id.clone());
            }
        }

        let qsets = self.statement_quorum_set_map(ctx);
        let get_qs = |node_id: &NodeId| -> Option<ScpQuorumSet> { qsets.get(node_id).cloned() };
        is_quorum(ctx.local_quorum_set, &supporters, get_qs)
    }

    pub(super) fn statement_votes_for_ballot(
        &self,
        ballot: &ScpBallot,
        statement: &ScpStatement,
    ) -> bool {
        match &statement.pledges {
            ScpStatementPledges::Prepare(prep) => {
                are_ballots_less_and_compatible(ballot, &prep.ballot)
            }
            ScpStatementPledges::Confirm(conf) => ballot_compatible(ballot, &conf.ballot),
            ScpStatementPledges::Externalize(ext) => ballot_compatible(ballot, &ext.commit),
            _ => false,
        }
    }

    pub(super) fn statement_votes_commit(
        &self,
        ballot: &ScpBallot,
        interval: (u32, u32),
        statement: &ScpStatement,
    ) -> bool {
        match &statement.pledges {
            ScpStatementPledges::Prepare(prep) => {
                if ballot_compatible(ballot, &prep.ballot) && prep.n_c != 0 {
                    prep.n_c <= interval.0 && interval.1 <= prep.n_h
                } else {
                    false
                }
            }
            ScpStatementPledges::Confirm(conf) => {
                ballot_compatible(ballot, &conf.ballot) && conf.n_commit <= interval.0
            }
            ScpStatementPledges::Externalize(ext) => {
                ballot_compatible(ballot, &ext.commit) && ext.commit.counter <= interval.0
            }
            _ => false,
        }
    }

    pub(super) fn has_prepared_ballot(ballot: &ScpBallot, statement: &ScpStatement) -> bool {
        match &statement.pledges {
            ScpStatementPledges::Prepare(prep) => {
                prep.prepared
                    .as_ref()
                    .map(|p| are_ballots_less_and_compatible(ballot, p))
                    .unwrap_or(false)
                    || prep
                        .prepared_prime
                        .as_ref()
                        .map(|p| are_ballots_less_and_compatible(ballot, p))
                        .unwrap_or(false)
            }
            ScpStatementPledges::Confirm(conf) => {
                let prepared = ScpBallot {
                    counter: conf.n_prepared,
                    value: conf.ballot.value.clone(),
                };
                are_ballots_less_and_compatible(ballot, &prepared)
            }
            ScpStatementPledges::Externalize(ext) => ballot_compatible(ballot, &ext.commit),
            _ => false,
        }
    }

    pub(super) fn check_heard_from_quorum<D: SCPDriver>(&mut self, ctx: &SlotContext<'_, D>) {
        let current = match self.current_ballot.as_ref() {
            Some(ballot) => ballot.clone(),
            None => return,
        };

        let mut nodes = HashSet::new();
        let mut quorum_sets = HashMap::new();

        for (node_id, envelope) in &self.latest_envelopes {
            let include = match &envelope.statement.pledges {
                ScpStatementPledges::Prepare(prep) => current.counter <= prep.ballot.counter,
                ScpStatementPledges::Confirm(_) | ScpStatementPledges::Externalize(_) => true,
                _ => false,
            };
            if !include {
                continue;
            }

            nodes.insert(node_id.clone());
            if let Some(qs) = self.statement_quorum_set(&envelope.statement, ctx) {
                quorum_sets.insert(node_id.clone(), qs);
            }
        }

        let get_qs =
            |node_id: &NodeId| -> Option<ScpQuorumSet> { quorum_sets.get(node_id).cloned() };

        if is_quorum(ctx.local_quorum_set, &nodes, get_qs) {
            let old = self.heard_from_quorum;
            self.heard_from_quorum = true;
            if !old {
                ctx.driver
                    .ballot_did_hear_from_quorum(ctx.slot_index, &current);
                // If we transition from not heard -> heard, start the ballot timer
                if self.phase != BallotPhase::Externalize {
                    let timeout = ctx.driver.compute_timeout(current.counter, false);
                    ctx.driver.setup_timer(
                        ctx.slot_index,
                        crate::driver::SCPTimerType::Ballot,
                        timeout,
                    );
                }
            }
            if self.phase == BallotPhase::Externalize {
                ctx.driver
                    .stop_timer(ctx.slot_index, crate::driver::SCPTimerType::Ballot);
            }
        } else {
            self.heard_from_quorum = false;
            ctx.driver
                .stop_timer(ctx.slot_index, crate::driver::SCPTimerType::Ballot);
        }
    }
}

/// Extract the "working ballot" from an SCP statement.
///
/// The working ballot is the ballot a node is actively working on:
/// - For PREPARE statements: returns the `ballot` field
/// - For CONFIRM statements: returns a ballot with `(n_commit, value)`
/// - For EXTERNALIZE statements: returns a ballot with `(UINT32_MAX, value)`
///
/// This is useful for comparing the progress of different nodes in the
/// ballot protocol.
///
/// # Arguments
/// * `statement` - The SCP statement to extract the working ballot from
///
/// # Returns
/// The working ballot if the statement is a ballot statement (PREPARE/CONFIRM/EXTERNALIZE),
/// or None if it's a nomination statement.
pub fn get_working_ballot(statement: &ScpStatement) -> Option<ScpBallot> {
    match &statement.pledges {
        ScpStatementPledges::Prepare(prep) => Some(prep.ballot.clone()),
        ScpStatementPledges::Confirm(conf) => Some(ScpBallot {
            counter: conf.n_commit,
            value: conf.ballot.value.clone(),
        }),
        ScpStatementPledges::Externalize(ext) => Some(ScpBallot {
            counter: u32::MAX,
            value: ext.commit.value.clone(),
        }),
        ScpStatementPledges::Nominate(_) => None,
    }
}

pub(crate) fn min_validation_level(
    left: ValidationLevel,
    right: ValidationLevel,
) -> ValidationLevel {
    match (left, right) {
        (ValidationLevel::Invalid, _) | (_, ValidationLevel::Invalid) => ValidationLevel::Invalid,
        (ValidationLevel::MaybeValid, _) | (_, ValidationLevel::MaybeValid) => {
            ValidationLevel::MaybeValid
        }
        _ => ValidationLevel::FullyValidated,
    }
}

/// Compare two ballots.
/// Returns Greater if a > b, Less if a < b, Equal if a == b.
pub(crate) fn ballot_compare(a: &ScpBallot, b: &ScpBallot) -> std::cmp::Ordering {
    match a.counter.cmp(&b.counter) {
        Ordering::Equal => a.value.cmp(&b.value),
        other => other,
    }
}

/// Compare two optional ballots (None < Some).
pub(crate) fn cmp_opt_ballot(a: &Option<ScpBallot>, b: &Option<ScpBallot>) -> std::cmp::Ordering {
    match (a, b) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) => Ordering::Less,
        (Some(_), None) => Ordering::Greater,
        (Some(a), Some(b)) => ballot_compare(a, b),
    }
}

/// Check if two ballots are compatible (same value).
pub(crate) fn ballot_compatible(a: &ScpBallot, b: &ScpBallot) -> bool {
    a.value == b.value
}

pub(super) fn are_ballots_less_and_compatible(a: &ScpBallot, b: &ScpBallot) -> bool {
    ballot_compare(a, b) != Ordering::Greater && ballot_compatible(a, b)
}

pub(super) fn are_ballots_less_and_incompatible(a: &ScpBallot, b: &ScpBallot) -> bool {
    ballot_compare(a, b) != Ordering::Greater && !ballot_compatible(a, b)
}
