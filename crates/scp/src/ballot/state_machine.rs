//! Ballot protocol state machine transitions (advance_slot, attempt_*, bump, abandon).

use std::cmp::Ordering;

use super::statements::{are_ballots_less_and_compatible, are_ballots_less_and_incompatible};
use super::*;

impl BallotProtocol {
    /// Try to advance the slot state based on received messages.
    pub(super) fn advance_slot<D: SCPDriver>(
        &mut self,
        hint: &ScpStatement,
        ctx: &SlotContext<'_, D>,
    ) -> EnvelopeState {
        self.current_message_level = self.current_message_level.saturating_add(1);
        if self.current_message_level >= MAX_PROTOCOL_TRANSITIONS {
            // stellar-core throws std::runtime_error here. We panic to match the behavior:
            // this indicates a bug in the protocol state machine, not a recoverable error.
            panic!("maximum number of transitions reached in advanceSlot");
        }
        let mut did_work = false;

        did_work = self.attempt_accept_prepared(hint, ctx) || did_work;
        did_work = self.attempt_confirm_prepared(hint, ctx) || did_work;
        did_work = self.attempt_accept_commit(hint, ctx) || did_work;
        did_work = self.attempt_confirm_commit(hint, ctx) || did_work;

        if self.current_message_level == 1 {
            loop {
                let bumped = self.attempt_bump(ctx);
                did_work = bumped || did_work;
                if !bumped {
                    break;
                }
            }
            self.check_heard_from_quorum(ctx);
        }

        self.current_message_level = self.current_message_level.saturating_sub(1);
        if did_work {
            self.send_latest_envelope(ctx.driver);
            EnvelopeState::ValidNew
        } else {
            EnvelopeState::Valid
        }
    }

    fn attempt_accept_prepared<D: SCPDriver>(
        &mut self,
        hint: &ScpStatement,
        ctx: &SlotContext<'_, D>,
    ) -> bool {
        if !matches!(self.phase, BallotPhase::Prepare | BallotPhase::Confirm) {
            return false;
        }

        let candidates = self.get_prepare_candidates(hint);

        for ballot in candidates.iter().rev() {
            if self.phase == BallotPhase::Confirm {
                if let Some(prepared) = &self.prepared {
                    if !are_ballots_less_and_compatible(prepared, ballot) {
                        continue;
                    }
                }
                if let Some(commit) = &self.commit {
                    if !ballot_compatible(commit, ballot) {
                        continue;
                    }
                }
            }

            if let Some(prepared_prime) = &self.prepared_prime {
                if ballot_compare(ballot, prepared_prime) != Ordering::Greater {
                    continue;
                }
            }

            if let Some(prepared) = &self.prepared {
                if are_ballots_less_and_compatible(ballot, prepared) {
                    continue;
                }
            }

            let accepted = self.federated_accept(
                |st| self.statement_votes_for_ballot(ballot, st),
                |st| Self::has_prepared_ballot(ballot, st),
                ctx,
            );

            if accepted && self.set_accept_prepared(ballot.clone(), ctx) {
                return true;
            }
        }

        false
    }

    fn set_accept_prepared<D: SCPDriver>(
        &mut self,
        ballot: ScpBallot,
        ctx: &SlotContext<'_, D>,
    ) -> bool {
        let mut did_work = self.set_prepared(ballot.clone(), ctx.driver, ctx.slot_index);

        if self.commit.is_some() {
            let Some(high) = self.high_ballot.as_ref() else {
                return did_work;
            };
            let incompatible = self
                .prepared
                .as_ref()
                .map(|p| are_ballots_less_and_incompatible(high, p))
                .unwrap_or(false)
                || self
                    .prepared_prime
                    .as_ref()
                    .map(|p| are_ballots_less_and_incompatible(high, p))
                    .unwrap_or(false);
            if incompatible {
                self.commit = None;
                did_work = true;
            }
        }

        if did_work {
            self.emit_current_state(ctx);
        }

        did_work
    }

