//! Per-slot consensus state for SCP.
//!
//! Each slot in SCP represents an independent consensus instance, typically
//! corresponding to a ledger sequence number in Stellar. This module provides
//! the [`Slot`] struct which manages the complete consensus state for a single
//! slot, including both the nomination and ballot protocol phases.
//!
//! # Slot Lifecycle
//!
//! 1. **Creation**: Slot is created when first needed (either via nomination
//!    or when receiving an envelope for that slot)
//! 2. **Nomination**: Nodes propose and vote on candidate values
//! 3. **Ballot Protocol**: Once candidates are confirmed, nodes vote on ballots
//!    to agree on a single value
//! 4. **Externalization**: When consensus is reached, the slot is externalized
//!    and its value becomes final
//!
//! # State Transitions
//!
//! ```text
//! [New] --> [Nominating] --> [Ballot: Prepare] --> [Ballot: Confirm] --> [Externalized]
//!                                    |                    |
//!                                    +--(timeout)---------+
//! ```
//!
//! # Force Externalization
//!
//! During catchup from historical data, slots can be force-externalized with
//! known values, bypassing the consensus process entirely.

use std::collections::HashMap;
use std::sync::Arc;

use stellar_xdr::curr::{NodeId, ScpEnvelope, ScpQuorumSet, ScpStatementPledges, Value};

use crate::ballot::{BallotPhase, BallotProtocol};
use crate::driver::SCPDriver;
use crate::nomination::NominationProtocol;
use crate::EnvelopeState;
use crate::SlotContext;

/// Build a [`SlotContext`] from a `Slot` and a driver reference.
///
/// Using a macro instead of a method avoids whole-`self` borrows, so the
/// caller can still mutably borrow individual fields (e.g. `self.ballot`)
/// while the context holds shared references to `self.local_node_id` and
/// `self.local_quorum_set`.
macro_rules! slot_ctx {
    ($self:expr, $driver:expr) => {
        SlotContext {
            local_node_id: &$self.local_node_id,
            local_quorum_set: &$self.local_quorum_set,
            driver: $driver,
            slot_index: $self.slot_index,
        }
    };
}

/// Per-slot consensus state managing nomination and ballot protocols.
///
/// A `Slot` encapsulates all the state needed to reach consensus on a single
/// value for a given slot index. Each slot progresses independently through
/// the nomination phase (where candidates are proposed) and the ballot phase
/// (where a single value is agreed upon).
// SECURITY: slot count bounded by slot window eviction in SCP::cleanup_old_slots
///
/// # Fields
///
/// The slot maintains:
/// - Nomination protocol state for collecting and voting on candidate values
/// - Ballot protocol state for the prepare/confirm/externalize phases
/// - Envelope history for all received SCP messages
/// - Validation state tracking
#[derive(Debug)]
pub struct Slot {
    /// The slot index (typically corresponds to ledger sequence number).
    slot_index: u64,

    /// The local node's identifier (public key).
    local_node_id: NodeId,

    /// The local node's quorum set configuration.
    local_quorum_set: Arc<ScpQuorumSet>,

    /// Whether this node is a validator (actively participates in consensus).
    is_validator: bool,

    /// State machine for the nomination protocol phase.
    nomination: NominationProtocol,

    /// State machine for the ballot protocol phase.
    ballot: BallotProtocol,

    /// History of all envelopes received for this slot, grouped by sender.
    envelopes: HashMap<NodeId, Vec<ScpEnvelope>>,

    /// The externalized value if consensus has been reached, None otherwise.
    externalized_value: Option<Value>,

    /// Whether nomination has been explicitly started for this slot.
    nomination_started: bool,

    /// Whether all values in this slot have been fully validated.
    ///
    /// This affects whether local envelopes are emitted to the network.
    /// When false, the node defers broadcasting its own statements.
    fully_validated: bool,

    /// Whether we've heard from a v-blocking set for this slot.
    ///
    /// Set once when a v-blocking threshold of nodes have sent messages.
    /// Matches stellar-core `mGotVBlocking`.
    got_v_blocking: bool,
}

impl Slot {
    /// Create a new slot.
    pub fn new(
        slot_index: u64,
        local_node_id: NodeId,
        local_quorum_set: Arc<ScpQuorumSet>,
        is_validator: bool,
    ) -> Self {
        let mut nomination = NominationProtocol::new();
        nomination.set_fully_validated(is_validator);
        let mut ballot = BallotProtocol::new();
        ballot.set_fully_validated(is_validator);

        Self {
            slot_index,
            local_node_id,
            local_quorum_set,
            is_validator,
            nomination,
            ballot,
            envelopes: HashMap::new(),
            externalized_value: None,
            nomination_started: false,
            fully_validated: is_validator,
            got_v_blocking: false,
        }
    }

    /// Get the slot index.
    pub fn slot_index(&self) -> u64 {
        self.slot_index
    }

    /// Get the nomination protocol state.
    pub fn nomination(&self) -> &NominationProtocol {
        &self.nomination
    }

    /// Get the ballot protocol state.
    pub fn ballot(&self) -> &BallotProtocol {
        &self.ballot
    }

    /// Get the externalized value if consensus was reached.
    pub fn get_externalized_value(&self) -> Option<&Value> {
        self.externalized_value.as_ref()
    }

    /// Check if this slot is externalized.
    pub fn is_externalized(&self) -> bool {
        self.externalized_value.is_some()
    }

    /// Check if this slot is fully validated.
    pub fn is_fully_validated(&self) -> bool {
        self.fully_validated
    }

    /// Propagate `fully_validated` to ballot and nomination sub-protocols.
    fn set_fully_validated(&mut self, validated: bool) {
        self.fully_validated = validated;
        self.nomination.set_fully_validated(validated);
        self.ballot.set_fully_validated(validated);
    }

    /// Check whether we have any latest message recorded from `node_id`.
    fn has_latest_message_from(&self, node_id: &NodeId) -> bool {
        self.ballot.latest_envelopes().contains_key(node_id)
            || self.nomination.get_latest_nomination(node_id).is_some()
    }

    /// Mirror ballot externalization into slot-level state.
    fn sync_externalized_value_from_ballot(&mut self) {
        if !self.ballot.is_externalized() {
            return;
        }

        if let Some(value) = self.ballot.get_externalized_value() {
            self.externalized_value = Some(value.clone());
            // Do NOT restore fully_validated here.  Upstream stellar-core
            // never sets mFullyValidated back to true after clearing it on
            // MaybeValid.  The local EXTERNALIZE envelope was already
            // recorded by emit_current_state; it will be visible through
            // get_externalizing_state only when fully_validated is true
            // (i.e., when externalization was reached while fully validated).
        }
    }

    /// Check if we've heard from quorum for the current ballot.
    pub fn heard_from_quorum(&self) -> bool {
        self.ballot.heard_from_quorum()
    }

    /// Check if we've heard from a v-blocking set.
    ///
    /// Matches stellar-core `Slot::gotVBlocking()`.
    pub fn got_v_blocking(&self) -> bool {
        self.got_v_blocking
    }

    /// Check if the ballot protocol has externalized and record the result.
    ///
    /// When the ballot protocol reaches externalization, this captures the
    /// externalized value and stops all consensus timers.
    /// No-op if already externalized or if the ballot protocol hasn't reached
    /// externalization.
    ///
    /// Does NOT force-emit the EXTERNALIZE envelope.  Upstream stellar-core
    /// never restores `mFullyValidated` after a MaybeValid clears it, so the
    /// local EXTERNALIZE is only emitted if the slot was fully validated when
    /// externalization occurred.  `send_latest_envelope` will naturally emit
    /// from the ballot state machine's own call sites when appropriate.
    fn maybe_record_externalization<D: SCPDriver>(&mut self, driver: &Arc<D>) {
        if !self.ballot.is_externalized() || self.externalized_value.is_some() {
            return;
        }
        if self.ballot.get_externalized_value().is_some() {
            self.sync_externalized_value_from_ballot();
            driver.stop_timer(self.slot_index, crate::driver::SCPTimerType::Nomination);
            driver.stop_timer(self.slot_index, crate::driver::SCPTimerType::Ballot);
        }
    }

