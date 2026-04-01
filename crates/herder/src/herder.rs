//! Main Herder implementation.
//!
//! The Herder is the central coordinator that drives consensus and manages
//! the transition between ledgers. It integrates with:
//!
//! - **SCP**: For Byzantine fault-tolerant consensus
//! - **Overlay**: For network communication (receiving transactions and SCP envelopes)
//! - **Ledger**: For state management and validation
//! - **Transaction processing**: Managing the pending transaction queue
//!
//! # Architecture
//!
//! The Herder owns several key components:
//!
//! - [`TransactionQueue`]: Pending transactions waiting for consensus
//! - [`PendingEnvelopes`]: SCP envelopes for future slots
//! - [`ScpDriver`]: Callbacks for SCP consensus
//! - [`SCP`]: The consensus protocol instance (validators only)
//!
//! # Operating Modes
//!
//! The Herder operates in two modes:
//!
//! - **Observer mode**: Tracks consensus by observing EXTERNALIZE messages from
//!   validators in the quorum. Does not vote or propose values.
//! - **Validator mode**: Actively participates in consensus by proposing transaction
//!   sets and voting. Requires a secret key and quorum set configuration.
//!
//! # Security: EXTERNALIZE Validation
//!
//! EXTERNALIZE messages can fast-forward a node's tracking slot, which is necessary
//! for catching up to the network. To prevent attacks, two security checks are applied:
//!
//! 1. **Quorum membership**: Sender must be in our transitive quorum set
//! 2. **Slot distance limit**: Slot must be within [`MAX_EXTERNALIZE_SLOT_DISTANCE`] of current

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use parking_lot::RwLock;
use tracing::{debug, error, info, warn};

use henyey_common::Hash256;
use henyey_crypto::{PublicKey, SecretKey};
use henyey_ledger::LedgerManager;
use henyey_scp::{BallotPhase, SlotIndex, SCP};
use stellar_xdr::curr::{
    EnvelopeType, LedgerCloseValueSignature, LedgerUpgrade, Limits, NodeId, ReadXdr, ScpEnvelope,
    ScpQuorumSet, Signature as XdrSignature, StellarValue, StellarValueExt, TimePoint,
    TransactionEnvelope, Uint256, UpgradeType, Value, WriteXdr,
};

use crate::error::HerderError;
use crate::fetching_envelopes::{FetchingEnvelopes, FetchingStats};
use crate::herder_utils::to_short_string;
use crate::pending::{PendingConfig, PendingEnvelopes, PendingResult, PendingStats};
use crate::quorum_tracker::{QuorumTracker, SlotQuorumTracker};
use crate::scp_driver::{HerderScpCallback, ScpDriver, ScpDriverConfig};
use crate::state::HerderState;
use crate::tx_queue::{
    account_key_from_account_id, TransactionQueue, TransactionSet, TxQueueConfig, TxQueueResult,
    TxQueueStats,
};
use crate::upgrades::{CurrentLedgerState, UpgradeParameters, Upgrades};
use crate::Result;

/// Maximum slot distance for accepting EXTERNALIZE messages.
///
/// Maximum number of slots to accept behind the current tracking slot.
///
/// This matches the stellar-core MAX_SLOTS_TO_REMEMBER constant. Slots
/// within this window of the tracking slot are accepted for processing.
/// This allows the node to catch up on recent slots after catchup without
/// rejecting valid SCP envelopes as "too old".
const MAX_SLOTS_TO_REMEMBER: u64 = 12;

/// Maximum time slip allowed for close times in SCP values (in seconds).
///
/// Matches stellar-core `Herder::MAX_TIME_SLIP_SECONDS = 60`. A proposed close time
/// can be at most 60 seconds ahead of the current wall-clock time.
const MAX_TIME_SLIP_SECONDS: u64 = 60;

/// Maximum ledger close time drift for envelope recency filtering (in seconds).
///
/// Matches stellar-core `Config::MAXIMUM_LEDGER_CLOSETIME_DRIFT`. When filtering
/// incoming SCP envelopes for recency (during initial boot before any sync),
/// close times must be within this window of the current wall-clock time.
/// stellar-core computes this as `min((MAX_SLOTS_TO_REMEMBER + 2) * TARGET_CLOSE_TIME / 1000, 90)`.
/// With default values: `min(14 * 5, 90) = 70` seconds.
const MAXIMUM_LEDGER_CLOSETIME_DRIFT: u64 = 70;

/// Maximum number of ledgers ahead of tracking that we accept SCP messages for.
///
/// Matches stellar-core `Herder::LEDGER_VALIDITY_BRACKET = 100`. When tracking consensus,
/// we reject envelopes for slots more than this many ahead of our next consensus
/// ledger index.
const LEDGER_VALIDITY_BRACKET: u64 = 100;

/// Genesis ledger sequence number.
///
/// Matches stellar-core `LedgerManager::GENESIS_LEDGER_SEQ = 1`.
const GENESIS_LEDGER_SEQ: u64 = 1;

/// Default checkpoint frequency (64 ledgers).
const DEFAULT_CHECKPOINT_FREQUENCY: u64 = 64;

/// Result of receiving an SCP envelope.
///
/// Indicates what happened when the Herder processed an incoming SCP envelope.
/// The caller can use this to decide whether to broadcast the envelope to peers
/// or take other actions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnvelopeState {
    /// Envelope was processed successfully and is new.
    Valid,
    /// Envelope is for a future slot and was buffered for later processing.
    Pending,
    /// Envelope is waiting for tx set to be fetched from peers.
    Fetching,
    /// Envelope was already seen/processed.
    Duplicate,
    /// Envelope is for a slot older than our current tracking slot.
    TooOld,
    /// Envelope has an invalid cryptographic signature.
    InvalidSignature,
    /// Envelope is structurally invalid or failed validation.
    Invalid,
}

/// Configuration for the Herder.
///
/// Controls the Herder's behavior including validator mode, queue limits,
/// quorum configuration, and timing parameters.
#[derive(Debug, Clone)]
pub struct HerderConfig {
    /// Maximum number of pending transactions in the queue.
    pub max_pending_transactions: usize,

    /// Whether this node should participate in consensus as a validator.
    ///
    /// When `true`, the node will propose values and vote in SCP.
    /// Requires `local_quorum_set` to be configured and a secret key to be provided.
    pub is_validator: bool,

    /// Target ledger close time in seconds (typically 5 for Stellar mainnet).
    pub ledger_close_time: u32,

    /// Our node's public key (derived from secret key for validators).
    pub node_public_key: PublicKey,

    /// Network ID hash (unique per network: mainnet, testnet, etc.).
    pub network_id: Hash256,

    /// Maximum number of externalized slots to keep in memory.
    ///
    /// Older slots are cleaned up to prevent unbounded memory growth.
    pub max_externalized_slots: usize,

    /// Configuration for the pending envelope buffer.
    pub pending_config: PendingConfig,

    /// Configuration for the transaction queue.
    pub tx_queue_config: TxQueueConfig,

    /// Local quorum set for consensus (required for validators).
    ///
    /// Defines which validators we trust and the threshold requirements.
    pub local_quorum_set: Option<ScpQuorumSet>,

    /// Maximum operations allowed in a transaction set.
    pub max_tx_set_size: usize,

    /// Protocol upgrades this validator proposes to include in nominations.
    pub proposed_upgrades: Vec<stellar_xdr::curr::LedgerUpgrade>,

    /// Maximum supported protocol version for upgrade validation.
    pub max_protocol_version: u32,

    /// Checkpoint frequency in ledgers (default 64, 8 for accelerated testing).
    pub checkpoint_frequency: u64,
}

const DEFAULT_MAX_EXTERNALIZED_SLOTS: usize = 12;

impl Default for HerderConfig {
    fn default() -> Self {
        Self {
            max_pending_transactions: 1000,
            is_validator: false,
            ledger_close_time: 5,
            node_public_key: PublicKey::from_bytes(&[0u8; 32]).unwrap(),
            network_id: Hash256::ZERO,
            max_externalized_slots: DEFAULT_MAX_EXTERNALIZED_SLOTS,
            pending_config: PendingConfig::default(),
            tx_queue_config: TxQueueConfig::default(),
            local_quorum_set: None,
            max_tx_set_size: 1000,
            proposed_upgrades: Vec::new(),
            max_protocol_version: 25,
            checkpoint_frequency: DEFAULT_CHECKPOINT_FREQUENCY,
        }
    }
}

/// The main Herder that coordinates consensus.
///
/// This is the central component that bridges the overlay network and the ledger
/// manager through SCP consensus. It:
///
/// - Receives transactions from the network and queues them for inclusion
/// - Receives SCP envelopes and processes them through consensus
/// - Proposes transaction sets when acting as a validator
/// - Tracks externalized values and triggers ledger close
/// - Manages the state machine (Booting -> Syncing -> Tracking)
///
/// # Thread Safety
///
/// The Herder is designed to be shared across threads. Internal state is protected
/// by appropriate synchronization primitives (`RwLock`, `DashMap`).
///
/// # Example
///
/// ```ignore
/// use henyey_herder::{Herder, HerderConfig};
///
/// // Create a non-validator herder
/// let config = HerderConfig::default();
/// let herder = Herder::new(config);
///
/// // Start syncing when catchup begins
/// herder.start_syncing();
///
/// // Bootstrap after catchup completes
/// herder.bootstrap(ledger_seq);
///
/// // Process incoming SCP envelopes
/// let state = herder.receive_scp_envelope(envelope);
/// ```
pub struct Herder {
    /// Configuration.
    config: HerderConfig,
    /// Current state.
    state: RwLock<HerderState>,
    /// Transaction queue.
    tx_queue: TransactionQueue,
    /// Pending envelopes for future slots.
    pending_envelopes: PendingEnvelopes,
    /// Fetching envelopes waiting for TxSet/QuorumSet dependencies.
    fetching_envelopes: FetchingEnvelopes,
    /// SCP driver for consensus callbacks.
    scp_driver: Arc<ScpDriver>,
    /// SCP consensus protocol instance.
    scp: Option<SCP<HerderScpCallback>>,
    /// Current tracking slot (ledger sequence as u64).
    tracking_slot: RwLock<u64>,
    /// Consensus close time for the tracking slot.
    /// Matches stellar-core `mTrackingSCP.mConsensusCloseTime`.
    tracking_consensus_close_time: RwLock<u64>,
    /// When we started tracking.
    tracking_started_at: RwLock<Option<Instant>>,
    /// Secret key for signing (if validator).
    secret_key: Option<SecretKey>,
    /// Ledger manager reference (optional, for validation).
    ledger_manager: RwLock<Option<Arc<LedgerManager>>>,
    /// Previous externalized value (for priority calculation).
    prev_value: RwLock<Value>,
    /// Slot-level quorum tracker for heard-from quorum/v-blocking checks.
    slot_quorum_tracker: RwLock<SlotQuorumTracker>,
    /// Transitive quorum tracker for the current quorum map.
    quorum_tracker: RwLock<QuorumTracker>,
    /// Runtime-mutable upgrade scheduling (set via HTTP `/upgrades?mode=set`).
    runtime_upgrades: RwLock<Upgrades>,
}

impl Herder {
    /// Create a new Herder (observer mode, no secret key).
    pub fn new(config: HerderConfig) -> Self {
        Self::build(config, None)
    }

    /// Create a new Herder with a secret key for validation.
    pub fn with_secret_key(config: HerderConfig, secret_key: SecretKey) -> Self {
        Self::build(config, Some(secret_key))
    }

    /// Shared constructor logic for both observer and validator modes.
    fn build(config: HerderConfig, secret_key: Option<SecretKey>) -> Self {
        let mut pending_config = config.pending_config.clone();
        let max_slots = config.max_externalized_slots.max(1);
        pending_config.max_slots = pending_config.max_slots.min(max_slots);
        pending_config.max_slot_distance = pending_config.max_slot_distance.min(max_slots as u64);

        let scp_driver_config = ScpDriverConfig {
            node_id: config.node_public_key,
            max_tx_set_cache: 256,
            max_time_drift: MAX_TIME_SLIP_SECONDS,
            local_quorum_set: config.local_quorum_set.clone(),
        };

        let scp_driver = Arc::new(if let Some(ref sk) = secret_key {
            ScpDriver::with_secret_key(scp_driver_config, config.network_id, sk.clone())
        } else {
            ScpDriver::new(scp_driver_config, config.network_id)
        });

        let tx_queue = TransactionQueue::new(config.tx_queue_config.clone());
        let pending_envelopes = PendingEnvelopes::new(pending_config);
        let fetching_envelopes = FetchingEnvelopes::with_defaults();

        // Pre-cache the local quorum set in fetching_envelopes so envelopes
        // referencing it don't wait for fetching.
        if let Some(ref quorum_set) = config.local_quorum_set {
            let qs_hash = Hash256::hash_xdr(quorum_set).unwrap_or(Hash256::ZERO);
            fetching_envelopes.cache_quorum_set(qs_hash, quorum_set.clone());
        }

        // Create SCP instance for validators with a secret key.
        let node_id = node_id_from_public_key(&config.node_public_key);
        let scp = if secret_key.is_some() && config.is_validator {
            if let Some(ref quorum_set) = config.local_quorum_set {
                let callback = HerderScpCallback::new(Arc::clone(&scp_driver));
                Some(SCP::new(
                    node_id.clone(),
                    true, // is_validator
                    quorum_set.clone(),
                    Arc::new(callback),
                ))
            } else {
                warn!("Validator mode requested but no quorum set configured");
                None
            }
        } else {
            None
        };

        let slot_quorum_tracker =
            SlotQuorumTracker::new(config.local_quorum_set.clone(), max_slots);
        let mut quorum_tracker = QuorumTracker::new(node_id.clone());
        if let Some(ref quorum_set) = config.local_quorum_set {
            let mode = if secret_key.is_some() {
                "validator"
            } else {
                "observer"
            };
            info!(
                validators = quorum_set.validators.len(),
                threshold = quorum_set.threshold,
                mode,
                "Initializing quorum tracker with local quorum set"
            );
            for v in quorum_set.validators.iter() {
                let key_hex = match &v.0 {
                    stellar_xdr::curr::PublicKey::PublicKeyTypeEd25519(
                        stellar_xdr::curr::Uint256(bytes),
                    ) => hex::encode(bytes),
                };
                info!(validator = %key_hex, "Quorum set validator");
            }
            let _ = quorum_tracker.expand(&node_id, quorum_set.clone());
            info!(
                tracked_nodes = quorum_tracker.tracked_node_count(),
                "Quorum tracker initialized"
            );
        }

        Self {
            config,
            state: RwLock::new(HerderState::Booting),
            tx_queue,
            pending_envelopes,
            fetching_envelopes,
            scp_driver,
            scp,
            tracking_slot: RwLock::new(0),
            tracking_consensus_close_time: RwLock::new(0),
            tracking_started_at: RwLock::new(None),
            secret_key,
            ledger_manager: RwLock::new(None),
            prev_value: RwLock::new(Value::default()),
            slot_quorum_tracker: RwLock::new(slot_quorum_tracker),
            quorum_tracker: RwLock::new(quorum_tracker),
            runtime_upgrades: RwLock::new(Upgrades::default()),
        }
    }

