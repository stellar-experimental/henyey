//! Ballot protocol state machine transitions (advance_slot, attempt_*, bump, abandon).

use std::cmp::Ordering;

use super::statements::{are_ballots_less_and_compatible, are_ballots_less_and_incompatible};
use super::*;

impl BallotProtocol {
    /// Try to advance the slot state based on received messages.
    ///
    // Spec: SCP_SPEC §10 — ballot protocol progression: attempt accept-prepared,
    // confirm-prepared, accept-commit, confirm-commit in order, then bump.
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

    pub(super) fn attempt_accept_prepared<D: SCPDriver>(
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
                // stellar-core BallotProtocol.cpp:823 —
                // dbgAssert(areBallotsCompatible(mCommit, ballot)). Commit-compat
                // is an invariant guaranteed by `p ~ c` in CONFIRM, not a defensive
                // filter; a violation is a bug that must surface (panic in
                // debug/test) rather than being silently masked. `debug_assert!` is
                // the exact analogue of `dbgAssert`: compiled out under `--release`
                // (NDEBUG), so release behavior is unchanged under the invariant.
                debug_assert!(
                    self.commit
                        .as_ref()
                        .map_or(true, |c| ballot_compatible(c, ballot)),
                    "p ~ c invariant violated in attempt_accept_prepared: commit={:?} incompatible with candidate ballot={:?}",
                    self.commit,
                    ballot,
                );
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

    pub(super) fn set_accept_prepared<D: SCPDriver>(
        &mut self,
        ballot: ScpBallot,
        ctx: &SlotContext<'_, D>,
    ) -> bool {
        let mut did_work = self.set_prepared(ballot.clone(), ctx.driver, ctx.slot_index);

        // Spec: SCP_SPEC §10.4 — clear commit when high_ballot is incompatible
        // with newly accepted prepared ballot. This can only happen in PREPARE phase;
        // in CONFIRM phase, attempt_accept_prepared filters out incompatible candidates.
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
                // stellar-core: dbgAssert(mPhase == SCP_PHASE_PREPARE)
                debug_assert_eq!(
                    self.phase,
                    BallotPhase::Prepare,
                    "commit voiding can only occur in PREPARE phase"
                );
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

    pub(super) fn set_confirm_prepared<D: SCPDriver>(
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

        did_work = self.update_current_if_needed(&new_h, ctx) || did_work;
        if did_work {
            // Fire the confirm-prepared notification callback (SCP §6.5-1),
            // with `new_h` being the high ballot, mirroring stellar-core
            // BallotProtocol::setConfirmPrepared (BallotProtocol.cpp:1074-1075).
            // Ordered update_current_if_needed -> ballot_did_confirm ->
            // emit_current_state, matching the accepted_commit ordering in
            // set_accept_commit (SCP §6.5-2).
            ctx.driver.ballot_did_confirm(ctx.slot_index, &new_h);
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
                    self.bump_to_ballot(&h, false, ctx);
                }
            }
            self.prepared_prime = None;
            did_work = true;
        }

        if did_work {
            self.update_current_if_needed(&h, ctx);
            // Fire the accept-commit notification callback (SCP §6.5-2),
            // ordered update_current_if_needed -> accepted_commit ->
            // emit_current_state to match stellar-core
            // BallotProtocol::setAcceptCommit (BallotProtocol.cpp:1343-1349).
            // The argument is the high ballot `h`.
            ctx.driver.accepted_commit(ctx.slot_index, &h);
            self.emit_current_state(ctx);
        }

        did_work
    }

    pub(super) fn attempt_confirm_commit<D: SCPDriver>(
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
        // stellar-core rejects PREPARE hints in attemptConfirmCommit
        // (BallotProtocol.cpp:1460-1465). Unlike attempt_accept_commit which
        // correctly processes PREPARE hints with n_c != 0, this function only
        // acts on CONFIRM/EXTERNALIZE statements.
        if matches!(hint.pledges, ScpStatementPledges::Prepare(_)) {
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
        self.update_current_if_needed(&h, ctx);
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

    fn update_current_if_needed<D: SCPDriver>(
        &mut self,
        ballot: &ScpBallot,
        ctx: &SlotContext<'_, D>,
    ) -> bool {
        if self
            .current_ballot
            .as_ref()
            .map(|b| ballot_compare(b, ballot) == Ordering::Less)
            .unwrap_or(true)
        {
            return self.bump_to_ballot(ballot, true, ctx);
        }
        false
    }

    /// Update current value enforcing invariants (matches stellar-core updateCurrentValue).
    ///
    /// This is more thorough than `update_current_if_needed`: it checks phase
    /// and commit compatibility before bumping.
    pub(super) fn update_current_value<D: SCPDriver>(
        &mut self,
        ballot: &ScpBallot,
        ctx: &SlotContext<'_, D>,
    ) -> bool {
        if self.phase != BallotPhase::Prepare && self.phase != BallotPhase::Confirm {
            return false;
        }

        let updated = if self.current_ballot.is_none() {
            self.bump_to_ballot(ballot, true, ctx);
            true
        } else {
            // If we have a commit and the new ballot is incompatible, reject
            if let Some(ref commit) = self.commit {
                if !ballot_compatible(commit, ballot) {
                    return false;
                }
            }

            let comp = ballot_compare(self.current_ballot.as_ref().unwrap(), ballot);
            match comp {
                Ordering::Less => self.bump_to_ballot(ballot, true, ctx),
                _ => false,
            }
        };

        // Matches stellar-core BallotProtocol.cpp:442. stellar-core's
        // checkInvariants() is a sequence of dbgAssert(...) — a debug-only abort
        // that is compiled out under NDEBUG. The Rust analogue is debug_assert!:
        // abort in debug/test builds, no-op (and stripped) under --release. Keep
        // the warn! so release builds retain the observability they have today.
        if let Err(e) = self.check_invariants() {
            tracing::warn!("Invariant violation after update_current_value: {e}");
            debug_assert!(false, "Invariant violation after update_current_value: {e}");
        }

        updated
    }

    pub(super) fn bump_to_ballot<D: SCPDriver>(
        &mut self,
        ballot: &ScpBallot,
        check: bool,
        ctx: &SlotContext<'_, D>,
    ) -> bool {
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

        // Fire `started_ballot_protocol` exactly once per slot, on the
        // `current_ballot` null→set transition — matching stellar-core
        // `BallotProtocol::bumpToBallot` (BallotProtocol.cpp:469-474), guarded on
        // `!mCurrentBallot`. The argument is the new ballot (not the composite
        // nomination value). Every production path that first-sets
        // `current_ballot` flows through here (nomination, follower-receive,
        // recovery), so this is the single firing site. The catchup helper
        // `force_externalize` (no stellar-core analog) does not reach it.
        if self.current_ballot.is_none() {
            ctx.driver.started_ballot_protocol(ctx.slot_index, ballot);
        }

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