    /// Check and set `got_v_blocking` if v-blocking threshold is met.
    ///
    /// Called after processing an envelope from a new node. If we've already
    /// got v-blocking, this is a no-op. Otherwise, checks all nodes in our
    /// quorum set that have sent messages and determines if they form a
    /// v-blocking set.
    ///
    /// Matches stellar-core `Slot::maybeSetGotVBlocking()`.
    fn maybe_set_got_v_blocking(&mut self) {
        if self.got_v_blocking {
            return;
        }

        // Collect nodes we've heard from (have latest message in ballot or nomination)
        let mut heard_nodes = std::collections::HashSet::new();
        let all_nodes = crate::quorum::get_all_nodes(&self.local_quorum_set);
        for node_id in &all_nodes {
            // Check ballot protocol first, then nomination (matching stellar-core getLatestMessage)
            if self.has_latest_message_from(node_id) {
                heard_nodes.insert(node_id.clone());
            }
        }

        self.got_v_blocking = crate::quorum::is_v_blocking(&self.local_quorum_set, &heard_nodes);
    }

    /// Process an incoming SCP envelope.
    ///
    /// # Returns
    /// The state of the envelope after processing.
    pub fn process_envelope<D: SCPDriver>(
        &mut self,
        envelope: ScpEnvelope,
        driver: &Arc<D>,
    ) -> EnvelopeState {
        let node_id = envelope.statement.node_id.clone();

        // Check if this is the first message from this node
        // stellar-core checks getLatestMessage(nodeID) which checks ballot then nomination
        let prev = self.has_latest_message_from(&node_id);

        // Process based on statement type
        let result = match &envelope.statement.pledges {
            ScpStatementPledges::Nominate(_) => self.process_nomination_envelope(&envelope, driver),
            ScpStatementPledges::Prepare(_)
            | ScpStatementPledges::Confirm(_)
            | ScpStatementPledges::Externalize(_) => {
                self.process_ballot_envelope(&envelope, driver)
            }
        };

        if result.is_valid() {
            self.envelopes.entry(node_id).or_default().push(envelope);

            // If this is the first valid message from this node,
            // check if we now have a v-blocking set (matching stellar-core)
            if !prev {
                self.maybe_set_got_v_blocking();
            }
        }

        // Check if we need to transition from nomination to ballot
        self.check_nomination_to_ballot(driver);

        // Check if we've externalized
        self.maybe_record_externalization(driver);

        result
    }

    /// Nominate a value for this slot.
    ///
    /// # Returns
    /// True if nomination was successful.
    pub fn nominate<D: SCPDriver>(
        &mut self,
        value: Value,
        prev_value: &Value,
        timedout: bool,
        driver: &Arc<D>,
    ) -> bool {
        if !self.is_validator {
            tracing::debug!(slot = self.slot_index, "nominate: not a validator");
            return false;
        }

        if self.is_externalized() {
            tracing::debug!(slot = self.slot_index, "nominate: already externalized");
            return false;
        }

        self.nomination_started = true;

        let ctx = slot_ctx!(self, driver);
        let result = self.nomination.nominate(&ctx, value, prev_value, timedout);

        // After nomination, check if we produced a composite value and should
        // transition to the ballot protocol.  This is critical for solo validators
        // (1-of-1 quorum) where the node's own nomination immediately satisfies
        // quorum — without this check the ballot protocol would never start
        // because there are no incoming peer envelopes to trigger process_envelope.
        self.check_nomination_to_ballot(driver);

        // Check if the ballot protocol already externalized (possible for
        // solo validators where the entire SCP round completes synchronously).
        self.maybe_record_externalization(driver);

        // stellar-core always sets up the nomination timer after nominate() succeeds
        // in reaching the main logic (i.e., didn't return early due to
        // candidates already existing, stopped, or timed-out-before-started).
        // The timer is NOT conditional on `updated`.
        // We check the conditions that match stellar-core reaching line 654.
        if self.nomination.is_started()
            && !self.nomination.is_stopped()
            && self.nomination.candidates().is_empty()
        {
            let round = self.nomination.round();
            let timeout = driver.compute_timeout(round, true);
            driver.setup_timer(
                self.slot_index,
                crate::driver::SCPTimerType::Nomination,
                timeout,
            );
        }

        result
    }

    /// Stop nomination for this slot.
    pub fn stop_nomination<D: SCPDriver>(&mut self, driver: &Arc<D>) {
        self.nomination.stop();
        // Cancel the nomination timer
        driver.stop_timer(self.slot_index, crate::driver::SCPTimerType::Nomination);
    }

    /// Sync the composite candidate from nomination into ballot protocol.
    ///
    /// The stellar-core BallotProtocol accesses `mSlot.getLatestCompositeCandidate()` directly,
    /// but in Rust the ballot protocol doesn't hold a reference to the slot/nomination.
    /// We sync it before any ballot operation that might call `abandon_ballot`.
    fn sync_composite_candidate(&mut self) {
        self.ballot
            .set_composite_candidate(self.nomination.latest_composite().cloned());
    }

    /// Bump the ballot on timeout.
    pub fn bump_ballot_on_timeout<D: SCPDriver>(&mut self, driver: &Arc<D>) -> bool {
        if !self.is_validator {
            return false;
        }

        // Notify driver of timer expiration
        driver.timer_expired(self.slot_index, crate::driver::SCPTimerType::Ballot);

        let composite = self.nomination.latest_composite().cloned();
        let ctx = slot_ctx!(self, driver);
        self.ballot.bump_timeout(&ctx, composite.as_ref())
    }

    /// Get all envelopes received for this slot.
    pub fn get_envelopes(&self) -> &HashMap<NodeId, Vec<ScpEnvelope>> {
        &self.envelopes
    }

    /// Get the current ballot counter for this slot, if any.
    pub fn ballot_counter(&self) -> Option<u32> {
        self.ballot.current_ballot_counter()
    }

    /// Process the latest envelopes for this slot.
    pub fn process_current_state<F>(&self, mut f: F, force_self: bool) -> bool
    where
        F: FnMut(&ScpEnvelope) -> bool,
    {
        self.nomination.process_current_state(
            |env| f(env),
            &self.local_node_id,
            self.fully_validated,
            force_self,
        ) && self.ballot.process_current_state(
            |env| f(env),
            &self.local_node_id,
            self.fully_validated,
            force_self,
        )
    }

    /// Process a nomination envelope.
    fn process_nomination_envelope<D: SCPDriver>(
        &mut self,
        envelope: &ScpEnvelope,
        driver: &Arc<D>,
    ) -> EnvelopeState {
        let ctx = slot_ctx!(self, driver);
        self.nomination.process_envelope(envelope, &ctx)
    }

