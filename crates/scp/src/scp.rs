//! Main SCP implementation coordinating consensus across multiple slots.
//!
//! This module provides the [`SCP`] struct, which is the primary entry point
//! for using the Stellar Consensus Protocol. It manages per-slot state and
//! coordinates the nomination and ballot protocols.
//!
//! # Architecture
//!
//! ```text
//! +-------+     +------+     +------------------+
//! |  SCP  | --> | Slot | --> | NominationProtocol |
//! +-------+     +------+     +------------------+
//!                   |
//!                   +------> | BallotProtocol |
//!                            +----------------+
//! ```
//!
//! - [`SCP`] owns a map of slots, keyed by slot index (ledger sequence number)
//! - Each [`Slot`](crate::slot::Slot) contains independent nomination and ballot protocol state
//! - The [`SCPDriver`](crate::driver::SCPDriver) provides application-specific callbacks
//!
//! # Usage
//!
//! ```ignore
//! let scp = SCP::new(node_id, true, quorum_set, driver);
//!
//! // Start nominating a value for a slot
//! scp.nominate(slot_index, value, &prev_value);
//!
//! // Process incoming messages
//! let state = scp.receive_envelope(envelope);
//!
//! // Check if consensus was reached
//! if let Some(value) = scp.get_externalized_value(slot_index) {
//!     // Apply the consensus value
//! }
//! ```
//!
//! # Memory Management
//!
//! Old slots are automatically purged when the slot count exceeds `max_slots`.
//! Use [`purge_slots`](SCP::purge_slots) for explicit cleanup of historical slots.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::RwLock;
use stellar_xdr::curr::{
    NodeId, ScpEnvelope, ScpQuorumSet, ScpStatement, ScpStatementPledges, Value,
};

use crate::driver::SCPDriver;
use crate::slot::Slot;
use crate::EnvelopeState;

/// Default maximum number of slots to retain in memory.
const DEFAULT_MAX_SLOTS: usize = 100;

/// Main SCP coordinator that manages consensus across multiple slots.
///
/// The `SCP` struct is the primary entry point for using the Stellar Consensus
/// Protocol. It manages per-slot state, routes incoming messages to the
/// appropriate slot, and coordinates transitions between the nomination
/// and ballot phases.
///
/// # Type Parameters
///
/// * `D` - The driver type implementing [`SCPDriver`](crate::driver::SCPDriver)
///
/// # Thread Safety
///
/// `SCP` uses interior mutability with `RwLock` for the slot map, allowing
/// concurrent read access to slot state while serializing writes.
///
/// # Validators vs Watchers
///
/// - **Validators** (`is_validator = true`): Actively participate in consensus
///   by nominating values and voting on ballots
/// - **Watchers** (`is_validator = false`): Only observe consensus, tracking
///   externalized values without voting
pub struct SCP<D: SCPDriver> {
    /// Local node identifier (public key).
    local_node_id: NodeId,

    /// Whether this node is a validator (participates in consensus).
    is_validator: bool,

    /// Local quorum set configuration defining trusted validators.
    local_quorum_set: ScpQuorumSet,

    /// Per-slot consensus state, keyed by slot index.
    slots: RwLock<HashMap<u64, Slot>>,

    /// Application driver for callbacks and application-specific logic.
    driver: Arc<D>,

    /// Maximum number of slots to retain in memory before cleanup.
    max_slots: usize,
}

impl<D: SCPDriver> SCP<D> {
    /// Create a new SCP instance.
    ///
    /// # Arguments
    /// * `node_id` - The local node's identifier
    /// * `is_validator` - Whether this node participates in consensus
    /// * `quorum_set` - The local node's quorum set
    /// * `driver` - The driver for application-specific callbacks
    pub fn new(
        node_id: NodeId,
        is_validator: bool,
        quorum_set: ScpQuorumSet,
        driver: Arc<D>,
    ) -> Self {
        Self {
            local_node_id: node_id,
            is_validator,
            local_quorum_set: quorum_set,
            slots: RwLock::new(HashMap::new()),
            driver,
            max_slots: DEFAULT_MAX_SLOTS,
        }
    }

    /// Get the local node ID.
    pub fn local_node_id(&self) -> &NodeId {
        &self.local_node_id
    }

    /// Check if this node is a validator.
    pub fn is_validator(&self) -> bool {
        self.is_validator
    }

    /// Get the local quorum set.
    pub fn local_quorum_set(&self) -> &ScpQuorumSet {
        &self.local_quorum_set
    }

    /// Update the local quorum set.
    pub fn set_local_quorum_set(&mut self, quorum_set: ScpQuorumSet) {
        self.local_quorum_set = quorum_set;
    }

    /// Get a reference to the SCP driver.
    ///
    /// This provides access to the application driver for callbacks
    /// and application-specific logic.
    pub fn driver(&self) -> &Arc<D> {
        &self.driver
    }

