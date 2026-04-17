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
use std::sync::atomic::{AtomicU64, Ordering};
use tracing::{debug, error, info, trace, warn};

use henyey_common::Hash256;
use henyey_crypto::{PublicKey, SecretKey};
use henyey_ledger::LedgerManager;
use henyey_scp::{BallotPhase, SlotIndex, SCP};
use stellar_xdr::curr::{
    EnvelopeType, LedgerCloseValueSignature, LedgerHeader, LedgerUpgrade, Limits, NodeId, ReadXdr,
    ScpEnvelope, ScpQuorumSet, ScpStatementPledges, Signature as XdrSignature, StellarValue,
    StellarValueExt, TimePoint, TransactionEnvelope, Uint256, UpgradeType, Value, WriteXdr,
};

use crate::error::HerderError;
use crate::fetching_envelopes::{FetchingEnvelopes, FetchingStats};

use crate::pending::{PendingConfig, PendingEnvelopes, PendingResult, PendingStats};
use crate::quorum_tracker::{QuorumTracker, SlotQuorumTracker};
use crate::scp_driver::{HerderScpCallback, ScpDriver, ScpDriverConfig, SharedTrackingState};
use crate::state::HerderState;
use crate::sync_recovery::LEDGER_VALIDITY_BRACKET;
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

/// Genesis ledger sequence number.
///
/// Matches stellar-core `LedgerManager::GENESIS_LEDGER_SEQ = 1`.
const GENESIS_LEDGER_SEQ: u64 = 1;

/// Default checkpoint frequency (64 ledgers).
const DEFAULT_CHECKPOINT_FREQUENCY: u64 = 64;