    fn attempt_confirm_prepared<D: SCPDriver>(
        &mut self,
        hint: &ScpStatement,
        ctx: &SlotContext<'_, D>,
    ) -> bool {
        if self.phase != BallotPhase::Prepare {
            return false;
        }
        if self.prepared.is_none() {
            return false;
        }

        let candidates = self.get_prepare_candidates(hint);
        let (new_h_ballot, new_h_index) =
            match self.find_highest_confirmed_prepared(&candidates, ctx) {
                Some(result) => result,
                None => return false,
            };

        let new_c = self.find_lowest_commit_ballot(&candidates, &new_h_ballot, new_h_index, ctx);

        self.set_confirm_prepared(new_c, new_h_ballot, ctx)
    }

    /// Find the highest ballot that a quorum has confirmed prepared.
    fn find_highest_confirmed_prepared<D: SCPDriver>(
        &self,
        candidates: &[ScpBallot],
        ctx: &SlotContext<'_, D>,
    ) -> Option<(ScpBallot, usize)> {
        for (idx, ballot) in candidates.iter().enumerate().rev() {
            if let Some(high) = &self.high_ballot {
                if ballot_compare(high, ballot) != Ordering::Less {
                    break;
                }
            }

            if self.federated_ratify(|st| Self::has_prepared_ballot(ballot, st), ctx) {
                return Some((ballot.clone(), idx));
            }
        }
        None
    }

    /// Find the lowest commit ballot among candidates up to new_h.
    fn find_lowest_commit_ballot<D: SCPDriver>(
        &self,
        candidates: &[ScpBallot],
        new_h_ballot: &ScpBallot,
        new_h_index: usize,
        ctx: &SlotContext<'_, D>,
    ) -> ScpBallot {
        let mut new_c = ScpBallot {
            counter: 0,
            value: new_h_ballot.value.clone(),
        };

        let fallback = ScpBallot {
            counter: 0,
            value: new_h_ballot.value.clone(),
        };
        let current = self.current_ballot.as_ref().unwrap_or(&fallback);

        let can_set_commit = self.commit.is_none()
            && self
                .prepared
                .as_ref()
                .map(|p| !are_ballots_less_and_incompatible(new_h_ballot, p))
                .unwrap_or(true)
            && self
                .prepared_prime
                .as_ref()
                .map(|p| !are_ballots_less_and_incompatible(new_h_ballot, p))
                .unwrap_or(true);

        if can_set_commit {
            for ballot in candidates[..=new_h_index].iter().rev() {
                if ballot_compare(ballot, current) == Ordering::Less {
                    break;
                }
                if !are_ballots_less_and_compatible(ballot, new_h_ballot) {
                    continue;
                }
                if self.federated_ratify(|st| Self::has_prepared_ballot(ballot, st), ctx) {
                    new_c = ballot.clone();
                } else {
                    break;
                }
            }
        }

        new_c
    }

    fn set_confirm_prepared<D: SCPDriver>(
        &mut self,
        new_c: ScpBallot,
        new_h: ScpBallot,
        ctx: &SlotContext<'_, D>,
    ) -> bool {
        let mut did_work = false;
        self.value_override = Some(new_h.value.clone());

        if self
            .current_ballot
            .as_ref()
            .map(|b| ballot_compatible(b, &new_h))
            .unwrap_or(true)
        {
            if self
                .high_ballot
                .as_ref()
                .map(|b| ballot_compare(&new_h, b) == Ordering::Greater)
                .unwrap_or(true)
            {
                self.high_ballot = Some(new_h.clone());
                did_work = true;
            }

            if new_c.counter != 0 && self.commit.is_none() {
                self.commit = Some(new_c);
                did_work = true;
            }
        }

        did_work = self.update_current_if_needed(&new_h) || did_work;
        if did_work {
            self.emit_current_state(ctx);
        }

        did_work
    }

