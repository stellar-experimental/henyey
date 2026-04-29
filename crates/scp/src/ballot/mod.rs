//! Ballot protocol implementation for SCP.
//!
//! The ballot protocol is the second phase of SCP consensus, following the
//! nomination phase. After nomination produces a composite value, nodes use
//! the ballot protocol to achieve Byzantine agreement on that exact value.
//!
//! # Protocol Phases
//!
//! The ballot protocol progresses through three phases:
//!
//! 1. **PREPARE**: Nodes vote to prepare ballots. A ballot is "prepared" when
//!    nodes agree it's safe to commit (no conflicting ballot can be committed).
//!
//! 2. **CONFIRM**: Nodes confirm that a ballot is prepared. This phase ensures
//!    the network agrees that a particular ballot is prepared.
//!
//! 3. **EXTERNALIZE**: Once a ballot is confirmed prepared, nodes commit to
//!    its value. This is the final state where consensus is reached.
//!
//! # Ballot Structure
//!
//! A ballot `<n, x>` consists of:
//! - `n`: A counter (increases on timeout to try new ballots)
//! - `x`: The consensus value
//!
//! Ballots with the same value but different counters are "compatible".
//! The protocol ensures only compatible ballots can be committed.
//!
//! # Key State Variables
//!
//! Following the SCP whitepaper notation:
//! - `b`: Current ballot we're working on
//! - `p`: Highest prepared ballot
//! - `p'`: Second-highest prepared ballot (if incompatible with p)
//! - `h`: Highest ballot we can confirm prepared
//! - `c`: Commit ballot (lowest ballot we can commit)
//!
//! # Safety Guarantees
//!
//! The ballot protocol ensures that if any node externalizes a value,
//! all other nodes will externalize the same value (or not externalize at all).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

/// Maximum number of state transitions allowed in a single advance_slot call.
/// Exceeding this limit indicates a bug in the protocol state machine.
const MAX_PROTOCOL_TRANSITIONS: u32 = 50;

use stellar_xdr::curr::{
    NodeId, ScpBallot, ScpEnvelope, ScpQuorumSet, ScpStatement, ScpStatementConfirm,
    ScpStatementExternalize, ScpStatementPledges, ScpStatementPrepare, Value,
};

use crate::driver::{SCPDriver, ValidationLevel};
use crate::quorum::{
    hash_quorum_set, is_quorum, is_quorum_set_sane, is_v_blocking, simple_quorum_set,
};
use crate::EnvelopeState;
use crate::SlotContext;

/// Phase of the ballot protocol.
///
mod envelope;
mod state_machine;
mod statements;

pub use statements::get_companion_quorum_set_hash;
pub use statements::get_working_ballot;
pub(crate) use statements::{ballot_compare, ballot_compatible, cmp_opt_ballot};

/// The ballot protocol progresses through these phases in order.
/// Once in the Externalize phase, the slot has reached consensus
/// and the value is final.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BallotPhase {
    /// Preparing ballots - voting to confirm a ballot is safe to commit.
    ///
    /// In this phase, nodes exchange PREPARE messages to establish
    /// which ballots are prepared (safe to commit).
    Prepare,

    /// Confirming a prepared ballot - agreeing on which ballot to commit.
    ///
    /// In this phase, nodes exchange CONFIRM messages to agree on
    /// a commit range. Transition occurs when accept-commit is achieved.
    Confirm,

    /// Externalized - consensus has been reached.
    ///
    /// This is the terminal state. The committed value is final and
    /// will not change. Nodes broadcast EXTERNALIZE messages.
    Externalize,
}

/// State machine for the ballot protocol phase.
///
/// The `BallotProtocol` implements the second phase of SCP consensus,
/// tracking all state needed to progress from PREPARE through CONFIRM
/// to EXTERNALIZE.
///
/// # Whitepaper Correspondence
///
/// This implementation follows the SCP whitepaper. Key state variables:
/// - `current_ballot` corresponds to `b` (current ballot)
/// - `prepared` corresponds to `p` (highest prepared ballot)
/// - `prepared_prime` corresponds to `p'` (second-highest prepared, incompatible with p)
/// - `high_ballot` corresponds to `h` (highest confirmable ballot)
/// - `commit` corresponds to `c` (commit ballot)
#[derive(Debug)]
pub struct BallotProtocol {
    /// Current ballot we're working on (`b` in the whitepaper).
    ///
    /// The ballot counter increases on timeout; the value comes from nomination.
    current_ballot: Option<ScpBallot>,

    /// Highest prepared ballot (`p` in the whitepaper).
    ///
    /// A ballot is prepared when we've accepted it as prepared.
    prepared: Option<ScpBallot>,

    /// Second-highest prepared ballot (`p'` in the whitepaper).
    ///
    /// Only set if it's incompatible with `prepared`. Used to prevent
    /// committing conflicting values.
    prepared_prime: Option<ScpBallot>,

    /// Highest ballot we can confirm prepared (`h` in the whitepaper).
    ///
    /// When h is set and c <= h, we can move to confirm phase.
    high_ballot: Option<ScpBallot>,

    /// Commit ballot (`c` in the whitepaper).
    ///
    /// The lowest ballot counter at which we can commit the value.
    commit: Option<ScpBallot>,

    /// Current protocol phase.
    phase: BallotPhase,

    /// Latest ballot envelope from each node.
    latest_envelopes: HashMap<NodeId, ScpEnvelope>,

    /// Override value set when confirming prepared/commit.
    ///
    /// Used to ensure we commit the correct value when switching ballots.
    value_override: Option<Value>,

    /// Latest composite candidate value from the nomination protocol.
    ///
    /// Set by the Slot before calling into ballot protocol methods, so that
    /// `abandon_ballot` can access it (matching stellar-core `mSlot.getLatestCompositeCandidate()`).
    composite_candidate: Option<Value>,

    /// Whether we've heard from a quorum for the current ballot.
    heard_from_quorum: bool,

    /// Recursion depth counter for `advance_slot` (prevents infinite loops).
    current_message_level: u32,

    /// Last envelope we constructed locally.
    last_envelope: Option<ScpEnvelope>,

    /// Last envelope we actually emitted to the network.
    last_envelope_emit: Option<ScpEnvelope>,

    /// Whether values are fully validated (affects envelope emission).
    fully_validated: bool,

    /// Signal that nomination should be stopped (set by set_confirm_commit).
    ///
    /// In stellar-core, `setConfirmCommit` calls `mSlot.stopNomination()` directly.
    /// In Rust, we set this flag and let the Slot handle it.
    needs_stop_nomination: bool,

    /// Signal that slot-level `fully_validated` should be cleared.
    ///
    /// Set by `emit_current_state` when local self-envelope validation returns
    /// MaybeValid/MaybeValidDeferred. The ballot protocol cannot directly reach
    /// the slot's `fully_validated` flag, so this flag is consumed by the slot
    /// after ballot processing returns (same pattern as `needs_stop_nomination`).
    ///
    /// Matches stellar-core `mSlot.setFullyValidated(false)` inside
    /// `BallotProtocol::processEnvelope` for self-envelopes.
    needs_clear_slot_validation: bool,

    /// Count of ballot protocol timer expirations (used for reporting).
    ///
    /// Incremented on each `bump_timeout` call, matching
    /// `BallotProtocol::ballotProtocolTimerExpired` incrementing `mTimerExpCount`
    /// (BallotProtocol.cpp:519).
    timer_exp_count: u32,
}

impl BallotProtocol {
    /// Create a new ballot protocol state.
    pub(crate) fn new() -> Self {
        Self {
            current_ballot: None,
            prepared: None,
            prepared_prime: None,
            high_ballot: None,
            commit: None,
            phase: BallotPhase::Prepare,
            latest_envelopes: HashMap::new(),
            value_override: None,
            composite_candidate: None,
            heard_from_quorum: false,
            current_message_level: 0,
            last_envelope: None,
            last_envelope_emit: None,
            fully_validated: true,
            needs_stop_nomination: false,
            needs_clear_slot_validation: false,
            timer_exp_count: 0,
        }
    }

    /// Get the current phase.
    pub fn phase(&self) -> BallotPhase {
        self.phase
    }

    /// Get the current ballot.
    pub fn current_ballot(&self) -> Option<&ScpBallot> {
        self.current_ballot.as_ref()
    }

    /// Get the current ballot counter, if any.
    pub fn current_ballot_counter(&self) -> Option<u32> {
        self.current_ballot.as_ref().map(|ballot| ballot.counter)
    }

    /// Get the prepared ballot.
    pub fn prepared(&self) -> Option<&ScpBallot> {
        self.prepared.as_ref()
    }

    /// Get the commit ballot.
    pub fn commit(&self) -> Option<&ScpBallot> {
        self.commit.as_ref()
    }

    /// Check if we're externalized.
    pub fn is_externalized(&self) -> bool {
        self.phase == BallotPhase::Externalize
    }

    /// Set the latest composite candidate from the nomination protocol.
    ///
    /// Called by the Slot before invoking ballot protocol methods, so that
    /// `abandon_ballot` can use the composite candidate value.
    pub(crate) fn set_composite_candidate(&mut self, value: Option<Value>) {
        self.composite_candidate = value;
    }

    /// Check and clear the needs_stop_nomination flag.
    ///
    /// Returns true if nomination should be stopped (set by set_confirm_commit).
    pub(crate) fn take_needs_stop_nomination(&mut self) -> bool {
        let val = self.needs_stop_nomination;
        self.needs_stop_nomination = false;
        val
    }

    /// Check and clear the needs_clear_slot_validation flag.
    ///
    /// Returns true if the slot should clear its `fully_validated` flag
    /// (set by `emit_current_state` when local self-validation returns
    /// MaybeValid/MaybeValidDeferred).
    pub(crate) fn take_needs_clear_slot_validation(&mut self) -> bool {
        let val = self.needs_clear_slot_validation;
        self.needs_clear_slot_validation = false;
        val
    }

    /// Process the latest ballot envelopes with a callback.
    pub fn process_current_state<F>(
        &self,
        f: F,
        local_node_id: &NodeId,
        fully_validated: bool,
        force_self: bool,
    ) -> bool
    where
        F: FnMut(&ScpEnvelope) -> bool,
    {
        crate::process_envelopes_current_state(
            &self.latest_envelopes,
            f,
            local_node_id,
            fully_validated,
            force_self,
        )
    }

    /// Update fully-validated state for local emission gating.
    pub(crate) fn set_fully_validated(&mut self, fully_validated: bool) {
        self.fully_validated = fully_validated;
    }

    /// Check whether we've heard from quorum for the current ballot.
    pub fn heard_from_quorum(&self) -> bool {
        self.heard_from_quorum
    }

    /// Get the externalized value if we've reached consensus.
    pub fn get_externalized_value(&self) -> Option<&Value> {
        if self.phase == BallotPhase::Externalize {
            self.current_ballot.as_ref().map(|b| &b.value)
        } else {
            None
        }
    }

    /// Force the ballot protocol to the externalized state with the given value.
    ///
    /// This is used when fast-forwarding via EXTERNALIZE messages from the network.
    /// It ensures that subsequent envelopes for this slot are properly validated
    /// against the externalized value.
    pub(crate) fn force_externalize(&mut self, value: Value) {
        let ballot = ScpBallot {
            counter: u32::MAX, // Infinite ballot for externalize
            value: value.clone(),
        };
        self.commit = Some(ballot.clone());
        self.high_ballot = Some(ballot.clone());
        self.current_ballot = Some(ballot);
        self.phase = BallotPhase::Externalize;
    }

    /// Get the last envelope constructed by this node.
    pub fn get_last_envelope(&self) -> Option<&ScpEnvelope> {
        self.last_envelope.as_ref()
    }

    /// Get envelopes that contributed to externalization.
    ///
    /// Matches stellar-core `BallotProtocol::getExternalizingState()`:
    /// - Only returns envelopes when in EXTERNALIZE phase
    /// - For other nodes: only includes envelopes with ballots compatible with commit
    /// - For self: only includes if `fully_validated` is true
    pub fn get_externalizing_state(
        &self,
        local_node_id: &NodeId,
        fully_validated: bool,
    ) -> Vec<ScpEnvelope> {
        let mut res = Vec::new();
        if self.phase != BallotPhase::Externalize {
            return res;
        }

        let commit = match &self.commit {
            Some(c) => c,
            None => return res,
        };

        for (node_id, envelope) in &self.latest_envelopes {
            if node_id != local_node_id {
                // For other nodes: check ballot compatibility with commit
                if let Some(working) = get_working_ballot(&envelope.statement) {
                    if ballot_compatible(&working, commit) {
                        res.push(envelope.clone());
                    }
                }
            } else if fully_validated {
                // Only return self messages if fully validated
                res.push(envelope.clone());
            }
        }

        res
    }

    /// Get the high ballot (highest confirmable).
    pub fn high_ballot(&self) -> Option<&ScpBallot> {
        self.high_ballot.as_ref()
    }

    /// Get the current consensus value.
    pub fn value(&self) -> Option<&Value> {
        self.current_ballot.as_ref().map(|b| &b.value)
    }

    /// Check protocol invariants for debugging.
    ///
    /// Returns Ok(()) if invariants hold, Err with description otherwise.
    pub fn check_invariants(&self) -> Result<(), String> {
        // In PREPARE phase, c == 0 (no commit yet)
        if self.phase == BallotPhase::Prepare && self.commit.is_some() {
            // Actually in prepare phase, c is only set when we have h set
            // The invariant is: c != 0 => h != 0
            if self.high_ballot.is_none() {
                return Err("commit set but high_ballot not set in Prepare phase".to_string());
            }
        }

        // If we have prepared' it must be < prepared and incompatible
        if let (Some(prepared), Some(prepared_prime)) = (&self.prepared, &self.prepared_prime) {
            if ballot_compare(prepared_prime, prepared) != std::cmp::Ordering::Less {
                return Err("prepared_prime must be less than prepared".to_string());
            }
            if ballot_compatible(prepared_prime, prepared) {
                return Err("prepared_prime must be incompatible with prepared".to_string());
            }
        }

        // c <= h (commit counter <= high counter)
        if let (Some(commit), Some(high)) = (&self.commit, &self.high_ballot) {
            if commit.counter > high.counter {
                return Err("commit counter exceeds high counter".to_string());
            }
            // c and h must have same value
            if commit.value != high.value {
                return Err("commit and high have different values".to_string());
            }
        }

        // In EXTERNALIZE, we must have commit and high
        if self.phase == BallotPhase::Externalize
            && (self.commit.is_none() || self.high_ballot.is_none())
        {
            return Err("externalize phase requires commit and high".to_string());
        }

        Ok(())
    }

    /// Get a string representation of the local state for debugging.
    pub fn get_local_state(&self) -> String {
        fn fmt_ballot(state: &mut String, label: &str, ballot: &Option<ScpBallot>) {
            if let Some(b) = ballot {
                state.push_str(&format!(
                    " {}=({},{})",
                    label,
                    b.counter,
                    hex::encode(&b.value.as_slice()[..4.min(b.value.len())])
                ));
            }
        }

        let mut state = format!("phase={:?}", self.phase);

        fmt_ballot(&mut state, "b", &self.current_ballot);
        fmt_ballot(&mut state, "p", &self.prepared);
        fmt_ballot(&mut state, "p'", &self.prepared_prime);
        fmt_ballot(&mut state, "h", &self.high_ballot);
        fmt_ballot(&mut state, "c", &self.commit);

        state.push_str(&format!(" heard_from_quorum={}", self.heard_from_quorum));

        state
    }

    /// Get the latest envelopes received from each node.
    ///
    /// Returns a map from node ID to the most recent envelope from that node.
    pub(crate) fn latest_envelopes(&self) -> &HashMap<NodeId, ScpEnvelope> {
        &self.latest_envelopes
    }

    /// `(phase, counter)` of the latest stored ballot envelope from
    /// `node_id`, if any.
    ///
    /// Returns `None` when either no envelope is stored for that peer
    /// or the stored pledge is a `Nominate` (the latter should not
    /// happen on `BallotProtocol::latest_envelopes` in practice, but
    /// `ballot_summary_of` is defensive). Used by the stale-ballot
    /// reject log attribution in `slot::process_ballot_envelope` so a
    /// single artifact line shows both sides of the comparison that
    /// caused the reject.
    pub(crate) fn stored_ballot_summary(&self, node_id: &NodeId) -> Option<(BallotPhase, u32)> {
        let env = self.latest_envelopes.get(node_id)?;
        crate::compare::ballot_summary_of(&env.statement.pledges)
    }

    /// Get the state of a node in the ballot protocol.
    ///
    /// Returns the QuorumInfoNodeState for a given node based on their
    /// latest ballot envelope, or Missing if we haven't heard from them.
    pub fn get_node_state(&self, node_id: &NodeId) -> crate::QuorumInfoNodeState {
        if let Some(envelope) = self.latest_envelopes.get(node_id) {
            crate::QuorumInfoNodeState::from_pledges(&envelope.statement.pledges)
        } else {
            crate::QuorumInfoNodeState::Missing
        }
    }