    /// Set runtime upgrade parameters (called from HTTP `/upgrades?mode=set`).
    ///
    /// Returns an error string if validation fails (e.g., protocol version too high).
    pub fn set_upgrade_parameters(
        &self,
        params: UpgradeParameters,
    ) -> std::result::Result<(), String> {
        let max_protocol = self.config.max_protocol_version;
        let mut upgrades = self.runtime_upgrades.write();
        upgrades.set_parameters(params, max_protocol)
    }

    /// Get current runtime upgrade parameters.
    pub fn upgrade_parameters(&self) -> UpgradeParameters {
        self.runtime_upgrades.read().parameters().clone()
    }

    /// Set the ledger manager reference.
    pub fn set_ledger_manager(&self, manager: Arc<LedgerManager>) {
        self.scp_driver.set_ledger_manager(Arc::clone(&manager));
        *self.ledger_manager.write() = Some(manager);
    }

    /// Get the current state of the Herder.
    pub fn state(&self) -> HerderState {
        *self.state.read()
    }

    /// Set the Herder state.
    ///
    /// Validates the transition per HERDER_SPEC §3.2: TRACKING→BOOTING and
    /// SYNCING→BOOTING are forbidden.  If an invalid transition is attempted,
    /// it is logged and ignored.
    pub fn set_state(&self, state: HerderState) {
        let current = *self.state.read();
        if !current.can_transition_to(state) {
            tracing::warn!(
                from = %current,
                to = %state,
                "ignoring forbidden herder state transition"
            );
            return;
        }
        *self.state.write() = state;
    }

    /// Get the current tracking slot.
    pub fn tracking_slot(&self) -> u64 {
        *self.tracking_slot.read()
    }

    /// Get the tracking consensus close time.
    /// Matches stellar-core `HerderImpl::trackingConsensusCloseTime()`.
    pub fn tracking_consensus_close_time(&self) -> u64 {
        *self.tracking_consensus_close_time.read()
    }

    /// Get the next consensus ledger index.
    /// Matches stellar-core `HerderImpl::nextConsensusLedgerIndex()`.
    ///
    /// Equivalent to [`tracking_slot`](Self::tracking_slot); both read the same field.
    pub fn next_consensus_ledger_index(&self) -> u64 {
        self.tracking_slot()
    }

    /// Get the most recent checkpoint sequence.
    ///
    /// Matches stellar-core `HerderImpl::getMostRecentCheckpointSeq()` which returns
    /// `HistoryManager::firstLedgerInCheckpointContaining(trackingConsensusLedgerIndex())`.
    ///
    /// With checkpoint frequency 64:
    /// - ledger 1..63  → checkpoint starts at 1 (first checkpoint is size 63)
    /// - ledger 64..127 → checkpoint starts at 64
    /// - ledger 128..191 → checkpoint starts at 128
    pub fn get_most_recent_checkpoint_seq(&self) -> u64 {
        let tracking_consensus_index = self.tracking_slot().saturating_sub(1);
        let freq = self.config.checkpoint_frequency;
        // checkpointContainingLedger: ((ledger / freq + 1) * freq) - 1
        let last = ((tracking_consensus_index / freq) + 1) * freq - 1;
        // sizeOfCheckpointContaining: first checkpoint is special (size freq-1), rest are freq
        let size = if tracking_consensus_index < freq {
            freq - 1
        } else {
            freq
        };
        // firstLedgerInCheckpointContaining
        last - (size - 1)
    }

    /// Returns the minimum ledger sequence to keep in SCP memory.
    ///
    /// Messages for slots below this value can be safely dropped from queues.
    /// Matches upstream `HerderImpl::getMinLedgerSeqToRemember()`.
    pub fn get_min_ledger_seq_to_remember(&self) -> u64 {
        let current_slot = self.tracking_slot();
        if current_slot > MAX_SLOTS_TO_REMEMBER {
            current_slot - MAX_SLOTS_TO_REMEMBER + 1
        } else {
            1
        }
    }

    /// Compute the minimum ledger sequence to ask peers for SCP state.
    pub fn get_min_ledger_seq_to_ask_peers(&self) -> u32 {
        let lcl = self
            .ledger_manager
            .read()
            .as_ref()
            .map(|manager| manager.current_ledger_seq())
            .unwrap_or_else(|| self.tracking_slot().min(u32::MAX as u64) as u32);
        let mut low = lcl.saturating_add(1);
        let max_slots = self.config.max_externalized_slots.max(1) as u32;
        let extra = 3u32;
        let window = max_slots.min(extra);
        if low > window {
            low = low.saturating_sub(window);
        } else {
            low = 1;
        }
        low
    }

    /// Get the configured target ledger close time in seconds.
    pub fn ledger_close_time(&self) -> u32 {
        self.config.ledger_close_time
    }

    /// Get the maximum size of a transaction set (ops).
    pub fn max_tx_set_size(&self) -> usize {
        self.config.max_tx_set_size
    }

    /// Get the maximum queue size in ops for demand sizing.
    pub fn max_queue_size_ops(&self) -> usize {
        self.config.max_pending_transactions
    }

    /// Return the set of node IDs from the local quorum set (if configured).
    pub fn local_quorum_nodes(&self) -> std::collections::HashSet<stellar_xdr::curr::NodeId> {
        fn collect_nodes(
            quorum_set: &stellar_xdr::curr::ScpQuorumSet,
            acc: &mut std::collections::HashSet<stellar_xdr::curr::NodeId>,
        ) {
            for validator in quorum_set.validators.iter() {
                acc.insert(validator.clone());
            }
            for inner in quorum_set.inner_sets.iter() {
                collect_nodes(inner, acc);
            }
        }

        let mut nodes = std::collections::HashSet::new();
        if let Some(qs) = &self.config.local_quorum_set {
            collect_nodes(qs, &mut nodes);
        }
        nodes
    }

    /// Check if the herder is tracking consensus.
    pub fn is_tracking(&self) -> bool {
        self.state().is_tracking()
    }

    /// Check if this node is a validator.
    pub fn is_validator(&self) -> bool {
        self.config.is_validator && self.secret_key.is_some()
    }

    /// Store a quorum set for a peer node.
    pub fn store_quorum_set(&self, node_id: &NodeId, quorum_set: ScpQuorumSet) {
        self.scp_driver
            .store_quorum_set(node_id, quorum_set.clone());
        let mut tracker = self.quorum_tracker.write();
        if !tracker.expand(node_id, quorum_set) {
            if let Err(err) = tracker.rebuild(|id| self.scp_driver.get_quorum_set(id)) {
                warn!(error = %err, "Failed to rebuild quorum tracker");
            }
        }
    }

    /// Get a quorum set by hash if available.
    pub fn get_quorum_set_by_hash(&self, hash: &Hash256) -> Option<ScpQuorumSet> {
        self.scp_driver.get_quorum_set_by_hash(hash)
    }

    /// Whether we already have a quorum set with the given hash.
    pub fn has_quorum_set_hash(&self, hash: &Hash256) -> bool {
        self.scp_driver.has_quorum_set_hash(hash)
    }

    /// Register a quorum set request if needed.
    /// The node_id is the envelope sender that uses this quorum set.
    pub fn request_quorum_set(&self, hash: Hash256, node_id: NodeId) -> bool {
        self.scp_driver.request_quorum_set(hash, node_id)
    }

    /// Clear a quorum set request.
    pub fn clear_quorum_set_request(&self, hash: &Hash256) {
        self.scp_driver.clear_quorum_set_request(hash);
    }

    /// Get the node IDs that are waiting for a quorum set with the given hash.
    pub fn get_pending_quorum_set_node_ids(&self, hash: &Hash256) -> Vec<NodeId> {
        self.scp_driver.get_pending_quorum_set_node_ids(hash)
    }

    /// Check whether we've heard from quorum for a slot.
    pub fn heard_from_quorum(&self, slot: SlotIndex) -> bool {
        self.slot_quorum_tracker
            .read()
            .has_quorum(slot, |node_id| self.scp_driver.get_quorum_set(node_id))
    }

    /// Check whether we have a v-blocking set for a slot.
    pub fn is_v_blocking(&self, slot: SlotIndex) -> bool {
        self.slot_quorum_tracker.read().is_v_blocking(slot)
    }

    /// Get all slots that have achieved v-blocking status, sorted descending.
    pub fn get_v_blocking_slots(&self) -> Vec<SlotIndex> {
        self.slot_quorum_tracker.read().get_v_blocking_slots()
    }

    /// Perform out-of-sync recovery by purging old slots.
    ///
    /// This mirrors stellar-core's `outOfSyncRecovery()` function.
    /// When we're out of sync, we scan v-blocking slots from highest to lowest
    /// and purge all slots more than LEDGER_VALIDITY_BRACKET (100) behind
    /// the highest v-blocking slot.
    ///
    /// Returns the slot we purged below, or None if no purging was done.
    pub fn out_of_sync_recovery(&self, lcl: u64) -> Option<u64> {
        // Don't call this when tracking normally
        if self.state() == HerderState::Tracking {
            return None;
        }

        let v_blocking_slots = self.get_v_blocking_slots();
        if v_blocking_slots.is_empty() {
            return None;
        }

        // Find the slot to purge below: 100 slots behind the highest v-blocking slot
        let mut max_slots_ahead = LEDGER_VALIDITY_BRACKET;
        let mut purge_slot = None;

        for slot in v_blocking_slots {
            max_slots_ahead = max_slots_ahead.saturating_sub(1);
            if max_slots_ahead == 0 {
                purge_slot = Some(slot);
                break;
            }
        }

        if let Some(purge_slot) = purge_slot {
            info!(purge_slot, "Out-of-sync recovery: purging slots below");

            // Calculate slot_to_keep (for checkpoint preservation, keep last checkpoint)
            let freq = self.config.checkpoint_frequency;
            let last_checkpoint = (lcl / freq) * freq;

            self.fetching_envelopes
                .erase_below(purge_slot, last_checkpoint);

            // Clear slot quorum tracker entries below purge_slot
            self.slot_quorum_tracker
                .write()
                .clear_slots_below(purge_slot);

            // Purge SCP state
            if let Some(ref scp) = self.scp {
                scp.purge_slots(purge_slot.saturating_sub(1), None);
            }

            // Purge externalized values and pending tx set requests
            self.scp_driver.purge_slots_below(purge_slot);

            return Some(purge_slot);
        }

        None
    }

    /// Bootstrap the Herder after catchup.
    ///
    /// This transitions the Herder from Syncing to Tracking state,
    /// setting the next consensus ledger as the tracking slot.
    pub fn bootstrap(&self, ledger_seq: u32) {
        let lcl = ledger_seq as u64;
        let slot = lcl + 1;

        info!("Bootstrapping Herder at ledger {}", ledger_seq);

        // Update tracking slot
        *self.tracking_slot.write() = slot;
        *self.tracking_started_at.write() = Some(Instant::now());

        // Set tracking consensus close time from LCL if available
        // (matching stellar-core setTrackingSCPState which sets close time from externalized value)
        let close_time = self
            .ledger_manager
            .read()
            .as_ref()
            .map(|lm| lm.current_header().scp_value.close_time.0)
            .unwrap_or(0);
        *self.tracking_consensus_close_time.write() = close_time;

        // Update pending envelopes current slot
        self.pending_envelopes.set_current_slot(slot);

        // Transition to tracking state
        *self.state.write() = HerderState::Tracking;

        // Update ScpDriver with tracking state for close-time validation
        self.scp_driver.set_tracking_state(true, slot, close_time);

        // Release any pending envelopes for this slot and previous
        let pending = self.pending_envelopes.release_up_to(slot);
        for (pending_slot, envelopes) in pending {
            debug!(
                "Released {} pending envelopes for slot {}",
                envelopes.len(),
                pending_slot
            );
            for envelope in envelopes {
                // Process released envelopes (ignore result as they may be old)
                let _ = self.process_scp_envelope(envelope);
            }
        }

        info!(
            lcl,
            tracking_slot = slot,
            "Herder now tracking next consensus slot"
        );
    }

    /// Start syncing (called when catchup begins).
    pub fn start_syncing(&self) {
        info!("Herder entering syncing state");
        *self.state.write() = HerderState::Syncing;
    }

    /// Check close time of all values in an SCP envelope.
    ///
    /// Matches stellar-core `HerderImpl::checkCloseTime(SCPEnvelope, enforceRecent)`.
    /// This is called BEFORE signature verification as a cheap pre-filter.
    ///
    /// When `enforce_recent` is true, values must have close times within
    /// `MAXIMUM_LEDGER_CLOSETIME_DRIFT` seconds of the current wall-clock time.
    ///
    /// Returns true if at least one value in the envelope passes close-time checks.
    fn check_envelope_close_time(&self, envelope: &ScpEnvelope, enforce_recent: bool) -> bool {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);

        // Compute close-time cutoff for recency check
        let ct_cutoff = if enforce_recent {
            now.saturating_sub(MAXIMUM_LEDGER_CLOSETIME_DRIFT)
        } else {
            0
        };

        let env_ledger_index = envelope.statement.slot_index;

        // Get LCL data
        let (lcl_seq, lcl_close_time) = self
            .ledger_manager
            .read()
            .as_ref()
            .map(|lm| {
                let header = lm.current_header();
                (header.ledger_seq as u64, header.scp_value.close_time.0)
            })
            .unwrap_or((0, 0));

        let mut last_close_index = lcl_seq;
        let mut last_close_time = lcl_close_time;

        // Use tracking consensus data for a better estimate when available
        // (matching stellar-core which upgrades lastCloseIndex/lastCloseTime from tracking)
        // stellar-core uses trackingConsensusLedgerIndex() which is the LCL seq (= next_consensus - 1)
        let state = self.state();
        if state != HerderState::Booting {
            let tracking_index = self.tracking_slot().saturating_sub(1);
            if env_ledger_index >= tracking_index && tracking_index > last_close_index {
                last_close_index = tracking_index;
                last_close_time = self.tracking_consensus_close_time();
            }
        }

        // Helper: check a single StellarValue's close time
        let check_value = |value: &Value| -> bool {
            let sv = match StellarValue::from_xdr(&value.0, Limits::none()) {
                Ok(sv) => sv,
                Err(_) => return false,
            };
            let close_time = sv.close_time.0;

            // Recency check: close time must be after cutoff
            if close_time < ct_cutoff {
                return false;
            }

            // Three cases (any must pass):
            // 1. Exact-match: envelope is for the same slot as last_close_index
            if last_close_index == env_ledger_index && last_close_time == close_time {
                return true;
            }
            // 2. Older slot: last_close_index > env_ledger_index and close_time < last_close_time
            if last_close_index > env_ledger_index && last_close_time > close_time {
                return true;
            }
            // 3. Future slot: use the simple check_close_time
            self.scp_driver
                .check_close_time(env_ledger_index, last_close_time, close_time)
        };