    fn attempt_accept_commit<D: SCPDriver>(
        &mut self,
        hint: &ScpStatement,
        ctx: &SlotContext<'_, D>,
    ) -> bool {
        if !matches!(self.phase, BallotPhase::Prepare | BallotPhase::Confirm) {
            return false;
        }

        let Some(ballot) = Self::hint_ballot_for_commit(hint) else {
            return false;
        };
        if self.phase == BallotPhase::Confirm {
            if let Some(high) = &self.high_ballot {
                if !ballot_compatible(&ballot, high) {
                    return false;
                }
            }
        }

        let boundaries = self.get_commit_boundaries_from_statements(&ballot);
        if boundaries.is_empty() {
            return false;
        }

        let candidate = self.find_extended_interval((0u32, 0u32), &boundaries, |interval| {
            self.federated_accept(
                |st| self.statement_votes_commit(&ballot, interval, st),
                |st| self.commit_predicate(&ballot, interval, st),
                ctx,
            )
        });

        if candidate.0 == 0 {
            return false;
        }

        if self.phase != BallotPhase::Confirm
            || candidate.1 > self.high_ballot.as_ref().map(|b| b.counter).unwrap_or(0)
        {
            let value = ballot.value;
            let c = ScpBallot {
                counter: candidate.0,
                value: value.clone(),
            };
            let h = ScpBallot {
                counter: candidate.1,
                value,
            };
            return self.set_accept_commit(c, h, ctx);
        }

        false
    }

    fn set_accept_commit<D: SCPDriver>(
        &mut self,
        c: ScpBallot,
        h: ScpBallot,
        ctx: &SlotContext<'_, D>,
    ) -> bool {
        let mut did_work = false;
        self.value_override = Some(h.value.clone());

        if self
            .high_ballot
            .as_ref()
            .map(|b| ballot_compare(b, &h) != Ordering::Equal)
            .unwrap_or(true)
            || self
                .commit
                .as_ref()
                .map(|b| ballot_compare(b, &c) != Ordering::Equal)
                .unwrap_or(true)
        {
            self.commit = Some(c);
            self.high_ballot = Some(h.clone());
            did_work = true;
        }

        if self.phase == BallotPhase::Prepare {
            self.phase = BallotPhase::Confirm;
            if let Some(current) = &self.current_ballot {
                if !are_ballots_less_and_compatible(&h, current) {
                    self.bump_to_ballot(&h, false);
                }
            }
            self.prepared_prime = None;
            did_work = true;
        }

        if did_work {
            self.update_current_if_needed(&h);
            self.emit_current_state(ctx);
        }

        did_work
    }

    fn attempt_confirm_commit<D: SCPDriver>(
        &mut self,
        hint: &ScpStatement,
        ctx: &SlotContext<'_, D>,
    ) -> bool {
        if self.phase != BallotPhase::Confirm {
            return false;
        }
        if self.high_ballot.is_none() || self.commit.is_none() {
            return false;
        }

        let Some(ballot) = Self::hint_ballot_for_commit(hint) else {
            return false;
        };
        if !ballot_compatible(&ballot, self.commit.as_ref().unwrap()) {
            return false;
        }

        let boundaries = self.get_commit_boundaries_from_statements(&ballot);
        let candidate = self.find_extended_interval((0u32, 0u32), &boundaries, |interval| {
            self.federated_ratify(|st| self.commit_predicate(&ballot, interval, st), ctx)
        });

        if candidate.0 == 0 {
            return false;
        }

        let value = ballot.value;
        let c = ScpBallot {
            counter: candidate.0,
            value: value.clone(),
        };
        let h = ScpBallot {
            counter: candidate.1,
            value,
        };
        self.set_confirm_commit(c, h, ctx)
    }

    pub(super) fn set_confirm_commit<D: SCPDriver>(
        &mut self,
        c: ScpBallot,
        h: ScpBallot,
        ctx: &SlotContext<'_, D>,
    ) -> bool {
        self.update_current_if_needed(&h);
        self.commit = Some(c);
        self.high_ballot = Some(h);
        self.phase = BallotPhase::Externalize;

        self.emit_current_state(ctx);

        // Signal that nomination should be stopped (stellar-core calls mSlot.stopNomination() here)
        self.needs_stop_nomination = true;

        // stellar-core uses mCommit->getBallot().value (c.value) for valueExternalized
        ctx.driver
            .value_externalized(ctx.slot_index, &self.commit.as_ref().unwrap().value);
        true
    }