    /// Get the reporting-specific state of a node in the ballot protocol.
    ///
    /// Matches `BallotProtocol::getState` (BallotProtocol.cpp:2020-2078).
    /// Uses timeout thresholds, local-node self-classification, and ballot
    /// comparison to determine Agree/Delayed/Disagree/Missing/NoInfo.
    pub(crate) fn get_reporting_state(
        &self,
        node_id: &NodeId,
        local_node_id: &NodeId,
        self_already_moved_on: bool,
    ) -> crate::ReportingNodeState {
        use crate::ReportingNodeState;

        // Always mark self as Agree (BallotProtocol.cpp:2023-2027).
        if node_id == local_node_id {
            return ReportingNodeState::Agree;
        }

        let it = self.latest_envelopes.get(node_id);
        if it.is_none() {
            // No envelope from this node.
            if self.timer_exp_count >= crate::NUM_TIMEOUTS_THRESHOLD_FOR_REPORTING
                || self_already_moved_on
            {
                return ReportingNodeState::Missing;
            } else {
                return ReportingNodeState::NoInfo;
            }
        }

        let env = it.unwrap();
        let mut state = ReportingNodeState::Agree;

        if let Some(ref last_emit) = self.last_envelope_emit {
            let pledges = &env.statement.pledges;
            let externalized = matches!(pledges, ScpStatementPledges::Externalize(_));
            let confirmed_commit = matches!(
                pledges,
                ScpStatementPledges::Confirm(c) if c.ballot.counter == u32::MAX
            );

            // Delayed: we externalized + moved on, peer hasn't (BallotProtocol.cpp:2052-2058).
            if self.phase == BallotPhase::Externalize
                && (!externalized || !confirmed_commit)
                && self_already_moved_on
            {
                state = ReportingNodeState::Delayed;
            }

            // Disagree: both in confirm/externalize but incompatible ballots
            // (BallotProtocol.cpp:2060-2075). Overrides Delayed.
            let self_accepted_confirm =
                self.phase == BallotPhase::Confirm || self.phase == BallotPhase::Externalize;
            let other_accepted_confirm = matches!(
                pledges,
                ScpStatementPledges::Confirm(_) | ScpStatementPledges::Externalize(_)
            );

            if self_accepted_confirm && other_accepted_confirm {
                let other_working = get_working_ballot(&env.statement);
                let self_working = get_working_ballot(&last_emit.statement);
                if let (Some(other_b), Some(self_b)) = (other_working, self_working) {
                    if !ballot_compatible(&other_b, &self_b) {
                        state = ReportingNodeState::Disagree;
                    }
                }
            }
        }

        state
    }

    /// Get JSON-serializable ballot information.
    ///
    /// Returns a BallotInfo struct that can be serialized to JSON
    /// for debugging and monitoring purposes.
    pub fn get_info(&self) -> crate::BallotInfo {
        let ballot_to_info = |b: &ScpBallot| crate::BallotValue {
            counter: b.counter,
            value: crate::value_to_str(&b.value),
        };

        crate::BallotInfo {
            phase: format!("{:?}", self.phase),
            ballot_counter: self.current_ballot.as_ref().map(|b| b.counter).unwrap_or(0),
            ballot_value: self
                .current_ballot
                .as_ref()
                .map(|b| crate::value_to_str(&b.value)),
            prepared: self.prepared.as_ref().map(ballot_to_info),
            prepared_prime: self.prepared_prime.as_ref().map(ballot_to_info),
            commit: self.commit.as_ref().map(|c| crate::CommitBounds {
                low: c.counter,
                high: self
                    .high_ballot
                    .as_ref()
                    .map(|h| h.counter)
                    .unwrap_or(c.counter),
            }),
            high: self.high_ballot.as_ref().map(|h| h.counter).unwrap_or(0),
            node_count: self.latest_envelopes.len(),
            heard_from_quorum: self.heard_from_quorum,
        }
    }

    /// Start the ballot protocol with a value from nomination.
    pub(crate) fn bump<D: SCPDriver>(
        &mut self,
        ctx: &SlotContext<'_, D>,
        value: Value,
        force: bool,
    ) -> bool {
        if !force && self.current_ballot.is_some() {
            return false;
        }

        // Calculate new ballot counter
        let counter = self
            .current_ballot
            .as_ref()
            .map(|current| current.counter + 1)
            .unwrap_or(1);

        self.bump_state(ctx, value, counter)
    }

    /// Bump ballot counter on timeout.
    ///
    /// This matches the stellar-core `ballotProtocolTimerExpired` → `abandonBallot(0)` flow.
    pub(crate) fn bump_timeout<D: SCPDriver>(
        &mut self,
        ctx: &SlotContext<'_, D>,
        composite_candidate: Option<&Value>,
    ) -> bool {
        self.timer_exp_count = self.timer_exp_count.saturating_add(1);
        // Sync composite candidate from caller before abandoning
        self.composite_candidate = composite_candidate.cloned();
        self.abandon_ballot(0, ctx)
    }

    /// Check whether a ballot statement is stale (not newer than what we already have).
    /// Used to skip validation for stale envelopes — matches stellar-core which
    /// rejects stale envelopes before validateValues.
    pub(crate) fn is_stale_ballot_statement(&self, statement: &ScpStatement) -> bool {
        match &statement.pledges {
            ScpStatementPledges::Prepare(_)
            | ScpStatementPledges::Confirm(_)
            | ScpStatementPledges::Externalize(_) => {}
            _ => return true,
        }
        !self.is_newer_statement(&statement.node_id, statement)
    }

    /// Process a ballot protocol envelope.
    ///
    /// # Caller contract
    ///
    /// - **Input**: must be a ballot pledge (Prepare/Confirm/Externalize).
    ///   Nominate pledges are rejected with `unexpected_pledge`.
    /// - **Sanity**: caller should have verified `is_statement_sane` before
    ///   invoking this method.
    /// - **Freshness**: this method performs its own `is_newer_statement`
    ///   check as a defensive guard. On the `Slot::process_ballot_envelope`
    ///   production path, the upstream `is_stale_ballot_statement` pre-check
    ///   already rejects stale envelopes before this method is called, so
    ///   the inner freshness gate is unreachable there. Direct callers
    ///   (e.g. unit tests) still rely on it.
    /// - **Validation**: `validation` must reflect the result of validating
    ///   the statement's values (via `validate_statement_values` or
    ///   equivalent) and be consistent with the statement content.
    pub(crate) fn process_envelope<D: SCPDriver>(
        &mut self,
        envelope: &ScpEnvelope,
        ctx: &SlotContext<'_, D>,
        validation: crate::ValidationLevel,
    ) -> EnvelopeState {
        let node_id = &envelope.statement.node_id;

        match &envelope.statement.pledges {
            ScpStatementPledges::Prepare(_)
            | ScpStatementPledges::Confirm(_)
            | ScpStatementPledges::Externalize(_) => {}
            _ => {
                tracing::info!(
                    target: "henyey::envelope_path",
                    slot = envelope.statement.slot_index,
                    node_id = ?node_id,
                    scp_gate = "unexpected_pledge",
                    "scp receive rejected (ballot)",
                );
                return EnvelopeState::Invalid;
            }
        }

        if !self.is_newer_statement(node_id, &envelope.statement) {
            // On the Slot::process_ballot_envelope production path, the
            // upstream is_stale_ballot_statement check already rejected
            // stale envelopes against the same latest_envelopes map (no
            // mutation occurs between the two checks). This gate is a
            // defensive guard for direct callers of process_envelope.
            // Demoted to debug for consistency with the stale_ballot gate
            // (also debug); see #1814.
            let incoming = crate::compare::ballot_summary_of(&envelope.statement.pledges);
            let stored = self.stored_ballot_summary(node_id);
            tracing::debug!(
                target: "henyey::envelope_path",
                slot = envelope.statement.slot_index,
                node_id = ?node_id,
                scp_gate = "not_newer_statement",
                ?incoming,
                ?stored,
                "scp receive rejected (ballot)",
            );
            return EnvelopeState::Invalid;
        }

        if self.phase == BallotPhase::Externalize {
            if self.statement_value_matches_commit(&envelope.statement) {
                self.latest_envelopes
                    .insert(node_id.clone(), envelope.clone());
                return EnvelopeState::Valid;
            }
            tracing::info!(
                target: "henyey::envelope_path",
                slot = envelope.statement.slot_index,
                node_id = ?node_id,
                scp_gate = "externalize_value_mismatch",
                "scp receive rejected (ballot)",
            );
            return EnvelopeState::Invalid;
        }

        // Parity: stellar-core BallotProtocol.cpp:208-211 clears
        // mFullyValidated here, before advanceSlot, so that any
        // send_latest_envelope during advance_slot sees the correct state.
        //
        // The henyey-specific `MaybeValidDeferred` variant does NOT
        // clear (see the enum doc comment on `ValidationLevel` and
        // issues #1795 / #1798). Using `clears_fully_validated()`
        // keeps the distinction in the type system.
        if validation.clears_fully_validated() {
            self.fully_validated = false;
        }

        // Store the envelope
        self.latest_envelopes
            .insert(node_id.clone(), envelope.clone());

        self.advance_slot(&envelope.statement, ctx)
    }

    fn statement_value_matches_commit(&self, statement: &ScpStatement) -> bool {
        let commit = match self.commit.as_ref() {
            Some(commit) => commit,
            None => return false,
        };

        match &statement.pledges {
            ScpStatementPledges::Prepare(prep) => commit.value == prep.ballot.value,
            ScpStatementPledges::Confirm(conf) => commit.value == conf.ballot.value,
            ScpStatementPledges::Externalize(ext) => commit.value == ext.commit.value,
            _ => false,
        }
    }

    fn collect_hint_ballots(hint: &ScpStatement) -> Vec<ScpBallot> {
        let mut hint_ballots: Vec<ScpBallot> = Vec::new();
        match &hint.pledges {
            ScpStatementPledges::Prepare(prep) => {
                hint_ballots.push(prep.ballot.clone());
                if let Some(prepared) = &prep.prepared {
                    hint_ballots.push(prepared.clone());
                }
                if let Some(prepared_prime) = &prep.prepared_prime {
                    hint_ballots.push(prepared_prime.clone());
                }
            }
            ScpStatementPledges::Confirm(conf) => {
                let value = conf.ballot.value.clone();
                hint_ballots.push(ScpBallot {
                    counter: conf.n_prepared,
                    value: value.clone(),
                });
                hint_ballots.push(ScpBallot {
                    counter: u32::MAX,
                    value,
                });
            }
            ScpStatementPledges::Externalize(ext) => {
                hint_ballots.push(ScpBallot {
                    counter: u32::MAX,
                    value: ext.commit.value.clone(),
                });
            }
            // Nominate pledges should never reach ballot protocol
            _ => debug_assert!(false, "unexpected pledge type in ballot protocol"),
        }
        hint_ballots
    }

    /// Restore state from a saved envelope (for crash recovery).
    ///
    /// This method is used to restore the ballot protocol state from a previously
    /// saved envelope when restarting after a crash. It sets up the internal state
    /// to match what it would have been after processing that envelope.
    ///
    /// # Arguments
    /// * `envelope` - The envelope to restore state from
    ///
    /// # Returns
    /// True if state was successfully restored, false if the envelope is invalid
    /// for state restoration.
    pub(crate) fn set_state_from_envelope(&mut self, envelope: &ScpEnvelope) -> bool {
        let valid = match &envelope.statement.pledges {
            ScpStatementPledges::Prepare(prep) => {
                let value = prep.ballot.value.clone();
                self.current_ballot = Some(prep.ballot.clone());
                self.prepared = prep.prepared.clone();
                self.prepared_prime = prep.prepared_prime.clone();
                if prep.n_c != 0 {
                    self.commit = Some(ScpBallot {
                        counter: prep.n_c,
                        value: value.clone(),
                    });
                }
                if prep.n_h != 0 {
                    self.high_ballot = Some(ScpBallot {
                        counter: prep.n_h,
                        value: value.clone(),
                    });
                }
                self.phase = BallotPhase::Prepare;
                true
            }
            ScpStatementPledges::Confirm(conf) => {
                let value = conf.ballot.value.clone();
                self.current_ballot = Some(conf.ballot.clone());
                self.prepared = Some(ScpBallot {
                    counter: conf.n_prepared,
                    value: value.clone(),
                });
                self.prepared_prime = None;
                self.commit = Some(ScpBallot {
                    counter: conf.n_commit,
                    value: value.clone(),
                });
                self.high_ballot = Some(ScpBallot {
                    counter: conf.n_h,
                    value: value.clone(),
                });
                self.phase = BallotPhase::Confirm;
                true
            }
            ScpStatementPledges::Externalize(ext) => {
                let value = ext.commit.value.clone();
                self.commit = Some(ext.commit.clone());
                self.high_ballot = Some(ScpBallot {
                    counter: ext.n_h,
                    value: value.clone(),
                });
                self.current_ballot = Some(ScpBallot {
                    counter: u32::MAX,
                    value: value.clone(),
                });
                self.prepared = Some(ScpBallot {
                    counter: u32::MAX,
                    value: value.clone(),
                });
                self.phase = BallotPhase::Externalize;
                true
            }
            _ => false,
        };

        if valid {
            self.latest_envelopes
                .insert(envelope.statement.node_id.clone(), envelope.clone());
            self.last_envelope = Some(envelope.clone());
            // Spec: SCP_SPEC §9.13 — set last_envelope_emit to prevent re-emission
            // after crash recovery.
            self.last_envelope_emit = Some(envelope.clone());
        }

        valid
    }

    /// Bump the ballot to a specific counter value.
    ///
    /// This is used when we need to bump to a specific ballot counter,
    /// for example when catching up to a higher ballot counter seen on the network.
    ///
    /// # Arguments
    /// * `local_node_id` - Our node ID
    /// * `local_quorum_set` - Our quorum set
    /// * `driver` - The SCP driver
    /// * `slot_index` - The slot index
    /// * `value` - The value for the ballot
    /// * `counter` - The specific counter to bump to
    ///
    /// # Returns
    /// True if the ballot was bumped, false if the operation failed.
    pub(crate) fn bump_state<D: SCPDriver>(
        &mut self,
        ctx: &SlotContext<'_, D>,
        value: Value,
        counter: u32,
    ) -> bool {
        if self.phase != BallotPhase::Prepare && self.phase != BallotPhase::Confirm {
            return false;
        }

        let effective_value = if let Some(ref override_val) = self.value_override {
            // Use the value that we saw confirmed prepared
            // or that we at least voted to commit to
            override_val.clone()
        } else {
            value
        };

        let ballot = ScpBallot {
            counter,
            value: effective_value,
        };

        let updated = self.update_current_value(&ballot);

        if updated {
            self.emit_current_state(ctx);
            self.check_heard_from_quorum(ctx);
        }

        updated
    }
}

impl Default for BallotProtocol {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
impl BallotProtocol {
    /// Test helper: set current_message_level to simulate deep recursion.
    pub fn set_current_message_level_for_test(&mut self, level: u32) {
        self.current_message_level = level;
    }