        // Check all values in the envelope based on statement type
        // Returns true if ANY value passes (conservative / permissive)
        use stellar_xdr::curr::ScpStatementPledges;
        match &envelope.statement.pledges {
            ScpStatementPledges::Nominate(nom) => {
                nom.accepted.iter().any(|v| check_value(v))
                    || nom.votes.iter().any(|v| check_value(v))
            }
            ScpStatementPledges::Prepare(prep) => {
                if check_value(&prep.ballot.value) {
                    return true;
                }
                if let Some(ref prepared) = prep.prepared {
                    if check_value(&prepared.value) {
                        return true;
                    }
                }
                if let Some(ref prepared_prime) = prep.prepared_prime {
                    if check_value(&prepared_prime.value) {
                        return true;
                    }
                }
                false
            }
            ScpStatementPledges::Confirm(conf) => check_value(&conf.ballot.value),
            ScpStatementPledges::Externalize(ext) => check_value(&ext.commit.value),
        }
    }

    /// Receive an SCP envelope from the network.
    pub fn receive_scp_envelope(&self, envelope: ScpEnvelope) -> EnvelopeState {
        let state = self.state();
        let slot = envelope.statement.slot_index;
        let current_slot = self.tracking_slot();
        let pending_slot = self.pending_envelopes.current_slot();

        // Check if we can receive SCP messages
        if !state.can_receive_scp() {
            debug!(
                "Rejecting SCP envelope: herder in {:?} state (cannot receive)",
                state
            );
            return EnvelopeState::Invalid;
        }

        // **** First perform checks that do NOT require signature verification
        // This allows fast-failing messages we'd throw away anyway (matching stellar-core)

        // Close-time pre-filter: reject envelopes with invalid close times
        // before incurring the cost of signature verification
        if !self.check_envelope_close_time(&envelope, false) {
            debug!(
                slot,
                "Rejecting SCP envelope: close-time pre-filter failed (check_envelope_close_time(false))"
            );
            return EnvelopeState::Invalid;
        }

        let checkpoint = self.get_most_recent_checkpoint_seq();
        let mut max_ledger_seq: u64 = u64::MAX;

        if state.is_tracking() {
            // When tracking, filter messages based on consensus information
            max_ledger_seq = self.next_consensus_ledger_index() + LEDGER_VALIDITY_BRACKET;
        } else {
            // When not tracking, apply recency-based close-time filtering.
            // Allow checkpoint messages through even if close time is stale.
            // enforce_recent = true only if we've never been in sync (tracking index <= genesis)
            let tracking_consensus_index = current_slot.saturating_sub(1);
            // enforce_recent = true only if:
            // 1. We've never been in sync (tracking index <= genesis), AND
            // 2. This is NOT the next consensus slot (which we're actively working on)
            // This matches stellar-core:
            //   trackingConsensusLedgerIndex() <= GENESIS_LEDGER_SEQ
            //       && index != nextConsensusLedgerIndex()
            let enforce_recent = tracking_consensus_index <= GENESIS_LEDGER_SEQ
                && slot != self.next_consensus_ledger_index();
            if !self.check_envelope_close_time(&envelope, enforce_recent) && slot != checkpoint {
                debug!(
                    slot,
                    tracking_consensus_index,
                    enforce_recent,
                    checkpoint,
                    "Rejecting SCP envelope: close-time filter (non-tracking, enforce_recent={})",
                    enforce_recent
                );
                return EnvelopeState::Invalid;
            }
        }

        // Calculate the minimum acceptable slot
        let min_ledger_seq = if current_slot > MAX_SLOTS_TO_REMEMBER {
            current_slot - MAX_SLOTS_TO_REMEMBER + 1
        } else {
            1
        };

        // Early check: reject envelopes for already-closed slots (from catchup)
        let lcl = self
            .ledger_manager
            .read()
            .as_ref()
            .map(|m| m.current_ledger_seq() as u64);

        let effective_min = lcl.map_or(min_ledger_seq, |l| min_ledger_seq.max(l + 1));

        // Range check: slot must be in [effective_min, max_ledger_seq], with
        // checkpoint exception (matching stellar-core)
        if (slot > max_ledger_seq || slot < effective_min) && slot != checkpoint {
            debug!(
                slot,
                current_slot,
                min_ledger_seq,
                effective_min,
                max_ledger_seq,
                checkpoint,
                "Rejecting envelope: slot outside validity bracket"
            );
            return EnvelopeState::TooOld;
        }

        // **** From this point, we have to check signatures (matching stellar-core)
        if let Err(e) = self.scp_driver.verify_envelope(&envelope) {
            debug!(slot, error = %e, "Invalid SCP envelope signature");
            return EnvelopeState::InvalidSignature;
        }

        self.slot_quorum_tracker
            .write()
            .record_envelope(slot, envelope.statement.node_id.clone());

        // Special handling for EXTERNALIZE messages - they can fast-forward our state
        // even if from future slots, as they represent network consensus
        if let stellar_xdr::curr::ScpStatementPledges::Externalize(ext) =
            &envelope.statement.pledges
        {
            // Extract value and tx_set_hash upfront before any potential move
            let value = ext.commit.value.clone();
            let sv = match StellarValue::from_xdr(&value.0, Limits::none()) {
                Ok(sv) => sv,
                Err(_) => {
                    warn!(slot, "Failed to parse StellarValue from EXTERNALIZE");
                    return EnvelopeState::Invalid;
                }
            };
            let tx_set_hash = Hash256::from_bytes(sv.tx_set_hash.0);
            debug!(
                slot,
                sender = %to_short_string(&envelope.statement.node_id),
                tx_set = %tx_set_hash,
                "Received EXTERNALIZE message"
            );

            // Only request tx sets for slots we haven't already closed via catchup
            if lcl.map_or(true, |l| slot > l) {
                // Request this tx set immediately - don't wait for ledger close
                if self.scp_driver.request_tx_set(tx_set_hash, slot) {
                    debug!(slot, hash = %tx_set_hash, "Immediately requesting tx set from EXTERNALIZE");
                }
            }

            if slot > current_slot {
                // Security check 1: Validate sender is in our transitive quorum
                // This prevents accepting EXTERNALIZE messages from nodes we don't trust
                let sender = &envelope.statement.node_id;
                let in_quorum = self
                    .quorum_tracker
                    .read()
                    .is_node_definitely_in_quorum(sender);
                if !in_quorum {
                    debug!(
                        slot,
                        current_slot,
                        sender = ?sender,
                        "Rejecting EXTERNALIZE from node not in quorum"
                    );
                    return EnvelopeState::Invalid;
                }

                // stellar-core has no max distance check for EXTERNALIZE.
                // The quorum check above is sufficient to prevent untrusted fast-forwards.

                // CRITICAL: Don't externalize without the tx_set!
                // Like stellar-core, we must wait until the tx_set is available before
                // recording externalization. Otherwise we create buffered ledgers that can
                // never close (no tx_set to apply).
                if let Some(state) = self.buffer_envelope_until_tx_set(slot, &tx_set_hash, envelope)
                {
                    return state;
                }

                // Parity: only fast-forward if the slot is ahead of the LCL.
                // After catchup, LCL may have jumped ahead of the tracking slot,
                // so an EXTERNALIZE for a slot already closed should be ignored.
                if lcl.is_some_and(|l| slot <= l) {
                    debug!(
                        slot,
                        lcl = lcl.unwrap_or(0),
                        "Ignoring EXTERNALIZE for slot already closed"
                    );
                    return EnvelopeState::Valid;
                }

                // Fast-forward to this slot using the externalized value
                debug!(
                    slot,
                    current_slot, "Fast-forwarding using EXTERNALIZE from network"
                );

                self.scp_driver.record_externalized(slot, value.clone());
                self.scp_driver
                    .cleanup_externalized(self.config.max_externalized_slots);

                // Inform the SCP library about this externalization so that
                // subsequent envelopes for this slot are properly validated.
                if let Some(ref scp) = self.scp {
                    scp.force_externalize(slot, value.clone());
                }

                // Store for reference
                *self.prev_value.write() = value;

                // Advance tracking slot
                self.advance_tracking_slot(slot);

                return EnvelopeState::Valid;
            } else if lcl.is_some_and(|l| slot > l && slot < current_slot) {
                // Gap slot: strictly between LCL and tracking_slot (not the tracking
                // slot itself). This happens when we fast-forwarded tracking_slot but
                // haven't closed the intermediate ledgers. Accept EXTERNALIZE from
                // trusted validators to fill the gap.
                // Note: slot == current_slot must NOT enter this path — those
                // EXTERNALIZE messages must be processed through normal SCP so the
                // ballot protocol emits our own EXTERNALIZE envelope to the network
                // (required for crawlers like StellarBeat to see us as validating).
                let sender = &envelope.statement.node_id;
                let in_quorum = self
                    .quorum_tracker
                    .read()
                    .is_node_definitely_in_quorum(sender);
                if !in_quorum {
                    debug!(
                        slot,
                        current_slot,
                        lcl = lcl.unwrap_or(0),
                        sender = ?sender,
                        "Rejecting gap slot EXTERNALIZE from node not in quorum"
                    );
                    return EnvelopeState::Invalid;
                }

                // Request the tx_set for this gap slot so we can close it
                let is_new = self.scp_driver.request_tx_set(tx_set_hash, slot);
                if is_new {
                    info!(slot, hash = %tx_set_hash, "Requesting tx set for gap slot");
                }

                // CRITICAL: Don't externalize without the tx_set!
                // Like stellar-core, we must wait until the tx_set is available.
                if let Some(state) = self.buffer_envelope_until_tx_set(slot, &tx_set_hash, envelope)
                {
                    return state;
                }

                // Record this externalization so we can close the gap ledger
                debug!(
                    slot,
                    current_slot,
                    lcl = lcl.unwrap_or(0),
                    "Accepting EXTERNALIZE for gap slot from trusted validator"
                );

                self.scp_driver.record_externalized(slot, value.clone());

                // Inform the SCP library about this externalization
                if let Some(ref scp) = self.scp {
                    scp.force_externalize(slot, value);
                }

                return EnvelopeState::Valid;
            }
        }

        // Check if this is for a future slot
        if slot > current_slot {
            // Clone the envelope in case we need to process it after pending.add fails
            // due to a race condition (another thread advanced pending.current_slot)
            let envelope_clone = envelope.clone();

            // Buffer for later
            match self.pending_envelopes.add(slot, envelope) {
                PendingResult::Added => {
                    debug!("Buffered envelope for future slot {}", slot);
                    return EnvelopeState::Pending;
                }
                PendingResult::Duplicate => {
                    return EnvelopeState::Duplicate;
                }
                PendingResult::SlotTooFar => {
                    debug!(
                        slot,
                        current_slot, pending_slot, "Envelope rejected: slot too far ahead"
                    );
                    return EnvelopeState::Invalid;
                }
                PendingResult::SlotTooOld => {
                    // This can happen due to race condition when pending.current_slot
                    // was advanced by another thread. Since we already checked against
                    // effective_min, treat this as a race and process directly.
                    debug!(
                        slot,
                        current_slot,
                        pending_slot,
                        "Pending said TooOld but slot is within window, processing directly"
                    );
                    // Process using the clone since original was consumed
                    return self.process_scp_envelope(envelope_clone);
                }
                PendingResult::BufferFull => {
                    debug!("Pending envelope buffer full");
                    return EnvelopeState::Invalid;
                }
            }
        }

        // Process the envelope - it's for current slot or a recent slot within the window
        self.process_scp_envelope(envelope)
    }

    /// Buffer an EXTERNALIZE envelope until its tx_set is available.
    ///
    /// Returns `Some(state)` if the envelope was buffered/rejected (caller should return),
    /// or `None` if the tx_set is available and processing can continue.
    fn buffer_envelope_until_tx_set(
        &self,
        slot: u64,
        tx_set_hash: &Hash256,
        envelope: ScpEnvelope,
    ) -> Option<EnvelopeState> {
        if self.scp_driver.has_tx_set(tx_set_hash) {
            return None; // tx_set available, continue processing
        }

        debug!(slot, hash = %tx_set_hash, "EXTERNALIZE waiting for tx_set");
        use crate::fetching_envelopes::RecvResult;
        match self.fetching_envelopes.recv_envelope(envelope) {
            RecvResult::Ready => {
                debug!(slot, "EXTERNALIZE ready after fetching check");
                None // tx_set was in fetching cache
            }
            RecvResult::Fetching => Some(EnvelopeState::Fetching),
            RecvResult::AlreadyProcessed => Some(EnvelopeState::Duplicate),
            RecvResult::Discarded => Some(EnvelopeState::Invalid),
        }
    }

    /// Process an SCP envelope (internal).
    ///
    /// This follows the stellar-core pattern: we only feed envelopes to SCP
    /// after their tx sets are available. This ensures that when SCP externalizes
    /// a slot, the tx set is already in cache and ready for ledger close.
    fn process_scp_envelope(&self, envelope: ScpEnvelope) -> EnvelopeState {
        let slot = envelope.statement.slot_index;

        debug!(
            "Processing SCP envelope for slot {} from {:?}",
            slot, envelope.statement.node_id
        );

        // Check if we have the tx sets needed for this envelope.
        // This is critical: we must not let SCP externalize until we have the tx set,
        // otherwise we'll be stuck unable to close the ledger.
        let tx_set_hashes = crate::herder_utils::get_tx_set_hashes_from_envelope(&envelope);
        let mut missing_tx_sets = Vec::new();
        for hash in &tx_set_hashes {
            if !self.scp_driver.has_tx_set(hash) {
                missing_tx_sets.push(*hash);
            }
        }

        if !missing_tx_sets.is_empty() {
            // Buffer this envelope until we get the tx set
            debug!(
                slot,
                missing_count = missing_tx_sets.len(),
                "Envelope waiting for tx set(s)"
            );

            // Use the fetching envelopes manager to track this
            use crate::fetching_envelopes::RecvResult;

            // Clone the envelope before passing to recv_envelope since it takes ownership
            let envelope_clone = envelope.clone();
            let result = self.fetching_envelopes.recv_envelope(envelope);

            match result {
                RecvResult::Ready => {
                    // The fetching manager says it's ready (has all deps in its cache).
                    // This can happen if the tx set was cached elsewhere.
                    // Process it now.
                    debug!(slot, "Envelope ready in fetching manager, processing now");
                    return self.process_scp_envelope_with_tx_set(envelope_clone);
                }
                RecvResult::Fetching => {
                    // Register pending tx set requests so the app can fetch them
                    for hash in missing_tx_sets {
                        self.scp_driver.request_tx_set(hash, slot);
                    }
                    return EnvelopeState::Fetching;
                }
                RecvResult::AlreadyProcessed => {
                    return EnvelopeState::Duplicate;
                }
                RecvResult::Discarded => {
                    return EnvelopeState::Invalid;
                }
            }
        }

        // All tx sets available - proceed with SCP processing
        self.process_scp_envelope_with_tx_set(envelope)
    }

    /// Process an SCP envelope after confirming tx sets are available.
    fn process_scp_envelope_with_tx_set(&self, envelope: ScpEnvelope) -> EnvelopeState {
        let slot = envelope.statement.slot_index;

        // If we have SCP (validator mode), process through consensus
        if let Some(ref scp) = self.scp {
            let result = scp.receive_envelope(envelope.clone());

            match result {
                henyey_scp::EnvelopeState::Invalid => {
                    return EnvelopeState::Invalid;
                }
                henyey_scp::EnvelopeState::Valid => {
                    // Valid but not new
                    return EnvelopeState::Duplicate;
                }
                henyey_scp::EnvelopeState::ValidNew => {
                    if self.heard_from_quorum(slot) {
                        debug!(slot, "Heard from quorum");
                    }
                    // Check if this slot is now externalized
                    if scp.is_slot_externalized(slot) {
                        if let Some(value) = scp.get_externalized_value(slot) {
                            info!(slot, "Slot externalized via SCP consensus");
                            self.scp_driver.record_externalized(slot, value.clone());
                            self.scp_driver
                                .cleanup_externalized(self.config.max_externalized_slots);

                            // Store for next round's priority calculation
                            *self.prev_value.write() = value;

                            // Advance tracking slot
                            self.advance_tracking_slot(slot);
                        }
                    }
                    return EnvelopeState::Valid;
                }
            }
        }

        // Non-validator mode: just track externalized values from network
        if let stellar_xdr::curr::ScpStatementPledges::Externalize(ext) =
            &envelope.statement.pledges
        {
            if self.heard_from_quorum(slot) {
                debug!(slot, "Heard from quorum (observer)");
            }
            let value = ext.commit.value.clone();
            self.scp_driver.record_externalized(slot, value.clone());
            self.scp_driver
                .cleanup_externalized(self.config.max_externalized_slots);

            // Store for reference
            *self.prev_value.write() = value;

            // Advance tracking slot
            self.advance_tracking_slot(slot);
        }

        EnvelopeState::Valid
    }

    /// Advance tracking slot after externalization.
    fn advance_tracking_slot(&self, externalized_slot: u64) {
        // Extract close time from the externalized value for tracking
        let close_time = self
            .scp_driver
            .get_externalized_close_time(externalized_slot)
            .unwrap_or(0);

        let mut tracking = self.tracking_slot.write();
        if externalized_slot >= *tracking {
            *tracking = externalized_slot + 1;
            // Update tracking consensus close time (matching stellar-core setTrackingSCPState)
            *self.tracking_consensus_close_time.write() = close_time;
            self.pending_envelopes
                .set_current_slot(externalized_slot + 1);

            // Transition to Tracking on successful externalization.
            // Matches stellar-core's setTrackingSCPState(slotIndex, b, true)
            // in HerderSCPDriver::valueExternalized which always sets
            // HERDER_TRACKING_NETWORK_STATE on externalization.
            {
                let mut state = self.state.write();
                if *state != HerderState::Tracking {
                    info!(
                        slot = externalized_slot,
                        "Transitioning to Tracking on externalization"
                    );
                    *state = HerderState::Tracking;
                }
            }

            // Update ScpDriver with tracking state for close-time validation
            self.scp_driver
                .set_tracking_state(true, externalized_slot + 1, close_time);

            // Release any pending envelopes for the new slot
            drop(tracking);
            let pending = self.pending_envelopes.release(externalized_slot + 1);
            for env in pending {
                let _ = self.process_scp_envelope(env);
            }
        }
    }

    /// Receive a transaction from the network.
    pub fn receive_transaction(&self, tx: TransactionEnvelope) -> TxQueueResult {
        let state = self.state();

        if !state.can_receive_transactions() {
            debug!("Ignoring transaction in {:?} state", state);
            return TxQueueResult::Invalid(None);
        }

        // Add to transaction queue
        let result = self.tx_queue.try_add(tx);

        match result {
            TxQueueResult::Added => {
                debug!("Added transaction to queue, size: {}", self.tx_queue.len());
            }
            TxQueueResult::Duplicate => {
                debug!("Duplicate transaction ignored");
            }
            TxQueueResult::QueueFull => {
                warn!("Transaction queue full");
            }
            TxQueueResult::FeeTooLow => {
                debug!("Transaction fee too low");
            }
            TxQueueResult::Invalid(code) => {
                debug!(?code, "Invalid transaction rejected");
            }
            TxQueueResult::Banned => {
                debug!("Banned transaction rejected");
            }
            TxQueueResult::Filtered => {
                debug!("Transaction rejected due to filtered operation type");
            }
            TxQueueResult::TryAgainLater => {
                debug!("Transaction rejected: account already has pending transaction");
            }
        }

        result
    }

    /// Trigger consensus for the next ledger (for validators).
    ///
    /// This is called periodically by the consensus timer.
    pub async fn trigger_next_ledger(&self, ledger_seq: u32) -> Result<()> {
        if !self.is_validator() {
            return Err(HerderError::NotValidating);
        }

        if !self.is_tracking() {
            return Err(HerderError::NotValidating);
        }

        let scp = match &self.scp {
            Some(scp) => scp,
            None => return Err(HerderError::NotValidating),
        };

        let slot = ledger_seq as u64;
        tracing::debug!("Triggering consensus for ledger {}", ledger_seq);

        // Get the previous ledger hash
        let previous_hash = if let Some(manager) = self.ledger_manager.read().as_ref() {
            manager.current_header_hash()
        } else {
            Hash256::ZERO
        };
        let starting_seq = self
            .ledger_manager
            .read()
            .as_ref()
            .and_then(|manager| self.build_starting_seq_map(manager));

        // Create transaction set from queue using the current ledger limit when available.
        // Must use generalized format (protocol >= 20) so the hash matches what peers
        // expect when they request the TxSet by hash from SCP StellarValue.
        let max_txs = self
            .ledger_manager
            .read()
            .as_ref()
            .map(|manager| manager.current_header().max_tx_set_size as usize)
            .unwrap_or(self.config.max_tx_set_size);
        let (tx_set, _gen_tx_set) = self.tx_queue.build_generalized_tx_set_with_starting_seq(
            previous_hash,
            max_txs,
            starting_seq.as_ref(),
        );

        info!(
            hash = %tx_set.hash,
            tx_count = tx_set.len(),
            "Proposing transaction set"
        );

        // Cache the transaction set
        self.scp_driver.cache_tx_set(tx_set.clone());

        // Create StellarValue for nomination
        // Parity: HerderImpl.cpp triggerNextLedger — clamp to ensure monotonic increase
        let lcl_close_time = self
            .ledger_manager
            .read()
            .as_ref()
            .map(|lm| lm.current_header().scp_value.close_time.0)
            .unwrap_or(0);
        let mut close_time = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        if close_time <= lcl_close_time {
            close_time = lcl_close_time + 1;
        }

        // Combine static (config) and runtime (HTTP /upgrades?mode=set) upgrades.
        // Parity: stellar-core uses only `Upgrades::createUpgradesFor` which checks
        // against current ledger state. We must similarly filter static proposed
        // upgrades to avoid proposing already-applied upgrades (e.g., a protocol
        // version upgrade that has already taken effect).
        let header = self
            .ledger_manager
            .read()
            .as_ref()
            .map(|lm| lm.current_header())
            .unwrap_or_default();
        let state = CurrentLedgerState {
            close_time,
            protocol_version: header.ledger_version,
            base_fee: header.base_fee,
            max_tx_set_size: header.max_tx_set_size,
            base_reserve: header.base_reserve,
            flags: match &header.ext {
                stellar_xdr::curr::LedgerHeaderExt::V0 => 0,
                stellar_xdr::curr::LedgerHeaderExt::V1(ext) => ext.flags,
            },
            max_soroban_tx_set_size: None, // TODO: read from config settings
        };

        // Filter static upgrades against current state to avoid proposing
        // upgrades that have already been applied.
        let mut upgrade_list: Vec<LedgerUpgrade> = self
            .config
            .proposed_upgrades
            .iter()
            .filter(|upgrade| match upgrade {
                LedgerUpgrade::Version(v) => *v != state.protocol_version,
                LedgerUpgrade::BaseFee(f) => *f != state.base_fee,
                LedgerUpgrade::MaxTxSetSize(s) => *s != state.max_tx_set_size,
                LedgerUpgrade::BaseReserve(r) => *r != state.base_reserve,
                LedgerUpgrade::Flags(f) => *f != state.flags,
                _ => true, // Config/Soroban upgrades always pass through
            })
            .cloned()
            .collect();

        // Add runtime upgrades from the Upgrades scheduling system
        let runtime_upgrades = self.runtime_upgrades.read().create_upgrades_for(&state);
        for upgrade in runtime_upgrades {
            // Don't duplicate upgrades of the same type
            let dominated = upgrade_list.iter().any(|existing| {
                std::mem::discriminant(existing) == std::mem::discriminant(&upgrade)
            });
            if !dominated {
                upgrade_list.push(upgrade);
            }
        }

        let upgrades: Vec<UpgradeType> = upgrade_list
            .iter()
            .filter_map(|upgrade| upgrade.to_xdr(Limits::none()).ok())
            .filter_map(|bytes| bytes.try_into().ok().map(UpgradeType))
            .collect();

        let stellar_value = self.make_stellar_value(tx_set.hash, close_time, upgrades)?;

        // Encode to Value
        let value_bytes = stellar_value
            .to_xdr(Limits::none())
            .map_err(|e| HerderError::Internal(format!("Failed to encode value: {}", e)))?;
        let value = Value(
            value_bytes
                .try_into()
                .map_err(|_| HerderError::Internal("Value too large".to_string()))?,
        );

        // Get previous value for priority calculation
        let prev_value = self.prev_value.read().clone();

        // Start SCP nomination
        if scp.nominate(slot, value, &prev_value) {
            info!(slot, "Started SCP nomination for ledger");
        } else {
            debug!(
                slot,
                "Nomination already in progress or slot already externalized"
            );
        }

        // For solo validators (1-of-1 quorum), the nomination→ballot→externalization
        // happens synchronously within scp.nominate(). Check if the slot was
        // externalized and advance tracking state accordingly.
        if self.scp_driver.latest_externalized_slot() == Some(slot) {
            if let Some(ext) = self.scp_driver.get_externalized(slot) {
                *self.prev_value.write() = ext.value;
            }
            self.advance_tracking_slot(slot);
        }

        Ok(())
    }

    /// Get the SCP driver.
    pub fn scp_driver(&self) -> &Arc<ScpDriver> {
        &self.scp_driver
    }

    /// Set the envelope broadcast callback.
    ///
    /// This is called when SCP needs to send an envelope to the network.
    pub fn set_envelope_sender<F>(&self, sender: F)
    where
        F: Fn(ScpEnvelope) + Send + Sync + 'static,
    {
        self.scp_driver.set_envelope_sender(sender);
    }

    /// Get the SCP instance (if validator).
    pub fn scp(&self) -> Option<&SCP<HerderScpCallback>> {
        self.scp.as_ref()
    }

    /// Check if a ledger close is ready and return the close info.
    ///
    /// This is called by the application to check if consensus has been
    /// reached and the ledger should be closed.
    pub fn check_ledger_close(&self, slot: SlotIndex) -> Option<LedgerCloseInfo> {
        // Check if we have externalized this slot
        let externalized = self.scp_driver.get_externalized(slot)?;

        // Parse the StellarValue
        let stellar_value = match StellarValue::from_xdr(&externalized.value, Limits::none()) {
            Ok(v) => v,
            Err(e) => {
                error!(slot, error = %e, "Failed to parse externalized StellarValue");
                return None;
            }
        };

        // Get the transaction set
        let tx_set_hash = Hash256::from_bytes(stellar_value.tx_set_hash.0);
        let tx_set = self.scp_driver.get_tx_set(&tx_set_hash);

        if tx_set.is_none() {
            // Register this as a pending tx set request
            self.scp_driver.request_tx_set(tx_set_hash, slot);
        }

        Some(LedgerCloseInfo {
            slot,
            close_time: stellar_value.close_time.0,
            tx_set_hash,
            tx_set,
            upgrades: stellar_value.upgrades.to_vec(),
            stellar_value_ext: stellar_value.ext,
        })
    }

    /// Mark a ledger as closed and clean up.
    ///
    /// Called after the application has applied the ledger.
    pub fn ledger_closed(
        &self,
        slot: SlotIndex,
        applied_tx_hashes: &[Hash256],
        applied_upgrades: &[UpgradeType],
        close_time: u64,
    ) {
        debug!(slot, txs = applied_tx_hashes.len(), "Ledger closed");

        // Remove applied transactions from queue
        self.tx_queue.remove_applied_by_hash(applied_tx_hashes);

        // Clear consumed upgrade parameters so they are not proposed again.
        // Mirrors stellar-core HerderImpl::processExternalized() -> removeUpgrades().
        {
            let (new_params, updated) = self
                .runtime_upgrades
                .read()
                .remove_upgrades(applied_upgrades, close_time);
            if updated {
                let max_protocol = self.config.max_protocol_version;
                let mut upgrades = self.runtime_upgrades.write();
                // Ignore the result — the params were already validated when set.
                let _ = upgrades.set_parameters(new_params, max_protocol);
            }
        }

        // Drop pending tx set requests for slots older than the next slot.
        let _ = self
            .scp_driver
            .cleanup_old_pending_slots(slot.saturating_add(1));

        // Clean up old SCP state
        if let Some(ref scp) = self.scp {
            scp.purge_slots(slot.saturating_sub(10), None);
        }

        // Clean up old fetching envelopes and cached tx sets (keep a small buffer)
        // Keep the current slot and 2 slots back for any late envelopes
        let keep_slot = slot.saturating_sub(2);
        self.fetching_envelopes.erase_below(slot, keep_slot);

        // Clean up old data
        self.cleanup();
    }

    /// Handle nomination timeout.
    ///
    /// Called when the nomination timer expires. Re-nominates with the same
    /// value to try to make progress.
    pub fn handle_nomination_timeout(&self, slot: SlotIndex) {
        if let Some(ref scp) = self.scp {
            let prev_value = self.prev_value.read().clone();
            let value = self.create_nomination_value(slot);

            if let Some(value) = value {
                if scp.nominate_timeout(slot, value, &prev_value) {
                    debug!(slot, "Re-nominated after timeout");
                }
            }
        }
    }

    /// Handle ballot timeout.
    ///
    /// Called when the ballot timer expires. Bumps the ballot counter to
    /// try to make progress.
    pub fn handle_ballot_timeout(&self, slot: SlotIndex) {
        if let Some(ref scp) = self.scp {
            if scp.bump_ballot(slot) {
                debug!(slot, "Bumped ballot after timeout");
            }
        }
    }

    /// Get the current nomination timeout.
    pub fn get_nomination_timeout(&self, slot: SlotIndex) -> Option<std::time::Duration> {
        if let Some(ref scp) = self.scp {
            if let Some(state) = scp.get_slot_state(slot) {
                if state.is_nominating {
                    return Some(scp.get_nomination_timeout(state.nomination_round));
                }
            }
        }
        None
    }

    /// Get the current ballot timeout.
    pub fn get_ballot_timeout(&self, slot: SlotIndex) -> Option<std::time::Duration> {
        if let Some(ref scp) = self.scp {
            if let Some(state) = scp.get_slot_state(slot) {
                if let Some(round) = state.ballot_round {
                    if state.heard_from_quorum
                        && !matches!(state.ballot_phase, BallotPhase::Externalize)
                    {
                        return Some(scp.get_ballot_timeout(round));
                    }
                }
            }
        }
        None
    }

    /// Create a nomination value for a slot.
    fn create_nomination_value(&self, _slot: SlotIndex) -> Option<Value> {
        // Get the previous ledger hash from our current ledger state
        let (previous_hash, max_txs, starting_seq) =
            if let Some(manager) = self.ledger_manager.read().as_ref() {
                let header = manager.current_header();
                let max = header.max_tx_set_size as usize;
                let starting_seq = self.build_starting_seq_map(manager);
                (manager.current_header_hash(), max, starting_seq)
            } else {
                (Hash256::ZERO, self.config.max_tx_set_size, None)
            };

        // Build GeneralizedTransactionSet with proper hash computation
        let (tx_set, _gen_tx_set) = self.tx_queue.build_generalized_tx_set_with_starting_seq(
            previous_hash,
            max_txs,
            starting_seq.as_ref(),
        );

        info!(
            hash = %tx_set.hash,
            tx_count = tx_set.transactions.len(),
            "Proposing transaction set"
        );

        // Cache the tx set so we can respond to GetTxSet requests
        self.scp_driver.cache_tx_set(tx_set.clone());

        // Create StellarValue with the GeneralizedTransactionSet hash
        let close_time = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);

        let upgrades: Vec<UpgradeType> = self
            .config
            .proposed_upgrades
            .iter()
            .filter_map(|upgrade| upgrade.to_xdr(Limits::none()).ok())
            .filter_map(|bytes| bytes.try_into().ok().map(UpgradeType))
            .collect();

        let stellar_value = self
            .make_stellar_value(tx_set.hash, close_time, upgrades)
            .ok()?;

        // Encode to Value
        let value_bytes = stellar_value.to_xdr(Limits::none()).ok()?;
        let value = Value(value_bytes.try_into().ok()?);
        Some(value)
    }

    /// Create a signed StellarValue.
    ///
    /// Parity: HerderImpl.cpp `makeStellarValue` — signs with STELLAR_VALUE_SIGNED.
    fn make_stellar_value(
        &self,
        tx_set_hash: Hash256,
        close_time: u64,
        upgrades: Vec<UpgradeType>,
    ) -> std::result::Result<StellarValue, HerderError> {
        let xdr_tx_set_hash = stellar_xdr::curr::Hash(tx_set_hash.0);
        let xdr_close_time = TimePoint(close_time);
        let secret_key = self
            .secret_key
            .as_ref()
            .ok_or_else(|| HerderError::Internal("No secret key for signing".to_string()))?;
        let network_id = self.scp_driver.network_id();
        let mut sign_data = network_id.0.to_vec();
        sign_data.extend_from_slice(&EnvelopeType::Scpvalue.to_xdr(Limits::none()).map_err(
            |e| HerderError::Internal(format!("Failed to encode envelope type: {}", e)),
        )?);
        sign_data.extend_from_slice(
            &xdr_tx_set_hash.to_xdr(Limits::none()).map_err(|e| {
                HerderError::Internal(format!("Failed to encode tx set hash: {}", e))
            })?,
        );
        sign_data.extend_from_slice(
            &xdr_close_time.to_xdr(Limits::none()).map_err(|e| {
                HerderError::Internal(format!("Failed to encode close time: {}", e))
            })?,
        );
        let sig = secret_key.sign(&sign_data);
        let node_id = NodeId(stellar_xdr::curr::PublicKey::PublicKeyTypeEd25519(Uint256(
            *secret_key.public_key().as_bytes(),
        )));

        Ok(StellarValue {
            tx_set_hash: xdr_tx_set_hash,
            close_time: xdr_close_time,
            upgrades: upgrades.try_into().unwrap_or_default(),
            ext: StellarValueExt::Signed(LedgerCloseValueSignature {
                node_id,
                signature: XdrSignature(sig.0.to_vec().try_into().unwrap_or_default()),
            }),
        })
    }

    fn build_starting_seq_map(
        &self,
        manager: &Arc<LedgerManager>,
    ) -> Option<HashMap<Vec<u8>, i64>> {
        let snapshot = manager.create_snapshot().ok()?;
        let ledger_seq = manager.current_ledger_seq();
        if ledger_seq > i32::MAX as u32 {
            return None;
        }
        let starting_seq = (ledger_seq as i64) << 32;
        let mut map: HashMap<Vec<u8>, i64> = HashMap::new();
        for account in self.tx_queue.pending_accounts() {
            let key = account_key_from_account_id(&account);
            match snapshot.get_account(&account) {
                Ok(Some(entry)) => {
                    map.insert(key, entry.seq_num.0);
                }
                Ok(None) => {
                    map.insert(key, starting_seq);
                }
                Err(_) => {}
            }
        }
        Some(map)
    }

    /// Get the transaction queue.
    pub fn tx_queue(&self) -> &TransactionQueue {
        &self.tx_queue
    }

    /// Check whether the given account has any pending transactions in the
    /// queue.
    ///
    /// Matches stellar-core `Herder::sourceAccountPending(AccountID const&)`.
    pub fn source_account_pending(&self, account_id: &stellar_xdr::curr::AccountId) -> bool {
        let key = account_key_from_account_id(account_id);
        self.tx_queue
            .pending_accounts()
            .iter()
            .any(|a| account_key_from_account_id(a) == key)
    }

    /// Get the pending envelope manager.
    pub fn pending_envelopes(&self) -> &PendingEnvelopes {
        &self.pending_envelopes
    }

    /// Get the latest externalized slot.
    pub fn latest_externalized_slot(&self) -> Option<u64> {
        self.scp_driver.latest_externalized_slot()
    }

    /// Get an externalized value.
    pub fn get_externalized(&self, slot: u64) -> Option<crate::scp_driver::ExternalizedSlot> {
        self.scp_driver.get_externalized(slot)
    }

    /// Find an externalized slot for a given tx set hash.
    pub fn find_externalized_slot_by_tx_set_hash(&self, hash: &Hash256) -> Option<SlotIndex> {
        self.scp_driver.find_externalized_slot_by_tx_set_hash(hash)
    }

    /// Get all externalized slot indices in a range (inclusive).
    /// Returns a sorted list of slots that have been externalized.
    pub fn get_externalized_slots_in_range(&self, from: u64, to: u64) -> Vec<u64> {
        self.scp_driver.get_externalized_slots_in_range(from, to)
    }

    /// Find missing (gap) slots in a range that have not been externalized.
    /// Returns slots that should have EXTERNALIZE but don't.
    pub fn find_missing_slots_in_range(&self, from: u64, to: u64) -> Vec<u64> {
        self.scp_driver.find_missing_slots_in_range(from, to)
    }

    /// Get SCP state envelopes for responding to peers.
    ///
    /// Returns SCP envelopes for slots starting from `from_slot`, along with
    /// our local quorum set if configured.
    pub fn get_scp_state(&self, from_slot: u64) -> (Vec<ScpEnvelope>, Option<ScpQuorumSet>) {
        let envelopes = if let Some(ref scp) = self.scp {
            scp.get_scp_state(from_slot)
        } else {
            vec![]
        };

        let quorum_set = self.scp_driver.get_local_quorum_set();

        (envelopes, quorum_set)
    }

    /// Get all SCP envelopes recorded for a slot.
    pub fn get_scp_envelopes(&self, slot: u64) -> Vec<ScpEnvelope> {
        if let Some(ref scp) = self.scp {
            scp.get_slot_envelopes(slot)
        } else {
            Vec::new()
        }
    }

    pub fn get_slot_state(&self, slot: u64) -> Option<henyey_scp::SlotState> {
        self.scp.as_ref().and_then(|scp| scp.get_slot_state(slot))
    }

    /// Get the local quorum set if configured.
    pub fn local_quorum_set(&self) -> Option<ScpQuorumSet> {
        self.scp_driver.get_local_quorum_set()
    }

    /// Purge SCP state for slots below the given slot.
    ///
    /// This is used during out-of-sync recovery to free memory and allow
    /// fresh state to be fetched from peers.
    pub fn purge_slots_below(&self, slot: SlotIndex) {
        self.scp_driver.purge_slots_below(slot);
        self.pending_envelopes.evict_expired();
    }

    /// Get the latest SCP messages for a slot.
    ///
    /// Returns envelopes that can be broadcast to peers during recovery.
    pub fn get_latest_messages(&self, slot: SlotIndex) -> Option<Vec<ScpEnvelope>> {
        let envelopes = self.scp_driver.get_local_envelopes(slot);
        if envelopes.is_empty() {
            None
        } else {
            Some(envelopes)
        }
    }

    /// Remove applied transactions from the queue.
    pub fn remove_applied_transactions(&self, tx_hashes: &[Hash256]) {
        self.tx_queue.remove_applied_by_hash(tx_hashes);
    }

    /// Clean up old data.
    pub fn cleanup(&self) {
        // Clean up old externalized slots
        self.scp_driver
            .cleanup_externalized(self.config.max_externalized_slots);

        // Clean up expired pending envelopes
        self.pending_envelopes.evict_expired();

        // Clean up expired transactions
        self.tx_queue.evict_expired();

        // Clean up old pending tx set requests (by time).
        // Keep them longer to allow lagging nodes to fetch historical sets.
        self.scp_driver.cleanup_pending_tx_sets(120);
    }

    /// Clear the transaction set cache to release memory.
    /// Called after catchup to release stale cached tx sets.
    pub fn clear_tx_set_cache(&self) {
        self.scp_driver.clear_tx_set_cache();
    }

    /// Clear all scp_driver caches to release memory.
    /// Called after catchup to release stale cached data.
    pub fn clear_scp_driver_caches(&self) {
        self.scp_driver.clear_all_caches();
    }

    /// Trim stale scp_driver caches while preserving data for future slots.
    /// Called after catchup to release memory while keeping pending tx_set
    /// requests and externalized data for slots after catchup.
    pub fn trim_scp_driver_caches(&self, keep_after_slot: SlotIndex) {
        self.scp_driver.trim_stale_caches(keep_after_slot);
    }

    /// Clear all fetching caches to release memory.
    /// Called after catchup to release stale cached data.
    pub fn clear_fetching_caches(&self) {
        self.fetching_envelopes.clear_all();
    }

    /// Trim stale fetching caches while preserving tx_sets for future slots.
    /// Called after catchup to release memory while keeping tx_sets needed for
    /// buffered ledgers that will be applied after catchup completes.
    pub fn trim_fetching_caches(&self, keep_after_slot: SlotIndex) {
        self.fetching_envelopes.trim_stale(keep_after_slot);
    }

    /// Clear pending envelopes to release memory.
    /// Called after catchup to release stale pending data.
    pub fn clear_pending_envelopes(&self) {
        self.pending_envelopes.clear();
    }

    /// Get pending transaction set hashes that need to be fetched from peers.
    pub fn get_pending_tx_set_hashes(&self) -> Vec<Hash256> {
        self.scp_driver.get_pending_tx_set_hashes()
    }

    /// Get pending transaction sets with their slots.
    pub fn get_pending_tx_sets(&self) -> Vec<(Hash256, SlotIndex)> {
        self.scp_driver.get_pending_tx_sets()
    }

    /// Clear all pending tx set requests.
    /// Used after rapid close cycles to discard stale requests whose tx_sets
    /// are no longer available from peers.
    pub fn clear_pending_tx_sets(&self) {
        self.scp_driver.clear_pending_tx_sets()
    }

    /// Drop pending tx set requests for slots older than the given slot.
    pub fn cleanup_old_pending_tx_sets(&self, current_slot: SlotIndex) -> usize {
        self.scp_driver.cleanup_old_pending_slots(current_slot)
    }

    /// Check if any pending tx set request has been waiting longer than the given duration.
    /// This is used to detect when tx sets are unavailable and faster catchup should be triggered.
    pub fn has_stale_pending_tx_set(&self, max_wait_secs: u64) -> bool {
        self.scp_driver.has_stale_pending_tx_set(max_wait_secs)
    }

    /// Check if we need a transaction set.
    pub fn needs_tx_set(&self, hash: &Hash256) -> bool {
        self.scp_driver.needs_tx_set(hash)
    }

    /// Receive a transaction set from the network.
    /// Returns the slot it was needed for, if any.
    ///
    /// This also processes any envelopes that were waiting for this tx set,
    /// feeding them to SCP now that the dependency is satisfied.
    pub fn receive_tx_set(&self, tx_set: TransactionSet) -> Option<SlotIndex> {
        let hash = tx_set.hash;
        let slot = self.scp_driver.receive_tx_set(tx_set);

        // Notify the fetching envelopes manager that this tx set is now available.
        // Use the slot from scp_driver, or tracking_slot as fallback.
        let notify_slot = slot.unwrap_or_else(|| *self.tracking_slot.read());
        self.fetching_envelopes.tx_set_available(hash, notify_slot);

        // Process any envelopes that became ready
        self.process_ready_fetching_envelopes();

        slot
    }

    /// Process envelopes that have become ready after tx set arrival.
    ///
    /// This is called after receiving a tx set to feed any buffered envelopes
    /// to SCP now that their dependencies are satisfied.
    pub fn process_ready_fetching_envelopes(&self) -> usize {
        let ready_slots = self.fetching_envelopes.ready_slots();
        let mut processed = 0;

        for slot in ready_slots {
            while let Some(envelope) = self.fetching_envelopes.pop(slot) {
                debug!(slot, "Processing envelope that was waiting for tx set");
                let _ = self.process_scp_envelope_with_tx_set(envelope);
                processed += 1;
            }
        }

        if processed > 0 {
            debug!(processed, "Processed envelopes after tx set arrival");
        }

        processed
    }

    /// Cache a transaction set directly.
    ///
    /// This also notifies the fetching envelopes manager, which may
    /// process any waiting envelopes.
    pub fn cache_tx_set(&self, tx_set: TransactionSet) {
        let hash = tx_set.hash;
        self.scp_driver.cache_tx_set(tx_set);

        // Notify fetching envelopes and process any that become ready
        let slot = *self.tracking_slot.read();
        self.fetching_envelopes.tx_set_available(hash, slot);
        self.process_ready_fetching_envelopes();
    }

    /// Check if a transaction set is cached.
    pub fn has_tx_set(&self, hash: &Hash256) -> bool {
        self.scp_driver.has_tx_set(hash)
    }

    /// Get a cached transaction set by hash.
    pub fn get_tx_set(&self, hash: &Hash256) -> Option<TransactionSet> {
        self.scp_driver.get_tx_set(hash)
    }

    /// Get statistics about the Herder.
    pub fn stats(&self) -> HerderStats {
        HerderStats {
            state: self.state(),
            tracking_slot: self.tracking_slot(),
            pending_transactions: self.tx_queue.len(),
            pending_envelopes: self.pending_envelopes.len(),
            pending_envelope_slots: self.pending_envelopes.slot_count(),
            cached_tx_sets: self.scp_driver.tx_set_cache_size(),
            is_validator: self.is_validator(),
            pending_envelope_stats: self.pending_envelopes.stats(),
            tx_queue_stats: self.tx_queue.stats(),
        }
    }

    // --- FetchingEnvelopes integration ---

    /// Receive a TxSet from a peer.
    ///
    /// Called when a TxSet or GeneralizedTxSet message is received.
    /// Returns true if the TxSet was needed and envelopes may be ready.
    pub fn recv_tx_set(&self, hash: Hash256, slot: u64, data: Vec<u8>) -> bool {
        self.fetching_envelopes.recv_tx_set(hash, slot, data)
    }

    /// Receive a QuorumSet from a peer.
    ///
    /// Called when an ScpQuorumset message is received.
    /// Returns true if the QuorumSet was needed and envelopes may be ready.
    pub fn recv_quorum_set(&self, hash: Hash256, quorum_set: ScpQuorumSet) -> bool {
        self.fetching_envelopes.recv_quorum_set(hash, quorum_set)
    }

    /// Process pending fetch requests.
    ///
    /// Should be called periodically (e.g., every second) to handle timeouts
    /// and retry fetching from different peers. Returns the number of requests sent.
    pub fn process_fetching(&self) -> usize {
        self.fetching_envelopes.process_pending()
    }

    /// Update the list of available peers for fetching.
    pub fn set_fetching_peers(&self, peers: Vec<henyey_overlay::PeerId>) {
        self.fetching_envelopes.set_available_peers(peers);
    }

    /// Get statistics about envelope fetching.
    pub fn fetching_stats(&self) -> FetchingStats {
        self.fetching_envelopes.stats()
    }

    /// Get the number of envelopes currently being fetched.
    pub fn fetching_count(&self) -> usize {
        self.fetching_envelopes.fetching_count()
    }

    /// Get the number of envelopes ready for processing.
    pub fn ready_count(&self) -> usize {
        self.fetching_envelopes.ready_count()
    }

    /// Pop a ready envelope for a slot.
    pub fn pop_ready_envelope(&self, slot: u64) -> Option<ScpEnvelope> {
        self.fetching_envelopes.pop(slot)
    }

    /// Get all slots with ready envelopes.
    pub fn ready_slots(&self) -> Vec<u64> {
        self.fetching_envelopes.ready_slots()
    }

    /// Erase fetching data for old slots.
    pub fn erase_fetching_below(&self, slot_index: u64, slot_to_keep: u64) {
        self.fetching_envelopes
            .erase_below(slot_index, slot_to_keep);
    }

    /// Check if we have a cached TxSet in the fetching envelopes cache.
    pub fn has_fetching_tx_set(&self, hash: &Hash256) -> bool {
        self.fetching_envelopes.has_tx_set(hash)
    }

    /// Check if we have a cached QuorumSet in the fetching envelopes cache.
    pub fn has_fetching_quorum_set(&self, hash: &Hash256) -> bool {
        self.fetching_envelopes.has_quorum_set(hash)
    }

    /// Get SCP driver cache sizes for diagnostics.
    pub fn scp_driver_cache_sizes(&self) -> crate::scp_driver::ScpDriverCacheSizes {
        self.scp_driver.cache_sizes()
    }

    /// Get fetching envelopes cache sizes for diagnostics.
    pub fn fetching_cache_sizes(&self) -> (usize, usize, usize) {
        (
            self.fetching_envelopes.tx_set_cache_size(),
            self.fetching_envelopes.quorum_set_cache_size(),
            self.fetching_envelopes.slots_count(),
        )
    }
}