    /// Get or create a slot for the given index.
    ///
    /// This is a helper to DRY up the repeated `entry().or_insert_with()`
    /// pattern used throughout the SCP methods.
    fn get_or_create_slot<'a>(
        &self,
        slots: &'a mut HashMap<u64, Slot>,
        slot_index: u64,
    ) -> &'a mut Slot {
        slots.entry(slot_index).or_insert_with(|| {
            Slot::new(
                slot_index,
                self.local_node_id.clone(),
                self.local_quorum_set.clone(),
                self.is_validator,
            )
        })
    }

    /// Check if SCP has no active slots.
    ///
    /// Returns true if there are no slots being tracked, indicating
    /// that SCP is in an idle state with no active consensus.
    pub fn empty(&self) -> bool {
        self.slots.read().is_empty()
    }

    /// Get the highest known slot index.
    ///
    /// Returns the highest slot index that SCP is tracking, regardless
    /// of whether it has been externalized. This differs from
    /// [`highest_externalized_slot`](Self::highest_externalized_slot) which
    /// only considers externalized slots.
    ///
    /// # Returns
    /// The highest slot index, or None if no slots exist.
    pub fn get_highest_known_slot(&self) -> Option<u64> {
        self.slots.read().keys().copied().max()
    }

    /// Process an incoming SCP envelope.
    ///
    /// This is the main entry point for processing SCP messages
    /// received from the network.
    ///
    /// # Arguments
    /// * `envelope` - The SCP envelope to process
    ///
    /// # Returns
    /// The state of the envelope after processing.
    pub fn receive_envelope(&self, envelope: ScpEnvelope) -> EnvelopeState {
        // Verify signature
        if !self.driver.verify_envelope(&envelope) {
            tracing::warn!(
                node_id = ?envelope.statement.node_id,
                slot = envelope.statement.slot_index,
                "Invalid envelope signature"
            );
            return EnvelopeState::Invalid;
        }

        let slot_index = envelope.statement.slot_index;

        let mut slots = self.slots.write();

        // Get or create slot
        let slot = self.get_or_create_slot(&mut slots, slot_index);

        // Process the envelope
        let result = slot.process_envelope(envelope, &self.driver);

        // Cleanup old slots if needed
        if slots.len() > self.max_slots {
            self.cleanup_old_slots(&mut slots);
        }

        result
    }

    /// Nominate a value for a slot.
    ///
    /// This starts the nomination process for a slot. Should be called
    /// when the application has a value it wants to propose for consensus.
    ///
    /// # Arguments
    /// * `slot_index` - The slot to nominate for
    /// * `value` - The value to nominate
    /// * `prev_value` - The previous slot's value (for priority calculation)
    ///
    /// # Returns
    /// True if nomination was started successfully.
    pub fn nominate(&self, slot_index: u64, value: Value, prev_value: &Value) -> bool {
        if !self.is_validator {
            return false;
        }

        let mut slots = self.slots.write();

        let slot = self.get_or_create_slot(&mut slots, slot_index);

        slot.nominate(value, prev_value, false, &self.driver)
    }

    /// Nominate with timeout flag.
    ///
    /// Called when the nomination timer expires without reaching consensus.
    pub fn nominate_timeout(&self, slot_index: u64, value: Value, prev_value: &Value) -> bool {
        if !self.is_validator {
            return false;
        }

        let mut slots = self.slots.write();

        if let Some(slot) = slots.get_mut(&slot_index) {
            slot.nominate(value, prev_value, true, &self.driver)
        } else {
            false
        }
    }

    /// Stop nomination for a slot.
    ///
    /// Called when we want to stop proposing new values,
    /// typically when the ballot protocol has taken over.
    pub fn stop_nomination(&self, slot_index: u64) {
        if let Some(slot) = self.slots.write().get_mut(&slot_index) {
            slot.stop_nomination(&self.driver);
        }
    }

    /// Bump ballot on timeout.
    ///
    /// Called when the ballot timer expires. Increases the ballot
    /// counter to try to make progress.
    pub fn bump_ballot(&self, slot_index: u64) -> bool {
        if let Some(slot) = self.slots.write().get_mut(&slot_index) {
            slot.bump_ballot_on_timeout(&self.driver)
        } else {
            false
        }
    }

    /// Handle ballot protocol timer expiration.
    ///
    /// This is an alias for [`bump_ballot`](Self::bump_ballot) that matches
    /// the stellar-core `ballotProtocolTimerExpired()` naming convention.
    ///
    /// # Arguments
    /// * `slot_index` - The slot whose timer expired
    ///
    /// # Returns
    /// True if the ballot was bumped, false otherwise.
    pub fn ballot_protocol_timer_expired(&self, slot_index: u64) -> bool {
        self.bump_ballot(slot_index)
    }

    /// Get the externalized value for a slot.
    ///
    /// # Returns
    /// The externalized value if consensus was reached, None otherwise.
    pub fn get_externalized_value(&self, slot_index: u64) -> Option<Value> {
        self.slots
            .read()
            .get(&slot_index)
            .and_then(|slot| slot.get_externalized_value().cloned())
    }

    /// Check if a slot is externalized.
    pub fn is_slot_externalized(&self, slot_index: u64) -> bool {
        self.slots
            .read()
            .get(&slot_index)
            .map(|slot| slot.is_externalized())
            .unwrap_or(false)
    }

    /// Check if a slot is fully validated.
    pub fn is_slot_fully_validated(&self, slot_index: u64) -> bool {
        self.slots
            .read()
            .get(&slot_index)
            .map(|slot| slot.is_fully_validated())
            .unwrap_or(false)
    }

    /// Force externalize a slot with a specific value.
    ///
    /// Used during catchup when applying historical ledgers.
    pub fn force_externalize(&self, slot_index: u64, value: Value) {
        let mut slots = self.slots.write();

        let slot = self.get_or_create_slot(&mut slots, slot_index);

        slot.force_externalize(value);
    }

    /// Purge old slots to free memory.
    ///
    /// Removes slots older than `max_slot_index`, but keeps `slot_to_keep`
    /// even if it's below the threshold.
    ///
    /// Matches stellar-core `SCP::purgeSlots(maxSlotIndex, slotToKeep)`.
    pub fn purge_slots(&self, max_slot_index: u64, slot_to_keep: Option<u64>) {
        self.slots.write().retain(|&slot_index, _| {
            slot_index >= max_slot_index || slot_to_keep == Some(slot_index)
        });
    }

    /// Get the number of active slots.
    pub fn slot_count(&self) -> usize {
        self.slots.read().len()
    }

    /// Get all active slot indices.
    pub fn active_slots(&self) -> Vec<u64> {
        self.slots.read().keys().copied().collect()
    }

    /// Get all envelopes for a specific slot.
    pub fn get_slot_envelopes(&self, slot_index: u64) -> Vec<ScpEnvelope> {
        let slots = self.slots.read();
        let Some(slot) = slots.get(&slot_index) else {
            return Vec::new();
        };

        slot.get_envelopes()
            .values()
            .flat_map(|envs| envs.iter().cloned())
            .collect()
    }

    /// Get the highest externalized slot.
    pub fn highest_externalized_slot(&self) -> Option<u64> {
        self.slots
            .read()
            .iter()
            .filter(|(_, slot)| slot.is_externalized())
            .map(|(&index, _)| index)
            .max()
    }

    /// Get the timeout duration for a nomination round.
    pub fn get_nomination_timeout(&self, round: u32) -> Duration {
        self.driver.compute_timeout(round, true)
    }

    /// Get the timeout duration for a ballot round.
    pub fn get_ballot_timeout(&self, round: u32) -> Duration {
        self.driver.compute_timeout(round, false)
    }

    /// Check if we've heard from a v-blocking set for a slot.
    ///
    /// A v-blocking set is a set of nodes that can block any quorum.
    /// If we've heard from a v-blocking set, it means we have significant
    /// network participation.
    ///
    /// # Arguments
    /// * `slot_index` - The slot to check
    ///
    /// # Returns
    /// True if we've heard from a v-blocking set.
    pub fn got_v_blocking(&self, slot_index: u64) -> bool {
        let slots = self.slots.read();
        let Some(slot) = slots.get(&slot_index) else {
            return false;
        };

        slot.got_v_blocking()
    }

    /// Get the cumulative statement count across all slots.
    ///
    /// This is used for monitoring the total number of SCP statements
    /// processed by this node.
    ///
    /// # Returns
    /// The total number of statements recorded across all slots.
    pub fn get_cumulative_statement_count(&self) -> usize {
        self.slots
            .read()
            .values()
            .map(|slot| slot.get_statement_count())
            .sum()
    }

    /// Get the latest messages that would be sent for a slot.
    ///
    /// Returns the latest envelopes for the slot that would be
    /// broadcast to peers.
    pub fn get_latest_messages_send(&self, slot_index: u64) -> Vec<ScpEnvelope> {
        let slots = self.slots.read();
        let Some(slot) = slots.get(&slot_index) else {
            return Vec::new();
        };

        slot.get_latest_messages_send()
    }

    /// Process slots in ascending order starting from a given slot.
    ///
    /// # Arguments
    /// * `from_slot` - The slot to start from
    /// * `f` - Callback function for each slot, receives (slot_index, slot). Return false to stop.
    ///
    /// # Returns
    /// True if iteration completed, false if stopped early by callback.
    pub fn process_slots_ascending_from<F>(&self, from_slot: u64, mut f: F) -> bool
    where
        F: FnMut(u64) -> bool,
    {
        let slots = self.slots.read();
        let mut indices: Vec<_> = slots.keys().copied().filter(|s| *s >= from_slot).collect();
        indices.sort_unstable();

        for slot_index in indices {
            if !f(slot_index) {
                return false;
            }
        }

        true
    }

    /// Process slots in descending order starting from a given slot.
    ///
    /// # Arguments
    /// * `from_slot` - The slot to start from (inclusive upper bound)
    /// * `f` - Callback function for each slot, receives slot_index. Return false to stop.
    ///
    /// # Returns
    /// True if iteration completed, false if stopped early by callback.
    pub fn process_slots_descending_from<F>(&self, from_slot: u64, mut f: F) -> bool
    where
        F: FnMut(u64) -> bool,
    {
        let slots = self.slots.read();
        let mut indices: Vec<_> = slots.keys().copied().filter(|s| *s <= from_slot).collect();
        indices.sort_unstable_by(|a, b| b.cmp(a));

        for slot_index in indices {
            if !f(slot_index) {
                return false;
            }
        }

        true
    }

    /// Get the latest message from a specific node across all slots.
    ///
    /// This returns the latest message (by slot index) from a given node.
    ///
    /// # Arguments
    /// * `node_id` - The node to get messages from
    ///
    /// # Returns
    /// The latest envelope from the node, if any.
    pub fn get_latest_message(&self, node_id: &NodeId) -> Option<ScpEnvelope> {
        let slots = self.slots.read();
        let mut latest: Option<(u64, ScpEnvelope)> = None;

        for (&slot_index, slot) in slots.iter() {
            if let Some(env) = slot.get_latest_envelope(node_id) {
                if latest
                    .as_ref()
                    .map(|(idx, _)| slot_index > *idx)
                    .unwrap_or(true)
                {
                    latest = Some((slot_index, env.clone()));
                }
            }
        }

        latest.map(|(_, env)| env)
    }

    /// Get the externalizing state for a slot.
    ///
    /// Returns envelopes that contribute to the externalized state
    /// of a slot, if it has been externalized.
    ///
    /// # Arguments
    /// * `slot_index` - The slot to get state for
    ///
    /// # Returns
    /// A vector of envelopes contributing to externalization.
    pub fn get_externalizing_state(&self, slot_index: u64) -> Vec<ScpEnvelope> {
        let slots = self.slots.read();
        let Some(slot) = slots.get(&slot_index) else {
            return Vec::new();
        };

        slot.get_externalizing_state()
    }

    /// Cleanup old slots, keeping only the most recent ones.
    fn cleanup_old_slots(&self, slots: &mut HashMap<u64, Slot>) {
        if slots.len() <= self.max_slots {
            return;
        }

        // Find the minimum slot to keep
        let mut indices: Vec<_> = slots.keys().copied().collect();
        indices.sort_unstable();

        let to_remove = indices.len() - self.max_slots;
        for index in indices.into_iter().take(to_remove) {
            slots.remove(&index);
        }
    }

    /// Get slot state for debugging.
    pub fn get_slot_state(&self, slot_index: u64) -> Option<SlotState> {
        self.slots.read().get(&slot_index).map(|slot| SlotState {
            slot_index,
            is_externalized: slot.is_externalized(),
            is_nominating: slot.is_nominating(),
            heard_from_quorum: slot.heard_from_quorum(),
            ballot_phase: slot.ballot_phase(),
            nomination_round: slot.nomination().round(),
            ballot_round: slot.ballot_counter(),
            fully_validated: Some(slot.is_fully_validated()),
        })
    }

    /// Restore state from a saved envelope (for crash recovery).
    ///
    /// This method is used to restore SCP state from a previously saved envelope
    /// when restarting after a crash. It creates the slot if needed and restores
    /// the state from the envelope.
    ///
    /// # Arguments
    /// * `envelope` - The envelope to restore state from
    ///
    /// # Returns
    /// True if state was successfully restored, false if the envelope is invalid.
    pub fn set_state_from_envelope(&self, envelope: &ScpEnvelope) -> bool {
        let slot_index = envelope.statement.slot_index;
        let mut slots = self.slots.write();

        let slot = self.get_or_create_slot(&mut slots, slot_index);

        slot.set_state_from_envelope(envelope)
    }

    /// Abandon the current ballot for a slot.
    ///
    /// This is used when we need to give up on the current ballot,
    /// for example when we detect that consensus cannot be reached.
    ///
    /// # Arguments
    /// * `slot_index` - The slot to abandon
    /// * `counter` - The counter for the new ballot (0 to auto-increment)
    ///
    /// # Returns
    /// True if the ballot was abandoned successfully.
    pub fn abandon_ballot(&self, slot_index: u64, counter: u32) -> bool {
        if let Some(slot) = self.slots.write().get_mut(&slot_index) {
            slot.abandon_ballot(&self.driver, counter)
        } else {
            false
        }
    }

    /// Bump the ballot for a slot to a specific counter value.
    ///
    /// # Arguments
    /// * `slot_index` - The slot to bump
    /// * `value` - The value for the ballot
    /// * `counter` - The specific counter to bump to
    ///
    /// # Returns
    /// True if the ballot was bumped, false if the operation failed.
    pub fn bump_state(&self, slot_index: u64, value: Value, counter: u32) -> bool {
        let mut slots = self.slots.write();
        let slot = self.get_or_create_slot(&mut slots, slot_index);
        slot.bump_state(&self.driver, value, counter)
    }

    /// Force-bump the ballot state for a slot, auto-computing the counter.
    ///
    /// This mirrors the stellar-core `bumpState(slotIndex, value)` which calls
    /// `BallotProtocol::bumpState(value, force=true)`. The counter is
    /// automatically set to `current_counter + 1` (or 1 if no current ballot).
    ///
    /// Creates the slot if it doesn't already exist.
    ///
    /// # Arguments
    /// * `slot_index` - The slot to bump
    /// * `value` - The value for the ballot
    ///
    /// # Returns
    /// True if the ballot was bumped, false if the operation failed.
    pub fn force_bump_state(&self, slot_index: u64, value: Value) -> bool {
        let mut slots = self.slots.write();
        let slot = self.get_or_create_slot(&mut slots, slot_index);
        slot.force_bump_state(&self.driver, value)
    }

    /// Get nodes that are missing from consensus for a slot.
    ///
    /// Returns the set of nodes in our quorum set that we haven't
    /// heard from for the given slot. This is useful for diagnosing
    /// why consensus might be stuck.
    ///
    /// # Arguments
    /// * `slot_index` - The slot to check
    ///
    /// # Returns
    /// Set of node IDs that haven't sent messages for this slot.
    pub fn get_missing_nodes(&self, slot_index: u64) -> std::collections::HashSet<NodeId> {
        let all_nodes = crate::quorum::get_all_nodes(&self.local_quorum_set);
        let slots = self.slots.read();

        if let Some(slot) = slots.get(&slot_index) {
            let heard_from: std::collections::HashSet<NodeId> =
                slot.ballot().latest_envelopes().keys().cloned().collect();

            all_nodes.difference(&heard_from).cloned().collect()
        } else {
            all_nodes
        }
    }

    /// Check if a statement is newer than what we have for that node.
    ///
    /// This compares the given statement against the latest statement
    /// we have from the same node, returning true if the new statement
    /// represents progress.
    ///
    /// # Arguments
    /// * `slot_index` - The slot to check
    /// * `statement` - The statement to compare
    ///
    /// # Returns
    /// True if the statement is newer than our current state for that node.
    pub fn is_newer_statement(&self, slot_index: u64, statement: &ScpStatement) -> bool {
        let slots = self.slots.read();
        if let Some(slot) = slots.get(&slot_index) {
            match &statement.pledges {
                ScpStatementPledges::Nominate(_) => slot
                    .nomination()
                    .is_newer_statement(&statement.node_id, statement),
                _ => slot
                    .ballot()
                    .is_newer_statement(&statement.node_id, statement),
            }
        } else {
            true // No slot means any statement is "newer"
        }
    }

    /// Get SCP envelopes for recent slots.
    ///
    /// Returns envelopes for slots starting from `from_slot` up to the current slot.
    /// This is used to respond to GetScpState requests from peers.
    pub fn get_scp_state(&self, from_slot: u64) -> Vec<ScpEnvelope> {
        let slots = self.slots.read();
        let mut envelopes = Vec::new();

        let mut indices: Vec<_> = slots.keys().copied().filter(|s| *s >= from_slot).collect();
        indices.sort_unstable();

        for slot_index in indices {
            if let Some(slot) = slots.get(&slot_index) {
                slot.process_current_state(
                    |envelope| {
                        envelopes.push(envelope.clone());
                        true
                    },
                    false,
                );
            }
        }

        envelopes
    }

    /// Get ALL current envelopes for a slot, including self even when not fully validated.
    /// This matches stellar-core `getEntireCurrentState()` / `getCurrentEnvelope()` pattern.
    pub fn get_entire_current_state(&self, slot_index: u64) -> Vec<ScpEnvelope> {
        let slots = self.slots.read();
        let mut envelopes = Vec::new();

        if let Some(slot) = slots.get(&slot_index) {
            slot.process_current_state(
                |envelope| {
                    envelopes.push(envelope.clone());
                    true
                },
                true,
            );
        }

        envelopes
    }

    /// Get the nomination leaders for a slot.
    ///
    /// Returns the set of nodes that are leaders for the current nomination round.
    /// Matches stellar-core `getNominationLeaders(slotIndex)` on the TestSCP wrapper.
    pub fn get_nomination_leaders(&self, slot_index: u64) -> std::collections::BTreeSet<NodeId> {
        let slots = self.slots.read();
        slots
            .get(&slot_index)
            .map(|slot| slot.get_nomination_leaders())
            .unwrap_or_default()
    }

    /// Get the latest composite candidate value for a slot.
    ///
    /// Returns the most recently computed composite value from the nomination protocol.
    /// Matches stellar-core `getLatestCompositeCandidate(slotIndex)`.
    pub fn get_latest_composite_candidate(&self, slot_index: u64) -> Option<Value> {
        let slots = self.slots.read();
        slots
            .get(&slot_index)
            .and_then(|slot| slot.get_latest_composite_candidate())
    }

    /// Get JSON-serializable information for a slot.
    ///
    /// Returns slot info that can be serialized to JSON for debugging
    /// and monitoring, matching stellar-core `getJsonInfo()`.
    ///
    /// # Arguments
    /// * `slot_index` - The slot to get info for
    ///
    /// # Returns
    /// SlotInfo if the slot exists, None otherwise.
    pub fn get_info(&self, slot_index: u64) -> Option<crate::SlotInfo> {
        let slots = self.slots.read();
        slots.get(&slot_index).map(|slot| slot.get_info())
    }

    /// Get JSON-serializable quorum information for a slot.
    ///
    /// Returns quorum info that can be serialized to JSON for debugging
    /// and monitoring, matching stellar-core `getJsonQuorumInfo()`.
    ///
    /// # Arguments
    /// * `slot_index` - The slot to get quorum info for
    ///
    /// # Returns
    /// QuorumInfo if the slot exists, None otherwise.
    pub fn get_quorum_info(&self, slot_index: u64) -> Option<crate::QuorumInfo> {
        let slots = self.slots.read();
        slots.get(&slot_index).map(|slot| slot.get_quorum_info())
    }

    /// Get JSON-serializable quorum information for a specific node in a slot.
    ///
    /// This returns information about a specific node's state in the consensus
    /// process, matching stellar-core `getJsonQuorumInfo(NodeID const& id, ...)`.
    ///
    /// # Arguments
    /// * `slot_index` - The slot to query
    /// * `node_id` - The node to get info for
    ///
    /// # Returns
    /// NodeInfo if the slot exists and we have information about the node,
    /// None otherwise.
    pub fn get_quorum_info_for_node(
        &self,
        slot_index: u64,
        node_id: &NodeId,
    ) -> Option<crate::NodeInfo> {
        let slots = self.slots.read();
        let slot = slots.get(&slot_index)?;

        // Check nomination protocol first
        let nom_state = slot.nomination().get_node_state(node_id);
        if nom_state != crate::QuorumInfoNodeState::Missing {
            return Some(crate::NodeInfo {
                state: format!("{:?}", nom_state),
                ballot_counter: None,
            });
        }

        // Check ballot protocol
        let ballot_state = slot.ballot().get_node_state(node_id);
        let ballot_counter =
            slot.ballot()
                .latest_envelopes()
                .get(node_id)
                .and_then(|env| match &env.statement.pledges {
                    stellar_xdr::curr::ScpStatementPledges::Prepare(p) => Some(p.ballot.counter),
                    stellar_xdr::curr::ScpStatementPledges::Confirm(c) => Some(c.ballot.counter),
                    stellar_xdr::curr::ScpStatementPledges::Externalize(e) => {
                        Some(e.commit.counter)
                    }
                    _ => None,
                });

        Some(crate::NodeInfo {
            state: format!("{:?}", ballot_state),
            ballot_counter,
        })
    }

    /// Get JSON-serializable information for all active slots.
    ///
    /// Returns a vector of SlotInfo for all slots currently tracked.
    pub fn get_all_slot_info(&self) -> Vec<crate::SlotInfo> {
        let slots = self.slots.read();
        let mut infos: Vec<_> = slots.values().map(|slot| slot.get_info()).collect();
        infos.sort_by_key(|info| info.slot_index);
        infos
    }
}