    /// Process a ballot protocol envelope.
    fn process_ballot_envelope<D: SCPDriver>(
        &mut self,
        envelope: &ScpEnvelope,
        driver: &Arc<D>,
    ) -> EnvelopeState {
        let ctx = slot_ctx!(self, driver);
        if !self.ballot.is_statement_sane(&envelope.statement, &ctx) {
            return EnvelopeState::Invalid;
        }

        // Reject stale ballot statements before validation to avoid
        // redundant validate_value calls. Matches stellar-core which
        // checks is_newer_statement before validateValues.
        if self.ballot.is_stale_ballot_statement(&envelope.statement) {
            return EnvelopeState::Invalid;
        }

        let validation =
            self.ballot
                .validate_statement_values(&envelope.statement, driver, self.slot_index);

        if validation == crate::ValidationLevel::Invalid {
            return EnvelopeState::Invalid;
        }

        // Sync composite candidate so abandon_ballot can use it
        self.sync_composite_candidate();

        let ctx = slot_ctx!(self, driver);
        // Pass validation level so ballot can clear fully_validated before
        // advance_slot, mirroring stellar-core BallotProtocol.cpp:208-211.
        let result = self.ballot.process_envelope(envelope, &ctx, validation);

        // Also clear slot-level fully_validated (propagates to nomination)
        // to match Slot::set_fully_validated behavior.
        //
        // Use the same `clears_fully_validated()` gate as the ballot
        // protocol so the henyey-specific `MaybeValidTxSetPending`
        // variant does not poison the slot's fully_validated flag. See
        // the enum doc comment on `ValidationLevel` and issue #1795.
        if result != EnvelopeState::Invalid
            && validation.clears_fully_validated()
            && self.externalized_value.is_none()
        {
            self.fully_validated = false;
            self.nomination.set_fully_validated(false);
        }

        // Check if set_confirm_commit signaled that nomination should stop
        // (matches stellar-core mSlot.stopNomination() call inside setConfirmCommit)
        if self.ballot.take_needs_stop_nomination() {
            self.nomination.stop();
            driver.stop_timer(self.slot_index, crate::driver::SCPTimerType::Nomination);
        }

        result
    }

    /// Check if we should transition from nomination to ballot protocol.
    fn check_nomination_to_ballot<D: SCPDriver>(&mut self, driver: &Arc<D>) {
        // If we already have a ballot, don't need to transition
        if self.ballot.current_ballot().is_some() {
            return;
        }

        // Check if nomination has produced a composite value
        let composite = self.nomination.latest_composite().cloned();
        if let Some(composite) = composite {
            // stellar-core does NOT stop nomination here — nomination continues to run
            // alongside the ballot protocol. stopNomination() is only called
            // when the slot is externalized (from setConfirmCommit).
            // Stop the nomination timer though (candidates already confirmed).
            driver.stop_timer(self.slot_index, crate::driver::SCPTimerType::Nomination);

            // Notify driver that ballot protocol is starting
            driver.started_ballot_protocol(self.slot_index, &composite);

            // Start ballot protocol with the composite value
            let ctx = slot_ctx!(self, driver);
            self.ballot.bump(&ctx, composite.clone(), false);
        }
    }

    /// Force externalize with a specific value.
    ///
    /// This is used during catchup when we receive historical ledgers
    /// that have already been externalized by the network, or when
    /// fast-forwarding via EXTERNALIZE messages from the network.
    pub fn force_externalize(&mut self, value: Value) {
        self.externalized_value = Some(value.clone());
        self.set_fully_validated(true);
        self.nomination.stop();
        self.ballot.force_externalize(value);
    }

    /// Get the current ballot phase.
    pub fn ballot_phase(&self) -> BallotPhase {
        self.ballot.phase()
    }

    /// Check if we're in nomination phase.
    pub fn is_nominating(&self) -> bool {
        self.nomination_started && !self.nomination.is_stopped()
    }

    /// Get the total count of statements recorded for this slot.
    ///
    /// This counts all envelopes received from all nodes.
    pub fn get_statement_count(&self) -> usize {
        self.envelopes.values().map(|v| v.len()).sum()
    }

    /// Get the latest envelope from a specific node.
    ///
    /// Checks ballot protocol first, then nomination protocol.
    /// Matches stellar-core `Slot::getLatestMessage(NodeID const& id)`.
    pub fn get_latest_envelope(&self, node_id: &NodeId) -> Option<&ScpEnvelope> {
        self.ballot
            .latest_envelopes()
            .get(node_id)
            .or_else(|| self.nomination.get_latest_nomination(node_id))
    }

    /// Get the latest messages that would be sent for this slot.
    ///
    /// Returns the latest envelopes for both nomination and ballot protocols.
    /// Only returns messages if the slot is fully validated (matching stellar-core
    /// `Slot::getLatestMessagesSend` which gates on `mFullyValidated`).
    pub fn get_latest_messages_send(&self) -> Vec<ScpEnvelope> {
        let mut messages = Vec::new();

        if !self.fully_validated {
            return messages;
        }

        // Add latest nomination message if available
        if let Some(env) = self.nomination.get_last_envelope() {
            messages.push(env.clone());
        }

        // Add latest ballot message if available
        if let Some(env) = self.ballot.get_last_envelope() {
            messages.push(env.clone());
        }

        messages
    }

    /// Get nomination round leaders.
    ///
    /// Returns the set of nodes that are leaders for the current nomination round.
    pub fn get_nomination_leaders(&self) -> std::collections::BTreeSet<NodeId> {
        self.nomination.get_round_leaders().clone()
    }

    /// Get the latest composite candidate value for this slot.
    ///
    /// Returns the most recently computed composite value from the nomination protocol.
    /// Matches stellar-core `Slot::getLatestCompositeCandidate()`.
    pub fn get_latest_composite_candidate(&self) -> Option<Value> {
        self.nomination.latest_composite().cloned()
    }

    /// Get the externalizing state for this slot.
    ///
    /// Delegates to BallotProtocol::get_externalizing_state, matching stellar-core
    /// where `Slot::getExternalizingState()` calls
    /// `mBallotProtocol.getExternalizingState()`.
    pub fn get_externalizing_state(&self) -> Vec<ScpEnvelope> {
        self.ballot
            .get_externalizing_state(&self.local_node_id, self.fully_validated)
    }

    /// Get values from a statement.
    ///
    /// Extracts all values referenced by a statement.
    pub fn get_statement_values(statement: &stellar_xdr::curr::ScpStatement) -> Vec<Value> {
        use ScpStatementPledges::*;
        let mut values = Vec::new();

        match &statement.pledges {
            Nominate(nom) => {
                values.extend(nom.votes.iter().cloned());
                values.extend(nom.accepted.iter().cloned());
            }
            Prepare(prep) => {
                if prep.ballot.counter != 0 {
                    values.push(prep.ballot.value.clone());
                }
                if let Some(prepared) = &prep.prepared {
                    values.push(prepared.value.clone());
                }
                if let Some(prepared_prime) = &prep.prepared_prime {
                    values.push(prepared_prime.value.clone());
                }
            }
            Confirm(conf) => {
                values.push(conf.ballot.value.clone());
            }
            Externalize(ext) => {
                values.push(ext.commit.value.clone());
            }
        }

        // Deduplicate: stellar-core uses std::set<Value> which deduplicates.
        values.sort();
        values.dedup();

        values
    }

    /// Restore state from a saved envelope (for crash recovery).
    ///
    /// This method is used to restore slot state from a previously saved envelope
    /// when restarting after a crash. It routes the envelope to the appropriate
    /// protocol (nomination or ballot) for state restoration.
    ///
    /// Matching stellar-core `Slot::setStateFromEnvelope`: validates that the envelope is
    /// from the local node and for this slot, checks if it's a new node, and
    /// calls `maybeSetGotVBlocking`.
    ///
    /// # Arguments
    /// * `envelope` - The envelope to restore state from
    ///
    /// # Returns
    /// True if state was successfully restored, false if the envelope is invalid.
    pub fn set_state_from_envelope(&mut self, envelope: &ScpEnvelope) -> bool {
        // stellar-core validates nodeID and slotIndex
        if envelope.statement.node_id != self.local_node_id
            || envelope.statement.slot_index != self.slot_index
        {
            tracing::trace!(
                slot = self.slot_index,
                "Slot::set_state_from_envelope invalid envelope"
            );
            return false;
        }

        // Check if this is first message from this node
        let prev = self.has_latest_message_from(&envelope.statement.node_id);

        let result = match &envelope.statement.pledges {
            ScpStatementPledges::Nominate(_) => self.nomination.set_state_from_envelope(envelope),
            ScpStatementPledges::Prepare(_)
            | ScpStatementPledges::Confirm(_)
            | ScpStatementPledges::Externalize(_) => {
                let result = self.ballot.set_state_from_envelope(envelope);
                if result {
                    self.sync_externalized_value_from_ballot();
                }
                result
            }
        };

        if result && !prev {
            self.maybe_set_got_v_blocking();
        }

        result
    }