/// Information about a ledger ready to close.
#[derive(Debug, Clone)]
pub struct LedgerCloseInfo {
    /// The slot/ledger sequence.
    pub slot: SlotIndex,
    /// Close time from consensus.
    pub close_time: u64,
    /// Transaction set hash.
    pub tx_set_hash: Hash256,
    /// Transaction set (if available in cache).
    pub tx_set: Option<TransactionSet>,
    /// Protocol upgrades.
    pub upgrades: Vec<UpgradeType>,
    /// StellarValue extension (Basic or Signed).
    /// This must match what the network used in consensus.
    pub stellar_value_ext: StellarValueExt,
}

/// Statistics about the Herder.
#[derive(Debug, Clone)]
pub struct HerderStats {
    /// Current state.
    pub state: HerderState,
    /// Current tracking slot.
    pub tracking_slot: u64,
    /// Number of pending transactions.
    pub pending_transactions: usize,
    /// Number of pending SCP envelopes.
    pub pending_envelopes: usize,
    /// Number of slots with pending envelopes.
    pub pending_envelope_slots: usize,
    /// Number of cached transaction sets.
    pub cached_tx_sets: usize,
    /// Whether this node is a validator.
    pub is_validator: bool,
    /// Detailed pending envelope statistics.
    pub pending_envelope_stats: PendingStats,
    /// Detailed transaction queue statistics.
    pub tx_queue_stats: TxQueueStats,
}