    /// Test helper: expose advance_slot for testing.
    pub(crate) fn advance_slot_for_test<D: SCPDriver>(
        &mut self,
        hint: &ScpStatement,
        ctx: &SlotContext<'_, D>,
    ) -> EnvelopeState {
        self.advance_slot(hint, ctx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::driver::ValidationLevel;
    use crate::test_utils::{
        make_node_id, make_quorum_set, make_value, MockDriver, MockDriverBuilder,
    };
    use crate::SlotContext;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;
    use std::time::Duration;
    use stellar_xdr::curr::{ScpNomination, VecM};

    /// Helper to construct a `SlotContext` from the old four-parameter pattern.
    macro_rules! ctx {
        ($node:expr, $qs:expr, $driver:expr, $slot:expr) => {
            SlotContext {
                local_node_id: $node,
                local_quorum_set: $qs,
                driver: $driver,
                slot_index: $slot,
            }
        };
    }

    #[test]
    fn test_ballot_protocol_new() {
        let bp = BallotProtocol::new();
        assert_eq!(bp.phase(), BallotPhase::Prepare);
        assert!(bp.current_ballot().is_none());
        assert!(bp.prepared().is_none());
        assert!(!bp.is_externalized());
    }

    #[test]
    fn test_ballot_compare() {
        let b1 = ScpBallot {
            counter: 1,
            value: vec![1].try_into().unwrap(),
        };
        let b2 = ScpBallot {
            counter: 2,
            value: vec![1].try_into().unwrap(),
        };
        let b3 = ScpBallot {
            counter: 1,
            value: vec![2].try_into().unwrap(),
        };

        assert_eq!(ballot_compare(&b1, &b1), std::cmp::Ordering::Equal);
        assert_eq!(ballot_compare(&b1, &b2), std::cmp::Ordering::Less);
        assert_eq!(ballot_compare(&b2, &b1), std::cmp::Ordering::Greater);
        // Same counter, different value - compared by value
        assert_eq!(ballot_compare(&b1, &b3), std::cmp::Ordering::Less);
    }

    fn make_prepare_envelope(
        node_id: NodeId,
        slot_index: u64,
        quorum_set: &ScpQuorumSet,
        ballot: ScpBallot,
    ) -> ScpEnvelope {
        let prep = ScpStatementPrepare {
            quorum_set_hash: hash_quorum_set(quorum_set).into(),
            ballot,
            prepared: None,
            prepared_prime: None,
            n_c: 0,
            n_h: 0,
        };
        let statement = ScpStatement {
            node_id,
            slot_index,
            pledges: ScpStatementPledges::Prepare(prep),
        };
        ScpEnvelope {
            statement,
            signature: stellar_xdr::curr::Signature(Vec::new().try_into().unwrap_or_default()),
        }
    }

    struct PrepareEnvelopeCounters {
        prepared: Option<ScpBallot>,
        prepared_prime: Option<ScpBallot>,
        n_c: u32,
        n_h: u32,
    }

    fn make_prepare_envelope_with_counters(
        node_id: NodeId,
        slot_index: u64,
        quorum_set: &ScpQuorumSet,
        ballot: ScpBallot,
        counters: PrepareEnvelopeCounters,
    ) -> ScpEnvelope {
        let prep = ScpStatementPrepare {
            quorum_set_hash: hash_quorum_set(quorum_set).into(),
            ballot,
            prepared: counters.prepared,
            prepared_prime: counters.prepared_prime,
            n_c: counters.n_c,
            n_h: counters.n_h,
        };
        let statement = ScpStatement {
            node_id,
            slot_index,
            pledges: ScpStatementPledges::Prepare(prep),
        };
        ScpEnvelope {
            statement,
            signature: stellar_xdr::curr::Signature(Vec::new().try_into().unwrap_or_default()),
        }
    }

    fn make_confirm_envelope(
        node_id: NodeId,
        slot_index: u64,
        quorum_set: &ScpQuorumSet,
        ballot: ScpBallot,
    ) -> ScpEnvelope {
        let conf = ScpStatementConfirm {
            ballot,
            n_prepared: 0,
            n_commit: 0,
            n_h: 0,
            quorum_set_hash: hash_quorum_set(quorum_set).into(),
        };
        let statement = ScpStatement {
            node_id,
            slot_index,
            pledges: ScpStatementPledges::Confirm(conf),
        };
        ScpEnvelope {
            statement,
            signature: stellar_xdr::curr::Signature(Vec::new().try_into().unwrap_or_default()),
        }
    }

    fn make_confirm_envelope_with_counters(
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
            signature: stellar_xdr::curr::Signature(Vec::new().try_into().unwrap_or_default()),
        }
    }

    fn make_nomination_envelope(
        node_id: NodeId,
        slot_index: u64,
        quorum_set: &ScpQuorumSet,
    ) -> ScpEnvelope {
        let nomination = ScpNomination {
            quorum_set_hash: hash_quorum_set(quorum_set).into(),
            votes: VecM::default(),
            accepted: VecM::default(),
        };
        let statement = ScpStatement {
            node_id,
            slot_index,
            pledges: ScpStatementPledges::Nominate(nomination),
        };
        ScpEnvelope {
            statement,
            signature: stellar_xdr::curr::Signature(Vec::new().try_into().unwrap_or_default()),
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
            signature: stellar_xdr::curr::Signature(Vec::new().try_into().unwrap_or_default()),
        }
    }

    fn make_quorum_set_hashless_confirm_envelope(
        node_id: NodeId,
        slot_index: u64,
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
            quorum_set_hash: stellar_xdr::curr::Hash([0u8; 32]),
        };
        let statement = ScpStatement {
            node_id,
            slot_index,
            pledges: ScpStatementPledges::Confirm(conf),
        };
        ScpEnvelope {
            statement,
            signature: stellar_xdr::curr::Signature(Vec::new().try_into().unwrap_or_default()),
        }
    }

    #[test]
    fn test_ballot_rejects_non_ballot_pledges() {
        let node = make_node_id(1);
        let quorum_set = make_quorum_set(vec![node.clone()], 1);
        let driver = Arc::new(MockDriver::with_quorum_set(quorum_set.clone()));
        let mut ballot = BallotProtocol::new();

        let env = make_nomination_envelope(make_node_id(2), 1, &quorum_set);
        let state = ballot.process_envelope(
            &env,
            &ctx!(&node, &quorum_set, &driver, 1),
            crate::ValidationLevel::FullyValidated,
        );
        assert_eq!(state, EnvelopeState::Invalid);
    }