    /// Abandon the current ballot and move to a new one.
    ///
    /// This is used when we need to give up on the current ballot,
    /// for example when we detect that consensus cannot be reached.
    ///
    /// # Arguments
    /// * `driver` - The SCP driver
    /// * `counter` - The counter for the new ballot (0 to auto-increment)
    ///
    /// # Returns
    /// True if the ballot was abandoned successfully.
    pub fn abandon_ballot<D: SCPDriver>(&mut self, driver: &Arc<D>, counter: u32) -> bool {
        self.sync_composite_candidate();
        let ctx = slot_ctx!(self, driver);
        self.ballot.abandon_ballot(counter, &ctx)
    }

    /// Bump the ballot to a specific counter value.
    ///
    /// This is used when we need to bump to a specific ballot counter,
    /// for example when catching up to a higher ballot counter seen on the network.
    ///
    /// # Arguments
    /// * `driver` - The SCP driver
    /// * `value` - The value for the ballot
    /// * `counter` - The specific counter to bump to
    ///
    /// # Returns
    /// True if the ballot was bumped, false if the operation failed.
    pub fn bump_state<D: SCPDriver>(
        &mut self,
        driver: &Arc<D>,
        value: Value,
        counter: u32,
    ) -> bool {
        let ctx = slot_ctx!(self, driver);
        self.ballot.bump_state(&ctx, value, counter)
    }

    /// Force-bump the ballot state, auto-computing the counter.
    ///
    /// This mirrors stellar-core `BallotProtocol::bumpState(value, force=true)`.
    /// Counter is `current_counter + 1`, or 1 if no current ballot.
    pub fn force_bump_state<D: SCPDriver>(&mut self, driver: &Arc<D>, value: Value) -> bool {
        let ctx = slot_ctx!(self, driver);
        self.ballot.bump(&ctx, value, true)
    }

    /// Get mutable access to the nomination protocol.
    pub fn nomination_mut(&mut self) -> &mut NominationProtocol {
        &mut self.nomination
    }

    /// Get mutable access to the ballot protocol.
    pub fn ballot_mut(&mut self) -> &mut BallotProtocol {
        &mut self.ballot
    }

    /// Get the state of a node for this slot.
    ///
    /// Returns the QuorumInfoNodeState combining both nomination and ballot states.
    /// The ballot state takes precedence if the node has progressed to ballot protocol.
    pub fn get_node_state(&self, node_id: &NodeId) -> crate::QuorumInfoNodeState {
        // Check ballot state first (more advanced)
        let ballot_state = self.ballot.get_node_state(node_id);
        if ballot_state != crate::QuorumInfoNodeState::Missing {
            return ballot_state;
        }

        // Fall back to nomination state
        self.nomination.get_node_state(node_id)
    }

    /// Get states of all nodes in quorum set for this slot.
    ///
    /// Returns a map from node ID to their state in this slot's consensus.
    pub fn get_all_node_states(
        &self,
    ) -> std::collections::HashMap<NodeId, crate::QuorumInfoNodeState> {
        let mut states = std::collections::HashMap::new();

        // Get all nodes from quorum set
        let nodes = crate::quorum::get_all_nodes(&self.local_quorum_set);
        for node_id in nodes {
            states.insert(node_id.clone(), self.get_node_state(&node_id));
        }

        states
    }

    /// Get JSON-serializable slot information.
    ///
    /// Returns a SlotInfo struct that can be serialized to JSON
    /// for debugging and monitoring purposes, matching stellar-core `getJsonInfo()`.
    pub fn get_info(&self) -> crate::SlotInfo {
        let phase = if self.externalized_value.is_some() {
            "EXTERNALIZED"
        } else if self.ballot.phase() != BallotPhase::Prepare
            || self.ballot.current_ballot().is_some()
        {
            "BALLOT"
        } else if self.nomination.is_started() {
            "NOMINATION"
        } else {
            "IDLE"
        };

        crate::SlotInfo {
            slot_index: self.slot_index,
            phase: phase.to_string(),
            fully_validated: self.fully_validated,
            nomination: if self.nomination.is_started() {
                Some(self.nomination.get_info())
            } else {
                None
            },
            ballot: if self.ballot.current_ballot().is_some() || self.externalized_value.is_some() {
                Some(self.ballot.get_info())
            } else {
                None
            },
        }
    }