    fn attempt_bump<D: SCPDriver>(&mut self, ctx: &SlotContext<'_, D>) -> bool {
        if !matches!(self.phase, BallotPhase::Prepare | BallotPhase::Confirm) {
            return false;
        }

        let local_counter = self.current_ballot.as_ref().map(|b| b.counter).unwrap_or(0);
        if !self.has_vblocking_subset_strictly_ahead_of(local_counter, ctx) {
            return false;
        }

        let mut counters = std::collections::BTreeSet::new();
        for envelope in self.latest_envelopes.values() {
            let counter = Self::statement_ballot_counter(&envelope.statement);
            if counter > local_counter {
                counters.insert(counter);
            }
        }

        for counter in counters {
            if !self.has_vblocking_subset_strictly_ahead_of(counter, ctx) {
                return self.abandon_ballot(counter, ctx);
            }
        }

        false
    }

    /// Abandon the current ballot.
    ///
    /// Matches stellar-core `abandonBallot(n)` which checks `mSlot.getLatestCompositeCandidate()`
    /// first, then falls back to `mCurrentBallot->value`, then calls `bumpState(value, n)`.
    /// This properly emits envelopes and checks heard-from-quorum (via `bump_state`).
    pub(crate) fn abandon_ballot<D: SCPDriver>(
        &mut self,
        counter: u32,
        ctx: &SlotContext<'_, D>,
    ) -> bool {
        // stellar-core priority: composite candidate first, then current ballot value
        let value = self
            .composite_candidate
            .as_ref()
            .filter(|v| !v.0.is_empty())
            .cloned()
            .or_else(|| self.current_ballot.as_ref().map(|b| b.value.clone()));

        if let Some(value) = value {
            if counter == 0 {
                // bumpState(value, true) which computes counter = current+1
                let n = self
                    .current_ballot
                    .as_ref()
                    .map(|b| b.counter + 1)
                    .unwrap_or(1);
                self.bump_state(ctx, value, n)
            } else {
                self.bump_state(ctx, value, counter)
            }
        } else {
            false
        }
    }

    fn update_current_if_needed(&mut self, ballot: &ScpBallot) -> bool {
        if self
            .current_ballot
            .as_ref()
            .map(|b| ballot_compare(b, ballot) == Ordering::Less)
            .unwrap_or(true)
        {
            return self.bump_to_ballot(ballot, true);
        }
        false
    }

    /// Update current value enforcing invariants (matches stellar-core updateCurrentValue).
    ///
    /// This is more thorough than `update_current_if_needed`: it checks phase
    /// and commit compatibility before bumping.
    pub(super) fn update_current_value(&mut self, ballot: &ScpBallot) -> bool {
        if self.phase != BallotPhase::Prepare && self.phase != BallotPhase::Confirm {
            return false;
        }

        if self.current_ballot.is_none() {
            self.bump_to_ballot(ballot, true);
            return true;
        }

        // If we have a commit and the new ballot is incompatible, reject
        if let Some(ref commit) = self.commit {
            if !ballot_compatible(commit, ballot) {
                return false;
            }
        }

        let comp = ballot_compare(self.current_ballot.as_ref().unwrap(), ballot);

        match comp {
            Ordering::Less => {
                self.bump_to_ballot(ballot, true);
                true
            }
            _ => false,
        }
    }

    pub(super) fn bump_to_ballot(&mut self, ballot: &ScpBallot, check: bool) -> bool {
        if check {
            if let Some(current) = &self.current_ballot {
                if ballot_compare(ballot, current) != Ordering::Greater {
                    return false;
                }
            }
        }

        let got_bumped = match &self.current_ballot {
            None => true,
            Some(current) => current.counter != ballot.counter,
        };

        self.current_ballot = Some(ballot.clone());

        // invariant: h.value = b.value
        if let Some(high) = &self.high_ballot {
            if !ballot_compatible(ballot, high) {
                self.high_ballot = None;
                // invariant: c set only when h is set
                self.commit = None;
            }
        }

        if got_bumped {
            self.heard_from_quorum = false;
        }

        true
    }