    #[test]
    fn test_ballot_heard_from_quorum_callback() {
        let node_a = make_node_id(1);
        let node_b = make_node_id(2);
        let node_c = make_node_id(3);
        let quorum_set = make_quorum_set(vec![node_a.clone(), node_b.clone(), node_c.clone()], 2);
        let driver = Arc::new(MockDriver::with_quorum_set(quorum_set.clone()));
        let mut ballot = BallotProtocol::new();

        let value = make_value(&[1, 2, 3]);
        assert!(ballot.bump(
            &ctx!(&node_a, &quorum_set, &driver, 1),
            value.clone(),
            false
        ));

        let current = ballot.current_ballot().expect("current ballot").clone();
        let env_b = make_prepare_envelope(node_b, 1, &quorum_set, current.clone());
        let env_c = make_prepare_envelope(node_c, 1, &quorum_set, current);

        ballot.process_envelope(
            &env_b,
            &ctx!(&node_a, &quorum_set, &driver, 1),
            crate::ValidationLevel::FullyValidated,
        );
        ballot.process_envelope(
            &env_c,
            &ctx!(&node_a, &quorum_set, &driver, 1),
            crate::ValidationLevel::FullyValidated,
        );

        assert_eq!(driver.heard_from_quorum.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn test_ballot_statement_ordering() {
        let node = make_node_id(1);
        let quorum_set = make_quorum_set(vec![node.clone()], 1);
        let driver = Arc::new(MockDriver::with_quorum_set(quorum_set.clone()));
        let mut ballot = BallotProtocol::new();
        let value = make_value(&[7]);
        let ballot_value = ScpBallot { counter: 1, value };

        let prepare = make_prepare_envelope(make_node_id(2), 2, &quorum_set, ballot_value.clone());
        let confirm = make_confirm_envelope(make_node_id(2), 2, &quorum_set, ballot_value.clone());

        let first = ballot.process_envelope(
            &prepare,
            &ctx!(&node, &quorum_set, &driver, 2),
            crate::ValidationLevel::FullyValidated,
        );
        let second = ballot.process_envelope(
            &confirm,
            &ctx!(&node, &quorum_set, &driver, 2),
            crate::ValidationLevel::FullyValidated,
        );
        let third = ballot.process_envelope(
            &prepare,
            &ctx!(&node, &quorum_set, &driver, 2),
            crate::ValidationLevel::FullyValidated,
        );

        assert!(matches!(
            first,
            EnvelopeState::Valid | EnvelopeState::ValidNew
        ));
        assert!(matches!(
            second,
            EnvelopeState::Valid | EnvelopeState::ValidNew
        ));
        assert_eq!(third, EnvelopeState::Invalid);
    }

    #[test]
    fn test_ballot_statement_ordering_confirm_counters() {
        let node = make_node_id(1);
        let quorum_set = make_quorum_set(vec![node.clone()], 1);
        let driver = Arc::new(MockDriver::with_quorum_set(quorum_set.clone()));
        let mut ballot = BallotProtocol::new();
        let ballot_value = ScpBallot {
            counter: 2,
            value: make_value(&[9]),
        };

        let older = make_confirm_envelope_with_counters(
            make_node_id(2),
            4,
            &quorum_set,
            ballot_value.clone(),
            1,
            0,
            1,
        );
        let newer = make_confirm_envelope_with_counters(
            make_node_id(2),
            4,
            &quorum_set,
            ballot_value.clone(),
            2,
            0,
            1,
        );

        let first = ballot.process_envelope(
            &older,
            &ctx!(&node, &quorum_set, &driver, 4),
            crate::ValidationLevel::FullyValidated,
        );
        let second = ballot.process_envelope(
            &newer,
            &ctx!(&node, &quorum_set, &driver, 4),
            crate::ValidationLevel::FullyValidated,
        );
        let third = ballot.process_envelope(
            &older,
            &ctx!(&node, &quorum_set, &driver, 4),
            crate::ValidationLevel::FullyValidated,
        );

        assert!(matches!(
            first,
            EnvelopeState::Valid | EnvelopeState::ValidNew
        ));
        assert!(matches!(
            second,
            EnvelopeState::Valid | EnvelopeState::ValidNew
        ));
        assert_eq!(third, EnvelopeState::Invalid);
    }

    #[test]
    fn test_ballot_statement_ordering_prepare_n_h() {
        let node = make_node_id(1);
        let quorum_set = make_quorum_set(vec![node.clone()], 1);
        let driver = Arc::new(MockDriver::with_quorum_set(quorum_set.clone()));
        let mut ballot = BallotProtocol::new();
        let ballot_value = ScpBallot {
            counter: 3,
            value: make_value(&[7]),
        };
        let prepared = Some(ScpBallot {
            counter: 2,
            value: make_value(&[6]),
        });

        let older = make_prepare_envelope_with_counters(
            make_node_id(2),
            5,
            &quorum_set,
            ballot_value.clone(),
            PrepareEnvelopeCounters {
                prepared: prepared.clone(),
                prepared_prime: None,
                n_c: 0,
                n_h: 1,
            },
        );
        let newer = make_prepare_envelope_with_counters(
            make_node_id(2),
            5,
            &quorum_set,
            ballot_value.clone(),
            PrepareEnvelopeCounters {
                prepared,
                prepared_prime: None,
                n_c: 0,
                n_h: 2,
            },
        );

        let first = ballot.process_envelope(
            &older,
            &ctx!(&node, &quorum_set, &driver, 5),
            crate::ValidationLevel::FullyValidated,
        );
        let second = ballot.process_envelope(
            &newer,
            &ctx!(&node, &quorum_set, &driver, 5),
            crate::ValidationLevel::FullyValidated,
        );
        let third = ballot.process_envelope(
            &older,
            &ctx!(&node, &quorum_set, &driver, 5),
            crate::ValidationLevel::FullyValidated,
        );

        assert!(matches!(
            first,
            EnvelopeState::Valid | EnvelopeState::ValidNew
        ));
        assert!(matches!(
            second,
            EnvelopeState::Valid | EnvelopeState::ValidNew
        ));
        assert_eq!(third, EnvelopeState::Invalid);
    }

    #[test]
    fn test_ballot_statement_ordering_prepare_prepared() {
        let node = make_node_id(1);
        let quorum_set = make_quorum_set(vec![node.clone()], 1);
        let driver = Arc::new(MockDriver::with_quorum_set(quorum_set.clone()));
        let mut ballot = BallotProtocol::new();
        let ballot_value = ScpBallot {
            counter: 3,
            value: make_value(&[4]),
        };

        let older = make_prepare_envelope_with_counters(
            make_node_id(2),
            6,
            &quorum_set,
            ballot_value.clone(),
            PrepareEnvelopeCounters {
                prepared: Some(ScpBallot {
                    counter: 1,
                    value: make_value(&[1]),
                }),
                prepared_prime: None,
                n_c: 0,
                n_h: 1,
            },
        );
        let newer = make_prepare_envelope_with_counters(
            make_node_id(2),
            6,
            &quorum_set,
            ballot_value.clone(),
            PrepareEnvelopeCounters {
                prepared: Some(ScpBallot {
                    counter: 2,
                    value: make_value(&[1]),
                }),
                prepared_prime: None,
                n_c: 0,
                n_h: 1,
            },
        );

        let first = ballot.process_envelope(
            &older,
            &ctx!(&node, &quorum_set, &driver, 6),
            crate::ValidationLevel::FullyValidated,
        );
        let second = ballot.process_envelope(
            &newer,
            &ctx!(&node, &quorum_set, &driver, 6),
            crate::ValidationLevel::FullyValidated,
        );
        let third = ballot.process_envelope(
            &older,
            &ctx!(&node, &quorum_set, &driver, 6),
            crate::ValidationLevel::FullyValidated,
        );

        assert!(matches!(
            first,
            EnvelopeState::Valid | EnvelopeState::ValidNew
        ));
        assert!(matches!(
            second,
            EnvelopeState::Valid | EnvelopeState::ValidNew
        ));
        assert_eq!(third, EnvelopeState::Invalid);
    }

    #[test]
    fn test_ballot_statement_ordering_prepare_prepared_prime() {
        let node = make_node_id(1);
        let quorum_set = make_quorum_set(vec![node.clone()], 1);
        let driver = Arc::new(MockDriver::with_quorum_set(quorum_set.clone()));
        let mut ballot = BallotProtocol::new();
        let ballot_value = ScpBallot {
            counter: 3,
            value: make_value(&[5]),
        };

        let older = make_prepare_envelope_with_counters(
            make_node_id(2),
            7,
            &quorum_set,
            ballot_value.clone(),
            PrepareEnvelopeCounters {
                prepared: Some(ScpBallot {
                    counter: 2,
                    value: make_value(&[2]),
                }),
                prepared_prime: Some(ScpBallot {
                    counter: 1,
                    value: make_value(&[9]),
                }),
                n_c: 0,
                n_h: 1,
            },
        );
        let newer = make_prepare_envelope_with_counters(
            make_node_id(2),
            7,
            &quorum_set,
            ballot_value.clone(),
            PrepareEnvelopeCounters {
                prepared: Some(ScpBallot {
                    counter: 2,
                    value: make_value(&[2]),
                }),
                prepared_prime: Some(ScpBallot {
                    counter: 2,
                    value: make_value(&[9]),
                }),
                n_c: 0,
                n_h: 1,
            },
        );

        let first = ballot.process_envelope(
            &older,
            &ctx!(&node, &quorum_set, &driver, 7),
            crate::ValidationLevel::FullyValidated,
        );
        let second = ballot.process_envelope(
            &newer,
            &ctx!(&node, &quorum_set, &driver, 7),
            crate::ValidationLevel::FullyValidated,
        );
        let third = ballot.process_envelope(
            &older,
            &ctx!(&node, &quorum_set, &driver, 7),
            crate::ValidationLevel::FullyValidated,
        );

        assert!(matches!(
            first,
            EnvelopeState::Valid | EnvelopeState::ValidNew
        ));
        assert!(matches!(
            second,
            EnvelopeState::Valid | EnvelopeState::ValidNew
        ));
        assert_eq!(third, EnvelopeState::Invalid);
    }

    #[test]
    fn test_ballot_statement_ordering_confirm_n_h() {
        let node = make_node_id(1);
        let quorum_set = make_quorum_set(vec![node.clone()], 1);
        let driver = Arc::new(MockDriver::with_quorum_set(quorum_set.clone()));
        let mut ballot = BallotProtocol::new();
        let ballot_value = ScpBallot {
            counter: 4,
            value: make_value(&[8]),
        };

        let older = make_confirm_envelope_with_counters(
            make_node_id(2),
            8,
            &quorum_set,
            ballot_value.clone(),
            1,
            0,
            1,
        );
        let newer = make_confirm_envelope_with_counters(
            make_node_id(2),
            8,
            &quorum_set,
            ballot_value.clone(),
            1,
            0,
            2,
        );

        let first = ballot.process_envelope(
            &older,
            &ctx!(&node, &quorum_set, &driver, 8),
            crate::ValidationLevel::FullyValidated,
        );
        let second = ballot.process_envelope(
            &newer,
            &ctx!(&node, &quorum_set, &driver, 8),
            crate::ValidationLevel::FullyValidated,
        );
        let third = ballot.process_envelope(
            &older,
            &ctx!(&node, &quorum_set, &driver, 8),
            crate::ValidationLevel::FullyValidated,
        );

        assert!(matches!(
            first,
            EnvelopeState::Valid | EnvelopeState::ValidNew
        ));
        assert!(matches!(
            second,
            EnvelopeState::Valid | EnvelopeState::ValidNew
        ));
        assert_eq!(third, EnvelopeState::Invalid);
    }

    #[test]
    fn test_ballot_timeout_bumps_counter() {
        let node = make_node_id(1);
        let other = make_node_id(99);
        let quorum_set = make_quorum_set(vec![node.clone(), other.clone()], 2);
        let driver = Arc::new(MockDriver::with_quorum_set(quorum_set.clone()));
        let mut ballot = BallotProtocol::new();
        let value = make_value(&[5]);

        assert!(ballot.bump(&ctx!(&node, &quorum_set, &driver, 3), value.clone(), false));
        assert_eq!(ballot.current_ballot_counter(), Some(1));

        assert!(ballot.bump_timeout(&ctx!(&node, &quorum_set, &driver, 3), None));
        assert_eq!(ballot.current_ballot_counter(), Some(2));
    }

    #[test]
    fn test_ballot_process_current_state_skips_self_when_not_validated() {
        let local = make_node_id(1);
        let remote = make_node_id(2);
        let quorum_set = make_quorum_set(vec![local.clone(), remote.clone()], 1);
        let driver = Arc::new(MockDriver::with_quorum_set(quorum_set.clone()));
        let mut ballot = BallotProtocol::new();

        let ballot_local = ScpBallot {
            counter: 1,
            value: make_value(&[1]),
        };
        let ballot_remote = ScpBallot {
            counter: 1,
            value: make_value(&[2]),
        };
        let env_local = make_prepare_envelope(local.clone(), 13, &quorum_set, ballot_local);
        let env_remote = make_prepare_envelope(remote.clone(), 13, &quorum_set, ballot_remote);

        ballot.process_envelope(
            &env_local,
            &ctx!(&local, &quorum_set, &driver, 13),
            crate::ValidationLevel::FullyValidated,
        );
        ballot.process_envelope(
            &env_remote,
            &ctx!(&local, &quorum_set, &driver, 13),
            crate::ValidationLevel::FullyValidated,
        );

        let mut seen = Vec::new();
        ballot.process_current_state(
            |env| {
                seen.push(env.statement.node_id.clone());
                true
            },
            &local,
            false,
            false,
        );

        assert!(seen.contains(&remote));
        assert!(!seen.contains(&local));
    }

    #[test]
    fn test_ballot_process_current_state_includes_self_when_forced() {
        let local = make_node_id(1);
        let quorum_set = make_quorum_set(vec![local.clone()], 1);
        let driver = Arc::new(MockDriver::with_quorum_set(quorum_set.clone()));
        let mut ballot = BallotProtocol::new();

        let ballot_local = ScpBallot {
            counter: 1,
            value: make_value(&[3]),
        };
        let env_local = make_prepare_envelope(local.clone(), 14, &quorum_set, ballot_local);
        ballot.process_envelope(
            &env_local,
            &ctx!(&local, &quorum_set, &driver, 14),
            crate::ValidationLevel::FullyValidated,
        );

        let mut seen = Vec::new();
        ballot.process_current_state(
            |env| {
                seen.push(env.statement.node_id.clone());
                true
            },
            &local,
            false,
            true,
        );

        assert!(seen.contains(&local));
    }

    #[test]
    fn test_ballot_rejects_bumps_after_externalize() {
        let node = make_node_id(1);
        let node2 = make_node_id(2);
        let node3 = make_node_id(3);
        let node4 = make_node_id(4);
        let node5 = make_node_id(5);
        let quorum_set = make_quorum_set(
            vec![
                node.clone(),
                node2.clone(),
                node3.clone(),
                node4.clone(),
                node5.clone(),
            ],
            4,
        );
        let driver = Arc::new(MockDriver::with_quorum_set(quorum_set.clone()));
        let mut ballot = BallotProtocol::new();

        let value = make_value(&[1]);
        assert!(ballot.bump(&ctx!(&node, &quorum_set, &driver, 15), value.clone(), false));

        let current = ballot.current_ballot().expect("current ballot").clone();
        let env2 = make_confirm_envelope_with_counters(
            node2.clone(),
            15,
            &quorum_set,
            current.clone(),
            current.counter,
            current.counter,
            current.counter,
        );
        let env3 = make_confirm_envelope_with_counters(
            node3.clone(),
            15,
            &quorum_set,
            current.clone(),
            current.counter,
            current.counter,
            current.counter,
        );
        let env4 = make_confirm_envelope_with_counters(
            node4.clone(),
            15,
            &quorum_set,
            current.clone(),
            current.counter,
            current.counter,
            current.counter,
        );

        ballot.process_envelope(
            &env2,
            &ctx!(&node, &quorum_set, &driver, 15),
            crate::ValidationLevel::FullyValidated,
        );
        ballot.process_envelope(
            &env3,
            &ctx!(&node, &quorum_set, &driver, 15),
            crate::ValidationLevel::FullyValidated,
        );
        ballot.process_envelope(
            &env4,
            &ctx!(&node, &quorum_set, &driver, 15),
            crate::ValidationLevel::FullyValidated,
        );

        assert!(ballot.is_externalized());
        let externalized_value = ballot.get_externalized_value().expect("value").clone();

        let bump_ballot = ScpBallot {
            counter: 2,
            value: make_value(&[2]),
        };
        let bump_env2 = make_quorum_set_hashless_confirm_envelope(
            node2,
            15,
            bump_ballot.clone(),
            bump_ballot.counter,
            bump_ballot.counter,
            bump_ballot.counter,
        );
        let bump_env3 = make_quorum_set_hashless_confirm_envelope(
            node3,
            15,
            bump_ballot.clone(),
            bump_ballot.counter,
            bump_ballot.counter,
            bump_ballot.counter,
        );
        let bump_env4 = make_quorum_set_hashless_confirm_envelope(
            node4,
            15,
            bump_ballot.clone(),
            bump_ballot.counter,
            bump_ballot.counter,
            bump_ballot.counter,
        );

        assert_eq!(
            ballot.process_envelope(
                &bump_env2,
                &ctx!(&node, &quorum_set, &driver, 15),
                crate::ValidationLevel::FullyValidated
            ),
            EnvelopeState::Invalid
        );
        assert_eq!(
            ballot.process_envelope(
                &bump_env3,
                &ctx!(&node, &quorum_set, &driver, 15),
                crate::ValidationLevel::FullyValidated
            ),
            EnvelopeState::Invalid
        );
        assert_eq!(
            ballot.process_envelope(
                &bump_env4,
                &ctx!(&node, &quorum_set, &driver, 15),
                crate::ValidationLevel::FullyValidated
            ),
            EnvelopeState::Invalid
        );

        assert_eq!(
            ballot.get_externalized_value().cloned(),
            Some(externalized_value)
        );
    }

    #[test]
    fn test_ballot_commit_range_externalizes() {
        let node = make_node_id(1);
        let node2 = make_node_id(2);
        let node3 = make_node_id(3);
        let node4 = make_node_id(4);
        let node5 = make_node_id(5);
        let quorum_set = make_quorum_set(
            vec![
                node.clone(),
                node2.clone(),
                node3.clone(),
                node4.clone(),
                node5.clone(),
            ],
            4,
        );
        let driver = Arc::new(MockDriver::with_quorum_set(quorum_set.clone()));
        let mut ballot = BallotProtocol::new();

        let value = make_value(&[9]);
        assert!(ballot.bump(&ctx!(&node, &quorum_set, &driver, 16), value.clone(), false));

        let current = ballot.current_ballot().expect("current ballot").clone();
        let prep2 = make_prepare_envelope(node2.clone(), 16, &quorum_set, current.clone());
        let prep3 = make_prepare_envelope(node3.clone(), 16, &quorum_set, current.clone());
        let prep4 = make_prepare_envelope(node4.clone(), 16, &quorum_set, current.clone());
        let prep5 = make_prepare_envelope(node5.clone(), 16, &quorum_set, current.clone());

        ballot.process_envelope(
            &prep2,
            &ctx!(&node, &quorum_set, &driver, 16),
            crate::ValidationLevel::FullyValidated,
        );
        ballot.process_envelope(
            &prep3,
            &ctx!(&node, &quorum_set, &driver, 16),
            crate::ValidationLevel::FullyValidated,
        );
        ballot.process_envelope(
            &prep4,
            &ctx!(&node, &quorum_set, &driver, 16),
            crate::ValidationLevel::FullyValidated,
        );
        ballot.process_envelope(
            &prep5,
            &ctx!(&node, &quorum_set, &driver, 16),
            crate::ValidationLevel::FullyValidated,
        );

        let prepared2 = make_prepare_envelope_with_counters(
            node2.clone(),
            16,
            &quorum_set,
            current.clone(),
            PrepareEnvelopeCounters {
                prepared: Some(current.clone()),
                prepared_prime: None,
                n_c: current.counter,
                n_h: current.counter,
            },
        );
        let prepared3 = make_prepare_envelope_with_counters(
            node3.clone(),
            16,
            &quorum_set,
            current.clone(),
            PrepareEnvelopeCounters {
                prepared: Some(current.clone()),
                prepared_prime: None,
                n_c: current.counter,
                n_h: current.counter,
            },
        );
        let prepared4 = make_prepare_envelope_with_counters(
            node4.clone(),
            16,
            &quorum_set,
            current.clone(),
            PrepareEnvelopeCounters {
                prepared: Some(current.clone()),
                prepared_prime: None,
                n_c: current.counter,
                n_h: current.counter,
            },
        );
        let prepared5 = make_prepare_envelope_with_counters(
            node5.clone(),
            16,
            &quorum_set,
            current.clone(),
            PrepareEnvelopeCounters {
                prepared: Some(current.clone()),
                prepared_prime: None,
                n_c: current.counter,
                n_h: current.counter,
            },
        );

        ballot.process_envelope(
            &prepared2,
            &ctx!(&node, &quorum_set, &driver, 16),
            crate::ValidationLevel::FullyValidated,
        );
        ballot.process_envelope(
            &prepared3,
            &ctx!(&node, &quorum_set, &driver, 16),
            crate::ValidationLevel::FullyValidated,
        );
        ballot.process_envelope(
            &prepared4,
            &ctx!(&node, &quorum_set, &driver, 16),
            crate::ValidationLevel::FullyValidated,
        );
        ballot.process_envelope(
            &prepared5,
            &ctx!(&node, &quorum_set, &driver, 16),
            crate::ValidationLevel::FullyValidated,
        );

        assert!(matches!(ballot.phase(), BallotPhase::Confirm));
        assert_eq!(ballot.commit().map(|b| b.counter), Some(1));

        let confirm1 = make_confirm_envelope_with_counters(
            node2.clone(),
            16,
            &quorum_set,
            ScpBallot {
                counter: 4,
                value: value.clone(),
            },
            2,
            2,
            4,
        );
        let confirm2 = make_confirm_envelope_with_counters(
            node3.clone(),
            16,
            &quorum_set,
            ScpBallot {
                counter: 6,
                value: value.clone(),
            },
            2,
            2,
            6,
        );
        let confirm4 = make_confirm_envelope_with_counters(
            node5,
            16,
            &quorum_set,
            ScpBallot {
                counter: 6,
                value: value.clone(),
            },
            3,
            3,
            6,
        );

        ballot.process_envelope(
            &confirm1,
            &ctx!(&node, &quorum_set, &driver, 16),
            crate::ValidationLevel::FullyValidated,
        );
        ballot.process_envelope(
            &confirm2,
            &ctx!(&node, &quorum_set, &driver, 16),
            crate::ValidationLevel::FullyValidated,
        );

        assert!(!ballot.is_externalized());

        ballot.process_envelope(
            &confirm4,
            &ctx!(&node, &quorum_set, &driver, 16),
            crate::ValidationLevel::FullyValidated,
        );

        assert!(ballot.is_externalized());
        assert_eq!(ballot.get_externalized_value(), Some(&value));
        assert_eq!(ballot.commit().map(|b| b.counter), Some(3));
    }

    #[test]
    fn test_ballot_statement_sanity_prepare_constraints() {
        let node = make_node_id(1);
        let quorum_set = make_quorum_set(vec![node.clone()], 1);
        let driver = Arc::new(MockDriver::with_quorum_set(quorum_set.clone()));
        let ballot = BallotProtocol::new();

        let prepared = ScpBallot {
            counter: 2,
            value: make_value(&[1]),
        };
        let prepared_prime = ScpBallot {
            counter: 1,
            value: make_value(&[1]),
        };
        let prep = ScpStatementPrepare {
            quorum_set_hash: hash_quorum_set(&quorum_set).into(),
            ballot: ScpBallot {
                counter: 3,
                value: make_value(&[2]),
            },
            prepared: Some(prepared),
            prepared_prime: Some(prepared_prime),
            n_c: 0,
            n_h: 0,
        };
        let statement = ScpStatement {
            node_id: node.clone(),
            slot_index: 7,
            pledges: ScpStatementPledges::Prepare(prep),
        };

        assert!(!ballot.is_statement_sane(&statement, &ctx!(&node, &quorum_set, &driver, 7)));

        let prep_bad_h = ScpStatementPrepare {
            quorum_set_hash: hash_quorum_set(&quorum_set).into(),
            ballot: ScpBallot {
                counter: 3,
                value: make_value(&[3]),
            },
            prepared: Some(ScpBallot {
                counter: 2,
                value: make_value(&[4]),
            }),
            prepared_prime: None,
            n_c: 0,
            n_h: 5,
        };
        let statement_bad_h = ScpStatement {
            node_id: node.clone(),
            slot_index: 8,
            pledges: ScpStatementPledges::Prepare(prep_bad_h),
        };
        assert!(!ballot.is_statement_sane(&statement_bad_h, &ctx!(&node, &quorum_set, &driver, 8)));

        let prep_bad_c = ScpStatementPrepare {
            quorum_set_hash: hash_quorum_set(&quorum_set).into(),
            ballot: ScpBallot {
                counter: 3,
                value: make_value(&[5]),
            },
            prepared: Some(ScpBallot {
                counter: 3,
                value: make_value(&[6]),
            }),
            prepared_prime: None,
            n_c: 1,
            n_h: 0,
        };
        let statement_bad_c = ScpStatement {
            node_id: node.clone(),
            slot_index: 9,
            pledges: ScpStatementPledges::Prepare(prep_bad_c),
        };
        assert!(!ballot.is_statement_sane(&statement_bad_c, &ctx!(&node, &quorum_set, &driver, 9)));
    }

    #[test]
    fn test_ballot_statement_sanity_confirm_constraints() {
        let node = make_node_id(1);
        let quorum_set = make_quorum_set(vec![node.clone()], 1);
        let driver = Arc::new(MockDriver::with_quorum_set(quorum_set.clone()));
        let ballot = BallotProtocol::new();

        let conf = ScpStatementConfirm {
            ballot: ScpBallot {
                counter: 0,
                value: make_value(&[1]),
            },
            n_prepared: 0,
            n_commit: 0,
            n_h: 0,
            quorum_set_hash: hash_quorum_set(&quorum_set).into(),
        };
        let statement = ScpStatement {
            node_id: node.clone(),
            slot_index: 10,
            pledges: ScpStatementPledges::Confirm(conf),
        };

        assert!(!ballot.is_statement_sane(&statement, &ctx!(&node, &quorum_set, &driver, 10)));
    }

    #[test]
    fn test_ballot_statement_sanity_externalize_constraints() {
        let node = make_node_id(1);
        let quorum_set = make_quorum_set(vec![node.clone()], 1);
        let driver = Arc::new(MockDriver::with_quorum_set(quorum_set.clone()));
        let ballot = BallotProtocol::new();

        let env = make_externalize_envelope(
            node.clone(),
            11,
            &quorum_set,
            ScpBallot {
                counter: 2,
                value: make_value(&[1]),
            },
            1,
        );

        assert!(!ballot.is_statement_sane(&env.statement, &ctx!(&node, &quorum_set, &driver, 11)));
    }

    struct ValidationDriver {
        quorum_set: ScpQuorumSet,
        invalid_value: Value,
    }

    impl ValidationDriver {
        fn new(quorum_set: ScpQuorumSet, invalid_value: Value) -> Self {
            Self {
                quorum_set,
                invalid_value,
            }
        }
    }

    impl SCPDriver for ValidationDriver {
        fn validate_value(
            &self,
            _slot_index: u64,
            value: &Value,
            _nomination: bool,
        ) -> ValidationLevel {
            if value == &self.invalid_value {
                ValidationLevel::Invalid
            } else {
                ValidationLevel::FullyValidated
            }
        }

        fn combine_candidates(&self, _slot_index: u64, _candidates: &[Value]) -> Option<Value> {
            None
        }

        fn extract_valid_value(&self, _slot_index: u64, _value: &Value) -> Option<Value> {
            None
        }

        fn emit_envelope(&self, _envelope: &ScpEnvelope) {}

        fn get_quorum_set(&self, _node_id: &NodeId) -> Option<ScpQuorumSet> {
            Some(self.quorum_set.clone())
        }

        fn nominating_value(&self, _slot_index: u64, _value: &Value) {}

        fn value_externalized(&self, _slot_index: u64, _value: &Value) {}

        fn ballot_did_prepare(&self, _slot_index: u64, _ballot: &ScpBallot) {}

        fn ballot_did_confirm(&self, _slot_index: u64, _ballot: &ScpBallot) {}

        fn compute_hash_node(
            &self,
            _slot_index: u64,
            _prev_value: &Value,
            _is_priority: bool,
            _round: u32,
            _node_id: &NodeId,
        ) -> u64 {
            0
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

        fn compute_timeout(&self, _round: u32, _is_nomination: bool) -> Duration {
            Duration::from_millis(1)
        }

        fn sign_envelope(&self, _envelope: &mut ScpEnvelope) {}

        fn verify_envelope(&self, _envelope: &ScpEnvelope) -> bool {
            true
        }
    }

    #[test]
    fn test_ballot_value_validation_rejects_invalid() {
        let node = make_node_id(1);
        let quorum_set = make_quorum_set(vec![node.clone()], 1);
        let invalid_value = make_value(&[0]);
        let driver = Arc::new(ValidationDriver::new(
            quorum_set.clone(),
            invalid_value.clone(),
        ));
        let ballot = BallotProtocol::new();

        let env = make_prepare_envelope(
            make_node_id(2),
            12,
            &quorum_set,
            ScpBallot {
                counter: 1,
                value: invalid_value,
            },
        );

        let result = ballot.validate_statement_values(&env.statement, &driver, 12);
        assert_eq!(result, ValidationLevel::Invalid);
    }

    #[test]
    fn test_ballot_rejects_unknown_quorum_set_hash() {
        let node = make_node_id(1);
        let quorum_set = make_quorum_set(vec![node.clone()], 1);
        let driver = Arc::new(MockDriver::with_quorum_set(quorum_set.clone()));
        let ballot = BallotProtocol::new();

        let other_qset = make_quorum_set(vec![make_node_id(2)], 1);
        let prep = ScpStatementPrepare {
            quorum_set_hash: hash_quorum_set(&other_qset).into(),
            ballot: ScpBallot {
                counter: 1,
                value: make_value(&[9]),
            },
            prepared: None,
            prepared_prime: None,
            n_c: 0,
            n_h: 0,
        };
        let statement = ScpStatement {
            node_id: make_node_id(3),
            slot_index: 13,
            pledges: ScpStatementPledges::Prepare(prep),
        };

        assert!(!ballot.is_statement_sane(&statement, &ctx!(&node, &quorum_set, &driver, 13)));
    }

    // ==================== Tests for new parity features ====================

    #[test]
    fn test_set_state_from_envelope_prepare() {
        let node = make_node_id(1);
        let quorum_set = make_quorum_set(vec![node.clone()], 1);
        let mut ballot = BallotProtocol::new();

        let value = make_value(&[1, 2, 3]);
        let ballot_val = ScpBallot {
            counter: 5,
            value: value.clone(),
        };
        let prepared = ScpBallot {
            counter: 3,
            value: value.clone(),
        };

        let prep = ScpStatementPrepare {
            quorum_set_hash: hash_quorum_set(&quorum_set).into(),
            ballot: ballot_val.clone(),
            prepared: Some(prepared.clone()),
            prepared_prime: None,
            n_c: 2,
            n_h: 3,
        };
        let statement = ScpStatement {
            node_id: node.clone(),
            slot_index: 1,
            pledges: ScpStatementPledges::Prepare(prep),
        };
        let envelope = ScpEnvelope {
            statement,
            signature: stellar_xdr::curr::Signature(Vec::new().try_into().unwrap_or_default()),
        };

        assert!(ballot.set_state_from_envelope(&envelope));
        assert_eq!(ballot.phase(), BallotPhase::Prepare);
        assert_eq!(ballot.current_ballot(), Some(&ballot_val));
        assert_eq!(ballot.prepared(), Some(&prepared));
        assert_eq!(ballot.commit().map(|b| b.counter), Some(2));
        assert_eq!(ballot.high_ballot().map(|b| b.counter), Some(3));
    }

    #[test]
    fn test_set_state_from_envelope_confirm() {
        let node = make_node_id(1);
        let quorum_set = make_quorum_set(vec![node.clone()], 1);
        let mut ballot = BallotProtocol::new();

        let value = make_value(&[4, 5, 6]);
        let ballot_val = ScpBallot {
            counter: 10,
            value: value.clone(),
        };

        let conf = ScpStatementConfirm {
            ballot: ballot_val.clone(),
            n_prepared: 8,
            n_commit: 5,
            n_h: 9,
            quorum_set_hash: hash_quorum_set(&quorum_set).into(),
        };
        let statement = ScpStatement {
            node_id: node.clone(),
            slot_index: 2,
            pledges: ScpStatementPledges::Confirm(conf),
        };
        let envelope = ScpEnvelope {
            statement,
            signature: stellar_xdr::curr::Signature(Vec::new().try_into().unwrap_or_default()),
        };

        assert!(ballot.set_state_from_envelope(&envelope));
        assert_eq!(ballot.phase(), BallotPhase::Confirm);
        assert_eq!(ballot.current_ballot(), Some(&ballot_val));
        assert_eq!(ballot.prepared().map(|b| b.counter), Some(8));
        assert_eq!(ballot.commit().map(|b| b.counter), Some(5));
        assert_eq!(ballot.high_ballot().map(|b| b.counter), Some(9));
    }

    #[test]
    fn test_set_state_from_envelope_externalize() {
        let node = make_node_id(1);
        let quorum_set = make_quorum_set(vec![node.clone()], 1);
        let mut ballot = BallotProtocol::new();

        let value = make_value(&[7, 8, 9]);
        let commit = ScpBallot {
            counter: 3,
            value: value.clone(),
        };

        let ext = ScpStatementExternalize {
            commit: commit.clone(),
            n_h: 5,
            commit_quorum_set_hash: hash_quorum_set(&quorum_set).into(),
        };
        let statement = ScpStatement {
            node_id: node.clone(),
            slot_index: 3,
            pledges: ScpStatementPledges::Externalize(ext),
        };
        let envelope = ScpEnvelope {
            statement,
            signature: stellar_xdr::curr::Signature(Vec::new().try_into().unwrap_or_default()),
        };

        assert!(ballot.set_state_from_envelope(&envelope));
        assert_eq!(ballot.phase(), BallotPhase::Externalize);
        assert!(ballot.is_externalized());
        assert_eq!(ballot.commit(), Some(&commit));
        assert_eq!(ballot.high_ballot().map(|b| b.counter), Some(5));
        assert_eq!(ballot.get_externalized_value(), Some(&value));
    }

    #[test]
    fn test_set_state_from_envelope_rejects_nomination() {
        let node = make_node_id(1);
        let quorum_set = make_quorum_set(vec![node.clone()], 1);
        let mut ballot = BallotProtocol::new();

        let nomination = ScpNomination {
            quorum_set_hash: hash_quorum_set(&quorum_set).into(),
            votes: vec![make_value(&[1])].try_into().unwrap(),
            accepted: vec![].try_into().unwrap(),
        };
        let statement = ScpStatement {
            node_id: node.clone(),
            slot_index: 4,
            pledges: ScpStatementPledges::Nominate(nomination),
        };
        let envelope = ScpEnvelope {
            statement,
            signature: stellar_xdr::curr::Signature(Vec::new().try_into().unwrap_or_default()),
        };

        assert!(!ballot.set_state_from_envelope(&envelope));
        assert_eq!(ballot.phase(), BallotPhase::Prepare);
        assert!(ballot.current_ballot().is_none());
    }

    #[test]
    fn test_bump_state_specific_counter() {
        let node = make_node_id(1);
        let other = make_node_id(99);
        let quorum_set = make_quorum_set(vec![node.clone(), other.clone()], 2);
        let driver = Arc::new(MockDriver::with_quorum_set(quorum_set.clone()));
        let mut ballot = BallotProtocol::new();

        let value = make_value(&[1, 2, 3]);

        // First bump to counter 1
        assert!(ballot.bump(&ctx!(&node, &quorum_set, &driver, 1), value.clone(), false));
        assert_eq!(ballot.current_ballot().map(|b| b.counter), Some(1));

        // Now bump to specific counter 5
        assert!(ballot.bump_state(&ctx!(&node, &quorum_set, &driver, 1), value.clone(), 5));
        assert_eq!(ballot.current_ballot().map(|b| b.counter), Some(5));

        // Cannot go backwards
        assert!(!ballot.bump_state(&ctx!(&node, &quorum_set, &driver, 1), value.clone(), 3));
        assert_eq!(ballot.current_ballot().map(|b| b.counter), Some(5));

        // Can go forwards
        assert!(ballot.bump_state(&ctx!(&node, &quorum_set, &driver, 1), value.clone(), 10));
        assert_eq!(ballot.current_ballot().map(|b| b.counter), Some(10));
    }

    #[test]
    fn test_bump_state_fails_when_externalized() {
        let node = make_node_id(1);
        let quorum_set = make_quorum_set(vec![node.clone()], 1);
        let driver = Arc::new(MockDriver::with_quorum_set(quorum_set.clone()));
        let mut ballot = BallotProtocol::new();

        // Externalize via set_state_from_envelope
        let value = make_value(&[1, 2, 3]);
        let commit = ScpBallot {
            counter: 3,
            value: value.clone(),
        };
        let ext = ScpStatementExternalize {
            commit: commit.clone(),
            n_h: 5,
            commit_quorum_set_hash: hash_quorum_set(&quorum_set).into(),
        };
        let statement = ScpStatement {
            node_id: node.clone(),
            slot_index: 1,
            pledges: ScpStatementPledges::Externalize(ext),
        };
        let envelope = ScpEnvelope {
            statement,
            signature: stellar_xdr::curr::Signature(Vec::new().try_into().unwrap_or_default()),
        };
        ballot.set_state_from_envelope(&envelope);

        // Cannot bump when externalized
        assert!(!ballot.bump_state(&ctx!(&node, &quorum_set, &driver, 1), value.clone(), 10));
    }

    #[test]
    fn test_abandon_ballot() {
        let node = make_node_id(1);
        let other = make_node_id(99);
        let quorum_set = make_quorum_set(vec![node.clone(), other.clone()], 2);
        let driver = Arc::new(MockDriver::with_quorum_set(quorum_set.clone()));
        let mut ballot = BallotProtocol::new();

        let value = make_value(&[1, 2, 3]);

        // Start with ballot counter 1
        assert!(ballot.bump(&ctx!(&node, &quorum_set, &driver, 1), value.clone(), false));
        assert_eq!(ballot.current_ballot().map(|b| b.counter), Some(1));

        // Abandon to counter 5
        assert!(ballot.abandon_ballot(5, &ctx!(&node, &quorum_set, &driver, 1)));
        assert_eq!(ballot.current_ballot().map(|b| b.counter), Some(5));

        // Abandon with counter 0 should auto-increment
        assert!(ballot.abandon_ballot(0, &ctx!(&node, &quorum_set, &driver, 1)));
        assert_eq!(ballot.current_ballot().map(|b| b.counter), Some(6));
    }

    #[test]
    fn test_check_invariants_valid() {
        let mut ballot = BallotProtocol::new();

        // Empty state is valid
        assert!(ballot.check_invariants().is_ok());

        // Set up valid Prepare state
        let value = make_value(&[1, 2, 3]);
        ballot.current_ballot = Some(ScpBallot {
            counter: 5,
            value: value.clone(),
        });
        ballot.prepared = Some(ScpBallot {
            counter: 3,
            value: value.clone(),
        });
        ballot.high_ballot = Some(ScpBallot {
            counter: 4,
            value: value.clone(),
        });
        ballot.commit = Some(ScpBallot {
            counter: 2,
            value: value.clone(),
        });
        ballot.phase = BallotPhase::Prepare;

        assert!(ballot.check_invariants().is_ok());
    }

    #[test]
    fn test_check_invariants_prepared_prime_must_be_less() {
        let mut ballot = BallotProtocol::new();
        let value1 = make_value(&[1]);
        let value2 = make_value(&[2]);

        ballot.prepared = Some(ScpBallot {
            counter: 3,
            value: value1.clone(),
        });
        // prepared_prime has higher counter than prepared - invalid
        ballot.prepared_prime = Some(ScpBallot {
            counter: 5,
            value: value2.clone(),
        });

        assert!(ballot.check_invariants().is_err());
    }

    #[test]
    fn test_get_local_state_formatting() {
        let mut ballot = BallotProtocol::new();
        let value = make_value(&[0xab, 0xcd, 0xef, 0x12]);

        ballot.current_ballot = Some(ScpBallot {
            counter: 5,
            value: value.clone(),
        });
        ballot.prepared = Some(ScpBallot {
            counter: 3,
            value: value.clone(),
        });
        ballot.phase = BallotPhase::Prepare;

        let state = ballot.get_local_state();
        assert!(state.contains("phase=Prepare"));
        assert!(state.contains("b=(5,"));
        assert!(state.contains("p=(3,"));
        assert!(state.contains("heard_from_quorum=false"));
    }

    #[test]
    fn test_get_working_ballot_prepare() {
        let node_id = make_node_id(1);
        let value = make_value(&[1, 2, 3]);
        let ballot = ScpBallot {
            counter: 5,
            value: value.clone(),
        };
        let quorum_set = make_quorum_set(vec![node_id.clone()], 1);
        let env = make_prepare_envelope(node_id, 1, &quorum_set, ballot.clone());

        let working = get_working_ballot(&env.statement);
        assert!(working.is_some());
        let working = working.unwrap();
        assert_eq!(working.counter, 5);
        assert_eq!(working.value, value);
    }

    #[test]
    fn test_get_working_ballot_confirm() {
        let node_id = make_node_id(1);
        let value = make_value(&[1, 2, 3]);
        let ballot = ScpBallot {
            counter: 5,
            value: value.clone(),
        };
        let quorum_set = make_quorum_set(vec![node_id.clone()], 1);
        let env =
            make_confirm_envelope_with_counters(node_id, 1, &quorum_set, ballot.clone(), 3, 2, 4);

        // For CONFIRM, working ballot uses n_commit as counter
        let working = get_working_ballot(&env.statement);
        assert!(working.is_some());
        let working = working.unwrap();
        assert_eq!(working.counter, 2); // n_commit
        assert_eq!(working.value, value);
    }

    #[test]
    fn test_get_working_ballot_externalize() {
        let node_id = make_node_id(1);
        let value = make_value(&[1, 2, 3]);
        let commit = ScpBallot {
            counter: 3,
            value: value.clone(),
        };
        let ext = ScpStatementExternalize {
            commit: commit.clone(),
            n_h: 5,
            commit_quorum_set_hash: [0u8; 32].into(),
        };
        let statement = ScpStatement {
            node_id,
            slot_index: 1,
            pledges: ScpStatementPledges::Externalize(ext),
        };

        // For EXTERNALIZE, working ballot uses u32::MAX as counter
        let working = get_working_ballot(&statement);
        assert!(working.is_some());
        let working = working.unwrap();
        assert_eq!(working.counter, u32::MAX);
        assert_eq!(working.value, value);
    }

    #[test]
    fn test_get_working_ballot_nominate() {
        let node_id = make_node_id(1);
        let nom = ScpNomination {
            quorum_set_hash: [0u8; 32].into(),
            votes: vec![make_value(&[1])].try_into().unwrap(),
            accepted: vec![].try_into().unwrap(),
        };
        let statement = ScpStatement {
            node_id,
            slot_index: 1,
            pledges: ScpStatementPledges::Nominate(nom),
        };

        // Nomination statements don't have a working ballot
        let working = get_working_ballot(&statement);
        assert!(working.is_none());
    }

    // =========================================================================
    // Ballot Protocol Parity Tests (Phase 3)
    // =========================================================================

    /// Mock driver that tracks timer operations, externalized values, etc.
    struct BallotParityDriver {
        quorum_set: ScpQuorumSet,
        emit_count: AtomicU32,
        timer_setups: std::sync::Mutex<Vec<(u64, crate::driver::SCPTimerType)>>,
        timer_stops: std::sync::Mutex<Vec<(u64, crate::driver::SCPTimerType)>>,
        externalized_values: std::sync::Mutex<Vec<(u64, Value)>>,
    }

    impl BallotParityDriver {
        fn new(quorum_set: ScpQuorumSet) -> Self {
            Self {
                quorum_set,
                emit_count: AtomicU32::new(0),
                timer_setups: std::sync::Mutex::new(Vec::new()),
                timer_stops: std::sync::Mutex::new(Vec::new()),
                externalized_values: std::sync::Mutex::new(Vec::new()),
            }
        }

        fn get_timer_setups(&self) -> Vec<(u64, crate::driver::SCPTimerType)> {
            self.timer_setups.lock().unwrap().clone()
        }

        fn get_timer_stops(&self) -> Vec<(u64, crate::driver::SCPTimerType)> {
            self.timer_stops.lock().unwrap().clone()
        }

        fn get_externalized_values(&self) -> Vec<(u64, Value)> {
            self.externalized_values.lock().unwrap().clone()
        }
    }

    impl SCPDriver for BallotParityDriver {
        fn validate_value(
            &self,
            _slot_index: u64,
            _value: &Value,
            _nomination: bool,
        ) -> ValidationLevel {
            ValidationLevel::FullyValidated
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
            Some(self.quorum_set.clone())
        }

        fn nominating_value(&self, _slot_index: u64, _value: &Value) {}

        fn value_externalized(&self, slot_index: u64, value: &Value) {
            self.externalized_values
                .lock()
                .unwrap()
                .push((slot_index, value.clone()));
        }

        fn ballot_did_prepare(&self, _slot_index: u64, _ballot: &ScpBallot) {}
        fn ballot_did_confirm(&self, _slot_index: u64, _ballot: &ScpBallot) {}
        fn ballot_did_hear_from_quorum(&self, _slot_index: u64, _ballot: &ScpBallot) {}

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
            value.iter().map(|b| *b as u64).sum()
        }

        fn compute_timeout(&self, _round: u32, _is_nomination: bool) -> Duration {
            Duration::from_millis(1000)
        }

        fn sign_envelope(&self, _envelope: &mut ScpEnvelope) {}

        fn verify_envelope(&self, _envelope: &ScpEnvelope) -> bool {
            true
        }

        fn setup_timer(
            &self,
            slot_index: u64,
            timer_type: crate::driver::SCPTimerType,
            _timeout: Duration,
        ) {
            self.timer_setups
                .lock()
                .unwrap()
                .push((slot_index, timer_type));
        }

        fn stop_timer(&self, slot_index: u64, timer_type: crate::driver::SCPTimerType) {
            self.timer_stops
                .lock()
                .unwrap()
                .push((slot_index, timer_type));
        }
    }

    /// B-bump parity test: bump_to_ballot resets high/commit when value is incompatible.
    ///
    /// stellar-core invariant: h.value == b.value. When bumping to a ballot with an
    /// incompatible value, high_ballot and commit must be cleared.
    #[test]
    fn test_bump_to_ballot_resets_incompatible_high_commit() {
        let mut bp = BallotProtocol::new();
        let value_a = make_value(&[1]);
        let value_b = make_value(&[2]);

        // Set up state: current ballot with value_a, high and commit set
        bp.current_ballot = Some(ScpBallot {
            counter: 1,
            value: value_a.clone(),
        });
        bp.high_ballot = Some(ScpBallot {
            counter: 1,
            value: value_a.clone(),
        });
        bp.commit = Some(ScpBallot {
            counter: 1,
            value: value_a.clone(),
        });

        // Bump to a ballot with incompatible value
        let new_ballot = ScpBallot {
            counter: 2,
            value: value_b.clone(),
        };
        assert!(bp.bump_to_ballot(&new_ballot, false));

        // h and c should be cleared because value_b != value_a
        assert!(
            bp.high_ballot.is_none(),
            "high_ballot should be cleared on incompatible bump"
        );
        assert!(
            bp.commit.is_none(),
            "commit should be cleared on incompatible bump"
        );
        assert_eq!(bp.current_ballot.as_ref().unwrap().value, value_b);
    }

    /// B-bump parity test: bump_to_ballot preserves high/commit when value is compatible.
    #[test]
    fn test_bump_to_ballot_preserves_compatible_high_commit() {
        let mut bp = BallotProtocol::new();
        let value_a = make_value(&[1]);

        // Set up state with high and commit
        bp.current_ballot = Some(ScpBallot {
            counter: 1,
            value: value_a.clone(),
        });
        bp.high_ballot = Some(ScpBallot {
            counter: 1,
            value: value_a.clone(),
        });
        bp.commit = Some(ScpBallot {
            counter: 1,
            value: value_a.clone(),
        });

        // Bump to higher counter with same value (compatible)
        let new_ballot = ScpBallot {
            counter: 2,
            value: value_a.clone(),
        };
        assert!(bp.bump_to_ballot(&new_ballot, false));

        // h and c should be preserved because same value
        assert!(
            bp.high_ballot.is_some(),
            "high_ballot should be preserved on compatible bump"
        );
        assert!(
            bp.commit.is_some(),
            "commit should be preserved on compatible bump"
        );
    }

    /// B-bump parity test: heard_from_quorum only resets when counter changes.
    #[test]
    fn test_bump_to_ballot_heard_from_quorum_counter_change() {
        let mut bp = BallotProtocol::new();
        let value_a = make_value(&[1]);

        // Set initial ballot and heard_from_quorum
        bp.current_ballot = Some(ScpBallot {
            counter: 1,
            value: value_a.clone(),
        });
        bp.heard_from_quorum = true;

        // Bump with same counter (different value) - should NOT reset heard_from_quorum
        let same_counter_ballot = ScpBallot {
            counter: 1,
            value: make_value(&[2]),
        };
        bp.bump_to_ballot(&same_counter_ballot, false);
        assert!(
            bp.heard_from_quorum,
            "heard_from_quorum should not reset when counter stays the same"
        );

        // Bump with different counter - SHOULD reset heard_from_quorum
        let new_counter_ballot = ScpBallot {
            counter: 2,
            value: make_value(&[2]),
        };
        bp.bump_to_ballot(&new_counter_ballot, false);
        assert!(
            !bp.heard_from_quorum,
            "heard_from_quorum should reset when counter changes"
        );
    }

    /// B-override parity test: bump_state uses value_override when set.
    ///
    /// stellar-core bumpState checks mValueOverride and uses that instead of the
    /// passed-in value when it's set (e.g., after confirming prepared).
    #[test]
    fn test_bump_state_uses_value_override() {
        let node = make_node_id(1);
        let other = make_node_id(99);
        let quorum_set = make_quorum_set(vec![node.clone(), other.clone()], 2);
        let driver = Arc::new(BallotParityDriver::new(quorum_set.clone()));
        let mut bp = BallotProtocol::new();

        let value_a = make_value(&[1]);
        let value_override = make_value(&[99]);

        // Start with a ballot
        assert!(bp.bump(
            &ctx!(&node, &quorum_set, &driver, 1),
            value_a.clone(),
            false
        ));
        assert_eq!(bp.current_ballot().unwrap().counter, 1);
        assert_eq!(bp.current_ballot().unwrap().value, value_a);

        // Set value_override (as would happen during confirm phase)
        bp.value_override = Some(value_override.clone());

        // Now bump_state with value_a - should use value_override instead
        assert!(bp.bump_state(&ctx!(&node, &quorum_set, &driver, 1), value_a.clone(), 2));
        assert_eq!(
            bp.current_ballot().unwrap().value,
            value_override,
            "bump_state should use value_override when set"
        );
    }

    /// B-override parity test: bump_state goes through update_current_value.
    ///
    /// stellar-core bumpState(value, n) calls updateCurrentValue which checks phase
    /// and commit compatibility. Verify that bump_state rejects incompatible
    /// commit values.
    #[test]
    fn test_bump_state_rejects_incompatible_with_commit() {
        let node = make_node_id(1);
        let quorum_set = make_quorum_set(vec![node.clone()], 1);
        let driver = Arc::new(BallotParityDriver::new(quorum_set.clone()));
        let mut bp = BallotProtocol::new();

        let value_a = make_value(&[1]);
        let value_b = make_value(&[2]);

        // Start with a ballot
        assert!(bp.bump(
            &ctx!(&node, &quorum_set, &driver, 1),
            value_a.clone(),
            false
        ));

        // Set a commit ballot with value_a
        bp.commit = Some(ScpBallot {
            counter: 1,
            value: value_a.clone(),
        });

        // bump_state with incompatible value_b should be rejected
        assert!(
            !bp.bump_state(&ctx!(&node, &quorum_set, &driver, 1), value_b.clone(), 5),
            "bump_state should reject value incompatible with commit"
        );
    }

    /// B-abandon parity test: abandon_ballot uses composite candidate over current ballot.
    ///
    /// stellar-core abandonBallot first checks mSlot.getLatestCompositeCandidate(),
    /// then falls back to mCurrentBallot->value.
    #[test]
    fn test_abandon_ballot_uses_composite_candidate() {
        let node = make_node_id(1);
        let quorum_set = make_quorum_set(vec![node.clone()], 1);
        let driver = Arc::new(BallotParityDriver::new(quorum_set.clone()));
        let mut bp = BallotProtocol::new();
        let value_current = make_value(&[1]);
        let value_composite = make_value(&[99]);

        // Set up current ballot
        bp.current_ballot = Some(ScpBallot {
            counter: 1,
            value: value_current.clone(),
        });

        // Set composite candidate (simulating nomination output)
        bp.set_composite_candidate(Some(value_composite.clone()));

        // Abandon should use composite candidate value
        assert!(bp.abandon_ballot(0, &ctx!(&node, &quorum_set, &driver, 1)));
        assert_eq!(
            bp.current_ballot.as_ref().unwrap().value,
            value_composite,
            "abandon_ballot should prefer composite candidate value"
        );
        assert_eq!(bp.current_ballot.as_ref().unwrap().counter, 2);
    }

    /// B-abandon parity test: abandon_ballot falls back to current ballot value.
    #[test]
    fn test_abandon_ballot_falls_back_to_current_value() {
        let node = make_node_id(1);
        let quorum_set = make_quorum_set(vec![node.clone()], 1);
        let driver = Arc::new(BallotParityDriver::new(quorum_set.clone()));
        let mut bp = BallotProtocol::new();
        let value_current = make_value(&[1]);

        // Set up current ballot, no composite candidate
        bp.current_ballot = Some(ScpBallot {
            counter: 1,
            value: value_current.clone(),
        });

        // No composite candidate set
        assert!(bp.abandon_ballot(0, &ctx!(&node, &quorum_set, &driver, 1)));
        assert_eq!(
            bp.current_ballot.as_ref().unwrap().value,
            value_current,
            "abandon_ballot should fall back to current ballot value"
        );
        assert_eq!(bp.current_ballot.as_ref().unwrap().counter, 2);
    }

    /// B-abandon parity test: abandon_ballot with specific counter.
    #[test]
    fn test_abandon_ballot_with_specific_counter() {
        let node = make_node_id(1);
        let quorum_set = make_quorum_set(vec![node.clone()], 1);
        let driver = Arc::new(BallotParityDriver::new(quorum_set.clone()));
        let mut bp = BallotProtocol::new();
        let value_current = make_value(&[1]);

        bp.current_ballot = Some(ScpBallot {
            counter: 1,
            value: value_current.clone(),
        });

        // Abandon with specific counter
        assert!(bp.abandon_ballot(10, &ctx!(&node, &quorum_set, &driver, 1)));
        assert_eq!(
            bp.current_ballot.as_ref().unwrap().counter,
            10,
            "abandon_ballot should use specified counter"
        );
    }

    /// B-stopnom parity test: set_confirm_commit signals nomination stop.
    ///
    /// stellar-core setConfirmCommit calls mSlot.stopNomination() between
    /// emitCurrentStateStatement() and valueExternalized().
    #[test]
    fn test_set_confirm_commit_signals_stop_nomination() {
        let node = make_node_id(1);
        let quorum_set = make_quorum_set(vec![node.clone()], 1);
        let driver = Arc::new(BallotParityDriver::new(quorum_set.clone()));
        let mut bp = BallotProtocol::new();

        let value = make_value(&[1]);

        // Must have a current ballot first
        bp.current_ballot = Some(ScpBallot {
            counter: 1,
            value: value.clone(),
        });

        let c = ScpBallot {
            counter: 1,
            value: value.clone(),
        };
        let h = ScpBallot {
            counter: 1,
            value: value.clone(),
        };

        bp.set_confirm_commit(c, h, &ctx!(&node, &quorum_set, &driver, 1));

        // Should signal that nomination needs to stop
        assert!(
            bp.needs_stop_nomination,
            "set_confirm_commit should signal nomination stop"
        );

        // And take_needs_stop_nomination should clear the flag
        assert!(bp.take_needs_stop_nomination());
        assert!(!bp.take_needs_stop_nomination());
    }

    /// B-stopnom parity test: set_confirm_commit uses commit value for externalize.
    ///
    /// stellar-core uses mCommit->getBallot().value (c.value) for valueExternalized,
    /// not h.value.
    #[test]
    fn test_set_confirm_commit_externalizes_commit_value() {
        let node = make_node_id(1);
        let quorum_set = make_quorum_set(vec![node.clone()], 1);
        let driver = Arc::new(BallotParityDriver::new(quorum_set.clone()));
        let mut bp = BallotProtocol::new();

        let value = make_value(&[42]);

        bp.current_ballot = Some(ScpBallot {
            counter: 1,
            value: value.clone(),
        });

        let c = ScpBallot {
            counter: 1,
            value: value.clone(),
        };
        let h = ScpBallot {
            counter: 3,
            value: value.clone(),
        };

        bp.set_confirm_commit(c.clone(), h, &ctx!(&node, &quorum_set, &driver, 1));

        let externalized = driver.get_externalized_values();
        assert_eq!(externalized.len(), 1);
        assert_eq!(
            externalized[0].1, c.value,
            "valueExternalized should be called with c.value"
        );
    }

    /// B-timer parity test: check_heard_from_quorum starts ballot timer on transition.
    ///
    /// stellar-core starts the ballot protocol timer when heard_from_quorum transitions
    /// from false to true and phase is not Externalize.
    #[test]
    fn test_check_heard_from_quorum_starts_timer() {
        let local = make_node_id(1);
        let remote = make_node_id(2);
        let quorum_set = make_quorum_set(vec![local.clone(), remote.clone()], 2);
        let driver = Arc::new(BallotParityDriver::new(quorum_set.clone()));
        let mut bp = BallotProtocol::new();

        let value = make_value(&[1]);
        let ballot = ScpBallot {
            counter: 1,
            value: value.clone(),
        };

        // Start with a ballot
        bp.current_ballot = Some(ballot.clone());
        bp.heard_from_quorum = false;

        // Add envelopes from both nodes (need quorum)
        let env_local = make_prepare_envelope(local.clone(), 1, &quorum_set, ballot.clone());
        let env_remote = make_prepare_envelope(remote.clone(), 1, &quorum_set, ballot.clone());
        bp.latest_envelopes.insert(local.clone(), env_local);
        bp.latest_envelopes.insert(remote.clone(), env_remote);

        // Check heard from quorum
        bp.check_heard_from_quorum(&ctx!(&local, &quorum_set, &driver, 1));

        assert!(bp.heard_from_quorum);

        // Ballot timer should have been set up
        let setups = driver.get_timer_setups();
        assert!(
            setups
                .iter()
                .any(|(_, t)| *t == crate::driver::SCPTimerType::Ballot),
            "Ballot timer should be started when heard_from_quorum transitions to true"
        );
    }

    /// B-timer parity test: check_heard_from_quorum stops timer when not quorum.
    ///
    /// stellar-core stops the ballot timer when heard_from_quorum is false.
    #[test]
    fn test_check_heard_from_quorum_stops_timer_no_quorum() {
        let local = make_node_id(1);
        let remote = make_node_id(2);
        let quorum_set = make_quorum_set(vec![local.clone(), remote.clone()], 2);
        let driver = Arc::new(BallotParityDriver::new(quorum_set.clone()));
        let mut bp = BallotProtocol::new();

        let value = make_value(&[1]);
        let ballot = ScpBallot {
            counter: 1,
            value: value.clone(),
        };

        bp.current_ballot = Some(ballot.clone());
        bp.heard_from_quorum = true; // Was previously true

        // Only local envelope (not a quorum for threshold=2)
        let env_local = make_prepare_envelope(local.clone(), 1, &quorum_set, ballot.clone());
        bp.latest_envelopes.insert(local.clone(), env_local);

        bp.check_heard_from_quorum(&ctx!(&local, &quorum_set, &driver, 1));

        assert!(!bp.heard_from_quorum);

        // Timer should have been stopped
        let stops = driver.get_timer_stops();
        assert!(
            stops
                .iter()
                .any(|(_, t)| *t == crate::driver::SCPTimerType::Ballot),
            "Ballot timer should be stopped when quorum is lost"
        );
    }

    /// B-timer parity test: check_heard_from_quorum stops timer in Externalize phase.
    ///
    /// stellar-core stops the ballot timer when heard_from_quorum is true but phase is Externalize.
    #[test]
    fn test_check_heard_from_quorum_stops_timer_externalize() {
        let local = make_node_id(1);
        let remote = make_node_id(2);
        let quorum_set = make_quorum_set(vec![local.clone(), remote.clone()], 2);
        let driver = Arc::new(BallotParityDriver::new(quorum_set.clone()));
        let mut bp = BallotProtocol::new();

        let value = make_value(&[1]);
        let ballot = ScpBallot {
            counter: 1,
            value: value.clone(),
        };

        bp.current_ballot = Some(ballot.clone());
        bp.phase = BallotPhase::Externalize;
        bp.heard_from_quorum = false; // Will transition to true

        // Add quorum of envelopes
        let env_local = make_prepare_envelope(local.clone(), 1, &quorum_set, ballot.clone());
        let env_remote = make_prepare_envelope(remote.clone(), 1, &quorum_set, ballot.clone());
        bp.latest_envelopes.insert(local.clone(), env_local);
        bp.latest_envelopes.insert(remote.clone(), env_remote);

        bp.check_heard_from_quorum(&ctx!(&local, &quorum_set, &driver, 1));

        assert!(bp.heard_from_quorum);

        // Timer should NOT have been started (phase is Externalize)
        let setups = driver.get_timer_setups();
        assert!(
            !setups
                .iter()
                .any(|(_, t)| *t == crate::driver::SCPTimerType::Ballot),
            "Ballot timer should NOT be started in Externalize phase"
        );

        // Timer should have been stopped
        let stops = driver.get_timer_stops();
        assert!(
            stops
                .iter()
                .any(|(_, t)| *t == crate::driver::SCPTimerType::Ballot),
            "Ballot timer should be stopped in Externalize phase"
        );
    }

    /// B-override parity test: update_current_value checks phase.
    #[test]
    fn test_update_current_value_rejects_externalize_phase() {
        let mut bp = BallotProtocol::new();
        let value = make_value(&[1]);

        bp.phase = BallotPhase::Externalize;

        let ballot = ScpBallot {
            counter: 1,
            value: value.clone(),
        };

        assert!(
            !bp.update_current_value(&ballot),
            "update_current_value should reject in Externalize phase"
        );
    }

    /// B-override parity test: update_current_value rejects commit-incompatible ballots.
    #[test]
    fn test_update_current_value_rejects_commit_incompatible() {
        let mut bp = BallotProtocol::new();
        let value_a = make_value(&[1]);
        let value_b = make_value(&[2]);

        bp.current_ballot = Some(ScpBallot {
            counter: 1,
            value: value_a.clone(),
        });
        bp.commit = Some(ScpBallot {
            counter: 1,
            value: value_a.clone(),
        });

        let incompatible_ballot = ScpBallot {
            counter: 2,
            value: value_b,
        };

        assert!(
            !bp.update_current_value(&incompatible_ballot),
            "update_current_value should reject ballot incompatible with commit"
        );
    }

    /// B-bump parity test: bump delegates to bump_state.
    ///
    /// Since bump now delegates to bump_state, it inherits value_override
    /// checking and update_current_value logic.
    #[test]
    fn test_bump_delegates_to_bump_state() {
        let node = make_node_id(1);
        let other = make_node_id(99);
        let quorum_set = make_quorum_set(vec![node.clone(), other.clone()], 2);
        let driver = Arc::new(BallotParityDriver::new(quorum_set.clone()));
        let mut bp = BallotProtocol::new();

        let value_a = make_value(&[1]);
        let value_override = make_value(&[99]);

        // Start with a ballot
        assert!(bp.bump(
            &ctx!(&node, &quorum_set, &driver, 1),
            value_a.clone(),
            false
        ));
        assert_eq!(bp.current_ballot().unwrap().value, value_a);

        // Set value_override
        bp.value_override = Some(value_override.clone());

        // Force bump should use value_override
        assert!(bp.bump(&ctx!(&node, &quorum_set, &driver, 1), value_a.clone(), true));
        assert_eq!(
            bp.current_ballot().unwrap().value,
            value_override,
            "bump with force should use value_override"
        );
    }

    // ── SCP_SPEC §9.13: set_state_from_envelope sets last_envelope_emit ──

    #[test]
    fn test_set_state_from_envelope_prepare_sets_last_envelope_emit() {
        let node = make_node_id(1);
        let quorum_set = make_quorum_set(vec![node.clone()], 1);
        let mut bp = BallotProtocol::new();

        assert!(bp.last_envelope_emit.is_none(), "should start as None");

        let value = make_value(&[1, 2, 3]);
        let envelope = make_prepare_envelope_with_counters(
            node.clone(),
            1,
            &quorum_set,
            ScpBallot {
                counter: 5,
                value: value.clone(),
            },
            PrepareEnvelopeCounters {
                prepared: Some(ScpBallot {
                    counter: 3,
                    value: value.clone(),
                }),
                prepared_prime: None,
                n_c: 2,
                n_h: 3,
            },
        );

        assert!(bp.set_state_from_envelope(&envelope));
        assert_eq!(
            bp.last_envelope_emit.as_ref(),
            Some(&envelope),
            "Prepare: last_envelope_emit must equal the restored envelope"
        );
        assert_eq!(
            bp.last_envelope.as_ref(),
            bp.last_envelope_emit.as_ref(),
            "last_envelope and last_envelope_emit must match after restore"
        );
    }

    #[test]
    fn test_set_state_from_envelope_confirm_sets_last_envelope_emit() {
        let node = make_node_id(1);
        let quorum_set = make_quorum_set(vec![node.clone()], 1);
        let mut bp = BallotProtocol::new();

        let value = make_value(&[4, 5, 6]);
        let envelope = make_confirm_envelope_with_counters(
            node.clone(),
            2,
            &quorum_set,
            ScpBallot {
                counter: 10,
                value: value.clone(),
            },
            8,
            5,
            9,
        );

        assert!(bp.set_state_from_envelope(&envelope));
        assert_eq!(
            bp.last_envelope_emit.as_ref(),
            Some(&envelope),
            "Confirm: last_envelope_emit must equal the restored envelope"
        );
    }

    #[test]
    fn test_set_state_from_envelope_externalize_sets_last_envelope_emit() {
        let node = make_node_id(1);
        let quorum_set = make_quorum_set(vec![node.clone()], 1);
        let mut bp = BallotProtocol::new();

        let value = make_value(&[7, 8, 9]);
        let envelope = make_externalize_envelope(
            node.clone(),
            3,
            &quorum_set,
            ScpBallot {
                counter: 3,
                value: value.clone(),
            },
            5,
        );

        assert!(bp.set_state_from_envelope(&envelope));
        assert_eq!(
            bp.last_envelope_emit.as_ref(),
            Some(&envelope),
            "Externalize: last_envelope_emit must equal the restored envelope"
        );
    }

    #[test]
    fn test_set_state_from_envelope_nomination_does_not_set_last_envelope_emit() {
        let node = make_node_id(1);
        let quorum_set = make_quorum_set(vec![node.clone()], 1);
        let mut bp = BallotProtocol::new();

        let envelope = make_nomination_envelope(node.clone(), 4, &quorum_set);

        assert!(!bp.set_state_from_envelope(&envelope));
        assert!(
            bp.last_envelope_emit.is_none(),
            "Nomination: last_envelope_emit must remain None"
        );
    }

    #[test]
    fn test_set_state_from_envelope_prevents_reemission() {
        // After restoring state, send_latest_envelope should NOT re-emit
        // because last_envelope_emit already matches last_envelope.
        let node = make_node_id(1);
        let quorum_set = make_quorum_set(vec![node.clone()], 1);
        let driver = Arc::new(BallotParityDriver::new(quorum_set.clone()));
        let mut bp = BallotProtocol::new();
        bp.fully_validated = true;

        let value = make_value(&[1, 2, 3]);
        let envelope = make_prepare_envelope(
            node.clone(),
            1,
            &quorum_set,
            ScpBallot { counter: 1, value },
        );

        bp.set_state_from_envelope(&envelope);

        let emit_before = driver.emit_count.load(Ordering::SeqCst);
        bp.send_latest_envelope(&driver);
        let emit_after = driver.emit_count.load(Ordering::SeqCst);

        assert_eq!(
            emit_before, emit_after,
            "send_latest_envelope must NOT re-emit when last_envelope_emit already set"
        );
    }

    // =========================================================================
    // Issues #1795 / #1798: MaybeValidDeferred must NOT clear fully_validated
    // =========================================================================
    //
    // Root cause (both issues): henyey's SCP fast-path forwards a peer
    // envelope to the ballot protocol earlier than stellar-core's
    // PendingEnvelopes / processSCPQueue would, producing a "maybe
    // valid" result the validator cannot recover from:
    //
    //   - #1795: EXTERNALIZE forwarded before tx_set is fetched.
    //     `validate_value_against_local_state` returned
    //     `ValueValidation::MaybeValid` on the `is_current_ledger &&
    //     !has_tx_set` branch.
    //
    //   - #1798: drain_and_process_pending(N+1) runs synchronously on
    //     SCP externalize of slot N, ahead of ledger apply; a peer
    //     envelope for slot N+1 drains with `lcl_seq < N` but
    //     `tracking_index == slot_index == N+1`. In
    //     `validate_past_or_future_value`, the
    //     `tracking_index == slot_index` branch returned
    //     `ValueValidation::MaybeValid`.
    //
    // Either MaybeValid cleared Slot::fully_validated, suppressing
    // local EXTERNALIZE emission forever (stellar-core parity:
    // fully_validated is never restored once cleared). The fix is a
    // shared `MaybeValidDeferred` level that signals "we couldn't
    // fully validate because of a henyey-specific fast-path
    // divergence" without triggering the ballot-layer clear.

    /// Driver that returns a configurable ValidationLevel.
    struct ConfigurableValidationDriver {
        quorum_set: ScpQuorumSet,
        level: ValidationLevel,
    }

    impl ConfigurableValidationDriver {
        fn new(quorum_set: ScpQuorumSet, level: ValidationLevel) -> Self {
            Self { quorum_set, level }
        }
    }

    impl SCPDriver for ConfigurableValidationDriver {
        fn validate_value(
            &self,
            _slot_index: u64,
            _value: &Value,
            _nomination: bool,
        ) -> ValidationLevel {
            self.level
        }

        fn combine_candidates(&self, _slot_index: u64, _candidates: &[Value]) -> Option<Value> {
            None
        }

        fn extract_valid_value(&self, _slot_index: u64, value: &Value) -> Option<Value> {
            Some(value.clone())
        }

        fn emit_envelope(&self, _envelope: &ScpEnvelope) {}

        fn get_quorum_set(&self, _node_id: &NodeId) -> Option<ScpQuorumSet> {
            Some(self.quorum_set.clone())
        }

        fn nominating_value(&self, _slot_index: u64, _value: &Value) {}
        fn value_externalized(&self, _slot_index: u64, _value: &Value) {}
        fn ballot_did_prepare(&self, _slot_index: u64, _ballot: &ScpBallot) {}
        fn ballot_did_confirm(&self, _slot_index: u64, _ballot: &ScpBallot) {}

        fn compute_hash_node(
            &self,
            _slot_index: u64,
            _prev_value: &Value,
            _is_priority: bool,
            _round: u32,
            _node_id: &NodeId,
        ) -> u64 {
            0
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

        fn compute_timeout(&self, _round: u32, _is_nomination: bool) -> Duration {
            Duration::from_millis(1)
        }

        fn sign_envelope(&self, _envelope: &mut ScpEnvelope) {}

        fn verify_envelope(&self, _envelope: &ScpEnvelope) -> bool {
            true
        }
    }

    /// Regression test for issue #2061: `MaybeValidDeferred` MUST
    /// clear the ballot protocol's `fully_validated` flag (matching
    /// `MaybeValid` behavior).  Emission is deferred until the herder
    /// restores `fully_validated` after the deferred condition resolves.
    #[test]
    fn test_issue_1795_maybe_valid_deferred_clears_fully_validated_tx_set_pending() {
        let node = make_node_id(1);
        let quorum_set = make_quorum_set(vec![node.clone()], 1);
        let driver = Arc::new(ConfigurableValidationDriver::new(
            quorum_set.clone(),
            ValidationLevel::MaybeValidDeferred,
        ));
        let mut bp = BallotProtocol::new();
        bp.fully_validated = true;

        let value = make_value(&[1, 2, 3]);
        let envelope = make_prepare_envelope(
            make_node_id(2),
            42,
            &quorum_set,
            ScpBallot { counter: 1, value },
        );

        let ctx = SlotContext {
            local_node_id: &node,
            local_quorum_set: &quorum_set,
            driver: &driver,
            slot_index: 42,
        };
        bp.process_envelope(&envelope, &ctx, ValidationLevel::MaybeValidDeferred);

        assert!(
            !bp.fully_validated,
            "MaybeValidDeferred must clear fully_validated — emission is deferred \
             until the herder restores it (issue #2061)"
        );
    }

    /// Regression test for issue #2061: `MaybeValidDeferred` MUST
    /// clear the ballot protocol's `fully_validated` flag on the
    /// future-slot trigger (apply lagging SCP), matching `MaybeValid`.
    #[test]
    fn test_issue_1798_maybe_valid_deferred_clears_fully_validated_future_slot() {
        let node = make_node_id(1);
        let quorum_set = make_quorum_set(vec![node.clone()], 1);
        let driver = Arc::new(ConfigurableValidationDriver::new(
            quorum_set.clone(),
            ValidationLevel::MaybeValidDeferred,
        ));
        let mut bp = BallotProtocol::new();
        bp.fully_validated = true;

        let value = make_value(&[4, 5, 6]);
        let envelope = make_prepare_envelope(
            make_node_id(2),
            100,
            &quorum_set,
            ScpBallot {
                counter: 1,
                value: value.clone(),
            },
        );

        let ctx = SlotContext {
            local_node_id: &node,
            local_quorum_set: &quorum_set,
            driver: &driver,
            slot_index: 100,
        };
        bp.process_envelope(&envelope, &ctx, ValidationLevel::MaybeValidDeferred);

        assert!(
            !bp.fully_validated,
            "MaybeValidDeferred must clear fully_validated on the \
             future-slot trigger (issue #2061)"
        );
    }

    /// Regression guard in the opposite direction: a genuine `MaybeValid`
    /// (no tracking, past-slot `tracking_index > slot_index`, stale
    /// close-time, etc.) MUST still clear `fully_validated`, matching
    /// stellar-core `BallotProtocol.cpp:208-211`. This pins the
    /// stellar-core parity so the #1795 / #1798 fixes do not
    /// accidentally regress it.
    #[test]
    fn test_maybe_valid_still_clears_fully_validated() {
        let node = make_node_id(1);
        let quorum_set = make_quorum_set(vec![node.clone()], 1);
        let driver = Arc::new(ConfigurableValidationDriver::new(
            quorum_set.clone(),
            ValidationLevel::MaybeValid,
        ));
        let mut bp = BallotProtocol::new();
        bp.fully_validated = true;

        let value = make_value(&[1, 2, 3]);
        let envelope = make_prepare_envelope(
            make_node_id(2),
            42,
            &quorum_set,
            ScpBallot { counter: 1, value },
        );

        let ctx = SlotContext {
            local_node_id: &node,
            local_quorum_set: &quorum_set,
            driver: &driver,
            slot_index: 42,
        };
        bp.process_envelope(&envelope, &ctx, ValidationLevel::MaybeValid);

        assert!(
            !bp.fully_validated,
            "MaybeValid must clear fully_validated \
             (stellar-core parity, BallotProtocol.cpp:208-211)"
        );
    }

    /// Unit test for `ValidationLevel::clears_fully_validated`: the gate
    /// used by both the slot and ballot protocol to decide whether to
    /// clear `fully_validated`.  `MaybeValidDeferred` now clears —
    /// matching `MaybeValid`'s behavior — because the SCP layer
    /// restoration mechanism ensures emission resumes after deferred
    /// conditions resolve (issue #2061).
    #[test]
    fn test_validation_level_clears_fully_validated_helper() {
        assert!(!ValidationLevel::Invalid.clears_fully_validated());
        assert!(ValidationLevel::MaybeValid.clears_fully_validated());
        assert!(ValidationLevel::MaybeValidDeferred.clears_fully_validated());
        assert!(!ValidationLevel::FullyValidated.clears_fully_validated());
    }

    /// Unit test for `min_validation_level`:
    /// `Invalid < MaybeValid < MaybeValidDeferred < FullyValidated`.
    #[test]
    fn test_min_validation_level_ordering() {
        use crate::ballot::statements::min_validation_level;

        // Invalid dominates everything.
        assert_eq!(
            min_validation_level(ValidationLevel::Invalid, ValidationLevel::FullyValidated),
            ValidationLevel::Invalid
        );
        assert_eq!(
            min_validation_level(
                ValidationLevel::Invalid,
                ValidationLevel::MaybeValidDeferred
            ),
            ValidationLevel::Invalid
        );

        // MaybeValid dominates MaybeValidDeferred — critical for
        // issues #1795 / #1798: if one sub-value is "properly"
        // MaybeValid, the whole statement must still clear
        // fully_validated.
        assert_eq!(
            min_validation_level(
                ValidationLevel::MaybeValid,
                ValidationLevel::MaybeValidDeferred
            ),
            ValidationLevel::MaybeValid
        );
        assert_eq!(
            min_validation_level(
                ValidationLevel::MaybeValidDeferred,
                ValidationLevel::MaybeValid
            ),
            ValidationLevel::MaybeValid
        );

        // MaybeValidDeferred dominates FullyValidated.
        assert_eq!(
            min_validation_level(
                ValidationLevel::MaybeValidDeferred,
                ValidationLevel::FullyValidated
            ),
            ValidationLevel::MaybeValidDeferred
        );

        // Two FullyValidated stays FullyValidated.
        assert_eq!(
            min_validation_level(
                ValidationLevel::FullyValidated,
                ValidationLevel::FullyValidated
            ),
            ValidationLevel::FullyValidated
        );
    }

    // ---- stored/incoming ballot summary tests (issue #1811) ----
    //
    // Lock in the diagnostic-only (phase, counter) extraction used by
    // the stale-ballot reject log attribution in
    // `slot::process_ballot_envelope`. The helpers are pure; these
    // tests do not depend on a tracing subscriber.

    use crate::compare::ballot_summary_of;
    use stellar_xdr::curr::{Hash, ScpStatementConfirm, ScpStatementExternalize, Signature};

    fn zero_hash() -> Hash {
        Hash::from([0u8; 32])
    }

    fn make_prepare_pledges(counter: u32, val: u8) -> ScpStatementPledges {
        ScpStatementPledges::Prepare(ScpStatementPrepare {
            quorum_set_hash: zero_hash(),
            ballot: ScpBallot {
                counter,
                value: vec![val].try_into().unwrap(),
            },
            prepared: None,
            prepared_prime: None,
            n_c: 0,
            n_h: 0,
        })
    }

    fn make_confirm_pledges(counter: u32, val: u8) -> ScpStatementPledges {
        ScpStatementPledges::Confirm(ScpStatementConfirm {
            ballot: ScpBallot {
                counter,
                value: vec![val].try_into().unwrap(),
            },
            n_prepared: 0,
            n_commit: 0,
            n_h: 0,
            quorum_set_hash: zero_hash(),
        })
    }

    fn make_externalize_pledges(counter: u32, val: u8) -> ScpStatementPledges {
        ScpStatementPledges::Externalize(ScpStatementExternalize {
            commit: ScpBallot {
                counter,
                value: vec![val].try_into().unwrap(),
            },
            n_h: 0,
            commit_quorum_set_hash: zero_hash(),
        })
    }

    fn make_nominate_pledges() -> ScpStatementPledges {
        ScpStatementPledges::Nominate(ScpNomination {
            quorum_set_hash: zero_hash(),
            votes: VecM::default(),
            accepted: VecM::default(),
        })
    }

    fn envelope_with_pledges(
        node_id: NodeId,
        slot_index: u64,
        pledges: ScpStatementPledges,
    ) -> ScpEnvelope {
        ScpEnvelope {
            statement: ScpStatement {
                node_id,
                slot_index,
                pledges,
            },
            signature: Signature(Vec::new().try_into().unwrap_or_default()),
        }
    }

    #[test]
    fn ballot_summary_of_prepare_returns_prepare_and_ballot_counter() {
        let pledges = make_prepare_pledges(7, 0);
        assert_eq!(ballot_summary_of(&pledges), Some((BallotPhase::Prepare, 7)));
    }

    #[test]
    fn ballot_summary_of_confirm_returns_confirm_and_ballot_counter() {
        let pledges = make_confirm_pledges(9, 0);
        assert_eq!(ballot_summary_of(&pledges), Some((BallotPhase::Confirm, 9)));
    }

    #[test]
    fn ballot_summary_of_externalize_returns_externalize_and_commit_counter() {
        let pledges = make_externalize_pledges(3, 0);
        assert_eq!(
            ballot_summary_of(&pledges),
            Some((BallotPhase::Externalize, 3))
        );
    }

    #[test]
    fn ballot_summary_of_nominate_returns_none() {
        let pledges = make_nominate_pledges();
        assert_eq!(ballot_summary_of(&pledges), None);
    }

    #[test]
    fn stored_ballot_summary_returns_none_for_unknown_node() {
        let ballot = BallotProtocol::new();
        let node_id = make_node_id(1);
        assert_eq!(ballot.stored_ballot_summary(&node_id), None);
    }

    #[test]
    fn stored_ballot_summary_returns_latest_envelopes_summary_prepare() {
        let mut ballot = BallotProtocol::new();
        let node_id = make_node_id(2);
        let env = envelope_with_pledges(node_id.clone(), 42, make_prepare_pledges(5, 1));
        ballot.latest_envelopes.insert(node_id.clone(), env);
        assert_eq!(
            ballot.stored_ballot_summary(&node_id),
            Some((BallotPhase::Prepare, 5)),
        );
    }

    #[test]
    fn stored_ballot_summary_returns_latest_envelopes_summary_confirm() {
        let mut ballot = BallotProtocol::new();
        let node_id = make_node_id(3);
        let env = envelope_with_pledges(node_id.clone(), 42, make_confirm_pledges(4, 1));
        ballot.latest_envelopes.insert(node_id.clone(), env);
        assert_eq!(
            ballot.stored_ballot_summary(&node_id),
            Some((BallotPhase::Confirm, 4)),
        );
    }

    #[test]
    fn stored_ballot_summary_returns_latest_envelopes_summary_externalize() {
        let mut ballot = BallotProtocol::new();
        let node_id = make_node_id(4);
        let env = envelope_with_pledges(node_id.clone(), 42, make_externalize_pledges(1, 1));
        ballot.latest_envelopes.insert(node_id.clone(), env);
        assert_eq!(
            ballot.stored_ballot_summary(&node_id),
            Some((BallotPhase::Externalize, 1)),
        );
    }

    #[test]
    fn stored_ballot_summary_returns_none_if_latest_envelope_is_nominate() {
        // Paranoid test: if a Nominate envelope is ever present in
        // BallotProtocol::latest_envelopes (which should not occur
        // through normal code paths — nominations live in
        // NominationProtocol::latest_nominations), stored_ballot_summary
        // returns None rather than panicking or returning garbage.
        let mut ballot = BallotProtocol::new();
        let node_id = make_node_id(5);
        let env = envelope_with_pledges(node_id.clone(), 42, make_nominate_pledges());
        ballot.latest_envelopes.insert(node_id.clone(), env);
        assert_eq!(ballot.stored_ballot_summary(&node_id), None);
    }

    // --- Reporting state tests ---

    #[test]
    fn test_reporting_state_local_node_always_agree() {
        let ballot = BallotProtocol::new();
        let local = make_node_id(1);
        // Local node is always Agree regardless of state.
        assert_eq!(
            ballot.get_reporting_state(&local, &local, false),
            crate::ReportingNodeState::Agree
        );
        assert_eq!(
            ballot.get_reporting_state(&local, &local, true),
            crate::ReportingNodeState::Agree
        );
    }

    #[test]
    fn test_reporting_state_no_envelope_noinfo_vs_missing() {
        let ballot = BallotProtocol::new();
        let local = make_node_id(1);
        let peer = make_node_id(2);

        // timer_exp_count=0, selfAlreadyMovedOn=false → NoInfo
        assert_eq!(
            ballot.get_reporting_state(&peer, &local, false),
            crate::ReportingNodeState::NoInfo
        );

        // selfAlreadyMovedOn=true → Missing even with timer_exp_count=0
        assert_eq!(
            ballot.get_reporting_state(&peer, &local, true),
            crate::ReportingNodeState::Missing
        );
    }

    #[test]
    fn test_reporting_state_timer_threshold_missing() {
        let mut ballot = BallotProtocol::new();
        let local = make_node_id(1);
        let peer = make_node_id(2);

        // Simulate 2 timer expirations (threshold is 2).
        ballot.timer_exp_count = 2;

        // No envelope + timer >= threshold → Missing
        assert_eq!(
            ballot.get_reporting_state(&peer, &local, false),
            crate::ReportingNodeState::Missing
        );
    }

    #[test]
    fn test_reporting_state_timer_below_threshold_noinfo() {
        let mut ballot = BallotProtocol::new();
        let local = make_node_id(1);
        let peer = make_node_id(2);

        ballot.timer_exp_count = 1; // Below threshold of 2

        assert_eq!(
            ballot.get_reporting_state(&peer, &local, false),
            crate::ReportingNodeState::NoInfo
        );
    }

    #[test]
    fn test_reporting_state_has_envelope_agree() {
        let mut ballot = BallotProtocol::new();
        let local = make_node_id(1);
        let peer = make_node_id(2);
        let qs = make_quorum_set(vec![local.clone(), peer.clone()], 2);

        // Peer has a PREPARE envelope → Agree (default).
        let env = make_prepare_envelope(
            peer.clone(),
            1,
            &qs,
            ScpBallot {
                counter: 1,
                value: make_value(&[1]),
            },
        );
        ballot.latest_envelopes.insert(peer.clone(), env);

        assert_eq!(
            ballot.get_reporting_state(&peer, &local, false),
            crate::ReportingNodeState::Agree
        );
    }

    #[test]
    fn test_reporting_state_delayed() {
        let mut ballot = BallotProtocol::new();
        let local = make_node_id(1);
        let peer = make_node_id(2);
        let qs = make_quorum_set(vec![local.clone(), peer.clone()], 2);

        // Local is in Externalize phase and has emitted.
        ballot.phase = BallotPhase::Externalize;
        let self_ext = make_externalize_envelope(
            local.clone(),
            1,
            &qs,
            ScpBallot {
                counter: 1,
                value: make_value(&[1]),
            },
            1,
        );
        ballot.last_envelope_emit = Some(self_ext);

        // Peer is still in PREPARE (hasn't externalized).
        let peer_env = make_prepare_envelope(
            peer.clone(),
            1,
            &qs,
            ScpBallot {
                counter: 1,
                value: make_value(&[1]),
            },
        );
        ballot.latest_envelopes.insert(peer.clone(), peer_env);

        // selfAlreadyMovedOn = true → Delayed.
        assert_eq!(
            ballot.get_reporting_state(&peer, &local, true),
            crate::ReportingNodeState::Delayed
        );

        // selfAlreadyMovedOn = false → Agree (not delayed).
        assert_eq!(
            ballot.get_reporting_state(&peer, &local, false),
            crate::ReportingNodeState::Agree
        );
    }

    #[test]
    fn test_reporting_state_disagree() {
        let mut ballot = BallotProtocol::new();
        let local = make_node_id(1);
        let peer = make_node_id(2);
        let qs = make_quorum_set(vec![local.clone(), peer.clone()], 2);

        // Local is in Confirm phase with value [1].
        ballot.phase = BallotPhase::Confirm;
        let self_env = make_confirm_envelope(
            local.clone(),
            1,
            &qs,
            ScpBallot {
                counter: 1,
                value: make_value(&[1]),
            },
        );
        ballot.last_envelope_emit = Some(self_env);

        // Peer has confirmed an incompatible value [2].
        let peer_env = make_confirm_envelope(
            peer.clone(),
            1,
            &qs,
            ScpBallot {
                counter: 1,
                value: make_value(&[2]),
            },
        );
        ballot.latest_envelopes.insert(peer.clone(), peer_env);

        // Incompatible confirmed ballots → Disagree.
        assert_eq!(
            ballot.get_reporting_state(&peer, &local, false),
            crate::ReportingNodeState::Disagree
        );
    }

    #[test]
    fn test_reporting_state_disagree_overrides_delayed() {
        let mut ballot = BallotProtocol::new();
        let local = make_node_id(1);
        let peer = make_node_id(2);
        let qs = make_quorum_set(vec![local.clone(), peer.clone()], 2);

        // Local externalized value [1].
        ballot.phase = BallotPhase::Externalize;
        let self_ext = make_externalize_envelope(
            local.clone(),
            1,
            &qs,
            ScpBallot {
                counter: 1,
                value: make_value(&[1]),
            },
            1,
        );
        ballot.last_envelope_emit = Some(self_ext);

        // Peer confirmed incompatible value [2].
        let peer_env = make_confirm_envelope(
            peer.clone(),
            1,
            &qs,
            ScpBallot {
                counter: 1,
                value: make_value(&[2]),
            },
        );
        ballot.latest_envelopes.insert(peer.clone(), peer_env);

        // Even with selfAlreadyMovedOn=true, Disagree overrides Delayed.
        assert_eq!(
            ballot.get_reporting_state(&peer, &local, true),
            crate::ReportingNodeState::Disagree
        );
    }

    #[test]
    fn test_bump_timeout_increments_timer_exp_count() {
        let node = make_node_id(1);
        let qs = make_quorum_set(vec![node.clone()], 1);
        let driver = Arc::new(MockDriver::with_quorum_set(qs.clone()));

        let mut ballot = BallotProtocol::new();
        assert_eq!(ballot.timer_exp_count, 0);

        // Set a current ballot so abandon_ballot can work.
        ballot.current_ballot = Some(ScpBallot {
            counter: 1,
            value: make_value(&[1]),
        });

        ballot.bump_timeout(&ctx!(&node, &qs, &driver, 1), None);
        assert_eq!(ballot.timer_exp_count, 1);

        ballot.bump_timeout(&ctx!(&node, &qs, &driver, 1), None);
        assert_eq!(ballot.timer_exp_count, 2);
    }

    // ---- Self-emission validation gate tests (issue #2089) ----

    /// Test that self-emission rejects Invalid local ballot values.
    /// Matches stellar-core processEnvelope returning INVALID for self when
    /// validateValues returns kInvalidValue.
    #[test]
    fn test_self_emission_invalid_value_rejected() {
        let node = make_node_id(1);
        let qs = make_quorum_set(vec![node.clone()], 1);
        let driver = Arc::new(
            MockDriverBuilder::new()
                .quorum_set(qs.clone())
                .validation_level(ValidationLevel::Invalid)
                .return_qset_by_hash()
                .build(),
        );

        let mut ballot = BallotProtocol::new();
        let value = make_value(&[42]);

        // Bump to set current_ballot (counter=1, value=[42])
        let ctx = ctx!(&node, &qs, &driver, 1);
        ballot.update_current_value(&ScpBallot {
            counter: 1,
            value: value.clone(),
        });

        // Now emit — validation should reject
        ballot.emit_current_state(&ctx);

        // Envelope should NOT be recorded
        assert!(
            !ballot.latest_envelopes.contains_key(&node),
            "Invalid self-envelope should not be recorded"
        );
        // No emission
        assert_eq!(
            driver.emit_count.load(Ordering::SeqCst),
            0,
            "Invalid self-envelope should not be emitted"
        );
        // fully_validated unchanged
        assert!(
            ballot.fully_validated,
            "fully_validated should remain true for Invalid (envelope rejected entirely)"
        );
    }

    /// Test that self-emission clears fully_validated for MaybeValid values
    /// and suppresses network emission.
    /// Matches stellar-core processEnvelope setting mSlot.setFullyValidated(false)
    /// for kMaybeValidValue.
    #[test]
    fn test_self_emission_maybe_valid_clears_fully_validated() {
        let node = make_node_id(1);
        let qs = make_quorum_set(vec![node.clone()], 1);
        let driver = Arc::new(
            MockDriverBuilder::new()
                .quorum_set(qs.clone())
                .validation_level(ValidationLevel::MaybeValid)
                .return_qset_by_hash()
                .build(),
        );

        let mut ballot = BallotProtocol::new();
        let value = make_value(&[42]);

        let ctx = ctx!(&node, &qs, &driver, 1);
        ballot.update_current_value(&ScpBallot {
            counter: 1,
            value: value.clone(),
        });

        ballot.emit_current_state(&ctx);

        // Envelope IS recorded (for quorum self-counting)
        assert!(
            ballot.latest_envelopes.contains_key(&node),
            "MaybeValid self-envelope should be recorded"
        );
        // fully_validated cleared
        assert!(
            !ballot.fully_validated,
            "fully_validated should be false after MaybeValid self-emission"
        );
        // NOT emitted (send_latest_envelope gates on fully_validated)
        assert_eq!(
            driver.emit_count.load(Ordering::SeqCst),
            0,
            "MaybeValid self-envelope should not be emitted"
        );
        // needs_clear_slot_validation flag set for slot propagation
        assert!(
            ballot.needs_clear_slot_validation,
            "needs_clear_slot_validation should be set for slot to consume"
        );
    }

    /// Test that self-emission with FullyValidated values records and emits.
    #[test]
    fn test_self_emission_fully_validated_emits() {
        let node = make_node_id(1);
        let qs = make_quorum_set(vec![node.clone()], 1);
        let driver = Arc::new(
            MockDriverBuilder::new()
                .quorum_set(qs.clone())
                .validation_level(ValidationLevel::FullyValidated)
                .return_qset_by_hash()
                .build(),
        );

        let mut ballot = BallotProtocol::new();
        let value = make_value(&[42]);

        let ctx = ctx!(&node, &qs, &driver, 1);
        ballot.update_current_value(&ScpBallot {
            counter: 1,
            value: value.clone(),
        });

        ballot.emit_current_state(&ctx);

        // Envelope recorded
        assert!(
            ballot.latest_envelopes.contains_key(&node),
            "FullyValidated self-envelope should be recorded"
        );
        // fully_validated stays true
        assert!(
            ballot.fully_validated,
            "fully_validated should remain true for FullyValidated"
        );
        // Emitted
        assert_eq!(
            driver.emit_count.load(Ordering::SeqCst),
            1,
            "FullyValidated self-envelope should be emitted"
        );
    }

    /// Test that MaybeValidDeferred also clears fully_validated on self-emission,
    /// consistent with the peer path behavior via clears_fully_validated().
    #[test]
    fn test_self_emission_maybe_valid_deferred_clears_fully_validated() {
        let node = make_node_id(1);
        let qs = make_quorum_set(vec![node.clone()], 1);
        let driver = Arc::new(
            MockDriverBuilder::new()
                .quorum_set(qs.clone())
                .validation_level(ValidationLevel::MaybeValidDeferred)
                .return_qset_by_hash()
                .build(),
        );

        let mut ballot = BallotProtocol::new();
        let value = make_value(&[42]);

        let ctx = ctx!(&node, &qs, &driver, 1);
        ballot.update_current_value(&ScpBallot {
            counter: 1,
            value: value.clone(),
        });

        ballot.emit_current_state(&ctx);

        // Envelope IS recorded
        assert!(
            ballot.latest_envelopes.contains_key(&node),
            "MaybeValidDeferred self-envelope should be recorded"
        );
        // fully_validated cleared (same as MaybeValid via clears_fully_validated())
        assert!(
            !ballot.fully_validated,
            "fully_validated should be false after MaybeValidDeferred self-emission"
        );
        // NOT emitted
        assert_eq!(
            driver.emit_count.load(Ordering::SeqCst),
            0,
            "MaybeValidDeferred self-envelope should not be emitted"
        );
    }
}