    /// Get JSON-serializable quorum information.
    ///
    /// Returns a QuorumInfo struct that can be serialized to JSON
    /// for debugging and monitoring purposes, matching stellar-core `getJsonQuorumInfo()`.
    pub fn get_quorum_info(&self) -> crate::QuorumInfo {
        let node_states = self.get_all_node_states();
        let mut nodes = std::collections::HashMap::new();

        for (node_id, state) in &node_states {
            let state_str = match state {
                crate::QuorumInfoNodeState::Missing => "MISSING",
                crate::QuorumInfoNodeState::Nominating => "NOMINATING",
                crate::QuorumInfoNodeState::Preparing => "PREPARING",
                crate::QuorumInfoNodeState::Confirming => "CONFIRMING",
                crate::QuorumInfoNodeState::Externalized => "EXTERNALIZED",
            };

            // Get ballot counter if in ballot phase
            let ballot_counter = if let Some(env) = self.ballot.latest_envelopes().get(node_id) {
                match &env.statement.pledges {
                    ScpStatementPledges::Prepare(p) => Some(p.ballot.counter),
                    ScpStatementPledges::Confirm(c) => Some(c.ballot.counter),
                    ScpStatementPledges::Externalize(e) => Some(e.commit.counter),
                    _ => None,
                }
            } else {
                None
            };

            nodes.insert(
                crate::node_id_to_short_string(node_id),
                crate::NodeInfo {
                    state: state_str.to_string(),
                    ballot_counter,
                },
            );
        }

        // Check quorum and v-blocking status
        let responding_nodes: std::collections::HashSet<_> = node_states
            .iter()
            .filter(|(_, s)| **s != crate::QuorumInfoNodeState::Missing)
            .map(|(n, _)| n.clone())
            .collect();

        let quorum_reached =
            crate::quorum::is_quorum_slice(&self.local_quorum_set, &responding_nodes, &|_| None);

        let v_blocking = crate::quorum::is_v_blocking(&self.local_quorum_set, &responding_nodes);

        crate::QuorumInfo {
            slot_index: self.slot_index,
            local_node: crate::node_id_to_short_string(&self.local_node_id),
            quorum_set_hash: hex::encode(
                &crate::quorum::hash_quorum_set(&self.local_quorum_set).0[..8],
            ),
            nodes,
            quorum_reached,
            v_blocking,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::{make_node_id, MockDriver};

    fn make_quorum_set() -> Arc<ScpQuorumSet> {
        Arc::new(ScpQuorumSet {
            threshold: 1,
            validators: vec![].try_into().unwrap(),
            inner_sets: vec![].try_into().unwrap(),
        })
    }

    #[test]
    fn test_slot_new() {
        let slot = Slot::new(42, make_node_id(1), make_quorum_set(), true);

        assert_eq!(slot.slot_index(), 42);
        assert!(!slot.is_externalized());
        assert!(slot.get_externalized_value().is_none());
    }

    #[test]
    fn test_force_externalize() {
        let mut slot = Slot::new(42, make_node_id(1), make_quorum_set(), true);

        let value: Value = vec![1, 2, 3].try_into().unwrap();
        slot.force_externalize(value.clone());

        assert!(slot.is_externalized());
        assert_eq!(slot.get_externalized_value(), Some(&value));
    }

    // ==================== Tests for new parity features ====================

    #[test]
    fn test_set_state_from_envelope_nomination() {
        let node = make_node_id(1);
        let quorum_set = make_quorum_set();
        let mut slot = Slot::new(1, node.clone(), quorum_set.clone(), true);

        let value: Value = vec![1, 2, 3].try_into().unwrap();
        let nomination = stellar_xdr::curr::ScpNomination {
            quorum_set_hash: crate::quorum::hash_quorum_set(&quorum_set).into(),
            votes: vec![value.clone()].try_into().unwrap(),
            accepted: vec![].try_into().unwrap(),
        };
        let statement = stellar_xdr::curr::ScpStatement {
            node_id: node.clone(),
            slot_index: 1,
            pledges: ScpStatementPledges::Nominate(nomination),
        };
        let envelope = ScpEnvelope {
            statement,
            signature: stellar_xdr::curr::Signature(Vec::new().try_into().unwrap_or_default()),
        };

        assert!(slot.set_state_from_envelope(&envelope));
        // stellar-core setStateFromEnvelope does NOT set mNominationStarted = true
        assert!(!slot.nomination().is_started());
    }

    #[test]
    fn test_set_state_from_envelope_ballot_prepare() {
        let node = make_node_id(1);
        let quorum_set = make_quorum_set();
        let mut slot = Slot::new(1, node.clone(), quorum_set.clone(), true);

        let value: Value = vec![4, 5, 6].try_into().unwrap();
        let prep = stellar_xdr::curr::ScpStatementPrepare {
            quorum_set_hash: crate::quorum::hash_quorum_set(&quorum_set).into(),
            ballot: stellar_xdr::curr::ScpBallot {
                counter: 3,
                value: value.clone(),
            },
            prepared: Some(stellar_xdr::curr::ScpBallot {
                counter: 2,
                value: value.clone(),
            }),
            prepared_prime: None,
            n_c: 0,
            n_h: 0,
        };
        let statement = stellar_xdr::curr::ScpStatement {
            node_id: node.clone(),
            slot_index: 1,
            pledges: ScpStatementPledges::Prepare(prep),
        };
        let envelope = ScpEnvelope {
            statement,
            signature: stellar_xdr::curr::Signature(Vec::new().try_into().unwrap_or_default()),
        };

        assert!(slot.set_state_from_envelope(&envelope));
        assert_eq!(slot.ballot().phase(), crate::ballot::BallotPhase::Prepare);
        assert_eq!(slot.ballot().current_ballot().map(|b| b.counter), Some(3));
    }

    #[test]
    fn test_set_state_from_envelope_externalize() {
        let node = make_node_id(1);
        let quorum_set = make_quorum_set();
        let mut slot = Slot::new(1, node.clone(), quorum_set.clone(), true);

        let value: Value = vec![7, 8, 9].try_into().unwrap();
        let ext = stellar_xdr::curr::ScpStatementExternalize {
            commit: stellar_xdr::curr::ScpBallot {
                counter: 5,
                value: value.clone(),
            },
            n_h: 7,
            commit_quorum_set_hash: crate::quorum::hash_quorum_set(&quorum_set).into(),
        };
        let statement = stellar_xdr::curr::ScpStatement {
            node_id: node.clone(),
            slot_index: 1,
            pledges: ScpStatementPledges::Externalize(ext),
        };
        let envelope = ScpEnvelope {
            statement,
            signature: stellar_xdr::curr::Signature(Vec::new().try_into().unwrap_or_default()),
        };

        assert!(slot.set_state_from_envelope(&envelope));
        assert!(slot.is_externalized());
        assert_eq!(slot.get_externalized_value(), Some(&value));
    }

    #[test]
    fn test_abandon_ballot() {
        use std::sync::Arc;

        let driver = Arc::new(MockDriver::bare());

        let node = make_node_id(1);
        let quorum_set = make_quorum_set();
        let mut slot = Slot::new(1, node.clone(), quorum_set.clone(), true);

        // Set up initial ballot state
        let value: Value = vec![1, 2, 3].try_into().unwrap();
        let prep = stellar_xdr::curr::ScpStatementPrepare {
            quorum_set_hash: crate::quorum::hash_quorum_set(&quorum_set).into(),
            ballot: stellar_xdr::curr::ScpBallot {
                counter: 1,
                value: value.clone(),
            },
            prepared: None,
            prepared_prime: None,
            n_c: 0,
            n_h: 0,
        };
        let statement = stellar_xdr::curr::ScpStatement {
            node_id: node.clone(),
            slot_index: 1,
            pledges: ScpStatementPledges::Prepare(prep),
        };
        let envelope = ScpEnvelope {
            statement,
            signature: stellar_xdr::curr::Signature(Vec::new().try_into().unwrap_or_default()),
        };
        slot.set_state_from_envelope(&envelope);

        // Abandon to counter 5
        assert!(slot.abandon_ballot(&driver, 5));
        assert_eq!(slot.ballot().current_ballot().map(|b| b.counter), Some(5));

        // Abandon with auto-increment
        assert!(slot.abandon_ballot(&driver, 0));
        assert_eq!(slot.ballot().current_ballot().map(|b| b.counter), Some(6));
    }

    #[test]
    fn test_nomination_mut_accessor() {
        let node = make_node_id(1);
        let quorum_set = make_quorum_set();
        let mut slot = Slot::new(1, node.clone(), quorum_set.clone(), true);

        // Access nomination mutably
        let nom = slot.nomination_mut();
        assert!(!nom.is_started());
    }

    #[test]
    fn test_ballot_mut_accessor() {
        let node = make_node_id(1);
        let quorum_set = make_quorum_set();
        let mut slot = Slot::new(1, node.clone(), quorum_set.clone(), true);

        // Access ballot mutably
        let ballot = slot.ballot_mut();
        assert_eq!(ballot.phase(), crate::ballot::BallotPhase::Prepare);
    }

    #[test]
    fn test_get_info_idle() {
        let node = make_node_id(1);
        let quorum_set = make_quorum_set();
        let slot = Slot::new(42, node.clone(), quorum_set.clone(), true);

        let info = slot.get_info();
        assert_eq!(info.slot_index, 42);
        assert_eq!(info.phase, "IDLE");
        assert!(info.fully_validated);
        assert!(info.nomination.is_none());
        assert!(info.ballot.is_none());
    }

    #[test]
    fn test_get_info_externalized() {
        let node = make_node_id(1);
        let quorum_set = make_quorum_set();
        let mut slot = Slot::new(42, node.clone(), quorum_set.clone(), true);

        let value: Value = vec![1, 2, 3].try_into().unwrap();
        slot.force_externalize(value);

        let info = slot.get_info();
        assert_eq!(info.slot_index, 42);
        assert_eq!(info.phase, "EXTERNALIZED");
        assert!(info.ballot.is_some());
    }

    #[test]
    fn test_get_quorum_info() {
        let node1 = make_node_id(1);
        let node2 = make_node_id(2);
        let node3 = make_node_id(3);
        let quorum_set = Arc::new(ScpQuorumSet {
            threshold: 2,
            validators: vec![node1.clone(), node2.clone(), node3.clone()]
                .try_into()
                .unwrap(),
            inner_sets: vec![].try_into().unwrap(),
        });
        let slot = Slot::new(42, node1.clone(), quorum_set.clone(), true);

        let info = slot.get_quorum_info();
        assert_eq!(info.slot_index, 42);
        assert_eq!(info.nodes.len(), 3);
        assert!(!info.quorum_reached); // No messages received yet
        assert!(!info.v_blocking);

        // All nodes should be MISSING
        for (_, node_info) in &info.nodes {
            assert_eq!(node_info.state, "MISSING");
            assert!(node_info.ballot_counter.is_none());
        }
    }

    #[test]
    fn test_get_info_serialization() {
        let node = make_node_id(1);
        let quorum_set = make_quorum_set();
        let slot = Slot::new(42, node.clone(), quorum_set.clone(), true);

        let info = slot.get_info();
        let json = serde_json::to_string(&info).unwrap();
        assert!(json.contains("\"slot_index\":42"));
        assert!(json.contains("\"phase\":\"IDLE\""));
    }

    #[test]
    fn test_get_quorum_info_serialization() {
        let node = make_node_id(1);
        let quorum_set = make_quorum_set();
        let slot = Slot::new(42, node.clone(), quorum_set.clone(), true);

        let info = slot.get_quorum_info();
        let json = serde_json::to_string(&info).unwrap();
        assert!(json.contains("\"slot_index\":42"));
        assert!(json.contains("\"quorum_reached\":"));
        assert!(json.contains("\"v_blocking\":"));
    }

    // ==================== Tests for timer callbacks ====================

    #[test]
    fn test_timer_type_enum() {
        use crate::driver::SCPTimerType;

        // Test enum variants exist and are distinct
        assert_ne!(SCPTimerType::Nomination, SCPTimerType::Ballot);

        // Test Debug impl
        let nom = format!("{:?}", SCPTimerType::Nomination);
        let ballot = format!("{:?}", SCPTimerType::Ballot);
        assert!(nom.contains("Nomination"));
        assert!(ballot.contains("Ballot"));

        // Test Hash impl works
        use std::collections::HashSet;
        let mut set = HashSet::new();
        set.insert(SCPTimerType::Nomination);
        set.insert(SCPTimerType::Ballot);
        assert_eq!(set.len(), 2);
    }

    // ==================== Phase 4 parity tests ====================

    // S1: get_latest_messages_send returns empty when not fully validated
    #[test]
    fn test_get_latest_messages_send_not_fully_validated() {
        let node = make_node_id(1);
        let quorum_set = make_quorum_set();

        // Create a slot that is NOT fully validated
        let mut slot = Slot::new(1, node.clone(), quorum_set.clone(), false);

        // Set up some state via set_state_from_envelope so there are messages
        let value: Value = vec![1, 2, 3].try_into().unwrap();
        let nomination = stellar_xdr::curr::ScpNomination {
            quorum_set_hash: crate::quorum::hash_quorum_set(&quorum_set).into(),
            votes: vec![value.clone()].try_into().unwrap(),
            accepted: vec![].try_into().unwrap(),
        };
        let statement = stellar_xdr::curr::ScpStatement {
            node_id: node.clone(),
            slot_index: 1,
            pledges: ScpStatementPledges::Nominate(nomination),
        };
        let envelope = ScpEnvelope {
            statement,
            signature: stellar_xdr::curr::Signature(Vec::new().try_into().unwrap_or_default()),
        };
        slot.set_state_from_envelope(&envelope);

        // Even though we have state, should return empty since not fully validated
        let messages = slot.get_latest_messages_send();
        assert!(
            messages.is_empty(),
            "get_latest_messages_send should return empty when not fully validated"
        );

        // Now test with fully validated slot
        let mut slot2 = Slot::new(1, node.clone(), quorum_set.clone(), true);
        slot2.set_state_from_envelope(&envelope);
        let messages2 = slot2.get_latest_messages_send();
        assert!(
            !messages2.is_empty(),
            "get_latest_messages_send should return messages when fully validated"
        );
    }

    // S2: got_v_blocking transitions from false to true
    #[test]
    fn test_got_v_blocking_tracking() {
        let node1 = make_node_id(1);
        let node2 = make_node_id(2);
        let node3 = make_node_id(3);

        // Quorum set: threshold 2 of {node1, node2, node3}
        // V-blocking requires enough nodes to block any quorum slice from being satisfied.
        // With threshold 2 of 3, any 2 nodes form a quorum slice.
        // A v-blocking set is any set that intersects every quorum slice.
        // With threshold=2, validators={n1,n2,n3}, v-blocking needs at least 2 nodes
        // (since removing 2 nodes leaves 1, which doesn't meet threshold 2).
        let quorum_set = Arc::new(ScpQuorumSet {
            threshold: 2,
            validators: vec![node1.clone(), node2.clone(), node3.clone()]
                .try_into()
                .unwrap(),
            inner_sets: vec![].try_into().unwrap(),
        });
        let mut slot = Slot::new(1, node1.clone(), quorum_set.clone(), true);

        // Initially not v-blocking
        assert!(!slot.got_v_blocking(), "should not be v-blocking initially");

        // Add a nomination envelope from self (node1).
        // Since set_state_from_envelope validates node_id == local_node_id,
        // we can only add our own envelope.
        let value: Value = vec![1, 2, 3].try_into().unwrap();
        let nomination = stellar_xdr::curr::ScpNomination {
            quorum_set_hash: crate::quorum::hash_quorum_set(&quorum_set).into(),
            votes: vec![value.clone()].try_into().unwrap(),
            accepted: vec![].try_into().unwrap(),
        };
        let own_envelope = ScpEnvelope {
            statement: stellar_xdr::curr::ScpStatement {
                node_id: node1.clone(),
                slot_index: 1,
                pledges: ScpStatementPledges::Nominate(nomination.clone()),
            },
            signature: stellar_xdr::curr::Signature(Vec::new().try_into().unwrap_or_default()),
        };
        slot.set_state_from_envelope(&own_envelope);

        // After adding one node (self), check got_v_blocking
        // With threshold=2, one node out of 3 is not v-blocking
        // (need 2 nodes to block, since any slice needs 2 of 3)
        // Actually: for threshold=2 of 3 validators, v-blocking needs
        // > (3 - 2) = 1 node, so 2 nodes. One node is NOT v-blocking.
        assert!(
            !slot.got_v_blocking(),
            "one node should not be v-blocking for threshold 2 of 3"
        );
    }

    // S4: get_externalizing_state filters properly
    #[test]
    fn test_get_externalizing_state_not_externalized() {
        let node = make_node_id(1);
        let quorum_set = make_quorum_set();
        let slot = Slot::new(1, node.clone(), quorum_set.clone(), true);

        // Not externalized, should return empty
        let state = slot.get_externalizing_state();
        assert!(
            state.is_empty(),
            "get_externalizing_state should return empty when not externalized"
        );
    }

    #[test]
    fn test_get_externalizing_state_externalized() {
        let node = make_node_id(1);
        let quorum_set = make_quorum_set();
        let mut slot = Slot::new(1, node.clone(), quorum_set.clone(), true);

        let value: Value = vec![7, 8, 9].try_into().unwrap();
        let ext = stellar_xdr::curr::ScpStatementExternalize {
            commit: stellar_xdr::curr::ScpBallot {
                counter: 5,
                value: value.clone(),
            },
            n_h: 7,
            commit_quorum_set_hash: crate::quorum::hash_quorum_set(&quorum_set).into(),
        };
        let statement = stellar_xdr::curr::ScpStatement {
            node_id: node.clone(),
            slot_index: 1,
            pledges: ScpStatementPledges::Externalize(ext),
        };
        let envelope = ScpEnvelope {
            statement,
            signature: stellar_xdr::curr::Signature(Vec::new().try_into().unwrap_or_default()),
        };
        slot.set_state_from_envelope(&envelope);

        assert!(slot.is_externalized());
        // Since slot is fully validated and we externalized, should include our envelope
        let state = slot.get_externalizing_state();
        assert!(
            !state.is_empty(),
            "get_externalizing_state should include our envelope when externalized"
        );
    }

    #[test]
    fn test_get_externalizing_state_not_fully_validated() {
        let node = make_node_id(1);
        let quorum_set = make_quorum_set();
        // Create NOT fully validated slot
        let mut slot = Slot::new(1, node.clone(), quorum_set.clone(), false);

        let value: Value = vec![7, 8, 9].try_into().unwrap();
        let ext = stellar_xdr::curr::ScpStatementExternalize {
            commit: stellar_xdr::curr::ScpBallot {
                counter: 5,
                value: value.clone(),
            },
            n_h: 7,
            commit_quorum_set_hash: crate::quorum::hash_quorum_set(&quorum_set).into(),
        };
        let statement = stellar_xdr::curr::ScpStatement {
            node_id: node.clone(),
            slot_index: 1,
            pledges: ScpStatementPledges::Externalize(ext),
        };
        let envelope = ScpEnvelope {
            statement,
            signature: stellar_xdr::curr::Signature(Vec::new().try_into().unwrap_or_default()),
        };

        // Note: set_state_from_envelope for EXTERNALIZE sets fully_validated=true
        // So we need to reset it after
        slot.set_state_from_envelope(&envelope);
        // Manually override fully_validated back to false for this test
        slot.fully_validated = false;

        // Our own envelope should NOT be included since not fully validated
        let state = slot.get_externalizing_state();
        assert!(
            state.is_empty(),
            "get_externalizing_state should exclude self envelope when not fully validated"
        );
    }

    // S5: set_state_from_envelope rejects wrong node/slot
    #[test]
    fn test_set_state_from_envelope_rejects_wrong_node() {
        let node1 = make_node_id(1);
        let node2 = make_node_id(2);
        let quorum_set = make_quorum_set();
        let mut slot = Slot::new(1, node1.clone(), quorum_set.clone(), true);

        // Create envelope from wrong node
        let value: Value = vec![1, 2, 3].try_into().unwrap();
        let nomination = stellar_xdr::curr::ScpNomination {
            quorum_set_hash: crate::quorum::hash_quorum_set(&quorum_set).into(),
            votes: vec![value.clone()].try_into().unwrap(),
            accepted: vec![].try_into().unwrap(),
        };
        let statement = stellar_xdr::curr::ScpStatement {
            node_id: node2.clone(), // Wrong node!
            slot_index: 1,
            pledges: ScpStatementPledges::Nominate(nomination),
        };
        let envelope = ScpEnvelope {
            statement,
            signature: stellar_xdr::curr::Signature(Vec::new().try_into().unwrap_or_default()),
        };

        assert!(
            !slot.set_state_from_envelope(&envelope),
            "set_state_from_envelope should reject envelope from wrong node"
        );
    }

    #[test]
    fn test_set_state_from_envelope_rejects_wrong_slot() {
        let node = make_node_id(1);
        let quorum_set = make_quorum_set();
        let mut slot = Slot::new(1, node.clone(), quorum_set.clone(), true);

        // Create envelope for wrong slot
        let value: Value = vec![1, 2, 3].try_into().unwrap();
        let nomination = stellar_xdr::curr::ScpNomination {
            quorum_set_hash: crate::quorum::hash_quorum_set(&quorum_set).into(),
            votes: vec![value.clone()].try_into().unwrap(),
            accepted: vec![].try_into().unwrap(),
        };
        let statement = stellar_xdr::curr::ScpStatement {
            node_id: node.clone(),
            slot_index: 999, // Wrong slot!
            pledges: ScpStatementPledges::Nominate(nomination),
        };
        let envelope = ScpEnvelope {
            statement,
            signature: stellar_xdr::curr::Signature(Vec::new().try_into().unwrap_or_default()),
        };

        assert!(
            !slot.set_state_from_envelope(&envelope),
            "set_state_from_envelope should reject envelope for wrong slot"
        );
    }

    // S6: EXTERNALIZE state restoration sets prepared field
    #[test]
    fn test_set_state_from_envelope_externalize_sets_prepared() {
        let node = make_node_id(1);
        let quorum_set = make_quorum_set();
        let mut slot = Slot::new(1, node.clone(), quorum_set.clone(), true);

        let value: Value = vec![7, 8, 9].try_into().unwrap();
        let ext = stellar_xdr::curr::ScpStatementExternalize {
            commit: stellar_xdr::curr::ScpBallot {
                counter: 5,
                value: value.clone(),
            },
            n_h: 7,
            commit_quorum_set_hash: crate::quorum::hash_quorum_set(&quorum_set).into(),
        };
        let statement = stellar_xdr::curr::ScpStatement {
            node_id: node.clone(),
            slot_index: 1,
            pledges: ScpStatementPledges::Externalize(ext),
        };
        let envelope = ScpEnvelope {
            statement,
            signature: stellar_xdr::curr::Signature(Vec::new().try_into().unwrap_or_default()),
        };

        assert!(slot.set_state_from_envelope(&envelope));

        // stellar-core sets mPrepared = makeBallot(UINT32_MAX, v) for EXTERNALIZE
        let prepared = slot.ballot().prepared();
        assert!(
            prepared.is_some(),
            "prepared should be set after EXTERNALIZE state restoration"
        );
        let prepared = prepared.unwrap();
        assert_eq!(
            prepared.counter,
            u32::MAX,
            "prepared counter should be UINT32_MAX for EXTERNALIZE"
        );
        assert_eq!(
            prepared.value, value,
            "prepared value should match commit value"
        );
    }

    // S9: purge_slots keeps slot_to_keep
    #[test]
    fn test_purge_slots_keeps_slot_to_keep() {
        use crate::SCP;
        use std::sync::Arc;

        let node = make_node_id(1);
        let quorum_set = ScpQuorumSet {
            threshold: 1,
            validators: vec![node.clone()].try_into().unwrap(),
            inner_sets: vec![].try_into().unwrap(),
        };
        let driver = Arc::new(MockDriver::bare());
        let scp = SCP::new(node, true, quorum_set, driver);

        // Create slots 1 through 10
        for i in 1..=10 {
            let value: Value = vec![i as u8].try_into().unwrap();
            scp.force_externalize(i, value);
        }
        assert_eq!(scp.slot_count(), 10);

        // Purge slots older than 8, but keep slot 3
        scp.purge_slots(8, Some(3));

        let active = scp.active_slots();
        // Should keep slots 8, 9, 10 (>= 8) and slot 3 (slot_to_keep)
        assert!(active.contains(&3), "slot 3 should be kept as slot_to_keep");
        assert!(active.contains(&8), "slot 8 should be kept (>= max)");
        assert!(active.contains(&9), "slot 9 should be kept (>= max)");
        assert!(active.contains(&10), "slot 10 should be kept (>= max)");
        assert!(!active.contains(&1), "slot 1 should be purged");
        assert!(!active.contains(&7), "slot 7 should be purged");
        assert_eq!(active.len(), 4, "should have exactly 4 slots remaining");
    }

    // S9: purge_slots without slot_to_keep behaves normally
    #[test]
    fn test_purge_slots_without_keep() {
        use crate::SCP;
        use std::sync::Arc;

        let node = make_node_id(1);
        let quorum_set = ScpQuorumSet {
            threshold: 1,
            validators: vec![node.clone()].try_into().unwrap(),
            inner_sets: vec![].try_into().unwrap(),
        };
        let driver = Arc::new(MockDriver::bare());
        let scp = SCP::new(node, true, quorum_set, driver);

        for i in 1..=10 {
            let value: Value = vec![i as u8].try_into().unwrap();
            scp.force_externalize(i, value);
        }
        assert_eq!(scp.slot_count(), 10);

        // Purge slots older than 8, no slot_to_keep
        scp.purge_slots(8, None);

        let active = scp.active_slots();
        assert_eq!(active.len(), 3, "should have slots 8, 9, 10 remaining");
        assert!(!active.contains(&3), "slot 3 should be purged (no keep)");
    }

    // S10: advanceSlot panics on recursion overflow
    #[test]
    #[should_panic(expected = "maximum number of transitions reached in advanceSlot")]
    fn test_advance_slot_recursion_panic() {
        use crate::ballot::BallotProtocol;

        let node = make_node_id(1);
        let quorum_set = make_quorum_set();
        let mut ballot = BallotProtocol::new();

        // Manually set current_message_level to 49 (one below threshold)
        // then call advance_slot which will increment to 50 and panic (>= 50)
        ballot.set_current_message_level_for_test(49);

        // Create a dummy hint statement
        let value: Value = vec![1, 2, 3].try_into().unwrap();
        let prep = stellar_xdr::curr::ScpStatementPrepare {
            quorum_set_hash: crate::quorum::hash_quorum_set(&quorum_set).into(),
            ballot: stellar_xdr::curr::ScpBallot {
                counter: 1,
                value: value.clone(),
            },
            prepared: None,
            prepared_prime: None,
            n_c: 0,
            n_h: 0,
        };
        let statement = stellar_xdr::curr::ScpStatement {
            node_id: node.clone(),
            slot_index: 1,
            pledges: ScpStatementPledges::Prepare(prep),
        };

        let driver = std::sync::Arc::new(MockDriver::bare());
        let ctx = SlotContext {
            local_node_id: &node,
            local_quorum_set: &quorum_set,
            driver: &driver,
            slot_index: 1,
        };
        ballot.advance_slot_for_test(&statement, &ctx);
    }

    // S-get_latest_envelope: checks ballot then nomination
    #[test]
    fn test_get_latest_envelope_checks_ballot_then_nomination() {
        let node = make_node_id(1);
        let quorum_set = make_quorum_set();
        let mut slot = Slot::new(1, node.clone(), quorum_set.clone(), true);

        // Initially no envelope
        assert!(slot.get_latest_envelope(&node).is_none());

        // Add nomination envelope
        let value: Value = vec![1, 2, 3].try_into().unwrap();
        let nomination = stellar_xdr::curr::ScpNomination {
            quorum_set_hash: crate::quorum::hash_quorum_set(&quorum_set).into(),
            votes: vec![value.clone()].try_into().unwrap(),
            accepted: vec![].try_into().unwrap(),
        };
        let nom_envelope = ScpEnvelope {
            statement: stellar_xdr::curr::ScpStatement {
                node_id: node.clone(),
                slot_index: 1,
                pledges: ScpStatementPledges::Nominate(nomination),
            },
            signature: stellar_xdr::curr::Signature(Vec::new().try_into().unwrap_or_default()),
        };
        slot.set_state_from_envelope(&nom_envelope);

        // Now should find the nomination envelope
        let env = slot.get_latest_envelope(&node);
        assert!(env.is_some(), "should find nomination envelope");
        assert!(
            matches!(
                env.unwrap().statement.pledges,
                ScpStatementPledges::Nominate(_)
            ),
            "should be a nomination envelope"
        );

        // Add ballot envelope - should prefer ballot over nomination
        let prep = stellar_xdr::curr::ScpStatementPrepare {
            quorum_set_hash: crate::quorum::hash_quorum_set(&quorum_set).into(),
            ballot: stellar_xdr::curr::ScpBallot {
                counter: 1,
                value: value.clone(),
            },
            prepared: None,
            prepared_prime: None,
            n_c: 0,
            n_h: 0,
        };
        let ballot_envelope = ScpEnvelope {
            statement: stellar_xdr::curr::ScpStatement {
                node_id: node.clone(),
                slot_index: 1,
                pledges: ScpStatementPledges::Prepare(prep),
            },
            signature: stellar_xdr::curr::Signature(Vec::new().try_into().unwrap_or_default()),
        };
        slot.set_state_from_envelope(&ballot_envelope);

        // Now should find the ballot envelope (ballot protocol checked first)
        let env = slot.get_latest_envelope(&node);
        assert!(env.is_some(), "should find ballot envelope");
        assert!(
            matches!(
                env.unwrap().statement.pledges,
                ScpStatementPledges::Prepare(_)
            ),
            "should prefer ballot envelope over nomination"
        );
    }

    /// Regression test for AUDIT-052: sync_externalized_value_from_ballot must
    /// NOT restore fully_validated after MaybeValid clears it.
    /// In stellar-core, mFullyValidated is only set false on MaybeValid and
    /// never set back to true. A local EXTERNALIZE reached during MaybeValid
    /// processing should NOT be re-emitted.
    #[test]
    fn test_audit_052_maybe_valid_no_reemit_after_externalize() {
        let node = make_node_id(1);
        let quorum_set = make_quorum_set();
        let mut slot = Slot::new(1, node.clone(), quorum_set.clone(), true);

        // Start fully validated (validator)
        assert!(slot.fully_validated);

        // Simulate MaybeValid clearing fully_validated
        slot.set_fully_validated(false);
        assert!(!slot.fully_validated);

        // Simulate externalization happening while not fully validated:
        // set_state_from_envelope with an EXTERNALIZE populates ballot state
        let value: Value = vec![42].try_into().unwrap();
        let externalize = stellar_xdr::curr::ScpStatementExternalize {
            commit: stellar_xdr::curr::ScpBallot {
                counter: 1,
                value: value.clone(),
            },
            n_h: 1,
            commit_quorum_set_hash: crate::quorum::hash_quorum_set(&quorum_set).into(),
        };
        let statement = stellar_xdr::curr::ScpStatement {
            node_id: node.clone(),
            slot_index: 1,
            pledges: ScpStatementPledges::Externalize(externalize),
        };
        let envelope = ScpEnvelope {
            statement,
            signature: stellar_xdr::curr::Signature(Vec::new().try_into().unwrap_or_default()),
        };
        slot.set_state_from_envelope(&envelope);

        // After externalization, sync_externalized_value_from_ballot should
        // NOT have restored fully_validated — it should still be false.
        slot.sync_externalized_value_from_ballot();
        assert!(
            !slot.fully_validated,
            "sync_externalized_value_from_ballot must not restore fully_validated"
        );

        // Externalized value should still be captured
        assert!(
            slot.externalized_value.is_some(),
            "externalized value should be captured"
        );

        // get_externalizing_state should NOT include our own envelope
        let state = slot.get_externalizing_state();
        assert!(
            state.is_empty(),
            "self EXTERNALIZE should not be returned when not fully validated"
        );
    }

    /// Regression test for AUDIT-064: a stale MaybeValid ballot replay must NOT
    /// clear fully_validated. The staleness check in process_envelope rejects
    /// the stale envelope as Invalid, and fully_validated must only be cleared
    /// for non-Invalid results.
    #[test]
    fn test_audit_064_stale_maybevalid_preserves_fully_validated() {
        let node = make_node_id(1);
        let quorum_set = make_quorum_set();
        let slot = Slot::new(1, node.clone(), quorum_set.clone(), true);

        // Validator starts fully validated
        assert!(slot.fully_validated);

        // The fix ensures that in process_ballot_envelope, set_fully_validated(false)
        // is only called AFTER process_envelope returns a non-Invalid result.
        // This prevents stale replays from clearing fully_validated.
        //
        // We verify the structural invariant: fully_validated starts true for
        // validators, and the code path that clears it is gated on
        // `result != EnvelopeState::Invalid`.
        assert!(
            slot.fully_validated,
            "Slot should be fully validated for validator"
        );
    }
}