/// Maximum age (in seconds) for pending tx sets before cleanup.
///
/// We keep pending tx sets longer than the consensus round to allow lagging
/// nodes to fetch historical sets they missed.
const PENDING_TX_SET_MAX_AGE_SECS: u64 = 120;

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
    /// Always created — observers run SCP with `is_validator: false` (same as stellar-core),
    /// which means `fully_validated` is false on slots and `send_latest_envelope` won't emit.
    scp: SCP<HerderScpCallback>,
    /// Shared tracking consensus state (also read by ScpDriver for value validation).
    tracking_state: Arc<RwLock<SharedTrackingState>>,
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
    /// Shared with ScpDriver for nomination validation.
    runtime_upgrades: Arc<RwLock<Upgrades>>,
    /// Count of transactions dropped due to queue full since last ledger close.
    queue_full_count: AtomicU64,
    /// Cached nomination value for the current slot, reused on timeout retries.
    /// Parity: stellar-core captures the value by-copy in the nomination timer
    /// lambda (NominationProtocol.cpp:654-659) so timeouts replay the same value.
    cached_nomination_value: RwLock<Option<(SlotIndex, Value)>>,
    /// Handle to the dedicated SCP signature-verify worker thread.
    /// See [`crate::scp_verify`]. The worker is a core component; failure
    /// to spawn it is fatal (see [`Herder::build`]).
    scp_verifier_handle: crate::scp_verify::SignatureVerifierHandle,
    /// Receiver for verified envelopes. Taken exactly once by the event loop
    /// (see `Herder::take_verified_rx`).
    verified_rx: std::sync::Mutex<
        Option<tokio::sync::mpsc::UnboundedReceiver<crate::scp_verify::VerifiedEnvelope>>,
    >,
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

        let tracking_state = Arc::new(RwLock::new(SharedTrackingState::default()));

        let scp_driver = Arc::new(if let Some(ref sk) = secret_key {
            ScpDriver::with_secret_key(
                scp_driver_config,
                config.network_id,
                sk.clone(),
                Arc::clone(&tracking_state),
            )
        } else {
            ScpDriver::new(
                scp_driver_config,
                config.network_id,
                Arc::clone(&tracking_state),
            )
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

        // Always create SCP instance — stellar-core does the same even for watchers.
        // Observers run SCP with is_validator=false, which sets fully_validated=false
        // on slots so they never emit their own envelopes.
        let node_id = node_id_from_public_key(&config.node_public_key);
        let is_validator = secret_key.is_some() && config.is_validator;
        let quorum_set = config
            .local_quorum_set
            .clone()
            .unwrap_or_else(|| ScpQuorumSet {
                threshold: 0,
                validators: vec![].try_into().unwrap(),
                inner_sets: vec![].try_into().unwrap(),
            });
        if is_validator && config.local_quorum_set.is_none() {
            warn!("Validator mode requested but no quorum set configured");
        }
        let callback = HerderScpCallback::new(Arc::clone(&scp_driver));
        let scp = SCP::new(
            node_id.clone(),
            is_validator,
            quorum_set,
            Arc::new(callback),
        );

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

        let runtime_upgrades = Arc::new(RwLock::new(Upgrades::default()));
        scp_driver.set_upgrades(Arc::clone(&runtime_upgrades));

        // Spawn the dedicated SCP signature-verification worker thread.
        // The worker is a core component of the event-loop pipeline
        // (issue #1734 Phase B); spawn failure is fatal rather than
        // silently falling back to event-loop verification.
        let spawned = crate::scp_verify::spawn_scp_verifier(
            config.network_id,
            crate::scp_verify::DEFAULT_VERIFIER_QUEUE_CAPACITY,
        )
        .expect("scp-verify worker thread must spawn (OS resource exhaustion?)");
        let scp_verifier_handle = spawned.handle;
        let verified_rx = Some(spawned.verified_rx);

        Self {
            config,
            state: RwLock::new(HerderState::Booting),
            tx_queue,
            pending_envelopes,
            fetching_envelopes,
            scp_driver,
            scp,
            tracking_state,
            tracking_started_at: RwLock::new(None),
            secret_key,
            ledger_manager: RwLock::new(None),
            prev_value: RwLock::new(Value::default()),
            slot_quorum_tracker: RwLock::new(slot_quorum_tracker),
            quorum_tracker: RwLock::new(quorum_tracker),
            runtime_upgrades,
            queue_full_count: AtomicU64::new(0),
            cached_nomination_value: RwLock::new(None),
            scp_verifier_handle,
            verified_rx: std::sync::Mutex::new(verified_rx),
        }
    }

    /// Handle to the dedicated SCP signature-verification worker.
    /// The event loop uses this to enqueue pre-filtered envelopes and the
    /// watchdog uses it to monitor liveness. The worker is guaranteed to
    /// exist (see [`Herder::build`]).
    pub fn scp_verifier_handle(&self) -> crate::scp_verify::SignatureVerifierHandle {
        self.scp_verifier_handle.clone()
    }

    /// Take the verified-envelope receiver. Exactly one consumer (the event
    /// loop) should call this once at startup; subsequent calls return `None`.
    pub fn take_verified_rx(
        &self,
    ) -> Option<tokio::sync::mpsc::UnboundedReceiver<crate::scp_verify::VerifiedEnvelope>> {
        self.verified_rx.lock().ok()?.take()
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

    /// Test-only: set the wall-clock override (seconds since UNIX epoch) seen
    /// by both `Herder::check_envelope_close_time` and
    /// `ScpDriver::check_close_time`. A value of `0` restores
    /// `SystemTime::now()` behavior.
    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    pub fn set_test_clock_seconds(&self, now: u64) {
        self.scp_driver
            .test_clock_handle()
            .store(now, std::sync::atomic::Ordering::Relaxed);
    }

    /// Test-only: overwrite the shared tracking state used by the close-time
    /// pre-filter and gate-drift recheck. Bypasses the `bootstrap` path so
    /// tests can pre-seed a specific `(slot, close_time)` without needing a
    /// live `LedgerManager`.
    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    pub fn set_tracking_for_testing(&self, slot: u64, close_time: u64) {
        let mut ts = self.tracking_state.write();
        ts.is_tracking = true;
        ts.consensus_index = slot;
        ts.consensus_close_time = close_time;
    }

    /// Test-only: expand the transitive quorum tracker with `(root, qset)`.
    /// Needed by integration tests to exercise the non-quorum gate in
    /// `process_verified` (which consults `quorum_tracker`, not the SCP
    /// driver's tracker).
    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    pub fn expand_quorum_tracker_for_testing(
        &self,
        root: &NodeId,
        qset: ScpQuorumSet,
    ) -> std::result::Result<(), String> {
        self.quorum_tracker
            .write()
            .expand(root, qset)
            .map_err(|e| format!("{:?}", e))
    }

    /// Test-only: set the pending-envelopes current-slot cursor, so envelopes
    /// for that slot bypass buffering and go straight into the
    /// `process_scp_envelope` path in `process_verified`.
    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    pub fn set_pending_current_slot_for_testing(&self, slot: u64) {
        self.pending_envelopes.set_current_slot(slot);
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

        // Keep SharedTrackingState.is_tracking in sync with HerderState.
        // stellar-core's isTracking() simply checks mState == HERDER_TRACKING_NETWORK_STATE,
        // but ScpDriver reads SharedTrackingState.is_tracking for value validation.
        // Without this, transitioning to Syncing leaves is_tracking=true, causing
        // validate_past_or_future_value to reject far-future EXTERNALIZE messages
        // instead of returning MaybeValid.
        if !state.is_tracking() {
            self.tracking_state.write().is_tracking = false;
        }
    }

    /// Get the current tracking slot.
    pub fn tracking_slot(&self) -> u64 {
        self.tracking_state.read().consensus_index
    }

    /// Get the tracking consensus close time.
    /// Matches stellar-core `HerderImpl::trackingConsensusCloseTime()`.
    pub fn tracking_consensus_close_time(&self) -> u64 {
        self.tracking_state.read().consensus_close_time
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
        // Number of extra ledgers to keep beyond max_externalized_slots, matching
        // stellar-core's LEDGER_VALIDITY_BRACKET lookback cushion.
        const PEER_LEDGER_WINDOW: u32 = 3;
        let window = max_slots.min(PEER_LEDGER_WINDOW);
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
        // Compute hash before storing so we can notify fetching_envelopes.
        let qs_hash = henyey_scp::hash_quorum_set(&quorum_set);

        self.scp_driver
            .store_quorum_set(node_id, quorum_set.clone());
        let mut tracker = self.quorum_tracker.write();
        if tracker.expand(node_id, quorum_set.clone()).is_err() {
            if let Err(err) = tracker.rebuild(|id| self.scp_driver.get_quorum_set(id)) {
                warn!(error = %err, "Failed to rebuild quorum tracker");
            }
        }

        // Mirror quorum-set receipt into FetchingEnvelopes so blocked envelopes
        // that are waiting for this quorum set get unblocked. Without this,
        // store_quorum_set only updates ScpDriver and the quorum tracker,
        // leaving FetchingEnvelopes unaware (AUDIT-004).
        self.fetching_envelopes.recv_quorum_set(qs_hash, quorum_set);

        // Drain ready queue — envelopes unblocked by this quorum set need
        // to be fed to SCP. Mirrors receive_tx_set() which also drains
        // after notifying FetchingEnvelopes (AUDIT-104).
        self.process_ready_fetching_envelopes();
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
        let tracker = self.slot_quorum_tracker.read();
        tracker.has_quorum(slot, |node_id| self.scp_driver.get_quorum_set(node_id))
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

        // Parity: stellar-core's outOfSyncRecovery iterates SCP's unbounded
        // mKnownSlots map (via processSlotsDescendingFrom) and counts slots
        // with v-blocking support. We must use scp.process_slots_descending_from
        // + scp.got_v_blocking to mirror this — NOT the capped SlotQuorumTracker,
        // which is limited to 12 slots and can never reach the 100-slot threshold.
        let mut max_slots_ahead = LEDGER_VALIDITY_BRACKET;
        let mut purge_slot = None;

        self.scp.process_slots_descending_from(u64::MAX, |seq| {
            if self.scp.got_v_blocking(seq) {
                max_slots_ahead = max_slots_ahead.saturating_sub(1);
                if max_slots_ahead == 0 {
                    purge_slot = Some(seq);
                }
            }
            max_slots_ahead != 0
        });

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
            self.scp.purge_slots(purge_slot.saturating_sub(1), None);

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

        debug!("Bootstrapping Herder at ledger {}", ledger_seq);

        // Get tracking consensus close time from LCL if available
        // (matching stellar-core setTrackingSCPState which sets close time from externalized value)
        let close_time = self
            .ledger_manager
            .read()
            .as_ref()
            .map(|lm| lm.current_header().scp_value.close_time.0)
            .unwrap_or(0);

        // Update shared tracking state (immediately visible to ScpDriver)
        {
            let mut ts = self.tracking_state.write();
            ts.is_tracking = true;
            ts.consensus_index = slot;
            ts.consensus_close_time = close_time;
        }
        *self.tracking_started_at.write() = Some(Instant::now());

        // Update pending envelopes current slot
        self.pending_envelopes.set_current_slot(slot);

        // Transition to tracking state
        *self.state.write() = HerderState::Tracking;

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

        debug!(
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
        // Honor the test-only wall-clock override on the shared ScpDriver so
        // both close-time sites (here and `ScpDriver::check_close_time`) see
        // the same "now". In production this is a single atomic load that
        // returns 0 → falls through to `SystemTime::now()`.
        let now = self.scp_driver.now_seconds();

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
    ///
    /// Synchronous wrapper around [`Herder::pre_filter_scp_envelope`],
    /// [`ScpDriver::verify_envelope`] and [`Herder::process_verified`].
    /// Retained for the synchronous catchup path and tests; production SCP
    /// flow on the event loop goes through the pipelined worker (see
    /// [`crate::scp_verify`]).
    pub fn receive_scp_envelope(&self, envelope: ScpEnvelope) -> EnvelopeState {
        use crate::scp_verify::{PipelinedIntake, PreFilter, Verdict, VerifiedEnvelope};
        let slot = envelope.statement.slot_index;
        let is_externalize = matches!(
            &envelope.statement.pledges,
            ScpStatementPledges::Externalize(_)
        );
        match self.pre_filter_scp_envelope(&envelope) {
            PreFilter::Accept(_) => {}
            PreFilter::Reject(reason) => {
                use crate::scp_verify::PreFilterRejectReason as R;
                debug!(
                    slot,
                    reason = reason.as_str(),
                    "pre-filter rejected envelope"
                );
                return match reason {
                    R::Range => EnvelopeState::TooOld,
                    R::CannotReceiveScp | R::CloseTime => EnvelopeState::Invalid,
                };
            }
        }
        if let Err(e) = self.scp_driver.verify_envelope(&envelope) {
            debug!(slot, error = %e, "Invalid SCP envelope signature");
            return EnvelopeState::InvalidSignature;
        }
        let intake = PipelinedIntake {
            envelope,
            slot,
            is_externalize,
            peer_id: None,
            enqueue_at: Instant::now(),
        };
        self.process_verified(VerifiedEnvelope {
            intake,
            verdict: Verdict::Ok,
        })
    }

    /// Test-only variant of [`Self::receive_scp_envelope`] that additionally
    /// returns a [`crate::scp_verify::PostVerifyReason`] attributing the
    /// outcome to the gate that fired. Used by the Phase B split-path
    /// equivalence test (Layer 2) so it can compare the wrapper path against
    /// the explicit `pre_filter → verify → process_verified_detailed` path at
    /// reason-level granularity — catching refactors that preserve
    /// `EnvelopeState` but diverge on WHICH gate claimed the envelope.
    ///
    /// Pre-filter rejections are mapped to the `GateDrift*` reasons so the
    /// wrapper and split paths use a single reason vocabulary. (In the split
    /// path these reasons arise when `process_verified_inner`'s rerun fires;
    /// in the wrapper path they arise on the first and only pre-filter call.)
    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    pub fn receive_scp_envelope_sync_detailed(
        &self,
        envelope: ScpEnvelope,
    ) -> (EnvelopeState, crate::scp_verify::PostVerifyReason) {
        use crate::scp_verify::{
            PipelinedIntake, PostVerifyReason, PreFilter, PreFilterRejectReason as R, Verdict,
            VerifiedEnvelope,
        };
        let slot = envelope.statement.slot_index;
        let is_externalize = matches!(
            &envelope.statement.pledges,
            ScpStatementPledges::Externalize(_)
        );
        match self.pre_filter_scp_envelope(&envelope) {
            PreFilter::Accept(_) => {}
            PreFilter::Reject(reason) => {
                debug!(
                    slot,
                    reason = reason.as_str(),
                    "pre-filter rejected envelope"
                );
                return match reason {
                    R::Range => (EnvelopeState::TooOld, PostVerifyReason::GateDriftRange),
                    R::CloseTime => (EnvelopeState::Invalid, PostVerifyReason::GateDriftCloseTime),
                    R::CannotReceiveScp => (
                        EnvelopeState::Invalid,
                        PostVerifyReason::GateDriftCannotReceive,
                    ),
                };
            }
        }
        if let Err(e) = self.scp_driver.verify_envelope(&envelope) {
            debug!(slot, error = %e, "Invalid SCP envelope signature");
            return (
                EnvelopeState::InvalidSignature,
                PostVerifyReason::InvalidSignature,
            );
        }
        let intake = PipelinedIntake {
            envelope,
            slot,
            is_externalize,
            peer_id: None,
            enqueue_at: Instant::now(),
        };
        self.process_verified_detailed(VerifiedEnvelope {
            intake,
            verdict: Verdict::Ok,
        })
    }

    /// Test-only unchecked setter for [`HerderState`] that bypasses the
    /// transition rules enforced by [`Self::set_state`] (Tracking/Syncing →
    /// Booting is normally forbidden). Used by integration tests that need to
    /// simulate state drift between pre-filter and post-verify — in
    /// particular the Layer 1 `GateDriftCannotReceive` row.
    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    pub fn force_state_for_testing(&self, state: HerderState) {
        *self.state.write() = state;
        if !state.is_tracking() {
            self.tracking_state.write().is_tracking = false;
        }
    }

    /// Pre-verify filter: runs the cheap mutable-state gates that would cause
    /// an envelope to be dropped regardless of signature validity.
    ///
    /// Order preserved from the original `receive_scp_envelope`:
    /// 1. `can_receive_scp` state check (HerderState.cpp)
    /// 2. Close-time pre-filter (tracking / non-tracking, checkpoint exception)
    /// 3. Slot-range check with `effective_min` derived from LCL
    ///
    /// Signature verification is **not** performed here. Accepted envelopes are
    /// returned as a [`PipelinedIntake`] carrying metadata needed downstream.
    pub fn pre_filter_scp_envelope(&self, envelope: &ScpEnvelope) -> crate::scp_verify::PreFilter {
        use crate::scp_verify::{PipelinedIntake, PreFilter, PreFilterRejectReason};
        let state = self.state();
        let slot = envelope.statement.slot_index;
        let current_slot = self.tracking_slot();

        if !state.can_receive_scp() {
            debug!(
                "Rejecting SCP envelope: herder in {:?} state (cannot receive)",
                state
            );
            return PreFilter::Reject(PreFilterRejectReason::CannotReceiveScp);
        }

        // Close-time pre-filter (tracking: check drift; non-tracking: check recency)
        if !self.check_envelope_close_time(envelope, false) {
            debug!(
                slot,
                "Rejecting SCP envelope: close-time pre-filter failed (check_envelope_close_time(false))"
            );
            return PreFilter::Reject(PreFilterRejectReason::CloseTime);
        }

        let checkpoint = self.get_most_recent_checkpoint_seq();
        let mut max_ledger_seq: u64 = u64::MAX;

        if state.is_tracking() {
            max_ledger_seq = self.next_consensus_ledger_index() + LEDGER_VALIDITY_BRACKET;
        } else {
            let tracking_consensus_index = current_slot.saturating_sub(1);
            let enforce_recent = tracking_consensus_index <= GENESIS_LEDGER_SEQ
                && slot != self.next_consensus_ledger_index();
            if !self.check_envelope_close_time(envelope, enforce_recent) && slot != checkpoint {
                debug!(
                    slot,
                    tracking_consensus_index,
                    enforce_recent,
                    checkpoint,
                    "Rejecting SCP envelope: close-time filter (non-tracking, enforce_recent={})",
                    enforce_recent
                );
                return PreFilter::Reject(PreFilterRejectReason::CloseTime);
            }
        }

        let min_ledger_seq = if current_slot > MAX_SLOTS_TO_REMEMBER {
            current_slot - MAX_SLOTS_TO_REMEMBER + 1
        } else {
            1
        };

        let lcl = self
            .ledger_manager
            .read()
            .as_ref()
            .map(|m| m.current_ledger_seq() as u64);

        let effective_min = lcl.map_or(min_ledger_seq, |l| min_ledger_seq.max(l + 1));

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
            return PreFilter::Reject(PreFilterRejectReason::Range);
        }

        let is_externalize = matches!(
            &envelope.statement.pledges,
            ScpStatementPledges::Externalize(_)
        );
        PreFilter::Accept(PipelinedIntake {
            envelope: envelope.clone(),
            slot,
            is_externalize,
            peer_id: None,
            enqueue_at: Instant::now(),
        })
    }

    /// Post-verification processing.
    ///
    /// Assumes signature verification already succeeded (or the verdict is
    /// surfaced as `InvalidSignature`). Re-runs the mutable pre-filter gates
    /// (state may have drifted while the worker was verifying) before
    /// executing self-message skip, non-quorum reject, slot_quorum_tracker
    /// update, EXTERNALIZE tx-set prefetch and `pending_envelopes.add` /
    /// `process_scp_envelope`.
    #[cfg(not(feature = "test-support"))]
    pub fn process_verified(&self, ve: crate::scp_verify::VerifiedEnvelope) -> EnvelopeState {
        use crate::scp_verify::{PreFilter, Verdict};
        let crate::scp_verify::VerifiedEnvelope { intake, verdict } = ve;
        let slot = intake.slot;

        match verdict {
            Verdict::Ok => {}
            Verdict::InvalidSignature => {
                debug!(slot, "SCP envelope rejected (InvalidSignature)");
                return EnvelopeState::InvalidSignature;
            }
            Verdict::Panic => {
                debug!(slot, "SCP verify worker panicked on this envelope");
                return EnvelopeState::Invalid;
            }
        }

        let envelope = intake.envelope;

        // Re-check mutable gates (state may have drifted during verify).
        match self.pre_filter_scp_envelope(&envelope) {
            PreFilter::Accept(_) => {}
            PreFilter::Reject(reason) => {
                use crate::scp_verify::PreFilterRejectReason as R;
                debug!(
                    slot,
                    reason = reason.as_str(),
                    "post-verify gate drift rejected envelope"
                );
                return match reason {
                    R::Range => EnvelopeState::TooOld,
                    R::CloseTime | R::CannotReceiveScp => EnvelopeState::Invalid,
                };
            }
        }

        let current_slot = self.tracking_slot();
        let pending_slot = self.pending_envelopes.current_slot();

        // Parity: skip self-messages (HerderImpl.cpp:885-891)
        let local_node_id = node_id_from_public_key(&self.config.node_public_key);
        if envelope.statement.node_id == local_node_id {
            trace!(slot, "Skipping self-message");
            return EnvelopeState::Invalid;
        }

        // Parity: reject envelopes from nodes not in our transitive quorum
        // (PendingEnvelopes.cpp:293-298 isNodeDefinitelyInQuorum)
        if self.config.local_quorum_set.is_some()
            && !self
                .quorum_tracker
                .read()
                .is_node_definitely_in_quorum(&envelope.statement.node_id)
        {
            debug!(
                slot,
                node = ?envelope.statement.node_id,
                "Rejecting envelope from non-quorum node"
            );
            return EnvelopeState::Invalid;
        }

        self.slot_quorum_tracker
            .write()
            .record_envelope(slot, envelope.statement.node_id.clone());

        let lcl = self
            .ledger_manager
            .read()
            .as_ref()
            .map(|m| m.current_ledger_seq() as u64);
        if let stellar_xdr::curr::ScpStatementPledges::Externalize(ext) =
            &envelope.statement.pledges
        {
            if let Ok(sv) = StellarValue::from_xdr(&ext.commit.value.0, Limits::none()) {
                let tx_set_hash = Hash256::from_bytes(sv.tx_set_hash.0);
                if lcl.map_or(true, |l| slot > l) {
                    if self.scp_driver.request_tx_set(tx_set_hash, slot) {
                        debug!(slot, hash = %tx_set_hash, "Requesting tx set from EXTERNALIZE");
                    }
                }
            }
        }

        if slot > current_slot {
            let envelope_clone = envelope.clone();
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
                    debug!(
                        slot,
                        current_slot,
                        pending_slot,
                        "Pending said TooOld but slot is within window, processing directly"
                    );
                    return self.process_scp_envelope(envelope_clone);
                }
                PendingResult::BufferFull => {
                    debug!("Pending envelope buffer full");
                    return EnvelopeState::Invalid;
                }
            }
        }

        self.process_scp_envelope(envelope)
    }

    /// Post-verification processing (test-support build).
    ///
    /// Thin wrapper around [`Self::process_verified_detailed`] that drops the
    /// [`crate::scp_verify::PostVerifyReason`] attribution. Production builds
    /// use a separate non-attributing implementation to keep `PostVerifyReason`
    /// out of the public release API entirely.
    #[cfg(feature = "test-support")]
    pub fn process_verified(&self, ve: crate::scp_verify::VerifiedEnvelope) -> EnvelopeState {
        self.process_verified_detailed(ve).0
    }

    /// Test-only variant of [`Self::process_verified`] that additionally
    /// returns a [`crate::scp_verify::PostVerifyReason`] attributing the
    /// outcome to a specific gate. See `PostVerifyReason` for the gate
    /// ordering. Used by the Phase B parity integration tests.
    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    pub fn process_verified_detailed(
        &self,
        ve: crate::scp_verify::VerifiedEnvelope,
    ) -> (EnvelopeState, crate::scp_verify::PostVerifyReason) {
        use crate::scp_verify::{PostVerifyReason, PreFilter, Verdict};
        let crate::scp_verify::VerifiedEnvelope { intake, verdict } = ve;
        let slot = intake.slot;

        match verdict {
            Verdict::Ok => {}
            Verdict::InvalidSignature => {
                debug!(slot, "SCP envelope rejected (InvalidSignature)");
                return (
                    EnvelopeState::InvalidSignature,
                    PostVerifyReason::InvalidSignature,
                );
            }
            Verdict::Panic => {
                debug!(slot, "SCP verify worker panicked on this envelope");
                return (EnvelopeState::Invalid, PostVerifyReason::PanicVerdict);
            }
        }

        let envelope = intake.envelope;

        // Re-check mutable gates (state may have drifted during verify).
        match self.pre_filter_scp_envelope(&envelope) {
            PreFilter::Accept(_) => {}
            PreFilter::Reject(reason) => {
                use crate::scp_verify::PreFilterRejectReason as R;
                debug!(
                    slot,
                    reason = reason.as_str(),
                    "post-verify gate drift rejected envelope"
                );
                let (state, post) = match reason {
                    R::Range => (EnvelopeState::TooOld, PostVerifyReason::GateDriftRange),
                    R::CloseTime => (EnvelopeState::Invalid, PostVerifyReason::GateDriftCloseTime),
                    R::CannotReceiveScp => (
                        EnvelopeState::Invalid,
                        PostVerifyReason::GateDriftCannotReceive,
                    ),
                };
                return (state, post);
            }
        }

        let current_slot = self.tracking_slot();
        let pending_slot = self.pending_envelopes.current_slot();

        // Parity: skip self-messages (HerderImpl.cpp:885-891)
        let local_node_id = node_id_from_public_key(&self.config.node_public_key);
        if envelope.statement.node_id == local_node_id {
            trace!(slot, "Skipping self-message");
            return (EnvelopeState::Invalid, PostVerifyReason::SelfMessage);
        }

        // Parity: reject envelopes from nodes not in our transitive quorum
        // (PendingEnvelopes.cpp:293-298 isNodeDefinitelyInQuorum)
        if self.config.local_quorum_set.is_some()
            && !self
                .quorum_tracker
                .read()
                .is_node_definitely_in_quorum(&envelope.statement.node_id)
        {
            debug!(
                slot,
                node = ?envelope.statement.node_id,
                "Rejecting envelope from non-quorum node"
            );
            return (EnvelopeState::Invalid, PostVerifyReason::NonQuorum);
        }

        self.slot_quorum_tracker
            .write()
            .record_envelope(slot, envelope.statement.node_id.clone());

        // Pre-fetch tx sets from EXTERNALIZE envelopes immediately so they're
        // available by the time SCP processes the envelope.
        let lcl = self
            .ledger_manager
            .read()
            .as_ref()
            .map(|m| m.current_ledger_seq() as u64);
        if let stellar_xdr::curr::ScpStatementPledges::Externalize(ext) =
            &envelope.statement.pledges
        {
            if let Ok(sv) = StellarValue::from_xdr(&ext.commit.value.0, Limits::none()) {
                let tx_set_hash = Hash256::from_bytes(sv.tx_set_hash.0);
                if lcl.map_or(true, |l| slot > l) {
                    if self.scp_driver.request_tx_set(tx_set_hash, slot) {
                        debug!(slot, hash = %tx_set_hash, "Requesting tx set from EXTERNALIZE");
                    }
                }
            }
        }

        // Future slot: buffer via PendingEnvelopes.
        if slot > current_slot {
            let envelope_clone = envelope.clone();
            match self.pending_envelopes.add(slot, envelope) {
                PendingResult::Added => {
                    debug!("Buffered envelope for future slot {}", slot);
                    return (EnvelopeState::Pending, PostVerifyReason::PendingAddBuffered);
                }
                PendingResult::Duplicate => {
                    return (
                        EnvelopeState::Duplicate,
                        PostVerifyReason::PendingAddDuplicate,
                    );
                }
                PendingResult::SlotTooFar => {
                    debug!(
                        slot,
                        current_slot, pending_slot, "Envelope rejected: slot too far ahead"
                    );
                    return (EnvelopeState::Invalid, PostVerifyReason::PendingAddTooFar);
                }
                PendingResult::SlotTooOld => {
                    debug!(
                        slot,
                        current_slot,
                        pending_slot,
                        "Pending said TooOld but slot is within window, processing directly"
                    );
                    return (
                        self.process_scp_envelope(envelope_clone),
                        PostVerifyReason::PendingAddProcessedDirectly,
                    );
                }
                PendingResult::BufferFull => {
                    debug!("Pending envelope buffer full");
                    return (
                        EnvelopeState::Invalid,
                        PostVerifyReason::PendingAddBufferFull,
                    );
                }
            }
        }

        // Current or recent slot — process directly.
        (
            self.process_scp_envelope(envelope),
            PostVerifyReason::Accepted,
        )
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
        let tx_set_hashes = crate::herder_utils::get_tx_set_hashes_from_envelope(&envelope);
        let mut missing_tx_sets = Vec::new();
        for hash in &tx_set_hashes {
            if !self.scp_driver.has_tx_set(hash) {
                missing_tx_sets.push(*hash);
            }
        }

        if !missing_tx_sets.is_empty() {
            let is_externalize = matches!(
                envelope.statement.pledges,
                stellar_xdr::curr::ScpStatementPledges::Externalize(_)
            );

            // EXTERNALIZE envelopes are processed through SCP even without
            // the tx_set. Matching stellar-core: SCP consensus (slot
            // externalization, tracking slot advance) does NOT require the
            // tx_set. The tx_set is only needed later when the application
            // layer closes the ledger.
            //
            // Without this, after catchup the node receives EXTERNALIZE
            // from peers but blocks them from SCP because the tx_set is
            // missing (expired from peers' caches). The tracking slot
            // stays frozen at LCL+1, the node can't participate in
            // real-time consensus for the current network slot, and it
            // enters a checkpoint cycling loop.
            //
            // For non-EXTERNALIZE envelopes (NOMINATE, PREPARE, CONFIRM),
            // we still require the tx_set before processing — these need
            // the tx_set for value validation during the ballot protocol.
            if is_externalize {
                // Register pending tx_set requests
                for hash in &missing_tx_sets {
                    self.scp_driver.request_tx_set(*hash, slot);
                }
                // Process through SCP to allow externalization + tracking advance
                let result = self.process_scp_envelope_with_tx_set(envelope);
                // If the slot externalized without the tx_set, the ledger
                // close path will wait for the tx_set to arrive before
                // actually closing. This is safe because check_ledger_close
                // checks tx_set availability independently.
                return result;
            }

            // Non-EXTERNALIZE: buffer until tx_set arrives
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

        // All tx sets available — check quorum-set availability too.
        // EXTERNALIZE envelopes bypass this check for the same reason they
        // bypass the tx_set check: catchup needs them to reach SCP even
        // without complete dependency resolution.
        //
        // For non-EXTERNALIZE: SCP resolves quorum sets through scp_driver;
        // if the sender's quorum set is unknown, SCP returns Invalid and the
        // app layer never requests the quorum set (it only fetches for
        // Valid/Pending). This permanently strands the envelope. Route through
        // FetchingEnvelopes so it gets buffered until the quorum set
        // arrives (AUDIT-104).
        let is_externalize = matches!(
            envelope.statement.pledges,
            stellar_xdr::curr::ScpStatementPledges::Externalize(_)
        );
        let qs_hash_raw = henyey_common::scp_quorum_set_hash(&envelope.statement);
        let qs_hash = Hash256::from_bytes(qs_hash_raw.0);
        if !is_externalize && !self.scp_driver.has_quorum_set_hash(&qs_hash) {
            debug!(
                slot,
                qs_hash = %hex::encode(qs_hash.0),
                "Envelope has cached tx_set but unknown quorum set, routing through FetchingEnvelopes"
            );

            use crate::fetching_envelopes::RecvResult;

            // Notify FetchingEnvelopes that the tx_sets are already
            // available in scp_driver — syncs the dual caches so only
            // the quorum set remains as a pending dependency.
            for hash in &tx_set_hashes {
                self.fetching_envelopes.tx_set_available(*hash, slot);
            }
            // Drain any OTHER envelopes that tx_set_available() just made
            // ready (they were waiting only on this tx_set).
            self.process_ready_fetching_envelopes();

            let envelope_clone = envelope.clone();
            let result = self.fetching_envelopes.recv_envelope(envelope);

            match result {
                RecvResult::Ready => {
                    // Quorum set arrived between the check and recv_envelope.
                    // Safe: any qset in FetchingEnvelopes' cache was stored
                    // to scp_driver first by store_quorum_set().
                    debug!(slot, "Envelope ready after qset race, processing now");
                    return self.process_scp_envelope_with_tx_set(envelope_clone);
                }
                RecvResult::Fetching => {
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

        // All tx sets and quorum set available - proceed with SCP processing
        self.process_scp_envelope_with_tx_set(envelope)
    }

    /// Process an SCP envelope after confirming tx sets are available.
    ///
    /// All nodes (validators and observers) process through SCP. Observers have
    /// `fully_validated = false` on slots, so SCP won't emit their own envelopes
    /// (matching stellar-core watcher behavior).
    fn process_scp_envelope_with_tx_set(&self, envelope: ScpEnvelope) -> EnvelopeState {
        let slot = envelope.statement.slot_index;

        let result = self.scp.receive_envelope(envelope.clone());

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
                if self.scp.is_slot_externalized(slot) {
                    if let Some(value) = self.scp.get_externalized_value(slot) {
                        debug!(slot, "Slot externalized via SCP consensus");

                        // Request the tx_set so we can close this ledger.
                        // During rapid catch-up the node may externalize
                        // via SCP without having participated in NOMINATE
                        // /PREPARE, so the tx_set might not be cached.
                        if let Ok(sv) = StellarValue::from_xdr(&value.0, Limits::none()) {
                            let tx_set_hash = sv.tx_set_hash;
                            self.scp_driver.request_tx_set(tx_set_hash.into(), slot);
                        }

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

    /// Advance tracking slot after externalization.
    fn advance_tracking_slot(&self, externalized_slot: u64) {
        // Extract close time from the externalized value for tracking
        let close_time = self
            .scp_driver
            .get_externalized_close_time(externalized_slot)
            .unwrap_or(0);

        let should_advance = {
            let mut ts = self.tracking_state.write();
            if externalized_slot >= ts.consensus_index {
                ts.is_tracking = true;
                ts.consensus_index = externalized_slot + 1;
                ts.consensus_close_time = close_time;
                true
            } else {
                false
            }
        };

        if should_advance {
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

            // Release any pending envelopes for the new slot
            let pending = self.pending_envelopes.release(externalized_slot + 1);
            for env in pending {
                let _ = self.process_scp_envelope(env);
            }
        }
    }

    /// Receive a transaction from the network.
    pub fn receive_transaction(&self, tx: TransactionEnvelope) -> TxQueueResult {
        let state = self.state();

        // If the herder hasn't reached Tracking yet (e.g., still Booting or
        // Syncing), defer submission by asking the client to retry. Returning
        // `Invalid(None)` here would map to `txINTERNAL_ERROR` in the compat
        // `/tx` handler and cause well-behaved clients (e.g., friendbot) to
        // abort instead of retrying. stellar-core does not gate reception on
        // tracking state (HerderImpl::recvTransaction always forwards to the
        // queue); `TryAgainLater` is the closest non-terminal signal we can
        // give while we're still coming up.
        if !state.can_receive_transactions() {
            debug!(
                "Deferring transaction in {:?} state (try again later)",
                state
            );
            return TxQueueResult::TryAgainLater;
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
                self.queue_full_count.fetch_add(1, Ordering::Relaxed);
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

        let slot = ledger_seq as u64;
        tracing::debug!("Triggering consensus for ledger {}", ledger_seq);

        let value = self
            .build_nomination_value()
            .ok_or_else(|| HerderError::Internal("Failed to build nomination value".into()))?;

        // Cache the nomination value for this slot so timeout retries reuse it,
        // matching stellar-core's by-value lambda capture.
        *self.cached_nomination_value.write() = Some((slot, value.clone()));

        // Get previous value for priority calculation
        let prev_value = self.prev_value.read().clone();

        // Start SCP nomination
        if self.scp.nominate(slot, value, &prev_value) {
            info!(slot, "Started SCP nomination for ledger");
        } else {
            debug!(
                slot,
                "Nomination already in progress or slot already externalized"
            );
        }

        // For solo validators (1-of-1 quorum), the nomination→ballot→externalization
        // happens synchronously within self.scp.nominate(). Check if the slot was
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

    /// Get the SCP instance.
    pub fn scp(&self) -> &SCP<HerderScpCallback> {
        &self.scp
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
        applied_txs: &[(TransactionEnvelope, i64)],
        applied_upgrades: &[UpgradeType],
        close_time: u64,
    ) {
        debug!(slot, txs = applied_txs.len(), "Ledger closed");

        // Emit aggregate queue-full count for the previous ledger interval.
        let dropped = self.queue_full_count.swap(0, Ordering::Relaxed);
        if dropped > 0 {
            warn!(
                slot,
                dropped, "Transaction queue full, dropped transactions since last ledger close"
            );
        }

        // Remove applied transactions from queue (sequence-based: removes
        // any queued tx where seq <= applied seq for the same source account,
        // matching stellar-core's removeApplied behaviour).
        self.tx_queue.remove_applied(applied_txs);

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
        self.scp.purge_slots(slot.saturating_sub(10), None);

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
        if !self.is_validator() {
            return; // Observers don't nominate
        }
        let prev_value = self.prev_value.read().clone();

        // Reuse the cached nomination value for this slot, matching stellar-core's
        // by-value lambda capture (NominationProtocol.cpp:654-659).
        let value = {
            let cached = self.cached_nomination_value.read();
            cached
                .as_ref()
                .filter(|(cached_slot, _)| *cached_slot == slot)
                .map(|(_, v)| v.clone())
        };

        // Fall back to building a fresh value if none cached (e.g., if
        // trigger_next_ledger wasn't called for this slot).
        let value = value.or_else(|| self.build_nomination_value());

        if let Some(value) = value {
            if self.scp.nominate_timeout(slot, value, &prev_value) {
                debug!(slot, "Re-nominated after timeout");
            }
        }
    }

    /// Handle ballot timeout.
    ///
    /// Called when the ballot timer expires. Bumps the ballot counter to
    /// try to make progress.
    pub fn handle_ballot_timeout(&self, slot: SlotIndex) {
        if !self.is_validator() {
            return; // Observers don't participate in balloting
        }
        if self.scp.bump_ballot(slot) {
            debug!(slot, "Bumped ballot after timeout");
        }
    }

    /// Get the current nomination timeout.
    pub fn get_nomination_timeout(&self, slot: SlotIndex) -> Option<std::time::Duration> {
        if let Some(state) = self.scp.get_slot_state(slot) {
            if state.is_nominating {
                return Some(self.scp.get_nomination_timeout(state.nomination_round));
            }
        }
        None
    }

    /// Get the current ballot timeout.
    pub fn get_ballot_timeout(&self, slot: SlotIndex) -> Option<std::time::Duration> {
        if let Some(state) = self.scp.get_slot_state(slot) {
            if let Some(round) = state.ballot_round {
                if state.heard_from_quorum
                    && !matches!(state.ballot_phase, BallotPhase::Externalize)
                {
                    return Some(self.scp.get_ballot_timeout(round));
                }
            }
        }
        None
    }

    /// Build a nomination-ready SCP `Value`: transaction set + signed StellarValue.
    ///
    /// Called by `trigger_next_ledger` to build the initial nomination value. Steps:
    ///   1. Read ledger state (previous_hash, max_txs, starting_seq)
    ///   2. Build generalized transaction set and cache it
    ///   3. Compute close_time with monotonic clamp (parity: stellar-core)
    ///   4. Merge config + runtime upgrades, filter already-applied
    ///   5. Sign via `make_stellar_value` and XDR-encode to `Value`
    fn build_nomination_value(&self) -> Option<Value> {
        // 1. Ledger state
        let (previous_hash, max_txs, starting_seq, header, lcl_close_time, max_soroban_tx_set_size) = {
            let guard = self.ledger_manager.read();
            if let Some(manager) = guard.as_ref() {
                let hdr = manager.current_header();
                let lcl_ct = hdr.scp_value.close_time.0;
                let max = hdr.max_tx_set_size as usize;
                let seq = self.build_starting_seq_map(manager);
                let soroban_max = manager
                    .soroban_network_info()
                    .map(|info| info.ledger_max_tx_count);
                (
                    manager.current_header_hash(),
                    max,
                    seq,
                    hdr,
                    lcl_ct,
                    soroban_max,
                )
            } else {
                (
                    Hash256::ZERO,
                    self.config.max_tx_set_size,
                    None,
                    LedgerHeader::default(),
                    0,
                    None,
                )
            }
        };

        // 2. Close time with monotonic clamp (parity: HerderImpl.cpp triggerNextLedger).
        // Computed BEFORE tx-set building so the offset can be used to trim
        // transactions that would expire at the proposed close time (#1192).
        let mut close_time = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock before UNIX epoch")
            .as_secs();
        if close_time <= lcl_close_time {
            close_time = lcl_close_time + 1;
        }
        let close_time_offset = close_time - lcl_close_time;

        // 3. Build & cache tx set, trimming against proposed close time.
        let (tx_set, _gen_tx_set) = self.tx_queue.build_generalized_tx_set_with_starting_seq(
            previous_hash,
            max_txs,
            starting_seq.as_ref(),
            close_time_offset,
        );
        debug!(
            hash = %tx_set.hash,
            tx_count = tx_set.len(),
            "Proposing transaction set"
        );
        // Use herder-level cache_tx_set which also notifies FetchingEnvelopes
        // and drains any envelopes that were waiting for this tx_set.
        self.cache_tx_set(tx_set.clone());

        // 4. Upgrades: config + runtime, filtered against current state.
        // Use lcl_close_time (not candidate close_time) for upgrade parameter
        // decisions to prevent one-ledger-early activation (#1166).
        let state = CurrentLedgerState {
            close_time: lcl_close_time,
            protocol_version: header.ledger_version,
            base_fee: header.base_fee,
            max_tx_set_size: header.max_tx_set_size,
            base_reserve: header.base_reserve,
            flags: match &header.ext {
                stellar_xdr::curr::LedgerHeaderExt::V0 => 0,
                stellar_xdr::curr::LedgerHeaderExt::V1(ext) => ext.flags,
            },
            max_soroban_tx_set_size: max_soroban_tx_set_size,
        };

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
                LedgerUpgrade::MaxSorobanTxSetSize(s) => {
                    state.max_soroban_tx_set_size.map_or(true, |c| *s != c)
                }
                _ => true,
            })
            .cloned()
            .collect();

        let runtime_upgrades = self.runtime_upgrades.read().create_upgrades_for(&state);
        for upgrade in runtime_upgrades {
            let dominated = upgrade_list.iter().any(|existing| {
                std::mem::discriminant(existing) == std::mem::discriminant(&upgrade)
            });
            if !dominated {
                upgrade_list.push(upgrade);
            }
        }

        // Sort upgrades by type to match stellar-core's deterministic ordering
        // (VERSION=0, BASE_FEE=1, MAX_TX_SET_SIZE=2, BASE_RESERVE=3, FLAGS=4,
        // CONFIG=5, MAX_SOROBAN_TX_SET_SIZE=6).
        upgrade_list.sort_by_key(|u| match u {
            LedgerUpgrade::Version(_) => 0u32,
            LedgerUpgrade::BaseFee(_) => 1,
            LedgerUpgrade::MaxTxSetSize(_) => 2,
            LedgerUpgrade::BaseReserve(_) => 3,
            LedgerUpgrade::Flags(_) => 4,
            LedgerUpgrade::Config(_) => 5,
            LedgerUpgrade::MaxSorobanTxSetSize(_) => 6,
        });

        let upgrades: Vec<UpgradeType> = upgrade_list
            .iter()
            .filter_map(|upgrade| upgrade.to_xdr(Limits::none()).ok())
            .filter_map(|bytes| bytes.try_into().ok().map(UpgradeType))
            .collect();

        // 5. Sign & encode
        let stellar_value = self
            .make_stellar_value(tx_set.hash, close_time, upgrades)
            .ok()?;
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
        fn encode_xdr(
            val: &impl WriteXdr,
            name: &str,
        ) -> std::result::Result<Vec<u8>, HerderError> {
            val.to_xdr(Limits::none())
                .map_err(|e| HerderError::Internal(format!("Failed to encode {}: {}", name, e)))
        }
        sign_data.extend_from_slice(&encode_xdr(&EnvelopeType::Scpvalue, "envelope type")?);
        sign_data.extend_from_slice(&encode_xdr(&xdr_tx_set_hash, "tx set hash")?);
        sign_data.extend_from_slice(&encode_xdr(&xdr_close_time, "close time")?);
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
                Err(e) => {
                    tracing::error!(
                        account = ?account,
                        error = %e,
                        "Failed to load account sequence for tx queue"
                    );
                }
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

    /// Check if a slot has EXTERNALIZE envelopes in-flight (waiting for tx_set).
    ///
    /// Returns true if the envelope was received but is stuck in fetching
    /// state because the tx_set hasn't arrived yet. This is distinct from
    /// "missing" — the envelope exists, just can't be processed yet.
    pub fn has_fetching_envelopes_for_slot(&self, slot: u64) -> bool {
        self.fetching_envelopes.has_fetching_for_slot(slot)
    }

    /// Get SCP state envelopes for responding to peers.
    ///
    /// Returns SCP envelopes for slots starting from `from_slot`, along with
    /// our local quorum set if configured.
    pub fn get_scp_state(&self, from_slot: u64) -> (Vec<ScpEnvelope>, Option<ScpQuorumSet>) {
        let envelopes = self.scp.get_scp_state(from_slot);

        let quorum_set = self.scp_driver.get_local_quorum_set();

        (envelopes, quorum_set)
    }

    /// Get all SCP envelopes recorded for a slot.
    pub fn get_scp_envelopes(&self, slot: u64) -> Vec<ScpEnvelope> {
        self.scp.get_slot_envelopes(slot)
    }

    pub fn get_slot_state(&self, slot: u64) -> Option<henyey_scp::SlotState> {
        self.scp.get_slot_state(slot)
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
        self.scp_driver
            .cleanup_pending_tx_sets(PENDING_TX_SET_MAX_AGE_SECS);
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
        let notify_slot = slot.unwrap_or_else(|| self.tracking_slot());
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
        let slot = self.tracking_slot();
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

    /// Regression test: EXTERNALIZE from non-quorum node is rejected at the
    /// herder level by the quorum membership check (PendingEnvelopes.cpp:293-298).
    /// Before #1098 fix, non-quorum envelopes reached SCP and could trigger
    /// fast-track catchup.
    #[test]
    fn test_externalize_rejected_when_node_not_in_quorum() {
        let local_secret = SecretKey::from_seed(&[7u8; 32]);
        let local_public = local_secret.public_key();
        let local_node_id = node_id_from_public_key(&local_public);

        // Unknown sender (not in quorum)
        let unknown_secret = SecretKey::from_seed(&[99u8; 32]);

        // Quorum set only includes local node
        let quorum_set = ScpQuorumSet {
            threshold: 1,
            validators: vec![local_node_id.clone()].try_into().unwrap(),
            inner_sets: vec![].try_into().unwrap(),
        };

        let config = HerderConfig {
            is_validator: true,
            node_public_key: local_public,
            local_quorum_set: Some(quorum_set.clone()),
            ..HerderConfig::default()
        };

        let herder = Herder::with_secret_key(config, local_secret.clone());
        herder.start_syncing();
        herder.bootstrap(100);

        herder
            .quorum_tracker
            .write()
            .expand(&local_node_id, quorum_set)
            .unwrap();

        let tracking = herder.tracking_slot(); // 101

        // Create a signed EXTERNALIZE from the unknown node
        let envelope = make_signed_externalize_from(tracking, &herder, &unknown_secret);
        let result = herder.receive_scp_envelope(envelope);

        // With the quorum membership guard, the envelope should be rejected
        // before reaching SCP (parity with stellar-core PendingEnvelopes.cpp:293)
        assert_eq!(
            result,
            EnvelopeState::Invalid,
            "Non-quorum node envelope should be rejected by quorum membership check"
        );
        assert_eq!(
            herder.tracking_slot(),
            tracking,
            "tracking slot should NOT advance — unknown node not in quorum"
        );
    }

    /// Regression test: self-message filtering (HerderImpl.cpp:885-891).
    /// Envelopes from the local node should be skipped.
    #[test]
    fn test_self_message_rejected() {
        let (herder, secret) = make_validator_herder();
        herder.start_syncing();
        herder.bootstrap(100);

        let tracking = herder.tracking_slot(); // 101

        // Create a signed envelope FROM the local node
        let value = make_valid_value_with_cached_tx_set(&herder, &secret);
        let node_id = node_id_from_public_key(&herder.config.node_public_key);
        let statement = ScpStatement {
            node_id: node_id.clone(),
            slot_index: tracking,
            pledges: ScpStatementPledges::Prepare(ScpStatementPrepare {
                quorum_set_hash: hash_quorum_set(herder.config.local_quorum_set.as_ref().unwrap())
                    .into(),
                ballot: ScpBallot {
                    counter: 1,
                    value: value.clone(),
                },
                prepared: None,
                prepared_prime: None,
                n_c: 0,
                n_h: 0,
            }),
        };
        let envelope = sign_statement(&statement, &herder, &secret);

        let result = herder.receive_scp_envelope(envelope);
        assert_eq!(
            result,
            EnvelopeState::Invalid,
            "Self-message should be rejected (HerderImpl.cpp:885-891)"
        );
    }

    #[test]
    fn test_externalize_accepted_for_far_future_slot() {
        // Future EXTERNALIZE envelopes are now buffered in PendingEnvelopes
        // (matching stellar-core behavior where all envelope types go through
        // the same pending→SCP pipeline). This test verifies that a future-slot
        // EXTERNALIZE is buffered, not immediately processed.
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

        let herder = Herder::with_secret_key(config, local_secret);
        herder.start_syncing();
        herder.bootstrap(100);

        herder
            .quorum_tracker
            .write()
            .expand(&other_node_id, quorum_set)
            .unwrap();

        let tracking = herder.tracking_slot(); // 101

        // An EXTERNALIZE for a future slot within the pending buffer distance
        let future_slot = tracking + 5;
        let envelope = make_signed_externalize_from(future_slot, &herder, &other_secret);
        let result = herder.receive_scp_envelope(envelope);

        // Should be buffered in PendingEnvelopes (not immediately processed)
        assert_eq!(result, EnvelopeState::Pending);
        // Tracking slot should NOT have advanced (envelope is buffered)
        assert_eq!(herder.tracking_slot(), tracking);
    }

    #[test]
    fn test_externalize_accepted_when_within_distance_and_in_quorum() {
        // When an EXTERNALIZE arrives for the current tracking slot from
        // a node in quorum with a valid signed value, SCP should accept it,
        // externalize the slot, and advance tracking.
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

        let herder = Herder::with_secret_key(config, local_secret);
        herder.start_syncing();
        herder.bootstrap(100);

        herder
            .quorum_tracker
            .write()
            .expand(&other_node_id, quorum_set)
            .unwrap();

        let tracking = herder.tracking_slot(); // 101

        // Create a properly signed EXTERNALIZE for the current tracking slot
        let envelope = make_signed_externalize_from(tracking, &herder, &other_secret);
        let result = herder.receive_scp_envelope(envelope);

        // Should be accepted and cause externalization through SCP
        assert_eq!(result, EnvelopeState::Valid);
        // Tracking slot should have advanced
        assert_eq!(herder.tracking_slot(), tracking + 1);
    }

    #[test]
    fn test_externalize_transitions_syncing_to_tracking() {
        // Matches stellar-core behavior: externalization always transitions
        // to HERDER_TRACKING_NETWORK_STATE, even from SYNCING state.
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

        let herder = Herder::with_secret_key(config, local_secret);
        herder.start_syncing();
        herder.bootstrap(100);

        herder
            .quorum_tracker
            .write()
            .expand(&other_node_id, quorum_set)
            .unwrap();

        // Simulate losing sync: force herder back to Syncing
        herder.set_state(HerderState::Syncing);
        assert_eq!(herder.state(), HerderState::Syncing);

        let tracking = herder.tracking_slot(); // 101

        // Process an EXTERNALIZE — should transition back to Tracking
        let envelope = make_signed_externalize_from(tracking, &herder, &other_secret);
        let result = herder.receive_scp_envelope(envelope);

        assert_eq!(result, EnvelopeState::Valid);
        assert_eq!(
            herder.state(),
            HerderState::Tracking,
            "Externalization should transition herder from Syncing to Tracking"
        );
    }

    fn make_minimal_tx_envelope() -> TransactionEnvelope {
        use stellar_xdr::curr::{Memo, TransactionV0, TransactionV0Envelope, TransactionV0Ext};
        TransactionEnvelope::TxV0(TransactionV0Envelope {
            tx: TransactionV0 {
                source_account_ed25519: stellar_xdr::curr::Uint256([0u8; 32]),
                fee: 100,
                seq_num: stellar_xdr::curr::SequenceNumber(1),
                time_bounds: None,
                memo: Memo::None,
                operations: vec![].try_into().unwrap(),
                ext: TransactionV0Ext::V0,
            },
            signatures: vec![].try_into().unwrap(),
        })
    }

    /// Regression: when the herder has not yet reached Tracking (e.g., fresh
    /// boot or manual-close mode before the first externalization), submitting
    /// a transaction must return `TryAgainLater` rather than `Invalid(None)`.
    /// The latter maps to `txINTERNAL_ERROR` in the compat `/tx` handler and
    /// causes well-behaved clients (e.g., friendbot) to abort with a fatal
    /// error instead of retrying.
    #[test]
    fn test_receive_transaction_before_tracking_returns_try_again_later() {
        let herder = make_test_herder();
        assert!(!herder.state().can_receive_transactions());

        let result = herder.receive_transaction(make_minimal_tx_envelope());
        assert!(
            matches!(result, TxQueueResult::TryAgainLater),
            "expected TryAgainLater when not tracking, got {:?}",
            result
        );
    }

    #[test]
    fn test_nomination_timeout_requires_started() {
        let (herder, secret) = make_validator_herder();
        let slot = 1u64;

        assert!(herder.get_nomination_timeout(slot).is_none());

        let value = make_valid_value_with_cached_tx_set(&herder, &secret);
        let prev_value = value.clone();
        assert!(herder.scp().nominate(slot, value, &prev_value));

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
        let slot = 1u64;

        assert!(herder.get_ballot_timeout(slot).is_none());

        // Create a valid value and start ballot protocol via bump_state
        let value = make_valid_value_with_cached_tx_set(&herder, &secret);
        assert!(herder.scp().bump_state(slot, value.clone(), 1));

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
        herder.scp().receive_envelope(env);

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
        let slot = 1u64;

        let value = make_valid_value_with_cached_tx_set(&herder, &secret);
        herder.scp().force_externalize(slot, value);

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
        herder.tracking_state.write().consensus_close_time = now;

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

    /// Regression test for AUDIT-028: upgrades must be ordered by type
    /// (VERSION < BASE_FEE < MAX_TX_SET_SIZE < BASE_RESERVE < FLAGS < CONFIG
    /// < MAX_SOROBAN_TX_SET_SIZE) to match stellar-core's deterministic ordering.
    #[tokio::test]
    async fn test_audit_028_nomination_upgrades_ordered_by_type() {
        let seed = [7u8; 32];
        let secret = SecretKey::from_seed(&seed);
        let public = secret.public_key();
        let config = HerderConfig {
            is_validator: true,
            node_public_key: public,
            local_quorum_set: Some(stellar_xdr::curr::ScpQuorumSet {
                threshold: 1,
                validators: vec![stellar_xdr::curr::NodeId(
                    stellar_xdr::curr::PublicKey::PublicKeyTypeEd25519(stellar_xdr::curr::Uint256(
                        *public.as_bytes(),
                    )),
                )]
                .try_into()
                .unwrap(),
                inner_sets: vec![].try_into().unwrap(),
            }),
            // Set upgrades in REVERSE of canonical order
            proposed_upgrades: vec![
                LedgerUpgrade::Flags(1),
                LedgerUpgrade::BaseFee(200),
                LedgerUpgrade::Version(25),
            ],
            ..Default::default()
        };
        // Also set runtime upgrades that would append BaseReserve after Flags
        let herder = Herder::with_secret_key(config, secret);
        let upgrade_params = crate::upgrades::UpgradeParameters {
            upgrade_time: 0,
            base_reserve: Some(5_000_000),
            ..Default::default()
        };
        herder
            .set_upgrade_parameters(upgrade_params)
            .expect("set_upgrade_parameters should succeed");

        herder.bootstrap(1);

        let result = herder.trigger_next_ledger(2).await;
        assert!(
            result.is_ok(),
            "trigger_next_ledger failed: {:?}",
            result.err()
        );

        let externalized = herder.scp_driver.get_externalized(2);
        assert!(externalized.is_some(), "slot 2 should be externalized");

        let ext = externalized.unwrap();
        let sv = StellarValue::from_xdr(&ext.value, Limits::none()).expect("parse StellarValue");

        let upgrades: Vec<LedgerUpgrade> = sv
            .upgrades
            .iter()
            .filter_map(|b| LedgerUpgrade::from_xdr(&b.0, Limits::none()).ok())
            .collect();

        assert!(
            upgrades.len() >= 3,
            "expected at least 3 upgrades, got {}",
            upgrades.len()
        );

        // Verify ordering: each upgrade type must have a lower index than the next
        let order = |u: &LedgerUpgrade| -> u32 {
            match u {
                LedgerUpgrade::Version(_) => 0,
                LedgerUpgrade::BaseFee(_) => 1,
                LedgerUpgrade::MaxTxSetSize(_) => 2,
                LedgerUpgrade::BaseReserve(_) => 3,
                LedgerUpgrade::Flags(_) => 4,
                LedgerUpgrade::Config(_) => 5,
                LedgerUpgrade::MaxSorobanTxSetSize(_) => 6,
            }
        };
        for w in upgrades.windows(2) {
            assert!(
                order(&w[0]) < order(&w[1]),
                "upgrades out of order: {:?} (order {}) should precede {:?} (order {})",
                w[0],
                order(&w[0]),
                w[1],
                order(&w[1]),
            );
        }
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
            .expand(&other_node_id, quorum_set)
            .unwrap();

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
        assert!(
            herder.scp().is_slot_externalized(tracking),
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

    /// Regression test for AUDIT-004: `store_quorum_set` must mirror the
    /// quorum set into `FetchingEnvelopes` so that blocked envelopes waiting
    /// for that quorum set get unblocked.
    #[test]
    fn test_audit_004_store_quorum_set_unblocks_fetching_envelopes() {
        use stellar_xdr::curr::Hash as XdrHash;

        let herder = make_test_herder();

        // Build a sane quorum set with one validator.
        let node_id = XdrNodeId(stellar_xdr::curr::PublicKey::PublicKeyTypeEd25519(
            stellar_xdr::curr::Uint256([1u8; 32]),
        ));
        let quorum_set = ScpQuorumSet {
            threshold: 1,
            validators: vec![node_id.clone()].try_into().unwrap(),
            inner_sets: vec![].try_into().unwrap(),
        };
        let qs_hash_bytes = hash_quorum_set(&quorum_set);

        // Submit a nomination envelope whose quorum_set_hash references the
        // quorum set we haven't stored yet. This should put the envelope
        // into Fetching state.
        let envelope = ScpEnvelope {
            statement: ScpStatement {
                node_id: node_id.clone(),
                slot_index: 100,
                pledges: ScpStatementPledges::Nominate(ScpNomination {
                    quorum_set_hash: XdrHash(qs_hash_bytes.0),
                    votes: vec![].try_into().unwrap(),
                    accepted: vec![].try_into().unwrap(),
                }),
            },
            signature: XdrSignature(vec![0u8; 64].try_into().unwrap()),
        };

        use crate::fetching_envelopes::RecvResult;
        let result = herder.fetching_envelopes.recv_envelope(envelope);
        assert_eq!(
            result,
            RecvResult::Fetching,
            "Envelope should be fetching (quorum set not yet known)"
        );

        // Before the fix, store_quorum_set only updated ScpDriver and the
        // quorum tracker, leaving FetchingEnvelopes unaware. The envelope
        // would remain stuck in Fetching forever.
        herder.store_quorum_set(&node_id, quorum_set.clone());

        // After the fix, store_quorum_set mirrors into FetchingEnvelopes,
        // which should now have the quorum set cached and the envelope
        // moved to the ready queue AND drained via process_ready_fetching_envelopes.
        assert!(
            herder.fetching_envelopes.has_quorum_set(&qs_hash_bytes),
            "FetchingEnvelopes must learn about the quorum set via store_quorum_set"
        );

        // The ready queue should be empty because process_ready_fetching_envelopes()
        // drains it as part of store_quorum_set (AUDIT-104 Bug B fix).
        let popped = herder.fetching_envelopes.pop(100);
        assert!(
            popped.is_none(),
            "Ready queue must be drained by store_quorum_set (envelope already processed through SCP)"
        );
    }

    /// Regression test for AUDIT-104 Bug A: when tx_sets are cached but the
    /// sender's quorum set is unknown, process_scp_envelope must route through
    /// FetchingEnvelopes instead of sending to SCP (which would return Invalid
    /// and permanently strand the envelope).
    #[test]
    fn test_audit_104_qset_missing_routes_through_fetching() {
        use stellar_xdr::curr::Hash as XdrHash;

        let local_secret = SecretKey::from_seed(&[7u8; 32]);
        let local_public = local_secret.public_key();
        let local_node_id = node_id_from_public_key(&local_public);

        let other_secret = SecretKey::from_seed(&[1u8; 32]);
        let other_public = other_secret.public_key();
        let other_node_id = node_id_from_public_key(&other_public);

        // Use an unknown quorum set (different from the local one)
        let unknown_qs = ScpQuorumSet {
            threshold: 1,
            validators: vec![other_node_id.clone()].try_into().unwrap(),
            inner_sets: vec![].try_into().unwrap(),
        };
        let unknown_qs_hash = hash_quorum_set(&unknown_qs);

        // Local quorum includes both nodes so the envelope passes membership check
        let local_qs = ScpQuorumSet {
            threshold: 1,
            validators: vec![local_node_id.clone(), other_node_id.clone()]
                .try_into()
                .unwrap(),
            inner_sets: vec![].try_into().unwrap(),
        };

        let config = HerderConfig {
            is_validator: true,
            node_public_key: local_public,
            local_quorum_set: Some(local_qs.clone()),
            ..HerderConfig::default()
        };

        let herder = Herder::with_secret_key(config, local_secret);
        herder.start_syncing();
        herder.bootstrap(100);

        herder
            .quorum_tracker
            .write()
            .expand(&other_node_id, local_qs)
            .unwrap();

        let tracking = herder.tracking_slot(); // 101

        // Cache a tx_set in scp_driver so the fast path triggers
        let value = make_valid_value_with_cached_tx_set(&herder, &other_secret);

        // Create a PREPARE envelope from other_node referencing:
        //   - The cached tx_set (so missing_tx_sets is empty)
        //   - An unknown quorum set hash (not stored in scp_driver)
        let statement = ScpStatement {
            node_id: other_node_id.clone(),
            slot_index: tracking,
            pledges: ScpStatementPledges::Prepare(ScpStatementPrepare {
                quorum_set_hash: XdrHash(unknown_qs_hash.0),
                ballot: ScpBallot { counter: 1, value },
                prepared: None,
                prepared_prime: None,
                n_c: 0,
                n_h: 0,
            }),
        };
        let envelope = sign_statement(&statement, &herder, &other_secret);

        // Before fix: this would go straight to SCP, get Invalid (unknown qset),
        // and the qset would never be fetched.
        // After fix: should return Fetching because the qset is unknown.
        let result = herder.receive_scp_envelope(envelope);
        assert_eq!(
            result,
            EnvelopeState::Fetching,
            "AUDIT-104: envelope with cached tx_set but unknown qset must return Fetching, not Invalid"
        );

        // The envelope should be buffered in FetchingEnvelopes (not in ready queue)
        assert!(
            herder.fetching_envelopes.stats().envelopes_fetching > 0,
            "FetchingEnvelopes should be tracking the envelope while waiting for quorum set"
        );
    }

    /// Regression test for AUDIT-104 Bug B: store_quorum_set must drain the
    /// ready queue after notifying FetchingEnvelopes, so envelopes unblocked
    /// by quorum-set arrival are fed to SCP immediately (not left in the
    /// ready queue until a tx_set arrival happens to drain it).
    #[test]
    fn test_audit_104_store_quorum_set_drains_ready() {
        use stellar_xdr::curr::Hash as XdrHash;

        let herder = make_test_herder();

        // Build a sane quorum set
        let node_id = XdrNodeId(stellar_xdr::curr::PublicKey::PublicKeyTypeEd25519(
            stellar_xdr::curr::Uint256([1u8; 32]),
        ));
        let quorum_set = ScpQuorumSet {
            threshold: 1,
            validators: vec![node_id.clone()].try_into().unwrap(),
            inner_sets: vec![].try_into().unwrap(),
        };
        let qs_hash = hash_quorum_set(&quorum_set);

        // Submit a nomination envelope waiting for this quorum set
        let envelope = ScpEnvelope {
            statement: ScpStatement {
                node_id: node_id.clone(),
                slot_index: 100,
                pledges: ScpStatementPledges::Nominate(ScpNomination {
                    quorum_set_hash: XdrHash(qs_hash.0),
                    votes: vec![].try_into().unwrap(),
                    accepted: vec![].try_into().unwrap(),
                }),
            },
            signature: XdrSignature(vec![0u8; 64].try_into().unwrap()),
        };

        use crate::fetching_envelopes::RecvResult;
        let result = herder.fetching_envelopes.recv_envelope(envelope);
        assert_eq!(result, RecvResult::Fetching);

        // store_quorum_set should: mirror to FetchingEnvelopes AND drain ready queue
        herder.store_quorum_set(&node_id, quorum_set);

        // After the fix, the ready queue should be empty because
        // process_ready_fetching_envelopes() was called and drained it.
        // (The envelope gets sent to SCP, which may return Invalid for a
        // non-tracking herder — that's fine, the point is it was drained.)
        let popped = herder.fetching_envelopes.pop(100);
        assert!(
            popped.is_none(),
            "AUDIT-104: ready queue must be drained by store_quorum_set (envelope already processed)"
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

    /// Regression test: transitioning from Tracking to Syncing must clear
    /// SharedTrackingState::is_tracking so that ScpDriver's
    /// validate_past_or_future_value returns MaybeValid (not Invalid)
    /// for far-future EXTERNALIZE messages. Without this, a watcher that
    /// loses sync rejects all far-future EXTERNALIZE messages and can
    /// never re-externalize, causing an infinite recovery loop.
    #[test]
    fn test_set_state_tracking_to_syncing_clears_shared_is_tracking() {
        let herder = make_test_herder();

        // Bootstrap sets is_tracking=true via bootstrap()
        herder.start_syncing();
        herder.bootstrap(100);
        assert_eq!(herder.state(), HerderState::Tracking);
        assert!(
            herder.tracking_state.read().is_tracking,
            "bootstrap should set is_tracking=true"
        );

        // Transition to Syncing (simulating lost sync)
        herder.set_state(HerderState::Syncing);
        assert_eq!(herder.state(), HerderState::Syncing);
        assert!(
            !herder.tracking_state.read().is_tracking,
            "Tracking→Syncing must clear SharedTrackingState::is_tracking"
        );

        // Re-entering Tracking (via bootstrap after catchup) should restore it
        herder.bootstrap(200);
        assert_eq!(herder.state(), HerderState::Tracking);
        assert!(
            herder.tracking_state.read().is_tracking,
            "bootstrap after catchup should set is_tracking=true again"
        );
    }

    /// Regression test for AUDIT-142 (#1501): cached_nomination_value is
    /// populated by trigger_next_ledger and reused by handle_nomination_timeout.
    /// Verifies the field exists and behaves as a slot-keyed cache.
    #[test]
    fn test_nomination_value_cache_slot_keyed() {
        let herder = make_test_herder();

        // Initially empty
        assert!(herder.cached_nomination_value.read().is_none());

        // Simulate caching a value for slot 10
        let test_value = Value(vec![1, 2, 3].try_into().unwrap());
        *herder.cached_nomination_value.write() = Some((10, test_value.clone()));

        // Reading for slot 10 should return the cached value
        let cached = herder.cached_nomination_value.read();
        let (slot, val) = cached.as_ref().unwrap();
        assert_eq!(*slot, 10);
        assert_eq!(*val, test_value);

        // Overwriting for slot 11 replaces slot 10's value
        drop(cached);
        let test_value_2 = Value(vec![4, 5, 6].try_into().unwrap());
        *herder.cached_nomination_value.write() = Some((11, test_value_2.clone()));

        let cached = herder.cached_nomination_value.read();
        let (slot, val) = cached.as_ref().unwrap();
        assert_eq!(*slot, 11);
        assert_eq!(*val, test_value_2);
    }
}

// =============================================================================
// Phase B pipeline tests (issue #1734): pre_filter_scp_envelope and
// process_verified behavior under the split-pipeline architecture.
// =============================================================================

#[cfg(test)]
mod scp_pipeline_tests {
    use super::*;
    use crate::scp_verify::{
        spawn_scp_verifier, PipelinedIntake, PreFilter, PreFilterRejectReason, Verdict,
        VerifiedEnvelope, VerifierState,
    };
    use henyey_crypto::SecretKey;
    use stellar_xdr::curr::{
        NodeId as XdrNodeId, ScpNomination, ScpStatement, ScpStatementPledges,
        Signature as XdrSignature,
    };
    fn make_test_herder() -> Herder {
        Herder::new(HerderConfig::default())
    }

    fn make_unsigned_envelope(slot: u64, node_seed: u8) -> ScpEnvelope {
        let node_id = XdrNodeId(stellar_xdr::curr::PublicKey::PublicKeyTypeEd25519(
            stellar_xdr::curr::Uint256([node_seed; 32]),
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
    fn test_pre_filter_rejects_in_booting_state() {
        let herder = make_test_herder();
        assert_eq!(herder.state(), HerderState::Booting);

        let env = make_unsigned_envelope(100, 1);
        match herder.pre_filter_scp_envelope(&env) {
            PreFilter::Reject(PreFilterRejectReason::CannotReceiveScp) => {}
            other => panic!(
                "expected CannotReceiveScp reject in Booting state, got {:?}",
                other
            ),
        }
    }

    #[test]
    fn test_pre_filter_accepts_valid_envelope_in_syncing() {
        let herder = make_test_herder();
        herder.start_syncing();
        herder.pending_envelopes.set_current_slot(95);

        // Construct a PREPARE envelope with a current close time so
        // check_envelope_close_time passes.
        let secret = SecretKey::from_seed(&[1u8; 32]);
        let env = make_signed_test_envelope_outer(100, &herder, &secret);
        match herder.pre_filter_scp_envelope(&env) {
            PreFilter::Accept(intake) => {
                assert_eq!(intake.slot, 100);
                assert!(!intake.is_externalize);
            }
            PreFilter::Reject(r) => panic!("expected Accept, got Reject({:?})", r),
        }
    }

    fn make_signed_test_envelope_outer(
        slot: u64,
        herder: &Herder,
        secret: &SecretKey,
    ) -> ScpEnvelope {
        use stellar_xdr::curr::{
            LedgerCloseValueSignature, Limits, NodeId as XdrNodeId, ScpNomination, ScpStatement,
            ScpStatementPledges, Signature as XdrSignature, StellarValue, StellarValueExt,
            TimePoint, Value, WriteXdr,
        };
        let public = secret.public_key();
        let node_id = XdrNodeId(stellar_xdr::curr::PublicKey::PublicKeyTypeEd25519(
            stellar_xdr::curr::Uint256(*public.as_bytes()),
        ));
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();

        // Build a signed StellarValue so check_envelope_close_time accepts it.
        let tx_set_hash = stellar_xdr::curr::Hash([0u8; 32]);
        let close_time = TimePoint(now);
        let mut sign_data = herder.scp_driver.network_id().0.to_vec();
        sign_data.extend_from_slice(
            &stellar_xdr::curr::EnvelopeType::Scpvalue
                .to_xdr(Limits::none())
                .unwrap(),
        );
        sign_data.extend_from_slice(&tx_set_hash.to_xdr(Limits::none()).unwrap());
        sign_data.extend_from_slice(&close_time.to_xdr(Limits::none()).unwrap());
        let value_sig = secret.sign(&sign_data);
        let sv = StellarValue {
            tx_set_hash,
            close_time,
            upgrades: vec![].try_into().unwrap(),
            ext: StellarValueExt::Signed(LedgerCloseValueSignature {
                node_id: node_id.clone(),
                signature: stellar_xdr::curr::Signature(
                    value_sig.0.to_vec().try_into().unwrap_or_default(),
                ),
            }),
        };
        let vote = Value(sv.to_xdr(Limits::none()).unwrap().try_into().unwrap());

        let statement = ScpStatement {
            node_id,
            slot_index: slot,
            pledges: ScpStatementPledges::Nominate(ScpNomination {
                quorum_set_hash: stellar_xdr::curr::Hash([0u8; 32]),
                votes: vec![vote].try_into().unwrap(),
                accepted: vec![].try_into().unwrap(),
            }),
        };
        let mut data = herder.scp_driver.network_id().0.to_vec();
        data.extend_from_slice(&1i32.to_be_bytes()); // ENVELOPE_TYPE_SCP
        data.extend_from_slice(&statement.to_xdr(Limits::none()).unwrap());
        let sig = secret.sign(&data);
        ScpEnvelope {
            statement,
            signature: XdrSignature(sig.0.to_vec().try_into().unwrap()),
        }
    }

    #[test]
    fn test_process_verified_invalid_signature_short_circuits() {
        let herder = make_test_herder();
        herder.start_syncing();
        herder.pending_envelopes.set_current_slot(95);

        let env = make_unsigned_envelope(100, 1);
        let intake = PipelinedIntake {
            envelope: env,
            slot: 100,
            is_externalize: false,
            peer_id: None,
            enqueue_at: std::time::Instant::now(),
        };
        let result = herder.process_verified(VerifiedEnvelope {
            intake,
            verdict: Verdict::InvalidSignature,
        });
        assert_eq!(result, EnvelopeState::InvalidSignature);
    }

    #[test]
    fn test_process_verified_panic_surfaces_invalid() {
        let herder = make_test_herder();
        herder.start_syncing();
        let env = make_unsigned_envelope(100, 1);
        let intake = PipelinedIntake {
            envelope: env,
            slot: 100,
            is_externalize: false,
            peer_id: None,
            enqueue_at: std::time::Instant::now(),
        };
        let result = herder.process_verified(VerifiedEnvelope {
            intake,
            verdict: Verdict::Panic,
        });
        assert_eq!(result, EnvelopeState::Invalid);
    }

    #[test]
    fn test_process_verified_self_message_skipped() {
        // Build a validator herder whose local node_id matches the envelope.
        let seed = [5u8; 32];
        let secret = SecretKey::from_seed(&seed);
        let public = secret.public_key();
        let node_id = node_id_from_public_key(&public);
        let quorum_set = ScpQuorumSet {
            threshold: 1,
            validators: vec![node_id.clone()].try_into().unwrap(),
            inner_sets: vec![].try_into().unwrap(),
        };
        let config = HerderConfig {
            is_validator: true,
            node_public_key: public,
            local_quorum_set: Some(quorum_set),
            ..HerderConfig::default()
        };
        let herder = Herder::with_secret_key(config, secret.clone());
        herder.start_syncing();
        herder.pending_envelopes.set_current_slot(95);

        // Build an envelope whose statement.node_id == the local node_id.
        let env = make_signed_test_envelope_outer(100, &herder, &secret);
        let intake = PipelinedIntake {
            envelope: env,
            slot: 100,
            is_externalize: false,
            peer_id: None,
            enqueue_at: std::time::Instant::now(),
        };
        let result = herder.process_verified(VerifiedEnvelope {
            intake,
            verdict: Verdict::Ok,
        });
        assert_eq!(
            result,
            EnvelopeState::Invalid,
            "self-message must be skipped as Invalid"
        );
    }

    #[test]
    fn test_process_verified_post_verify_gate_drift_too_old() {
        // Simulate gate drift: build an envelope whose signed close_time
        // is current (so pre_filter's close-time gate passes), but by the
        // time process_verified runs, the herder has advanced its current
        // slot so far that the envelope's slot falls below `effective_min`
        // — the Range gate must now reject it as `EnvelopeState::TooOld`.
        let herder = make_test_herder();
        herder.start_syncing();
        // Set tracking_slot high so pre_filter's Range gate (keyed on
        // tracking_slot — not pending_envelopes.current_slot) rejects slot=100.
        herder.tracking_state.write().consensus_index = 10_000;
        herder.pending_envelopes.set_current_slot(10_000);

        let secret = SecretKey::from_seed(&[7u8; 32]);
        // Build a Nominate envelope with a fresh, signed StellarValue so
        // close-time passes; slot=100 is far below effective_min.
        let env = make_signed_test_envelope_outer(100, &herder, &secret);
        let intake = PipelinedIntake {
            envelope: env,
            slot: 100,
            is_externalize: false,
            peer_id: None,
            enqueue_at: std::time::Instant::now(),
        };
        let result = herder.process_verified(VerifiedEnvelope {
            intake,
            verdict: Verdict::Ok,
        });
        assert_eq!(
            result,
            EnvelopeState::TooOld,
            "post-verify Range gate should reject drifted slot as TooOld"
        );
    }

    // -------- scp_verify::worker tests --------

    #[test]
    fn test_worker_dead_on_channel_close() {
        let spawned = spawn_scp_verifier(Hash256::from_bytes([4u8; 32]), 8).expect("spawn");
        let state = spawned.handle.state.clone();
        let tx = spawned.handle.tx.clone();
        let verified_rx = spawned.verified_rx;
        assert_eq!(
            VerifierState::from_u8(state.load(Ordering::Relaxed)),
            VerifierState::Running
        );

        // Close all senders so blocking_recv returns None and the worker
        // transitions to Dead. The output receiver closure is not required
        // but we drop it for symmetry.
        drop(spawned.handle); // original handle owns one tx
        drop(tx); // our clone
        drop(verified_rx);

        // Spin for up to 2s.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while std::time::Instant::now() < deadline
            && VerifierState::from_u8(state.load(Ordering::Relaxed)) != VerifierState::Dead
        {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(
            VerifierState::from_u8(state.load(Ordering::Relaxed)),
            VerifierState::Dead,
            "worker should transition to Dead after input channel closes"
        );
    }

    #[test]
    fn test_worker_panics_marks_dead() {
        // The worker has a cfg(test) sentinel: slot == u64::MAX - 1 panics
        // inside catch_unwind. The worker must emit a Panic verdict AND
        // transition state to Dead, even if the input channel stays open.
        let spawned = spawn_scp_verifier(Hash256::from_bytes([11u8; 32]), 8).expect("spawn");
        let h = spawned.handle.clone();
        let mut verified_rx = spawned.verified_rx;

        let intake = PipelinedIntake {
            envelope: make_unsigned_envelope(1, 1),
            slot: u64::MAX - 1,
            is_externalize: false,
            peer_id: None,
            enqueue_at: std::time::Instant::now(),
        };
        h.tx.blocking_send(intake).expect("send");

        let ve = verified_rx
            .blocking_recv()
            .expect("worker should emit Panic verdict before exiting");
        assert!(
            matches!(ve.verdict, Verdict::Panic),
            "expected Panic verdict, got {:?}",
            ve.verdict
        );

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while std::time::Instant::now() < deadline && h.state() != VerifierState::Dead {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(
            h.state(),
            VerifierState::Dead,
            "worker must transition to Dead after a caught panic"
        );
    }

    #[test]
    fn test_verifier_handle_queue_len_reports_used_slots() {
        // Construct a SignatureVerifierHandle directly (no spawned worker)
        // so the channel isn't being drained. This tests only the queue
        // arithmetic: `max_capacity() - capacity()` == number of enqueued
        // items.
        use crate::scp_verify::SignatureVerifierHandle;
        use std::sync::atomic::{AtomicU64, AtomicU8, AtomicUsize};
        use std::sync::Arc;

        let (tx, _rx) = tokio::sync::mpsc::channel::<PipelinedIntake>(8);
        let handle = SignatureVerifierHandle {
            tx: tx.clone(),
            state: Arc::new(AtomicU8::new(VerifierState::Running as u8)),
            heartbeat: Arc::new(AtomicU64::new(0)),
            backlog: Arc::new(AtomicUsize::new(0)),
        };

        assert_eq!(handle.queue_len(), 0, "empty channel reports 0 used slots");

        for i in 0..3 {
            tx.blocking_send(PipelinedIntake {
                envelope: make_unsigned_envelope(i as u64, 1),
                slot: i as u64,
                is_externalize: false,
                peer_id: None,
                enqueue_at: std::time::Instant::now(),
            })
            .expect("send");
        }

        assert_eq!(
            handle.queue_len(),
            3,
            "three enqueued items must report queue_len == 3"
        );
    }
}