/// Summary of a slot's consensus state for debugging and monitoring.
///
/// This struct provides a snapshot of the key state indicators for a slot,
/// useful for debugging, logging, and monitoring consensus progress.
#[derive(Debug, Clone)]
pub struct SlotState {
    /// The slot index (typically corresponds to ledger sequence number).
    pub slot_index: u64,

    /// Whether this slot has externalized (reached consensus).
    ///
    /// Once externalized, the slot's value is final and will not change.
    pub is_externalized: bool,

    /// Whether the nomination phase is currently active.
    ///
    /// Nomination stops when a composite value is produced or when
    /// explicitly stopped to transition to the ballot phase.
    pub is_nominating: bool,

    /// Whether we've heard from a quorum for the current ballot.
    ///
    /// This indicates whether sufficient nodes have voted on the
    /// current ballot to potentially make progress.
    pub heard_from_quorum: bool,

    /// The current phase of the ballot protocol.
    ///
    /// Progresses from Prepare -> Confirm -> Externalize.
    pub ballot_phase: crate::ballot::BallotPhase,

    /// The current nomination round number.
    ///
    /// Increases each time nomination times out or is restarted.
    pub nomination_round: u32,

    /// The current ballot counter, if a ballot is active.
    ///
    /// The ballot counter increases on timeouts to help the network
    /// converge on a common ballot.
    pub ballot_round: Option<u32>,