fn node_id_from_public_key(pk: &PublicKey) -> NodeId {
    NodeId(pk.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tx_queue::TransactionSet;
    use henyey_crypto::SecretKey;
    use henyey_scp::hash_quorum_set;
    use stellar_xdr::curr::{
        EnvelopeType, LedgerCloseValueSignature, LedgerUpgrade, Limits, NodeId as XdrNodeId,
        ReadXdr, ScpBallot, ScpNomination, ScpStatement, ScpStatementExternalize,
        ScpStatementPledges, ScpStatementPrepare, Signature as XdrSignature, StellarValue,
        StellarValueExt, TimePoint, Value, WriteXdr,
    };

    fn make_test_herder() -> Herder {
        let config = HerderConfig::default();
        Herder::new(config)
    }

    fn make_validator_herder() -> (Herder, SecretKey) {
        let seed = [7u8; 32];
        let secret_for_herder = SecretKey::from_seed(&seed);
        let public = secret_for_herder.public_key();
        let node_id = node_id_from_public_key(&public);

        let quorum_set = ScpQuorumSet {
            threshold: 1,
            validators: vec![node_id].try_into().unwrap(),
            inner_sets: vec![].try_into().unwrap(),
        };

        let config = HerderConfig {
            is_validator: true,
            node_public_key: public,
            local_quorum_set: Some(quorum_set),
            ..HerderConfig::default()
        };

        let herder = Herder::with_secret_key(config, secret_for_herder);
        let secret_for_signing = SecretKey::from_seed(&seed);

        (herder, secret_for_signing)
    }

    fn make_valid_value_with_cached_tx_set(herder: &Herder, secret_key: &SecretKey) -> Value {
        let tx_set = TransactionSet::new(Hash256::ZERO, Vec::new());
        let tx_set_hash = tx_set.hash;
        herder.scp_driver.cache_tx_set(tx_set);

        let xdr_tx_set_hash = stellar_xdr::curr::Hash(tx_set_hash.0);
        let close_time = TimePoint(1);

        // Sign: (networkID, ENVELOPE_TYPE_SCPVALUE, txSetHash, closeTime)
        let network_id = herder.scp_driver.network_id();
        let mut sign_data = network_id.0.to_vec();
        sign_data.extend_from_slice(&EnvelopeType::Scpvalue.to_xdr(Limits::none()).expect("xdr"));
        sign_data.extend_from_slice(&xdr_tx_set_hash.to_xdr(Limits::none()).expect("xdr"));
        sign_data.extend_from_slice(&close_time.to_xdr(Limits::none()).expect("xdr"));
        let sig = secret_key.sign(&sign_data);

        let node_id = XdrNodeId(stellar_xdr::curr::PublicKey::PublicKeyTypeEd25519(
            stellar_xdr::curr::Uint256(*secret_key.public_key().as_bytes()),
        ));

        let stellar_value = StellarValue {
            tx_set_hash: xdr_tx_set_hash,
            close_time,
            upgrades: vec![].try_into().unwrap(),
            ext: StellarValueExt::Signed(LedgerCloseValueSignature {
                node_id,
                signature: stellar_xdr::curr::Signature(
                    sig.0.to_vec().try_into().unwrap_or_default(),
                ),
            }),
        };
        let value_bytes = stellar_value.to_xdr(Limits::none()).unwrap();
        Value(value_bytes.try_into().unwrap())
    }

    fn sign_statement(
        statement: &ScpStatement,
        herder: &Herder,
        secret: &SecretKey,
    ) -> ScpEnvelope {
        let statement_bytes = statement.to_xdr(Limits::none()).unwrap();
        let mut data = herder.scp_driver.network_id().0.to_vec();
        data.extend_from_slice(&1i32.to_be_bytes()); // ENVELOPE_TYPE_SCP = 1
        data.extend_from_slice(&statement_bytes);

        let signature = secret.sign(&data);
        let sig_bytes: Vec<u8> = signature.as_bytes().to_vec();

        ScpEnvelope {
            statement: statement.clone(),
            signature: XdrSignature(sig_bytes.try_into().unwrap()),
        }
    }

    /// Creates a test envelope with a valid signature for the given herder's network.
    fn make_signed_test_envelope(slot: u64, herder: &Herder) -> ScpEnvelope {
        // Generate a test keypair
        let secret = SecretKey::from_seed(&[1u8; 32]);
        let public = secret.public_key();

        let node_id = XdrNodeId(stellar_xdr::curr::PublicKey::PublicKeyTypeEd25519(
            stellar_xdr::curr::Uint256(*public.as_bytes()),
        ));

        // Include a vote with a valid close time so the envelope passes
        // check_envelope_close_time filtering (matching stellar-core which always has values)
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let vote = make_value_with_close_time(now);

        let statement = ScpStatement {
            node_id: node_id.clone(),
            slot_index: slot,
            pledges: ScpStatementPledges::Nominate(ScpNomination {
                quorum_set_hash: stellar_xdr::curr::Hash([0u8; 32]),
                votes: vec![vote].try_into().unwrap(),
                accepted: vec![].try_into().unwrap(),
            }),
        };

        // Sign the statement with network ID + ENVELOPE_TYPE_SCP prefix
        // (same format as verify_envelope expects)
        let statement_bytes = statement.to_xdr(Limits::none()).unwrap();
        let mut data = herder.scp_driver.network_id().0.to_vec();
        data.extend_from_slice(&1i32.to_be_bytes()); // ENVELOPE_TYPE_SCP = 1
        data.extend_from_slice(&statement_bytes);

        let signature = secret.sign(&data);
        let sig_bytes: Vec<u8> = signature.as_bytes().to_vec();

        ScpEnvelope {
            statement,
            signature: XdrSignature(sig_bytes.try_into().unwrap()),
        }
    }

    fn make_test_envelope(slot: u64) -> ScpEnvelope {
        let node_id = XdrNodeId(stellar_xdr::curr::PublicKey::PublicKeyTypeEd25519(
            stellar_xdr::curr::Uint256([0u8; 32]),
        ));

        ScpEnvelope {
            statement: ScpStatement {
                node_id,
                slot_index: slot,
                pledges: ScpStatementPledges::Nominate(ScpNomination {
                    quorum_set_hash: stellar_xdr::curr::Hash([0u8; 32]),
                    votes: vec![].try_into().unwrap(),
                    accepted: vec![].try_into().unwrap(),
                }),
            },
            signature: XdrSignature(vec![0u8; 64].try_into().unwrap()),
        }
    }

    #[test]
    fn test_initial_state() {
        let herder = make_test_herder();
        assert_eq!(herder.state(), HerderState::Booting);
        assert!(!herder.is_tracking());
    }

    #[test]
    fn test_bootstrap() {
        let herder = make_test_herder();

        herder.start_syncing();
        assert_eq!(herder.state(), HerderState::Syncing);

        herder.bootstrap(100);
        assert_eq!(herder.state(), HerderState::Tracking);
        assert_eq!(herder.tracking_slot(), 101);
        assert!(herder.is_tracking());
    }

    #[test]
    fn test_receive_envelope_before_tracking() {
        let herder = make_test_herder();

        let envelope = make_test_envelope(100);
        let result = herder.receive_scp_envelope(envelope);

        // Should be invalid because we're not syncing or tracking
        assert_eq!(result, EnvelopeState::Invalid);
    }

    #[test]
    fn test_receive_envelope_while_syncing() {
        let herder = make_test_herder();
        herder.start_syncing();

        // Syncing but not yet tracking, envelopes go to pending
        // Use signed envelope to pass signature verification
        let envelope = make_signed_test_envelope(100, &herder);

        // We need to set a current slot first
        herder.pending_envelopes.set_current_slot(95);

        let result = herder.receive_scp_envelope(envelope);
        assert_eq!(result, EnvelopeState::Pending);
    }

    #[test]
    fn test_stats() {
        let herder = make_test_herder();
        herder.bootstrap(50);

        let stats = herder.stats();
        assert_eq!(stats.state, HerderState::Tracking);
        assert_eq!(stats.tracking_slot, 51);
        assert_eq!(stats.pending_transactions, 0);
        assert!(!stats.is_validator);
    }

    // MAX_EXTERNALIZE_SLOT_DISTANCE was removed — stellar-core has no such limit.
    // When not tracking, stellar-core accepts EXTERNALIZE for any slot (maxLedgerSeq = uint32::max).

    /// Creates a signed EXTERNALIZE envelope for testing.
    fn make_signed_externalize_envelope(slot: u64, herder: &Herder) -> ScpEnvelope {
        let secret = SecretKey::from_seed(&[1u8; 32]);
        let public = secret.public_key();

        let node_id = XdrNodeId(stellar_xdr::curr::PublicKey::PublicKeyTypeEd25519(
            stellar_xdr::curr::Uint256(*public.as_bytes()),
        ));

        // Create a minimal valid StellarValue for the externalized value
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let stellar_value = stellar_xdr::curr::StellarValue {
            tx_set_hash: stellar_xdr::curr::Hash([0u8; 32]),
            close_time: stellar_xdr::curr::TimePoint(now),
            upgrades: vec![].try_into().unwrap(),
            ext: stellar_xdr::curr::StellarValueExt::Basic,
        };
        let value_bytes = stellar_value.to_xdr(Limits::none()).unwrap();

        let statement = ScpStatement {
            node_id: node_id.clone(),
            slot_index: slot,
            pledges: ScpStatementPledges::Externalize(ScpStatementExternalize {
                commit: ScpBallot {
                    counter: 1,
                    value: Value(value_bytes.try_into().unwrap()),
                },
                n_h: 1,
                commit_quorum_set_hash: stellar_xdr::curr::Hash([0u8; 32]),
            }),
        };

        // Sign the statement
        let statement_bytes = statement.to_xdr(Limits::none()).unwrap();
        let mut data = herder.scp_driver.network_id().0.to_vec();
        data.extend_from_slice(&1i32.to_be_bytes()); // ENVELOPE_TYPE_SCP = 1
        data.extend_from_slice(&statement_bytes);

        let signature = secret.sign(&data);
        let sig_bytes: Vec<u8> = signature.as_bytes().to_vec();

        ScpEnvelope {
            statement,
            signature: XdrSignature(sig_bytes.try_into().unwrap()),
        }
    }

    #[test]
    fn test_externalize_rejected_when_node_not_in_quorum() {
        let herder = make_test_herder();
        herder.start_syncing();
        herder.bootstrap(100);

        // Create an EXTERNALIZE envelope from a node that is NOT in our quorum
        // (the test herder has no quorum set configured, so no nodes are in quorum)
        let envelope = make_signed_externalize_envelope(105, &herder);

        let result = herder.receive_scp_envelope(envelope);

        // Should be rejected because sender is not in our transitive quorum
        assert_eq!(result, EnvelopeState::Invalid);
    }

    #[test]
    fn test_externalize_accepted_for_far_future_slot() {
        // When tracking, stellar-core applies LEDGER_VALIDITY_BRACKET to limit how far
        // ahead we accept messages. This test verifies EXTERNALIZE within the
        // bracket is accepted, and that it advances our tracking slot.
        let secret = SecretKey::from_seed(&[1u8; 32]);
        let public = secret.public_key();
        let test_node_id = node_id_from_public_key(&public);

        let quorum_set = ScpQuorumSet {
            threshold: 1,
            validators: vec![test_node_id.clone()].try_into().unwrap(),
            inner_sets: vec![].try_into().unwrap(),
        };

        let config = HerderConfig {
            local_quorum_set: Some(quorum_set),
            ..HerderConfig::default()
        };
        let herder = Herder::new(config);
        herder.start_syncing();
        herder.bootstrap(100);

        // Add the test node to the quorum tracker
        let test_qs = ScpQuorumSet {
            threshold: 1,
            validators: vec![test_node_id.clone()].try_into().unwrap(),
            inner_sets: vec![].try_into().unwrap(),
        };
        herder.quorum_tracker.write().expand(&test_node_id, test_qs);

        // Create an EXTERNALIZE envelope within the validity bracket
        // next_consensus_ledger_index() = 100, bracket = 100, so max = 200
        let future_slot = 195;
        let envelope = make_signed_externalize_envelope(future_slot, &herder);

        // Cache a dummy tx_set so the EXTERNALIZE handler can find it
        let dummy_tx_set = TransactionSet {
            hash: Hash256([0u8; 32]),
            previous_ledger_hash: Hash256::ZERO,
            transactions: vec![],
            generalized_tx_set: None,
        };
        herder.cache_tx_set(dummy_tx_set);

        let result = herder.receive_scp_envelope(envelope);

        // Should be accepted — within LEDGER_VALIDITY_BRACKET
        assert_eq!(result, EnvelopeState::Valid);
        // Tracking slot should have advanced
        assert_eq!(herder.tracking_slot(), future_slot + 1);
    }

    #[test]
    fn test_externalize_accepted_when_within_distance_and_in_quorum() {
        // Create a herder with a quorum set that includes our test node
        let secret = SecretKey::from_seed(&[1u8; 32]);
        let public = secret.public_key();
        let test_node_id = node_id_from_public_key(&public);

        let quorum_set = ScpQuorumSet {
            threshold: 1,
            validators: vec![test_node_id.clone()].try_into().unwrap(),
            inner_sets: vec![].try_into().unwrap(),
        };

        let config = HerderConfig {
            local_quorum_set: Some(quorum_set),
            ..HerderConfig::default()
        };
        let herder = Herder::new(config);
        herder.start_syncing();
        herder.bootstrap(100);

        // Add the test node to the quorum tracker
        let test_qs = ScpQuorumSet {
            threshold: 1,
            validators: vec![test_node_id.clone()].try_into().unwrap(),
            inner_sets: vec![].try_into().unwrap(),
        };
        herder.quorum_tracker.write().expand(&test_node_id, test_qs);

        // Create an EXTERNALIZE envelope for a slot within acceptable distance
        let acceptable_slot = 100 + 50; // 150, well within MAX_EXTERNALIZE_SLOT_DISTANCE
        let envelope = make_signed_externalize_envelope(acceptable_slot, &herder);

        // Cache a dummy tx_set with the hash used in make_signed_externalize_envelope ([0u8; 32])
        // The EXTERNALIZE handler requires the tx_set to be available before accepting the envelope
        let dummy_tx_set = TransactionSet {
            hash: Hash256([0u8; 32]),
            previous_ledger_hash: Hash256::ZERO,
            transactions: vec![],
            generalized_tx_set: None,
        };
        herder.cache_tx_set(dummy_tx_set);

        let result = herder.receive_scp_envelope(envelope);

        // Should be accepted and cause fast-forward
        assert_eq!(result, EnvelopeState::Valid);
        // Tracking slot should have advanced (to slot + 1, since EXTERNALIZE completes that slot)
        assert_eq!(herder.tracking_slot(), acceptable_slot + 1);
    }

    #[test]
    fn test_externalize_transitions_syncing_to_tracking() {
        // Matches stellar-core behavior: setTrackingSCPState(slotIndex, b, true)
        // always transitions to HERDER_TRACKING_NETWORK_STATE on externalization,
        // even if the herder was previously in SYNCING state.
        let secret = SecretKey::from_seed(&[1u8; 32]);
        let public = secret.public_key();
        let test_node_id = node_id_from_public_key(&public);

        let quorum_set = ScpQuorumSet {
            threshold: 1,
            validators: vec![test_node_id.clone()].try_into().unwrap(),
            inner_sets: vec![].try_into().unwrap(),
        };

        let config = HerderConfig {
            local_quorum_set: Some(quorum_set),
            ..HerderConfig::default()
        };
        let herder = Herder::new(config);
        herder.start_syncing();
        herder.bootstrap(100);

        // Add the test node to the quorum tracker
        let test_qs = ScpQuorumSet {
            threshold: 1,
            validators: vec![test_node_id.clone()].try_into().unwrap(),
            inner_sets: vec![].try_into().unwrap(),
        };
        herder.quorum_tracker.write().expand(&test_node_id, test_qs);

        // Simulate losing sync: force herder back to Syncing
        herder.set_state(HerderState::Syncing);
        assert_eq!(herder.state(), HerderState::Syncing);

        // Cache a dummy tx_set so the EXTERNALIZE handler can find it
        let dummy_tx_set = TransactionSet {
            hash: Hash256([0u8; 32]),
            previous_ledger_hash: Hash256::ZERO,
            transactions: vec![],
            generalized_tx_set: None,
        };
        herder.cache_tx_set(dummy_tx_set);

        // Process an EXTERNALIZE — should transition back to Tracking
        let envelope = make_signed_externalize_envelope(105, &herder);
        let result = herder.receive_scp_envelope(envelope);

        assert_eq!(result, EnvelopeState::Valid);
        assert_eq!(
            herder.state(),
            HerderState::Tracking,
            "Externalization should transition herder from Syncing to Tracking"
        );
    }

    #[test]
    fn test_nomination_timeout_requires_started() {
        let (herder, secret) = make_validator_herder();
        let scp = herder.scp().expect("validator scp");
        let slot = 1u64;

        assert!(herder.get_nomination_timeout(slot).is_none());

        let value = make_valid_value_with_cached_tx_set(&herder, &secret);
        let prev_value = value.clone();
        assert!(scp.nominate(slot, value, &prev_value));

        assert!(herder.get_nomination_timeout(slot).is_some());
    }

    #[test]
    fn test_ballot_timeout_requires_heard_from_quorum() {
        // Use a 2-node quorum (threshold 2) so that the local node's self-envelope
        // cannot cascade all the way to externalization via recursive self-processing.
        // We then send a PREPARE from the second node so heard_from_quorum is satisfied.
        let seed = [7u8; 32];
        let secret_for_herder = SecretKey::from_seed(&seed);
        let public = secret_for_herder.public_key();
        let local_node_id = XdrNodeId(stellar_xdr::curr::PublicKey::PublicKeyTypeEd25519(
            stellar_xdr::curr::Uint256(*public.as_bytes()),
        ));

        // Second node
        let other_secret = SecretKey::from_seed(&[8u8; 32]);
        let other_public = other_secret.public_key();
        let other_node_id = XdrNodeId(stellar_xdr::curr::PublicKey::PublicKeyTypeEd25519(
            stellar_xdr::curr::Uint256(*other_public.as_bytes()),
        ));

        let quorum_set = ScpQuorumSet {
            threshold: 2,
            validators: vec![local_node_id.clone(), other_node_id.clone()]
                .try_into()
                .unwrap(),
            inner_sets: vec![].try_into().unwrap(),
        };

        let config = HerderConfig {
            is_validator: true,
            node_public_key: public,
            local_quorum_set: Some(quorum_set.clone()),
            ..HerderConfig::default()
        };

        let herder = Herder::with_secret_key(config, secret_for_herder);
        let secret = SecretKey::from_seed(&seed);
        let scp = herder.scp().expect("validator scp");
        let slot = 1u64;

        assert!(herder.get_ballot_timeout(slot).is_none());

        // Create a valid value and start ballot protocol via bump_state
        let value = make_valid_value_with_cached_tx_set(&herder, &secret);
        assert!(scp.bump_state(slot, value.clone(), 1));

        // Ballot started but no quorum heard yet (only local node's PREPARE)
        assert!(
            herder.get_ballot_timeout(slot).is_none(),
            "should not have ballot timeout without quorum"
        );

        // Send a PREPARE from the second node to form quorum
        let qs_hash = hash_quorum_set(&quorum_set);
        let ballot = ScpBallot {
            counter: 1,
            value: value.clone(),
        };
        let prepare = ScpStatementPrepare {
            quorum_set_hash: qs_hash.into(),
            ballot: ballot.clone(),
            prepared: None,
            prepared_prime: None,
            n_c: 0,
            n_h: 0,
        };
        let statement = ScpStatement {
            node_id: other_node_id,
            slot_index: slot,
            pledges: ScpStatementPledges::Prepare(prepare),
        };
        let env = sign_statement(&statement, &herder, &other_secret);
        scp.receive_envelope(env);

        // Now we should have heard from quorum (local + other = 2, threshold = 2)
        assert!(herder.get_ballot_timeout(slot).is_some());
    }

    #[test]
    fn test_timeouts_none_for_non_validator() {
        let herder = make_test_herder();
        let slot = 1u64;

        assert!(herder.get_nomination_timeout(slot).is_none());
        assert!(herder.get_ballot_timeout(slot).is_none());
    }

    #[test]
    fn test_ballot_timeout_none_when_externalized() {
        let (herder, secret) = make_validator_herder();
        let scp = herder.scp().expect("validator scp");
        let slot = 1u64;

        let value = make_valid_value_with_cached_tx_set(&herder, &secret);
        scp.force_externalize(slot, value);

        assert!(herder.get_ballot_timeout(slot).is_none());
    }

    // =========================================================================
    // Phase 6 H1 parity tests — close-time validation in Herder
    // =========================================================================

    /// Create a StellarValue with a given close time and encode to Value.
    fn make_value_with_close_time(close_time: u64) -> Value {
        let sv = StellarValue {
            tx_set_hash: stellar_xdr::curr::Hash([0u8; 32]),
            close_time: TimePoint(close_time),
            upgrades: vec![].try_into().unwrap(),
            ext: StellarValueExt::Basic,
        };
        Value(sv.to_xdr(Limits::none()).unwrap().try_into().unwrap())
    }

    /// Create a nomination envelope with a specific close time in its voted values.
    fn make_nomination_envelope_with_close_time(slot: u64, close_time: u64) -> ScpEnvelope {
        let node_id = XdrNodeId(stellar_xdr::curr::PublicKey::PublicKeyTypeEd25519(
            stellar_xdr::curr::Uint256([1u8; 32]),
        ));
        let value = make_value_with_close_time(close_time);

        ScpEnvelope {
            statement: ScpStatement {
                node_id,
                slot_index: slot,
                pledges: ScpStatementPledges::Nominate(ScpNomination {
                    quorum_set_hash: stellar_xdr::curr::Hash([0u8; 32]),
                    votes: vec![value].try_into().unwrap(),
                    accepted: vec![].try_into().unwrap(),
                }),
            },
            signature: XdrSignature(vec![0u8; 64].try_into().unwrap()),
        }
    }

    #[test]
    fn test_get_most_recent_checkpoint_seq() {
        // Test checkpoint computation matches stellar-core HistoryManager
        let herder = make_test_herder();

        // Default tracking_slot is 0, so tracking_consensus_index = 0
        // first checkpoint: ((0/64 + 1) * 64) - 1 = 63, size = 63, first = 1
        assert_eq!(herder.get_most_recent_checkpoint_seq(), 1);

        // Bootstrap to slot 100 (tracking_slot = 101)
        herder.bootstrap(100);
        // tracking_consensus_index = 100
        // checkpoint containing 100: ((100/64 + 1) * 64) - 1 = 127, size = 64, first = 64
        assert_eq!(herder.get_most_recent_checkpoint_seq(), 64);

        // Bootstrap to slot 127 (tracking_slot = 128)
        herder.bootstrap(127);
        // checkpoint containing 127: ((127/64 + 1) * 64) - 1 = 127, size = 64, first = 64
        assert_eq!(herder.get_most_recent_checkpoint_seq(), 64);

        // Bootstrap to slot 129 (tracking_slot = 129, LCL = 128)
        herder.bootstrap(129);
        // checkpoint containing 128: ((128/64 + 1) * 64) - 1 = 191, size = 64, first = 128
        assert_eq!(herder.get_most_recent_checkpoint_seq(), 128);
    }

    #[test]
    fn test_check_envelope_close_time_basic() {
        // check_envelope_close_time(envelope, false) — basic structural check
        let herder = make_test_herder();
        herder.bootstrap(100);

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();

        // Envelope for a future slot with valid close time should pass
        let env = make_nomination_envelope_with_close_time(102, now);
        assert!(herder.check_envelope_close_time(&env, false));

        // Envelope with close time 0 (ancient) should fail for a future slot
        // because check_close_time requires close_time > last_close_time
        // Here last_close_time is tracking_consensus_close_time (0 from bootstrap)
        // and close_time = 0, so 0 > 0 is false -> but the three-case logic:
        // last_close_index(101) > env_ledger_index(1) for a very old slot
        let env_old = make_nomination_envelope_with_close_time(1, 0);
        // This is case 2 (older slot): last_close_index > env_ledger_index
        // but last_close_time(0) > close_time(0) is false -> fails
        // However last_close_index=101, env_ledger_index=1, so case 2 applies
        // but last_close_time=0, close_time=0, so 0 > 0 fails
        // So this should fail
        assert!(!herder.check_envelope_close_time(&env_old, false));
    }

    #[test]
    fn test_check_envelope_close_time_recency_enforcement() {
        // check_envelope_close_time(envelope, true) — enforces recency
        let herder = make_test_herder();
        herder.bootstrap(100);

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();

        // Recent close time should pass
        let env_recent = make_nomination_envelope_with_close_time(102, now);
        assert!(herder.check_envelope_close_time(&env_recent, true));

        // Close time older than MAXIMUM_LEDGER_CLOSETIME_DRIFT should fail with enforce_recent
        let env_stale = make_nomination_envelope_with_close_time(102, now - 200);
        assert!(!herder.check_envelope_close_time(&env_stale, true));
    }

    #[test]
    fn test_check_envelope_close_time_exact_match() {
        // Case 1: envelope is for same slot as last_close_index and close time matches
        // This represents a late message for an already-externalized slot
        let herder = make_test_herder();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        herder.bootstrap(100);
        // Set tracking consensus close time
        *herder.tracking_consensus_close_time.write() = now;

        // After bootstrap(100): tracking_slot=100, LCL=99
        // check_envelope_close_time uses tracking_index = tracking_slot - 1 = 99
        // So for exact match: envelope must be for slot 99 with close_time = now
        let env = make_nomination_envelope_with_close_time(99, now);
        assert!(herder.check_envelope_close_time(&env, false));
    }

    #[test]
    fn test_receive_scp_envelope_rejects_bad_close_time_before_sig_verify() {
        // Close-time filtering should happen before signature verification
        // so even an envelope with an invalid signature should be rejected
        // with Invalid (not InvalidSignature) if close time is bad
        let herder = make_test_herder();
        herder.bootstrap(100);

        // Create envelope with close time 0 for a future slot (102)
        // This has no valid signature, but close-time check should reject first
        let env = make_nomination_envelope_with_close_time(102, 0);
        let result = herder.receive_scp_envelope(env);
        // Should be Invalid due to close-time, NOT InvalidSignature
        assert_eq!(result, EnvelopeState::Invalid);
    }

    /// Regression test for root cause #5: solo validator should include upgrades
    /// in externalized value when runtime upgrades are armed.
    #[tokio::test]
    async fn test_trigger_next_ledger_includes_runtime_upgrades() {
        let (herder, _secret) = make_validator_herder();

        // Set runtime upgrades: protocol version 25, base reserve 5000000
        let upgrade_params = crate::upgrades::UpgradeParameters {
            upgrade_time: 0, // immediate
            protocol_version: Some(25),
            base_reserve: Some(5_000_000),
            ..Default::default()
        };
        herder
            .set_upgrade_parameters(upgrade_params)
            .expect("set_upgrade_parameters should succeed");

        // Bootstrap herder to Tracking state (required for trigger_next_ledger)
        herder.bootstrap(1);
        assert_eq!(herder.state(), HerderState::Tracking);

        // Trigger consensus for ledger 2 (no LedgerManager, so header defaults
        // to version=0, base_reserve=0 — upgrades should fire)
        let result = herder.trigger_next_ledger(2).await;
        assert!(
            result.is_ok(),
            "trigger_next_ledger should succeed: {:?}",
            result.err()
        );

        // For a solo validator (1-of-1 quorum), nomination→ballot→externalization
        // happens synchronously. The externalized value should contain the upgrades.
        let externalized = herder.scp_driver.get_externalized(2);
        assert!(
            externalized.is_some(),
            "Slot 2 should be externalized for solo validator"
        );

        let ext = externalized.unwrap();
        let stellar_value =
            StellarValue::from_xdr(&ext.value, Limits::none()).expect("should parse StellarValue");

        // Verify upgrades are present
        assert!(
            !stellar_value.upgrades.is_empty(),
            "Externalized StellarValue should contain upgrades, but upgrades vec is empty"
        );

        // Decode and verify the specific upgrades
        let mut found_version = false;
        let mut found_reserve = false;
        for upgrade_bytes in stellar_value.upgrades.iter() {
            if let Ok(upgrade) = LedgerUpgrade::from_xdr(&upgrade_bytes.0, Limits::none()) {
                match upgrade {
                    LedgerUpgrade::Version(v) => {
                        assert_eq!(v, 25, "Expected protocol version 25");
                        found_version = true;
                    }
                    LedgerUpgrade::BaseReserve(r) => {
                        assert_eq!(r, 5_000_000, "Expected base reserve 5000000");
                        found_reserve = true;
                    }
                    other => panic!("Unexpected upgrade: {:?}", other),
                }
            }
        }
        assert!(found_version, "Should have found Version(25) upgrade");
        assert!(
            found_reserve,
            "Should have found BaseReserve(5000000) upgrade"
        );
    }

    /// Regression test for Task 7: genesis-adjacent close-time relaxation.
    ///
    /// When a node is at genesis (tracking_consensus_index <= GENESIS_LEDGER_SEQ)
    /// and an envelope arrives for the next consensus slot, enforce_recent should
    /// be false so the envelope is NOT rejected by the recency filter.
    ///
    /// stellar-core computes:
    ///   enforceRecent = trackingConsensusLedgerIndex() <= GENESIS_LEDGER_SEQ
    ///                   && index != nextConsensusLedgerIndex()
    ///
    /// Without the `&& slot != next_consensus` condition, the next consensus
    /// slot's envelope would be incorrectly subject to the recency filter.
    #[test]
    fn test_genesis_close_time_relaxation_for_next_consensus_slot() {
        let herder = make_test_herder();
        // Put herder in Syncing state (non-tracking) with tracking_slot = 0
        // tracking_consensus_index = tracking_slot.saturating_sub(1) = 0 <= GENESIS_LEDGER_SEQ
        herder.set_state(HerderState::Syncing);
        assert!(!herder.state().is_tracking());

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();

        // next_consensus_ledger_index() = tracking_slot = 0
        // Create a stale-but-structurally-valid close time
        let stale_time = now.saturating_sub(MAXIMUM_LEDGER_CLOSETIME_DRIFT + 60);

        // Envelope for slot 0 (the next consensus slot) with stale close time
        let env_stale = make_nomination_envelope_with_close_time(0, stale_time);

        // Direct test: enforce_recent=false should accept stale close time
        assert!(
            herder.check_envelope_close_time(&env_stale, false),
            "stale close time should pass with enforce_recent=false"
        );
        // enforce_recent=true should reject it (below cutoff)
        assert!(
            !herder.check_envelope_close_time(&env_stale, true),
            "stale close time should fail with enforce_recent=true"
        );

        // Full receive path: for the next consensus slot at genesis,
        // our fix computes enforce_recent = (0 <= 1) && (0 != 0) = false,
        // so the stale envelope passes the non-tracking recency gate.
        // It gets rejected later (e.g. by signature check), but NOT by recency.
        // A stale envelope for a DIFFERENT slot should be rejected by recency:
        let other_slot_env = make_nomination_envelope_with_close_time(5, stale_time);
        // For slot 5: enforce_recent = (0 <= 1) && (5 != 0) = true → rejected
        let result_other = herder.receive_scp_envelope(other_slot_env);
        assert_eq!(
            result_other,
            EnvelopeState::Invalid,
            "stale close time for non-next-consensus slot at genesis should be rejected"
        );
    }

    // =========================================================================
    // SCP ballot protocol participation tests
    // =========================================================================

    /// Creates a signed EXTERNALIZE envelope from a specific secret key,
    /// with a properly signed StellarValue (STELLAR_VALUE_SIGNED).
    fn make_signed_externalize_from(slot: u64, herder: &Herder, secret: &SecretKey) -> ScpEnvelope {
        let public = secret.public_key();
        let node_id = XdrNodeId(stellar_xdr::curr::PublicKey::PublicKeyTypeEd25519(
            stellar_xdr::curr::Uint256(*public.as_bytes()),
        ));

        // Use the value helper that caches the tx set and signs properly
        let value = make_valid_value_with_cached_tx_set(herder, secret);

        let statement = ScpStatement {
            node_id: node_id.clone(),
            slot_index: slot,
            pledges: ScpStatementPledges::Externalize(ScpStatementExternalize {
                commit: ScpBallot { counter: 1, value },
                n_h: 1,
                commit_quorum_set_hash: stellar_xdr::curr::Hash([0u8; 32]),
            }),
        };

        sign_statement(&statement, herder, secret)
    }

    #[test]
    fn test_externalize_goes_through_scp_for_validator() {
        // Verify that a validator processes EXTERNALIZE for the current tracking
        // slot through SCP (not force-externalize). This is the key behavioral
        // change: SCP processes the EXTERNALIZE internally, which allows the
        // node to participate in the ballot protocol and send its own messages.
        let local_secret = SecretKey::from_seed(&[7u8; 32]);
        let local_public = local_secret.public_key();
        let local_node_id = node_id_from_public_key(&local_public);

        let other_secret = SecretKey::from_seed(&[1u8; 32]);
        let other_public = other_secret.public_key();
        let other_node_id = node_id_from_public_key(&other_public);

        let quorum_set = ScpQuorumSet {
            threshold: 1,
            validators: vec![local_node_id.clone(), other_node_id.clone()]
                .try_into()
                .unwrap(),
            inner_sets: vec![].try_into().unwrap(),
        };

        let config = HerderConfig {
            is_validator: true,
            node_public_key: local_public,
            local_quorum_set: Some(quorum_set.clone()),
            ..HerderConfig::default()
        };

        let herder = Herder::with_secret_key(config, SecretKey::from_seed(&[7u8; 32]));
        herder.start_syncing();
        herder.bootstrap(100);

        herder
            .quorum_tracker
            .write()
            .expand(&other_node_id, quorum_set);

        let tracking = herder.tracking_slot(); // 101

        // Track emitted envelopes to verify SCP participation
        let emitted = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let emitted_clone = emitted.clone();
        herder.scp_driver.set_envelope_sender(move |env| {
            emitted_clone.lock().unwrap().push(env);
        });

        // Process EXTERNALIZE through SCP — should externalize via SCP consensus
        let envelope = make_signed_externalize_from(tracking, &herder, &other_secret);
        let result = herder.receive_scp_envelope(envelope);

        assert_eq!(result, EnvelopeState::Valid);

        // SCP should have externalized the slot internally
        let scp = herder.scp().expect("validator has scp");
        assert!(
            scp.is_slot_externalized(tracking),
            "SCP should externalize when receiving valid EXTERNALIZE"
        );

        // Herder should have recorded the externalization
        assert_eq!(
            herder.latest_externalized_slot(),
            Some(tracking),
            "herder should record externalized slot"
        );
        assert_eq!(
            herder.tracking_slot(),
            tracking + 1,
            "tracking slot should advance after externalization"
        );

        // Verify SCP emitted ballot messages (EXTERNALIZE) — proves the node
        // participates in the ballot protocol instead of force-externalizing
        let msgs = emitted.lock().unwrap();
        assert!(
            !msgs.is_empty(),
            "SCP should emit ballot messages when processing EXTERNALIZE"
        );
        // The emitted message should be an EXTERNALIZE from our local node
        let has_externalize = msgs
            .iter()
            .any(|env| matches!(env.statement.pledges, ScpStatementPledges::Externalize(_)));
        assert!(
            has_externalize,
            "SCP should emit its own EXTERNALIZE message"
        );
    }
}

// NOTE: Additional set_state tests are appended below (outside the `mod tests` block
// was closed above). We re-open a *second* cfg(test) module to avoid touching
// earlier test code.

#[cfg(test)]
mod set_state_tests {
    use super::*;

    fn make_test_herder() -> Herder {
        Herder::new(HerderConfig::default())
    }

    // =========================================================================
    // set_state — forbidden transition tests
    // =========================================================================

    #[test]
    fn test_set_state_tracking_to_booting_is_forbidden() {
        let herder = make_test_herder();
        // Advance to Tracking
        herder.set_state(HerderState::Syncing);
        herder.set_state(HerderState::Tracking);
        assert_eq!(herder.state(), HerderState::Tracking);

        // Attempt forbidden transition Tracking → Booting
        herder.set_state(HerderState::Booting);
        assert_eq!(
            herder.state(),
            HerderState::Tracking,
            "Tracking→Booting must be ignored"
        );
    }

    #[test]
    fn test_set_state_syncing_to_booting_is_forbidden() {
        let herder = make_test_herder();
        herder.set_state(HerderState::Syncing);
        assert_eq!(herder.state(), HerderState::Syncing);

        // Attempt forbidden transition Syncing → Booting
        herder.set_state(HerderState::Booting);
        assert_eq!(
            herder.state(),
            HerderState::Syncing,
            "Syncing→Booting must be ignored"
        );
    }

    #[test]
    fn test_set_state_allowed_transitions() {
        let herder = make_test_herder();

        // Booting → Syncing (allowed)
        herder.set_state(HerderState::Syncing);
        assert_eq!(herder.state(), HerderState::Syncing);

        // Syncing → Tracking (allowed)
        herder.set_state(HerderState::Tracking);
        assert_eq!(herder.state(), HerderState::Tracking);

        // Tracking → Syncing (allowed)
        herder.set_state(HerderState::Syncing);
        assert_eq!(herder.state(), HerderState::Syncing);

        // Syncing → Syncing (allowed, same state)
        herder.set_state(HerderState::Syncing);
        assert_eq!(herder.state(), HerderState::Syncing);

        // Back to Tracking
        herder.set_state(HerderState::Tracking);
        assert_eq!(herder.state(), HerderState::Tracking);

        // Tracking → Tracking (allowed, same state)
        herder.set_state(HerderState::Tracking);
        assert_eq!(herder.state(), HerderState::Tracking);
    }

    #[test]
    fn test_set_state_booting_to_tracking_directly() {
        let herder = make_test_herder();
        assert_eq!(herder.state(), HerderState::Booting);

        // Booting → Tracking (allowed)
        herder.set_state(HerderState::Tracking);
        assert_eq!(herder.state(), HerderState::Tracking);
    }

    #[test]
    fn test_set_state_booting_identity() {
        let herder = make_test_herder();
        assert_eq!(herder.state(), HerderState::Booting);

        // Booting → Booting (same state, should be allowed)
        herder.set_state(HerderState::Booting);
        assert_eq!(herder.state(), HerderState::Booting);
    }

    #[test]
    fn test_set_state_multiple_forbidden_attempts_do_not_change_state() {
        let herder = make_test_herder();
        herder.set_state(HerderState::Tracking);
        assert_eq!(herder.state(), HerderState::Tracking);

        // Multiple forbidden attempts
        for _ in 0..5 {
            herder.set_state(HerderState::Booting);
        }
        assert_eq!(
            herder.state(),
            HerderState::Tracking,
            "Repeated Tracking→Booting must all be ignored"
        );
    }
}