    fn get_commit_boundaries_from_statements(
        &self,
        ballot: &ScpBallot,
    ) -> std::collections::BTreeSet<u32> {
        let mut res = std::collections::BTreeSet::new();
        for envelope in self.latest_envelopes.values() {
            match &envelope.statement.pledges {
                ScpStatementPledges::Prepare(prep) => {
                    if ballot_compatible(ballot, &prep.ballot) && prep.n_c != 0 {
                        res.insert(prep.n_c);
                        res.insert(prep.n_h);
                    }
                }
                ScpStatementPledges::Confirm(conf) => {
                    if ballot_compatible(ballot, &conf.ballot) {
                        res.insert(conf.n_commit);
                        res.insert(conf.n_h);
                    }
                }
                ScpStatementPledges::Externalize(ext) => {
                    if ballot_compatible(ballot, &ext.commit) {
                        res.insert(ext.commit.counter);
                        res.insert(ext.n_h);
                        res.insert(u32::MAX);
                    }
                }
                // Nominate pledges should never reach ballot protocol
                // (stellar-core BallotProtocol.cpp:1366)
                _ => debug_assert!(false, "unexpected pledge type in ballot protocol"),
            }
        }
        res
    }

    fn find_extended_interval<F>(
        &self,
        initial: (u32, u32),
        boundaries: &std::collections::BTreeSet<u32>,
        pred: F,
    ) -> (u32, u32)
    where
        F: Fn((u32, u32)) -> bool,
    {
        let mut candidate = initial;
        for boundary in boundaries.iter().rev() {
            let current = if candidate.0 == 0 {
                (*boundary, *boundary)
            } else if *boundary > candidate.1 {
                continue;
            } else {
                (*boundary, candidate.1)
            };

            if pred(current) {
                candidate = current;
            } else if candidate.0 != 0 {
                break;
            }
        }
        candidate
    }

    fn hint_ballot_for_commit(hint: &ScpStatement) -> Option<ScpBallot> {
        match &hint.pledges {
            ScpStatementPledges::Prepare(prep) => {
                if prep.n_c != 0 {
                    Some(ScpBallot {
                        counter: prep.n_h,
                        value: prep.ballot.value.clone(),
                    })
                } else {
                    None
                }
            }
            ScpStatementPledges::Confirm(conf) => Some(ScpBallot {
                counter: conf.n_h,
                value: conf.ballot.value.clone(),
            }),
            ScpStatementPledges::Externalize(ext) => Some(ScpBallot {
                counter: ext.n_h,
                value: ext.commit.value.clone(),
            }),
            _ => None,
        }
    }

    /// Set prepared ballot.
    fn set_prepared<D: SCPDriver>(
        &mut self,
        ballot: ScpBallot,
        driver: &Arc<D>,
        slot_index: u64,
    ) -> bool {
        let mut did_work = false;
        if let Some(ref current_prepared) = self.prepared {
            match ballot_compare(current_prepared, &ballot) {
                Ordering::Less => {
                    if !ballot_compatible(current_prepared, &ballot) {
                        self.prepared_prime = Some(current_prepared.clone());
                    }
                    driver.ballot_did_prepare(slot_index, &ballot);
                    self.prepared = Some(ballot);
                    did_work = true;
                }
                Ordering::Greater => {
                    let should_update_prime = match &self.prepared_prime {
                        None => true,
                        Some(prepared_prime) => {
                            ballot_compare(prepared_prime, &ballot) == Ordering::Less
                                && !ballot_compatible(current_prepared, &ballot)
                        }
                    };
                    if should_update_prime {
                        driver.ballot_did_prepare(slot_index, &ballot);
                        self.prepared_prime = Some(ballot);
                        did_work = true;
                    }
                }
                Ordering::Equal => {}
            }
        } else {
            driver.ballot_did_prepare(slot_index, &ballot);
            self.prepared = Some(ballot);
            did_work = true;
        }

        did_work
    }
}