    /// Whether this slot is fully validated (not `MaybeValid`).
    ///
    /// A fully validated slot will broadcast its envelopes to peers.
    /// This is `None` if the information is unavailable.
    pub fully_validated: Option<bool>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::driver::ValidationLevel;
    use crate::quorum::hash_quorum_set;
    use crate::test_utils::{
        make_node_id, make_quorum_set, make_value, MockDriver, MockDriverBuilder, TimeoutMode,
        ValueHashMode,
    };
    use std::sync::atomic::Ordering;
    use std::time::Duration;
    use stellar_xdr::curr::{
        ScpBallot, ScpNomination, ScpStatement, ScpStatementPledges, ScpStatementPrepare,
    };

    /// Convenience: empty quorum set for scp.rs tests that don't need quorum
    /// logic.
    fn make_empty_quorum_set() -> ScpQuorumSet {
        ScpQuorumSet {
            threshold: 1,
            validators: vec![].try_into().unwrap(),
            inner_sets: vec![].try_into().unwrap(),
        }
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

    fn make_nomination_envelope(
        node_id: NodeId,
        slot_index: u64,
        quorum_set: &ScpQuorumSet,
        value: Value,
    ) -> ScpEnvelope {
        let nomination = ScpNomination {
            quorum_set_hash: hash_quorum_set(quorum_set).into(),
            votes: vec![value].try_into().unwrap(),
            accepted: vec![].try_into().unwrap(),
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

    #[test]
    fn test_scp_new() {
        let driver = Arc::new(MockDriver::bare());
        let scp = SCP::new(make_node_id(1), true, make_empty_quorum_set(), driver);

        assert!(scp.is_validator());
        assert_eq!(scp.slot_count(), 0);
    }

    #[test]
    fn test_force_externalize() {
        let driver = Arc::new(MockDriver::bare());
        let scp = SCP::new(make_node_id(1), true, make_empty_quorum_set(), driver);

        let value: Value = vec![1, 2, 3].try_into().unwrap();
        scp.force_externalize(42, value.clone());

        assert!(scp.is_slot_externalized(42));
        assert_eq!(scp.get_externalized_value(42), Some(value));
    }

    #[test]
    fn test_get_scp_state_skips_self_when_not_fully_validated() {
        let node_a = make_node_id(1);
        let node_b = make_node_id(2);
        let node_c = make_node_id(3);
        // Use threshold=2 so that a single PREPARE from node_b doesn't form
        // a quorum for federated_accept (which would cascade to externalization
        // and set fully_validated=true, defeating the purpose of this test).
        let quorum_set = make_quorum_set(vec![node_a.clone(), node_b.clone(), node_c.clone()], 2);
        let driver = Arc::new(
            MockDriverBuilder::new()
                .quorum_set(quorum_set.clone())
                .validation_level(ValidationLevel::MaybeValid)
                .value_hash_mode(ValueHashMode::Fixed(1))
                .timeout_mode(TimeoutMode::Linear {
                    base: Duration::from_secs(1),
                    step: Duration::from_secs(1),
                })
                .build(),
        );
        let scp = SCP::new(node_a.clone(), true, quorum_set.clone(), driver.clone());

        let ballot = ScpBallot {
            counter: 1,
            value: make_value(&[7]),
        };
        let env_b = make_prepare_envelope(node_b, 1, &quorum_set, ballot);
        scp.receive_envelope(env_b);

        let value = make_value(&[1, 2, 3]);
        let prev = make_value(&[0]);
        scp.nominate(1, value, &prev);

        let envelopes = scp.get_scp_state(1);
        assert!(!envelopes.is_empty());
        assert!(envelopes.iter().all(|env| env.statement.node_id != node_a));
        assert_eq!(driver.emit_count.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn test_get_scp_state_includes_self_when_fully_validated() {
        let node_a = make_node_id(1);
        let quorum_set = make_quorum_set(vec![node_a.clone()], 1);
        let driver = Arc::new(MockDriver::bare());
        let scp = SCP::new(node_a.clone(), true, quorum_set.clone(), driver);

        let value = make_value(&[4, 5, 6]);
        let prev = make_value(&[1]);
        scp.nominate(1, value, &prev);

        let envelopes = scp.get_scp_state(1);
        assert!(envelopes.iter().any(|env| env.statement.node_id == node_a));
    }

    #[test]
    fn test_nominate_solo_quorum_starts_ballot_without_peer_envelopes() {
        let node = make_node_id(1);
        let quorum_set = make_quorum_set(vec![node.clone()], 1);
        let driver = Arc::new(
            MockDriverBuilder::new()
                .quorum_set(quorum_set.clone())
                .value_hash_mode(ValueHashMode::Fixed(1))
                .timeout_mode(TimeoutMode::Linear {
                    base: Duration::from_secs(1),
                    step: Duration::from_secs(1),
                })
                .build(),
        );
        let scp = SCP::new(node, true, quorum_set, driver);

        let value = make_value(&[9, 9, 9]);
        let prev = make_value(&[0]);

        assert!(scp.nominate(1, value.clone(), &prev));
        let slot_state = scp.get_slot_state(1).expect("slot should exist");
        assert!(
            slot_state.ballot_round.is_some(),
            "solo quorum should transition to ballot after nominate"
        );
    }

    #[test]
    fn test_get_scp_state_orders_by_node_id() {
        let node_a = make_node_id(1);
        let node_b = make_node_id(2);
        let quorum_set = make_quorum_set(vec![node_a.clone(), node_b.clone()], 1);
        let driver = Arc::new(MockDriver::bare());
        let scp = SCP::new(node_a.clone(), true, quorum_set.clone(), driver);

        let value_a = make_value(&[1]);
        let value_b = make_value(&[2]);
        let env_b = make_nomination_envelope(node_b.clone(), 1, &quorum_set, value_b);
        let env_a = make_nomination_envelope(node_a.clone(), 1, &quorum_set, value_a);
        scp.receive_envelope(env_b);
        scp.receive_envelope(env_a);

        let envelopes = scp.get_scp_state(1);
        assert!(envelopes.len() >= 2);
        assert!(envelopes[0].statement.node_id <= envelopes[1].statement.node_id);
    }

    #[test]
    fn test_get_scp_state_orders_by_slot() {
        let node_a = make_node_id(1);
        let node_b = make_node_id(2);
        let quorum_set = make_quorum_set(vec![node_a.clone(), node_b.clone()], 1);
        let driver = Arc::new(MockDriver::bare());
        let scp = SCP::new(node_a.clone(), true, quorum_set.clone(), driver);

        let env_slot2 = make_nomination_envelope(node_b.clone(), 2, &quorum_set, make_value(&[2]));
        let env_slot1 = make_nomination_envelope(node_a.clone(), 1, &quorum_set, make_value(&[1]));
        scp.receive_envelope(env_slot2);
        scp.receive_envelope(env_slot1);

        let envelopes = scp.get_scp_state(1);
        assert!(envelopes.len() >= 2);
        assert!(envelopes[0].statement.slot_index <= envelopes[1].statement.slot_index);
    }

    #[test]
    fn test_purge_slots() {
        let driver = Arc::new(MockDriver::bare());
        let scp = SCP::new(make_node_id(1), true, make_empty_quorum_set(), driver);

        // Create some slots
        for i in 1..=10 {
            let value: Value = vec![i as u8].try_into().unwrap();
            scp.force_externalize(i, value);
        }

        assert_eq!(scp.slot_count(), 10);

        // Purge old slots
        scp.purge_slots(6, None);

        assert_eq!(scp.slot_count(), 5);
        assert!(scp.get_externalized_value(5).is_none());
        assert!(scp.get_externalized_value(6).is_some());
    }

    // ==================== Tests for new parity features ====================

    #[test]
    fn test_got_v_blocking() {
        let node_a = make_node_id(1);
        let node_b = make_node_id(2);
        let quorum_set = ScpQuorumSet {
            threshold: 2,
            validators: vec![node_a.clone(), node_b.clone()].try_into().unwrap(),
            inner_sets: vec![].try_into().unwrap(),
        };
        let driver = Arc::new(
            MockDriverBuilder::new()
                .quorum_set(quorum_set.clone())
                .validation_level(ValidationLevel::MaybeValid)
                .value_hash_mode(ValueHashMode::Fixed(1))
                .timeout_mode(TimeoutMode::Linear {
                    base: Duration::from_secs(1),
                    step: Duration::from_secs(1),
                })
                .build(),
        );
        let scp = SCP::new(node_a.clone(), true, quorum_set.clone(), driver);

        // No slot created yet
        assert!(!scp.got_v_blocking(1));

        // Force externalize to create a slot
        let value: Value = vec![1, 2, 3].try_into().unwrap();
        scp.force_externalize(1, value);

        // No envelopes yet, so no v-blocking
        // (Note: got_v_blocking checks ballot envelopes, not nomination)
        assert!(!scp.got_v_blocking(1));
    }

    #[test]
    fn test_get_cumulative_statement_count() {
        let node_a = make_node_id(1);
        let quorum_set = make_empty_quorum_set();
        let driver = Arc::new(MockDriver::bare());
        let scp = SCP::new(node_a.clone(), true, quorum_set.clone(), driver);

        // No slots, count should be 0
        assert_eq!(scp.get_cumulative_statement_count(), 0);

        // Force externalize to create slots
        for i in 1..=3 {
            let value: Value = vec![i as u8].try_into().unwrap();
            scp.force_externalize(i, value);
        }

        // Slots exist but might have no statements (depends on implementation)
        let _count = scp.get_cumulative_statement_count(); // Just verify it doesn't panic
    }

    #[test]
    fn test_get_missing_nodes() {
        let node_a = make_node_id(1);
        let node_b = make_node_id(2);
        let node_c = make_node_id(3);
        let quorum_set = ScpQuorumSet {
            threshold: 2,
            validators: vec![node_a.clone(), node_b.clone(), node_c.clone()]
                .try_into()
                .unwrap(),
            inner_sets: vec![].try_into().unwrap(),
        };
        let driver = Arc::new(
            MockDriverBuilder::new()
                .quorum_set(quorum_set.clone())
                .validation_level(ValidationLevel::MaybeValid)
                .value_hash_mode(ValueHashMode::Fixed(1))
                .timeout_mode(TimeoutMode::Linear {
                    base: Duration::from_secs(1),
                    step: Duration::from_secs(1),
                })
                .build(),
        );
        let scp = SCP::new(node_a.clone(), true, quorum_set.clone(), driver);

        // No slot yet - all nodes should be missing
        let missing = scp.get_missing_nodes(1);
        assert!(missing.contains(&node_a));
        assert!(missing.contains(&node_b));
        assert!(missing.contains(&node_c));
    }

    #[test]
    fn test_is_newer_statement() {
        let node_a = make_node_id(1);
        let quorum_set = make_empty_quorum_set();
        let driver = Arc::new(MockDriver::bare());
        let scp = SCP::new(node_a.clone(), true, quorum_set.clone(), driver);

        // Create a nomination statement
        let nom = ScpNomination {
            quorum_set_hash: hash_quorum_set(&quorum_set).into(),
            votes: vec![make_value(&[1])].try_into().unwrap(),
            accepted: vec![].try_into().unwrap(),
        };
        let statement = ScpStatement {
            node_id: node_a.clone(),
            slot_index: 1,
            pledges: ScpStatementPledges::Nominate(nom),
        };

        // No slot exists yet, any statement is "newer"
        assert!(scp.is_newer_statement(1, &statement));
    }

    #[test]
    fn test_empty() {
        let driver = Arc::new(MockDriver::bare());
        let scp = SCP::new(make_node_id(1), true, make_empty_quorum_set(), driver);

        // New SCP should be empty
        assert!(scp.empty());

        // After force_externalize, should not be empty
        let value: Value = vec![1, 2, 3].try_into().unwrap();
        scp.force_externalize(1, value);
        assert!(!scp.empty());
    }

    #[test]
    fn test_get_highest_known_slot() {
        let driver = Arc::new(MockDriver::bare());
        let scp = SCP::new(make_node_id(1), true, make_empty_quorum_set(), driver);

        // No slots initially
        assert_eq!(scp.get_highest_known_slot(), None);

        // Add some slots
        for i in [5, 2, 8, 3] {
            let value: Value = vec![i as u8].try_into().unwrap();
            scp.force_externalize(i, value);
        }

        // Should return the highest slot
        assert_eq!(scp.get_highest_known_slot(), Some(8));
    }

    #[test]
    fn test_driver_access() {
        let driver = Arc::new(MockDriver::bare());
        let scp = SCP::new(
            make_node_id(1),
            true,
            make_empty_quorum_set(),
            driver.clone(),
        );

        // Should be able to access the driver
        let retrieved = scp.driver();
        assert!(Arc::ptr_eq(&driver, retrieved));
    }
}
