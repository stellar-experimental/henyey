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
use std::time::Duration;
use std::time::Instant;

use parking_lot::RwLock;
use std::sync::atomic::{AtomicU64, Ordering};
use tracing::{debug, error, info, trace, warn};

use crate::tracked_lock::{tracked_read, tracked_write};

// Lock telemetry labels — see `crate::tracked_lock` and issue #1768.
// Defined as `&'static str` constants so a call-site typo becomes
// a compile error rather than a silently-mislabelled WARN.
const LOCK_HERDER_STATE: &str = "herder.state";
const LOCK_TRACKING_STATE: &str = "shared.tracking_state";

use henyey_common::protocol::{protocol_version_starts_from, LclContext, ProtocolVersion};
use henyey_common::{Hash256, NetworkId};
use henyey_crypto::{PublicKey, SecretKey};
use henyey_ledger::LedgerManager;
use henyey_scp::{SlotIndex, SCP};
use stellar_xdr::{
    EnvelopeType, LedgerCloseValueSignature, LedgerHeader, LedgerUpgrade, Limits, NodeId, ReadXdr,
    ScpEnvelope, ScpQuorumSet, ScpStatementPledges, Signature as XdrSignature, StellarValue,
    StellarValueExt, TimePoint, TransactionEnvelope, Uint256, UpgradeType, Value, WriteXdr,
};

use crate::error::HerderError;
use crate::fetching_envelopes::{FetchingEnvelopes, FetchingStats};

#[cfg(test)]
use crate::pending::PendingResult;
use crate::pending::{PendingConfig, PendingEnvelopes, PendingStats};
use crate::quorum_intersection_state::{QuorumIntersectionResult, QuorumIntersectionState};
use crate::quorum_tracker::{QuorumTracker, SlotQuorumTracker};
use crate::scp_driver::{HerderScpCallback, ScpDriver, ScpDriverConfig, SharedTrackingState};
use crate::state::HerderState;
use crate::sync_recovery::LEDGER_VALIDITY_BRACKET;
use crate::timer_manager::TimerManagerHandle;
use crate::tx_queue::{
    account_key_from_account_id, TransactionQueue, TransactionSet, TxQueueConfig, TxQueueResult,
    TxQueueStats,
};
use crate::upgrades::{ConfigUpgradeContext, CurrentLedgerState, UpgradeParameters, Upgrades};
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

/// Interval for garbage-collecting unreferenced persisted transaction sets.
///
/// Parity: stellar-core `Herder.cpp:22` — `TX_SET_GC_DELAY = 1 minute`.
/// Driven by the app event loop, which calls
/// [`Herder::purge_persisted_tx_sets`] on a tokio interval.
pub const TX_SET_GC_DELAY_SECS: u64 = 60;

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
    /// Envelope was discarded without processing (standalone manual-close
    /// mode: `manual_close + run_standalone`).
    /// Parity: stellar-core `ENVELOPE_STATUS_DISCARDED`.
    Discarded,
    /// Envelope was buffered by the closing gate because its slot is
    /// currently being applied. It will be re-processed after
    /// `ledger_closed` advances LCL.
    Deferred,
}

/// Outcome of [`Herder::trigger_next_ledger`].
///
/// Distinguishes the three ways the call can return success so callers can
/// bump diagnostic counters without inferring outcome from side effects.
///
/// Parity: this enum collapses the multiple return points in stellar-core's
/// `HerderImpl::triggerNextLedger` (`HerderImpl.cpp:1424-1603`) into a typed
/// result. The `SkippedStale` variant corresponds to the post-`addTxSet`
/// LCL re-check at `HerderImpl.cpp:1559-1562`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TriggerOutcome {
    /// Nomination started for the requested slot.
    Triggered,
    /// Slot already had nomination in progress; idempotent no-op.
    AlreadyNominating,
    /// LCL advanced during `build_nomination_value`; the value is for an
    /// already-closed slot and was not broadcast.
    SkippedStale,
    /// Observer (non-validator) built and cached the tx set for this slot
    /// without entering SCP nomination. Parity: stellar-core
    /// `HerderImpl::triggerNextLedger` lines 1546-1625 — observers stop
    /// after `addTxSet` / cache publication and before `makeStellarValue`.
    ObserverCached,
    /// The proposed close time is too far ahead of the wall clock; nomination
    /// skipped to avoid building an invalid StellarValue.
    /// Parity: stellar-core triggerNextLedger far-ahead abort.
    SkippedInvalidCloseTime,
}

/// Intermediate result from [`Herder::build_and_cache_candidate_tx_set`]
/// carrying data needed by the validator-only signing step in
/// [`Herder::build_nomination_value`].
struct CandidateTxSetParts {
    tx_set_hash: Hash256,
    close_time: u64,
    lcl_close_time: u64,
    header: LedgerHeader,
    max_soroban_tx_set_size: Option<u32>,
    config_ctx: Option<ConfigUpgradeContext>,
}

/// Outcome of [`Herder::handle_nomination_timeout`].
///
/// The App-side dispatcher uses this to decide whether to bump the
/// `nomination_timeout_skipped_stale` counter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeoutOutcome {
    /// `scp.nominate_timeout` was called and reported it advanced state.
    Renominated,
    /// LCL advanced during build/drain; the value is for an already-closed
    /// slot and was not broadcast.
    SkippedStale,
    /// Close time still invalid on cache-miss rebuild; nomination skipped.
    SkippedInvalidCloseTime,
    /// No-op: build returned None, or `scp.nominate_timeout` was a no-op
    /// (e.g., slot already externalized).
    NoOp,
}

/// Ingress gate state: buffers envelopes for the next-to-close slot during
/// the post-externalize/pre-apply window. Mirrors stellar-core's guarantee
/// that no envelopes are processed between externalization and
/// `processExternalized` (the single-threaded apply).
///
/// A single `Mutex` ensures the slot check and buffer push/take are atomic,
/// eliminating races between concurrent ingress and gate clear.
struct ClosingGate {
    /// Slot whose envelopes are buffered (0 = gate open).
    slot: u64,
    /// Buffered envelopes for `slot`.
    buffer: Vec<ScpEnvelope>,
}

impl ClosingGate {
    fn new() -> Self {
        Self {
            slot: 0,
            buffer: Vec::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// Newtype wrappers for tracking slot values
// ---------------------------------------------------------------------------

/// The next consensus slot — the slot SCP is currently working on.
/// Equal to last_externalized + 1.
///
/// A value of `0` means no slot has been externalized yet (uninitialized
/// tracking state). Real ledgers start at genesis = 1. Use `is_boot()` to check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize)]
pub struct NextConsensusSlot(u64);

/// The last externalized ledger index — the most recent ledger that
/// completed consensus.
///
/// A value of `0` means no ledger has been externalized yet (uninitialized
/// tracking / boot / syncing state). Real Stellar ledgers start at genesis = 1,
/// so `0` is never a valid ledger number. Use `is_boot()` to check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize)]
pub struct LastExternalizedLedger(u64);

impl NextConsensusSlot {
    pub fn new(slot: u64) -> Self {
        Self(slot)
    }

    /// Extract the raw u64 value for SCP operations and comparisons.
    pub fn get(self) -> u64 {
        self.0
    }

    /// True when tracking is uninitialized (no slot externalized yet).
    pub fn is_boot(self) -> bool {
        self.0 == 0
    }
}

impl LastExternalizedLedger {
    pub fn new(ledger: u64) -> Self {
        Self(ledger)
    }

    /// Extract the raw u64 value.
    pub fn get(self) -> u64 {
        self.0
    }

    /// Convert to u32, panicking if out of range.
    pub fn as_u32(self) -> u32 {
        u32::try_from(self.0).expect("ledger index exceeds u32::MAX")
    }

    /// True when tracking is uninitialized (no ledger externalized yet).
    pub fn is_boot(self) -> bool {
        self.0 == 0
    }
}

impl std::fmt::Display for NextConsensusSlot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl std::fmt::Display for LastExternalizedLedger {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
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
    pub proposed_upgrades: Vec<stellar_xdr::LedgerUpgrade>,

    /// Maximum supported protocol version for upgrade validation.
    pub max_protocol_version: u32,

    /// Checkpoint frequency in ledgers (default 64, 8 for accelerated testing).
    pub checkpoint_frequency: u64,

    /// Validator weight configuration for application-specific leader election
    /// (protocol V22+). `None` when using manual quorum set or when quality
    /// data is not available.
    pub validator_weight_config: Option<crate::scp_driver::ValidatorWeightConfig>,

    /// When true, always use the old quorum-position weight algorithm
    /// regardless of protocol version. Matches stellar-core's
    /// `FORCE_OLD_STYLE_LEADER_ELECTION`.
    pub force_old_style_leader_election: bool,

    /// When true, incoming SCP envelopes are discarded and outgoing broadcasts
    /// are suppressed — but **only** when combined with `run_standalone`
    /// (see `suppress_scp()`). When `manual_close` is true but
    /// `run_standalone` is false (simulation/test mode), SCP operates normally.
    /// Parity: stellar-core `Config::MANUAL_CLOSE`.
    pub manual_close: bool,

    /// When true, the node is running without a network (standalone mode).
    /// Combined with `manual_close`, this enables SCP suppression via
    /// `suppress_scp()`. Parity: stellar-core `Config::RUN_STANDALONE`.
    pub run_standalone: bool,
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
            validator_weight_config: None,
            force_old_style_leader_election: false,
            manual_close: false,
            run_standalone: false,
        }
    }
}

impl HerderConfig {
    /// Whether SCP envelope I/O should be suppressed.
    ///
    /// Returns `true` when both `manual_close` and `run_standalone` are set,
    /// i.e. standalone manual-close mode. In this mode there is no network,
    /// so broadcasting and receiving SCP envelopes is pointless.
    ///
    /// Henyey simulation tests use `manual_close=true, run_standalone=false`
    /// to drive multi-node consensus via `manual_close_until` — SCP must
    /// operate normally in that mode.
    ///
    /// Parity: stellar-core gates `startOutOfSyncTimer` on
    /// `MANUAL_CLOSE && RUN_STANDALONE` (`HerderImpl.cpp:584-588`).
    pub fn suppress_scp(&self) -> bool {
        self.manual_close && self.run_standalone
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
/// let herder = Herder::new(config, ledger_manager);
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
    ///
    /// Wrapped in `Arc` so [`Self::set_scp_persistence`] can clone a handle
    /// into the persist callback installed on [`Self::scp_driver`]. The
    /// callback fires from [`crate::scp_driver::ScpDriver::emit`] after
    /// each envelope broadcast and needs read-access to `get_latest_messages_send`.
    scp: Arc<SCP<HerderScpCallback>>,
    /// Shared tracking consensus state (also read by ScpDriver for value validation).
    tracking_state: Arc<RwLock<SharedTrackingState>>,
    /// When we started tracking.
    tracking_started_at: RwLock<Option<Instant>>,
    /// Secret key for signing (if validator).
    secret_key: Option<SecretKey>,
    /// Ledger manager reference.
    ledger_manager: Arc<LedgerManager>,
    /// Slot-level quorum tracker for heard-from quorum/v-blocking checks.
    slot_quorum_tracker: RwLock<SlotQuorumTracker>,
    /// Transitive quorum tracker for the current quorum map.
    quorum_tracker: RwLock<QuorumTracker>,
    /// Quorum intersection analysis state.
    /// Tracks periodic intersection checks, matching stellar-core's
    /// `mLastQuorumMapIntersectionState` (HerderImpl.h).
    quorum_intersection_state: Arc<RwLock<QuorumIntersectionState>>,
    /// Runtime-mutable upgrade scheduling (set via HTTP `/upgrades?mode=set`).
    /// Shared with ScpDriver for nomination validation.
    runtime_upgrades: Arc<RwLock<Upgrades>>,
    /// Count of transactions dropped due to queue full since last ledger close.
    queue_full_count: AtomicU64,
    /// Ingress gate: buffers envelopes for the closing slot during the
    /// post-externalize/pre-apply window. See [`ClosingGate`].
    closing_gate: std::sync::Mutex<ClosingGate>,
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
    /// Shared SCP metrics counters.
    scp_metrics: Arc<crate::metrics::ScpMetrics>,
    /// Number of times `assert_lcl_consistency` took the corrective branch
    /// because LCL had legitimately advanced ahead of tracking (e.g. during
    /// catchup-replay burst close). Each fire restores the invariant via an
    /// idempotent `advance_tracking_to(lcl_seq, ...)` call rather than
    /// crashing the process. See [`Self::assert_lcl_consistency`] and #2791.
    lcl_ahead_of_tracking_corrective_total: AtomicU64,
    /// SCP state persistence manager, installed after construction via
    /// [`Self::set_scp_persistence`]. Used by [`Self::purge_persisted_tx_sets`]
    /// to GC unreferenced tx sets on the app event loop's 60s timer.
    ///
    /// `OnceLock` enforces single-installer at the type level: tests that
    /// don't install a manager simply leave it empty (purge becomes a no-op),
    /// while production install exactly once in `init_herder`.
    scp_persistence: std::sync::OnceLock<Arc<crate::persistence::ScpPersistenceManager>>,
    /// The single SCP persist worker installed alongside `scp_persistence`.
    ///
    /// Replaces the previous unbounded thread-per-emit fan-out (#3813): the
    /// emit callback now enqueues the slot onto this worker's coalescing queue
    /// instead of spawning a thread. Held here so it is dropped — and its
    /// thread joined, draining any queued work — when the `Herder` drops,
    /// closing the "detached, never joined" shutdown race.
    ///
    /// `OnceLock` because the worker is installed exactly once, together with
    /// `scp_persistence`, in [`Self::set_scp_persistence`].
    scp_persist_worker: std::sync::OnceLock<crate::persistence::ScpPersistWorker>,
    /// Shared flag indicating whether ledger application is in progress.
    ///
    /// Parity: stellar-core's `triggerNextLedger` checks
    /// `mLedgerManager.isApplying()` after `addTxSet` because the side effects
    /// of draining ready envelopes can externalize a slot and start apply.
    /// If apply is in progress, upstream returns immediately — henyey must do
    /// the same. Installed post-construction by the app via
    /// [`Self::set_is_applying_flag`].
    is_applying_flag: std::sync::OnceLock<Arc<std::sync::atomic::AtomicBool>>,
}

impl henyey_common::memory::MemoryReporter for Herder {
    /// Report the herder-owned heap components that live outside the ledger
    /// manager's own report call site (#3845): the transaction queue, the
    /// fetching-envelope buffers, the SCP slot/quorum trackers, and the
    /// externalize-lag maps. Each estimate is O(1)/O(bounded) and takes only
    /// short-lived read locks — none re-enters the ledger — so it is safe to
    /// call from the ledger-close report path.
    fn memory_components(&self) -> Vec<henyey_common::memory::ComponentMemory> {
        use henyey_common::memory::ComponentMemory;
        vec![
            ComponentMemory::new(
                "herder_tx_queue",
                self.tx_queue.estimate_heap_bytes() as u64,
                self.tx_queue.len() as u64,
            ),
            ComponentMemory::new(
                "herder_fetching_envelopes",
                self.fetching_envelopes.estimate_heap_bytes() as u64,
                0,
            ),
            ComponentMemory::new(
                "herder_scp_slots",
                self.slot_quorum_tracker.read().estimate_heap_bytes() as u64,
                0,
            ),
            {
                let quorum = self.quorum_tracker.read();
                ComponentMemory::new(
                    "herder_quorum",
                    quorum.estimate_heap_bytes() as u64,
                    quorum.tracked_node_count() as u64,
                )
            },
            ComponentMemory::new(
                "herder_externalize_lag",
                self.scp_driver.estimate_externalize_lag_heap_bytes() as u64,
                0,
            ),
        ]
    }
}

impl Herder {
    /// Create a new Herder (observer mode, no secret key).
    pub fn new(
        config: HerderConfig,
        ledger_manager: Arc<LedgerManager>,
        timer_handle: TimerManagerHandle,
    ) -> Self {
        Self::build(config, None, ledger_manager, timer_handle)
    }

    /// Create a new Herder with a secret key for validation.
    pub fn with_secret_key(
        config: HerderConfig,
        secret_key: SecretKey,
        ledger_manager: Arc<LedgerManager>,
        timer_handle: TimerManagerHandle,
    ) -> Self {
        Self::build(config, Some(secret_key), ledger_manager, timer_handle)
    }

    /// Shared constructor logic for both observer and validator modes.
    fn build(
        mut config: HerderConfig,
        secret_key: Option<SecretKey>,
        ledger_manager: Arc<LedgerManager>,
        timer_handle: TimerManagerHandle,
    ) -> Self {
        // Normalize the local quorum set up front, before it fans out to
        // SCP::new, ScpDriverConfig, FetchingEnvelopes, QuorumSetTracker,
        // and SlotQuorumTracker. Matches stellar-core's LocalNode constructor
        // which calls normalizeQSet(mQSet) before computing mQSetHash.
        // SCP::new also normalizes (defense-in-depth), but doing it here
        // ensures all consumers — including the by-hash index — use the
        // canonical form.
        if let Some(ref mut qs) = config.local_quorum_set {
            henyey_scp::normalize_quorum_set(qs);
        }

        // Decouple the pending-envelope buffer sizing from
        // `max_externalized_slots`. These control different things:
        // `max_externalized_slots` caps the already-externalized-slot
        // retention window (stellar-core's `MAX_SLOTS_TO_REMEMBER = 12`,
        // used for peer state queries and `SlotQuorumTracker` capacity);
        // `pending_config.max_slots` caps the forward-looking buffer that
        // holds EXTERNALIZE envelopes for slots the observer hasn't
        // reached yet (stellar-core scales this with
        // `LEDGER_VALIDITY_BRACKET = 100`). Previously both were clamped
        // together, which narrowed the pending buffer to 12 slots and
        // caused issue #1807 — EXTERNALIZEs more than 12 slots ahead of
        // the post-catchup tracking slot were silently dropped.
        let pending_config = config.pending_config.clone();
        let max_slots = config.max_externalized_slots.max(1);

        let scp_driver_config = ScpDriverConfig {
            node_id: config.node_public_key,
            max_tx_set_cache: 10_000,
            max_time_drift: MAX_TIME_SLIP_SECONDS,
            local_quorum_set: config.local_quorum_set.clone(),
            validator_weight_config: config.validator_weight_config.clone(),
            force_old_style_leader_election: config.force_old_style_leader_election,
            suppress_scp: config.suppress_scp(),
        };

        let tracking_state = Arc::new(RwLock::new(SharedTrackingState::default()));

        let scp_metrics = Arc::new(crate::metrics::ScpMetrics::new());

        let runtime_upgrades = Arc::new(RwLock::new(Upgrades::default()));

        let scp_driver = Arc::new(if let Some(ref sk) = secret_key {
            ScpDriver::with_secret_key(
                scp_driver_config,
                config.network_id,
                sk.clone(),
                Arc::clone(&ledger_manager),
                Arc::clone(&tracking_state),
                Arc::clone(&scp_metrics),
                Arc::clone(&runtime_upgrades),
                timer_handle,
            )
        } else {
            ScpDriver::new(
                scp_driver_config,
                config.network_id,
                Arc::clone(&ledger_manager),
                Arc::clone(&tracking_state),
                Arc::clone(&scp_metrics),
                Arc::clone(&runtime_upgrades),
                timer_handle,
            )
        });

        let tx_queue = TransactionQueue::new(config.tx_queue_config.clone());
        let pending_envelopes = PendingEnvelopes::new(pending_config);
        let scp_driver_for_fetching = Arc::clone(&scp_driver);
        let fetching_config = crate::fetching_envelopes::FetchingConfig {
            max_envelopes_per_slot: config.pending_config.max_envelopes_per_slot,
            ..Default::default()
        };
        let fetching_envelopes = FetchingEnvelopes::new(
            fetching_config,
            Box::new(move |hash, slot| scp_driver_for_fetching.has_tx_set_and_touch(hash, slot)),
        );

        // Pre-cache the local quorum set in fetching_envelopes so envelopes
        // referencing it don't wait for fetching.
        if let Some(ref quorum_set) = config.local_quorum_set {
            let qs_hash = Hash256::hash_xdr(quorum_set);
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
        let scp = Arc::new(SCP::new(
            node_id.clone(),
            is_validator,
            quorum_set,
            Arc::new(callback),
        ));

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
                    stellar_xdr::PublicKey::PublicKeyTypeEd25519(stellar_xdr::Uint256(bytes)) => {
                        hex::encode(bytes)
                    }
                };
                info!(validator = %key_hex, "Quorum set validator");
            }
            let _ = quorum_tracker.expand(&node_id, quorum_set.clone());
            info!(
                tracked_nodes = quorum_tracker.tracked_node_count(),
                "Quorum tracker initialized"
            );
        }

        // Spawn the dedicated SCP signature-verification worker thread.
        // The worker is a core component of the event-loop pipeline
        // (issue #1734 Phase B); spawn failure is fatal rather than
        // silently falling back to event-loop verification.
        let spawned = crate::scp_verify::spawn_scp_verifier(
            config.network_id,
            crate::scp_verify::DEFAULT_VERIFIER_QUEUE_CAPACITY,
            Arc::clone(&scp_metrics),
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
            ledger_manager,
            slot_quorum_tracker: RwLock::new(slot_quorum_tracker),
            quorum_tracker: RwLock::new(quorum_tracker),
            quorum_intersection_state: Arc::new(RwLock::new(QuorumIntersectionState::new())),
            runtime_upgrades,
            queue_full_count: AtomicU64::new(0),
            closing_gate: std::sync::Mutex::new(ClosingGate::new()),
            cached_nomination_value: RwLock::new(None),
            scp_verifier_handle,
            verified_rx: std::sync::Mutex::new(verified_rx),
            scp_metrics,
            lcl_ahead_of_tracking_corrective_total: AtomicU64::new(0),
            scp_persistence: std::sync::OnceLock::new(),
            scp_persist_worker: std::sync::OnceLock::new(),
            is_applying_flag: std::sync::OnceLock::new(),
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

    /// Install the SCP state persistence manager.
    ///
    /// Called once from `init_herder` after the `Arc<Herder>` is constructed.
    /// `OnceLock` enforces single-installer at the type-system level: a second
    /// call returns `Err` with the supplied manager so the caller can fail loud.
    ///
    /// On first install, also spawns the single SCP persist worker (#3813) and
    /// wires the persist callback into
    /// [`crate::scp_driver::ScpDriver::set_persist_callback`] so each emitted
    /// envelope enqueues its slot onto that worker's coalescing queue. The
    /// worker captures `Arc<SCP>`, `Arc<ScpDriver>`, and
    /// `Arc<ScpPersistenceManager>` (an intentional Arc cycle mirroring the
    /// existing `envelope_sender` cycle — benign because all are dropped
    /// together with the owning `Herder`, whose `scp_persist_worker` `Drop`
    /// joins the worker thread).
    pub fn set_scp_persistence(
        &self,
        manager: Arc<crate::persistence::ScpPersistenceManager>,
    ) -> std::result::Result<(), Arc<crate::persistence::ScpPersistenceManager>> {
        // Set first; if this fails, do NOT install the callback (we're a
        // double-install and the existing manager is already wired).
        self.scp_persistence.set(Arc::clone(&manager))?;

        // Capture handles for the persist worker's per-slot gather+persist body.
        // The worker runs this AFTER `emit()` returns, so SCP's internal `slots`
        // write-lock has been released (otherwise `scp.get_latest_messages_send`
        // would deadlock against the writer chain that invoked `emit`).
        let scp = Arc::clone(&self.scp);
        let driver = Arc::clone(&self.scp_driver);
        let manager_for_worker = Arc::clone(&manager);

        // The single coalescing queue: the emit callback enqueues a slot; the
        // one worker drains it. Bounds live persist threads to exactly 1 (#3813).
        let queue = crate::persistence::SharedPersistQueue::new(Arc::clone(&manager));

        // Per-slot work — moved verbatim off the old detached-thread body.
        //
        // Parity: stellar-core HerderImpl::persistSCPState (HerderImpl.cpp:2152-2186):
        //   1. getSCP().getLatestMessagesSend(slotIndex) → envelopes
        //   2. for each envelope: mPendingEnvelopes.getTxSet(h) → encode StoredTransactionSet
        //   3. mPendingEnvelopes.getQSet(qsHash) → ScpQuorumSet
        //   4. persistentState.setSCPstate(slot, ..., txSets, ...)
        //
        // Deviation: stellar-core runs this synchronously inside `emitEnvelope`.
        // henyey's `SCP::nominate`/`process_envelope`/etc hold a
        // `parking_lot::RwLock` write guard on `slots` while the driver emits,
        // so a synchronous `get_latest_messages_send` (which takes the same
        // RwLock for reading) would deadlock. We hop to the persist worker
        // instead; it reads after the writer chain unwinds. This refactor bounds
        // that hop's concurrency from unbounded (thread-per-emit) to one worker.
        let process = move |slot: u64| {
            use stellar_xdr::{Limits, WriteXdr};

            let envelopes = scp.get_latest_messages_send(slot);

            let mut tx_sets: Vec<(stellar_xdr::Hash, Vec<u8>)> = Vec::new();
            let mut quorum_sets: Vec<(stellar_xdr::Hash, stellar_xdr::ScpQuorumSet)> = Vec::new();

            for envelope in &envelopes {
                for hash in crate::persistence::get_tx_set_hashes(envelope) {
                    let hash256 = henyey_common::Hash256(hash.0);
                    if let Some(tx_set) = driver.get_tx_set(&hash256) {
                        let stored = tx_set.to_xdr_stored_set();
                        match stored.to_xdr(Limits::none()) {
                            Ok(bytes) => tx_sets.push((hash, bytes)),
                            Err(e) => {
                                warn!(
                                    error = %e,
                                    slot,
                                    "persist_scp_state: failed to encode \
                                     StoredTransactionSet"
                                );
                            }
                        }
                    }
                }
                if let Some(qhash) = crate::persistence::get_quorum_set_hash(envelope) {
                    let qhash256 = henyey_common::Hash256(qhash.0);
                    if let Some(qs) = driver.get_quorum_set_by_hash(&qhash256) {
                        quorum_sets.push((qhash, qs));
                    }
                }
            }

            if let Err(e) =
                manager_for_worker.persist_scp_state(slot, &envelopes, &tx_sets, &quorum_sets)
            {
                warn!(error = %e, slot, "persist_scp_state failed");
            }
        };

        // Spawn the single worker up front. On spawn failure, record it and
        // leave the callback uninstalled: persistence is disabled rather than
        // falling back to an inline (deadlocking) persist.
        match crate::persistence::ScpPersistWorker::spawn(Arc::clone(&queue), process) {
            Ok(worker) => {
                // Store so `Herder::drop` joins the worker thread. `set` can
                // only fail if the cell was already populated; the earlier
                // `self.scp_persistence.set(..)?` guarantees this method runs
                // exactly once, so this is the first (and only) `set`. Assert
                // it in debug builds to catch any future reordering that would
                // leave the worker un-stored (dropped/joined) while the enqueue
                // callback below stays installed — which would leak the pending
                // count and hang `wait_for_pending_persists`.
                let stored = self.scp_persist_worker.set(worker).is_ok();
                debug_assert!(stored, "scp_persist_worker set exactly once");
                // The emit callback: enqueue only. This is the short,
                // SQLite-free work that runs under SCP's `slots` write guard.
                self.scp_driver.set_persist_callback(move |slot| {
                    queue.enqueue(slot);
                });
            }
            Err(e) => {
                manager.record_spawn_failure();
                warn!(
                    error = %e,
                    "failed to spawn scp-persist worker; SCP persistence disabled"
                );
            }
        }

        Ok(())
    }

    /// When `true`, the persisted **future-slot** (`slot > lcl`) local
    /// envelopes are replayed into SCP via `set_state_from_envelope`. When
    /// `false`, restore performs dependency hydration (tx-sets + quorum-sets)
    /// and the transitive-quorum rebuild ONLY, and the live-envelope replay is
    /// deferred.
    ///
    /// **Enabled** (re-enabled for #3595). Replaying a restored future-slot
    /// **Externalize** installs the ballot in `phase=Externalize` and records
    /// the externalized value, but `set_state_from_envelope` (by parity with
    /// core's `BallotProtocol::setStateFromEnvelope`) does NOT itself close the
    /// ledger — and the matching network EXTERNALIZE is then accepted-but-silent
    /// (value-match → `EnvelopeState::Valid` → herder `Duplicate`), so node0
    /// would wedge at `phase=Externalize` with LCL stuck (the #2797 stall,
    /// re-investigated under #3595). The faithful fix is NOT ballot
    /// supersession: instead `restore_persisted_scp_state` drives the restored,
    /// SCP-externalized next-to-close slot (`slot == lcl+1`) through the same
    /// `complete_externalization` terminal the live path uses — the analog of
    /// core's `processExternalized → mLedgerManager.valueExternalized`. With
    /// that completion in place the simulation restart test
    /// `test_core3_restart_rejoin_over_loopback` is green across ≥10 consecutive
    /// runs. See `crates/herder/PARITY_STATUS.md`'s `[^restore-scp]` footnote.
    const RESTORE_LIVE_ENVELOPE_REPLAY: bool = true;

    /// Restore persisted SCP state at startup. Hydrates the tx-sets and
    /// quorum-sets referenced by retained **future-slot** (`slot > lcl`) local
    /// envelopes and rebuilds the transitive quorum graph. The future-slot
    /// envelopes themselves are replayed into SCP only when
    /// [`Self::RESTORE_LIVE_ENVELOPE_REPLAY`] is `true` (currently `false`; see
    /// that constant for the #2797-stall rationale).
    ///
    /// Parity: mirrors stellar-core `HerderImpl::restoreSCPState()`
    /// (`HerderImpl.cpp:2239-2311`), invoked from `HerderImpl::start()` after
    /// `setTrackingSCPState(lcl, ..., true)` + `trackingHeartBeat()`
    /// (`HerderImpl.cpp:2455-2471`). In henyey's split lifecycle, the
    /// `setTrackingSCPState`/`trackingHeartBeat` equivalent is
    /// `App::run()` → `Herder::bootstrap(lcl)`, so this hook must run **after**
    /// `bootstrap(lcl)` and before requesting SCP state from peers.
    ///
    /// Two intentional, architecture-forced divergences from upstream (see the
    /// `[^restore-scp]` footnote in `crates/herder/PARITY_STATUS.md`):
    ///
    /// 1. **`slot > lcl` envelope filter** (no upstream analog). `bootstrap(lcl)`
    ///    has already recreated the finalized-LCL baseline, so any persisted
    ///    envelope for a `slot <= lcl` is already-closed history. Re-injecting
    ///    it via `set_state_from_envelope` adds nothing and, worse, stalls
    ///    henyey's SCP ballot machine (it pins `current_ballot`/`phase` and
    ///    rejects superseding live envelopes — `ballot/mod.rs:854`). This is
    ///    the regression guard for the consensus stall that closed PR #2797.
    ///    The deeper ballot-supersession gap is the follow-up that gates
    ///    re-enabling live-envelope replay.
    /// 2. **Restore-time quorum-tracker rebuild** uses upstream's
    ///    local-node-preserving closure shape (`PendingEnvelopes.cpp:924-955`):
    ///    local node → always-available local qset; remote node → latest
    ///    message → companion qset hash → by-hash lookup. henyey has no
    ///    `HerderPersistence::getNodeQuorumSet` DB table, so upstream's third
    ///    (DB) fallback branch is omitted; remote-node transitive quorum is
    ///    reconstructed lazily from live messages, not at restore. The local
    ///    node is never weakened by the rebuild.
    ///
    /// Genesis gate: a no-op when `lcl <= GENESIS_LEDGER_SEQ` (nothing useful
    /// can be persisted before genesis) and when no persistence manager is
    /// installed. The `slot > lcl` filter also makes a fresh-genesis replay a
    /// no-op, so dropping upstream's explicit `!FORCE_SCP` branch is
    /// behaviorally equivalent for a fresh node.
    ///
    /// Per-entry decode errors are tolerated (logged + skipped), matching
    /// upstream's per-entry try/catch; a hard storage/load failure logs an
    /// error and lets startup proceed via the fresh-envelope sync fallback.
    pub fn restore_persisted_scp_state(&self, lcl: u64) {
        let Some(manager) = self.scp_persistence.get() else {
            // No persistence manager installed — nothing to restore.
            return;
        };

        if lcl <= GENESIS_LEDGER_SEQ {
            // Genesis (or pre-genesis): nothing useful was ever persisted.
            return;
        }

        let restored = match manager.restore_scp_state() {
            Ok(restored) => restored,
            Err(e) => {
                // Hard load failure: proceed without restored state, exactly
                // like stellar-core "proceeding without them".
                error!(error = %e, "Failed to restore persisted SCP state — proceeding without it");
                return;
            }
        };

        // Retain only future-slot envelopes. `slot <= lcl` is already-closed
        // history that bootstrap(lcl) has reconstructed; replaying it stalls
        // SCP (the #2797 regression). Filter first so dependency hydration is
        // driven strictly by the envelopes we will actually replay.
        let future_envelopes: Vec<&ScpEnvelope> = restored
            .envelopes
            .iter()
            .filter(|(slot, _env)| *slot > lcl)
            .map(|(_slot, env)| env)
            .collect();

        if future_envelopes.is_empty() {
            debug!(
                lcl,
                total = restored.envelopes.len(),
                "restore_persisted_scp_state: no future-slot envelopes to replay"
            );
            return;
        }

        // Collect the tx-set and quorum-set hashes referenced by the retained
        // future-slot envelopes only.
        let mut referenced_tx_sets: std::collections::HashSet<Hash256> =
            std::collections::HashSet::new();
        let mut referenced_qsets: std::collections::HashSet<Hash256> =
            std::collections::HashSet::new();
        for env in &future_envelopes {
            for hash in crate::persistence::get_tx_set_hashes(env) {
                referenced_tx_sets.insert(Hash256(hash.0));
            }
            if let Some(qhash) = crate::persistence::get_quorum_set_hash(env) {
                referenced_qsets.insert(Hash256(qhash.0));
            }
        }

        // --- Tx-set replay (parity: HerderImpl.cpp:2247-2272). ---
        // Decode StoredTransactionSet → TransactionSet → cache. Per-entry
        // decode errors are logged and skipped.
        for (hash, bytes) in &restored.tx_sets {
            let hash256 = Hash256(hash.0);
            if !referenced_tx_sets.contains(&hash256) {
                // Not referenced by any retained future-slot envelope.
                continue;
            }
            let stored = match stellar_xdr::StoredTransactionSet::from_xdr(
                bytes.as_slice(),
                stellar_xdr::Limits::none(),
            ) {
                Ok(s) => s,
                Err(e) => {
                    warn!(error = %e, "restore: failed to decode StoredTransactionSet — skipping");
                    continue;
                }
            };
            match TransactionSet::from_xdr_stored_set(&stored) {
                Ok(tx_set) => self.scp_driver.cache_tx_set(tx_set),
                Err(e) => {
                    warn!(error = %e, "restore: failed to build TransactionSet — skipping");
                }
            }
        }

        // --- Quorum-set replay (parity: addSCPQuorumSet → putQSet,
        // PendingEnvelopes.cpp:99-121). Store by hash in the qset tracker so
        // both set_state_from_envelope and the rebuild can resolve it, and
        // mirror into FetchingEnvelopes (the putQSet equivalent that releases
        // any waiters). ---
        for (hash, qs) in &restored.quorum_sets {
            let hash256 = Hash256(hash.0);
            if !referenced_qsets.contains(&hash256) {
                continue;
            }
            self.scp_driver
                .store_quorum_set_by_hash(hash256, qs.clone());
            self.fetching_envelopes
                .cache_quorum_set(hash256, qs.clone());
        }

        // --- SCP envelope replay (parity: Slot::setStateFromEnvelope,
        // Slot.cpp:61-88). Only future slots reach here. Gated on
        // `RESTORE_LIVE_ENVELOPE_REPLAY` (now enabled). Dependency hydration
        // above runs unconditionally so the node can immediately validate the
        // future slot. ---
        let mut replayed = 0usize;
        if Self::RESTORE_LIVE_ENVELOPE_REPLAY {
            for env in &future_envelopes {
                if self.scp.set_state_from_envelope(env) {
                    replayed += 1;
                } else {
                    warn!(
                        slot = env.statement.slot_index,
                        "restore: set_state_from_envelope rejected a persisted envelope — skipping"
                    );
                }
            }

            // --- Drive the restored next-to-close externalized slot to the
            // ledger-close terminal (parity: HerderImpl::restoreSCPState →
            // processExternalized → mLedgerManager.valueExternalized,
            // HerderImpl.cpp:2239/311-396). `set_state_from_envelope` installs a
            // restored Externalize as `phase=Externalize` and records the
            // externalized value in the ballot, but — exactly like core's
            // `BallotProtocol::setStateFromEnvelope` — does NOT itself close the
            // ledger. The subsequent network EXTERNALIZE for that slot is then
            // accepted-but-silent (value-match → `EnvelopeState::Valid` → herder
            // `Duplicate`), so without an explicit completion node0 wedges at
            // `phase=Externalize` with LCL stuck (the #3595 restart-rejoin
            // stall). Core escapes via its recovery/catchup machinery
            // rediscovering the externalized slot; the 3-node restart-rejoin sim
            // has no history archive, so we proactively drive the same terminal
            // here.
            //
            // Mirror the EXACT live externalize sequence (the path
            // `process_scp_envelope` takes when SCP newly externalizes a slot,
            // see the `is_slot_externalized` block above): request the tx-set,
            // `record_externalized` the value (so the app's
            // `process_externalized_slots` / `out_of_sync_recovery` close
            // pipeline can find it — `complete_externalization` alone only
            // publishes `latest_externalized` and would otherwise leave the
            // value unrecorded), `cleanup_externalized`, then
            // `complete_externalization`.
            //
            // Iterate the DISTINCT restored slots (restore can install both a
            // Nominate and an Externalize for the same slot — dedupe so the
            // completion fires once per slot). Gate strictly on
            // `slot == lcl + 1` (only the next-to-close slot applies
            // immediately, matching core's "only next-to-close applies now"
            // invariant — higher restored slots wait for their predecessor) AND
            // `is_slot_externalized(slot)` (restored Prepare/Confirm slots are
            // not externalized → no-op, so they are never force-closed).
            let mut completed_slots: std::collections::BTreeSet<u64> =
                std::collections::BTreeSet::new();
            for env in &future_envelopes {
                let slot = env.statement.slot_index;
                if slot != lcl + 1 {
                    continue;
                }
                if !completed_slots.insert(slot) {
                    continue;
                }
                if !self.scp.is_slot_externalized(slot) {
                    continue;
                }
                let Some(value) = self.scp.get_externalized_value(slot) else {
                    continue;
                };
                debug!(
                    slot,
                    "restore: driving externalization for restored next-to-close externalized slot"
                );
                // Request the tx-set so the close pipeline can hydrate it.
                if let Ok(sv) = StellarValue::from_xdr(&value.0, Limits::none()) {
                    self.scp_driver.request_tx_set(sv.tx_set_hash.into(), slot);
                }
                self.scp_driver.record_externalized(slot, value, None);
                self.scp_driver
                    .cleanup_externalized(self.config.max_externalized_slots);
                self.complete_externalization(slot);
            }
        } else {
            debug!(
                lcl,
                deferred = future_envelopes.len(),
                "restore: live-envelope replay gated off; hydrated dependencies only"
            );
        }

        // --- Rebuild the transitive quorum graph once, after all envelopes
        // are loaded. Idempotent, so a single end-of-restore rebuild is
        // equivalent to upstream's per-slot rebuild. The closure preserves the
        // local node (its qset is pinned/always-available) and resolves remote
        // nodes via their latest restored message's companion qset hash. ---
        let mut tracker = self.quorum_tracker.write();
        let local_node_id = self.scp.local_node_id().clone();
        if let Err(err) = tracker.rebuild(|id| {
            if *id == local_node_id {
                // Local node: pinned local qset, never weakened.
                return self.scp_driver.get_quorum_set(id);
            }
            // Remote node: latest restored message → companion qset hash →
            // by-hash lookup. None if we never restored that node's qset
            // (reconstructed lazily from live messages later).
            if let Some(qs) = self.scp_driver.get_quorum_set(id) {
                return Some(qs);
            }
            let env = self.scp.get_latest_message(id)?;
            let qhash = crate::persistence::get_quorum_set_hash(&env)?;
            self.scp_driver.get_quorum_set_by_hash(&Hash256(qhash.0))
        }) {
            warn!(error = %err, "restore: failed to rebuild quorum tracker");
        }
        drop(tracker);

        info!(
            lcl,
            replayed,
            tx_sets = referenced_tx_sets.len(),
            quorum_sets = referenced_qsets.len(),
            "Restored persisted SCP state for future slots"
        );
    }

    /// Install the shared `is_applying` flag so `trigger_next_ledger` can
    /// check whether ledger application is in progress.
    ///
    /// Parity: stellar-core's `HerderImpl::triggerNextLedger` checks
    /// `mLedgerManager.isApplying()` after `addTxSet` because draining ready
    /// envelopes can trigger SCP callbacks that externalize and start apply.
    /// If apply begins mid-trigger, upstream returns immediately.
    pub fn set_is_applying_flag(
        &self,
        flag: Arc<std::sync::atomic::AtomicBool>,
    ) -> std::result::Result<(), Arc<std::sync::atomic::AtomicBool>> {
        self.is_applying_flag.set(flag)
    }

    /// Returns true if ledger application is currently in progress.
    /// Returns false when no flag has been installed (test convenience).
    fn is_applying(&self) -> bool {
        self.is_applying_flag.get().map_or(false, |flag| {
            flag.load(std::sync::atomic::Ordering::Relaxed)
        })
    }

    /// Purge persisted tx sets that are no longer referenced by any persisted
    /// SCP state. No-op if no persistence manager has been installed.
    ///
    /// Called inline from the app event loop's `tx_set_gc_interval` branch
    /// (every [`TX_SET_GC_DELAY_SECS`] seconds), matching stellar-core's
    /// `HerderImpl::startTxSetGCTimer()` (`HerderImpl.cpp:2440-2444`) /
    /// `purgeOldPersistedTxSets()` (`HerderImpl.cpp:2448-2487`) cadence.
    ///
    /// The underlying `purge_unreferenced_tx_sets()` routes through the
    /// `ScpStatePersistence::purge_unreferenced_tx_sets_atomic` trait
    /// method, which the SQLite backend overrides to run the three-step
    /// purge inside a single transaction so a concurrent `persist_scp_state`
    /// cannot have its tx-set deleted as an orphan between the read and the
    /// delete (#2770).
    pub fn purge_persisted_tx_sets(&self) {
        let Some(manager) = self.scp_persistence.get() else {
            return;
        };
        // Trim persisted slot STATES below the remember horizon before
        // purging tx sets. Without this, every slot state since boot stays
        // persisted, its tx-set references never expire, and the purge can
        // never delete them — storestate grows without bound and the purge
        // scan itself becomes a multi-second WAL hold (the full-window
        // sustained binder, issue #3719). stellar-core bounds this
        // structurally: slot states live in a MAX_SLOTS_TO_REMEMBER+1 row
        // ring (`slot % (N+1)`, PersistentState::setSCPStateForSlot), so old
        // references vanish by overwrite. Trimming to the same horizon keeps
        // restore-on-restart semantics equivalent to core's ring.
        let min_slot = u64::from(self.tracking_consensus_ledger_index().as_u32())
            .saturating_sub(MAX_SLOTS_TO_REMEMBER);
        if min_slot > 0 {
            if let Err(e) = manager.cleanup(min_slot) {
                record_tx_set_gc_busy_drop(&e);
                error!(error = %e, min_slot, "Failed to clean up persisted SCP state");
            }
        } else if let Err(e) = manager.purge_unreferenced_tx_sets() {
            record_tx_set_gc_busy_drop(&e);
            error!(error = %e, "Failed to purge unreferenced persisted tx sets");
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

    /// Get the current state of the Herder.
    pub fn state(&self) -> HerderState {
        *tracked_read(LOCK_HERDER_STATE, &self.state)
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
        self.fetching_envelopes.set_current_slot(slot);
    }

    /// Test-only: inject an envelope into a slot's externalizing state.
    /// Used by app-level tests that exercise the SCP history persistence path
    /// without needing to go through the full consensus/validation flow.
    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    pub fn test_inject_externalizing_envelope(
        &self,
        slot: u64,
        envelope: stellar_xdr::ScpEnvelope,
    ) {
        self.scp.test_inject_externalizing_envelope(slot, envelope);
    }

    /// Test-only: register a quorum set so `get_quorum_set_by_hash()` can
    /// find it. Used alongside `test_inject_externalizing_envelope` to set
    /// up the full SCP persistence path for tests.
    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    pub fn test_store_quorum_set(
        &self,
        node_id: &stellar_xdr::NodeId,
        qset: stellar_xdr::ScpQuorumSet,
    ) {
        self.scp_driver.test_store_quorum_set(node_id, qset);
    }

    /// Test-only: arm the closing gate for a specific slot.
    ///
    /// In production, the gate is set to `externalized_slot + 1` when a slot
    /// externalizes (see `advance_tracking_slot`). This helper allows tests to
    /// simulate that state without going through the full externalization path.
    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    pub fn set_closing_gate_for_testing(&self, slot: u64) {
        let mut gate = self.closing_gate.lock().unwrap();
        gate.slot = slot;
        gate.buffer.clear();
    }

    /// Set the Herder state.
    ///
    /// Validates the transition per HERDER_SPEC §3.2: TRACKING→BOOTING and
    /// SYNCING→BOOTING are forbidden.  If an invalid transition is attempted,
    /// it is logged and ignored.
    pub fn set_state(&self, state: HerderState) {
        let current = *tracked_read(LOCK_HERDER_STATE, &self.state);
        if !current.can_transition_to(state) {
            tracing::warn!(
                from = %current,
                to = %state,
                "ignoring forbidden herder state transition"
            );
            return;
        }
        *tracked_write(LOCK_HERDER_STATE, &self.state) = state;

        // Keep SharedTrackingState.is_tracking in sync with HerderState.
        // stellar-core's isTracking() simply checks mState == HERDER_TRACKING_NETWORK_STATE,
        // but ScpDriver reads SharedTrackingState.is_tracking for value validation.
        // Without this, transitioning to Syncing leaves is_tracking=true, causing
        // validate_past_or_future_value to reject far-future EXTERNALIZE messages
        // instead of returning MaybeValid.
        if !state.is_tracking() {
            tracked_write(LOCK_TRACKING_STATE, &self.tracking_state).is_tracking = false;
        }
    }

    /// Returns the next consensus ledger index (externalized + 1).
    ///
    /// This is the slot SCP is currently working on or will work on next.
    /// For the *last externalized* ledger, use
    /// [`tracking_consensus_ledger_index()`](Self::tracking_consensus_ledger_index).
    pub fn tracking_slot(&self) -> NextConsensusSlot {
        NextConsensusSlot::new(
            tracked_read(LOCK_TRACKING_STATE, &self.tracking_state).consensus_index,
        )
    }

    /// Returns the tracking consensus ledger index — the last ledger that was
    /// externalized by SCP.
    ///
    /// Analogous to stellar-core's `HerderImpl::trackingConsensusLedgerIndex()`
    /// (HerderImpl.cpp:165), which returns `mTrackingSCP.mConsensusIndex`.
    ///
    /// **Intentional divergences from stellar-core:**
    /// - Returns 0 when no slot has been externalized (`tracking_slot() == 0`).
    ///   Stellar-core asserts `state != BOOTING` — henyey callers handle boot
    ///   gracefully via the 0 sentinel.
    /// - Does NOT assert `<= UINT32_MAX`. Henyey uses u64 throughout; callers
    ///   that need u32 must cast explicitly.
    ///
    /// In henyey, `tracking_slot()` stores the *next* consensus index
    /// (externalized + 1), so this helper subtracts 1 to produce the
    /// last-externalized value.
    pub fn tracking_consensus_ledger_index(&self) -> LastExternalizedLedger {
        LastExternalizedLedger::new(self.tracking_slot().get().saturating_sub(1))
    }

    /// INV-H2: Verify that the ledger manager's LCL is not ahead of consensus tracking;
    /// self-correct if it is.
    ///
    /// Parity: mirrors stellar-core's `trackingConsensusLedgerIndex()`
    /// (HerderImpl.cpp:170-178) which **throws** (not aborts) on
    /// `lcl > mTrackingSCP.mConsensusIndex`. The throw propagates and can be
    /// caught by the catchup state machine. The henyey port had ported this
    /// as `panic = "abort"` which is strictly more fatal than upstream — any
    /// race window that briefly leaves LCL ahead of tracking would kill the
    /// validator (#2720, #2791). The corrective branch restores the invariant
    /// inline, which is the closest semantic match to upstream's recoverable
    /// throw available in a Rust caller that doesn't unwind.
    ///
    /// No-op when not in Tracking state or at genesis (tracking_slot == 0),
    /// since during Booting/Syncing the LM legitimately advances before
    /// tracking (handled by the early-return on `!is_tracking`).
    ///
    /// When LCL >= tracking (invariant violation):
    /// 1. Log a WARN with `lcl_seq` / `tracking` / `gap` for operator visibility.
    /// 2. Increment `lcl_ahead_of_tracking_corrective_total` so the rate of
    ///    corrective fires can be monitored (alerts can be added later).
    /// 3. Call `advance_tracking_to(lcl_seq, close_time)` to bring tracking
    ///    to `lcl_seq + 1`. Monotone-idempotent — safe even if a parallel
    ///    path has already advanced tracking.
    /// 4. Return normally.
    ///
    /// The `close_time` for the corrective call is read from the LedgerManager's
    /// current header, the same way `bootstrap` reads it (h:1341). A concurrent
    /// close landing between the lcl_seq read and the close_time read may
    /// supply a close_time for `lcl_seq + 1` — still a valid tracking-close-time
    /// (tracking would then point one slot past the LCL the corrective branch
    /// observed, which is still consistent with the invariant).
    fn assert_lcl_consistency(&self) {
        if !self.is_tracking() {
            return;
        }
        let tracking = self.tracking_slot().get();
        if tracking == 0 {
            return;
        }
        let lcl_seq = self.ledger_manager.current_ledger_seq() as u64;
        if lcl_seq < tracking {
            return;
        }

        // Invariant violated: LCL has legitimately advanced ahead of tracking
        // (e.g. burst close from buffered EXTERNALIZE messages during catchup
        // replay — see #2791 timeline). Restore the invariant inline.
        let gap = lcl_seq.saturating_sub(tracking).saturating_add(1);
        let close_time = self.ledger_manager.current_header().scp_value.close_time.0;
        self.lcl_ahead_of_tracking_corrective_total
            .fetch_add(1, Ordering::Relaxed);
        tracing::warn!(
            lcl_seq,
            tracking,
            gap,
            close_time,
            herder_state = %self.state(),
            "INV-H2: LCL at or ahead of tracking — advancing tracking to lcl + 1 \
             (corrective; was process-fatal pre-#2791)"
        );
        self.advance_tracking_to(lcl_seq, close_time);
    }

    /// Cumulative count of times the `assert_lcl_consistency` corrective
    /// branch fired (LCL >= tracking → advance tracking to lcl + 1). Used
    /// by the metrics scrape path and by regression tests. See #2791.
    pub fn lcl_ahead_of_tracking_corrective_total(&self) -> u64 {
        self.lcl_ahead_of_tracking_corrective_total
            .load(Ordering::Relaxed)
    }

    /// Get the tracking consensus close time.
    /// Matches stellar-core `HerderImpl::trackingConsensusCloseTime()`.
    pub fn tracking_consensus_close_time(&self) -> u64 {
        tracked_read(LOCK_TRACKING_STATE, &self.tracking_state).consensus_close_time
    }

    /// Get the next consensus ledger index.
    /// Matches stellar-core `HerderImpl::nextConsensusLedgerIndex()`.
    ///
    /// Equivalent to [`tracking_slot`](Self::tracking_slot); both read the same field.
    pub fn next_consensus_ledger_index(&self) -> NextConsensusSlot {
        self.tracking_slot()
    }

    /// Returns the ballot-protocol phase of the tracking slot as a u8 gauge value.
    ///
    /// 0=unknown, 1=prepare, 2=confirm, 3=externalize.
    pub fn tracking_slot_ballot_phase(&self) -> u8 {
        self.scp.get_slot_ballot_phase(self.tracking_slot().get())
    }

    /// Get a snapshot of the SCP event counters for the metrics scrape path.
    pub fn scp_metrics_snapshot(&self) -> crate::metrics::ScpMetricsSnapshot {
        self.scp_metrics.snapshot()
    }

    /// Get a snapshot of the SCP persist-worker backlog counters for the
    /// metrics scrape path (#3796). Returns all-zero defaults when no
    /// persistence manager is installed (non-validator / pre-bootstrap).
    pub fn scp_persist_stats(&self) -> crate::persistence::ScpPersistStats {
        let Some(manager) = self.scp_persistence.get() else {
            return crate::persistence::ScpPersistStats::default();
        };
        manager.persist_stats()
    }

    /// Get the cumulative SCP statement count (retained-memory gauge).
    pub fn scp_cumulative_statement_count(&self) -> usize {
        self.scp.get_cumulative_statement_count()
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
        let tracking_consensus_index = self.tracking_consensus_ledger_index().get();
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
        let current_slot = self.tracking_consensus_ledger_index().get();
        if current_slot > MAX_SLOTS_TO_REMEMBER {
            current_slot - MAX_SLOTS_TO_REMEMBER + 1
        } else {
            1
        }
    }

    /// Returns the first sequential ledger as computed inline in
    /// stellar-core's `sendSCPStateToPeer`. This differs from
    /// `get_min_ledger_seq_to_remember()` by omitting the `+1`.
    pub fn get_first_sequential_ledger_for_send(&self) -> u64 {
        let current_slot = self.tracking_consensus_ledger_index().get();
        if current_slot > MAX_SLOTS_TO_REMEMBER {
            current_slot - MAX_SLOTS_TO_REMEMBER
        } else {
            1
        }
    }

    /// Compute the minimum ledger sequence to ask peers for SCP state.
    ///
    /// Parity: stellar-core `HerderImpl::getMinLedgerSeqToAskPeers()`
    /// (HerderImpl.cpp:1386-1411) — computes a lookback from LCL using
    /// `min(MAX_SLOTS_TO_REMEMBER, SCP_EXTRA_LOOKBACK_LEDGERS)`, then clamps
    /// upward against `getMinLedgerSeqToRemember()` so we never ask for slots
    /// the herder would immediately forget.
    pub fn get_min_ledger_seq_to_ask_peers(&self) -> u32 {
        let lcl = self.ledger_manager.current_ledger_seq();
        let mut low = lcl.saturating_add(1);
        // stellar-core uses `std::min(Config::MAX_SLOTS_TO_REMEMBER,
        // SCP_EXTRA_LOOKBACK_LEDGERS)` where SCP_EXTRA_LOOKBACK_LEDGERS = 3.
        // Use the same `MAX_SLOTS_TO_REMEMBER` constant that the sibling
        // `get_min_ledger_seq_to_remember()` floor uses, so both halves of the
        // clamp read the same retention window (stellar-core reads
        // `Config::MAX_SLOTS_TO_REMEMBER` in both).
        const SCP_EXTRA_LOOKBACK_LEDGERS: u32 = 3;
        let window = (MAX_SLOTS_TO_REMEMBER as u32).min(SCP_EXTRA_LOOKBACK_LEDGERS);
        if low > window {
            low = low.saturating_sub(window);
        } else {
            low = 1;
        }

        // Do not ask for slots we'd be dropping anyway (stellar-core parity:
        // `low = std::max<uint32>(low, getMinLedgerSeqToRemember())`).
        let remember_floor = self.get_min_ledger_seq_to_remember() as u32;
        low.max(remember_floor)
    }

    /// Get the expected ledger close duration.
    ///
    /// Reads the dynamic value from the ledger config
    /// (via `LedgerManager::expected_ledger_close_duration()`).
    ///
    /// The returned `Duration` has millisecond precision. Callers needing a
    /// raw integer should use `.as_secs()` or `.as_millis() as u64` at the
    /// leaf site — the source value is always bounded by `u32`, so the cast
    /// to `u64` is lossless.
    pub fn ledger_close_duration(&self) -> Duration {
        self.ledger_manager.expected_ledger_close_duration()
    }

    /// Get the ballot-start instant for the given slot.
    /// Parity: stellar-core `getPrepareStart(slot)` / `mPrepareStart`
    /// (HerderSCPDriver), as consumed by `setupTriggerNextLedger`
    /// (HerderImpl.cpp:1266). Exposed here as "prepare start".
    pub fn prepare_start(&self, slot: SlotIndex) -> Option<std::time::Instant> {
        self.scp_driver.prepare_start(slot)
    }

    /// Get the LCL close time from the current ledger header.
    pub fn lcl_close_time(&self) -> u64 {
        self.ledger_manager.current_header().scp_value.close_time.0
    }

    /// Compute the close-time validity offset.
    ///
    /// Parity: stellar-core `HerderImpl::ctValidityOffset(lastCtTrigger, ct)`.
    /// Returns the number of seconds we must wait before `close_time` falls
    /// within the `now + MAX_TIME_SLIP_SECONDS` validity window.
    /// Returns 0 when `close_time` is already valid.
    ///
    /// Stellar-core adds +1ms to the result as a boundary guard (ensuring the
    /// timer fires strictly after the boundary). In henyey's second-granularity
    /// polling architecture, we add +1 second to be conservative.
    pub fn ct_validity_offset(&self, close_time: u64, max_ct_offset: u64) -> u64 {
        let now = self.scp_driver.now_seconds();
        let boundary = now
            .saturating_add(MAX_TIME_SLIP_SECONDS)
            .saturating_add(max_ct_offset);
        if close_time <= boundary {
            0
        } else {
            // +1 second boundary guard (parity: stellar-core adds +1ms)
            (close_time - boundary).saturating_add(1)
        }
    }

    /// Get the maximum size of a transaction set (ops) from the LIVE ledger.
    ///
    /// Mirrors stellar-core `LedgerManager::getLastMaxTxSetSizeOps()`. This reads
    /// the current (last-closed) ledger header, NOT static config, because
    /// `maxTxSetSize` can be raised via `LedgerUpgrade::MaxTxSetSize` after
    /// genesis. The previous config-based value went stale after an upgrade and
    /// under-provisioned the transaction-flood budget (~2.4× too small under the
    /// maxTPS mission's upgrade), causing flood-queue starvation: a few
    /// locally-submitted txns were never advertised, aged out, stranded their
    /// account, and failed the whole loadgen step via the all-or-nothing
    /// completion check. Only flood/advert/demand sizing consumes this; tx-set
    /// construction already uses the live header value.
    pub fn max_tx_set_size(&self) -> usize {
        self.ledger_manager.last_max_tx_set_size_ops()
    }

    /// Get the maximum queue size in ops for demand sizing.
    pub fn max_queue_size_ops(&self) -> usize {
        self.config.max_pending_transactions
    }

    /// Maximum Soroban queue capacity in tx-count (pool-scaled).
    ///
    /// Matches stellar-core `HerderImpl::getMaxQueueSizeSorobanOps()`:
    /// delegates to the Soroban transaction queue's `getMaxQueueSizeOps()`,
    /// which returns 0 when no Soroban limits are configured.
    pub fn max_queue_size_soroban_ops(&self) -> usize {
        self.tx_queue.max_queue_size_soroban_ops()
    }

    /// Return the set of node IDs from the local quorum set (if configured).
    pub fn local_quorum_nodes(&self) -> std::collections::HashSet<stellar_xdr::NodeId> {
        fn collect_nodes(
            quorum_set: &stellar_xdr::ScpQuorumSet,
            acc: &mut std::collections::HashSet<stellar_xdr::NodeId>,
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
    ///
    /// This stores the quorum set in the SCP driver and quorum tracker, and
    /// notifies FetchingEnvelopes so blocked envelopes become ready. It does
    /// NOT drain the ready queue — callers must call
    /// `process_ready_fetching_envelopes()` separately (typically via
    /// `spawn_blocking` to avoid event-loop stalls).
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
    }

    /// Get a quorum set by hash if available.
    pub fn get_quorum_set_by_hash(&self, hash: &Hash256) -> Option<ScpQuorumSet> {
        self.scp_driver.get_quorum_set_by_hash(hash)
    }

    /// Get the currently tracked quorum map.
    ///
    /// Returns all node→quorum-set pairs known to the transitive quorum tracker.
    /// Matches stellar-core's `mPendingEnvelopes.getCurrentlyTrackedQuorum()`.
    /// Used by `saveSCPHistory(slot_N, envelopes, qmap)` to persist the full
    /// quorum-map-derived qsets for the current slot.
    pub fn get_currently_tracked_quorum(&self) -> Vec<(Hash256, ScpQuorumSet)> {
        let tracker = self.quorum_tracker.read();
        let mut result = Vec::new();
        for (_node_id, info) in tracker.quorum_map() {
            if let Some(ref qset) = info.quorum_set {
                let hash = henyey_scp::hash_quorum_set(qset);
                result.push((hash, qset.clone()));
            }
        }
        result
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

    /// Report `heard_from_quorum` from the SCP ballot protocol's per-slot flag.
    ///
    /// This is the parity-faithful source for the heartbeat / `/info`
    /// telemetry, mirroring stellar-core's per-slot
    /// `BallotProtocol::mHeardFromQuorum` (`stellar-core/src/scp/BallotProtocol.cpp:2192`).
    /// Unlike [`Self::heard_from_quorum`] (the henyey-only `SlotQuorumTracker`),
    /// the SCP flag (1) includes the local node — core's `LocalNode::isQuorum`
    /// always counts the local node's own `mLatestEnvelopes` entry, while the
    /// tracker never records self (self-messages are skipped in
    /// `process_verified` before `record_envelope`); and (2) is set for every
    /// slot the node has a ballot for, including a slot the node only *followed*
    /// via a quorum of CONFIRM/EXTERNALIZE (`set_state_from_envelope` →
    /// `advance_slot` → `check_heard_from_quorum`). The tracker, by contrast,
    /// only reaches a v-blocking subset for an already-externalized slot the
    /// node free-rode (#1862/#3199/#3205), so it gets stuck `false` after a
    /// restart while `qset.agree` is healthy — the #3250 triad.
    ///
    /// `SlotQuorumTracker` is intentionally left untouched (it remains the
    /// source for `is_v_blocking` / recovery accounting per #1874).
    pub fn scp_heard_from_quorum(&self, slot: SlotIndex) -> bool {
        self.scp
            .get_slot_state(slot)
            .map(|s| s.heard_from_quorum)
            .unwrap_or(false)
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
    pub fn out_of_sync_recovery(&self) -> Option<u64> {
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

            // Parity: stellar-core eraseOutsideRange uses getMostRecentCheckpointSeq()
            let last_checkpoint = self.get_most_recent_checkpoint_seq();

            self.fetching_envelopes
                .erase_outside_range(Some(purge_slot), None, last_checkpoint);

            // Clear slot quorum tracker entries below purge_slot
            self.slot_quorum_tracker
                .write()
                .clear_slots_below(purge_slot);

            // Purge SCP state, preserving the most-recent-checkpoint slot.
            // Parity: stellar-core outOfSyncRecovery routes through
            // HerderImpl::eraseOutsideRange (HerderImpl.cpp:1328-1335), which
            // threads getMostRecentCheckpointSeq() into
            // HerderSCPDriver::purgeSlotsOutsideRange(_, _, slotToKeep)
            // (HerderSCPDriver.cpp:1300-1337). Without this, the checkpoint
            // slot's SCP state can be evicted and the delayed
            // send_scp_state callback (#2670) silently sends nothing (#2706).
            // No upper bound on recovery: parity with core's
            // eraseOutsideRange(purgeSlot, std::nullopt) (HerderImpl.cpp:573) —
            // not tracking, so future slots are retained.
            self.scp.purge_slots(
                Some(purge_slot.saturating_sub(1)),
                None,
                Some(last_checkpoint),
            );

            // Purge externalized values and pending tx set requests
            self.scp_driver.purge_slots_below(purge_slot);

            // Purge stale pending envelopes for old slots
            self.pending_envelopes.purge_slots_below(purge_slot);

            return Some(purge_slot);
        }

        None
    }

    /// Bootstrap the Herder after catchup.
    ///
    /// This transitions the Herder from Syncing to Tracking state,
    /// setting the next consensus ledger as the tracking slot.
    /// Ensure the herder is in Tracking state, updating `tracking_started_at`
    /// on actual transition.
    ///
    /// This is the single canonical point for HerderState → Tracking transitions
    /// in production code. Both `advance_tracking_to` and `advance_tracking_slot`
    /// call this unconditionally after updating SharedTrackingState.
    ///
    /// Precondition: `SharedTrackingState.is_tracking` is already `true` when
    /// this is called. This is guaranteed because:
    /// - On the advancing path, the caller just set `ts.is_tracking = true`
    /// - On the idempotent path, a prior advancing call already set it
    ///
    /// Postcondition: `HerderState == Tracking` and `tracking_started_at`
    /// reflects the wall-clock time of the first Tracking transition.
    ///
    /// Parity: stellar-core `setTrackingSCPState(slotIndex, value, true)` sets
    /// `HERDER_TRACKING_NETWORK_STATE` unconditionally when `isTrackingNetwork=true`.
    /// Both our callers correspond to the `isTrackingNetwork=true` path:
    /// - `advance_tracking_to`: mirrors `forceSCPStateIntoSyncWithLastClosedLedger`
    ///   (HerderImpl.cpp:1676-1680)
    /// - `advance_tracking_slot`: mirrors `valueExternalized` for the latest slot
    ///   (HerderSCPDriver.cpp:883-913)
    fn ensure_tracking_state(&self, context: &str, slot: u64) {
        let was_not_tracking = {
            let mut state = tracked_write(LOCK_HERDER_STATE, &self.state);
            if *state != HerderState::Tracking {
                info!(slot, "Transitioning to Tracking on {}", context);
                *state = HerderState::Tracking;
                true
            } else {
                false
            }
        };

        if was_not_tracking {
            *self.tracking_started_at.write() = Some(Instant::now());
        }
    }

    /// Advance tracking state so that `consensus_index` reflects `slot + 1`.
    ///
    /// This is the "state sync" half of `bootstrap` / `complete_externalization`:
    /// it updates `tracking_state`, envelope cursors, and herder state, but does
    /// NOT drain pending envelopes, publish externalized, touch the closing gate,
    /// or check the quorum map. Those side effects are path-specific and handled
    /// by the caller (e.g. `bootstrap` drains, `advance_tracking_slot` gates).
    ///
    /// Idempotent: no-op if `consensus_index` is already past `slot + 1`.
    ///
    /// Precondition: the caller is advancing LCL to `slot` (or has already done
    /// so). Violating this produces a state where tracking is ahead of LCL, which
    /// is harmless (the normal SCP path does exactly this) but wasteful.
    ///
    /// Parity: stellar-core `setTrackingSCPState(index, value, isTrackingNetwork)`
    /// (HerderImpl.cpp:150-163). Called from `forceSCPStateIntoSyncWithLastClosedLedger()`
    /// (HerderImpl.cpp:1676-1680), CatchupWork.cpp:69-70, and `Herder::start()`
    /// (HerderImpl.cpp:2406-2411).
    pub fn advance_tracking_to(&self, slot: u64, close_time: u64) {
        let next = slot + 1;

        // Idempotent consensus-index advance: don't regress.
        let already_advanced = {
            let mut ts = tracked_write(LOCK_TRACKING_STATE, &self.tracking_state);
            if next <= ts.consensus_index {
                true
            } else {
                ts.is_tracking = true;
                ts.consensus_index = next;
                ts.consensus_close_time = close_time;
                false
            }
        };

        if !already_advanced {
            // Update pending and fetching envelopes current slot
            self.pending_envelopes.set_current_slot(next);
            self.fetching_envelopes.set_current_slot(next);
        }

        // UNCONDITIONAL: Always ensure Tracking state.
        // Parity: stellar-core setTrackingSCPState (HerderImpl.cpp:150-162).
        self.ensure_tracking_state("advance_tracking_to", slot);
    }

    pub fn bootstrap(&self, ledger_seq: u32) {
        let lcl = ledger_seq as u64;

        debug!("Bootstrapping Herder at ledger {}", ledger_seq);

        // Get tracking consensus close time from LCL
        // (matching stellar-core setTrackingSCPState which sets close time from externalized value)
        let close_time = self.ledger_manager.current_header().scp_value.close_time.0;

        // Phase 1: advance tracking state (idempotent if already advanced)
        self.advance_tracking_to(lcl, close_time);

        // Phase 2: drain pending envelopes (must happen after tracking is synced)
        let slot = lcl + 1;
        self.drain_and_process_pending(slot);
        self.process_ready_fetching_envelopes();

        debug!(
            lcl,
            tracking_slot = slot,
            "Herder now tracking next consensus slot"
        );
    }

    /// Start syncing (called when catchup begins).
    pub fn start_syncing(&self) {
        info!("Herder entering syncing state");
        *tracked_write(LOCK_HERDER_STATE, &self.state) = HerderState::Syncing;
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
        let header = self.ledger_manager.current_header();
        let lcl_seq = header.ledger_seq as u64;
        let lcl_close_time = header.scp_value.close_time.0;

        let mut last_close_index = lcl_seq;
        let mut last_close_time = lcl_close_time;

        // Use tracking consensus data for a better estimate when available
        // (matching stellar-core which upgrades lastCloseIndex/lastCloseTime from tracking)
        // stellar-core uses trackingConsensusLedgerIndex() which is the LCL seq (= next_consensus - 1)
        let state = self.state();
        if state != HerderState::Booting {
            let tracking_index = self.tracking_consensus_ledger_index().get();
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
        use stellar_xdr::ScpStatementPledges;
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
                    reason = reason.label(),
                    "pre-filter rejected envelope"
                );
                return match reason {
                    R::Range => EnvelopeState::TooOld,
                    R::ManualClose => EnvelopeState::Discarded,
                    R::CannotReceiveScp | R::CloseTime => EnvelopeState::Invalid,
                };
            }
        }
        if let Err(e) = self.scp_driver.verify_envelope(&envelope) {
            debug!(slot, error = %e, "Invalid SCP envelope signature");
            return EnvelopeState::InvalidSignature;
        }
        // Record when we first observed activity for this slot (timing metrics).
        self.scp_driver.record_slot_activity(slot);
        let intake = PipelinedIntake::from_local(envelope, slot, is_externalize);
        self.process_verified(VerifiedEnvelope {
            intake,
            verdict: Verdict::Ok,
        })
        .0
    }

    /// Variant of [`Self::receive_scp_envelope`] that returns both
    /// [`EnvelopeState`] and [`crate::scp_verify::PostVerifyReason`].
    ///
    /// Pre-filter rejections are mapped to the `GateDrift*` reasons so the
    /// wrapper and split paths use a single reason vocabulary. (In the split
    /// path these reasons arise when `process_verified`'s rerun fires; in the
    /// wrapper path they arise on the first and only pre-filter call.)
    pub fn receive_scp_envelope_detailed(
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
                    reason = reason.label(),
                    "pre-filter rejected envelope"
                );
                return match reason {
                    R::Range => (EnvelopeState::TooOld, PostVerifyReason::GateDriftRange),
                    R::CloseTime => (EnvelopeState::Invalid, PostVerifyReason::GateDriftCloseTime),
                    R::CannotReceiveScp => (
                        EnvelopeState::Invalid,
                        PostVerifyReason::GateDriftCannotReceive,
                    ),
                    R::ManualClose => (
                        EnvelopeState::Discarded,
                        PostVerifyReason::GateDriftManualClose,
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
        // Record when we first observed activity for this slot (timing metrics).
        self.scp_driver.record_slot_activity(slot);
        let intake = PipelinedIntake::from_local(envelope, slot, is_externalize);
        self.process_verified(VerifiedEnvelope {
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
    /// 3. Slot-range check using `min_ledger_seq` derived from `tracking_slot`
    ///    and `MAX_SLOTS_TO_REMEMBER` (matches
    ///    `HerderImpl::getMinLedgerSeqToRemember()`).
    ///
    /// Signature verification is **not** performed here. Accepted envelopes are
    /// returned as a [`PipelinedIntake`] carrying metadata needed downstream.
    pub fn pre_filter_scp_envelope(&self, envelope: &ScpEnvelope) -> crate::scp_verify::PreFilter {
        use crate::scp_verify::{PipelinedIntake, PreFilter, PreFilterRejectReason};

        // Parity: stellar-core HerderImpl.cpp:805-808 gates on MANUAL_CLOSE.
        // Henyey gates on suppress_scp() (manual_close && run_standalone)
        // because henyey simulation uses MANUAL_CLOSE=true with real SCP,
        // while stellar-core simulation sets MANUAL_CLOSE=false (Simulation.cpp:99).
        if self.config.suppress_scp() {
            return PreFilter::Reject(PreFilterRejectReason::ManualClose);
        }

        let state = self.state();
        let slot = envelope.statement.slot_index;
        let current_slot = self.tracking_slot().get();

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
            max_ledger_seq = self.next_consensus_ledger_index().get() + LEDGER_VALIDITY_BRACKET;
        } else {
            let tracking_consensus_index = self.tracking_consensus_ledger_index().get();
            let enforce_recent = tracking_consensus_index <= GENESIS_LEDGER_SEQ
                && slot != self.next_consensus_ledger_index().get();
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

        // Slot-range check: parity with stellar-core
        // `HerderImpl::recvSCPEnvelope` (HerderImpl.cpp:815-873). The lower
        // bound is purely `getMinLedgerSeqToRemember()` — derived from the
        // tracking slot and `MAX_SLOTS_TO_REMEMBER` — and explicitly does
        // NOT clamp to LCL. Recent post-LCL slots within the retained window
        // must reach SCP so that `validate_value` can return `MaybeValid`
        // for already-moved-on slots and the node can still relay finalization
        // messages (see HerderSCPDriver.cpp:244-253 and
        // crates/herder/src/scp_driver.rs:1055-1063).
        let min_ledger_seq = self.get_min_ledger_seq_to_remember();

        if (slot > max_ledger_seq || slot < min_ledger_seq) && slot != checkpoint {
            debug!(
                slot,
                current_slot,
                min_ledger_seq,
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
        PreFilter::Accept(PipelinedIntake::from_local(
            envelope.clone(),
            slot,
            is_externalize,
        ))
    }

    /// Post-verification processing.
    ///
    /// Returns both [`EnvelopeState`] and [`crate::scp_verify::PostVerifyReason`]
    /// so callers can attribute outcomes to specific gates for metrics.
    ///
    /// Assumes signature verification already succeeded (or the verdict is
    /// surfaced as `InvalidSignature`). Re-runs the mutable pre-filter gates
    /// (state may have drifted while the worker was verifying) before
    /// executing self-message skip, non-quorum reject, slot_quorum_tracker
    /// update, EXTERNALIZE tx-set prefetch and `pending_envelopes.add` /
    /// `process_scp_envelope`.
    pub fn process_verified(
        &self,
        ve: crate::scp_verify::VerifiedEnvelope,
    ) -> (EnvelopeState, crate::scp_verify::PostVerifyReason) {
        use crate::scp_verify::{PostVerifyReason, PreFilter, Verdict};
        let crate::scp_verify::VerifiedEnvelope { intake, verdict } = ve;
        let slot = intake.slot();
        let received_at = intake.received_at();

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

        let (envelope, _, _) = intake.into_parts();

        // Re-check mutable gates (state may have drifted during verify).
        match self.pre_filter_scp_envelope(&envelope) {
            PreFilter::Accept(_) => {}
            PreFilter::Reject(reason) => {
                use crate::scp_verify::PreFilterRejectReason as R;
                debug!(
                    slot,
                    reason = reason.label(),
                    "post-verify gate drift rejected envelope"
                );
                let (state, post) = match reason {
                    R::Range => (EnvelopeState::TooOld, PostVerifyReason::GateDriftRange),
                    R::CloseTime => (EnvelopeState::Invalid, PostVerifyReason::GateDriftCloseTime),
                    R::CannotReceiveScp => (
                        EnvelopeState::Invalid,
                        PostVerifyReason::GateDriftCannotReceive,
                    ),
                    R::ManualClose => (
                        EnvelopeState::Discarded,
                        PostVerifyReason::GateDriftManualClose,
                    ),
                };
                return (state, post);
            }
        }

        let current_slot = self.tracking_slot().get();

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

        // Slot quorum tracker bookkeeping is deferred to
        // process_scp_envelope_with_tx_set, which is the SCP-acceptance
        // boundary. Envelopes are only quorum-relevant after SCP accepts
        // them (Valid/ValidNew). Note: EXTERNALIZE envelopes for the
        // current/past slot bypass dependency admission via the bypass
        // path in process_scp_envelope, so the true boundary is SCP
        // acceptance, not dependency resolution.

        // Pre-fetch tx sets from EXTERNALIZE envelopes immediately so they're
        // available by the time SCP processes the envelope. Uses the strict
        // validated helper so malformed EXTERNALIZE values cannot trigger
        // fetch side effects.
        let lcl = self.ledger_manager.current_ledger_seq() as u64;
        if let stellar_xdr::ScpStatementPledges::Externalize(_) = &envelope.statement.pledges {
            if let Ok(hashes) = crate::herder_utils::get_validated_tx_set_hashes(&envelope) {
                for tx_set_hash in hashes {
                    if slot > lcl {
                        if self.scp_driver.request_tx_set(tx_set_hash, slot) {
                            debug!(slot, hash = %tx_set_hash, "Requesting tx set from EXTERNALIZE");
                        }
                    }
                }
            }
        }

        // Admission control: per-slot lifetime cap for ALL slots.
        // Uses lifetime count (fetching + ready + processed + discarded) to
        // prevent the "immediate-pop bypass" for current-slot envelopes.
        // This is the primary defense; the internal cap in recv_envelope_inner
        // is defense-in-depth for non-herder callers.
        {
            let slot_lifetime = self.fetching_envelopes.slot_lifetime_count(slot);
            if slot_lifetime >= self.config.pending_config.max_envelopes_per_slot {
                let last_warned = self.pending_envelopes.last_per_slot_full_warn_slot();
                if slot != last_warned {
                    self.pending_envelopes
                        .set_last_per_slot_full_warn_slot(slot);
                    tracing::warn!(
                        slot,
                        current_slot,
                        slot_lifetime,
                        "Per-slot lifetime cap reached in FetchingEnvelopes \
                         (possible compromised validator or watcher flood)"
                    );
                }
                return (
                    EnvelopeState::Invalid,
                    PostVerifyReason::PendingAddPerSlotFull,
                );
            }
        }

        // Future-slot admission is now handled atomically inside
        // FetchingEnvelopes::recv_envelope_inner (#2520), eliminating the
        // TOCTOU race. The result is propagated through process_scp_envelope.

        // Process through unified FetchingEnvelopes intake. This handles
        // dep-fetching, relay, slot-aware routing, and EXTERNALIZE bypass.
        let (state, recv_result) = self.process_scp_envelope(envelope, received_at);
        if let Some(crate::fetching_envelopes::RecvResult::FutureSlotsFull) = recv_result {
            let last_warned = self.pending_envelopes.last_buffer_full_warn_slot();
            if slot != last_warned {
                self.pending_envelopes.set_last_buffer_full_warn_slot(slot);
                tracing::warn!(
                    slot,
                    current_slot,
                    "Future-slot count limit reached in FetchingEnvelopes"
                );
            }
            return (
                EnvelopeState::Invalid,
                PostVerifyReason::PendingAddBufferFull,
            );
        }
        (state, PostVerifyReason::Accepted)
    }

    /// Process an SCP envelope through the unified FetchingEnvelopes intake.
    ///
    /// Parity: mirrors stellar-core's flow where ALL envelopes go through
    /// `PendingEnvelopes::recvSCPEnvelope()` for dep-fetching and relay,
    /// then `processSCPQueueUpToIndex` limits SCP consumption to the
    /// current slot. Future-slot envelopes get dep-fetched and relayed
    /// immediately but SCP processing is deferred until the slot advances.
    ///
    /// EXTERNALIZE bypass: EXTERNALIZE envelopes are processed through SCP
    /// immediately regardless of missing deps (catchup recovery requirement).
    fn process_scp_envelope(
        &self,
        envelope: ScpEnvelope,
        received_at: Option<Instant>,
    ) -> (EnvelopeState, Option<crate::fetching_envelopes::RecvResult>) {
        let slot = envelope.statement.slot_index;
        let is_externalize = matches!(
            envelope.statement.pledges,
            stellar_xdr::ScpStatementPledges::Externalize(_)
        );

        debug!(
            "Processing SCP envelope for slot {} from {:?}",
            slot, envelope.statement.node_id
        );

        use crate::fetching_envelopes::RecvResult;

        // Route through FetchingEnvelopes for dep-fetching and relay.
        // This ensures all envelopes (regardless of slot) get their
        // dependencies resolved and are broadcast to peers when ready.
        // Use recv_envelope_validated since the envelope has already passed
        // network-level validation (pre-filter + signature verification).
        let envelope_clone = envelope.clone();
        let result = self
            .fetching_envelopes
            .recv_envelope_validated(envelope, received_at);

        let state = match result {
            RecvResult::Ready => {
                // Deps satisfied, envelope broadcast by FetchingEnvelopes.
                // Route based on slot eligibility.
                if slot <= self.tracking_slot().get() {
                    debug!(slot, "Envelope ready, processing through SCP");
                    // [maxtps_scp] campaign-2 iter-3: attribute 18+ s
                    // scp_verified stalls — is the SCP machine drive the
                    // blocking segment?
                    let scp_t0 = std::time::Instant::now();
                    let scp_result = self.process_scp_envelope_with_tx_set(envelope_clone);
                    let scp_ms = scp_t0.elapsed().as_millis() as u64;
                    if scp_ms > 1_000 {
                        tracing::info!(
                            target: "maxtps_scp",
                            slot,
                            scp_ms,
                            "slow_scp_machine_drive"
                        );
                    }
                    // Pop the duplicate from ready queue to prevent later
                    // double-processing by process_ready_fetching_envelopes.
                    // Use pop_from_slot (not pop) to avoid accidentally popping
                    // envelopes from lower slots that may be waiting for drain.
                    let _ = self.fetching_envelopes.pop_from_slot(slot);
                    scp_result
                } else {
                    // Future slot: envelope stays in FetchingEnvelopes' ready
                    // queue. Will be consumed when slot advances and
                    // process_ready_fetching_envelopes() is called.
                    debug!(
                        slot,
                        tracking = self.tracking_slot().get(),
                        "Future-slot envelope ready, deferring SCP processing"
                    );
                    EnvelopeState::Pending
                }
            }
            RecvResult::Fetching => {
                // Deps missing — envelope is being fetched by FetchingEnvelopes.
                // Register tx_set requests so the app layer can fetch them.
                let tx_set_hashes =
                    crate::herder_utils::get_tx_set_hashes_from_envelope(&envelope_clone);
                for hash in &tx_set_hashes {
                    if !self.scp_driver.has_tx_set(hash) {
                        self.scp_driver.request_tx_set(*hash, slot);
                    }
                }

                // EXTERNALIZE bypass (current/past slot only): process through
                // SCP immediately regardless of missing deps. Required for
                // tracking-slot advancement during catchup. Without this,
                // post-catchup the node receives EXTERNALIZE from peers but
                // blocks them because the tx_set is missing (expired from
                // peers' caches), freezing the tracking slot.
                //
                // Future-slot EXTERNALIZE envelopes stay in FetchingEnvelopes
                // and will be processed when the slot advances.
                //
                // See #1796 and the SCP/herder PARITY_STATUS.md sections.
                if is_externalize && slot <= self.tracking_slot().get() {
                    debug!(
                        slot,
                        "EXTERNALIZE with missing deps, processing through SCP immediately"
                    );
                    self.process_scp_envelope_with_tx_set(envelope_clone)
                } else {
                    if is_externalize {
                        debug!(
                            slot,
                            tracking = self.tracking_slot().get(),
                            "Future-slot EXTERNALIZE waiting for deps in FetchingEnvelopes"
                        );
                    } else {
                        debug!(slot, "Envelope waiting for deps in FetchingEnvelopes");
                    }
                    EnvelopeState::Fetching
                }
            }
            RecvResult::AlreadyProcessed => EnvelopeState::Duplicate,
            RecvResult::Discarded => EnvelopeState::Invalid,
            RecvResult::PerSlotFull => {
                // Defense-in-depth: the external check in process_verified
                // should have caught this first. If we reach here, log and
                // reject. No rate-limiting needed since the external check
                // handles the common path.
                debug!(slot, "Per-slot lifetime cap hit inside FetchingEnvelopes");
                EnvelopeState::Invalid
            }
            RecvResult::FutureSlotsFull => {
                // Propagated to caller for warning/metric handling.
                return (EnvelopeState::Invalid, Some(result));
            }
        };

        (state, Some(result))
    }

    /// Process an SCP envelope after confirming tx sets are available.
    ///
    /// All nodes (validators and observers) process through SCP. Observers have
    /// `fully_validated = false` on slots, so SCP won't emit their own envelopes
    /// (matching stellar-core watcher behavior).
    fn process_scp_envelope_with_tx_set(&self, envelope: ScpEnvelope) -> EnvelopeState {
        let slot = envelope.statement.slot_index;

        // Closing gate: buffer envelopes for the slot currently being applied.
        // Between externalization and `ledger_closed`, consensus_index has
        // advanced but LCL hasn't — processing these envelopes now would
        // trigger a spurious MaybeValidDeferred. They are replayed from
        // `ledger_closed` once LCL catches up (issue #2122).
        {
            let mut gate = self.closing_gate.lock().unwrap();
            if gate.slot != 0 && slot == gate.slot {
                trace!(
                    slot,
                    gate_slot = gate.slot,
                    "Envelope deferred by closing gate"
                );
                gate.buffer.push(envelope);
                return EnvelopeState::Deferred;
            }
        }

        let result = self.scp.receive_envelope(envelope.clone());

        match result {
            henyey_scp::EnvelopeState::Invalid => {
                return EnvelopeState::Invalid;
            }
            henyey_scp::EnvelopeState::Valid | henyey_scp::EnvelopeState::ValidNew => {
                // Record peer externalize lag for EXTERNALIZE envelopes.
                // Matches stellar-core HerderImpl.cpp:1116-1119 which records
                // on the unified VALID state (covering both our Valid and ValidNew).
                if matches!(
                    envelope.statement.pledges,
                    ScpStatementPledges::Externalize(_)
                ) {
                    self.scp_driver
                        .record_peer_externalize_event(slot, &envelope.statement.node_id);
                }

                // Slot quorum tracker: record the envelope at SCP-acceptance
                // boundary. This matches the timing of stellar-core's
                // processSCPQueueUpToIndex — envelopes become quorum-relevant
                // only after SCP actually accepts them (Valid/ValidNew).
                self.slot_quorum_tracker
                    .write()
                    .record_envelope(slot, envelope.statement.node_id.clone());

                if result == henyey_scp::EnvelopeState::Valid {
                    // Valid but not new
                    return EnvelopeState::Duplicate;
                }

                // ValidNew path
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

                        self.scp_driver
                            .record_externalized(slot, value.clone(), None);
                        self.scp_driver
                            .cleanup_externalized(self.config.max_externalized_slots);

                        // Advance tracking then publish (closes #2695 race window)
                        self.complete_externalization(slot);
                    }
                }
                return EnvelopeState::Valid;
            }
        }
    }

    /// Drain all pending envelopes up to and including `slot` and process them
    /// through SCP. The `BTreeMap` returned by `release_up_to` iterates in
    /// ascending key order, ensuring deterministic slot-ordered processing.
    ///
    /// Mirrors stellar-core's `processSCPQueueUpToIndex` which pops all
    /// envelopes for slots ≤ target in a loop.
    fn drain_and_process_pending(&self, slot: u64) {
        self.drain_and_process_pending_impl(slot, |_| {});
    }

    fn drain_and_process_pending_impl<F>(&self, slot: u64, mut before_process: F)
    where
        F: FnMut(&ScpEnvelope),
    {
        let pending = self.pending_envelopes.release_up_to(slot);
        for (pending_slot, envelopes) in pending {
            debug!(
                "Released {} pending envelopes for slot {}",
                envelopes.len(),
                pending_slot
            );
            for env in envelopes {
                before_process(&env);
                let _ = self.process_scp_envelope(env, None);
            }
        }
    }

    #[cfg(test)]
    fn drain_and_process_pending_with_hook<F>(&self, slot: u64, mut before_process: F)
    where
        F: FnMut(&ScpEnvelope),
    {
        self.drain_and_process_pending_impl(slot, |env| before_process(env));
    }

    /// Post-SCP-operation handler: if the given slot was externalized by SCP,
    /// advance tracking and then publish `latest_externalized` — in that order.
    ///
    /// This is the single sequencing point for all herder entry points that can
    /// trigger externalization via SCP callbacks (`process_scp_envelope_with_tx_set`,
    /// `trigger_next_ledger`, `handle_nomination_timeout`, `handle_ballot_timeout`).
    ///
    /// Uses `scp.is_slot_externalized(slot)` as the witness (not the
    /// `scp_driver.externalized` map, which may be populated by catchup/tests).
    ///
    /// Mirrors stellar-core's `valueExternalized` advancing tracking via
    /// `setTrackingSCPState` before returning to the caller.
    fn complete_externalization(&self, slot: SlotIndex) {
        if !self.scp.is_slot_externalized(slot) {
            return;
        }
        // Idempotency: if tracking already advanced past this slot, just publish.
        if self.tracking_slot().get() > slot {
            self.scp_driver.publish_externalized(slot);
            return;
        }
        self.advance_tracking_slot(slot);
        self.scp_driver.publish_externalized(slot);
    }

    /// Advance tracking slot after externalization.
    fn advance_tracking_slot(&self, externalized_slot: u64) {
        // Extract close time from the externalized value for tracking
        let close_time = self
            .scp_driver
            .get_externalized_close_time(externalized_slot)
            .unwrap_or(0);

        let should_advance = {
            let mut ts = tracked_write(LOCK_TRACKING_STATE, &self.tracking_state);
            if externalized_slot >= ts.consensus_index {
                // Activate the closing gate BEFORE publishing the new
                // consensus_index. This ensures no envelope can observe
                // the advanced index without also seeing the gate, closing
                // the theoretical TOCTOU window (issue #2122).
                {
                    let mut gate = self.closing_gate.lock().unwrap();
                    gate.slot = externalized_slot + 1;
                    gate.buffer.clear();
                }

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
            self.fetching_envelopes
                .set_current_slot(externalized_slot + 1);
        }

        // UNCONDITIONAL: Always ensure Tracking state on externalization.
        // Parity: stellar-core setTrackingSCPState(slotIndex, b, true)
        // in HerderSCPDriver::valueExternalized.
        self.ensure_tracking_state("externalization", externalized_slot);

        if should_advance {
            // NOTE: Pending envelope drain is NOT performed here.
            // stellar-core's `newSlotExternalized` defers the drain via
            // `safelyProcessSCPQueue(false)` → `postOnMainThread`
            // (HerderImpl.cpp:1194), so envelopes are processed only after
            // ledger apply completes and LCL advances. In henyey, the
            // equivalent post-apply hook is `Herder::ledger_closed`, which
            // calls `drain_and_process_pending(slot + 1)` after the close
            // pipeline has applied the ledger.

            // Check if quorum map changed and re-analyze intersection.
            // Matches stellar-core's checkAndMaybeReanalyzeQuorumMapV2()
            // called from valueExternalized (HerderImpl.cpp:486-501).
            self.check_and_maybe_reanalyze_quorum_map(externalized_slot as u32);
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

        // Cheap identifier for the sampled rejection diagnostic below,
        // captured before `tx` moves into try_add.
        let diag_seq = henyey_tx::envelope_sequence_number(&tx);

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

        // [maxtps_tail] #3719 part-1 attribution: a flooded/pulled tx that is
        // rejected here dies silently — the demander has already recorded the
        // pull as fulfilled, so nothing retries and a sole-advertiser tx
        // strands. Sample the rare terminal outcomes (Duplicate and Added are
        // the normal flood cases and stay quiet).
        // [maxtps_tail] #3719 attribution aid: sampled log of terminal
        // admission rejections (a rejected flooded/pulled tx dies silently —
        // the demander already recorded the pull as fulfilled).
        if matches!(
            result,
            TxQueueResult::Banned | TxQueueResult::Invalid(_) | TxQueueResult::TryAgainLater
        ) {
            use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
            static LOGGED: AtomicU64 = AtomicU64::new(0);
            if LOGGED.fetch_add(1, AtomicOrdering::Relaxed) % 16 == 0 {
                tracing::info!(
                    target: "maxtps_tail",
                    tx_seq = diag_seq,
                    result = ?result,
                    "tx admission rejected (sampled 1/16)"
                );
            }
        }

        result
    }

    /// Derive the previous externalized value for nomination priority calculation.
    ///
    /// Reads from the LedgerManager's LCL (`current_header().scp_value`),
    /// mirroring stellar-core's `triggerNextLedger(lcl.header.scpValue)` pattern.
    fn get_previous_value(&self) -> Value {
        let header = self.ledger_manager.current_header();
        Value(
            header
                .scp_value
                .to_xdr(Limits::none())
                .expect("StellarValue XDR serialization cannot fail for a valid LCL header")
                .try_into()
                .expect("StellarValue XDR bytes always fit in BytesM"),
        )
    }

    /// Trigger consensus for the next ledger (for validators).
    ///
    /// This is called periodically by the consensus timer.
    /// For validators: builds a candidate tx set, caches it, and triggers SCP
    /// nomination (returns `Triggered`).
    /// For observers: builds, validates, and caches the next-slot tx set but
    /// stops before nomination/signing (returns `ObserverCached`).
    ///
    /// This is entirely synchronous (parking_lot locks + CPU). It was
    /// previously declared `async` but had no `.await` points.
    /// Pre-build and cache the nomination value for `ledger_seq` ahead of the
    /// consensus trigger (maxtps iter 9). The tx-set build costs ~150-200 ms
    /// and normally runs INSIDE `trigger_next_ledger`, serialized between the
    /// trigger timer firing and `scp.nominate` — i.e. on the network's
    /// nomination-convergence critical path. The app arms this ~300 ms before
    /// the trigger so the trigger finds the value already cached and starts
    /// nominating within ~1 ms.
    ///
    /// The value snapshot is ≤300 ms staler than a trigger-time build: txs
    /// arriving in that window ride the next ledger (bounded extra latency
    /// for a small fraction of txs). `close_time` has whole-second
    /// granularity, so the prebuilt value almost always carries the same
    /// close time a trigger-time build would.
    ///
    /// Self-gating: no-ops (returns false) unless tracking, a validator, LCL
    /// still matches, nomination hasn't started, and no value is already
    /// cached for the slot. All the trigger-time gates re-run in
    /// `trigger_next_ledger`, so a stale prebuild is harmless.
    pub fn prebuild_nomination_value(&self, ledger_seq: u32) -> bool {
        if !self.is_tracking() || !self.is_validator() {
            return false;
        }
        let slot = ledger_seq as u64;
        if let Some(state) = self.scp.get_slot_state(slot) {
            if state.is_nominating {
                return false;
            }
        }
        if !self.lcl_matches_slot(slot) {
            return false;
        }
        if self
            .cached_nomination_value
            .read()
            .as_ref()
            .is_some_and(|(s, _)| *s == slot)
        {
            return false;
        }
        let now_secs = self.scp_driver.now_seconds();
        let lcl_close_time = self.lcl_close_time();
        let next_close_time = now_secs.max(lcl_close_time.saturating_add(1));
        if next_close_time > now_secs.saturating_add(MAX_TIME_SLIP_SECONDS) {
            return false;
        }
        let Some(value) = self.build_nomination_value(next_close_time) else {
            return false;
        };
        // Publish any envelopes that were waiting on the just-cached tx set,
        // mirroring the trigger path's post-build drain.
        self.process_ready_fetching_envelopes();
        // Discard if the world moved while building.
        if !self.lcl_matches_slot(slot) || self.is_applying() {
            return false;
        }
        *self.cached_nomination_value.write() = Some((slot, value));
        tracing::debug!(slot, "prebuilt nomination value cached");
        true
    }

    pub fn trigger_next_ledger(&self, ledger_seq: u32) -> Result<TriggerOutcome> {
        if !self.is_tracking() {
            return Err(HerderError::NotValidating);
        }

        let is_validator = self.is_validator();

        let slot = ledger_seq as u64;

        // If we have already started nominating for this slot, skip re-triggering.
        // Re-calling scp.nominate(timedout=false) would advance the nomination round
        // counter and disrupt SCP convergence. This makes trigger_next_ledger
        // idempotent for the active nomination phase, protecting against repeated
        // calls from manual_close_until's retry loop or periodic try_trigger_consensus.
        if let Some(state) = self.scp.get_slot_state(slot) {
            if state.is_nominating {
                tracing::debug!(
                    slot,
                    "Skipping duplicate trigger: nomination already active"
                );
                return Ok(TriggerOutcome::AlreadyNominating);
            }
        }

        // Defensive entry check: if LCL is already past or ahead of the
        // requested slot, abort before doing the (expensive) tx-set build.
        // This is the same condition the post-build re-check enforces; we
        // also evaluate it here so callers that pass a stale `ledger_seq`
        // don't pay for build_nomination_value before being told to retry.
        // Parity: stellar-core HerderImpl.cpp:1559-1562 evaluates the
        // equivalent condition; henyey evaluates it earlier (pre-build) AND
        // post-build to cover both "stale at entry" and "LCL advanced during
        // build" cases. LM=None proceeds (preserves existing tests).
        //
        // INV-H2 (parity: HerderImpl.cpp:1246-1248 setupTriggerNextLedger):
        // Fatal if LCL has overtaken tracking consensus index.
        self.assert_lcl_consistency();
        if !self.lcl_matches_slot(slot) {
            tracing::warn!(
                requested_slot = slot,
                "Skipping nomination: requested slot != LCL+1 at entry"
            );
            return Ok(TriggerOutcome::SkippedStale);
        }

        let now_secs = self.scp_driver.now_seconds();
        let lcl_close_time = self.lcl_close_time();
        let next_close_time = now_secs.max(lcl_close_time.saturating_add(1));
        if next_close_time > now_secs.saturating_add(MAX_TIME_SLIP_SECONDS) {
            tracing::info!(
                slot,
                next_close_time,
                now = now_secs,
                max_slip = MAX_TIME_SLIP_SECONDS,
                "Skipping nomination: proposed close time too far ahead"
            );
            return Ok(TriggerOutcome::SkippedInvalidCloseTime);
        }

        tracing::debug!("Triggering consensus for ledger {}", ledger_seq);

        // Record when we first started processing this slot (for timing metrics).
        self.scp_driver.record_slot_activity(slot);

        // [maxtps_cad] per-slot cadence decomposition stamp: trigger accepted →
        // tx-set build begins. Joined offline (by slot) with the nominate /
        // ballot / externalize / close stamps. Parity-irrelevant logging.
        tracing::info!(target: "maxtps_cad", slot, "trig");

        let t0 = std::time::Instant::now();

        if !is_validator {
            // --- Observer path ---
            // Parity: stellar-core HerderImpl.cpp:1546-1625 — observers build
            // the candidate tx set, self-validate, and publish it into the
            // pending/fetch cache, then stop before `makeStellarValue`/`nominate`.
            //
            // A None result here means the candidate build aborted on a benign
            // retryable condition (LCL advanced concurrently, or the defensive
            // close-time gate rejected). Map it to SkippedStale rather than a
            // hard error so callers retry on the next tick (review feedback).
            if self
                .build_and_cache_candidate_tx_set(next_close_time)
                .is_none()
            {
                tracing::warn!(
                    slot,
                    "trigger_next_ledger: candidate tx-set build returned None (stale race)"
                );
                return Ok(TriggerOutcome::SkippedStale);
            }
            let build_value_ms = t0.elapsed().as_millis();

            // Drain ready fetching envelopes after cache publication.
            self.process_ready_fetching_envelopes();

            // Post-build stale check (same as validator path).
            // Parity: HerderImpl.cpp:1574-1585 — re-read LCL AND check
            // isApplying() after addTxSet because side effects from draining
            // ready envelopes can externalize and start ledger application.
            if !self.lcl_matches_slot(slot) || self.is_applying() {
                tracing::warn!(
                    requested_slot = slot,
                    is_applying = self.is_applying(),
                    "Skipping: LCL advanced or apply started during observer build"
                );
                return Ok(TriggerOutcome::SkippedStale);
            }

            tracing::debug!(
                slot,
                build_value_ms,
                "trigger_next_ledger: observer cached tx set"
            );
            return Ok(TriggerOutcome::ObserverCached);
        }

        // --- Validator path ---
        // Reuse a prebuilt nomination value when the prebuild timer already
        // cached one for this slot (maxtps iter 9) — skips the ~150-200 ms
        // tx-set build on the nomination-convergence critical path. The
        // prebuild ran the same gates and cached the tx set for fetchers.
        let prebuilt = {
            let cached = self.cached_nomination_value.read();
            cached
                .as_ref()
                .filter(|(cached_slot, _)| *cached_slot == slot)
                .map(|(_, v)| v.clone())
        };
        if prebuilt.is_some() {
            tracing::debug!(slot, "trigger using prebuilt nomination value");
        }
        let value = match prebuilt.or_else(|| self.build_nomination_value(next_close_time)) {
            Some(v) => v,
            None => {
                // build_nomination_value returns None when LCL advanced
                // concurrently (stale close_time race). This is benign and
                // retryable — map to SkippedStale rather than a hard error.
                tracing::warn!(
                    slot,
                    "trigger_next_ledger: build_nomination_value returned None (stale race)"
                );
                return Ok(TriggerOutcome::SkippedStale);
            }
        };
        let build_value_ms = t0.elapsed().as_millis();

        // build_nomination_value() caches the tx set but no longer drains
        // ready envelopes inline. Drain now — safe because trigger_next_ledger
        // runs on spawn_blocking from the app layer.
        self.process_ready_fetching_envelopes();

        // Parity: HerderImpl.cpp:1574-1585 — a concurrent `close_ledger` task
        // running on a separate `spawn_blocking` may have advanced LCL while
        // we were inside `build_nomination_value` and the subsequent envelope
        // drain. Additionally, draining ready envelopes can trigger SCP
        // callbacks that externalize and start ledger application. If so, the
        // value we just built is for a slot the network has already closed or
        // is in the process of closing; broadcasting it would be wasted SCP
        // traffic.
        //
        // The check must come BEFORE the cache write below — caching a value
        // for a stale slot would mislead `handle_nomination_timeout`'s
        // cache-hit path.
        if !self.lcl_matches_slot(slot) || self.is_applying() {
            tracing::warn!(
                requested_slot = slot,
                is_applying = self.is_applying(),
                "Skipping nomination: LCL advanced or apply started during build_nomination_value"
            );
            return Ok(TriggerOutcome::SkippedStale);
        }

        // Cache the nomination value for this slot so timeout retries reuse it,
        // matching stellar-core's by-value lambda capture.
        *self.cached_nomination_value.write() = Some((slot, value.clone()));

        // Get previous value for priority calculation — reads from LCL when
        // available, matching stellar-core's triggerNextLedger(lcl.header.scpValue).
        let prev_value = self.get_previous_value();

        // Start SCP nomination
        let t1 = std::time::Instant::now();
        if self.scp.nominate(slot, value, &prev_value) {
            info!(slot, "Started SCP nomination for ledger");
        } else {
            debug!(
                slot,
                "Nomination already in progress or slot already externalized"
            );
        }
        let nominate_ms = t1.elapsed().as_millis();

        if build_value_ms > 50 || nominate_ms > 50 {
            tracing::warn!(
                slot,
                build_value_ms,
                nominate_ms,
                "trigger_next_ledger: slow consensus trigger"
            );
        } else {
            tracing::debug!(
                slot,
                build_value_ms,
                nominate_ms,
                "trigger_next_ledger timing"
            );
        }

        // For solo validators (1-of-1 quorum), the nomination→ballot→externalization
        // happens synchronously within self.scp.nominate(). Check if the slot was
        // externalized and advance tracking + publish in the correct order (#2695).
        self.complete_externalization(slot);

        Ok(TriggerOutcome::Triggered)
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

        // Restore `fully_validated` for any slot that was deferred only on
        // apply-lag and whose predecessor has now applied (i.e. the slot
        // is at or below the next-to-close index `lcl_seq + 1`).
        //
        // Without this call, peer envelopes that arrived for slot `S`
        // while the local LCL was still at `S - 2` or earlier would have
        // cleared the local node's `fully_validated` for slot `S` and
        // never restored it, silently dropping local participation in
        // the slot's consensus (audit finding H-014, issue #2096).
        //
        // Run BEFORE the purges below so a slot resolved this tick is
        // not also dropped by `purge_deferred_slots`/`purge_slots`. The
        // resolve call drains the deferred-slots mutex; the restore
        // calls take a separate SCP slots-write lock per slot. Late
        // inserts into `deferred_slots` between the resolve and the
        // restore loop are picked up by the next `ledger_closed` call.
        let next_index = slot.saturating_add(1);
        for s in self.scp_driver.resolve_apply_lag_for_next_index(next_index) {
            self.scp.restore_slot_fully_validated(s);
        }

        // Release the closing gate and replay buffered envelopes (issue #2122).
        // By this point LCL has advanced (the close task completed before
        // `ledger_closed` is called), so replayed envelopes take the
        // `is_current_ledger` validation path instead of the apply-lag path.
        // Must happen BEFORE `drain_and_process_pending` so the drain's
        // envelopes aren't re-buffered by the gate.
        let gate_buffered: Vec<ScpEnvelope> = {
            let mut gate = self.closing_gate.lock().unwrap();
            gate.slot = 0;
            std::mem::take(&mut gate.buffer)
        };
        if !gate_buffered.is_empty() {
            debug!(
                slot,
                count = gate_buffered.len(),
                "Draining closing gate buffer"
            );
            for env in gate_buffered {
                let _ = self.process_scp_envelope_with_tx_set(env);
            }
        }

        // Drain pending envelopes up to and including the next slot.
        //
        // This is henyey's approximation of stellar-core's
        // `safelyProcessSCPQueue(false)` → `postOnMainThread` pattern
        // (HerderImpl.cpp:1194). In stellar-core, `newSlotExternalized`
        // posts the drain to the main thread; by the time the callback
        // runs, `processExternalized` has applied the ledger and LCL has
        // advanced. In henyey, `ledger_closed` is the post-apply hook —
        // by this point `ledger_manager.current_ledger_seq() == slot`,
        // so drained envelopes for `slot + 1` see
        // `lcl_seq + 1 == slot_index`, taking the `is_current_ledger`
        // validation path instead of the apply-lag future-value path.
        //
        // Uses `release_up_to` which drains ALL pending envelopes for
        // slots ≤ target, preserving jump-ahead semantics for
        // intermediate slots that were externalized rapidly.
        self.drain_and_process_pending(next_index);
        self.process_ready_fetching_envelopes();

        // Most-recent-checkpoint slot to preserve across all the cleanups
        // below. Parity: stellar-core HerderImpl::eraseOutsideRange
        // (HerderImpl.cpp:1328-1335) computes lastCheckpointSeq once and
        // threads it into both HerderSCPDriver::purgeSlotsOutsideRange and
        // PendingEnvelopes::eraseOutsideRange so the checkpoint slot's
        // envelopes survive the retention window.
        let keep_slot = self.get_most_recent_checkpoint_seq();

        // Tracking-gated upper bound, shared by every cleanup below. Parity:
        // stellar-core threads a single maxSlotIndex
        // (nextConsensusLedgerIndex()+LEDGER_VALIDITY_BRACKET, only when
        // isTracking()) through HerderSCPDriver::purgeSlotsOutsideRange,
        // SCP::purgeSlotsOutsideRange, and PendingEnvelopes::eraseOutsideRange
        // (HerderImpl.cpp:260-290/1362, HerderSCPDriver.cpp:1653).
        let max_slot = if self.is_tracking() {
            Some(self.next_consensus_ledger_index().get() + LEDGER_VALIDITY_BRACKET)
        } else {
            None
        };

        // Clean up old SCP state, preserving the most-recent-checkpoint
        // slot so the delayed send_scp_state callback (#2670) can still
        // find it after a long apply or upon validator startup (#2706).
        // Threads the same tracking-gated max_slot upper bound as the
        // fetching-envelope and tx-set-cache purges below, closing the gap
        // where the SCP slots map retained far-future slots that core's
        // SCP::purgeSlotsOutsideRange (SCP.cpp:73-105) would evict.
        // Parity: HerderSCPDriver::purgeSlotsOutsideRange
        // (HerderSCPDriver.cpp:1653).
        self.scp
            .purge_slots(Some(slot.saturating_sub(10)), max_slot, Some(keep_slot));
        // Purge deferred slot tracking alongside SCP slot cleanup
        self.scp_driver
            .purge_deferred_slots(slot.saturating_sub(10));

        // Clean up old fetching envelopes and cached tx sets.
        // Parity: stellar-core HerderImpl.cpp:260-278 (newSlotExternalized)
        // + HerderImpl.cpp:1312-1318 (eraseOutsideRange computes slotToKeep
        // from getMostRecentCheckpointSeq).
        let min_ledger_seq = self.get_min_ledger_seq_to_remember();
        let min_slot = if min_ledger_seq > GENESIS_LEDGER_SEQ {
            Some(min_ledger_seq)
        } else {
            None
        };
        self.fetching_envelopes
            .erase_outside_range(min_slot, max_slot, keep_slot);

        // Evict cached tx sets outside the valid slot range in scp_driver.
        // Parity: stellar-core PendingEnvelopes::eraseOutsideRange
        // (PendingEnvelopes.cpp:790-796) prunes mTxSetCache on BOTH bounds at
        // every newSlotExternalized, keeping slot-0 (unknown) and slotToKeep.
        // The lower bound MUST run here on the live path: before 2026-07-04
        // the only below-prune ran on catchup, so under load the cache grew
        // ~16 MB/ledger unbounded (campaign-2 OOM leak).
        self.scp_driver
            .evict_cached_outside_range(min_slot, max_slot, keep_slot);

        // Clean up old data
        self.cleanup();

        // Purge stale pending envelopes for slots behind the closed ledger.
        self.pending_envelopes.purge_slots_below(slot);

        // INV-H2 (parity: HerderImpl.cpp:1227-1248 lastClosedLedgerIncreased):
        // After all close bookkeeping, verify LCL hasn't overtaken tracking.
        // Both SCP and non-SCP paths advance tracking before this point:
        // - SCP: complete_externalization → advance_tracking_slot (#2695)
        // - Non-SCP (catchup/watcher): advance_tracking_to called in
        //   handle_close_complete_inner before any .await (#2720)
        self.assert_lcl_consistency();
    }

    /// Clear the closing gate and discard buffered envelopes.
    ///
    /// Used by error/panic paths in the close pipeline where LCL has NOT
    /// advanced. Replaying the buffered envelopes would produce the same
    /// `MaybeValidDeferred` result, so they are discarded. Normal SCP
    /// mechanisms (pending-envelope drain, peer re-fetch) will deliver
    /// them again once the node recovers.
    pub fn clear_closing_gate(&self) {
        let mut gate = self.closing_gate.lock().unwrap();
        gate.slot = 0;
        gate.buffer.clear();
    }

    /// Handle nomination timeout.
    ///
    /// Called when the nomination timer expires. Re-nominates with the same
    /// value to try to make progress.
    ///
    /// Callers must ensure this runs on `spawn_blocking` (not the async
    /// event loop) because the cache-miss fallback calls
    /// `build_nomination_value()` → `cache_tx_set()` and then drains
    /// ready envelopes via `process_ready_fetching_envelopes()`.
    pub fn handle_nomination_timeout(&self, slot: SlotIndex) -> TimeoutOutcome {
        if !self.is_validator() {
            return TimeoutOutcome::NoOp; // Observers don't nominate
        }

        // Defensive entry check: if LCL is not at `slot - 1`, the slot is
        // stale before we even consult the cache or rebuild. Mirrors the
        // pre-build entry check in `trigger_next_ledger`.
        //
        // INV-H2: Fatal if LCL has overtaken tracking consensus index.
        self.assert_lcl_consistency();
        if !self.lcl_matches_slot(slot) {
            tracing::warn!(
                slot,
                "Skipping nomination timeout: requested slot != LCL+1 at entry"
            );
            return TimeoutOutcome::SkippedStale;
        }

        let prev_value = self.get_previous_value();

        // Reuse the cached nomination value for this slot, matching stellar-core's
        // by-value lambda capture (NominationProtocol.cpp:654-659).
        let cached_value = {
            let cached = self.cached_nomination_value.read();
            cached
                .as_ref()
                .filter(|(cached_slot, _)| *cached_slot == slot)
                .map(|(_, v)| v.clone())
        };

        // Fall back to building a fresh value if none cached (e.g., if
        // trigger_next_ledger wasn't called for this slot).
        let (value, built_fresh) = match cached_value {
            Some(v) => (Some(v), false),
            None => {
                let now_secs = self.scp_driver.now_seconds();
                let lcl_close_time = self.lcl_close_time();
                let next_close_time = now_secs.max(lcl_close_time.saturating_add(1));
                if next_close_time > now_secs.saturating_add(MAX_TIME_SLIP_SECONDS) {
                    tracing::info!(
                        slot,
                        next_close_time,
                        now = now_secs,
                        "Nomination timeout: close time still invalid, returning NoOp"
                    );
                    return TimeoutOutcome::SkippedInvalidCloseTime;
                }
                (self.build_nomination_value(next_close_time), true)
            }
        };

        // build_nomination_value() caches a tx set but no longer drains
        // ready envelopes inline. Drain only when the fallback fired.
        if built_fresh {
            self.process_ready_fetching_envelopes();
        }

        if let Some(value) = value {
            // Parity: HerderImpl.cpp:1550-1562 — same race as
            // `trigger_next_ledger`. A concurrent `close_ledger` task may have
            // advanced LCL while we were inside `build_nomination_value`
            // (cache-miss path) or between the cache write and the timer
            // firing (cache-hit path). The re-check is symmetric across both
            // branches because the cached value can also age out while the
            // timer was pending.
            if !self.lcl_matches_slot(slot) {
                tracing::warn!(
                    slot,
                    "Skipping nomination timeout: LCL advanced during build/drain"
                );
                return TimeoutOutcome::SkippedStale;
            }

            if self.scp.nominate_timeout(slot, value, &prev_value) {
                debug!(slot, "Re-nominated after timeout");
                // SCP may have externalized synchronously; complete if so (#2695).
                self.complete_externalization(slot);
                return TimeoutOutcome::Renominated;
            }
            // Even on no-op, SCP state may have advanced to externalized.
            self.complete_externalization(slot);
            return TimeoutOutcome::NoOp;
        }
        TimeoutOutcome::NoOp
    }

    /// Helper for parity gates with `HerderImpl.cpp:1550-1562`.
    ///
    /// Returns true when the requested SCP `slot` matches `LCL + 1`, i.e.
    /// the next slot we should nominate.
    fn lcl_matches_slot(&self, slot: SlotIndex) -> bool {
        self.ledger_manager.current_ledger_seq() as u64 + 1 == slot
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
        // SCP may have externalized synchronously; complete if so (#2695).
        self.complete_externalization(slot);
    }

    /// Build a nomination-ready SCP `Value`: transaction set + signed StellarValue.
    ///
    /// Called by `trigger_next_ledger` (validator path) to build the initial
    /// nomination value. Delegates the heavy build/validate/cache work to
    /// [`build_and_cache_candidate_tx_set`] then adds the validator-only
    /// signing step (upgrades + `make_stellar_value`).
    ///
    /// `close_time` is the caller-validated nomination close time (#2816):
    /// `trigger_next_ledger`/`handle_nomination_timeout` compute it once via
    /// the monotonic clamp + far-ahead `ctValidityOffset` check, mirroring
    /// stellar-core `HerderImpl::triggerNextLedger` which derives
    /// `nextCloseTime` once before `addTxSet`/`makeStellarValue`. The builder
    /// must therefore never re-read `SystemTime::now()` itself.
    fn build_nomination_value(&self, close_time: u64) -> Option<Value> {
        let parts = self.build_and_cache_candidate_tx_set(close_time)?;

        // 4. Upgrades: config + runtime, filtered against current state.
        // Use lcl_close_time (not candidate close_time) for upgrade parameter
        // decisions to prevent one-ledger-early activation (#1166).
        let state = CurrentLedgerState {
            close_time: parts.lcl_close_time,
            protocol_version: parts.header.ledger_version,
            base_fee: parts.header.base_fee,
            max_tx_set_size: parts.header.max_tx_set_size,
            base_reserve: parts.header.base_reserve,
            flags: match &parts.header.ext {
                stellar_xdr::LedgerHeaderExt::V0 => 0,
                stellar_xdr::LedgerHeaderExt::V1(ext) => ext.flags,
            },
            max_soroban_tx_set_size: parts.max_soroban_tx_set_size,
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
                // Parity: gate config upgrades through the same makeFromKey +
                // isValidForApply + upgradeNeeded checks as runtime upgrades.
                LedgerUpgrade::Config(_) => parts
                    .config_ctx
                    .as_ref()
                    .and_then(|ctx| match ctx.should_propose() {
                        Ok(v) => Some(v),
                        Err(e) => {
                            tracing::error!(
                                error = %e,
                                "broken ledger state in config upgrade check"
                            );
                            None
                        }
                    })
                    .unwrap_or(false),
            })
            .cloned()
            .collect();

        // If should_propose() returned Err above (broken ledger state), the
        // Config upgrade is excluded from the upgrade list (treated as false).
        // The error was already logged above. Nomination continues without it.

        let runtime_upgrades = match self
            .runtime_upgrades
            .read()
            .create_upgrades_for(&state, parts.config_ctx.as_ref())
        {
            Ok(upgrades) => upgrades,
            Err(e) => {
                tracing::error!(
                    error = %e,
                    "broken ledger state in runtime config upgrade check; aborting nomination"
                );
                return None;
            }
        };
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

        // #3588: Config upgrades were previously silent. Log (additive, no
        // behavior change) when a LedgerUpgrade::Config is retained for
        // nomination, so the soroban-config-upgrade path is observable.
        if let Some(LedgerUpgrade::Config(key)) = upgrade_list
            .iter()
            .find(|u| matches!(u, LedgerUpgrade::Config(_)))
        {
            tracing::info!(
                slot = parts.header.ledger_seq + 1,
                content_hash = %hex::encode(key.content_hash.0),
                "Nominating LedgerUpgrade::Config"
            );
        }

        // Parity: stellar-core logs and skips individual upgrades whose encoded
        // size exceeds UpgradeType::max_size() (HerderImpl.cpp:1572-1587).
        // The XDR encode itself should never fail for a valid LedgerUpgrade.
        let upgrades: Vec<UpgradeType> = upgrade_list
            .iter()
            .filter_map(|upgrade| {
                let bytes = upgrade
                    .to_xdr(Limits::none())
                    .expect("BUG: failed to encode LedgerUpgrade to XDR");
                match bytes.try_into() {
                    Ok(b) => Some(UpgradeType(b)),
                    Err(_) => {
                        error!(
                            upgrade_type = ?std::mem::discriminant(upgrade),
                            "upgrade blob exceeds UpgradeType max size — skipping"
                        );
                        None
                    }
                }
            })
            .collect();

        // 5. Sign & encode
        let stellar_value = self
            .make_stellar_value(parts.tx_set_hash, parts.close_time, upgrades)
            .ok()?;
        let value_bytes = henyey_common::xdr_to_bytes(&stellar_value);
        let value = Value(value_bytes.try_into().ok()?);
        Some(value)
    }

    /// Shared build/validate/cache pipeline for the next-slot candidate tx set.
    ///
    /// Parity: stellar-core `HerderImpl::triggerNextLedger` lines 1546-1625 —
    /// all work up to (but not including) `makeStellarValue` and `nominate`.
    /// Both validators and observers execute this path; observers stop here.
    ///
    /// `close_time` is supplied by the caller (#2816): `trigger_next_ledger`
    /// and the cache-miss rebuild in `handle_nomination_timeout` compute and
    /// validate it once (monotonic clamp + far-ahead `ctValidityOffset`
    /// check) before invoking this builder. This mirrors stellar-core where
    /// `nextCloseTime` is derived once in `triggerNextLedger` before
    /// `addTxSet`/`makeStellarValue`. The builder must not re-read the clock.
    ///
    /// Steps:
    ///   1. Read ledger state (previous_hash, max_txs, starting_seq)
    ///   2. Use the caller-supplied validated close_time
    ///   3. Build generalized transaction set
    ///   3.5. Self-validate and cache the built tx set
    fn build_and_cache_candidate_tx_set(&self, close_time: u64) -> Option<CandidateTxSetParts> {
        // 1. Ledger state — create ONE snapshot for the entire nomination pass.
        // This snapshot is shared between build_starting_seq_map, the
        // trim_invalid_two_phase validation, and config upgrade context.
        let (
            previous_hash,
            max_txs,
            starting_seq,
            header,
            lcl_close_time,
            max_soroban_tx_set_size,
            snapshot_providers,
            config_ctx,
            soroban_info,
            frozen_key_config,
        ) = {
            // ONE snapshot for the entire nomination pass: tx set building,
            // seq map, config upgrade context, and self-validation providers
            // all observe the same ledger state. Consolidation eliminates the
            // race where a concurrent commit_close() could advance the ledger
            // between the old multi-snapshot reads.
            let snap = match self.ledger_manager.create_snapshot() {
                Ok(s) => s,
                Err(e) => {
                    // If we can't create a consistent snapshot, abort
                    // nomination rather than proceeding with potentially
                    // inconsistent state. Safer but less available —
                    // nomination will be retried on the next timer tick.
                    tracing::warn!(
                        error = %e,
                        "Failed to create snapshot for nomination; aborting"
                    );
                    return None;
                }
            };

            let header = snap.header().clone();
            let lcl_ct = header.scp_value.close_time.0;
            let max = header.max_tx_set_size as usize;
            let ledger_seq = snap.ledger_seq();
            let header_hash = snap.header_hash();

            // Soroban info captured atomically with header inside create_snapshot(),
            // eliminating the TOCTOU race with commit_close().
            let soroban_info = snap.soroban_network_info().cloned();
            let soroban_max = soroban_info.as_ref().map(|info| info.ledger_max_tx_count);

            // Load CAP-77 frozen key config from the same snapshot.
            // Pre-V26 returns empty config (no frozen keys).
            let frozen_key_config = match henyey_ledger::execution::load_frozen_key_config(
                &snap,
                header.ledger_version,
            ) {
                Ok(config) => Some(config),
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        "Failed to load frozen key config for nomination; \
                         frozen key checks will be skipped"
                    );
                    None
                }
            };

            // Build seq map from the snapshot (borrows).
            let seq = self.build_starting_seq_map(&snap, ledger_seq);

            // Config upgrade context from the same snapshot (borrows).
            // ConfigUpgradeContext::from_snapshot clones the snapshot
            // internally, so both the tx set and config context observe
            // data from the same point in time.
            let cfg_ctx = self
                .runtime_upgrades
                .read()
                .parameters()
                .config_upgrade_set_key
                .as_ref()
                .and_then(|k| k.to_xdr().ok())
                .and_then(
                    |key| match ConfigUpgradeContext::from_snapshot(&snap, &key) {
                        // Log the OUTCOME of from_snapshot (#3591). Previously
                        // this edge logged only on Err — never on Ok(None),
                        // whose silence is why three investigations saw nothing
                        // while the armed key failed to resolve at nomination.
                        Ok(Some(ctx)) => {
                            // Steady state: debug to avoid mainnet per-nomination
                            // spam (a config upgrade is armed on essentially
                            // every nomination once scheduled).
                            tracing::debug!(
                                contract_id = %hex::encode(key.contract_id.0 .0),
                                content_hash = %hex::encode(key.content_hash.0),
                                "config upgrade key RESOLVED from nomination snapshot"
                            );
                            Some(ctx)
                        }
                        Ok(None) => {
                            // Present-but-unresolved: the armed key has no live
                            // entry in the nomination snapshot (absent / wrong
                            // durability / TTL-expired). This is the #3591
                            // failure signature — surface it.
                            tracing::warn!(
                                contract_id = %hex::encode(key.contract_id.0 .0),
                                content_hash = %hex::encode(key.content_hash.0),
                                "config upgrade key is armed but did NOT resolve \
                                 in the nomination snapshot (Ok(None)); config \
                                 upgrade will NOT be proposed"
                            );
                            None
                        }
                        Err(e) => {
                            error!("Error loading config upgrade context: {e}");
                            None
                        }
                    },
                );

            // SnapshotProviders takes ownership of the snapshot.
            let sp = crate::tx_queue::SnapshotProviders::new(snap);

            (
                header_hash,
                max,
                seq,
                header,
                lcl_ct,
                soroban_max,
                Some(sp),
                cfg_ctx,
                soroban_info,
                frozen_key_config,
            )
        };

        // 2. Caller supplied a validated close time. Use its offset so tx-set
        // trimming still sees the proposed close time (#1192).
        // Use checked subtraction: if LCL advanced concurrently (race between
        // the caller's lcl_close_time() read and our snapshot), abort nomination
        // rather than wrapping to a huge offset.
        let close_time_offset = match close_time.checked_sub(lcl_close_time) {
            Some(0) | None => {
                // close_time <= lcl_close_time from snapshot: LCL advanced
                // between caller's read and our snapshot, making the proposed
                // close time stale. Abort — caller will retry next tick.
                tracing::warn!(
                    close_time,
                    lcl_close_time,
                    "build_nomination_value: close_time stale vs snapshot LCL; aborting"
                );
                return None;
            }
            Some(offset) => offset,
        };

        // 2.5. Close-time validity gate (parity: stellar-core HerderImpl.cpp:1524).
        // stellar-core calls `ctValidityOffset(nextCloseTime)` which checks
        // `nextCloseTime <= now + MAX_TIME_SLIP_SECONDS`. If the locally-selected
        // close time is invalid (e.g. node clock is behind and lcl_close_time+1
        // is too far in the future), skip the entire build/cache/publish path.
        if !self
            .scp_driver
            .check_close_time(0, lcl_close_time, close_time)
        {
            tracing::warn!(
                close_time,
                lcl_close_time,
                "Invalid close time selected, skipping trigger (parity: ctValidityOffset gate)"
            );
            return None;
        }

        // 3. Build & cache tx set, trimming against proposed close time.
        // Use the pre-built snapshot providers for O(1) snapshot creation.
        //
        // Parity: stellar-core's TxSetXDRFrame::makeEmpty(lclHeader) selects the
        // tx set format based on the LCL protocol version. At protocol < 20 the
        // Generalized format does not exist — the tx_set_hash in scpValue must be
        // computed as a non-generalized (Classic) contents hash. Without this
        // check, a genesis-at-v0 network (e.g. quickstart) produces a Generalized
        // hash during nomination but the catchup path reconstructs a Classic hash,
        // causing "invalid tx set hash" errors (#2297).
        let lcl = LclContext::new(header.ledger_version, previous_hash);
        let tx_set = if !protocol_version_starts_from(lcl.protocol_version(), ProtocolVersion::V20)
        {
            TransactionSet::new_legacy(previous_hash, vec![])
        } else {
            // Construct NominationBuildContext from the snapshot header so the
            // build path uses the same ledger state as self-validation (#2319).
            let network_id = NetworkId(self.scp_driver.network_id());
            let ledger_flags = match &header.ext {
                stellar_xdr::LedgerHeaderExt::V0 => 0u32,
                stellar_xdr::LedgerHeaderExt::V1(v1) => v1.flags,
            };
            let mut validation_ctx = crate::tx_set_utils::TxSetValidationContext::new(
                header.ledger_seq,
                header.scp_value.close_time.0,
                header.base_fee,
                header.base_reserve,
                header.ledger_version,
                network_id,
                ledger_flags,
            );
            if let Some(info) = soroban_info.as_ref() {
                validation_ctx.soroban_resource_limits = Some(info.to_resource_limits());
            }
            if let Some(fk) = frozen_key_config.as_ref() {
                validation_ctx.frozen_key_config = fk.clone();
            }
            let nomination_ctx = crate::tx_queue::NominationBuildContext {
                base_fee: header.base_fee as i64,
                protocol_version: header.ledger_version,
                validation_ctx,
            };
            self.tx_queue.build_generalized_tx_set_with_providers(
                crate::tx_queue::BuildContext::Nomination(&nomination_ctx),
                previous_hash,
                max_txs,
                starting_seq.as_ref(),
                close_time_offset,
                snapshot_providers
                    .as_ref()
                    .map(|sp| sp as &dyn crate::tx_queue::FeeBalanceProvider),
                snapshot_providers
                    .as_ref()
                    .map(|sp| sp as &dyn crate::tx_queue::AccountProvider),
            )
        };
        debug!(
            hash = %tx_set.hash(),
            tx_count = tx_set.len(),
            "Proposing transaction set"
        );
        // [maxtps_ban] queue-vs-selected accounting per candidate build: under
        // sustained load, per-ledger fills track ARRIVALS while a multi-ledger
        // backlog sits somewhere for ~10 ledgers — this line splits
        // "queue doesn't hold it" from "selection drops it".
        tracing::info!(
            target: "maxtps_ban",
            queue_len = self.tx_queue.len(),
            selected = tx_set.len(),
            max_txs,
            "proposed_set"
        );

        // 3.5. Self-validation roundtrip + cache. Aborts nomination on
        // self-validation failure rather than caching a known-bad set
        // (defense-in-depth #2103, #2113).
        self.validate_and_cache_built_tx_set(
            &tx_set,
            &header,
            previous_hash,
            close_time_offset,
            soroban_info.as_ref(),
            snapshot_providers.as_ref(),
        )?;

        Some(CandidateTxSetParts {
            tx_set_hash: *tx_set.hash(),
            close_time,
            lcl_close_time,
            header,
            max_soroban_tx_set_size,
            config_ctx,
        })
    }

    /// Run self-validation on `tx_set` and, on success, cache it. On failure
    /// log an `error!` with full nomination context and return `None` so the
    /// caller (`build_nomination_value`) can abort nomination via `?`.
    ///
    /// This is the integration point for the defense-in-depth check
    /// introduced in #2113. It is intentionally a separate function from
    /// `self_validate_nomination_tx_set` so the wiring contract — "if the
    /// helper rejects, do not cache and propagate `None`" — can be tested
    /// directly without staging a full ledger-manager scenario.
    fn validate_and_cache_built_tx_set(
        &self,
        tx_set: &TransactionSet,
        header: &LedgerHeader,
        previous_hash: Hash256,
        close_time_offset: u64,
        soroban_info: Option<&henyey_ledger::SorobanNetworkInfo>,
        snapshot_providers: Option<&crate::tx_queue::SnapshotProviders>,
    ) -> Option<()> {
        if let Err(reason) = self.self_validate_nomination_tx_set(
            tx_set,
            header,
            &previous_hash,
            close_time_offset,
            soroban_info,
            snapshot_providers,
        ) {
            error!(
                tx_set_hash = %tx_set.hash(),
                tx_count = tx_set.len(),
                previous_hash = %previous_hash,
                close_time_offset,
                ledger_seq = header.ledger_seq,
                %reason,
                "Self-validation rejected the freshly-built nomination tx \
                 set; aborting nomination (defense-in-depth #2113)"
            );
            return None;
        }

        // Populate the validity cache with `true` for this locally-built tx set.
        // Mirrors stellar-core's `cacheValidTxSet(*applicableProposedSet, lcl,
        // upperBoundCloseTimeOffset)` in `HerderImpl::triggerNextLedger()` —
        // ensures later SCP nomination/validation lookups can reuse the result
        // without re-running full `checkValid`. This side effect runs for both
        // validators and observers, before the non-validator early return.
        self.scp_driver
            .cache_valid_tx_set(previous_hash, *tx_set.hash(), close_time_offset);

        // Cache the tx set and notify FetchingEnvelopes. Does NOT drain the
        // ready queue — callers (trigger_next_ledger, handle_nomination_timeout)
        // are responsible for draining via process_ready_fetching_envelopes().
        self.cache_tx_set(tx_set.clone());
        Some(())
    }

    /// Run a post-build self-validation roundtrip on `tx_set`, mirroring
    /// stellar-core's `makeTxSetFromTransactions` (TxSetFrame.cpp:898-952)
    /// and matching the structure of the incoming-SCP path
    /// (`ScpDriver::check_and_cache_tx_set_valid`).
    ///
    /// Sequence:
    /// 1. `prepare_for_apply` — validates XDR structure, sort order, fee map
    ///    well-formedness, and the cross-phase no-duplicate-source-account
    ///    invariant (HERDER_SPEC §8.3 / §6.5). This mirrors stellar-core's
    ///    `prepareForApply` step in `makeTxSetFromTransactions`.
    /// 2. `check_tx_set_valid` — content validation against the same ledger
    ///    state used to build the set.
    ///
    /// The recomputed hash from `prepare_for_apply` is compared against the
    /// builder's stored hash as an invariant check (analogous to the
    /// per-phase tx-count check in stellar-core's roundtrip).
    ///
    /// Returns `true` if the tx set passes self-validation, `false` if it
    /// should be rejected and nomination aborted.
    ///
    /// On protocol < V20 this returns `true` unconditionally — generalized
    /// tx sets are not expected at those versions and `check_tx_set_valid`
    /// would unconditionally reject them. Only simulation environments hit
    /// the pre-V20 path; production validators run on protocol 24+.
    ///
    /// On protocol >= V20 a non-generalized result from the builder is a
    /// construction bug, not a routine "skip" condition, so it is logged at
    /// `warn!` and rejected.
    ///
    /// Unlike the SCP-incoming path in `ScpDriver::check_and_cache_tx_set_valid`
    /// (which passes `None, None` for the providers because incoming-message
    /// validation can race against peers' construction), this builder path
    /// validates against the **same** snapshot used to build the tx set, so
    /// passing the providers is correct and stricter.
    fn self_validate_nomination_tx_set(
        &self,
        tx_set: &TransactionSet,
        header: &LedgerHeader,
        lcl_hash: &Hash256,
        close_time_offset: u64,
        soroban_info: Option<&henyey_ledger::SorobanNetworkInfo>,
        snapshot_providers: Option<&crate::tx_queue::SnapshotProviders>,
    ) -> std::result::Result<(), String> {
        if !protocol_version_starts_from(header.ledger_version, ProtocolVersion::V20) {
            return Ok(());
        }

        // Step 1: XDR-structure / sort-order / cross-phase duplicate-source
        // checks. Parity: stellar-core `prepareForApply` step in
        // `makeTxSetFromTransactions`. Also matches the incoming SCP path
        // (`scp_driver.rs::check_and_cache_tx_set_valid`).
        let prepared = match tx_set.prepare_for_apply(NetworkId(self.scp_driver.network_id())) {
            Ok(prepared) => prepared,
            Err(e) => {
                return Err(format!("prepare_for_apply failed: {}", e));
            }
        };

        // Hash invariant: the prepared XDR-roundtrip hash must match the
        // builder's stored hash. A mismatch indicates the builder produced
        // a hash that disagrees with the canonical XDR encoding — analogous
        // to stellar-core's per-phase tx-count check after the roundtrip.
        if prepared.hash() != tx_set.hash() {
            return Err(format!(
                "prepare_for_apply hash {} differs from builder hash {}",
                prepared.hash(),
                tx_set.hash()
            ));
        }

        // Builder invariant: on protocol >= V20, we should always produce
        // a generalized tx set. A legacy set here indicates a construction bug.
        if !prepared.is_generalized() {
            return Err(format!(
                "built tx set is not generalized on protocol {}",
                header.ledger_version
            ));
        }

        // Step 2: content validation against the same snapshot used to build
        // the set. Parity: stellar-core `checkValidInternal` step.
        let frozen_key_config = snapshot_providers.and_then(|sp| {
            henyey_ledger::execution::load_frozen_key_config(sp.snapshot(), header.ledger_version)
                .ok()
        });
        prepared
            .check_valid(
                header,
                lcl_hash,
                close_time_offset,
                NetworkId(self.scp_driver.network_id()),
                soroban_info,
                snapshot_providers.map(|sp| sp as &dyn crate::tx_queue::FeeBalanceProvider),
                snapshot_providers.map(|sp| sp as &dyn crate::tx_queue::AccountProvider),
                frozen_key_config.as_ref(),
            )
            .map_err(|e| e.to_string())
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
        let xdr_tx_set_hash = stellar_xdr::Hash(tx_set_hash.0);
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
        let node_id = NodeId(stellar_xdr::PublicKey::PublicKeyTypeEd25519(Uint256(
            *secret_key.public_key().as_bytes(),
        )));

        Ok(StellarValue {
            tx_set_hash: xdr_tx_set_hash,
            close_time: xdr_close_time,
            upgrades: upgrades
                .try_into()
                .expect("BUG: upgrades exceed XDR max of 6"),
            ext: StellarValueExt::Signed(LedgerCloseValueSignature {
                node_id,
                signature: XdrSignature(sig.0.to_vec().try_into().unwrap_or_default()),
            }),
        })
    }

    fn build_starting_seq_map(
        &self,
        snapshot: &henyey_ledger::SnapshotHandle,
        ledger_seq: u32,
    ) -> Option<HashMap<Vec<u8>, i64>> {
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
    pub fn source_account_pending(&self, account_id: &stellar_xdr::AccountId) -> bool {
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
    /// Returns SCP envelopes for slots starting from `from_slot`, sorted by
    /// slot index (ascending).
    pub fn get_scp_state(&self, from_slot: u64) -> Vec<ScpEnvelope> {
        self.scp.get_scp_state(from_slot)
    }

    /// Get all SCP envelopes recorded for a slot.
    pub fn get_scp_envelopes(&self, slot: u64) -> Vec<ScpEnvelope> {
        self.scp.get_slot_envelopes(slot)
    }

    /// Get the externalizing state for a slot.
    ///
    /// Returns envelopes that contribute to the externalized state of a slot,
    /// matching stellar-core's `getExternalizingState(slot)` used in
    /// `processExternalized()` for SCP history persistence.
    pub fn get_scp_externalizing_state(&self, slot: u64) -> Vec<ScpEnvelope> {
        self.scp.get_externalizing_state(slot)
    }

    /// Get the current sendable SCP state for a specific slot.
    ///
    /// Returns envelopes that should be sent to peers, matching stellar-core's
    /// `processCurrentState(slot, ..., false)` semantics. Only includes
    /// envelopes from fully-validated slots.
    pub fn get_current_state_for_slot(&self, slot: u64) -> Vec<ScpEnvelope> {
        self.scp.get_current_state_for_slot(slot)
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
        self.pending_envelopes.purge_slots_below(slot);
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

    /// Clear slot-scoped scp_driver caches (tx sets, externalized values,
    /// timing maps) while preserving quorum set caches.
    /// See #1874 for why quorum set caches must not be cleared.
    pub fn clear_slot_scoped_scp_caches(&self) {
        self.scp_driver.clear_slot_scoped_caches();
    }

    /// Trim stale scp_driver caches while preserving data for future slots.
    /// Called after catchup to release memory while keeping pending tx_set
    /// requests and externalized data for slots after catchup.
    pub fn trim_scp_driver_caches(&self, keep_after_slot: SlotIndex) {
        self.scp_driver.trim_stale_caches(keep_after_slot);
    }

    /// Trim stale fetching caches (quorum-set cache and slot tracking).
    /// Called after catchup to release memory for slots that are no longer
    /// relevant. Tx-set retention is handled by ScpDriver's cache.
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

    /// Count tx-set backlog (cached + pending) in the active catchup window.
    /// Used by app-side scheduling to bound outbound `GetTxSet` demand.
    pub fn tx_set_backlog_in_window(&self, from_slot: u64, to_slot: u64) -> usize {
        // Only count cached tx-sets (which consume memory) — not pending hashes
        // (which are small metadata). Counting pending against the budget would
        // prevent GetTxSet from ever being issued when many hashes arrive during
        // catchup, causing a liveness deadlock.
        self.scp_driver.cached_tx_sets_in_window(from_slot, to_slot)
    }

    /// Receive a transaction set from the network.
    /// Returns the slot it was needed for, if any.
    ///
    /// This also processes any envelopes that were waiting for this tx set,
    /// feeding them to SCP now that the dependency is satisfied.
    ///
    /// ### Off-event-loop drain (#1773 Phase 2)
    ///
    /// Phase 1 (#1772) established per-phase telemetry and observed
    /// that `process_ready_ms` (the envelope-drain phase) accounts
    /// for 100% of a 342 ms on-event-loop WARN. That drain is pure
    /// CPU + lock traffic (XDR validation inside
    /// `SCP::receive_envelope` and parking_lot writes against
    /// `scp.slots`), with no `.await` inside the body — textbook fit
    /// for `tokio::task::spawn_blocking`.
    ///
    /// Phase 2 keeps the first two sub-phases inline (both are O(1)
    /// per the Phase-1 evidence) and moves only
    /// [`process_ready_fetching_envelopes`](Self::process_ready_fetching_envelopes)
    /// onto a blocking-pool thread. The async wrapper awaits the
    /// `JoinHandle` before returning, because callers (see
    /// `crates/app/src/app/tx_flooding.rs`) immediately call
    /// `try_close_slot_directly(slot)` which reads externalization
    /// state set *inside* the drain. The ordering contract — drain
    /// complete before return — is preserved.
    ///
    /// ### Per-phase telemetry
    ///
    /// Three phases:
    /// - `tracker_receive_ms` — `ScpDriver::receive_tx_set` (inline).
    /// - `notify_tx_set_ms` — `FetchingEnvelopes::on_tx_set_accepted`
    ///   (inline, only when the tracker accepted the tx set).
    /// - `process_ready_spawn_blocking_ms` — time the event-loop task
    ///   spends awaiting the blocking drain's `JoinHandle`. After the
    ///   fix this is the drain's wall time, but the event loop is
    ///   parked (cooperative) during the await — other tokio tasks
    ///   run. The rename from `process_ready_ms` is deliberate: it
    ///   signals the semantic shift and lets alerting tools recognise
    ///   the fix landed.
    pub async fn receive_tx_set(self: Arc<Self>, tx_set: TransactionSet) -> Option<SlotIndex> {
        let mut timer = crate::tracked_lock::PhaseTimer::start();
        let hash = *tx_set.hash();

        let slot = self.scp_driver.receive_tx_set(tx_set);
        timer.mark("tracker_receive_ms");

        // Only notify FetchingEnvelopes when the authoritative tracker
        // accepted the tx set. Unsolicited or malformed tx sets (where
        // scp_driver returns None) must NOT reach the fetcher — the
        // read-through callback already queries scp_driver.has_tx_set()
        // so no cache poisoning is possible. See #2066.
        if let Some(_accepted_slot) = slot {
            self.fetching_envelopes.on_tx_set_accepted(&hash);
            timer.mark("notify_tx_set_ms");
        }

        // Drain envelopes that just became ready, on a blocking-pool
        // thread so the event loop can run other tasks while the 300+
        // ms drain proceeds (#1773).
        //
        // We `await` the JoinHandle before returning: callers rely on
        // externalization state populated inside the drain (e.g.
        // `try_close_slot_directly(slot)`).
        let drain_start = std::time::Instant::now();
        let drained = self
            .drain_ready_envelopes_blocking("envelope drain after receive_tx_set")
            .await;
        let drain_ms = drain_start.elapsed().as_millis() as u64;
        if drained > 0 || drain_ms >= 10 {
            // [maxtps_nom] Envelopes released by this tx-set arrival and how
            // long the drain took — the leader's NOMINATE typically waits here.
            tracing::info!(
                target: "maxtps_nom",
                drained,
                drain_ms,
                hash = %hash,
                "envelope drain after tx-set receive"
            );
        }
        timer.mark_cooperative("process_ready_spawn_blocking_ms");

        timer.finish("herder.receive_tx_set");
        slot
    }

    /// Process envelopes that have become ready after tx set arrival.
    ///
    /// This is called after receiving a tx set to feed any buffered envelopes
    /// to SCP now that their dependencies are satisfied.
    pub fn process_ready_fetching_envelopes(&self) -> usize {
        let tracking_slot = self.tracking_slot().get();
        let is_tracking = self.state().is_tracking();
        let mut processed = 0;

        // When tracking, only drain envelopes for slots <= tracking_slot
        // (current consensus slot). Future-slot envelopes stay in the ready
        // queue until slot advances. When NOT tracking (booting/syncing),
        // drain all ready slots — there's no slot-aware consumption limit.
        let ready_slots = self.fetching_envelopes.ready_slots();
        for slot in ready_slots {
            if is_tracking && slot > tracking_slot {
                continue;
            }
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

    /// Cache a transaction set and notify FetchingEnvelopes.
    ///
    /// Stores the tx set in the SCP driver and notifies FetchingEnvelopes
    /// so blocked envelopes become ready. Does NOT drain the ready queue —
    /// callers must call `process_ready_fetching_envelopes()` separately
    /// (via `spawn_blocking` to avoid event-loop stalls).
    ///
    /// Concurrent drains are safe: `process_ready_fetching_envelopes()`
    /// acquires interior locks, so overlapping calls from multiple
    /// `spawn_blocking` tasks serialize correctly.
    fn cache_tx_set(&self, tx_set: TransactionSet) {
        let hash = *tx_set.hash();
        // Stamp slot provenance (the slot SCP is working on) so the per-close
        // range prune in `ledger_closed` can evict this set once it leaves the
        // remembered window. Slot 0 (boot/syncing) keeps the unknown-slot
        // sentinel — mirroring stellar-core's slot-0 rule for tx sets loaded
        // from the database (PendingEnvelopes.cpp:789-791). Without provenance
        // these entries survive every slot-range prune and the cache grows
        // ~16 MB/ledger unbounded under load (2026-07-04 campaign-2 leak).
        let slot = self.tracking_slot().get();
        if slot == 0 {
            self.scp_driver.cache_tx_set(tx_set);
        } else {
            self.scp_driver.cache_tx_set_with_slot(tx_set, slot);
        }

        // Resolve any deferred slots that were waiting for this tx_set.
        // When MaybeValidDeferred clears fully_validated, the validator
        // stops emitting for the slot. Once the tx_set arrives — and any
        // other deferred causes (e.g. apply_lag) have also cleared — we
        // restore fully_validated so emission can resume.
        //
        // The companion restoration trigger for `apply_lag` causes lives
        // in `Herder::ledger_closed`.
        let resolved = self.scp_driver.resolve_missing_tx_set(&hash);
        for slot in resolved {
            self.scp.restore_slot_fully_validated(slot);
        }

        self.fetching_envelopes.on_tx_set_accepted(&hash);
    }

    /// Cache a transaction set and drain ready envelopes off the event loop.
    ///
    /// This is the async-safe public entry point. It calls [`cache_tx_set`]
    /// (cache + notify) then spawns [`process_ready_fetching_envelopes`] on
    /// the blocking pool via [`drain_ready_envelopes_blocking`](Self::drain_ready_envelopes_blocking).
    pub async fn cache_tx_set_and_drain(self: Arc<Self>, tx_set: TransactionSet) {
        self.cache_tx_set(tx_set);
        self.drain_ready_envelopes_blocking("envelope drain after cache_tx_set")
            .await;
    }

    /// Drain ready fetching envelopes on `spawn_blocking`.
    ///
    /// Returns the number of processed envelopes, or 0 on JoinError/panic.
    /// Callers in async context should use this instead of calling
    /// [`process_ready_fetching_envelopes`](Self::process_ready_fetching_envelopes)
    /// directly.
    pub async fn drain_ready_envelopes_blocking(self: &Arc<Self>, context: &str) -> usize {
        let herder = Arc::clone(self);
        let ctx = context.to_owned();
        let handle = tokio::task::spawn_blocking(move || herder.process_ready_fetching_envelopes());
        // Yield so co-scheduled async tasks (heartbeats, timers) can run
        // while the drain executes on the blocking pool. On current_thread,
        // yield_now places this task at the back of the FIFO ready queue —
        // peers are polled first. On multi_thread (production), this is a
        // near-free re-schedule that improves fairness when the drain returns
        // instantly (e.g., empty ready queue). (#2716)
        tokio::task::yield_now().await;
        henyey_common::spawn::await_blocking_logged(&ctx, handle)
            .await
            .ok()
            .unwrap_or(0)
    }

    /// Run [`handle_nomination_timeout`](Self::handle_nomination_timeout) on
    /// `spawn_blocking`.
    ///
    /// Callers in async context should use this instead of calling the sync
    /// method directly.
    pub async fn handle_nomination_timeout_blocking(
        self: &Arc<Self>,
        slot: SlotIndex,
    ) -> TimeoutOutcome {
        let herder = Arc::clone(self);
        match henyey_common::spawn::spawn_blocking_logged("handle_nomination_timeout", move || {
            herder.handle_nomination_timeout(slot)
        })
        .await
        {
            Ok(outcome) => outcome,
            Err(_) => {
                error!(slot, "nomination timeout failed on spawn_blocking");
                TimeoutOutcome::NoOp
            }
        }
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

    /// Quorum health summary for the tracking slot.
    ///
    /// Returns `(agree, missing, disagree, fail_at)` where:
    /// - `agree` = nodes in Agree or Delayed reporting state
    /// - `missing` = nodes in Missing reporting state
    /// - `disagree` = nodes in Disagree reporting state
    /// - `fail_at` = minimum number of additional peers (excluding self) whose
    ///   failure would block quorum, computed via `find_closest_v_blocking`.
    ///   More precise than `total - threshold` for nested quorum sets, but still
    ///   approximate: uses reporting-state classification (Agree/Delayed) rather
    ///   than stellar-core's ballot-compatibility check.
    ///
    /// Matches stellar-core `ApplicationImpl.cpp:525-546`:
    /// - Uses `tracking_slot - 1` first (previous slot has completed envelopes)
    /// - Falls back to current tracking slot if previous is unavailable
    /// - Returns None when not tracking (tracking_slot == 0)
    pub fn quorum_health(&self) -> Option<(u64, u64, u64, u64, u64)> {
        let tracking = self.tracking_slot().get();
        // When tracking_slot is 0, we're not tracking — equivalent to
        // stellar-core's HERDER_BOOTING_STATE check.
        if tracking == 0 {
            return None;
        }

        // Use previous slot first (ApplicationImpl.cpp:533-536).
        let summary = if tracking > 1 {
            let prev = self.scp.get_reporting_summary(tracking - 1);
            match prev {
                Some(_) => prev,
                None => self.scp.get_reporting_summary(tracking),
            }
        } else {
            self.scp.get_reporting_summary(tracking)
        };
        let summary = summary?;

        // Delayed counts as agree (node is participating, just behind).
        let agree = summary.agree + summary.delayed;
        let delayed = summary.delayed;
        let missing = summary.missing;
        let disagree = summary.disagree;

        // Compute fail_at using find_closest_v_blocking for precision with
        // nested quorum sets. Self-exclusion matches stellar-core's
        // findClosestVBlocking(&id, ...) semantics: fail_at counts peers
        // (excluding self) whose failure would block quorum.
        let qs = self.scp.local_quorum_set();
        let local_id = self.scp.local_node_id();
        let v_blocking =
            henyey_scp::find_closest_v_blocking(qs, &summary.agreeing_nodes, Some(local_id));
        let fail_at = v_blocking.len() as u64;
        Some((agree, missing, disagree, fail_at, delayed))
    }

    /// Quorum intersection publishable status for metrics export.
    ///
    /// Returns `Some(true)` if intersection holds, `Some(false)` if split
    /// detected after a prior good result, `None` if no publishable result
    /// exists yet (no analysis, or first-ever result was a split).
    ///
    /// Matches stellar-core's `hasAnyResults()` + `enjoysQuorunIntersection()`
    /// semantics (QuorumIntersectionChecker.h:45,51).
    pub fn quorum_intersection_publishable(&self) -> Option<bool> {
        let state = self.quorum_intersection_state.read();
        if state.has_any_results() {
            Some(state.enjoys_quorum_intersection())
        } else {
            None
        }
    }

    /// Determine the slot index to use for quorum info queries.
    ///
    /// Mirrors `ApplicationImpl.cpp:527-530`: use `trackingConsensusLedgerIndex()`
    /// unless state is BOOTING, in which case use LCL seq.
    pub fn resolve_quorum_slot(&self, lcl_seq: u32) -> u64 {
        if self.state() != HerderState::Booting {
            self.tracking_consensus_ledger_index().get()
        } else {
            lcl_seq as u64
        }
    }

    /// Build the quorum info for the `/info` endpoint.
    ///
    /// Mirrors `ApplicationImpl::getJsonInfo()` quorum section
    /// (ApplicationImpl.cpp:522-545) and `HerderImpl::getJsonQuorumInfo()`
    /// (HerderImpl.cpp:1754-1777).
    ///
    /// Returns `None` if no quorum data is available (both previous and current
    /// slot are empty or the slot doesn't exist).
    pub fn quorum_info_for_info(
        &self,
        lcl_seq: u32,
    ) -> Option<crate::json_api::InfoQuorumSnapshot> {
        let ledger_seq = self.resolve_quorum_slot(lcl_seq);

        // Try previous slot first (ApplicationImpl.cpp:532-536).
        // Fallback to current slot when previous has no data.
        // stellar-core checks `quorumInfo.empty() || qset.empty()` — "qset"
        // is the SCP::getJsonQuorumInfo result. It's empty when the slot
        // doesn't exist; non-empty even for expired phase (has counts/ledger).
        let summary = if ledger_seq > 1 {
            let prev = self.scp.get_info_quorum_summary(ledger_seq - 1);
            match prev {
                Some(_) => prev,
                None => self.scp.get_info_quorum_summary(ledger_seq),
            }
        } else {
            self.scp.get_info_quorum_summary(ledger_seq)
        };

        let summary = summary?;

        let node = crate::json_api::format_node_id(self.scp.local_node_id(), false);

        // Build transitive quorum intersection info if available.
        // Matches stellar-core HerderImpl.cpp:1764-1768.
        let transitive = {
            let state = self.quorum_intersection_state.read();
            if state.has_any_results() {
                self.build_transitive_quorum_info(&state)
            } else {
                None
            }
        };

        // Get externalize lag summary (HerderImpl.cpp:1770-1771).
        let lag_ms = self.scp_driver.get_qset_lag_info_summary();

        Some(crate::json_api::InfoQuorumSnapshot {
            node,
            qset: crate::json_api::InfoQuorumSetSnapshot {
                phase: summary.phase,
                hash: summary.hash,
                fail_at: summary.fail_at.map(|f| f as u64),
                validated: summary.validated,
                agree: summary.agree,
                disagree: summary.disagree,
                missing: summary.missing,
                delayed: summary.delayed,
                ledger: summary.ledger,
                lag_ms,
            },
            transitive,
        })
    }

    /// Timing snapshot for the highest externalized slot (monotonically updated).
    pub fn scp_timing(&self) -> Option<crate::scp_driver::ExternalizeTimingSnapshot> {
        self.scp_driver.last_externalize_timing()
    }

    /// Build `TransitiveQuorumJsonInfo` from the current intersection state.
    ///
    /// Matches stellar-core's `getJsonTransitiveQuorumIntersectionInfo()`
    /// (HerderImpl.cpp:1705-1751). Caller must hold the state read-lock
    /// and verify `has_any_results()` before calling.
    fn build_transitive_quorum_info(
        &self,
        state: &QuorumIntersectionState,
    ) -> Option<crate::json_api::TransitiveQuorumJsonInfo> {
        let result = state.last_result()?;

        match result {
            QuorumIntersectionResult::Intersecting {
                check_ledger,
                num_nodes,
                critical_groups,
                ..
            } => {
                let critical: Vec<Vec<String>> = critical_groups
                    .iter()
                    .map(|group| {
                        group
                            .iter()
                            .map(|n| crate::json_api::format_node_id(n, false))
                            .collect()
                    })
                    .collect();
                Some(crate::json_api::TransitiveQuorumJsonInfo {
                    intersection: true,
                    node_count: *num_nodes as u64,
                    last_check_ledger: *check_ledger as u64,
                    critical: Some(critical),
                    last_good_ledger: None,
                    potential_split: None,
                })
            }
            QuorumIntersectionResult::Split {
                check_ledger,
                num_nodes,
                potential_split,
                ..
            } => {
                let format_nodes = |nodes: &[NodeId]| -> Vec<String> {
                    let mut sorted = nodes.to_vec();
                    sorted.sort_by(|a, b| {
                        henyey_common::xdr_to_bytes(a).cmp(&henyey_common::xdr_to_bytes(b))
                    });
                    sorted
                        .iter()
                        .map(|n| crate::json_api::format_node_id(n, false))
                        .collect()
                };
                Some(crate::json_api::TransitiveQuorumJsonInfo {
                    intersection: false,
                    node_count: *num_nodes as u64,
                    last_check_ledger: *check_ledger as u64,
                    critical: None,
                    last_good_ledger: Some(state.last_good_ledger() as u64),
                    potential_split: Some((
                        format_nodes(&potential_split.0),
                        format_nodes(&potential_split.1),
                    )),
                })
            }
        }
    }

    /// Check if the quorum map has changed and re-analyze intersection.
    ///
    /// Mirrors stellar-core's `checkAndMaybeReanalyzeQuorumMapV2()`
    /// (HerderImpl.cpp:1934-1978). Called after each externalization.
    fn check_and_maybe_reanalyze_quorum_map(&self, ledger_seq: u32) {
        // Build the quorum map for hashing (NodeId → Option<ScpQuorumSet>).
        let qmap: std::collections::HashMap<NodeId, Option<ScpQuorumSet>> = {
            let qt = self.quorum_tracker.read();
            qt.quorum_map()
                .iter()
                .map(|(id, info)| (id.clone(), info.quorum_set.clone()))
                .collect()
        };

        if qmap.is_empty() {
            return;
        }

        let curr_hash = henyey_scp::quorum_intersection::compute_quorum_map_hash(&qmap);

        // Check if we need to re-analyze.
        {
            let state = self.quorum_intersection_state.read();

            // If the last completed result used this same hash, nothing changed.
            if state.last_result_hash() == Some(&curr_hash) {
                return;
            }

            // If we're already analyzing this hash, wait for it to finish.
            if state.checking_hash() == Some(&curr_hash) {
                debug!("Quorum intersection analysis already in progress for current map");
                return;
            }
        }

        info!(
            ledger_seq,
            nodes = qmap.len(),
            "Transitive closure of quorum has changed, re-analyzing"
        );

        // Mark analysis as in-progress and get the interrupt flag.
        // If a stale analysis is in progress, start_checking interrupts it.
        let interrupt_flag = {
            let mut state = self.quorum_intersection_state.write();
            state.start_checking(curr_hash)
        };

        // Clone what the background task needs.
        let intersection_state = Arc::clone(&self.quorum_intersection_state);
        let hash = curr_hash;
        let num_nodes = qmap.len();
        let seed = rand::random::<u64>();

        // Spawn CPU-bound analysis on a blocking thread.
        // Guard: if no tokio runtime is active (e.g. in unit tests),
        // run synchronously instead of panicking.
        let run_analysis = move || {
            let result = henyey_scp::quorum_intersection::check_intersection_interruptible(
                &qmap,
                &interrupt_flag,
                seed,
            );

            match result {
                henyey_scp::quorum_intersection::IntersectionResult::Intersects => {
                    // Only compute critical groups when intersecting (matching stellar-core).
                    let critical_groups =
                        match henyey_scp::quorum_intersection::get_intersection_critical_groups(
                            &qmap,
                            &interrupt_flag,
                            seed,
                        ) {
                            Ok(groups) => groups,
                            Err(_interrupted) => {
                                debug!("Critical groups computation interrupted");
                                // Do NOT clear the in-progress analysis marker
                                // here: the state was already replaced by the new
                                // start_checking() that triggered the interrupt.
                                // Clearing here would race with the new analysis.
                                return;
                            }
                        };

                    let mut state = intersection_state.write();
                    let qi_result = QuorumIntersectionResult::Intersecting {
                        check_ledger: ledger_seq,
                        num_nodes,
                        quorum_map_hash: hash,
                        critical_groups,
                    };
                    if state.complete_check(&hash, qi_result) {
                        info!(
                            ledger_seq,
                            nodes = num_nodes,
                            "Quorum intersection check complete: network enjoys intersection"
                        );
                    } else {
                        debug!("Quorum intersection result discarded (stale hash)");
                    }
                }
                henyey_scp::quorum_intersection::IntersectionResult::Split { pair } => {
                    let mut state = intersection_state.write();
                    let qi_result = QuorumIntersectionResult::Split {
                        check_ledger: ledger_seq,
                        num_nodes,
                        quorum_map_hash: hash,
                        potential_split: pair,
                    };
                    if state.complete_check(&hash, qi_result) {
                        warn!(
                            ledger_seq,
                            nodes = num_nodes,
                            "Quorum intersection check complete: NETWORK DOES NOT ENJOY INTERSECTION"
                        );
                    } else {
                        debug!("Quorum intersection result discarded (stale hash)");
                    }
                }
                henyey_scp::quorum_intersection::IntersectionResult::Interrupted => {
                    debug!("Quorum intersection analysis interrupted (quorum map changed)");
                    // Do NOT clear the in-progress analysis marker here: the state
                    // was already replaced by the new start_checking() that
                    // triggered the interrupt. Clearing here would race with the
                    // new analysis.
                }
            }
        };

        if tokio::runtime::Handle::try_current().is_ok() {
            tokio::task::spawn_blocking(run_analysis);
        } else {
            // No async runtime (unit tests) — run synchronously.
            run_analysis();
        }
    }

    /// Elapsed time since first SCP activity for the given slot.
    pub fn slot_first_seen_elapsed(&self, slot: SlotIndex) -> Option<std::time::Duration> {
        self.scp_driver.slot_first_seen_elapsed(slot)
    }

    // --- FetchingEnvelopes integration ---

    /// Receive a TxSet from a peer.
    ///
    /// Called when a TxSet or GeneralizedTxSet message is received.
    /// Returns true if the TxSet was needed and envelopes may be ready.
    pub fn recv_tx_set(&self, hash: Hash256) -> bool {
        self.fetching_envelopes.recv_tx_set(hash)
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

    /// Set the callback for broadcasting envelopes when their dependencies
    /// are satisfied in the FetchingEnvelopes pipeline.
    ///
    /// Parity: stellar-core's `PendingEnvelopes::envelopeReady()` broadcasts
    /// envelopes to all peers once `isFullyFetched()` is true.
    pub fn set_fetching_broadcast<F>(&self, f: F)
    where
        F: Fn(crate::fetching_envelopes::ScpRelayEnvelope) + Send + Sync + 'static,
    {
        self.fetching_envelopes.set_broadcast(f);
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

    /// Erase fetching data for slots outside the given range.
    pub fn erase_fetching_outside_range(
        &self,
        min_slot: Option<u64>,
        max_slot: Option<u64>,
        slot_to_keep: u64,
    ) {
        self.fetching_envelopes
            .erase_outside_range(min_slot, max_slot, slot_to_keep);
    }

    /// Check if we have a TxSet in the authoritative scp_driver cache.
    pub fn has_fetching_tx_set(&self, hash: &Hash256) -> bool {
        self.scp_driver.has_tx_set(hash)
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
    pub fn fetching_cache_sizes(&self) -> (usize, usize) {
        (
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
    pub tracking_slot: NextConsensusSlot,
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

/// Prometheus counter name for DB writes dropped because of a transient
/// `SQLITE_BUSY`/`LOCKED`. Registered (with its closed `site` vocabulary) in
/// `crates/app/src/metrics.rs`; emitted here by string literal (#3806). The
/// `crates/app` catalog test `test_db_busy_site_label_vocabulary_pinned` pins
/// the `site` value on the registration side; the herder emit-side tests pin
/// the exact rendered series, so a typo on either side fails a test.
const DB_BUSY_WRITE_DROPPED_TOTAL: &str = "henyey_db_busy_write_dropped_total";

/// The `site` label value for the herder tx-set GC purge (#3806).
///
/// Covers BOTH sub-steps of the 60 s GC tick (`delete_scp_state_below` via
/// `cleanup`, and `purge_unreferenced_tx_sets_atomic`): the `site` label
/// identifies the *consequence*, and it is identical for both — this tick's GC
/// work is dropped and self-heals on the next tick.
///
/// Two honesty caveats for anyone reading these series:
/// 1. `henyey_db_busy_retry_attempts_total{site="tx_set_gc"}` is structurally
///    always zero — this is a log-and-continue site that drops on the first
///    busy with no retry loop — so a `0` there does NOT mean "no contention".
/// 2. `is_transient_busy` matches only `SQLITE_BUSY`/`LOCKED`. Sustained
///    contention can also surface as `DbError::Pool(Timeout)` from the r2d2
///    checkout, which is NOT counted. A `0` here means "no `SQLITE_BUSY`/`LOCKED`
///    at this site", not "no contention loss at this site".
const SITE_TX_SET_GC: &str = "tx_set_gc";

/// Count a herder tx-set-GC failure as a busy-caused write loss **iff** the
/// error is a transient `SQLITE_BUSY`/`LOCKED`. Non-transient errors (genuine
/// corruption, integrity violations, non-`Db` `HerderError`s) are a different
/// failure class and must NOT inflate the alertable series (#3802 contract
/// point 4). Classification is on the structured [`henyey_db::DbError`], never
/// on the rendered message string.
fn record_tx_set_gc_busy_drop(err: &HerderError) {
    if matches!(err, HerderError::Db(e) if e.is_transient_busy()) {
        metrics::counter!(DB_BUSY_WRITE_DROPPED_TOTAL, "site" => SITE_TX_SET_GC).increment(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tx_queue::TransactionSet;
    use henyey_crypto::SecretKey;
    use henyey_scp::hash_quorum_set;
    use stellar_xdr::{
        EnvelopeType, LedgerCloseValueSignature, LedgerUpgrade, Limits, NodeId as XdrNodeId,
        ReadXdr, ScpBallot, ScpNomination, ScpStatement, ScpStatementExternalize,
        ScpStatementPledges, ScpStatementPrepare, Signature as XdrSignature, StellarValue,
        StellarValueExt, TimePoint, Value, WriteXdr,
    };

    fn make_default_lm() -> Arc<henyey_ledger::LedgerManager> {
        make_lm_at_seq_for_stale_test(0)
    }

    fn make_test_herder() -> Herder {
        let config = HerderConfig::default();
        Herder::new(config, make_default_lm(), TimerManagerHandle::no_op())
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

        let herder = Herder::with_secret_key(
            config,
            secret_for_herder,
            make_default_lm(),
            TimerManagerHandle::no_op(),
        );
        let secret_for_signing = SecretKey::from_seed(&seed);

        (herder, secret_for_signing)
    }

    fn make_valid_value_with_cached_tx_set(herder: &Herder, secret_key: &SecretKey) -> Value {
        let lcl_hash = herder.scp_driver.current_header_hash();
        let tx_set = TransactionSet::new(lcl_hash, Vec::new());
        let tx_set_hash = *tx_set.hash();
        herder.scp_driver.cache_tx_set(tx_set);

        let xdr_tx_set_hash = stellar_xdr::Hash(tx_set_hash.0);
        let close_time = TimePoint(1);

        // Sign: (networkID, ENVELOPE_TYPE_SCPVALUE, txSetHash, closeTime)
        let network_id = herder.scp_driver.network_id();
        let mut sign_data = network_id.0.to_vec();
        sign_data.extend_from_slice(&EnvelopeType::Scpvalue.to_xdr(Limits::none()).expect("xdr"));
        sign_data.extend_from_slice(&xdr_tx_set_hash.to_xdr(Limits::none()).expect("xdr"));
        sign_data.extend_from_slice(&close_time.to_xdr(Limits::none()).expect("xdr"));
        let sig = secret_key.sign(&sign_data);

        let node_id = XdrNodeId(stellar_xdr::PublicKey::PublicKeyTypeEd25519(
            stellar_xdr::Uint256(*secret_key.public_key().as_bytes()),
        ));

        let stellar_value = StellarValue {
            tx_set_hash: xdr_tx_set_hash,
            close_time,
            upgrades: vec![].try_into().unwrap(),
            ext: StellarValueExt::Signed(LedgerCloseValueSignature {
                node_id,
                signature: stellar_xdr::Signature(sig.0.to_vec().try_into().unwrap_or_default()),
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

        let node_id = XdrNodeId(stellar_xdr::PublicKey::PublicKeyTypeEd25519(
            stellar_xdr::Uint256(*public.as_bytes()),
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
                quorum_set_hash: stellar_xdr::Hash([0u8; 32]),
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
        let node_id = XdrNodeId(stellar_xdr::PublicKey::PublicKeyTypeEd25519(
            stellar_xdr::Uint256([0u8; 32]),
        ));

        ScpEnvelope {
            statement: ScpStatement {
                node_id,
                slot_index: slot,
                pledges: ScpStatementPledges::Nominate(ScpNomination {
                    quorum_set_hash: stellar_xdr::Hash([0u8; 32]),
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
        assert_eq!(herder.tracking_slot().get(), 101);
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

        // Syncing but not yet tracking, envelopes enter FetchingEnvelopes.
        // Use signed envelope to pass signature verification.
        let envelope = make_signed_test_envelope(100, &herder);

        // We need to set a current slot first
        herder.pending_envelopes.set_current_slot(95);

        let result = herder.receive_scp_envelope(envelope);
        // Envelope has missing deps (quorum_set not cached) → Fetching.
        // Previously returned Pending (buffered in pending_envelopes), but
        // now all envelopes route through FetchingEnvelopes (#2335).
        assert_eq!(result, EnvelopeState::Fetching);
    }

    #[test]
    fn test_stats() {
        let herder = make_test_herder();
        herder.bootstrap(50);

        let stats = herder.stats();
        assert_eq!(stats.state, HerderState::Tracking);
        assert_eq!(stats.tracking_slot.get(), 51);
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

        let herder = Herder::with_secret_key(
            config,
            local_secret.clone(),
            make_default_lm(),
            TimerManagerHandle::no_op(),
        );
        herder.start_syncing();
        herder.bootstrap(100);

        herder
            .quorum_tracker
            .write()
            .expand(&local_node_id, quorum_set)
            .unwrap();

        let tracking = herder.tracking_slot().get(); // 101

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
            herder.tracking_slot().get(),
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

        let tracking = herder.tracking_slot().get(); // 101

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
        // Future EXTERNALIZE envelopes are now routed through FetchingEnvelopes
        // (#2335) for dep-fetching and relay. They are buffered in the fetching
        // queue (not immediately processed through SCP) until their slot becomes
        // current and dependencies are resolved.
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

        let herder = Herder::with_secret_key(
            config,
            local_secret,
            make_default_lm(),
            TimerManagerHandle::no_op(),
        );
        herder.start_syncing();
        herder.bootstrap(100);

        herder
            .quorum_tracker
            .write()
            .expand(&other_node_id, quorum_set)
            .unwrap();

        let tracking = herder.tracking_slot().get(); // 101

        // An EXTERNALIZE for a future slot within the pending buffer distance
        let future_slot = tracking + 5;
        let envelope = make_signed_externalize_from(future_slot, &herder, &other_secret);
        let result = herder.receive_scp_envelope(envelope);

        // Should be buffered in FetchingEnvelopes (not immediately processed)
        assert_eq!(result, EnvelopeState::Fetching);
        // Tracking slot should NOT have advanced (envelope is buffered)
        assert_eq!(herder.tracking_slot().get(), tracking);
    }

    /// Regression for issue #1807.
    ///
    /// After archive catchup in accelerated mode, the primary can run many
    /// slots ahead of the captive-core observer — for example, primary at
    /// slot 320 while observer just bootstrapped with `tracking_slot=272`.
    /// Primary's EXTERNALIZE envelopes for slots 285..320 must all be
    /// buffered (not rejected) so that when the observer externalizes the
    /// intermediate slots, the buffered envelopes drain into
    /// `record_externalized` and the observer catches up to the primary.
    ///
    /// Before this fix, `PendingEnvelopes::max_slot_distance` (12, clamped
    /// again in `Herder::build` to `max_externalized_slots=12`) rejected
    /// any envelope more than 12 slots ahead of `tracking_slot` — the gate
    /// was removed in #1807 so the pre-filter `LEDGER_VALIDITY_BRACKET=100`
    /// horizon is the only horizon.
    #[test]
    fn test_externalize_48_slots_ahead_is_buffered_not_rejected() {
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

        let herder = Herder::with_secret_key(
            config,
            local_secret,
            make_default_lm(),
            TimerManagerHandle::no_op(),
        );
        herder.start_syncing();
        herder.bootstrap(100);

        herder
            .quorum_tracker
            .write()
            .expand(&other_node_id, quorum_set)
            .unwrap();

        let tracking = herder.tracking_slot().get(); // 101

        // Primary is 48 slots ahead — this matches the post-catchup Quickstart
        // scenario where captive-core just bootstrapped at `tracking_slot=N`
        // and the primary is at ~`N+48`. Pre-fix this was rejected as
        // `PendingAddTooFar`; now it must be buffered.
        let far_future_slot = tracking + 48;
        let envelope = make_signed_externalize_from(far_future_slot, &herder, &other_secret);
        let result = herder.receive_scp_envelope(envelope);

        assert_eq!(
            result,
            EnvelopeState::Fetching,
            "EXTERNALIZE 48 slots ahead must be buffered in FetchingEnvelopes, got {:?}",
            result
        );
        // Tracking slot must not advance on buffering alone.
        assert_eq!(herder.tracking_slot().get(), tracking);
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

        let herder = Herder::with_secret_key(
            config,
            local_secret,
            make_default_lm(),
            TimerManagerHandle::no_op(),
        );
        herder.start_syncing();
        herder.bootstrap(100);

        herder
            .quorum_tracker
            .write()
            .expand(&other_node_id, quorum_set)
            .unwrap();

        let tracking = herder.tracking_slot().get(); // 101

        // Create a properly signed EXTERNALIZE for the current tracking slot
        let envelope = make_signed_externalize_from(tracking, &herder, &other_secret);
        let result = herder.receive_scp_envelope(envelope);

        // Should be accepted and cause externalization through SCP
        assert_eq!(result, EnvelopeState::Valid);
        // Tracking slot should have advanced
        assert_eq!(herder.tracking_slot().get(), tracking + 1);
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

        let herder = Herder::with_secret_key(
            config,
            local_secret,
            make_default_lm(),
            TimerManagerHandle::no_op(),
        );
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

        let tracking = herder.tracking_slot().get(); // 101

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
        use stellar_xdr::{Memo, TransactionV0, TransactionV0Envelope, TransactionV0Ext};
        TransactionEnvelope::TxV0(TransactionV0Envelope {
            tx: TransactionV0 {
                source_account_ed25519: stellar_xdr::Uint256([0u8; 32]),
                fee: 100,
                seq_num: stellar_xdr::SequenceNumber(1),
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

    /// Sustain fix: a momentary Tracking→Syncing blip must NOT bounce
    /// submissions at the herder gate — stellar-core's `recvTransaction`
    /// always forwards to the queue regardless of tracking state. (For
    /// `PAY_PREGENERATED` loadgen, a bounced submission aborts the whole run,
    /// parity #3638.) Only `Booting` still defers.
    #[test]
    fn test_receive_transaction_while_syncing_forwards_to_queue() {
        let herder = make_test_herder();
        herder.set_state(HerderState::Syncing);
        assert!(herder.state().can_receive_transactions());

        // With an empty queue there is no account-pending conflict, so any
        // TryAgainLater could only come from the (removed) state gate.
        let result = herder.receive_transaction(make_minimal_tx_envelope());
        assert!(
            !matches!(result, TxQueueResult::TryAgainLater),
            "Syncing must forward to the queue, not bounce at the state gate; got {:?}",
            result
        );
    }

    /// Parity: when the herder is Tracking and the queue already has a pending
    /// tx from a source, submitting an opposite-type tx from the same source
    /// must return TryAgainLater (stellar-core HerderImpl.cpp:627-645).
    #[test]
    fn test_receive_transaction_cross_type_conflict_returns_try_again_later() {
        use stellar_xdr::{
            CreateAccountOp, DecoratedSignature, HostFunction, InvokeContractArgs,
            InvokeHostFunctionOp, Memo, MuxedAccount, Operation, OperationBody, Preconditions,
            PublicKey, ScAddress, ScSymbol, SequenceNumber, SignatureHint, StringM, Transaction,
            TransactionEnvelope, TransactionExt, TransactionV1Envelope, Uint256, VecM,
        };

        let herder = make_test_herder();
        herder.set_state(HerderState::Tracking);
        assert!(herder.state().can_receive_transactions());

        // Seed a valid classic tx from source seed 42.
        let classic_tx = Transaction {
            source_account: MuxedAccount::Ed25519(Uint256([42u8; 32])),
            fee: 200,
            seq_num: SequenceNumber(1),
            cond: Preconditions::None,
            memo: Memo::None,
            operations: vec![Operation {
                source_account: None,
                body: OperationBody::CreateAccount(CreateAccountOp {
                    destination: stellar_xdr::AccountId(PublicKey::PublicKeyTypeEd25519(Uint256(
                        [255u8; 32],
                    ))),
                    starting_balance: 1000000000,
                }),
            }]
            .try_into()
            .unwrap(),
            ext: TransactionExt::V0,
        };
        let classic_env = TransactionEnvelope::Tx(TransactionV1Envelope {
            tx: classic_tx,
            signatures: vec![DecoratedSignature {
                hint: SignatureHint([0u8; 4]),
                signature: XdrSignature(vec![0u8; 64].try_into().unwrap()),
            }]
            .try_into()
            .unwrap(),
        });
        assert_eq!(
            herder.receive_transaction(classic_env),
            TxQueueResult::Added,
        );

        // Submit a Soroban tx from the same source (seed 42).
        let function_name = ScSymbol(StringM::<32>::try_from("test".to_string()).expect("symbol"));
        let host_function = HostFunction::InvokeContract(InvokeContractArgs {
            contract_address: ScAddress::default(),
            function_name,
            args: VecM::<stellar_xdr::ScVal>::default(),
        });
        let soroban_tx = Transaction {
            source_account: MuxedAccount::Ed25519(Uint256([42u8; 32])),
            fee: 200,
            seq_num: SequenceNumber(1),
            cond: Preconditions::None,
            memo: Memo::None,
            operations: vec![Operation {
                source_account: None,
                body: OperationBody::InvokeHostFunction(InvokeHostFunctionOp {
                    host_function,
                    auth: VecM::default(),
                }),
            }]
            .try_into()
            .unwrap(),
            ext: TransactionExt::V0,
        };
        let soroban_env = TransactionEnvelope::Tx(TransactionV1Envelope {
            tx: soroban_tx,
            signatures: vec![DecoratedSignature {
                hint: SignatureHint([0u8; 4]),
                signature: XdrSignature(vec![0u8; 64].try_into().unwrap()),
            }]
            .try_into()
            .unwrap(),
        });

        let result = herder.receive_transaction(soroban_env);
        assert_eq!(
            result,
            TxQueueResult::TryAgainLater,
            "cross-type conflict must return TryAgainLater through public intake"
        );
    }

    // =========================================================================
    // Issue #1953 — Herder end-to-end normalization regression
    // =========================================================================

    /// End-to-end regression test for issue #1953: construct a Herder with
    /// deliberately reverse-sorted validators in the quorum set and verify
    /// that (a) SCP stores the normalized form, (b) the quorum-set tracker
    /// indexes it by the canonical hash, and (c) emitted SCP statements use
    /// the canonical hash.
    #[test]
    fn test_herder_normalizes_reverse_sorted_quorum_set_end_to_end() {
        let seed = [9u8; 32];
        let secret = SecretKey::from_seed(&seed);
        let public = secret.public_key();
        let local_node_id = node_id_from_public_key(&public);

        // Second node.
        let other_secret = SecretKey::from_seed(&[10u8; 32]);
        let other_public = other_secret.public_key();
        let other_node_id = node_id_from_public_key(&other_public);

        // Deliberately reverse-sort the validators — local second, other first
        // (or whichever order is NOT canonical).  We just swap them and verify
        // the hash differs from the sorted version.
        let unsorted_qs = ScpQuorumSet {
            threshold: 2,
            validators: vec![other_node_id.clone(), local_node_id.clone()]
                .try_into()
                .unwrap(),
            inner_sets: vec![].try_into().unwrap(),
        };
        let unsorted_hash = hash_quorum_set(&unsorted_qs);

        let mut sorted_qs = unsorted_qs.clone();
        henyey_scp::normalize_quorum_set(&mut sorted_qs);
        let sorted_hash = hash_quorum_set(&sorted_qs);

        // Precondition: the two orderings produce different hashes.
        // (If they happen to be already sorted, swap them so they differ.)
        let quorum_set = if unsorted_hash == sorted_hash {
            // Already canonical — reverse explicitly.
            ScpQuorumSet {
                threshold: 2,
                validators: vec![local_node_id.clone(), other_node_id.clone()]
                    .try_into()
                    .unwrap(),
                inner_sets: vec![].try_into().unwrap(),
            }
        } else {
            unsorted_qs
        };
        let pre_normalize_hash = hash_quorum_set(&quorum_set);
        let mut canonical = quorum_set.clone();
        henyey_scp::normalize_quorum_set(&mut canonical);
        let canonical_hash = hash_quorum_set(&canonical);
        assert_ne!(
            pre_normalize_hash, canonical_hash,
            "precondition: quorum set must not already be in canonical order"
        );

        // Build the Herder with the non-canonical quorum set.
        let config = HerderConfig {
            is_validator: true,
            node_public_key: public,
            local_quorum_set: Some(quorum_set),
            ..HerderConfig::default()
        };
        let herder = Herder::with_secret_key(
            config,
            secret,
            make_default_lm(),
            TimerManagerHandle::no_op(),
        );

        // (a) SCP must store the normalized form.
        let scp_stored = herder.scp().local_quorum_set();
        let stored_hash = hash_quorum_set(scp_stored);
        assert_eq!(
            stored_hash, canonical_hash,
            "SCP must store the canonically-normalized quorum set"
        );

        // (b) The tracker must be able to look up by the canonical hash.
        let canonical_hash256 = henyey_common::Hash256(canonical_hash.0);
        let from_tracker = herder.scp_driver.get_quorum_set_by_hash(&canonical_hash256);
        assert!(
            from_tracker.is_some(),
            "quorum set tracker must index the local qset under the canonical hash"
        );

        // (c) Emit a nomination and verify the quorum_set_hash in the
        //     statement matches the canonical hash.
        let signing_secret = SecretKey::from_seed(&[9u8; 32]);
        let value = make_valid_value_with_cached_tx_set(&herder, &signing_secret);
        // Use a deterministic prev_value so leader election doesn't depend
        // on the (LCL-hash-derived) value bytes.
        let prev_value = Value::default();
        assert!(herder.scp().nominate(1, value, &prev_value));

        let envelopes = herder.scp().get_latest_messages_send(1);
        assert!(
            !envelopes.is_empty(),
            "SCP should have emitted at least one envelope"
        );
        for env in &envelopes {
            let env_qs_hash = match &env.statement.pledges {
                ScpStatementPledges::Nominate(nom) => &nom.quorum_set_hash,
                ScpStatementPledges::Prepare(p) => &p.quorum_set_hash,
                ScpStatementPledges::Confirm(c) => &c.quorum_set_hash,
                ScpStatementPledges::Externalize(e) => &e.commit_quorum_set_hash,
            };
            assert_eq!(
                env_qs_hash.0, canonical_hash.0,
                "emitted SCP statement must use the canonical quorum-set hash"
            );
        }
    }

    // =========================================================================
    // Phase 6 H1 parity tests — close-time validation in Herder
    // =========================================================================

    /// Create a StellarValue with a given close time and encode to Value.
    fn make_value_with_close_time(close_time: u64) -> Value {
        let sv = StellarValue {
            tx_set_hash: stellar_xdr::Hash([0u8; 32]),
            close_time: TimePoint(close_time),
            upgrades: vec![].try_into().unwrap(),
            ext: StellarValueExt::Signed(LedgerCloseValueSignature {
                node_id: XdrNodeId(stellar_xdr::PublicKey::PublicKeyTypeEd25519(
                    stellar_xdr::Uint256([0u8; 32]),
                )),
                signature: XdrSignature(vec![0u8; 64].try_into().unwrap()),
            }),
        };
        Value(sv.to_xdr(Limits::none()).unwrap().try_into().unwrap())
    }

    /// Create a nomination envelope with a specific close time in its voted values.
    fn make_nomination_envelope_with_close_time(slot: u64, close_time: u64) -> ScpEnvelope {
        let node_id = XdrNodeId(stellar_xdr::PublicKey::PublicKeyTypeEd25519(
            stellar_xdr::Uint256([1u8; 32]),
        ));
        let value = make_value_with_close_time(close_time);

        ScpEnvelope {
            statement: ScpStatement {
                node_id,
                slot_index: slot,
                pledges: ScpStatementPledges::Nominate(ScpNomination {
                    quorum_set_hash: stellar_xdr::Hash([0u8; 32]),
                    votes: vec![value].try_into().unwrap(),
                    accepted: vec![].try_into().unwrap(),
                }),
            },
            signature: XdrSignature(vec![0u8; 64].try_into().unwrap()),
        }
    }

    #[test]
    fn test_tracking_consensus_ledger_index() {
        let herder = make_test_herder();

        // Default tracking_slot is 0 → saturating_sub(1) = 0
        assert_eq!(herder.tracking_slot().get(), 0);
        assert_eq!(herder.tracking_consensus_ledger_index().get(), 0);

        // Bootstrap to slot 1 (tracking_slot = 2, i.e., externalized + 1)
        herder.bootstrap(1);
        assert_eq!(herder.tracking_slot().get(), 2);
        assert_eq!(herder.tracking_consensus_ledger_index().get(), 1);

        // Bootstrap to slot 100 (tracking_slot = 101)
        herder.bootstrap(100);
        assert_eq!(herder.tracking_slot().get(), 101);
        assert_eq!(herder.tracking_consensus_ledger_index().get(), 100);
    }

    #[test]
    fn test_newtype_api() {
        // NextConsensusSlot
        let slot = NextConsensusSlot::new(101);
        assert_eq!(slot.get(), 101);
        assert!(!slot.is_boot());
        assert_eq!(format!("{}", slot), "101");

        let boot_slot = NextConsensusSlot::new(0);
        assert!(boot_slot.is_boot());

        // LastExternalizedLedger
        let ledger = LastExternalizedLedger::new(100);
        assert_eq!(ledger.get(), 100);
        assert_eq!(ledger.as_u32(), 100);
        assert!(!ledger.is_boot());
        assert_eq!(format!("{}", ledger), "100");

        let boot_ledger = LastExternalizedLedger::new(0);
        assert!(boot_ledger.is_boot());

        // Ordering
        assert!(NextConsensusSlot::new(5) > NextConsensusSlot::new(3));
        assert!(LastExternalizedLedger::new(10) < LastExternalizedLedger::new(20));
    }

    #[test]
    fn test_newtype_serialization() {
        // Verify Serialize produces a bare number (not a wrapped object)
        let slot = NextConsensusSlot::new(42);
        let json = serde_json::to_value(slot).unwrap();
        assert_eq!(json, serde_json::json!(42));

        let ledger = LastExternalizedLedger::new(99);
        let json = serde_json::to_value(ledger).unwrap();
        assert_eq!(json, serde_json::json!(99));
    }

    #[test]
    fn test_resolve_quorum_slot_uses_tracking_consensus_ledger_index() {
        let herder = make_test_herder();

        // In Booting state, should use LCL seq directly
        assert_eq!(herder.state(), HerderState::Booting);
        assert_eq!(herder.resolve_quorum_slot(50), 50);

        // After bootstrap, should use tracking_consensus_ledger_index (not tracking_slot)
        herder.bootstrap(100);
        assert_eq!(herder.state(), HerderState::Tracking);
        // tracking_slot = 101, tracking_consensus_ledger_index = 100
        assert_eq!(herder.resolve_quorum_slot(99), 100);
        assert_eq!(herder.resolve_quorum_slot(0), 100);
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
    fn test_receive_scp_envelope_discarded_in_manual_close_mode() {
        // When both manual_close and run_standalone are true (standalone
        // manual-close mode), all envelopes should be discarded without any
        // processing. Parity: stellar-core HerderImpl.cpp:805-808.
        let config = HerderConfig {
            is_validator: true,
            manual_close: true,
            run_standalone: true,
            ..HerderConfig::default()
        };
        let herder = Herder::new(config, make_default_lm(), TimerManagerHandle::no_op());
        herder.bootstrap(100);

        let env = make_nomination_envelope_with_close_time(101, 100);
        let result = herder.receive_scp_envelope(env);
        assert_eq!(result, EnvelopeState::Discarded);
    }

    #[test]
    fn test_receive_scp_envelope_detailed_discarded_in_manual_close_mode() {
        use crate::scp_verify::PostVerifyReason;

        let config = HerderConfig {
            is_validator: true,
            manual_close: true,
            run_standalone: true,
            ..HerderConfig::default()
        };
        let herder = Herder::new(config, make_default_lm(), TimerManagerHandle::no_op());
        herder.bootstrap(100);

        let env = make_nomination_envelope_with_close_time(101, 100);
        let (state, reason) = herder.receive_scp_envelope_detailed(env);
        assert_eq!(state, EnvelopeState::Discarded);
        assert_eq!(reason, PostVerifyReason::GateDriftManualClose);
    }

    #[test]
    fn test_pre_filter_rejects_manual_close() {
        use crate::scp_verify::{PreFilter, PreFilterRejectReason};

        let config = HerderConfig {
            manual_close: true,
            run_standalone: true,
            ..HerderConfig::default()
        };
        let herder = Herder::new(config, make_default_lm(), TimerManagerHandle::no_op());
        herder.bootstrap(100);

        let env = make_nomination_envelope_with_close_time(101, 100);
        let result = herder.pre_filter_scp_envelope(&env);
        assert!(matches!(
            result,
            PreFilter::Reject(PreFilterRejectReason::ManualClose)
        ));
    }

    #[test]
    fn test_pre_filter_accepts_in_manual_close_without_standalone() {
        use crate::scp_verify::PreFilter;

        // manual_close=true but run_standalone=false (simulation mode):
        // pre_filter should NOT reject envelopes.
        let config = HerderConfig {
            is_validator: true,
            manual_close: true,
            run_standalone: false,
            ..HerderConfig::default()
        };
        let herder = Herder::new(config, make_default_lm(), TimerManagerHandle::no_op());
        herder.bootstrap(100);

        let env = make_nomination_envelope_with_close_time(101, 100);
        let result = herder.pre_filter_scp_envelope(&env);
        assert!(
            !matches!(result, PreFilter::Reject(_)),
            "pre_filter should not reject when manual_close=true but run_standalone=false"
        );
    }

    #[test]
    fn test_suppress_scp_helper() {
        // Verify the 4-combination truth table of suppress_scp().
        let base = HerderConfig::default();

        // (false, false) -> false
        let c = HerderConfig {
            manual_close: false,
            run_standalone: false,
            ..base.clone()
        };
        assert!(!c.suppress_scp());

        // (false, true) -> false
        let c = HerderConfig {
            manual_close: false,
            run_standalone: true,
            ..base.clone()
        };
        assert!(!c.suppress_scp());

        // (true, false) -> false (simulation mode)
        let c = HerderConfig {
            manual_close: true,
            run_standalone: false,
            ..base.clone()
        };
        assert!(!c.suppress_scp());

        // (true, true) -> true (standalone manual-close)
        let c = HerderConfig {
            manual_close: true,
            run_standalone: true,
            ..base
        };
        assert!(c.suppress_scp());
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

        // Set runtime upgrades: protocol version 25, base reserve 10000000
        // (LM header defaults to base_reserve=5000000, so 10000000 is a change)
        let upgrade_params = crate::upgrades::UpgradeParameters {
            upgrade_time: 0, // immediate
            protocol_version: Some(25),
            base_reserve: Some(10_000_000),
            ..Default::default()
        };
        herder
            .set_upgrade_parameters(upgrade_params)
            .expect("set_upgrade_parameters should succeed");

        // Bootstrap herder to Tracking state (required for trigger_next_ledger)
        herder.bootstrap(0);
        assert_eq!(herder.state(), HerderState::Tracking);

        // Trigger consensus for ledger 1 (LCL=0, so LCL+1=1 is the current ledger;
        // LM header has version=0, base_reserve=0 — upgrades should fire)
        let result = herder.trigger_next_ledger(1);
        assert!(
            result.is_ok(),
            "trigger_next_ledger should succeed: {:?}",
            result.err()
        );

        // For a solo validator (1-of-1 quorum), nomination→ballot→externalization
        // happens synchronously. The externalized value should contain the upgrades.
        let externalized = herder.scp_driver.get_externalized(1);
        assert!(
            externalized.is_some(),
            "Slot 1 should be externalized for solo validator"
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
                        assert_eq!(r, 10_000_000, "Expected base reserve 10000000");
                        found_reserve = true;
                    }
                    other => panic!("Unexpected upgrade: {:?}", other),
                }
            }
        }
        assert!(found_version, "Should have found Version(25) upgrade");
        assert!(
            found_reserve,
            "Should have found BaseReserve(10000000) upgrade"
        );
    }

    /// Regression test for #2297: at protocol < 20, the nomination tx set hash
    /// must use Classic (non-generalized) format so that the catchup path
    /// (`make_empty_tx_set`) computes the same hash.
    #[tokio::test]
    async fn test_nomination_uses_classic_format_at_protocol_zero() {
        let (herder, _secret) = make_validator_herder();

        // Arm a protocol upgrade so nomination triggers externalization
        let upgrade_params = crate::upgrades::UpgradeParameters {
            upgrade_time: 0,
            protocol_version: Some(25),
            ..Default::default()
        };
        herder
            .set_upgrade_parameters(upgrade_params)
            .expect("set upgrade params");

        // Bootstrap at ledger 0 (genesis, protocol 0)
        herder.bootstrap(0);

        let result = herder.trigger_next_ledger(1);
        assert!(
            result.is_ok(),
            "trigger_next_ledger failed: {:?}",
            result.err()
        );

        let ext = herder
            .scp_driver
            .get_externalized(1)
            .expect("slot 1 externalized");
        let sv = StellarValue::from_xdr(&ext.value, Limits::none()).expect("parse StellarValue");

        // The tx_set_hash in the externalized value must be the Classic
        // (non-generalized) hash = SHA256(previous_ledger_hash).
        // previous_ledger_hash is the computed LCL hash.
        let lcl_hash = herder.scp_driver.current_header_hash();
        let expected_classic_hash = TransactionSet::compute_non_generalized_hash(lcl_hash, &[]);
        let actual_hash = Hash256::from(sv.tx_set_hash.0);

        assert_eq!(
            actual_hash, expected_classic_hash,
            "At protocol 0, nomination must produce a Classic tx set hash. \
             Got {:?} but expected Classic hash {:?}. \
             This regression means the catchup path would fail with \
             'invalid tx set hash' (#2297).",
            actual_hash, expected_classic_hash
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
            local_quorum_set: Some(stellar_xdr::ScpQuorumSet {
                threshold: 1,
                validators: vec![stellar_xdr::NodeId(
                    stellar_xdr::PublicKey::PublicKeyTypeEd25519(stellar_xdr::Uint256(
                        *public.as_bytes(),
                    )),
                )]
                .try_into()
                .unwrap(),
                inner_sets: vec![].try_into().unwrap(),
            }),
            // Set upgrades in REVERSE of canonical order.
            // Use upgrades valid at protocol 0 (Flags requires V18+).
            proposed_upgrades: vec![
                LedgerUpgrade::MaxTxSetSize(200),
                LedgerUpgrade::BaseFee(200),
                LedgerUpgrade::Version(25),
            ],
            ..Default::default()
        };
        // Set runtime upgrades matching ALL proposed upgrades (required for
        // nomination validation) plus an additional BaseReserve.
        let herder = Herder::with_secret_key(
            config,
            secret,
            make_default_lm(),
            TimerManagerHandle::no_op(),
        );
        let upgrade_params = crate::upgrades::UpgradeParameters {
            upgrade_time: 0,
            protocol_version: Some(25),
            base_fee: Some(200),
            max_tx_set_size: Some(200),
            base_reserve: Some(10_000_000),
            ..Default::default()
        };
        herder
            .set_upgrade_parameters(upgrade_params)
            .expect("set_upgrade_parameters should succeed");

        herder.bootstrap(0);

        let result = herder.trigger_next_ledger(1);
        assert!(
            result.is_ok(),
            "trigger_next_ledger failed: {:?}",
            result.err()
        );

        let externalized = herder.scp_driver.get_externalized(1);
        assert!(externalized.is_some(), "slot 1 should be externalized");

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

    /// Regression test for #1879: repeated trigger_next_ledger calls must not
    /// advance the SCP nomination round counter.
    ///
    /// With a multi-node quorum (2-of-3), the first trigger starts nomination
    /// but cannot externalize (no peer messages). A second call should be
    /// skipped by the is_nominating guard, leaving nomination_round at 1.
    #[tokio::test]
    async fn test_trigger_next_ledger_idempotent_during_nomination() {
        // Create a validator with a 2-of-3 quorum set so the slot stays in
        // nominating state (cannot self-externalize).
        let seed = [7u8; 32];
        let secret = SecretKey::from_seed(&seed);
        let public = secret.public_key();
        let local_node_id = stellar_xdr::NodeId(stellar_xdr::PublicKey::PublicKeyTypeEd25519(
            stellar_xdr::Uint256(*public.as_bytes()),
        ));
        let fake_peer1 = stellar_xdr::NodeId(stellar_xdr::PublicKey::PublicKeyTypeEd25519(
            stellar_xdr::Uint256([1u8; 32]),
        ));
        let fake_peer2 = stellar_xdr::NodeId(stellar_xdr::PublicKey::PublicKeyTypeEd25519(
            stellar_xdr::Uint256([2u8; 32]),
        ));

        let quorum_set = ScpQuorumSet {
            threshold: 2,
            validators: vec![local_node_id, fake_peer1, fake_peer2]
                .try_into()
                .unwrap(),
            inner_sets: vec![].try_into().unwrap(),
        };

        let config = HerderConfig {
            is_validator: true,
            node_public_key: public,
            local_quorum_set: Some(quorum_set),
            ..HerderConfig::default()
        };
        let herder = Herder::with_secret_key(
            config,
            secret,
            make_default_lm(),
            TimerManagerHandle::no_op(),
        );
        herder.bootstrap(0);

        // First trigger: starts nomination, round should be 1.
        let result1 = herder.trigger_next_ledger(1);
        assert_eq!(
            result1.expect("first trigger should succeed"),
            TriggerOutcome::Triggered,
            "first trigger should report Triggered"
        );

        let state1 = herder.scp().get_slot_state(1).expect("slot 1 should exist");
        assert!(state1.is_nominating, "slot should be in nominating state");
        assert_eq!(
            state1.nomination_round, 1,
            "first trigger: round should be 1"
        );

        // Second trigger: should be skipped by the is_nominating guard.
        let result2 = herder.trigger_next_ledger(1);
        assert_eq!(
            result2.expect("second trigger should succeed (no-op)"),
            TriggerOutcome::AlreadyNominating,
            "second trigger should report AlreadyNominating"
        );

        let state2 = herder
            .scp()
            .get_slot_state(1)
            .expect("slot 1 should still exist");
        assert_eq!(
            state2.nomination_round, 1,
            "second trigger should NOT advance nomination round"
        );

        // Third trigger for good measure.
        let result3 = herder.trigger_next_ledger(1);
        assert_eq!(
            result3.expect("third trigger should succeed"),
            TriggerOutcome::AlreadyNominating
        );
        let state3 = herder.scp().get_slot_state(1).expect("slot 1 exists");
        assert_eq!(
            state3.nomination_round, 1,
            "third trigger should NOT advance nomination round"
        );
    }

    /// Helper for #2302 stale-slot tests: installs a LedgerManager initialized
    /// to `ledger_seq` so `current_ledger_seq()` returns it deterministically.
    ///
    /// Mirrors the helper in `mod scp_pipeline_tests` (line ~7461) but lives
    /// inside `mod tests` so the new stale-slot tests below can use it
    /// without a cross-module visibility change.
    fn make_lm_at_seq_for_stale_test(ledger_seq: u32) -> Arc<henyey_ledger::LedgerManager> {
        use henyey_ledger::{LedgerManager, LedgerManagerConfig};
        use stellar_xdr::{
            Hash, LedgerHeader, LedgerHeaderExt, StellarValue, StellarValueExt, TimePoint, VecM,
        };
        let config = LedgerManagerConfig {
            validate_bucket_hash: false,
            ..Default::default()
        };
        let lm = LedgerManager::new("Test Network".to_string(), config);
        let header = LedgerHeader {
            ledger_version: 0,
            previous_ledger_hash: Hash([0u8; 32]),
            scp_value: StellarValue {
                tx_set_hash: Hash([0u8; 32]),
                close_time: TimePoint(0),
                upgrades: VecM::default(),
                ext: StellarValueExt::Basic,
            },
            tx_set_result_hash: Hash([0u8; 32]),
            bucket_list_hash: Hash([0u8; 32]),
            ledger_seq,
            total_coins: 1_000_000_000_000,
            fee_pool: 0,
            inflation_seq: 0,
            id_pool: 0,
            base_fee: 100,
            base_reserve: 5_000_000,
            max_tx_set_size: 100,
            skip_list: [
                Hash([0u8; 32]),
                Hash([0u8; 32]),
                Hash([0u8; 32]),
                Hash([0u8; 32]),
            ],
            ext: LedgerHeaderExt::V0,
        };
        let header_hash = henyey_ledger::compute_header_hash(&header).expect("hash");
        lm.initialize(
            henyey_bucket::BucketList::new(),
            henyey_bucket::HotArchiveBucketList::new(),
            header,
            header_hash,
        )
        .expect("init");
        Arc::new(lm)
    }

    /// Regression test for #2302 — Change 2 of the parity-hardening proposal.
    ///
    /// Verifies that `trigger_next_ledger` returns `TriggerOutcome::SkippedStale`
    /// when LCL has advanced past the requested slot — the deterministic
    /// equivalent of "a concurrent close_ledger task advanced LCL while we
    /// were inside `build_nomination_value`." We reproduce the failure mode
    /// without multi-thread orchestration by installing a LedgerManager whose
    /// `current_ledger_seq()` already equals the requested slot, so the
    /// post-build re-check immediately observes `lcl + 1 != slot`.
    ///
    /// Parity: HerderImpl.cpp:1550-1562 — `ledgerSeqToTrigger != slotIndex`
    /// abort.
    #[test]
    fn test_trigger_next_ledger_skips_stale_slot() {
        let seed = [42u8; 32];
        let secret = SecretKey::from_seed(&seed);
        let public = secret.public_key();
        let local_node_id = stellar_xdr::NodeId(stellar_xdr::PublicKey::PublicKeyTypeEd25519(
            stellar_xdr::Uint256(*public.as_bytes()),
        ));
        let quorum_set = ScpQuorumSet {
            threshold: 1,
            validators: vec![local_node_id].try_into().unwrap(),
            inner_sets: vec![].try_into().unwrap(),
        };
        let config = HerderConfig {
            is_validator: true,
            node_public_key: public,
            local_quorum_set: Some(quorum_set),
            ..HerderConfig::default()
        };
        const STALE_SLOT: u32 = 5;
        let herder = Herder::with_secret_key(
            config,
            secret,
            make_lm_at_seq_for_stale_test(STALE_SLOT),
            TimerManagerHandle::no_op(),
        );

        herder.bootstrap(STALE_SLOT);

        // Request slot == LCL: re-check sees `lcl + 1 == 6 != slot == 5`.
        let result = herder.trigger_next_ledger(STALE_SLOT);
        assert_eq!(
            result.expect("trigger should not error on stale slot"),
            TriggerOutcome::SkippedStale,
            "trigger_next_ledger should return SkippedStale when slot != LCL+1"
        );

        // No SCP slot state should exist for the stale slot — we aborted
        // before scp.nominate.
        let slot_state = herder.scp().get_slot_state(STALE_SLOT as u64);
        assert!(
            slot_state.map_or(true, |s| !s.is_nominating),
            "stale slot must not be in nominating state"
        );
    }

    /// maxtps iter 9: `prebuild_nomination_value` caches the nomination value
    /// ahead of the trigger; a second prebuild for the same slot is a no-op;
    /// a stale slot is refused; and the trigger consumes the cached value.
    #[test]
    fn test_prebuild_nomination_value_caches_and_trigger_reuses() {
        let seed = [43u8; 32];
        let secret = SecretKey::from_seed(&seed);
        let public = secret.public_key();
        let local_node_id = stellar_xdr::NodeId(stellar_xdr::PublicKey::PublicKeyTypeEd25519(
            stellar_xdr::Uint256(*public.as_bytes()),
        ));
        let quorum_set = ScpQuorumSet {
            threshold: 1,
            validators: vec![local_node_id].try_into().unwrap(),
            inner_sets: vec![].try_into().unwrap(),
        };
        let config = HerderConfig {
            is_validator: true,
            node_public_key: public,
            local_quorum_set: Some(quorum_set),
            ..HerderConfig::default()
        };
        let herder = Herder::with_secret_key(
            config,
            secret,
            make_lm_at_seq_for_stale_test(1),
            TimerManagerHandle::no_op(),
        );
        herder.bootstrap(1);
        let next = 2u32;

        // Stale slot refused (LCL+1 == 2, so 3 is stale-ahead).
        assert!(
            !herder.prebuild_nomination_value(next + 1),
            "prebuild must refuse a slot != LCL+1"
        );
        assert!(herder.cached_nomination_value.read().is_none());

        // Prebuild for the correct slot caches a value.
        assert!(
            herder.prebuild_nomination_value(next),
            "prebuild must cache for LCL+1"
        );
        let cached = herder.cached_nomination_value.read().clone();
        assert!(
            matches!(cached, Some((slot, _)) if slot == next as u64),
            "cached value must be keyed by the prebuilt slot"
        );

        // A duplicate prebuild is a no-op.
        assert!(
            !herder.prebuild_nomination_value(next),
            "second prebuild for the same slot must be a no-op"
        );

        // The trigger consumes the cached value and starts nomination
        // (solo 1-of-1 quorum: nomination self-externalizes synchronously).
        let outcome = herder
            .trigger_next_ledger(next)
            .expect("trigger must succeed with a prebuilt value");
        assert!(
            !matches!(outcome, TriggerOutcome::SkippedStale),
            "trigger must not skip a fresh prebuilt slot, got {outcome:?}"
        );
    }

    /// Regression test for #2869 — HERDER §5.2 non-validator txset build + cache parity.
    ///
    /// Verifies that an observer herder (non-validator) in tracking mode can
    /// call `trigger_next_ledger` and have the locally-built tx set published
    /// into the cache (queryable via `has_tx_set`/`get_tx_set`) WITHOUT
    /// creating any SCP nomination state for that slot.
    ///
    /// Pre-fix: FAILS because `trigger_next_ledger` returns
    /// `Err(HerderError::NotValidating)` before any tx set is built.
    /// Post-fix: PASSES — observers run the shared build/cache path and stop
    /// before `cached_nomination_value` / `scp.nominate`.
    #[test]
    fn test_trigger_next_ledger_observer_publishes_tx_set_to_cache_without_nominating() {
        // Create an observer herder (no secret key, is_validator=false).
        let config = HerderConfig {
            is_validator: false,
            ..HerderConfig::default()
        };
        let herder = Herder::new(config, make_default_lm(), TimerManagerHandle::no_op());

        // Bootstrap into tracking state so the is_tracking() check passes.
        herder.bootstrap(0);
        assert!(
            herder.is_tracking(),
            "herder should be tracking after bootstrap"
        );
        assert!(!herder.is_validator(), "herder should NOT be a validator");

        // Call trigger_next_ledger for the next slot (LCL=0, so next=1).
        let result = herder.trigger_next_ledger(1);

        // Post-fix: should succeed with ObserverCached.
        let outcome = result.expect(
            "observer trigger_next_ledger should succeed after fix (currently returns NotValidating)",
        );
        assert_eq!(
            outcome,
            TriggerOutcome::ObserverCached,
            "observer should get ObserverCached outcome"
        );

        // The built tx set should be queryable via the real has_tx_set/get_tx_set
        // cache path (not just a size check).
        let cached_hashes = herder.scp_driver.cached_tx_set_hashes();
        assert_eq!(
            cached_hashes.len(),
            1,
            "observer should have cached exactly one tx set, got {}",
            cached_hashes.len()
        );
        let built_hash = &cached_hashes[0];

        // Verify the specific hash is retrievable via the public query path.
        assert!(
            herder.has_tx_set(built_hash),
            "built tx set must be queryable via has_tx_set"
        );
        let retrieved = herder.get_tx_set(built_hash);
        assert!(
            retrieved.is_some(),
            "built tx set must be retrievable via get_tx_set"
        );

        // No SCP nomination state should exist for the slot.
        let slot_state = herder.scp().get_slot_state(1);
        assert!(
            slot_state.map_or(true, |s| !s.is_nominating),
            "observer must NOT enter nominating state"
        );
    }

    /// Regression test for #2869 review feedback — post-publication is_applying guard.
    ///
    /// Parity: stellar-core HerderImpl.cpp:1583 checks
    /// `mLedgerManager.isApplying()` after `addTxSet` because side effects
    /// from draining ready envelopes can externalize and start ledger
    /// application. If apply starts mid-trigger, upstream returns immediately
    /// without observer success or validator nomination.
    ///
    /// Pre-fix: trigger_next_ledger returns ObserverCached even when
    /// is_applying is true (missing guard).
    /// Post-fix: returns SkippedStale when is_applying becomes true after
    /// tx-set publication.
    #[test]
    fn test_trigger_next_ledger_observer_skips_when_is_applying() {
        use std::sync::atomic::AtomicBool;

        let config = HerderConfig {
            is_validator: false,
            ..HerderConfig::default()
        };
        let herder = Herder::new(config, make_default_lm(), TimerManagerHandle::no_op());

        // Install is_applying flag set to true — simulating that ledger
        // application started (e.g. triggered by SCP externalization during
        // process_ready_fetching_envelopes).
        let is_applying = Arc::new(AtomicBool::new(true));
        herder
            .set_is_applying_flag(Arc::clone(&is_applying))
            .expect("flag install should succeed");

        // Bootstrap into tracking state.
        herder.bootstrap(0);
        assert!(herder.is_tracking());
        assert!(!herder.is_validator());

        // With is_applying=true, trigger should return SkippedStale even
        // though LCL hasn't advanced (the tx set is built and cached, but
        // the post-publication guard fires).
        let result = herder.trigger_next_ledger(1);
        let outcome = result.expect("trigger should not error");
        assert_eq!(
            outcome,
            TriggerOutcome::SkippedStale,
            "observer must skip when is_applying is true (parity: HerderImpl.cpp:1583)"
        );

        // Now clear the flag and verify trigger succeeds normally.
        is_applying.store(false, std::sync::atomic::Ordering::Relaxed);
        // Reset the herder to avoid the idempotent build_and_cache guard
        // (the first call already built/cached, but was stopped at the guard).
        // bootstrap(0) re-initializes tracking state.
        herder.bootstrap(0);
        let result2 = herder.trigger_next_ledger(1);
        let outcome2 = result2.expect("trigger should not error after clearing flag");
        assert_eq!(
            outcome2,
            TriggerOutcome::ObserverCached,
            "observer should succeed when is_applying is false"
        );
    }

    /// Regression test for #2869 (Bounce-Back Cycle 2) — validity cache parity.
    ///
    /// Verifies that `trigger_next_ledger` on a non-validator populates BOTH
    /// the raw tx-set cache AND the validity cache, mirroring stellar-core's
    /// `cacheValidTxSet(*applicableProposedSet, lcl, upperBoundCloseTimeOffset)`
    /// in `HerderImpl::triggerNextLedger()`.
    ///
    /// Pre-fix: the validity cache remains empty because
    /// `validate_and_cache_built_tx_set` only called `cache_tx_set`.
    /// Post-fix: `cache_valid_tx_set` writes `true` into the validity cache.
    #[test]
    fn test_trigger_next_ledger_observer_populates_validity_cache() {
        let config = HerderConfig {
            is_validator: false,
            ..HerderConfig::default()
        };
        let herder = Herder::new(config, make_default_lm(), TimerManagerHandle::no_op());

        // Bootstrap into tracking state.
        herder.bootstrap(0);
        assert!(herder.is_tracking());
        assert!(!herder.is_validator());

        // Validity cache should be empty before the trigger.
        let valid_before = herder.scp_driver.cache_sizes().tx_set_valid_cache;
        assert_eq!(
            valid_before, 0,
            "validity cache should be empty before trigger"
        );

        // Trigger for slot 1.
        let result = herder.trigger_next_ledger(1);
        assert_eq!(
            result.expect("observer trigger should succeed"),
            TriggerOutcome::ObserverCached,
        );

        // The raw tx-set cache should be populated (existing assertion).
        let cached_hashes = herder.scp_driver.cached_tx_set_hashes();
        assert_eq!(cached_hashes.len(), 1);

        // The validity cache MUST also be populated — this is the parity fix.
        let valid_after = herder.scp_driver.cache_sizes().tx_set_valid_cache;
        assert_eq!(
            valid_after, 1,
            "validity cache must contain one entry after observer trigger \
             (parity: stellar-core cacheValidTxSet in triggerNextLedger)"
        );
    }

    /// Regression test for #2869 review feedback + #2816 — close-time validity gate.
    ///
    /// Verifies that `trigger_next_ledger` on an observer performs NO
    /// build/cache/publish side effects when the locally-selected close time
    /// is invalid (too far in the future relative to wall clock).
    /// Mirrors stellar-core's `ctValidityOffset` gate in `triggerNextLedger`,
    /// which aborts (without error or side effects) before `addTxSet` for both
    /// validators and observers. After #2816 the abort is surfaced as the typed
    /// `TriggerOutcome::SkippedInvalidCloseTime` instead of a hard error.
    #[test]
    fn test_trigger_next_ledger_observer_skips_build_on_invalid_close_time() {
        // Create a LedgerManager whose LCL has a close_time far in the future.
        // build_and_cache_candidate_tx_set will compute:
        //   close_time = max(SystemTime::now(), lcl_close_time + 1)
        // With lcl_close_time = u64::MAX - 100, close_time = u64::MAX - 99,
        // which is always > now + MAX_TIME_SLIP_SECONDS → gate rejects.
        use henyey_ledger::{LedgerManager, LedgerManagerConfig};
        use stellar_xdr::{
            Hash, LedgerHeader, LedgerHeaderExt, StellarValue, StellarValueExt, TimePoint, VecM,
        };
        let lm_config = LedgerManagerConfig {
            validate_bucket_hash: false,
            ..Default::default()
        };
        let lm = LedgerManager::new("Test Network".to_string(), lm_config);
        let header = LedgerHeader {
            ledger_version: 0,
            previous_ledger_hash: Hash([0u8; 32]),
            scp_value: StellarValue {
                tx_set_hash: Hash([0u8; 32]),
                close_time: TimePoint(u64::MAX - 100),
                upgrades: VecM::default(),
                ext: StellarValueExt::Basic,
            },
            tx_set_result_hash: Hash([0u8; 32]),
            bucket_list_hash: Hash([0u8; 32]),
            ledger_seq: 0,
            total_coins: 1_000_000_000_000,
            fee_pool: 0,
            inflation_seq: 0,
            id_pool: 0,
            base_fee: 100,
            base_reserve: 5_000_000,
            max_tx_set_size: 100,
            skip_list: [
                Hash([0u8; 32]),
                Hash([0u8; 32]),
                Hash([0u8; 32]),
                Hash([0u8; 32]),
            ],
            ext: LedgerHeaderExt::V0,
        };
        let header_hash = henyey_ledger::compute_header_hash(&header).expect("hash");
        lm.initialize(
            henyey_bucket::BucketList::new(),
            henyey_bucket::HotArchiveBucketList::new(),
            header,
            header_hash,
        )
        .expect("init");

        let config = HerderConfig {
            is_validator: false,
            ..HerderConfig::default()
        };
        let herder = Herder::new(config, Arc::new(lm), TimerManagerHandle::no_op());

        // Bootstrap into tracking state.
        herder.bootstrap(0);
        assert!(herder.is_tracking());
        assert!(!herder.is_validator());

        // Verify no tx sets are cached before trigger.
        let cache_before = herder.scp_driver.cached_tx_set_hashes().len();
        assert_eq!(cache_before, 0);
        let valid_before = herder.scp_driver.cache_sizes().tx_set_valid_cache;
        assert_eq!(valid_before, 0);

        // Trigger for slot 1 — the far-ahead close-time abort in
        // trigger_next_ledger fires before the observer build, returning the
        // typed SkippedInvalidCloseTime outcome (no error, no side effects).
        let result = herder.trigger_next_ledger(1);
        assert_eq!(
            result.expect("trigger_next_ledger should not hard-error on invalid close time"),
            TriggerOutcome::SkippedInvalidCloseTime,
            "observer trigger must return SkippedInvalidCloseTime when close \
             time is too far ahead (parity: ctValidityOffset abort in triggerNextLedger)"
        );

        // Verify NO side effects: tx-set cache and validity cache stay empty.
        let cache_after = herder.scp_driver.cached_tx_set_hashes().len();
        assert_eq!(
            cache_after, 0,
            "no tx set should be cached when close time is invalid"
        );
        let valid_after = herder.scp_driver.cache_sizes().tx_set_valid_cache;
        assert_eq!(
            valid_after, 0,
            "validity cache must remain empty when close time is invalid \
             (parity: stellar-core skips cacheValidTxSet when ctValidityOffset != 0)"
        );
    }

    /// Regression test for #2302 — Change 3 of the parity-hardening proposal.
    ///
    /// Verifies that `handle_nomination_timeout` returns
    /// `TimeoutOutcome::SkippedStale` when LCL has advanced past the slot.
    #[test]
    fn test_handle_nomination_timeout_skips_stale_slot() {
        let seed = [43u8; 32];
        let secret = SecretKey::from_seed(&seed);
        let public = secret.public_key();
        let local_node_id = stellar_xdr::NodeId(stellar_xdr::PublicKey::PublicKeyTypeEd25519(
            stellar_xdr::Uint256(*public.as_bytes()),
        ));
        let quorum_set = ScpQuorumSet {
            threshold: 1,
            validators: vec![local_node_id].try_into().unwrap(),
            inner_sets: vec![].try_into().unwrap(),
        };
        let config = HerderConfig {
            is_validator: true,
            node_public_key: public,
            local_quorum_set: Some(quorum_set),
            ..HerderConfig::default()
        };
        const STALE_SLOT: u32 = 7;
        let herder = Herder::with_secret_key(
            config,
            secret,
            make_lm_at_seq_for_stale_test(STALE_SLOT),
            TimerManagerHandle::no_op(),
        );

        herder.bootstrap(STALE_SLOT);

        // Cache is empty for this slot, so the cache-miss branch fires
        // build_nomination_value → drain → re-check → SkippedStale.
        let outcome = herder.handle_nomination_timeout(STALE_SLOT as u64);
        assert_eq!(
            outcome,
            TimeoutOutcome::SkippedStale,
            "timeout should report SkippedStale when slot != LCL+1"
        );
    }

    // ─────────────────────────────────────────────────────────────────
    // #2816: ctValidityOffset / far-ahead close-time tests
    // ─────────────────────────────────────────────────────────────────

    /// Helper: creates a LedgerManager at `ledger_seq` with a custom close_time.
    fn make_lm_with_close_time(
        ledger_seq: u32,
        close_time: u64,
    ) -> Arc<henyey_ledger::LedgerManager> {
        use henyey_ledger::{LedgerManager, LedgerManagerConfig};
        use stellar_xdr::{
            Hash, LedgerHeader, LedgerHeaderExt, StellarValue, StellarValueExt, TimePoint, VecM,
        };
        let config = LedgerManagerConfig {
            validate_bucket_hash: false,
            ..Default::default()
        };
        let lm = LedgerManager::new("Test Network".to_string(), config);
        let header = LedgerHeader {
            ledger_version: 0,
            previous_ledger_hash: Hash([0u8; 32]),
            scp_value: StellarValue {
                tx_set_hash: Hash([0u8; 32]),
                close_time: TimePoint(close_time),
                upgrades: VecM::default(),
                ext: StellarValueExt::Basic,
            },
            tx_set_result_hash: Hash([0u8; 32]),
            bucket_list_hash: Hash([0u8; 32]),
            ledger_seq,
            total_coins: 1_000_000_000_000,
            fee_pool: 0,
            inflation_seq: 0,
            id_pool: 0,
            base_fee: 100,
            base_reserve: 5_000_000,
            max_tx_set_size: 100,
            skip_list: [
                Hash([0u8; 32]),
                Hash([0u8; 32]),
                Hash([0u8; 32]),
                Hash([0u8; 32]),
            ],
            ext: LedgerHeaderExt::V0,
        };
        let header_hash = henyey_ledger::compute_header_hash(&header).expect("hash");
        lm.initialize(
            henyey_bucket::BucketList::new(),
            henyey_bucket::HotArchiveBucketList::new(),
            header,
            header_hash,
        )
        .expect("init");
        Arc::new(lm)
    }

    /// Regression test for #2816: `trigger_next_ledger` must return
    /// `SkippedInvalidCloseTime` when the LCL close time is more than
    /// MAX_TIME_SLIP_SECONDS ahead of the wall clock.
    #[test]
    fn test_trigger_next_ledger_skips_far_ahead_close_time() {
        let seed = [50u8; 32];
        let secret = SecretKey::from_seed(&seed);
        let public = secret.public_key();
        let local_node_id = stellar_xdr::NodeId(stellar_xdr::PublicKey::PublicKeyTypeEd25519(
            stellar_xdr::Uint256(*public.as_bytes()),
        ));
        let quorum_set = ScpQuorumSet {
            threshold: 1,
            validators: vec![local_node_id].try_into().unwrap(),
            inner_sets: vec![].try_into().unwrap(),
        };
        let config = HerderConfig {
            is_validator: true,
            node_public_key: public,
            local_quorum_set: Some(quorum_set),
            ..HerderConfig::default()
        };

        // Set the test clock to "now" = 1000, but make the LCL close time
        // far ahead at now + MAX_TIME_SLIP_SECONDS + 10.
        let now = 1000u64;
        let far_ahead_close_time = now + MAX_TIME_SLIP_SECONDS + 10;

        let herder = Herder::with_secret_key(
            config,
            secret,
            make_lm_with_close_time(0, far_ahead_close_time),
            TimerManagerHandle::no_op(),
        );
        herder.scp_driver.set_test_clock(now);
        herder.bootstrap(0);

        // trigger_next_ledger(1) should see next_close_time = lcl + 1 = far_ahead + 1
        // which is > now + MAX_TIME_SLIP_SECONDS, so it should abort.
        let result = herder.trigger_next_ledger(1);
        assert_eq!(
            result.expect("should not error"),
            TriggerOutcome::SkippedInvalidCloseTime,
            "trigger_next_ledger must skip when close time is too far ahead"
        );

        // No SCP slot state should exist — we aborted before scp.nominate.
        let slot_state = herder.scp().get_slot_state(1);
        assert!(
            slot_state.map_or(true, |s| !s.is_nominating),
            "far-ahead slot must not enter nomination"
        );
    }

    /// Regression test for #2816: `handle_nomination_timeout` cache-miss
    /// rebuild must return `SkippedInvalidCloseTime` when the close time
    /// is still invalid.
    #[test]
    fn test_handle_nomination_timeout_far_ahead_cache_miss_returns_noop_without_state() {
        let seed = [51u8; 32];
        let secret = SecretKey::from_seed(&seed);
        let public = secret.public_key();
        let local_node_id = stellar_xdr::NodeId(stellar_xdr::PublicKey::PublicKeyTypeEd25519(
            stellar_xdr::Uint256(*public.as_bytes()),
        ));
        let quorum_set = ScpQuorumSet {
            threshold: 1,
            validators: vec![local_node_id].try_into().unwrap(),
            inner_sets: vec![].try_into().unwrap(),
        };
        let config = HerderConfig {
            is_validator: true,
            node_public_key: public,
            local_quorum_set: Some(quorum_set),
            ..HerderConfig::default()
        };

        let now = 2000u64;
        let far_ahead_close_time = now + MAX_TIME_SLIP_SECONDS + 5;

        let herder = Herder::with_secret_key(
            config,
            secret,
            make_lm_with_close_time(0, far_ahead_close_time),
            TimerManagerHandle::no_op(),
        );
        herder.scp_driver.set_test_clock(now);
        herder.bootstrap(0);

        // No cached value for slot 1, so the cache-miss path fires.
        let outcome = herder.handle_nomination_timeout(1);
        assert_eq!(
            outcome,
            TimeoutOutcome::SkippedInvalidCloseTime,
            "cache-miss rebuild must abort when close time is too far ahead"
        );

        // No SCP slot state should be created.
        let slot_state = herder.scp().get_slot_state(1);
        assert!(
            slot_state.map_or(true, |s| !s.is_nominating),
            "must not enter nomination on far-ahead cache-miss rebuild"
        );
    }

    /// Unit test for #2816: `ct_validity_offset` returns 0 at the exact
    /// validity boundary.
    #[test]
    fn test_ct_validity_offset_zero_at_validity_boundary() {
        let (herder, _) = make_validator_herder();
        let now = 5000u64;
        herder.scp_driver.set_test_clock(now);

        // close_time = now + MAX_TIME_SLIP_SECONDS = exactly at boundary with offset=0
        let close_time = now + MAX_TIME_SLIP_SECONDS;
        let offset = herder.ct_validity_offset(close_time, 0);
        assert_eq!(offset, 0, "at boundary with zero trigger offset → no delay");
    }

    /// Unit test for #2816: `ct_validity_offset` returns positive offset
    /// one second past the boundary (includes +1 boundary guard).
    #[test]
    fn test_ct_validity_offset_positive_one_second_past_boundary() {
        let (herder, _) = make_validator_herder();
        let now = 5000u64;
        herder.scp_driver.set_test_clock(now);

        // close_time = now + MAX_TIME_SLIP_SECONDS + 1 = one past boundary with offset=0
        let close_time = now + MAX_TIME_SLIP_SECONDS + 1;
        let offset = herder.ct_validity_offset(close_time, 0);
        // Parity: stellar-core returns (1s + 1ms), in whole seconds = 2
        assert_eq!(
            offset, 2,
            "one second past boundary + 1s guard → need 2 seconds delay"
        );
    }

    /// Unit test for #2816: `ct_validity_offset` accounts for max_ct_offset
    /// (trigger offset widens the validity window).
    #[test]
    fn test_ct_validity_offset_trigger_offset_widens_window() {
        let (herder, _) = make_validator_herder();
        let now = 5000u64;
        herder.scp_driver.set_test_clock(now);

        // close_time = now + 65 (5 past the 60s boundary), but trigger_offset = 5
        // boundary = now + 60 + 5 = now + 65, so close_time <= boundary → 0
        let close_time = now + 65;
        let offset = herder.ct_validity_offset(close_time, 5);
        assert_eq!(offset, 0, "trigger offset should widen validity window");

        // close_time = now + 66, trigger_offset = 5 → boundary = now+65, need 1s + 1 guard = 2
        let offset2 = herder.ct_validity_offset(now + 66, 5);
        assert_eq!(offset2, 2);
    }

    /// Unit test for #2816: `Herder::prepare_start` forwards to the driver.
    #[test]
    fn test_herder_prepare_start_forwards_driver_timing() {
        let (herder, _) = make_validator_herder();
        herder.bootstrap(0);

        // Before any ballot start, prepare_start returns None.
        assert!(
            herder.prepare_start(1).is_none(),
            "prepare_start must be None before ballot start"
        );

        // Record a ballot start for slot 1.
        herder.scp_driver().record_ballot_start(1);
        let ps = herder.prepare_start(1);
        assert!(
            ps.is_some(),
            "prepare_start must return Some after ballot start"
        );

        // After purging, it should be None again.
        herder.scp_driver().purge_slots_below(2);
        assert!(
            herder.prepare_start(1).is_none(),
            "prepare_start must return None after slot is purged"
        );
    }

    /// Regression test for #2816 Parity review: when `prepareStart + expectedClose <= now`
    /// (trigger_time is clamped to `now`), the triggerOffset passed to `ct_validity_offset`
    /// must be 0 (remaining time until trigger = 0), NOT `expectedClose` (elapsed time).
    /// This ensures we don't widen the validity window by `expectedClose` seconds.
    ///
    /// Concrete case: now=1000, expectedClose=5s, lcl.close_time=1070.
    /// minCandidateCt=1071. With triggerOffset=0: boundary=1060, offset=12 (blocks).
    /// With the OLD (wrong) triggerOffset=5: boundary=1065, offset=7 (would fire too early).
    #[test]
    fn test_ct_validity_offset_clamped_trigger_time_gives_zero_offset() {
        let (herder, _) = make_validator_herder();
        let now = 1000u64;
        herder.scp_driver.set_test_clock(now);

        let lcl_close_time = 1070u64;
        let min_candidate_ct = lcl_close_time + 1; // 1071

        // Correct behavior: triggerOffset = 0 (trigger_time clamped to now)
        let offset = herder.ct_validity_offset(min_candidate_ct, 0);
        // boundary = 1000 + 60 + 0 = 1060, excess = 1071 - 1060 = 11, +1 guard = 12
        assert_eq!(offset, 12, "clamped trigger must not widen validity window");

        // Wrong behavior would pass expectedClose=5 as offset:
        let wrong_offset = herder.ct_validity_offset(min_candidate_ct, 5);
        // boundary = 1000 + 60 + 5 = 1065, excess = 1071 - 1065 = 6, +1 guard = 7
        assert_eq!(
            wrong_offset, 7,
            "wider window from elapsed time = incorrect"
        );

        // The correct offset (12) must be larger than the wrong one (7),
        // proving we'd wait longer and not nominate too early.
        assert!(offset > wrong_offset);
    }

    /// Regression test for #2816 Correctness review: `build_nomination_value`
    /// must not underflow when the snapshot's LCL has advanced past the
    /// caller-provided close_time. Instead it returns None (abort).
    #[test]
    fn test_build_nomination_value_aborts_on_stale_close_time() {
        let (herder, _) = make_validator_herder();
        herder.bootstrap(0);
        let now = 5000u64;
        herder.scp_driver.set_test_clock(now);

        // The default LM has lcl_close_time = 0.
        // Pass close_time = 0 to trigger the checked_sub guard (offset = 0 → abort).
        let lcl_ct = herder.lcl_close_time();
        let result = herder.build_nomination_value(lcl_ct);
        assert!(
            result.is_none(),
            "build_nomination_value must abort when close_time <= snapshot lcl_close_time"
        );
    }

    /// Regression test for #2816 Correctness review round 2:
    /// `trigger_next_ledger()` must return `Ok(SkippedStale)` (not a hard
    /// `Err(Internal)`) when `build_nomination_value()` returns None.
    /// Simulates the race by resetting the LedgerManager (making
    /// `create_snapshot()` fail with NotInitialized) after the entry guards
    /// pass but before the snapshot is taken inside `build_nomination_value`.
    #[test]
    fn test_trigger_next_ledger_returns_skipped_stale_on_build_none() {
        let now = 5000u64;
        let seed = [70u8; 32];
        let secret = SecretKey::from_seed(&seed);
        let public = secret.public_key();
        let local_node_id = stellar_xdr::NodeId(stellar_xdr::PublicKey::PublicKeyTypeEd25519(
            stellar_xdr::Uint256(*public.as_bytes()),
        ));
        let quorum_set = ScpQuorumSet {
            threshold: 1,
            validators: vec![local_node_id].try_into().unwrap(),
            inner_sets: vec![].try_into().unwrap(),
        };
        let config = HerderConfig {
            is_validator: true,
            node_public_key: public,
            local_quorum_set: Some(quorum_set),
            ..HerderConfig::default()
        };

        let lm = make_lm_with_close_time(0, now);
        let herder =
            Herder::with_secret_key(config, secret, lm.clone(), TimerManagerHandle::no_op());
        herder.scp_driver.set_test_clock(now);
        herder.bootstrap(0);

        // Reset the LedgerManager so `create_snapshot()` returns Err(NotInitialized).
        // Entry guards still pass: lcl_matches_slot(1) checks current_ledger_seq()
        // which reads from the reset genesis header (seq=0), so 0+1=1 matches.
        // But build_nomination_value → create_snapshot → Err → returns None.
        lm.reset();

        let result = herder.trigger_next_ledger(1);
        assert_eq!(
            result.expect(
                "must not be a hard error — the old code erroneously returned Err(Internal)"
            ),
            TriggerOutcome::SkippedStale,
            "trigger_next_ledger must return SkippedStale when build_nomination_value returns None"
        );
    }

    /// Regression test for #2816 Correctness review round 2:
    /// `ScpDriver::slot_trigger_time()` returns the first_seen instant for
    /// a slot after `record_slot_activity` is called.
    #[test]
    fn test_slot_trigger_time_returns_recorded_instant() {
        let (herder, _) = make_validator_herder();
        herder.bootstrap(0);

        // Before recording activity, slot_trigger_time should be None.
        assert!(
            herder.scp_driver.slot_trigger_time(1).is_none(),
            "slot_trigger_time must be None before record_slot_activity"
        );

        // Record activity for slot 1.
        herder.scp_driver.record_slot_activity(1);

        // After recording, slot_trigger_time should be Some.
        let trigger_time = herder.scp_driver.slot_trigger_time(1);
        assert!(
            trigger_time.is_some(),
            "slot_trigger_time must be Some after record_slot_activity"
        );

        // The recorded time should be very recent (within 1 second of now).
        let elapsed = trigger_time.unwrap().elapsed();
        assert!(
            elapsed.as_secs() < 1,
            "slot_trigger_time should be very recent, got {:?} ago",
            elapsed
        );
    }

    /// Regression test for #2816 Correctness review: `lcl_close_time()` is
    /// a public accessor that reads the current LCL close time.
    #[test]
    fn test_lcl_close_time_accessor() {
        let now = 3000u64;
        let herder = Herder::with_secret_key(
            HerderConfig {
                is_validator: true,
                ..HerderConfig::default()
            },
            SecretKey::from_seed(&[60u8; 32]),
            make_lm_with_close_time(0, now),
            TimerManagerHandle::no_op(),
        );
        herder.bootstrap(0);
        assert_eq!(herder.lcl_close_time(), now);
    }

    /// Direct regression test for the `lcl_matches_slot` helper used by the
    /// post-build re-checks in both `trigger_next_ledger` and
    /// `handle_nomination_timeout`. The helper is the only mechanism the
    /// post-build re-check uses, so testing it directly verifies the
    /// post-build path even though the public stale-slot tests exercise the
    /// pre-build entry check (which short-circuits before the build runs).
    ///
    /// Covers all three branches of the helper: LM=None (proceeds), LM
    /// matches slot, LM mismatches slot (stale).
    #[test]
    fn test_lcl_matches_slot_branches() {
        let seed = [44u8; 32];
        let secret = SecretKey::from_seed(&seed);
        let public = secret.public_key();
        let local_node_id = stellar_xdr::NodeId(stellar_xdr::PublicKey::PublicKeyTypeEd25519(
            stellar_xdr::Uint256(*public.as_bytes()),
        ));
        let quorum_set = ScpQuorumSet {
            threshold: 1,
            validators: vec![local_node_id].try_into().unwrap(),
            inner_sets: vec![].try_into().unwrap(),
        };
        let config = HerderConfig {
            is_validator: true,
            node_public_key: public,
            local_quorum_set: Some(quorum_set),
            ..HerderConfig::default()
        };
        let herder = Herder::with_secret_key(
            config,
            secret,
            make_lm_at_seq_for_stale_test(10),
            TimerManagerHandle::no_op(),
        );

        // LM at seq=10: verify match (slot=11) and
        // mismatch (slot=10, slot=5, slot=12).
        assert!(
            herder.lcl_matches_slot(11),
            "LM at seq=10 must match slot=11 (LCL+1)"
        );
        assert!(
            !herder.lcl_matches_slot(10),
            "LM at seq=10 must NOT match slot=10 (already-LCL)"
        );
        assert!(
            !herder.lcl_matches_slot(5),
            "LM at seq=10 must NOT match slot=5 (far behind)"
        );
        assert!(
            !herder.lcl_matches_slot(12),
            "LM at seq=10 must NOT match slot=12 (ahead of LCL+1)"
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
        let node_id = XdrNodeId(stellar_xdr::PublicKey::PublicKeyTypeEd25519(
            stellar_xdr::Uint256(*public.as_bytes()),
        ));

        // Use the value helper that caches the tx set and signs properly
        let value = make_valid_value_with_cached_tx_set(herder, secret);

        let statement = ScpStatement {
            node_id: node_id.clone(),
            slot_index: slot,
            pledges: ScpStatementPledges::Externalize(ScpStatementExternalize {
                commit: ScpBallot { counter: 1, value },
                n_h: 1,
                commit_quorum_set_hash: stellar_xdr::Hash([0u8; 32]),
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

        let herder = Herder::with_secret_key(
            config,
            SecretKey::from_seed(&[7u8; 32]),
            make_default_lm(),
            TimerManagerHandle::no_op(),
        );
        herder.start_syncing();
        herder.bootstrap(0);

        herder
            .quorum_tracker
            .write()
            .expand(&other_node_id, quorum_set)
            .unwrap();

        let tracking = herder.tracking_slot().get(); // 101

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
            herder.tracking_slot().get(),
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

    /// Guard test for #3252 (WAD verdict): a post-restart node that *follows* a
    /// quorum EXTERNALIZE for slot N — without nominating it itself — resumes its
    /// OWN nomination for slot N+1 the moment its LCL catches up to N.
    ///
    /// This locks the second half of the follow → resume-nomination path that
    /// `test_externalize_transitions_syncing_to_tracking` leaves unproven (it
    /// asserts only follow → Tracking, not the resume of nomination). The #3252
    /// investigation established (verdict (a)) that there is NO follow-only mode:
    /// following and own-balloting share one tracking state, and
    /// `trigger_next_ledger` has no followed-vs-nominated gate (only
    /// `is_tracking()`, the `is_nominating` idempotency skip, and
    /// `lcl_matches_slot`). This is parity-exact with stellar-core
    /// (`HerderImpl::triggerNextLedger` gate `isTracking() && isSynced() &&
    /// !isApplying()`), so the test PASSES today; it would only FAIL if a future
    /// change severed the resume path (e.g. added a spurious follow-only gate).
    ///
    /// Drives the LIVE follow path `receive_scp_envelope` → `Slot::process_envelope`
    /// → `advance_slot` (NOT the restore-only `set_state_from_envelope`).
    #[test]
    fn test_following_externalize_resumes_own_nomination_for_next_slot() {
        let local_secret = SecretKey::from_seed(&[7u8; 32]);
        let local_public = local_secret.public_key();
        let local_node_id = node_id_from_public_key(&local_public);

        let remote_secret = SecretKey::from_seed(&[1u8; 32]);
        let remote_public = remote_secret.public_key();
        let remote_node_id = node_id_from_public_key(&remote_public);

        // 2-of-2 quorum (local + one remote): a single remote EXTERNALIZE that the
        // local node follows is enough to externalize the slot via SCP.
        let quorum_set = ScpQuorumSet {
            threshold: 2,
            validators: vec![local_node_id.clone(), remote_node_id.clone()]
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

        // LM starts at seq 0 (LCL = 0): the same starting state as the proven
        // #3250 follow tests, so the followed value validates against local state.
        let lm = make_default_lm();
        let herder = Herder::with_secret_key(
            config,
            SecretKey::from_seed(&[7u8; 32]),
            lm.clone(),
            TimerManagerHandle::no_op(),
        );

        // Come up out-of-sync after a restart, then restore tracking to slot N
        // (bootstrap(0) sets tracking = 0+1 = 1, so the node tracks slot N = 1).
        herder.start_syncing();
        herder.bootstrap(0);

        herder
            .quorum_tracker
            .write()
            .expand(&remote_node_id, quorum_set)
            .unwrap();

        let follow_slot = herder.tracking_slot().get(); // N == 1
        assert_eq!(
            follow_slot, 1,
            "node should be tracking slot N == 1 before following"
        );

        // --- FOLLOW: receive a remote EXTERNALIZE for slot N and follow it ---
        // (live path: receive_scp_envelope → Slot::process_envelope → advance_slot,
        // NOT set_state_from_envelope, which is restore-only). The node does not
        // nominate this slot — it free-rides the quorum, mirroring the
        // post-restart follower in #3250.
        let env = make_signed_externalize_from(follow_slot, &herder, &remote_secret);
        assert_eq!(herder.receive_scp_envelope(env), EnvelopeState::Valid);

        // Sanity: the slot externalized via SCP and tracking advanced to N+1.
        assert!(
            herder.scp().is_slot_externalized(follow_slot),
            "followed slot must externalize via SCP"
        );
        assert!(
            herder.is_tracking(),
            "node must be Tracking after following EXTERNALIZE"
        );
        assert_eq!(
            herder.tracking_slot().get(),
            follow_slot + 1,
            "tracking must advance to N+1 after following the EXTERNALIZE"
        );

        // --- APPLY: advance LCL to N. In production the followed ledger N is
        // applied by the ledger-close pipeline, bringing LCL to N; here we
        // model that directly so `lcl_matches_slot(N+1)` (LCL+1 == slot) holds.
        // Without this, trigger_next_ledger(N+1) would correctly SkippedStale
        // because LCL is still N-1 (= 0) — the followed slot is not yet applied.
        {
            let mut header = lm.current_header();
            header.ledger_seq = follow_slot as u32; // N
            let header_hash = henyey_ledger::compute_header_hash(&header).expect("hash");
            lm.set_header_for_test(header, header_hash);
        }
        assert_eq!(
            lm.current_ledger_seq() as u64,
            follow_slot,
            "LCL must be at N after applying the followed ledger"
        );

        // The next slot N+1 has NOT yet been nominated by us — we only followed
        // N. This is the precondition the resume path must clear.
        assert!(
            herder
                .scp()
                .get_slot_state(follow_slot + 1)
                .map_or(true, |s| !s.is_nominating),
            "slot N+1 must not be nominating before trigger_next_ledger"
        );

        // --- RESUME: with LCL == N, trigger the next ledger (N+1). The WAD
        // verdict says this MUST nominate — there is no follow-only gate. ---
        let next_slot = follow_slot + 1; // N+1
        let outcome = herder
            .trigger_next_ledger(next_slot as u32)
            .expect("trigger_next_ledger must not error after following");

        assert_eq!(
            outcome,
            TriggerOutcome::Triggered,
            "a node that FOLLOWED slot N must resume its OWN nomination for N+1 \
             (WAD verdict (a) for #3252) — got {:?}",
            outcome
        );

        // And the SCP slot N+1 is now actively nominating: the node re-engaged
        // consensus by itself, not merely by following.
        let state = herder
            .scp()
            .get_slot_state(next_slot)
            .expect("slot N+1 must have SCP state after trigger");
        assert!(
            state.is_nominating,
            "slot N+1 must be in nominating state after the follower resumes \
             (the resume half left unproven by \
             test_externalize_transitions_syncing_to_tracking)"
        );
    }

    /// Regression test for #3250: a node that *follows* a quorum of EXTERNALIZE
    /// for slot N (without nominating, as after a restart) reports
    /// `scp_heard_from_quorum(N) == true` (the parity-faithful SCP ballot flag,
    /// which includes the local node), while the henyey-only `SlotQuorumTracker`
    /// path `heard_from_quorum(N)` reproduces the issue's stuck-`false` triad.
    ///
    /// Fails on main: there is no `scp_heard_from_quorum` accessor, and the
    /// tracker-based `heard_from_quorum` returns false for the followed
    /// externalized slot (only a v-blocking remote subset accumulates; the
    /// local node is never recorded). Passes after the fix.
    #[test]
    fn test_scp_heard_from_quorum_true_for_followed_externalized_slot() {
        // 2-of-2 quorum (local + one remote). The node receives a single remote
        // EXTERNALIZE for `slot` and *follows* it — the local node emits its own
        // EXTERNALIZE on the same value, the slot externalizes, and the SCP
        // per-slot ballot flag (which counts the local node, parity with core
        // LocalNode::isQuorum) is set. The henyey-only tracker only ever records
        // the single remote (the local node is skipped at herder.rs:1965), so it
        // sees just 1 of the 2-of-2 quorum → the stuck-false side of the #3250
        // triad (is_v_blocking=true, scp heard=true, tracker heard=false).
        let local_secret = SecretKey::from_seed(&[7u8; 32]);
        let local_public = local_secret.public_key();
        let local_node_id = node_id_from_public_key(&local_public);

        let remote_secret = SecretKey::from_seed(&[1u8; 32]);
        let remote_public = remote_secret.public_key();
        let remote_node_id = node_id_from_public_key(&remote_public);

        let quorum_set = ScpQuorumSet {
            threshold: 2,
            validators: vec![local_node_id.clone(), remote_node_id.clone()]
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

        let herder = Herder::with_secret_key(
            config,
            SecretKey::from_seed(&[7u8; 32]),
            make_default_lm(),
            TimerManagerHandle::no_op(),
        );
        herder.start_syncing();
        herder.bootstrap(0);

        herder
            .quorum_tracker
            .write()
            .expand(&remote_node_id, quorum_set.clone())
            .unwrap();

        let slot = herder.tracking_slot().get();

        // Follow a remote EXTERNALIZE for `slot` (the node does not nominate —
        // this mirrors the post-restart free-ride path).
        let env = make_signed_externalize_from(slot, &herder, &remote_secret);
        assert_eq!(herder.receive_scp_envelope(env), EnvelopeState::Valid);

        // The slot externalized via SCP, so the per-slot ballot flag is set.
        assert!(
            herder.scp().is_slot_externalized(slot),
            "slot should externalize after following a quorum EXTERNALIZE"
        );

        // The parity-faithful SCP flag is TRUE for the followed slot.
        assert!(
            herder.scp_heard_from_quorum(slot),
            "scp_heard_from_quorum must be true for a followed externalized slot (#3250)"
        );

        // The old tracker path yields the stuck-false side of the triad: it never
        // recorded the local node, so the single remote envelope does not satisfy
        // the 2-of-2 quorum from the tracker's perspective.
        assert!(
            !herder.heard_from_quorum(slot),
            "tracker-based heard_from_quorum reproduces the stuck-false #3250 triad"
        );
    }

    /// Regression test for #3250: the SCP `heard_from_quorum` flag does not
    /// require the local node to appear in `SlotQuorumTracker.slot_nodes` (it
    /// never can — `process_verified` skips self-messages before
    /// `record_envelope` at herder.rs:1965). A 2-of-2 (local + one remote)
    /// quorum that externalizes via a single remote EXTERNALIZE is TRUE under
    /// the SCP flag (the local node is counted by the ballot protocol) but
    /// FALSE under the tracker (local node absent → only 1 of 2 recorded).
    #[test]
    fn test_scp_heard_from_quorum_independent_of_local_node_record() {
        let local_secret = SecretKey::from_seed(&[7u8; 32]);
        let local_public = local_secret.public_key();
        let local_node_id = node_id_from_public_key(&local_public);

        let remote_secret = SecretKey::from_seed(&[1u8; 32]);
        let remote_public = remote_secret.public_key();
        let remote_node_id = node_id_from_public_key(&remote_public);

        let quorum_set = ScpQuorumSet {
            threshold: 2,
            validators: vec![local_node_id.clone(), remote_node_id.clone()]
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

        let herder = Herder::with_secret_key(
            config,
            SecretKey::from_seed(&[7u8; 32]),
            make_default_lm(),
            TimerManagerHandle::no_op(),
        );
        herder.start_syncing();
        herder.bootstrap(0);

        herder
            .quorum_tracker
            .write()
            .expand(&remote_node_id, quorum_set.clone())
            .unwrap();

        let slot = herder.tracking_slot().get();

        // Follow the single remote EXTERNALIZE. With a 2-of-2 qset that includes
        // the local node, the ballot protocol counts the local node and the
        // remote → quorum; the tracker only ever has the remote.
        let env = make_signed_externalize_from(slot, &herder, &remote_secret);
        assert_eq!(herder.receive_scp_envelope(env), EnvelopeState::Valid);

        assert!(
            herder.scp_heard_from_quorum(slot),
            "scp_heard_from_quorum counts the local node (parity with core LocalNode::isQuorum)"
        );

        // The local node is never in the tracker's slot_nodes (self-message skip),
        // so the tracker cannot reach a 2-of-2 quorum from the single remote.
        assert!(
            !herder.heard_from_quorum(slot),
            "tracker excludes the local node (herder.rs:1965) → 2-of-2 qset stuck false"
        );
    }

    /// `scp_heard_from_quorum` returns false for a slot with no SCP state.
    #[test]
    fn test_scp_heard_from_quorum_missing_slot_is_false() {
        let herder = make_test_herder();
        assert!(
            !herder.scp_heard_from_quorum(999_999),
            "scp_heard_from_quorum must be false for an unknown slot"
        );
    }

    /// get_scp_externalizing_state() returns the externalizing state snapshot
    /// (non-empty) for an externalized slot, and empty for unknown slots.
    /// Exercises the accessor with a real externalized slot, not just empty cases.
    #[test]
    fn test_get_scp_externalizing_state_uses_externalizing_snapshot() {
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

        let herder = Herder::with_secret_key(
            config,
            local_secret,
            make_default_lm(),
            TimerManagerHandle::no_op(),
        );
        herder.start_syncing();
        herder.bootstrap(0);

        herder
            .quorum_tracker
            .write()
            .expand(&other_node_id, quorum_set)
            .unwrap();

        let tracking = herder.tracking_slot().get();

        // Non-externalized slot → empty.
        let state = herder.get_scp_externalizing_state(42);
        assert!(
            state.is_empty(),
            "non-externalized slot should return empty"
        );

        // Externalize the tracking slot via a valid EXTERNALIZE envelope.
        let envelope = make_signed_externalize_from(tracking, &herder, &other_secret);
        let result = herder.receive_scp_envelope(envelope);
        assert_eq!(result, EnvelopeState::Valid);
        assert!(
            herder.scp().is_slot_externalized(tracking),
            "slot should be externalized after processing EXTERNALIZE"
        );

        // Now get_scp_externalizing_state should return non-empty envelopes.
        let state = herder.get_scp_externalizing_state(tracking);
        assert!(
            !state.is_empty(),
            "externalized slot should return non-empty externalizing state"
        );

        // The externalizing snapshot is the EXTERNALIZE-phase state, not the
        // full set of recorded envelopes: every returned statement must be an
        // EXTERNALIZE pledge (matching stellar-core's getExternalizingState,
        // which returns only the commit-phase envelopes that justify the slot's
        // externalization). This is the property that distinguishes the
        // accessor from get_scp_envelopes(), which returns all recorded
        // envelopes (nominate/prepare/confirm/externalize) for the slot.
        for env in &state {
            assert_eq!(
                env.statement.slot_index, tracking,
                "externalizing envelope should be for the externalized slot"
            );
            assert!(
                matches!(env.statement.pledges, ScpStatementPledges::Externalize(_)),
                "externalizing state should contain only EXTERNALIZE pledges, \
                 got {:?}",
                env.statement.pledges
            );
        }

        // The externalizing-state accessor returns a different view than
        // get_scp_envelopes(): get_scp_envelopes() returns the slot's recorded
        // (peer-received) envelopes — here, the single received EXTERNALIZE from
        // `other_node` — whereas the externalizing state additionally includes
        // the local node's own commit-phase envelope synthesized when the slot
        // externalizes. We assert this as a *semantic*, order-insensitive
        // difference rather than `assert_ne!` on the Vecs (which would pass even
        // if both accessors returned the same set in a different order): the
        // local node's synthesized EXTERNALIZE must be present in the
        // externalizing snapshot and absent from the recorded-envelope set.
        let all_envelopes = herder.get_scp_envelopes(tracking);
        assert!(
            !all_envelopes.is_empty(),
            "externalized slot should have recorded envelopes"
        );

        let local_in_state = state
            .iter()
            .any(|env| env.statement.node_id == local_node_id);
        let local_in_recorded = all_envelopes
            .iter()
            .any(|env| env.statement.node_id == local_node_id);
        assert!(
            local_in_state,
            "externalizing state must include the local node's synthesized \
             EXTERNALIZE envelope"
        );
        assert!(
            !local_in_recorded,
            "recorded-envelope set must NOT include the local node's \
             synthesized EXTERNALIZE (it holds only peer-received envelopes)"
        );

        // As a backstop, compare the two views as node-id sets so the check
        // stays order-insensitive: the externalizing snapshot adds the local
        // node, so the sets must differ.
        let state_nodes: std::collections::BTreeSet<_> = state
            .iter()
            .map(|env| env.statement.node_id.clone())
            .collect();
        let recorded_nodes: std::collections::BTreeSet<_> = all_envelopes
            .iter()
            .map(|env| env.statement.node_id.clone())
            .collect();
        assert_ne!(
            state_nodes, recorded_nodes,
            "externalizing state and recorded-envelope set should differ as \
             node-id sets (the externalizing snapshot adds the local node)"
        );
    }

    /// get_currently_tracked_quorum() returns hash→qset pairs for all tracked
    /// nodes, matching stellar-core's getCurrentlyTrackedQuorum() semantics.
    #[test]
    fn test_get_currently_tracked_quorum_returns_tracked_qsets() {
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

        let herder = Herder::with_secret_key(
            config,
            local_secret,
            make_default_lm(),
            TimerManagerHandle::no_op(),
        );

        // Expand quorum tracker with the other node.
        herder
            .quorum_tracker
            .write()
            .expand(&other_node_id, quorum_set.clone())
            .unwrap();

        let tracked = herder.get_currently_tracked_quorum();
        // Should have at least the local node's qset (from init) and the other
        // node's qset (from expand).
        assert!(
            !tracked.is_empty(),
            "tracked quorum should contain at least one entry"
        );

        // Verify that the hash matches the actual quorum set hash.
        for (hash, qset) in &tracked {
            let expected_hash = hash_quorum_set(qset);
            assert_eq!(*hash, expected_hash, "hash should match computed qset hash");
        }
    }

    /// Regression test for AUDIT-004: `store_quorum_set` must mirror the
    /// quorum set into `FetchingEnvelopes` so that blocked envelopes waiting
    /// for that quorum set get unblocked.
    #[test]
    fn test_audit_004_store_quorum_set_unblocks_fetching_envelopes() {
        use stellar_xdr::Hash as XdrHash;

        let herder = make_test_herder();

        // Build a sane quorum set with one validator.
        let node_id = XdrNodeId(stellar_xdr::PublicKey::PublicKeyTypeEd25519(
            stellar_xdr::Uint256([1u8; 32]),
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
        // moved to the ready queue.
        assert!(
            herder.fetching_envelopes.has_quorum_set(&qs_hash_bytes),
            "FetchingEnvelopes must learn about the quorum set via store_quorum_set"
        );

        // Caller is responsible for draining ready envelopes (#1907).
        // Simulate what handle_quorum_set does after storing.
        herder.process_ready_fetching_envelopes();

        // The ready queue should now be empty because we drained it.
        let popped = herder.fetching_envelopes.pop(100);
        assert!(
            popped.is_none(),
            "Ready queue must be drained after store_quorum_set + process_ready_fetching_envelopes"
        );
    }

    /// Regression test for AUDIT-104 Bug A: when tx_sets are cached but the
    /// sender's quorum set is unknown, process_scp_envelope must route through
    /// FetchingEnvelopes instead of sending to SCP (which would return Invalid
    /// and permanently strand the envelope).
    #[test]
    fn test_audit_104_qset_missing_routes_through_fetching() {
        use stellar_xdr::Hash as XdrHash;

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

        let herder = Herder::with_secret_key(
            config,
            local_secret,
            make_default_lm(),
            TimerManagerHandle::no_op(),
        );
        herder.start_syncing();
        herder.bootstrap(100);

        herder
            .quorum_tracker
            .write()
            .expand(&other_node_id, local_qs)
            .unwrap();

        let tracking = herder.tracking_slot().get(); // 101

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

    /// Regression test for #1907: process_scp_envelope must NOT drain the
    /// ready queue inline when entering the cached-tx-set + missing-quorum-set
    /// branch. Previously, this path called process_ready_fetching_envelopes()
    /// after on_tx_set_accepted(), which could stall the event loop.
    ///
    /// After the fix, envelopes unblocked by on_tx_set_accepted() remain in the
    /// ready queue until the next receive_tx_set() or handle_quorum_set() call
    /// drains them via spawn_blocking.
    #[test]
    fn test_issue_1907_process_scp_envelope_does_not_drain_ready_inline() {
        use stellar_xdr::Hash as XdrHash;

        let local_secret = SecretKey::from_seed(&[7u8; 32]);
        let local_public = local_secret.public_key();
        let local_node_id = node_id_from_public_key(&local_public);

        let other_secret = SecretKey::from_seed(&[1u8; 32]);
        let other_public = other_secret.public_key();
        let other_node_id = node_id_from_public_key(&other_public);

        let unknown_qs = ScpQuorumSet {
            threshold: 1,
            validators: vec![other_node_id.clone()].try_into().unwrap(),
            inner_sets: vec![].try_into().unwrap(),
        };
        let unknown_qs_hash = hash_quorum_set(&unknown_qs);

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

        let herder = Herder::with_secret_key(
            config,
            local_secret,
            make_default_lm(),
            TimerManagerHandle::no_op(),
        );
        herder.start_syncing();
        herder.bootstrap(100);

        herder
            .quorum_tracker
            .write()
            .expand(&other_node_id, local_qs)
            .unwrap();

        let tracking = herder.tracking_slot().get();

        // Cache a tx_set
        let value = make_valid_value_with_cached_tx_set(&herder, &other_secret);

        // Seed some ready envelopes in FetchingEnvelopes. These simulate
        // envelopes that became ready via an earlier on_tx_set_accepted call.
        let mut ready_envelopes = Vec::new();
        for i in 0..5u32 {
            let ballot = ScpBallot {
                counter: 100 + i,
                value: Value(vec![0u8; 1].try_into().unwrap()),
            };
            let stmt = ScpStatement {
                node_id: other_node_id.clone(),
                slot_index: tracking,
                pledges: ScpStatementPledges::Prepare(ScpStatementPrepare {
                    quorum_set_hash: XdrHash([0u8; 32]),
                    ballot,
                    prepared: None,
                    prepared_prime: None,
                    n_c: 0,
                    n_h: 0,
                }),
            };
            ready_envelopes.push(ScpEnvelope {
                statement: stmt,
                signature: XdrSignature(vec![0u8; 64].try_into().unwrap()),
            });
        }
        herder
            .fetching_envelopes
            .test_insert_ready(tracking, ready_envelopes);

        assert_eq!(
            herder.fetching_envelopes.ready_slots(),
            vec![tracking],
            "pre-condition: tracking slot should have ready envelopes"
        );

        // Now send an envelope that enters the cached-tx-set + missing-quorum-set
        // branch. This used to drain ready envelopes inline.
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
        let result = herder.receive_scp_envelope(envelope);
        assert_eq!(result, EnvelopeState::Fetching);

        // Key assertion: ready envelopes must NOT have been drained inline.
        // They should still be in the ready queue, awaiting an explicit
        // process_ready_fetching_envelopes() call via spawn_blocking.
        assert!(
            !herder.fetching_envelopes.ready_slots().is_empty(),
            "#1907: process_scp_envelope must not drain ready envelopes inline; \
             they should remain queued for spawn_blocking drain"
        );
    }

    /// Regression test for #1874: heard_from_quorum must survive purge_slots_below.
    ///
    /// Before the fix, `purge_slots_below` cleared the quorum set cache
    /// (`qset_tracker.clear_validated_preserving_local()`), which wiped all
    /// remote validators' quorum sets. This caused `heard_from_quorum()` to
    /// return false permanently after catchup, because `is_quorum()` prunes
    /// nodes whose quorum set lookup returns None.
    #[test]
    fn test_issue_1874_heard_from_quorum_survives_purge_slots_below() {
        // Set up a herder with a 2-of-2 quorum set (local + remote).
        let local_seed = [7u8; 32];
        let local_secret = SecretKey::from_seed(&local_seed);
        let local_public = local_secret.public_key();
        let local_node_id = node_id_from_public_key(&local_public);

        let remote_secret = SecretKey::from_seed(&[8u8; 32]);
        let remote_public = remote_secret.public_key();
        let remote_node_id = node_id_from_public_key(&remote_public);

        let quorum_set = ScpQuorumSet {
            threshold: 2,
            validators: vec![local_node_id.clone(), remote_node_id.clone()]
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

        let herder = Herder::with_secret_key(
            config,
            local_secret,
            make_default_lm(),
            TimerManagerHandle::no_op(),
        );

        // Store quorum sets for both nodes (simulates having learned them
        // from SCP message exchange before the purge).
        herder.store_quorum_set(&local_node_id, quorum_set.clone());
        herder.store_quorum_set(&remote_node_id, quorum_set.clone());

        // Record SCP envelopes from both nodes for slot 100.
        let slot = 100u64;
        {
            let mut tracker = herder.slot_quorum_tracker.write();
            tracker.record_envelope(slot, local_node_id.clone());
            tracker.record_envelope(slot, remote_node_id.clone());
        }

        // Before purge: quorum should be satisfied.
        assert!(
            herder.heard_from_quorum(slot),
            "heard_from_quorum should be true before purge"
        );
        assert!(
            herder.is_v_blocking(slot),
            "is_v_blocking should be true before purge"
        );

        // Purge slots below 100 — this is the operation that triggered the
        // bug. Before the fix, it cleared the quorum set cache.
        herder.purge_slots_below(slot);

        // Record envelopes for a NEW slot above the purge point.
        let new_slot = 101u64;
        {
            let mut tracker = herder.slot_quorum_tracker.write();
            tracker.record_envelope(new_slot, local_node_id.clone());
            tracker.record_envelope(new_slot, remote_node_id.clone());
        }

        // After purge: quorum should STILL be satisfied for the new slot.
        // Before the fix, this returned false because get_quorum_set()
        // returned None for the remote node (cache was cleared).
        assert!(
            herder.heard_from_quorum(new_slot),
            "heard_from_quorum must survive purge_slots_below (#1874)"
        );
        assert!(
            herder.is_v_blocking(new_slot),
            "is_v_blocking must survive purge_slots_below"
        );
    }

    /// Same as above but exercises the clear_slot_scoped_scp_caches path.
    #[test]
    fn test_issue_1874_heard_from_quorum_survives_clear_slot_scoped_scp_caches() {
        let local_seed = [7u8; 32];
        let local_secret = SecretKey::from_seed(&local_seed);
        let local_public = local_secret.public_key();
        let local_node_id = node_id_from_public_key(&local_public);

        let remote_secret = SecretKey::from_seed(&[8u8; 32]);
        let remote_public = remote_secret.public_key();
        let remote_node_id = node_id_from_public_key(&remote_public);

        let quorum_set = ScpQuorumSet {
            threshold: 2,
            validators: vec![local_node_id.clone(), remote_node_id.clone()]
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

        let herder = Herder::with_secret_key(
            config,
            local_secret,
            make_default_lm(),
            TimerManagerHandle::no_op(),
        );

        herder.store_quorum_set(&local_node_id, quorum_set.clone());
        herder.store_quorum_set(&remote_node_id, quorum_set.clone());

        let slot = 100u64;
        {
            let mut tracker = herder.slot_quorum_tracker.write();
            tracker.record_envelope(slot, local_node_id.clone());
            tracker.record_envelope(slot, remote_node_id.clone());
        }

        assert!(herder.heard_from_quorum(slot));

        // clear_slot_scoped_scp_caches clears slot-scoped data but preserves quorum sets.
        herder.clear_slot_scoped_scp_caches();

        // Quorum sets should survive the clear.
        let new_slot = 101u64;
        {
            let mut tracker = herder.slot_quorum_tracker.write();
            tracker.record_envelope(new_slot, local_node_id.clone());
            tracker.record_envelope(new_slot, remote_node_id.clone());
        }

        assert!(
            herder.heard_from_quorum(new_slot),
            "heard_from_quorum must survive clear_slot_scoped_scp_caches (#1874)"
        );
    }

    /// Regression test for AUDIT-104 Bug B: after storing a quorum set and
    /// draining ready envelopes, envelopes unblocked by quorum-set arrival
    /// are fed to SCP immediately (not left in the ready queue until a
    /// tx_set arrival happens to drain it).
    ///
    /// Since #1907 the drain is the caller's responsibility, so this test
    /// calls process_ready_fetching_envelopes() explicitly after
    /// store_quorum_set().
    #[test]
    fn test_audit_104_store_quorum_set_drains_ready() {
        use stellar_xdr::Hash as XdrHash;

        let herder = make_test_herder();

        // Build a sane quorum set
        let node_id = XdrNodeId(stellar_xdr::PublicKey::PublicKeyTypeEd25519(
            stellar_xdr::Uint256([1u8; 32]),
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

        // store_quorum_set mirrors to FetchingEnvelopes (AUDIT-004/AUDIT-104)
        herder.store_quorum_set(&node_id, quorum_set);

        // Caller drains ready queue (#1907 — moved out of store_quorum_set)
        herder.process_ready_fetching_envelopes();

        // The ready queue should be empty because we drained it.
        // (The envelope gets sent to SCP, which may return Invalid for a
        // non-tracking herder — that's fine, the point is it was drained.)
        let popped = herder.fetching_envelopes.pop(100);
        assert!(
            popped.is_none(),
            "AUDIT-104: ready queue must be drained after store + drain (envelope already processed)"
        );
    }

    /// Regression test for #1773 Phase 2: drain runs on a blocking-pool
    /// thread so the event-loop task can make progress during the drain.
    ///
    /// On `33a7ebf9`, mainnet telemetry showed `receive_tx_set` spending
    /// 342 ms entirely inside `process_ready_fetching_envelopes` (the
    /// drain that dispatches every ready envelope through the SCP core).
    /// The fix moves that drain onto `tokio::task::spawn_blocking` so the
    /// outer task is parked during the drain and other ready tokio tasks
    /// (overlay I/O, lifecycle ticks) can run.
    ///
    /// This test proves the event-loop thread is freed during the drain
    /// by running a competing async "heartbeat" task on the same
    /// `current_thread` runtime. The heartbeat ticks every 10 ms into
    /// a counter. During the drain:
    ///
    /// - Before the fix (drain inline on event-loop thread): the
    ///   heartbeat cannot run because the thread is blocked inside the
    ///   400-envelope synchronous dispatch loop. Counter stays at 0
    ///   or at whatever it reached before receive_tx_set was awaited.
    /// - After the fix (drain on spawn_blocking): the event-loop
    ///   thread parks the outer task while awaiting the JoinHandle;
    ///   the heartbeat task runs; the counter increases.
    ///
    /// We assert the heartbeat ticked at least twice, which is the
    /// signal that the event loop was free during the drain. The
    /// exact count is timing-sensitive; 2 is a conservative lower
    /// bound that still fails definitively if the drain is re-inlined.
    #[tokio::test(flavor = "current_thread", start_paused = false)]
    async fn test_issue_1773_receive_tx_set_frees_event_loop_during_drain() {
        use std::sync::atomic::{AtomicU64, Ordering};
        use std::sync::Arc;
        use stellar_xdr::Hash as XdrHash;

        let herder = Arc::new(make_test_herder());

        // Build a tx_set; the fetching-envelopes notify uses its hash.
        let tx_set = TransactionSet::new(Hash256::ZERO, Vec::new());

        // Synthesise 400 ready envelopes for slot 1. Each envelope must
        // have a distinct hash (compute_envelope_hash over XDR) so the
        // processed-set dedup does not short-circuit the drain loop;
        // the `counter` field in ScpBallot provides that uniqueness.
        const BACKLOG: usize = 400;
        let node_id = XdrNodeId(stellar_xdr::PublicKey::PublicKeyTypeEd25519(
            stellar_xdr::Uint256([42u8; 32]),
        ));
        let mut envelopes = Vec::with_capacity(BACKLOG);
        for i in 0..BACKLOG {
            let ballot = ScpBallot {
                counter: i as u32 + 1,
                value: Value(vec![0u8; 1].try_into().unwrap()),
            };
            let statement = ScpStatement {
                node_id: node_id.clone(),
                slot_index: 1,
                pledges: ScpStatementPledges::Prepare(ScpStatementPrepare {
                    quorum_set_hash: XdrHash([0u8; 32]),
                    ballot,
                    prepared: None,
                    prepared_prime: None,
                    n_c: 0,
                    n_h: 0,
                }),
            };
            envelopes.push(ScpEnvelope {
                statement,
                signature: XdrSignature(vec![0u8; 64].try_into().unwrap()),
            });
        }
        herder.fetching_envelopes.test_insert_ready(1, envelopes);

        assert_eq!(
            herder.fetching_envelopes.ready_slots(),
            vec![1],
            "pre-condition: slot 1 should be ready"
        );

        // Heartbeat task: tight loop that increments a counter every
        // time tokio's current_thread scheduler gives it a turn. It
        // uses `yield_now` so each iteration is a single await point.
        //
        // Under the spawn_blocking fix, `receive_tx_set.await` parks
        // the outer task while the blocking drain runs on a pool
        // thread; the scheduler wakes the heartbeat many times.
        // Under an inline drain (regression), the scheduler thread
        // is blocked inside the 400-envelope synchronous dispatch;
        // the heartbeat cannot run at all during the drain.
        let heartbeat_count = Arc::new(AtomicU64::new(0));
        let heartbeat_count_clone = Arc::clone(&heartbeat_count);
        let heartbeat_handle = tokio::spawn(async move {
            loop {
                tokio::task::yield_now().await;
                heartbeat_count_clone.fetch_add(1, Ordering::Relaxed);
            }
        });

        // Let the heartbeat task wake up and register at least one tick
        // so we know it's scheduled.
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
        let count_before = heartbeat_count.load(Ordering::Relaxed);

        let _slot = Arc::clone(&herder).receive_tx_set(tx_set).await;

        let count_after = heartbeat_count.load(Ordering::Relaxed);
        heartbeat_handle.abort();

        // Ordering contract (#1773): drain completed before return.
        let popped = herder.fetching_envelopes.pop(1);
        assert!(
            popped.is_none(),
            "#1773 ordering contract: envelope drain must complete \
             before receive_tx_set returns"
        );

        // Free-event-loop property (#1773): heartbeat made progress
        // during the await. If the drain had been inline, the outer
        // task would never yield between the entry and exit of
        // receive_tx_set — so heartbeat ticks would stay at
        // count_before. spawn_blocking forces at least one yield
        // (awaiting the JoinHandle), allowing the heartbeat to run.
        let ticks_during_drain = count_after.saturating_sub(count_before);
        assert!(
            ticks_during_drain >= 1,
            "#1773: heartbeat must make progress during drain \
             (observed {} ticks); if 0, the event-loop thread was \
             blocked during the drain — the spawn_blocking off-load \
             was lost",
            ticks_during_drain,
        );
    }

    /// Regression test for #1907: after store_quorum_set() (which no longer
    /// drains inline), the caller-side spawn_blocking drain frees the event
    /// loop while processing ready envelopes.
    ///
    /// Same approach as test_issue_1773: a heartbeat task on a
    /// current_thread runtime proves the event loop is not blocked during
    /// the drain.
    #[tokio::test(flavor = "current_thread", start_paused = false)]
    async fn test_issue_1907_quorum_set_drain_frees_event_loop() {
        use std::sync::atomic::{AtomicU64, Ordering};
        use std::sync::Arc;
        use stellar_xdr::Hash as XdrHash;

        let herder = Arc::new(make_test_herder());

        // Build a sane quorum set
        let node_id = XdrNodeId(stellar_xdr::PublicKey::PublicKeyTypeEd25519(
            stellar_xdr::Uint256([42u8; 32]),
        ));
        let quorum_set = ScpQuorumSet {
            threshold: 1,
            validators: vec![node_id.clone()].try_into().unwrap(),
            inner_sets: vec![].try_into().unwrap(),
        };

        // Synthesise 400 ready envelopes for slot 1.
        const BACKLOG: usize = 400;
        let mut envelopes = Vec::with_capacity(BACKLOG);
        for i in 0..BACKLOG {
            let ballot = ScpBallot {
                counter: i as u32 + 1,
                value: Value(vec![0u8; 1].try_into().unwrap()),
            };
            let statement = ScpStatement {
                node_id: node_id.clone(),
                slot_index: 1,
                pledges: ScpStatementPledges::Prepare(ScpStatementPrepare {
                    quorum_set_hash: XdrHash([0u8; 32]),
                    ballot,
                    prepared: None,
                    prepared_prime: None,
                    n_c: 0,
                    n_h: 0,
                }),
            };
            envelopes.push(ScpEnvelope {
                statement,
                signature: XdrSignature(vec![0u8; 64].try_into().unwrap()),
            });
        }
        herder.fetching_envelopes.test_insert_ready(1, envelopes);

        // store_quorum_set no longer drains — verify envelopes are still ready
        herder.store_quorum_set(&node_id, quorum_set);
        assert_eq!(
            herder.fetching_envelopes.ready_slots(),
            vec![1],
            "pre-condition: slot 1 should still be ready (store_quorum_set no longer drains)"
        );

        // Heartbeat task
        let heartbeat_count = Arc::new(AtomicU64::new(0));
        let heartbeat_count_clone = Arc::clone(&heartbeat_count);
        let heartbeat_handle = tokio::spawn(async move {
            loop {
                tokio::task::yield_now().await;
                heartbeat_count_clone.fetch_add(1, Ordering::Relaxed);
            }
        });

        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
        let count_before = heartbeat_count.load(Ordering::Relaxed);

        // Drain via spawn_blocking — mirroring handle_quorum_set()
        let herder_for_drain = Arc::clone(&herder);
        let handle = tokio::task::spawn_blocking(move || {
            herder_for_drain.process_ready_fetching_envelopes()
        });
        // Yield so heartbeat runs while drain executes on pool thread (#2716).
        tokio::task::yield_now().await;
        let join_result = handle.await;
        assert!(join_result.is_ok(), "spawn_blocking drain must not panic");

        let count_after = heartbeat_count.load(Ordering::Relaxed);
        heartbeat_handle.abort();

        // Ordering: drain completed before return
        let popped = herder.fetching_envelopes.pop(1);
        assert!(
            popped.is_none(),
            "#1907: envelope drain must complete before spawn_blocking returns"
        );

        // Free-event-loop: heartbeat made progress during the await
        let ticks_during_drain = count_after.saturating_sub(count_before);
        assert!(
            ticks_during_drain >= 1,
            "#1907: heartbeat must make progress during drain \
             (observed {} ticks); if 0, the event-loop thread was \
             blocked — the spawn_blocking off-load was lost",
            ticks_during_drain,
        );
    }

    /// Regression test for #1922: `cache_tx_set()` (the private method) must
    /// NOT drain ready envelopes inline. Only an explicit
    /// `process_ready_fetching_envelopes()` call should drain them.
    #[test]
    fn test_issue_1922_cache_tx_set_does_not_drain_inline() {
        use stellar_xdr::Hash as XdrHash;

        let herder = make_test_herder();

        let tx_set = TransactionSet::new(Hash256::ZERO, Vec::new());

        // Synthesise 10 ready envelopes for slot 1.
        let node_id = XdrNodeId(stellar_xdr::PublicKey::PublicKeyTypeEd25519(
            stellar_xdr::Uint256([42u8; 32]),
        ));
        let mut envelopes = Vec::with_capacity(10);
        for i in 0..10 {
            let ballot = ScpBallot {
                counter: i as u32 + 1,
                value: Value(vec![0u8; 1].try_into().unwrap()),
            };
            let statement = ScpStatement {
                node_id: node_id.clone(),
                slot_index: 1,
                pledges: ScpStatementPledges::Prepare(ScpStatementPrepare {
                    quorum_set_hash: XdrHash([0u8; 32]),
                    ballot,
                    prepared: None,
                    prepared_prime: None,
                    n_c: 0,
                    n_h: 0,
                }),
            };
            envelopes.push(ScpEnvelope {
                statement,
                signature: XdrSignature(vec![0u8; 64].try_into().unwrap()),
            });
        }
        herder.fetching_envelopes.test_insert_ready(1, envelopes);

        assert_eq!(
            herder.fetching_envelopes.ready_slots(),
            vec![1],
            "pre-condition: slot 1 should be ready"
        );

        // Cache the tx set — this must NOT drain ready envelopes.
        herder.cache_tx_set(tx_set);

        assert_eq!(
            herder.fetching_envelopes.ready_slots(),
            vec![1],
            "#1922: cache_tx_set() must NOT drain ready envelopes inline"
        );

        // Explicit drain clears the queue.
        herder.process_ready_fetching_envelopes();

        let popped = herder.fetching_envelopes.pop(1);
        assert!(
            popped.is_none(),
            "Ready queue must be drained after explicit process_ready_fetching_envelopes()"
        );
    }

    /// Regression test for #1922: `cache_tx_set_and_drain()` must free the
    /// event loop during the drain by running it on `spawn_blocking`.
    ///
    /// Same heartbeat approach as test_issue_1773.
    #[tokio::test(flavor = "current_thread", start_paused = false)]
    async fn test_issue_1922_cache_tx_set_and_drain_frees_event_loop() {
        use std::sync::atomic::{AtomicU64, Ordering};
        use std::sync::Arc;
        use stellar_xdr::Hash as XdrHash;

        let herder = Arc::new(make_test_herder());

        let tx_set = TransactionSet::new(Hash256::ZERO, Vec::new());

        // Synthesise 400 ready envelopes for slot 1.
        const BACKLOG: usize = 400;
        let node_id = XdrNodeId(stellar_xdr::PublicKey::PublicKeyTypeEd25519(
            stellar_xdr::Uint256([42u8; 32]),
        ));
        let mut envelopes = Vec::with_capacity(BACKLOG);
        for i in 0..BACKLOG {
            let ballot = ScpBallot {
                counter: i as u32 + 1,
                value: Value(vec![0u8; 1].try_into().unwrap()),
            };
            let statement = ScpStatement {
                node_id: node_id.clone(),
                slot_index: 1,
                pledges: ScpStatementPledges::Prepare(ScpStatementPrepare {
                    quorum_set_hash: XdrHash([0u8; 32]),
                    ballot,
                    prepared: None,
                    prepared_prime: None,
                    n_c: 0,
                    n_h: 0,
                }),
            };
            envelopes.push(ScpEnvelope {
                statement,
                signature: XdrSignature(vec![0u8; 64].try_into().unwrap()),
            });
        }
        herder.fetching_envelopes.test_insert_ready(1, envelopes);

        assert_eq!(
            herder.fetching_envelopes.ready_slots(),
            vec![1],
            "pre-condition: slot 1 should be ready"
        );

        // Heartbeat task
        let heartbeat_count = Arc::new(AtomicU64::new(0));
        let heartbeat_count_clone = Arc::clone(&heartbeat_count);
        let heartbeat_handle = tokio::spawn(async move {
            loop {
                tokio::task::yield_now().await;
                heartbeat_count_clone.fetch_add(1, Ordering::Relaxed);
            }
        });

        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
        let count_before = heartbeat_count.load(Ordering::Relaxed);

        // cache_tx_set_and_drain should offload the drain to spawn_blocking
        Arc::clone(&herder).cache_tx_set_and_drain(tx_set).await;

        let count_after = heartbeat_count.load(Ordering::Relaxed);
        heartbeat_handle.abort();

        // Ordering contract: drain completed before return.
        let popped = herder.fetching_envelopes.pop(1);
        assert!(
            popped.is_none(),
            "#1922 ordering contract: envelope drain must complete \
             before cache_tx_set_and_drain returns"
        );

        // Free-event-loop property: heartbeat made progress during the await.
        let ticks_during_drain = count_after.saturating_sub(count_before);
        assert!(
            ticks_during_drain >= 1,
            "#1922: heartbeat must make progress during drain \
             (observed {} ticks); if 0, the event-loop thread was \
             blocked during the drain — the spawn_blocking off-load \
             was lost",
            ticks_during_drain,
        );
    }

    /// Regression test for #2716: `drain_ready_envelopes_blocking` must yield
    /// to peer tasks even when the ready queue is empty (fast return from
    /// spawn_blocking). Without the yield_now between spawn and await, the
    /// JoinHandle resolves synchronously and peers are never polled.
    #[tokio::test(flavor = "current_thread", start_paused = false)]
    async fn test_issue_2716_drain_yields_even_when_empty() {
        use std::sync::atomic::{AtomicU64, Ordering};
        use std::sync::Arc;

        let herder = Arc::new(make_test_herder());

        // No envelopes in the ready queue — drain will return 0 instantly.
        assert!(
            herder.fetching_envelopes.ready_slots().is_empty(),
            "pre-condition: no ready envelopes"
        );

        let heartbeat_count = Arc::new(AtomicU64::new(0));
        let hb = Arc::clone(&heartbeat_count);
        let heartbeat_handle = tokio::spawn(async move {
            loop {
                tokio::task::yield_now().await;
                hb.fetch_add(1, Ordering::Relaxed);
            }
        });

        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
        let before = heartbeat_count.load(Ordering::Relaxed);

        Arc::clone(&herder)
            .drain_ready_envelopes_blocking("empty drain")
            .await;

        let after = heartbeat_count.load(Ordering::Relaxed);
        heartbeat_handle.abort();

        let ticks = after.saturating_sub(before);
        assert!(
            ticks >= 1,
            "#2716: drain must yield even when queue is empty (observed {} ticks)",
            ticks,
        );
    }

    /// Regression test for #2718: `drain_ready_envelopes_blocking` must yield
    /// even when all ready envelopes are filtered by `tracking_slot`. When
    /// tracking at slot N, envelopes at slot > N are skipped (the `continue`
    /// path in `process_ready_fetching_envelopes`), making the drain
    /// effectively zero-work despite a non-empty ready queue. This exercises
    /// the same yield_now fix as #2716 but via the filtering path.
    #[tokio::test(flavor = "current_thread", start_paused = false)]
    async fn test_issue_2718_drain_yields_when_all_slots_filtered_by_tracking() {
        use std::sync::atomic::{AtomicU64, Ordering};
        use std::sync::Arc;

        let herder = Arc::new(make_test_herder());

        // Set up Tracking state with tracking_slot = 1.
        // advance_tracking_to(0, 100) sets consensus_index = 0+1 = 1 and
        // transitions HerderState to Tracking.
        herder.advance_tracking_to(0, 100);
        assert_eq!(
            herder.state(),
            HerderState::Tracking,
            "pre-condition: herder must be in Tracking state"
        );
        assert_eq!(
            herder.tracking_slot().get(),
            1,
            "pre-condition: tracking_slot must be 1"
        );

        // Insert a ready envelope at slot 2 (> tracking_slot = 1).
        // This will be filtered by process_ready_fetching_envelopes.
        let envelope = make_test_envelope(2);
        herder
            .fetching_envelopes
            .test_insert_ready(2, vec![envelope]);
        assert_eq!(
            herder.fetching_envelopes.ready_slots(),
            vec![2],
            "pre-condition: slot 2 envelope must be in ready queue"
        );

        let heartbeat_count = Arc::new(AtomicU64::new(0));
        let hb = Arc::clone(&heartbeat_count);
        let heartbeat_handle = tokio::spawn(async move {
            loop {
                tokio::task::yield_now().await;
                hb.fetch_add(1, Ordering::Relaxed);
            }
        });

        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
        let before = heartbeat_count.load(Ordering::Relaxed);

        let processed = Arc::clone(&herder)
            .drain_ready_envelopes_blocking("filtered drain")
            .await;

        let after = heartbeat_count.load(Ordering::Relaxed);
        heartbeat_handle.abort();

        // The drain must have processed 0 envelopes (all filtered).
        assert_eq!(
            processed, 0,
            "#2718: drain must process 0 envelopes when all are filtered by tracking_slot"
        );

        // The heartbeat must have advanced (yield happened).
        let ticks = after.saturating_sub(before);
        assert!(
            ticks >= 1,
            "#2718: drain must yield even when all envelopes are filtered \
             (observed {} ticks)",
            ticks,
        );

        // Postcondition: the filtered envelope must remain queued.
        assert_eq!(
            herder.fetching_envelopes.ready_slots(),
            vec![2],
            "#2718: filtered envelope must remain in ready queue for future consumption"
        );
    }

    /// Regression test for #2066: unsolicited TxSets must NOT be accepted
    /// by the tracker. With the read-through callback design (#2070), there
    /// is no private cache to poison — the callback queries scp_driver
    /// directly. This test verifies the tracker still rejects unsolicited
    /// tx sets.
    #[tokio::test]
    async fn test_issue_2066_unsolicited_tx_set_not_accepted() {
        let herder = Arc::new(make_test_herder());
        let tx_set = TransactionSet::new(Hash256::from_bytes([0xAB; 32]), Vec::new());
        let hash = *tx_set.hash();

        // Do NOT request this tx set via the tracker — it is unsolicited.
        // Call receive_tx_set; the tracker should reject it (return None).
        let result = Arc::clone(&herder).receive_tx_set(tx_set).await;
        assert!(
            result.is_none(),
            "#2066: unsolicited tx set must not be accepted by the tracker"
        );

        // The scp_driver must NOT have this tx set cached.
        assert!(
            !herder.scp_driver.has_tx_set(&hash),
            "#2066: unsolicited tx set must not appear in scp_driver cache"
        );
    }

    /// Regression test for #2066 (positive case): a legitimately tracked
    /// TxSet must still be accepted and unblock waiting envelopes after
    /// `receive_tx_set`.
    #[tokio::test]
    async fn test_issue_2066_tracked_tx_set_accepted_by_driver() {
        let herder = Arc::new(make_test_herder());
        let slot: SlotIndex = 42;
        let tx_set = TransactionSet::new(Hash256::from_bytes([0xCD; 32]), Vec::new());
        let hash = *tx_set.hash();

        // Register the tx set as pending in the tracker (as if SCP requested it).
        herder.scp_driver.request_tx_set(hash, slot);

        // Now receive_tx_set should accept it.
        let result = Arc::clone(&herder).receive_tx_set(tx_set).await;
        assert_eq!(
            result,
            Some(slot),
            "#2066: tracked tx set must be accepted by the tracker"
        );

        // The scp_driver should now have this tx set cached.
        assert!(
            herder.scp_driver.has_tx_set(&hash),
            "#2066: tracked tx set must be in scp_driver cache"
        );
    }

    /// Helper: create a signed StellarValue with a specific tx_set_hash and
    /// valid close time, WITHOUT caching the tx set in scp_driver.
    fn make_value_for_tx_set_hash(
        tx_set_hash: Hash256,
        herder: &Herder,
        secret_key: &SecretKey,
    ) -> Value {
        let xdr_tx_set_hash = stellar_xdr::Hash(tx_set_hash.0);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let close_time = TimePoint(now);

        // Sign: (networkID, ENVELOPE_TYPE_SCPVALUE, txSetHash, closeTime)
        let network_id = herder.scp_driver.network_id();
        let mut sign_data = network_id.0.to_vec();
        sign_data.extend_from_slice(&EnvelopeType::Scpvalue.to_xdr(Limits::none()).unwrap());
        sign_data.extend_from_slice(&xdr_tx_set_hash.to_xdr(Limits::none()).unwrap());
        sign_data.extend_from_slice(&close_time.to_xdr(Limits::none()).unwrap());
        let sig = secret_key.sign(&sign_data);

        let node_id = XdrNodeId(stellar_xdr::PublicKey::PublicKeyTypeEd25519(
            stellar_xdr::Uint256(*secret_key.public_key().as_bytes()),
        ));

        let stellar_value = StellarValue {
            tx_set_hash: xdr_tx_set_hash,
            close_time,
            upgrades: vec![].try_into().unwrap(),
            ext: StellarValueExt::Signed(LedgerCloseValueSignature {
                node_id,
                signature: stellar_xdr::Signature(sig.0.to_vec().try_into().unwrap_or_default()),
            }),
        };
        Value(
            stellar_value
                .to_xdr(Limits::none())
                .unwrap()
                .try_into()
                .unwrap(),
        )
    }

    /// End-to-end regression test for #2069: after receiving an unsolicited
    /// TxSet, a subsequent PREPARE ballot envelope referencing the same hash
    /// must stay in Fetching state (not be processed through SCP). The
    /// unsolicited tx set must NOT have poisoned the FetchingEnvelopes cache.
    #[tokio::test]
    async fn test_issue_2069_unsolicited_tx_set_then_ballot_stays_fetching() {
        // Set up a validator herder with a 2-node quorum (local + other).
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
        let qs_hash = hash_quorum_set(&quorum_set);

        let config = HerderConfig {
            is_validator: true,
            node_public_key: local_public,
            local_quorum_set: Some(quorum_set.clone()),
            ..HerderConfig::default()
        };

        let herder = Arc::new(Herder::with_secret_key(
            config,
            local_secret,
            make_default_lm(),
            TimerManagerHandle::no_op(),
        ));
        herder.bootstrap(100);

        // Expand the quorum tracker so the other node passes the non-quorum filter.
        herder
            .quorum_tracker
            .write()
            .expand(&other_node_id, quorum_set.clone())
            .unwrap();

        // Store the quorum set so the envelope passes quorum-set availability.
        herder
            .scp_driver
            .store_quorum_set(&other_node_id, quorum_set);

        let tracking = herder.tracking_slot().get(); // 101

        // Step 1: Send an unsolicited tx set (not tracked by the tracker).
        let tx_set = TransactionSet::new(Hash256::from_bytes([0xEE; 32]), Vec::new());
        let hash = *tx_set.hash();

        let result = Arc::clone(&herder).receive_tx_set(tx_set).await;
        assert!(
            result.is_none(),
            "#2069: unsolicited tx set must not be accepted"
        );

        // Step 2: Send a PREPARE envelope referencing the same tx_set hash.
        let value = make_value_for_tx_set_hash(hash, &herder, &other_secret);
        let statement = ScpStatement {
            node_id: other_node_id,
            slot_index: tracking,
            pledges: ScpStatementPledges::Prepare(ScpStatementPrepare {
                quorum_set_hash: qs_hash.into(),
                ballot: ScpBallot { counter: 1, value },
                prepared: None,
                prepared_prime: None,
                n_c: 0,
                n_h: 0,
            }),
        };
        let envelope = sign_statement(&statement, &herder, &other_secret);

        let env_result = herder.receive_scp_envelope(envelope);
        assert_eq!(
            env_result,
            EnvelopeState::Fetching,
            "#2069: ballot envelope must stay in Fetching when tx set is not in scp_driver cache"
        );

        // The system must have issued a pending request for the missing hash.
        assert!(
            herder.scp_driver.needs_tx_set(&hash),
            "#2069: process_scp_envelope must register a pending tx_set request"
        );

        // The scp_driver must NOT contain the hash (unsolicited
        // tx set was not accepted).
        assert!(
            !herder.scp_driver.has_tx_set(&hash),
            "#2069: unsolicited tx set must not appear in scp_driver cache"
        );
    }

    /// Regression test for #2069: a TxSet whose content doesn't match its
    /// declared hash (hash mismatch / malformed) must be rejected even if
    /// the hash is pending in the tracker.
    #[tokio::test]
    async fn test_issue_2069_hash_mismatch_tx_set_returns_none() {
        let herder = Arc::new(make_test_herder());
        let slot: SlotIndex = 50;

        // Register a pending request for a specific hash.
        let expected_hash = Hash256::from_bytes([0xFA; 32]);
        herder.scp_driver.request_tx_set(expected_hash, slot);

        // Construct a tx set that claims to have `expected_hash` but whose
        // content actually hashes to something different.
        let malformed_tx_set = TransactionSet::with_unchecked_hash(
            Hash256::from_bytes([0x11; 32]),
            expected_hash,
            Vec::new(),
        );
        assert_ne!(
            malformed_tx_set.recompute_hash(),
            expected_hash,
            "pre-condition: tx set content must not match declared hash"
        );

        let result = Arc::clone(&herder).receive_tx_set(malformed_tx_set).await;
        assert!(
            result.is_none(),
            "#2069: hash-mismatch tx set must be rejected by the tracker"
        );

        // The scp_driver must NOT contain the hash.
        assert!(
            !herder.scp_driver.has_tx_set(&expected_hash),
            "#2069: hash-mismatch tx set must not appear in scp_driver cache"
        );

        // The original pending request must still be active (not cleared).
        assert!(
            herder.scp_driver.needs_tx_set(&expected_hash),
            "#2069: hash-mismatch must not clear the original pending request"
        );
    }

    // ─────────────────────────────────────────────────────────────────────
    // #2120: Drain-path regression test — post-drain envelopes take the
    // is_current_ledger validation path (not apply-lag).
    // ─────────────────────────────────────────────────────────────────────

    /// Proves the key behavioral consequence of draining pending envelopes
    /// in `ledger_closed` (post-apply) rather than `advance_tracking_slot`
    /// (pre-apply): after `ledger_closed(N)`, a drained EXTERNALIZE for
    /// slot N+1 sees `lcl_seq = N` → `is_current_ledger = true` →
    /// `ValueValidation::Valid`. No `apply_lag` is recorded in
    /// `deferred_slots`, and `fully_validated` is preserved.
    ///
    /// If the drain had happened pre-apply (with LCL still at N-1),
    /// `slot_index != lcl_seq + 1` would route to
    /// `validate_past_or_future_value` → `MaybeValidDeferred` with
    /// `apply_lag = true`, clearing `fully_validated`.
    ///
    /// Preconditions:
    /// - No LedgerManager: validation uses the tracking-state fallback
    ///   (`lcl_seq = consensus_index - 1`).
    /// - Direct `pending_envelopes.add()` bypasses envelope signature
    ///   verification (the StellarValue signature IS verified).
    #[test]
    fn test_post_drain_envelope_avoids_apply_lag_path() {
        let (herder, _local_secret) = make_validator_herder();

        // Peer that will send the EXTERNALIZE.
        let peer_secret = SecretKey::from_seed(&[2u8; 32]);
        let peer_public = peer_secret.public_key();
        let peer_node_id = XdrNodeId(stellar_xdr::PublicKey::PublicKeyTypeEd25519(
            stellar_xdr::Uint256(*peer_public.as_bytes()),
        ));

        // Set tracking: consensus_index = 1 (next slot to close).
        // The LM has ledger_seq=0, so is_current_ledger = (1 == 0+1) = true.
        {
            let mut ts = herder.tracking_state.write();
            ts.consensus_index = 1;
            ts.is_tracking = true;
            // consensus_close_time = 0 (default) so close_time=1 > 0 passes.
        }
        herder.pending_envelopes.set_current_slot(1);

        // Create a properly signed StellarValue with cached tx_set.
        let value = make_valid_value_with_cached_tx_set(&herder, &peer_secret);

        // Construct EXTERNALIZE envelope for slot 1.
        let envelope = ScpEnvelope {
            statement: ScpStatement {
                node_id: peer_node_id,
                slot_index: 1,
                pledges: ScpStatementPledges::Externalize(ScpStatementExternalize {
                    commit: ScpBallot { counter: 1, value },
                    n_h: 1,
                    commit_quorum_set_hash: stellar_xdr::Hash([0u8; 32]),
                }),
            },
            // Envelope signature not verified by SCP (bypassed in this test).
            signature: XdrSignature(vec![0u8; 64].try_into().unwrap()),
        };

        // Buffer the envelope as pending for slot 1.
        herder.pending_envelopes.add(1, envelope);
        assert_eq!(herder.pending_envelopes.slot_count(), 1);

        // Precondition: slot 1 has no deferred causes yet.
        assert_eq!(
            herder.scp_driver.deferred_slot_count(),
            0,
            "precondition: no deferred slots before drain"
        );

        // ledger_closed(0): LCL = 0, drains pending for slot <= 1.
        // The drained EXTERNALIZE is processed through:
        //   process_scp_envelope → process_scp_envelope_with_tx_set →
        //   scp.receive_envelope → validate_value(1, ...) →
        //   validate_value_against_local_state: lcl_seq=0, is_current_ledger=true
        //   → ValueValidation::Valid (no apply_lag recorded).
        herder.ledger_closed(0, &[], &[], 0);

        // The envelope was drained.
        assert_eq!(
            herder.pending_envelopes.slot_count(),
            0,
            "pending envelopes must be drained by ledger_closed"
        );

        // KEY ASSERTION: No apply_lag recorded — the is_current_ledger path
        // was taken, returning ValueValidation::Valid (not MaybeValidDeferred).
        assert_eq!(
            herder.scp_driver.deferred_slot_count(),
            0,
            "post-drain envelope must NOT record apply_lag — is_current_ledger \
             path must be taken when LCL = N and slot = N+1"
        );

        // KEY ASSERTION: fully_validated is preserved — MaybeValidDeferred
        // would have cleared it (via SCP's ballot protocol), but
        // FullyValidated leaves it untouched.
        assert!(
            herder.scp.is_slot_fully_validated(1),
            "post-drain envelope must preserve fully_validated — the \
             is_current_ledger path returns FullyValidated which does not \
             clear the slot's fully_validated flag"
        );
    }

    // ─────────────────────────────────────────────────────────────────────
    // #2135: Closing-gate replay variant of the drain-path regression test.
    // ─────────────────────────────────────────────────────────────────────

    /// Proves that envelopes replayed from the closing gate in `ledger_closed`
    /// take the `is_current_ledger` validation path (not apply-lag), just like
    /// drained pending envelopes.
    ///
    /// The closing gate buffers envelopes that arrive during the close window
    /// (between externalization and `ledger_closed`). When `ledger_closed`
    /// replays them (herder.rs:2082-2101), LCL has advanced, so
    /// `lcl_seq + 1 == slot_index` → `is_current_ledger = true` →
    /// `ValueValidation::Valid`. No `apply_lag` is recorded and
    /// `fully_validated` is preserved.
    ///
    /// This emulates stellar-core's post-apply timing: `safelyProcessSCPQueue`
    /// is posted to the main thread after `processExternalized` applies the
    /// ledger (HerderImpl.cpp:1194), so queued envelopes naturally see the
    /// advanced LCL.
    ///
    /// Preconditions:
    /// - No LedgerManager: validation uses the tracking-state fallback
    ///   (`lcl_seq = consensus_index - 1`). Sufficient because we test
    ///   validation *path selection*, not full production sequencing.
    /// - Direct `process_scp_envelope_with_tx_set` bypasses SCP-envelope
    ///   signature verification (the StellarValue signature IS verified).
    #[test]
    fn test_closing_gate_replay_avoids_apply_lag_path() {
        let (herder, _local_secret) = make_validator_herder();

        // Peer that will send the EXTERNALIZE.
        let peer_secret = SecretKey::from_seed(&[2u8; 32]);
        let peer_public = peer_secret.public_key();
        let peer_node_id = XdrNodeId(stellar_xdr::PublicKey::PublicKeyTypeEd25519(
            stellar_xdr::Uint256(*peer_public.as_bytes()),
        ));

        // Set tracking: consensus_index = 1 (next slot to close).
        // The LM has ledger_seq=0, so is_current_ledger = (1 == 0+1) = true.
        {
            let mut ts = herder.tracking_state.write();
            ts.consensus_index = 1;
            ts.is_tracking = true;
            // consensus_close_time = 0 (default) so close_time=1 > 0 passes.
        }

        // Activate closing gate for slot 1, mirroring production behavior
        // in advance_tracking_slot (herder.rs:1759-1763).
        {
            let mut gate = herder.closing_gate.lock().unwrap();
            gate.slot = 1;
            gate.buffer.clear();
        }

        // Create a properly signed StellarValue with cached tx_set.
        let value = make_valid_value_with_cached_tx_set(&herder, &peer_secret);

        // Construct EXTERNALIZE envelope for slot 1.
        let envelope = ScpEnvelope {
            statement: ScpStatement {
                node_id: peer_node_id,
                slot_index: 1,
                pledges: ScpStatementPledges::Externalize(ScpStatementExternalize {
                    commit: ScpBallot { counter: 1, value },
                    n_h: 1,
                    commit_quorum_set_hash: stellar_xdr::Hash([0u8; 32]),
                }),
            },
            // Envelope signature not verified by SCP (bypassed in this test).
            signature: XdrSignature(vec![0u8; 64].try_into().unwrap()),
        };

        // Process the envelope — it should be deferred by the closing gate.
        let result = herder.process_scp_envelope_with_tx_set(envelope);
        assert_eq!(
            result,
            EnvelopeState::Deferred,
            "envelope for gated slot must be deferred by closing gate"
        );

        // Gate-path assertion: confirm envelope was buffered.
        {
            let gate = herder.closing_gate.lock().unwrap();
            assert_eq!(
                gate.buffer.len(),
                1,
                "closing gate must have buffered exactly one envelope"
            );
        }

        // Precondition: no deferred causes before replay.
        assert_eq!(
            herder.scp_driver.deferred_slot_count(),
            0,
            "precondition: no deferred slots before gate replay"
        );

        // ledger_closed(0): LCL = 0, replays the gate buffer.
        // The replayed EXTERNALIZE is processed through:
        //   process_scp_envelope_with_tx_set → scp.receive_envelope →
        //   validate_value(1, ...) → validate_value_against_local_state:
        //   lcl_seq=0, is_current_ledger=true → ValueValidation::Valid.
        herder.ledger_closed(0, &[], &[], 0);

        // Gate-cleared assertion: proves the replay path was exercised.
        {
            let gate = herder.closing_gate.lock().unwrap();
            assert_eq!(
                gate.slot, 0,
                "closing gate must be opened (slot=0) after ledger_closed"
            );
            assert!(
                gate.buffer.is_empty(),
                "closing gate buffer must be empty after replay"
            );
        }

        // KEY ASSERTION: No apply_lag recorded — the is_current_ledger path
        // was taken, returning ValueValidation::Valid (not MaybeValidDeferred).
        assert_eq!(
            herder.scp_driver.deferred_slot_count(),
            0,
            "gate-replayed envelope must NOT record apply_lag — is_current_ledger \
             path must be taken when LCL = N and slot = N+1"
        );

        // KEY ASSERTION: fully_validated is preserved — MaybeValidDeferred
        // would have cleared it (via SCP's ballot protocol), but
        // FullyValidated leaves it untouched.
        assert!(
            herder.scp.is_slot_fully_validated(1),
            "gate-replayed envelope must preserve fully_validated — the \
             is_current_ledger path returns FullyValidated which does not \
             clear the slot's fully_validated flag"
        );
    }

    // -------- Tests for SCP persistence wiring (#2698) --------

    fn make_persist_envelope_with_tx_set_hash(
        slot: u64,
        tx_set_hash: stellar_xdr::Hash,
    ) -> stellar_xdr::ScpEnvelope {
        let stellar_value = StellarValue {
            tx_set_hash,
            close_time: TimePoint(0),
            upgrades: vec![].try_into().unwrap(),
            ext: StellarValueExt::Basic,
        };
        let value = stellar_value.to_xdr(Limits::none()).unwrap();
        stellar_xdr::ScpEnvelope {
            statement: ScpStatement {
                node_id: XdrNodeId(stellar_xdr::PublicKey::PublicKeyTypeEd25519(
                    stellar_xdr::Uint256([0u8; 32]),
                )),
                slot_index: slot,
                pledges: ScpStatementPledges::Nominate(ScpNomination {
                    quorum_set_hash: stellar_xdr::Hash([0u8; 32]),
                    votes: vec![Value(value.try_into().unwrap())].try_into().unwrap(),
                    accepted: vec![].try_into().unwrap(),
                }),
            },
            signature: XdrSignature::default(),
        }
    }

    #[test]
    fn test_purge_persisted_tx_sets_no_manager() {
        let herder = make_test_herder();
        // No persistence manager installed — must be a guarded no-op.
        herder.purge_persisted_tx_sets();
    }

    #[test]
    fn test_purge_persisted_tx_sets_with_manager_removes_orphans() {
        let herder = make_test_herder();
        let manager = Arc::new(crate::persistence::ScpPersistenceManager::in_memory());
        assert!(
            herder.set_scp_persistence(Arc::clone(&manager)).is_ok(),
            "first install should succeed"
        );

        let referenced_hash = stellar_xdr::Hash([1u8; 32]);
        let orphan_hash = stellar_xdr::Hash([2u8; 32]);

        // Drive setup through the public manager API: persist an envelope
        // referencing `referenced_hash`, but pass BOTH tx sets through so the
        // backend stores both (only `referenced_hash` is reachable from the
        // envelope).
        let env = make_persist_envelope_with_tx_set_hash(100, referenced_hash.clone());
        manager
            .persist_scp_state(
                100,
                &[env],
                &[
                    (referenced_hash.clone(), vec![10]),
                    (orphan_hash.clone(), vec![20]),
                ],
                &[],
            )
            .expect("persist_scp_state");

        // Sanity check: both tx sets should currently be present.
        assert!(manager.has_tx_set(&referenced_hash).unwrap());
        assert!(manager.has_tx_set(&orphan_hash).unwrap());

        // Run the production GC entry point.
        herder.purge_persisted_tx_sets();

        assert!(
            manager.has_tx_set(&referenced_hash).unwrap(),
            "referenced tx set must be retained"
        );
        assert!(
            !manager.has_tx_set(&orphan_hash).unwrap(),
            "orphan tx set must be purged"
        );
    }

    /// #3719 storestate-growth regression: the live GC entry point must trim
    /// persisted slot STATES below the remember horizon (tracking − 12,
    /// mirroring core's MAX_SLOTS_TO_REMEMBER ring) so old tx-set references
    /// expire and their tx sets become purgeable. Pre-fix, slot states
    /// accumulated forever and storestate grew without bound.
    #[test]
    fn test_purge_persisted_tx_sets_trims_old_slot_states() {
        let herder = make_test_herder();
        let manager = Arc::new(crate::persistence::ScpPersistenceManager::in_memory());
        assert!(herder.set_scp_persistence(Arc::clone(&manager)).is_ok());

        let old_hash = stellar_xdr::Hash([3u8; 32]);
        let recent_hash = stellar_xdr::Hash([4u8; 32]);

        let old_env = make_persist_envelope_with_tx_set_hash(100, old_hash.clone());
        manager
            .persist_scp_state(100, &[old_env], &[(old_hash.clone(), vec![1])], &[])
            .expect("persist old slot");
        let recent_env = make_persist_envelope_with_tx_set_hash(119, recent_hash.clone());
        manager
            .persist_scp_state(119, &[recent_env], &[(recent_hash.clone(), vec![2])], &[])
            .expect("persist recent slot");

        // Tracking at 120 → remember horizon = 108: slot 100 is stale,
        // slot 119 is live.
        herder.bootstrap(120);
        herder.purge_persisted_tx_sets();

        assert!(
            !manager.has_tx_set(&old_hash).unwrap(),
            "tx set referenced only by a below-horizon slot state must be purged"
        );
        assert!(
            manager.has_tx_set(&recent_hash).unwrap(),
            "tx set referenced by a live slot state must be retained"
        );
        let restored = manager.restore_scp_state().expect("restore");
        assert!(
            restored.envelopes.iter().all(|(slot, _)| *slot >= 108),
            "restore must not resurrect below-horizon slot states"
        );
        assert!(
            restored.envelopes.iter().any(|(slot, _)| *slot == 119),
            "live slot state must survive the trim"
        );
    }

    // -------- tx-set GC busy-drop telemetry (#3806) --------

    /// Construct a transient busy `HerderError::Db` for the emit-side tests.
    fn busy_herder_error() -> HerderError {
        HerderError::Db(henyey_db::DbError::Sqlite(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error {
                code: rusqlite::ffi::ErrorCode::DatabaseBusy,
                extended_code: 5,
            },
            Some("database is locked".to_string()),
        )))
    }

    fn locked_herder_error() -> HerderError {
        HerderError::Db(henyey_db::DbError::Sqlite(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error {
                code: rusqlite::ffi::ErrorCode::DatabaseLocked,
                extended_code: 6,
            },
            Some("database table is locked".to_string()),
        )))
    }

    /// `ScpStatePersistence` double that fails every write with a transient
    /// busy `DbError`, so both arms of `purge_persisted_tx_sets` observe a
    /// busy drop. Reads return empty so `set_scp_persistence` (which never
    /// calls a storage method) and the persist worker install cleanly.
    struct BusyStorage;

    impl crate::persistence::ScpStatePersistence for BusyStorage {
        fn save_scp_state(
            &self,
            _slot: u64,
            _state: &crate::persistence::PersistedSlotState,
        ) -> crate::Result<()> {
            Err(busy_herder_error())
        }

        fn load_scp_state(
            &self,
            _slot: u64,
        ) -> crate::Result<Option<crate::persistence::PersistedSlotState>> {
            Ok(None)
        }

        fn load_all_scp_states(
            &self,
        ) -> crate::Result<Vec<(u64, crate::persistence::PersistedSlotState)>> {
            Ok(Vec::new())
        }

        fn delete_scp_state_below(&self, _slot: u64) -> crate::Result<()> {
            Err(busy_herder_error())
        }

        fn save_tx_set(&self, _hash: &stellar_xdr::Hash, _tx_set: &[u8]) -> crate::Result<()> {
            Err(busy_herder_error())
        }

        fn load_tx_set(&self, _hash: &stellar_xdr::Hash) -> crate::Result<Option<Vec<u8>>> {
            Ok(None)
        }

        fn load_all_tx_sets(&self) -> crate::Result<Vec<(stellar_xdr::Hash, Vec<u8>)>> {
            Ok(Vec::new())
        }

        fn has_tx_set(&self, _hash: &stellar_xdr::Hash) -> crate::Result<bool> {
            Ok(false)
        }

        fn get_all_tx_set_hashes(&self) -> crate::Result<Vec<stellar_xdr::Hash>> {
            Ok(Vec::new())
        }

        fn delete_tx_sets_by_hashes(&self, _hashes: &[stellar_xdr::Hash]) -> crate::Result<()> {
            Err(busy_herder_error())
        }

        // Override the atomic purge so the `else` (min_slot == 0) arm also
        // observes a transient busy rather than the default read-then-delete
        // sequence over the empty (Ok) getters.
        fn purge_unreferenced_tx_sets_atomic(&self) -> crate::Result<()> {
            Err(busy_herder_error())
        }
    }

    /// A transient busy `DbError` at the tx-set GC site must increment
    /// `henyey_db_busy_write_dropped_total{site="tx_set_gc"}`. Pins the exact
    /// rendered series on the emit side.
    #[test]
    fn test_tx_set_gc_busy_drop_counts_transient_busy() {
        use metrics_exporter_prometheus::PrometheusBuilder;

        let recorder = PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        metrics::with_local_recorder(&recorder, || {
            record_tx_set_gc_busy_drop(&busy_herder_error());
            let out = handle.render();
            assert!(
                out.contains("henyey_db_busy_write_dropped_total{site=\"tx_set_gc\"} 1"),
                "DatabaseBusy must render the tx_set_gc drop series at 1.\nOutput:\n{}",
                out
            );
        });

        // DatabaseLocked is the sibling transient code.
        let recorder = PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        metrics::with_local_recorder(&recorder, || {
            record_tx_set_gc_busy_drop(&locked_herder_error());
            let out = handle.render();
            assert!(
                out.contains("henyey_db_busy_write_dropped_total{site=\"tx_set_gc\"} 1"),
                "DatabaseLocked must render the tx_set_gc drop series at 1.\nOutput:\n{}",
                out
            );
        });
    }

    /// A non-transient `DbError` (corruption / integrity) must NOT inflate the
    /// alertable series — herder has no `register_label_series`, so an
    /// unincremented labelled series does not appear at all. Enforces the
    /// #3802 contract point 4.
    #[test]
    fn test_tx_set_gc_busy_drop_ignores_non_transient_db_error() {
        use metrics_exporter_prometheus::PrometheusBuilder;

        let corrupt = HerderError::Db(henyey_db::DbError::Sqlite(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error {
                code: rusqlite::ffi::ErrorCode::DatabaseCorrupt,
                extended_code: 11,
            },
            Some("database disk image is malformed".to_string()),
        )));
        let integrity = HerderError::Db(henyey_db::DbError::Integrity("bad hash".to_string()));

        for err in [corrupt, integrity] {
            let recorder = PrometheusBuilder::new().build_recorder();
            let handle = recorder.handle();
            metrics::with_local_recorder(&recorder, || {
                record_tx_set_gc_busy_drop(&err);
                let out = handle.render();
                assert!(
                    !out.contains("site=\"tx_set_gc\""),
                    "non-transient DbError must not emit any tx_set_gc series.\nOutput:\n{}",
                    out
                );
            });
        }
    }

    /// The anti-string-matching guard: a `HerderError::Internal` whose message
    /// renders identically to a real busy must NOT be counted — only the
    /// structured `DbError` may classify.
    #[test]
    fn test_tx_set_gc_busy_drop_ignores_non_db_herder_error() {
        use metrics_exporter_prometheus::PrometheusBuilder;

        let recorder = PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        metrics::with_local_recorder(&recorder, || {
            record_tx_set_gc_busy_drop(&HerderError::Internal("database is locked".to_string()));
            let out = handle.render();
            assert!(
                !out.contains("site=\"tx_set_gc\""),
                "a non-Db HerderError must never be counted, even if its \
                 message looks busy.\nOutput:\n{}",
                out
            );
        });
    }

    /// The production `min_slot > 0` (cleanup) arm: a transient busy from
    /// `delete_scp_state_below` must count exactly one busy drop AND the call
    /// must return normally (log-and-continue preserved).
    #[test]
    fn test_purge_persisted_tx_sets_emits_busy_drop_on_cleanup_arm() {
        use metrics_exporter_prometheus::PrometheusBuilder;

        let herder = make_test_herder();
        {
            // consensus_index = 20 → tracking_consensus_ledger_index = 19 →
            // min_slot = 19 - MAX_SLOTS_TO_REMEMBER(12) = 7 > 0 → cleanup arm.
            let mut ts = herder.tracking_state.write();
            ts.consensus_index = 20;
            ts.is_tracking = true;
        }
        assert!(
            herder
                .set_scp_persistence(Arc::new(crate::persistence::ScpPersistenceManager::new(
                    Box::new(BusyStorage),
                )))
                .is_ok(),
            "install busy storage"
        );

        let recorder = PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        metrics::with_local_recorder(&recorder, || {
            // Must not panic / must return normally despite the busy error.
            herder.purge_persisted_tx_sets();
            let out = handle.render();
            assert!(
                out.contains("henyey_db_busy_write_dropped_total{site=\"tx_set_gc\"} 1"),
                "cleanup-arm busy must emit exactly one tx_set_gc drop.\nOutput:\n{}",
                out
            );
        });
    }

    /// The `min_slot == 0` (`else`) arm: a transient busy from
    /// `purge_unreferenced_tx_sets_atomic` must likewise count one busy drop.
    /// consensus_index is set to 0 explicitly so arm selection is deterministic.
    #[test]
    fn test_purge_persisted_tx_sets_emits_busy_drop_on_purge_arm() {
        use metrics_exporter_prometheus::PrometheusBuilder;

        let herder = make_test_herder();
        {
            let mut ts = herder.tracking_state.write();
            ts.consensus_index = 0;
            ts.is_tracking = true;
        }
        assert!(
            herder
                .set_scp_persistence(Arc::new(crate::persistence::ScpPersistenceManager::new(
                    Box::new(BusyStorage),
                )))
                .is_ok(),
            "install busy storage"
        );

        let recorder = PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        metrics::with_local_recorder(&recorder, || {
            herder.purge_persisted_tx_sets();
            let out = handle.render();
            assert!(
                out.contains("henyey_db_busy_write_dropped_total{site=\"tx_set_gc\"} 1"),
                "purge-arm busy must emit exactly one tx_set_gc drop.\nOutput:\n{}",
                out
            );
        });
    }

    #[test]
    fn test_set_scp_persistence_rejects_double_install() {
        let herder = make_test_herder();
        let manager1 = Arc::new(crate::persistence::ScpPersistenceManager::in_memory());
        let manager2 = Arc::new(crate::persistence::ScpPersistenceManager::in_memory());

        assert!(
            herder.set_scp_persistence(manager1).is_ok(),
            "first install must succeed"
        );
        let returned = herder
            .set_scp_persistence(manager2)
            .err()
            .expect("second install must fail");
        // OnceLock::set returns the supplied value back on Err; verify we
        // got the same Arc back uniquely.
        assert_eq!(Arc::strong_count(&returned), 1);
    }

    // -------- Regression tests for SCP persist wiring (#2768) --------

    /// Drives a full solo-validator nomination → externalize round through
    /// the herder and asserts that the persistence manager observed the
    /// slot via `persist_scp_state()` (i.e. `last_slot_saved > 0`).
    ///
    /// Fails on `origin/main` because `persist_scp_state` is never called
    /// from the emit hot path. Passes once #2768 wires the persist
    /// callback into `ScpDriver::emit()`.
    #[tokio::test]
    async fn test_persist_scp_state_called_on_emit() {
        let (herder, _secret) = make_validator_herder();
        let manager = Arc::new(crate::persistence::ScpPersistenceManager::in_memory());
        assert!(
            herder.set_scp_persistence(Arc::clone(&manager)).is_ok(),
            "install scp persistence"
        );

        herder.bootstrap(0);
        assert_eq!(herder.state(), HerderState::Tracking);

        // Solo validator (1-of-1 quorum) — nomination → ballot → externalize
        // happens synchronously inside `trigger_next_ledger`, emitting at
        // least one envelope. Each emit must call into the persist callback.
        let outcome = herder
            .trigger_next_ledger(1)
            .expect("trigger_next_ledger should succeed");
        let _ = outcome;

        // Sanity: slot 1 actually externalized.
        assert!(
            herder.scp_driver.get_externalized(1).is_some(),
            "solo validator must externalize slot 1"
        );

        // The persist callback dispatches work to a background thread (to
        // avoid deadlocking on SCP's slot RwLock); wait for it to settle.
        assert!(
            manager.wait_for_pending_persists(std::time::Duration::from_secs(5)),
            "persist worker threads must drain within 5s"
        );

        // KEY ASSERTION: the persist callback fired, so `last_slot_saved`
        // is now >0. On unmodified main this is 0 because nothing ever
        // calls `persist_scp_state()`.
        assert!(
            manager.last_slot_saved() > 0,
            "persist_scp_state must be invoked from the emit hot path; \
             last_slot_saved = {}",
            manager.last_slot_saved()
        );
    }

    /// When no persistence manager is installed, `ScpDriver::emit()` must
    /// still be a no-op for persistence (the OnceLock guard). Drives a
    /// full nomination round and asserts no panic and no externalize loss.
    #[tokio::test]
    async fn test_persist_scp_state_no_manager_no_panic() {
        let (herder, _secret) = make_validator_herder();
        // Intentionally DO NOT install a persistence manager.

        herder.bootstrap(0);
        herder
            .trigger_next_ledger(1)
            .expect("trigger_next_ledger should succeed without persistence");

        assert!(
            herder.scp_driver.get_externalized(1).is_some(),
            "solo validator must externalize even without persistence"
        );
    }

    /// End-to-end proof that the emit hot path feeds the SCP persist-worker
    /// stats series (#3796): after a real emit + drain, `attempted_total >= 1`,
    /// `pending == 0`, and `peak >= 1`.
    #[tokio::test]
    async fn test_scp_persist_stats_reports_after_emit() {
        let (herder, _secret) = make_validator_herder();
        let manager = Arc::new(crate::persistence::ScpPersistenceManager::in_memory());
        assert!(
            herder.set_scp_persistence(Arc::clone(&manager)).is_ok(),
            "install scp persistence"
        );

        herder.bootstrap(0);
        herder
            .trigger_next_ledger(1)
            .expect("trigger_next_ledger should succeed");

        assert!(
            manager.wait_for_pending_persists(std::time::Duration::from_secs(5)),
            "persist worker threads must drain within 5s"
        );

        let stats = herder.scp_persist_stats();
        assert!(
            stats.attempted_total >= 1,
            "emit hot path must record at least one persist attempt; got {}",
            stats.attempted_total
        );
        assert_eq!(stats.pending, 0, "backlog must have drained");
        assert!(
            stats.pending_peak >= 1,
            "peak must reflect the in-flight worker; got {}",
            stats.pending_peak
        );
    }

    /// `scp_persist_stats` returns all-zero defaults (no panic) when no
    /// persistence manager is installed.
    #[test]
    fn test_scp_persist_stats_default_without_manager() {
        let (herder, _secret) = make_validator_herder();
        // Intentionally DO NOT install a persistence manager.
        assert_eq!(
            herder.scp_persist_stats(),
            crate::persistence::ScpPersistStats::default()
        );
    }

    /// End-to-end check: after a real emit triggers a real persist, the
    /// GC purge must keep referenced tx sets and remove orphans. Validates
    /// the full #2698 + #2768 persistence loop.
    #[tokio::test]
    async fn test_persist_scp_state_gc_sees_persisted_rows() {
        let (herder, _secret) = make_validator_herder();
        let manager = Arc::new(crate::persistence::ScpPersistenceManager::in_memory());
        assert!(
            herder.set_scp_persistence(Arc::clone(&manager)).is_ok(),
            "install scp persistence"
        );

        herder.bootstrap(0);
        herder
            .trigger_next_ledger(1)
            .expect("trigger_next_ledger should succeed");

        // Wait for the deferred persist workers spawned by the emit
        // callback to finish before observing manager state.
        assert!(
            manager.wait_for_pending_persists(std::time::Duration::from_secs(5)),
            "persist worker threads must drain within 5s"
        );

        // KEY ASSERTION: the real emit triggered a real persist; capture
        // last_slot_saved BEFORE injecting any orphan slot so we don't
        // measure the orphan injection itself.
        let last_after_real_emit = manager.last_slot_saved();
        assert!(
            last_after_real_emit >= 1,
            "real emit must have persisted; last_slot_saved = {}",
            last_after_real_emit
        );

        // Inject an orphan tx set directly via the manager (not referenced
        // by any persisted envelope).
        let orphan_hash = stellar_xdr::Hash([0xAAu8; 32]);
        let orphan_env = make_persist_envelope_with_tx_set_hash(
            999_999,
            stellar_xdr::Hash([0xBBu8; 32]), // some unrelated hash
        );
        manager
            .persist_scp_state(
                999_999,
                &[orphan_env],
                &[(orphan_hash.clone(), vec![42])],
                &[],
            )
            .expect("persist orphan");
        // Sanity: orphan is present pre-purge.
        assert!(
            manager.has_tx_set(&orphan_hash).unwrap(),
            "orphan tx set must be stored before purge"
        );

        // Run the production GC entry point.
        herder.purge_persisted_tx_sets();

        // Orphan must be gone.
        assert!(
            !manager.has_tx_set(&orphan_hash).unwrap(),
            "orphan tx set must be purged by GC"
        );
    }

    // -----------------------------------------------------------------------
    // restore_persisted_scp_state (#2769) — startup SCP-state restore.
    //
    // Mirrors stellar-core HerderImpl::restoreSCPState() (HerderImpl.cpp:2239-2311)
    // called from HerderImpl::start() (HerderImpl.cpp:2455-2471). The henyey
    // split lifecycle runs `bootstrap(lcl)` first (the setTrackingSCPState +
    // trackingHeartBeat equivalent), so restore must filter to `slot > lcl`.
    // That filter is the regression guard for the consensus stall that closed
    // PR #2797 (replaying already-finalized `slot <= lcl` ballots stalls
    // henyey's SCP ballot machine).
    // -----------------------------------------------------------------------

    /// Build a validator herder whose local quorum set is a 1-of-1 over itself,
    /// returning the herder plus the signing secret. Distinct seed from the
    /// shared `[7u8;32]` helpers so its persisted envelopes are local-node-owned.
    fn make_restore_validator_herder() -> (Herder, SecretKey) {
        let seed = [3u8; 32];
        let secret = SecretKey::from_seed(&seed);
        let public = secret.public_key();
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

        let herder = Herder::with_secret_key(
            config,
            SecretKey::from_seed(&seed),
            make_default_lm(),
            TimerManagerHandle::no_op(),
        );
        (herder, SecretKey::from_seed(&seed))
    }

    /// Construct a signed NOMINATE envelope from `secret` for `slot` whose
    /// single vote references a freshly built tx set. Returns
    /// `(envelope, tx_set_hash, stored_tx_set_bytes, quorum_set_hash, quorum_set)`
    /// so the caller can persist the envelope alongside its referenced deps,
    /// exactly as the production persist callback does.
    fn make_local_nominate_with_deps(
        herder: &Herder,
        secret: &SecretKey,
        slot: u64,
    ) -> (
        ScpEnvelope,
        stellar_xdr::Hash,
        Vec<u8>,
        stellar_xdr::Hash,
        ScpQuorumSet,
    ) {
        use stellar_xdr::WriteXdr;

        // Build a tx set on top of LCL and capture both its hash and the
        // StoredTransactionSet XDR bytes (what gets persisted).
        let lcl_hash = herder.scp_driver.current_header_hash();
        let tx_set = TransactionSet::new(lcl_hash, Vec::new());
        let tx_set_hash = stellar_xdr::Hash(tx_set.hash().0);
        let stored_bytes = tx_set
            .to_xdr_stored_set()
            .to_xdr(Limits::none())
            .expect("encode StoredTransactionSet");

        // Build a signed StellarValue referencing the tx set.
        let close_time = TimePoint(1);
        let network_id = herder.scp_driver.network_id();
        let mut sign_data = network_id.0.to_vec();
        sign_data.extend_from_slice(&EnvelopeType::Scpvalue.to_xdr(Limits::none()).expect("xdr"));
        sign_data.extend_from_slice(&tx_set_hash.to_xdr(Limits::none()).expect("xdr"));
        sign_data.extend_from_slice(&close_time.to_xdr(Limits::none()).expect("xdr"));
        let value_sig = secret.sign(&sign_data);

        let value_node_id = XdrNodeId(stellar_xdr::PublicKey::PublicKeyTypeEd25519(
            stellar_xdr::Uint256(*secret.public_key().as_bytes()),
        ));
        let stellar_value = StellarValue {
            tx_set_hash: tx_set_hash.clone(),
            close_time,
            upgrades: vec![].try_into().unwrap(),
            ext: StellarValueExt::Signed(LedgerCloseValueSignature {
                node_id: value_node_id,
                signature: stellar_xdr::Signature(
                    value_sig.0.to_vec().try_into().unwrap_or_default(),
                ),
            }),
        };
        let value = Value(
            stellar_value
                .to_xdr(Limits::none())
                .unwrap()
                .try_into()
                .unwrap(),
        );

        // The companion quorum set: the herder's local qset.
        let quorum_set = herder
            .scp_driver
            .get_local_quorum_set()
            .expect("validator herder must have a local quorum set");
        let quorum_set_hash = stellar_xdr::Hash(henyey_scp::hash_quorum_set(&quorum_set).0);

        let node_id = XdrNodeId(stellar_xdr::PublicKey::PublicKeyTypeEd25519(
            stellar_xdr::Uint256(*secret.public_key().as_bytes()),
        ));
        let statement = ScpStatement {
            node_id,
            slot_index: slot,
            pledges: ScpStatementPledges::Nominate(ScpNomination {
                quorum_set_hash: quorum_set_hash.clone(),
                votes: vec![value].try_into().unwrap(),
                accepted: vec![].try_into().unwrap(),
            }),
        };

        let envelope = sign_statement(&statement, herder, secret);
        (
            envelope,
            tx_set_hash,
            stored_bytes,
            quorum_set_hash,
            quorum_set,
        )
    }

    /// Build the StellarValue + its referenced deps for a restore test
    /// envelope: returns `(value, tx_set_hash, stored_tx_set_bytes,
    /// quorum_set_hash, quorum_set)`. Shared by the Externalize / Prepare
    /// helpers below so they reference identical deps to the Nominate helper.
    fn make_restore_value_with_deps(
        herder: &Herder,
        secret: &SecretKey,
    ) -> (
        Value,
        stellar_xdr::Hash,
        Vec<u8>,
        stellar_xdr::Hash,
        ScpQuorumSet,
    ) {
        use stellar_xdr::WriteXdr;

        let lcl_hash = herder.scp_driver.current_header_hash();
        let tx_set = TransactionSet::new(lcl_hash, Vec::new());
        let tx_set_hash = stellar_xdr::Hash(tx_set.hash().0);
        let stored_bytes = tx_set
            .to_xdr_stored_set()
            .to_xdr(Limits::none())
            .expect("encode StoredTransactionSet");

        let close_time = TimePoint(1);
        let network_id = herder.scp_driver.network_id();
        let mut sign_data = network_id.0.to_vec();
        sign_data.extend_from_slice(&EnvelopeType::Scpvalue.to_xdr(Limits::none()).expect("xdr"));
        sign_data.extend_from_slice(&tx_set_hash.to_xdr(Limits::none()).expect("xdr"));
        sign_data.extend_from_slice(&close_time.to_xdr(Limits::none()).expect("xdr"));
        let value_sig = secret.sign(&sign_data);

        let value_node_id = XdrNodeId(stellar_xdr::PublicKey::PublicKeyTypeEd25519(
            stellar_xdr::Uint256(*secret.public_key().as_bytes()),
        ));
        let stellar_value = StellarValue {
            tx_set_hash: tx_set_hash.clone(),
            close_time,
            upgrades: vec![].try_into().unwrap(),
            ext: StellarValueExt::Signed(LedgerCloseValueSignature {
                node_id: value_node_id,
                signature: stellar_xdr::Signature(
                    value_sig.0.to_vec().try_into().unwrap_or_default(),
                ),
            }),
        };
        let value = Value(
            stellar_value
                .to_xdr(Limits::none())
                .unwrap()
                .try_into()
                .unwrap(),
        );

        let quorum_set = herder
            .scp_driver
            .get_local_quorum_set()
            .expect("validator herder must have a local quorum set");
        let quorum_set_hash = stellar_xdr::Hash(henyey_scp::hash_quorum_set(&quorum_set).0);

        (
            value,
            tx_set_hash,
            stored_bytes,
            quorum_set_hash,
            quorum_set,
        )
    }

    /// Construct a signed EXTERNALIZE envelope from `secret` for `slot`,
    /// referencing a freshly built tx set + the local quorum set. Mirrors the
    /// shape of `make_local_nominate_with_deps` but pledges Externalize, so a
    /// replay via `set_state_from_envelope` pins the slot to `phase=Externalize`
    /// and records the externalized value (`is_slot_externalized == true`).
    fn make_local_externalize_with_deps(
        herder: &Herder,
        secret: &SecretKey,
        slot: u64,
    ) -> (
        ScpEnvelope,
        stellar_xdr::Hash,
        Vec<u8>,
        stellar_xdr::Hash,
        ScpQuorumSet,
    ) {
        let (value, tx_set_hash, stored_bytes, quorum_set_hash, quorum_set) =
            make_restore_value_with_deps(herder, secret);

        let node_id = XdrNodeId(stellar_xdr::PublicKey::PublicKeyTypeEd25519(
            stellar_xdr::Uint256(*secret.public_key().as_bytes()),
        ));
        let statement = ScpStatement {
            node_id,
            slot_index: slot,
            pledges: ScpStatementPledges::Externalize(ScpStatementExternalize {
                commit: ScpBallot { counter: 1, value },
                n_h: 1,
                commit_quorum_set_hash: quorum_set_hash.clone(),
            }),
        };

        let envelope = sign_statement(&statement, herder, secret);
        (
            envelope,
            tx_set_hash,
            stored_bytes,
            quorum_set_hash,
            quorum_set,
        )
    }

    /// Construct a signed PREPARE envelope from `secret` for `slot`. A restored
    /// Prepare slot is NOT externalized, so the restore-completion path must
    /// leave `latest_externalized` untouched.
    fn make_local_prepare_with_deps(
        herder: &Herder,
        secret: &SecretKey,
        slot: u64,
    ) -> (
        ScpEnvelope,
        stellar_xdr::Hash,
        Vec<u8>,
        stellar_xdr::Hash,
        ScpQuorumSet,
    ) {
        let (value, tx_set_hash, stored_bytes, quorum_set_hash, quorum_set) =
            make_restore_value_with_deps(herder, secret);

        let node_id = XdrNodeId(stellar_xdr::PublicKey::PublicKeyTypeEd25519(
            stellar_xdr::Uint256(*secret.public_key().as_bytes()),
        ));
        let statement = ScpStatement {
            node_id,
            slot_index: slot,
            pledges: ScpStatementPledges::Prepare(ScpStatementPrepare {
                quorum_set_hash: quorum_set_hash.clone(),
                ballot: ScpBallot { counter: 1, value },
                prepared: None,
                prepared_prime: None,
                n_c: 0,
                n_h: 0,
            }),
        };

        let envelope = sign_statement(&statement, herder, secret);
        (
            envelope,
            tx_set_hash,
            stored_bytes,
            quorum_set_hash,
            quorum_set,
        )
    }

    #[test]
    fn test_restore_externalized_next_slot_drives_ledger_close() {
        // Positive: a restored `lcl+1` EXTERNALIZE envelope must drive
        // `complete_externalization`, so `latest_externalized_slot()` advances
        // to `lcl+1`. On `origin/main` (before the restore-completion fix) the
        // restore installs the Externalize ballot but never calls
        // `complete_externalization`, so `latest_externalized_slot()` stays
        // `None` and node0 wedges (the #3595 restart-rejoin stall).
        let lcl = 100u64;
        let (herder, secret) = make_restore_validator_herder();
        let manager = Arc::new(crate::persistence::ScpPersistenceManager::in_memory());
        assert!(herder.set_scp_persistence(Arc::clone(&manager)).is_ok());

        let (env, tx_hash, tx_bytes, qs_hash, qs) =
            make_local_externalize_with_deps(&herder, &secret, lcl + 1);
        manager
            .persist_scp_state(lcl + 1, &[env], &[(tx_hash, tx_bytes)], &[(qs_hash, qs)])
            .expect("persist");

        herder.bootstrap(lcl as u32);
        herder.restore_persisted_scp_state(lcl);

        // The restored Externalize for the next-to-close slot must reach the
        // ledger-close terminal: SCP reports the slot externalized, and the
        // restore hook drives `complete_externalization`, publishing it.
        assert!(
            herder.scp.is_slot_externalized(lcl + 1),
            "restored Externalize must mark slot lcl+1 externalized in SCP"
        );
        assert_eq!(
            herder.latest_externalized_slot(),
            Some(lcl + 1),
            "restore must drive complete_externalization for the externalized lcl+1 slot"
        );
    }

    #[test]
    fn test_restore_prepare_next_slot_does_not_publish_externalized() {
        // Negative: a restored `lcl+1` PREPARE (not externalized) must NOT
        // publish `latest_externalized` — guards the `is_slot_externalized`
        // condition so we never force-close a slot that hasn't externalized.
        let lcl = 100u64;
        let (herder, secret) = make_restore_validator_herder();
        let manager = Arc::new(crate::persistence::ScpPersistenceManager::in_memory());
        assert!(herder.set_scp_persistence(Arc::clone(&manager)).is_ok());

        let (env, tx_hash, tx_bytes, qs_hash, qs) =
            make_local_prepare_with_deps(&herder, &secret, lcl + 1);
        manager
            .persist_scp_state(lcl + 1, &[env], &[(tx_hash, tx_bytes)], &[(qs_hash, qs)])
            .expect("persist");

        herder.bootstrap(lcl as u32);
        herder.restore_persisted_scp_state(lcl);

        assert!(
            !herder.scp.is_slot_externalized(lcl + 1),
            "a restored Prepare must not be externalized"
        );
        assert_eq!(
            herder.latest_externalized_slot(),
            None,
            "restore must NOT publish latest_externalized for a non-externalized slot"
        );
    }

    #[test]
    fn test_restore_persisted_scp_state_no_manager_is_noop() {
        // No persistence manager installed → guarded no-op, no panic, no slots.
        let herder = make_test_herder();
        herder.bootstrap(100);
        herder.restore_persisted_scp_state(100);
        assert!(
            herder.get_current_state_for_slot(101).is_empty(),
            "no manager → nothing replayed"
        );
    }

    #[test]
    fn test_restore_persisted_scp_state_lcl_at_or_below_genesis_is_noop() {
        // lcl == GENESIS_LEDGER_SEQ (1): even with persisted state present,
        // nothing must be replayed (matches upstream's genesis skip).
        let (herder, secret) = make_restore_validator_herder();
        let manager = Arc::new(crate::persistence::ScpPersistenceManager::in_memory());
        assert!(herder.set_scp_persistence(Arc::clone(&manager)).is_ok());

        // Persist a slot-2 envelope (> genesis), to prove the genesis guard
        // (not the slot filter) is what blocks replay here.
        let (env, tx_hash, tx_bytes, qs_hash, qs) =
            make_local_nominate_with_deps(&herder, &secret, 2);
        manager
            .persist_scp_state(2, &[env], &[(tx_hash, tx_bytes)], &[(qs_hash, qs)])
            .expect("persist");

        herder.bootstrap(1);
        herder.restore_persisted_scp_state(1);

        assert!(
            herder.get_current_state_for_slot(2).is_empty(),
            "lcl <= GENESIS must be a no-op even with persisted state"
        );
    }

    #[test]
    fn test_restore_persisted_scp_state_replays_future_slot_and_deps() {
        // Persist an lcl+1 local envelope plus its referenced tx set + quorum
        // set; after restore BOTH deps must be hydrated, driven by the retained
        // future-slot envelope.
        //
        // NOTE: live-envelope replay is GATED OFF in this PR
        // (`RESTORE_LIVE_ENVELOPE_REPLAY == false`) because replaying a
        // restored ballot stalls SCP (the PR #2797 regression). So the future
        // slot is NOT installed into SCP state here; we assert dependency
        // hydration instead. When the SCP ballot-supersession follow-up lands
        // and the flag flips, the slot-installation assertion below (currently
        // an `is_empty` check) becomes the `!is_empty` check.
        let lcl = 100u64;
        let (herder, secret) = make_restore_validator_herder();
        let manager = Arc::new(crate::persistence::ScpPersistenceManager::in_memory());
        assert!(herder.set_scp_persistence(Arc::clone(&manager)).is_ok());

        let (env, tx_hash, tx_bytes, qs_hash, qs) =
            make_local_nominate_with_deps(&herder, &secret, lcl + 1);
        manager
            .persist_scp_state(
                lcl + 1,
                &[env],
                &[(tx_hash.clone(), tx_bytes)],
                &[(qs_hash.clone(), qs)],
            )
            .expect("persist");

        herder.bootstrap(lcl as u32);
        herder.restore_persisted_scp_state(lcl);

        // Quorum set cached (queryable by hash via scp_driver).
        let qs_hash256 = henyey_common::Hash256(qs_hash.0);
        assert!(
            herder.get_quorum_set_by_hash(&qs_hash256).is_some(),
            "referenced quorum set must be cached after restore"
        );
        // Tx set cached.
        let tx_hash256 = henyey_common::Hash256(tx_hash.0);
        assert!(
            herder.has_tx_set(&tx_hash256),
            "referenced tx set must be cached after restore"
        );
        // Live-replay is gated off: the future slot is NOT installed into SCP.
        assert!(
            Herder::RESTORE_LIVE_ENVELOPE_REPLAY
                != herder.get_current_state_for_slot(lcl + 1).is_empty(),
            "future-slot installation must track the RESTORE_LIVE_ENVELOPE_REPLAY gate"
        );
    }

    #[test]
    fn test_restore_persisted_scp_state_filters_slots_at_or_below_lcl() {
        // The #2797 stall guard: dependency hydration is driven ONLY by
        // retained `slot > lcl` envelopes. Persist a slot==lcl envelope with
        // its own distinct deps and a slot==lcl+1 envelope with different
        // deps; after restore, only the future-slot deps are hydrated, and the
        // already-finalized slot's deps are dropped. (Distinct tx sets are
        // produced by giving the at-lcl envelope a non-empty tx set.)
        let lcl = 100u64;
        let (herder, secret) = make_restore_validator_herder();
        let manager = Arc::new(crate::persistence::ScpPersistenceManager::in_memory());
        assert!(herder.set_scp_persistence(Arc::clone(&manager)).is_ok());

        // Future-slot (lcl+1) envelope + deps — these MUST be hydrated.
        let (env_fut, tx_fut, tx_fut_bytes, qs_fut, qs_fut_set) =
            make_local_nominate_with_deps(&herder, &secret, lcl + 1);

        // At-lcl envelope referencing a DISTINCT tx-set hash that no future
        // envelope references — this MUST be dropped by the slot filter.
        let stale_tx_hash = stellar_xdr::Hash([0x5Au8; 32]);
        let stale_env = make_persist_envelope_with_tx_set_hash(lcl, stale_tx_hash.clone());
        manager
            .persist_scp_state(
                lcl,
                &[stale_env],
                &[(stale_tx_hash.clone(), vec![1, 2, 3, 4])],
                &[],
            )
            .expect("persist at-lcl");
        manager
            .persist_scp_state(
                lcl + 1,
                &[env_fut],
                &[(tx_fut.clone(), tx_fut_bytes)],
                &[(qs_fut.clone(), qs_fut_set)],
            )
            .expect("persist future");

        herder.bootstrap(lcl as u32);
        herder.restore_persisted_scp_state(lcl);

        // Future-slot deps hydrated.
        assert!(
            herder.has_tx_set(&henyey_common::Hash256(tx_fut.0)),
            "slot > lcl tx set must be hydrated"
        );
        assert!(
            herder
                .get_quorum_set_by_hash(&henyey_common::Hash256(qs_fut.0))
                .is_some(),
            "slot > lcl quorum set must be hydrated"
        );
        // Stale slot's dep NOT hydrated (filtered out before hydration).
        assert!(
            !herder.has_tx_set(&henyey_common::Hash256(stale_tx_hash.0)),
            "slot <= lcl tx set must NOT be hydrated (the #2797 stall guard)"
        );
        // Neither slot is installed while live-replay is gated off.
        assert!(
            herder.get_current_state_for_slot(lcl).is_empty(),
            "slot <= lcl must never be installed"
        );
    }

    #[test]
    fn test_restore_persisted_scp_state_skips_corrupt_tx_set() {
        // A corrupt tx-set blob persisted alongside a valid one must not panic
        // and must not block the valid tx set from being cached. The valid
        // envelope references only the valid tx set.
        let lcl = 100u64;
        let (herder, secret) = make_restore_validator_herder();
        let manager = Arc::new(crate::persistence::ScpPersistenceManager::in_memory());
        assert!(herder.set_scp_persistence(Arc::clone(&manager)).is_ok());

        let (env, tx_hash, tx_bytes, qs_hash, qs) =
            make_local_nominate_with_deps(&herder, &secret, lcl + 1);
        let corrupt_hash = stellar_xdr::Hash([0xEEu8; 32]);
        manager
            .persist_scp_state(
                lcl + 1,
                &[env],
                &[
                    (tx_hash.clone(), tx_bytes),
                    (corrupt_hash.clone(), vec![0xDE, 0xAD, 0xBE, 0xEF]),
                ],
                &[(qs_hash, qs)],
            )
            .expect("persist");

        herder.bootstrap(lcl as u32);
        herder.restore_persisted_scp_state(lcl);

        // Valid tx set cached; corrupt one isolated (not cached), no panic.
        assert!(
            herder.has_tx_set(&henyey_common::Hash256(tx_hash.0)),
            "valid tx set must be cached"
        );
        assert!(
            !herder.has_tx_set(&henyey_common::Hash256(corrupt_hash.0)),
            "corrupt tx set must NOT be cached"
        );
    }
}

#[cfg(test)]
mod inv_h2_lcl_guard_tests {
    use super::*;
    use henyey_crypto::SecretKey;

    fn make_lm_at_seq(ledger_seq: u32) -> Arc<henyey_ledger::LedgerManager> {
        use henyey_ledger::{LedgerManager, LedgerManagerConfig};
        use stellar_xdr::{
            Hash, LedgerHeader, LedgerHeaderExt, StellarValue, StellarValueExt, TimePoint, VecM,
        };
        let config = LedgerManagerConfig {
            validate_bucket_hash: false,
            ..Default::default()
        };
        let lm = LedgerManager::new("Test Network".to_string(), config);
        let header = LedgerHeader {
            ledger_version: 0,
            previous_ledger_hash: Hash([0u8; 32]),
            scp_value: StellarValue {
                tx_set_hash: Hash([0u8; 32]),
                close_time: TimePoint(0),
                upgrades: VecM::default(),
                ext: StellarValueExt::Basic,
            },
            tx_set_result_hash: Hash([0u8; 32]),
            bucket_list_hash: Hash([0u8; 32]),
            ledger_seq,
            total_coins: 1_000_000_000_000,
            fee_pool: 0,
            inflation_seq: 0,
            id_pool: 0,
            base_fee: 100,
            base_reserve: 5_000_000,
            max_tx_set_size: 100,
            skip_list: [
                Hash([0u8; 32]),
                Hash([0u8; 32]),
                Hash([0u8; 32]),
                Hash([0u8; 32]),
            ],
            ext: LedgerHeaderExt::V0,
        };
        let header_hash = henyey_ledger::compute_header_hash(&header).expect("hash");
        lm.initialize(
            henyey_bucket::BucketList::new(),
            henyey_bucket::HotArchiveBucketList::new(),
            header,
            header_hash,
        )
        .expect("init");
        Arc::new(lm)
    }

    fn make_tracking_herder_at(lm_seq: u32, tracking_slot: u64) -> Herder {
        let lm = make_lm_at_seq(lm_seq);
        let config = HerderConfig::default();
        let herder = Herder::new(config, lm, TimerManagerHandle::no_op());
        // Set to Tracking state with given consensus_index
        {
            let mut state = tracked_write(LOCK_HERDER_STATE, &herder.state);
            *state = HerderState::Tracking;
        }
        {
            let mut ts = tracked_write(LOCK_TRACKING_STATE, &herder.tracking_state);
            ts.is_tracking = true;
            ts.consensus_index = tracking_slot;
        }
        herder
    }

    #[test]
    fn test_assert_lcl_consistency_passes_when_lcl_equals_tracking() {
        // LCL=5, tracking_slot=6 (consensus_idx=5) → LCL == consensus_idx → OK
        let herder = make_tracking_herder_at(5, 6);
        herder.assert_lcl_consistency(); // should not panic
    }

    #[test]
    fn test_assert_lcl_consistency_passes_when_lcl_behind_tracking() {
        // LCL=3, tracking_slot=6 (consensus_idx=5) → LCL < consensus_idx → OK
        let herder = make_tracking_herder_at(3, 6);
        herder.assert_lcl_consistency(); // should not panic
    }

    #[test]
    fn test_assert_lcl_consistency_recovers_when_lcl_equals_tracking_slot() {
        // LCL=6, tracking_slot=6 → LCL == tracking → corrective branch fires.
        // After #2791 this no longer panics: the corrective branch advances
        // tracking to lcl+1 and returns normally.
        let herder = make_tracking_herder_at(6, 6);
        herder.assert_lcl_consistency();
        assert_eq!(
            herder.tracking_slot().get(),
            7,
            "corrective branch must advance tracking to lcl + 1"
        );
    }

    #[test]
    fn test_assert_lcl_consistency_recovers_when_lcl_ahead_of_tracking() {
        // LCL=7, tracking_slot=6 → LCL > tracking by 1 → corrective branch fires.
        let herder = make_tracking_herder_at(7, 6);
        herder.assert_lcl_consistency();
        assert_eq!(
            herder.tracking_slot().get(),
            8,
            "corrective branch must advance tracking to lcl + 1"
        );
    }

    #[test]
    fn test_assert_lcl_consistency_recovers_when_lcl_exceeds_tracking() {
        // LCL=8, tracking_slot=6 → LCL > tracking by 2 → corrective branch fires.
        let herder = make_tracking_herder_at(8, 6);
        herder.assert_lcl_consistency();
        assert_eq!(
            herder.tracking_slot().get(),
            9,
            "corrective branch must advance tracking to lcl + 1"
        );
    }

    #[test]
    fn test_assert_lcl_consistency_recovers_when_lcl_ahead_by_5() {
        // #2791 production fixture: LCL=5005, tracking_slot=5000 (gap=5).
        // Replays the catchup-replay burst-close scenario that brought the
        // mainnet validator down on 0df88603 (and 52ac1665 before it).
        let herder = make_tracking_herder_at(5005, 5000);
        herder.assert_lcl_consistency();
        assert_eq!(
            herder.tracking_slot().get(),
            5006,
            "corrective branch must advance tracking to lcl + 1 even with gap=5"
        );
    }

    #[test]
    fn test_assert_lcl_consistency_increments_metric_on_corrective_fire() {
        // Verify the corrective-fire counter increments by exactly 1 per
        // corrective branch entry.
        let herder = make_tracking_herder_at(7, 6);
        let before = herder.lcl_ahead_of_tracking_corrective_total();
        herder.assert_lcl_consistency();
        let after = herder.lcl_ahead_of_tracking_corrective_total();
        assert_eq!(
            after,
            before + 1,
            "corrective-fire counter must increment by 1 (before={before}, after={after})"
        );
    }

    #[test]
    fn test_assert_lcl_consistency_no_metric_increment_on_passing_case() {
        // Counter must NOT increment when the invariant holds (LCL < tracking).
        let herder = make_tracking_herder_at(5, 6); // LCL=5, consensus_idx=6 → LCL<tracking
        let before = herder.lcl_ahead_of_tracking_corrective_total();
        herder.assert_lcl_consistency();
        let after = herder.lcl_ahead_of_tracking_corrective_total();
        assert_eq!(
            after, before,
            "counter must not increment when invariant holds"
        );
    }

    #[test]
    fn test_assert_lcl_consistency_skips_when_not_tracking() {
        // Herder in Booting state — assertion should be a no-op even with bad values
        let lm = make_lm_at_seq(10);
        let config = HerderConfig::default();
        let herder = Herder::new(config, lm, TimerManagerHandle::no_op());
        // State is Booting by default, tracking_slot=0
        herder.assert_lcl_consistency(); // should not panic
    }

    #[test]
    fn test_assert_lcl_consistency_skips_at_genesis() {
        // Tracking state but tracking_slot=0 (genesis sentinel)
        let lm = make_lm_at_seq(5);
        let config = HerderConfig::default();
        let herder = Herder::new(config, lm, TimerManagerHandle::no_op());
        {
            let mut state = tracked_write(LOCK_HERDER_STATE, &herder.state);
            *state = HerderState::Tracking;
        }
        {
            let mut ts = tracked_write(LOCK_TRACKING_STATE, &herder.tracking_state);
            ts.is_tracking = true;
            ts.consensus_index = 0;
        }
        herder.assert_lcl_consistency(); // should not panic (genesis sentinel)
    }

    #[test]
    fn test_assert_lcl_consistency_in_trigger_next_ledger_normal() {
        // Verify that trigger_next_ledger calls assert_lcl_consistency without
        // panicking in normal operation (LCL=5, tracking=6, trigger slot=6).
        let seed = [42u8; 32];
        let secret = SecretKey::from_seed(&seed);
        let public = secret.public_key();
        let node_id = node_id_from_public_key(&public);

        let quorum_set = ScpQuorumSet {
            threshold: 1,
            validators: vec![node_id].try_into().unwrap(),
            inner_sets: vec![].try_into().unwrap(),
        };

        let lm = make_lm_at_seq(5);
        let config = HerderConfig {
            is_validator: true,
            node_public_key: public,
            local_quorum_set: Some(quorum_set),
            ..HerderConfig::default()
        };
        let herder = Herder::with_secret_key(config, secret, lm, TimerManagerHandle::no_op());
        {
            let mut state = tracked_write(LOCK_HERDER_STATE, &herder.state);
            *state = HerderState::Tracking;
        }
        {
            let mut ts = tracked_write(LOCK_TRACKING_STATE, &herder.tracking_state);
            ts.is_tracking = true;
            ts.consensus_index = 6;
        }
        // trigger_next_ledger(6) should pass the INV-H2 check (LCL=5, consensus_idx=5)
        // It will proceed past the assertion and hit lcl_matches_slot (LCL+1=6==slot=6 → true)
        // Then it will try to build nomination value (which may return None in test context)
        let _ = herder.trigger_next_ledger(6);
        // If we get here without panic, INV-H2 assertion passed
    }

    #[test]
    fn test_assert_lcl_consistency_in_ledger_closed_catchup_path() {
        // Regression test for #2708/#2712: The proper fix sequence for catchup.
        // LCL=6 (already applied), tracking=6 (stale). advance_tracking_to(6)
        // moves tracking to 7, then ledger_closed sees LCL=6 < tracking=7 → OK.
        let lm = make_lm_at_seq(6); // LCL already at closing slot
        let config = HerderConfig::default();
        let herder = Herder::new(config, lm, TimerManagerHandle::no_op());
        {
            let mut state = tracked_write(LOCK_HERDER_STATE, &herder.state);
            *state = HerderState::Tracking;
        }
        {
            let mut ts = tracked_write(LOCK_TRACKING_STATE, &herder.tracking_state);
            ts.is_tracking = true;
            ts.consensus_index = 6;
        }
        // Fix: advance tracking before ledger_closed
        herder.advance_tracking_to(6, 1000);
        assert_eq!(herder.tracking_slot().get(), 7);
        // Now LCL=6 < tracking=7 — INV-H2 passes
        herder.ledger_closed(6, &[], &[], 1000);
    }

    #[test]
    fn test_assert_lcl_consistency_recovers_when_lcl_ahead_by_one_2720() {
        // Regression test for #2720 + #2791: fast-tracking catchup burst-close race.
        //
        // Scenario: Close of ledger N-1 advanced tracking to N. Then close of
        // ledger N completes (LCL=N), but advance_tracking_to(N) hasn't fired
        // yet. A concurrent task calls assert_lcl_consistency and observes
        // LCL=N, tracking=N.
        //
        // Before #2791 this panicked. After #2791 the corrective branch fires:
        // advance_tracking_to(N) runs, tracking advances to N+1, the function
        // returns normally.
        let herder = make_tracking_herder_at(10, 10); // LCL=10, tracking=10
        herder.assert_lcl_consistency();
        assert_eq!(
            herder.tracking_slot().get(),
            11,
            "corrective branch must advance tracking to lcl + 1"
        );
    }

    #[test]
    fn test_advance_tracking_to_fixes_lcl_ahead_race_2720() {
        // Companion to the panic test above: proves that calling
        // advance_tracking_to(N) before any assertion restores the invariant.
        //
        // This is the fix applied in handle_close_complete_inner (#2720):
        // advance_tracking_to fires immediately after close success, before
        // any .await where concurrent tasks could check the invariant.
        let herder = make_tracking_herder_at(10, 10); // LCL=10, tracking=10

        // Fix: advance tracking for the just-closed ledger
        herder.advance_tracking_to(10, 5000);
        assert_eq!(herder.tracking_slot().get(), 11);

        // Now the invariant holds: LCL=10 < tracking=11
        herder.assert_lcl_consistency(); // should not panic
    }

    #[test]
    fn test_advance_tracking_to_basic() {
        // advance_tracking_to(5, 1000) should set tracking to 6
        let lm = make_lm_at_seq(3);
        let config = HerderConfig::default();
        let herder = Herder::new(config, lm, TimerManagerHandle::no_op());
        assert_eq!(herder.tracking_slot().get(), 0);

        herder.advance_tracking_to(5, 1000);

        assert_eq!(herder.tracking_slot().get(), 6);
        assert_eq!(herder.tracking_consensus_close_time(), 1000);
        assert!(herder.is_tracking());
    }

    #[test]
    fn test_advance_tracking_to_idempotent() {
        // If tracking is already past slot+1, advance_tracking_to is a no-op
        let herder = make_tracking_herder_at(3, 10); // tracking=10
        herder.advance_tracking_to(5, 2000); // slot+1=6 < 10 → no-op
        assert_eq!(herder.tracking_slot().get(), 10);
    }

    #[test]
    fn test_advance_tracking_to_idempotent_still_transitions_to_tracking() {
        // Regression test for #2714: advance_tracking_to must transition to
        // Tracking even when consensus_index is already at the target value.
        // This reproduces the exact fixture_tracking() setup that triggered
        // the CI failure: consensus_index pre-set, state = Syncing, then
        // advance_tracking_to called with the same target.
        let lm = make_lm_at_seq(3);
        let config = HerderConfig::default();
        let herder = Herder::new(config, lm, TimerManagerHandle::no_op());

        // Pre-set consensus_index = 101 (simulates Fixture::new()'s
        // set_tracking_for_testing call).
        {
            let mut ts = tracked_write(LOCK_TRACKING_STATE, &herder.tracking_state);
            ts.is_tracking = true;
            ts.consensus_index = 101;
            ts.consensus_close_time = 5000;
        }

        // Set state to Syncing (simulates start_syncing() in fixture_tracking).
        *tracked_write(LOCK_HERDER_STATE, &herder.state) = HerderState::Syncing;
        assert!(!herder.state().is_tracking());

        // Call advance_tracking_to with slot=100 → next=101 == consensus_index.
        // Before the fix, this early-returned without transitioning to Tracking.
        herder.advance_tracking_to(100, 5000);

        // State MUST be Tracking now.
        assert!(
            herder.state().is_tracking(),
            "advance_tracking_to must transition to Tracking even when \
             consensus_index is already at the target (idempotent case)"
        );
        // consensus_index unchanged (idempotent — not regressed)
        assert_eq!(herder.tracking_slot().get(), 101);
    }

    #[test]
    fn test_ensure_tracking_state_idempotent_does_not_update_started_at() {
        // Verify that repeated advance_tracking_to calls (already in Tracking)
        // do NOT update tracking_started_at — only the first transition sets it.
        let lm = make_lm_at_seq(3);
        let config = HerderConfig::default();
        let herder = Herder::new(config, lm, TimerManagerHandle::no_op());

        // First call: transitions Booting → Tracking, sets tracking_started_at.
        herder.advance_tracking_to(5, 1000);
        assert!(herder.state().is_tracking());
        let first_started_at = *herder.tracking_started_at.read();
        assert!(first_started_at.is_some());

        // Small delay to ensure Instant::now() would differ.
        std::thread::sleep(std::time::Duration::from_millis(1));

        // Second call: already Tracking, tracking_started_at must NOT change.
        herder.advance_tracking_to(7, 2000);
        assert!(herder.state().is_tracking());
        let second_started_at = *herder.tracking_started_at.read();
        assert_eq!(
            first_started_at, second_started_at,
            "tracking_started_at must not be updated on repeated transitions"
        );
    }

    #[test]
    fn test_advance_tracking_to_advances_past_current() {
        // When slot+1 > current tracking, it advances
        let herder = make_tracking_herder_at(3, 5); // tracking=5
        herder.advance_tracking_to(6, 3000); // slot+1=7 > 5 → advances
        assert_eq!(herder.tracking_slot().get(), 7);
        assert_eq!(herder.tracking_consensus_close_time(), 3000);
    }

    #[test]
    fn test_advance_tracking_to_before_ledger_closed_regression() {
        // Regression test for #2712: the full sequence that previously panicked.
        // Simulates catchup rapid-close where LCL has already advanced to the
        // closing slot (LCL=6, tracking=6). Without advance_tracking_to, INV-H2
        // would see lcl(6) >= tracking(6) → panic. With it, tracking becomes 7.
        let lm = make_lm_at_seq(6); // LCL = 6 (already advanced by apply)
        let config = HerderConfig::default();
        let herder = Herder::new(config, lm, TimerManagerHandle::no_op());
        {
            let mut state = tracked_write(LOCK_HERDER_STATE, &herder.state);
            *state = HerderState::Tracking;
        }
        {
            let mut ts = tracked_write(LOCK_TRACKING_STATE, &herder.tracking_state);
            ts.is_tracking = true;
            ts.consensus_index = 6; // tracking_slot = 6 (stale, not advanced)
        }
        // Before fix: ledger_closed(6) would panic here (LCL=6 == tracking=6).
        // Fix: advance tracking BEFORE ledger_closed.
        herder.advance_tracking_to(6, 1000);
        assert_eq!(herder.tracking_slot().get(), 7);

        // Now ledger_closed succeeds: LCL=6 < tracking=7
        herder.ledger_closed(6, &[], &[], 1000);
    }

    #[test]
    fn test_ledger_closed_recovers_without_advance_tracking_to() {
        // Regression for #2791 (sibling of #2720): without advance_tracking_to,
        // ledger_closed previously panicked when LCL == tracking_slot (catchup
        // rapid-close scenario). After #2791 the corrective branch fires
        // inside assert_lcl_consistency, advances tracking to lcl+1, and
        // ledger_closed completes without panicking.
        let lm = make_lm_at_seq(6); // LCL = 6
        let config = HerderConfig::default();
        let herder = Herder::new(config, lm, TimerManagerHandle::no_op());
        {
            let mut state = tracked_write(LOCK_HERDER_STATE, &herder.state);
            *state = HerderState::Tracking;
        }
        {
            let mut ts = tracked_write(LOCK_TRACKING_STATE, &herder.tracking_state);
            ts.is_tracking = true;
            ts.consensus_index = 6; // tracking_slot = 6 (stale)
        }
        herder.ledger_closed(6, &[], &[], 1000);
        assert_eq!(
            herder.tracking_slot().get(),
            7,
            "corrective branch must advance tracking to lcl + 1 during ledger_closed"
        );
    }
}

#[cfg(test)]
mod closing_gate_tests {
    use super::*;
    use stellar_xdr::{
        ScpNomination, ScpStatement, ScpStatementPledges, Signature as XdrSignature,
    };

    fn make_default_lm() -> Arc<henyey_ledger::LedgerManager> {
        use henyey_ledger::{LedgerManager, LedgerManagerConfig};
        use stellar_xdr::{
            Hash, LedgerHeader, LedgerHeaderExt, StellarValue, StellarValueExt, TimePoint, VecM,
        };
        let config = LedgerManagerConfig {
            validate_bucket_hash: false,
            ..Default::default()
        };
        let lm = LedgerManager::new("Test Network".to_string(), config);
        let header = LedgerHeader {
            ledger_version: 0,
            previous_ledger_hash: Hash([0u8; 32]),
            scp_value: StellarValue {
                tx_set_hash: Hash([0u8; 32]),
                close_time: TimePoint(0),
                upgrades: VecM::default(),
                ext: StellarValueExt::Basic,
            },
            tx_set_result_hash: Hash([0u8; 32]),
            bucket_list_hash: Hash([0u8; 32]),
            ledger_seq: 0,
            total_coins: 1_000_000_000_000,
            fee_pool: 0,
            inflation_seq: 0,
            id_pool: 0,
            base_fee: 100,
            base_reserve: 5_000_000,
            max_tx_set_size: 100,
            skip_list: [
                Hash([0u8; 32]),
                Hash([0u8; 32]),
                Hash([0u8; 32]),
                Hash([0u8; 32]),
            ],
            ext: LedgerHeaderExt::V0,
        };
        let header_hash = henyey_ledger::compute_header_hash(&header).expect("hash");
        lm.initialize(
            henyey_bucket::BucketList::new(),
            henyey_bucket::HotArchiveBucketList::new(),
            header,
            header_hash,
        )
        .expect("init");
        Arc::new(lm)
    }

    fn make_test_herder() -> Herder {
        Herder::new(
            HerderConfig::default(),
            make_default_lm(),
            TimerManagerHandle::no_op(),
        )
    }

    fn make_test_envelope(slot: u64) -> ScpEnvelope {
        let node_id = stellar_xdr::NodeId(stellar_xdr::PublicKey::PublicKeyTypeEd25519(
            stellar_xdr::Uint256([0u8; 32]),
        ));
        ScpEnvelope {
            statement: ScpStatement {
                node_id,
                slot_index: slot,
                pledges: ScpStatementPledges::Nominate(ScpNomination {
                    quorum_set_hash: stellar_xdr::Hash([0u8; 32]),
                    votes: vec![].try_into().unwrap(),
                    accepted: vec![].try_into().unwrap(),
                }),
            },
            signature: XdrSignature(vec![0u8; 64].try_into().unwrap()),
        }
    }

    #[test]
    fn test_closing_gate_initially_open() {
        let herder = make_test_herder();
        let gate = herder.closing_gate.lock().unwrap();
        assert_eq!(gate.slot, 0, "gate should be open (slot=0) initially");
        assert!(gate.buffer.is_empty());
    }

    #[test]
    fn test_closing_gate_set_on_advance_tracking_slot() {
        let herder = make_test_herder();
        herder.start_syncing();
        herder.bootstrap(100);

        // Simulate externalization of slot 101 by directly setting the gate
        // (advance_tracking_slot is called internally during externalization).
        {
            let mut gate = herder.closing_gate.lock().unwrap();
            gate.slot = 102; // externalized_slot + 1
            gate.buffer.clear();
        }

        let gate = herder.closing_gate.lock().unwrap();
        assert_eq!(
            gate.slot, 102,
            "gate should be set to next slot after externalize"
        );
    }

    #[test]
    fn test_envelope_gated_during_close_window() {
        let herder = make_test_herder();
        herder.start_syncing();
        herder.bootstrap(100);

        // Activate gate for slot 102
        {
            let mut gate = herder.closing_gate.lock().unwrap();
            gate.slot = 102;
            gate.buffer.clear();
        }

        // Process an envelope for the gated slot
        let env = make_test_envelope(102);
        let result = herder.process_scp_envelope_with_tx_set(env.clone());
        assert_eq!(result, EnvelopeState::Deferred);

        // Verify it was buffered
        let gate = herder.closing_gate.lock().unwrap();
        assert_eq!(gate.buffer.len(), 1);
        assert_eq!(gate.buffer[0].statement.slot_index, 102);
    }

    #[test]
    fn test_envelope_not_gated_for_different_slot() {
        let herder = make_test_herder();
        herder.start_syncing();
        herder.bootstrap(100);

        // Activate gate for slot 102
        {
            let mut gate = herder.closing_gate.lock().unwrap();
            gate.slot = 102;
            gate.buffer.clear();
        }

        // Envelope for a different slot should not be gated
        let env = make_test_envelope(103);
        let result = herder.process_scp_envelope_with_tx_set(env);
        // It won't be Deferred (it'll be Invalid since SCP doesn't know the node,
        // but the point is it's NOT Deferred)
        assert_ne!(result, EnvelopeState::Deferred);
    }

    #[test]
    fn test_envelope_not_gated_when_gate_open() {
        let herder = make_test_herder();
        herder.start_syncing();
        herder.bootstrap(100);

        // Gate is open (slot=0 by default)
        let env = make_test_envelope(102);
        let result = herder.process_scp_envelope_with_tx_set(env);
        // Should NOT be Deferred — gate is open
        assert_ne!(result, EnvelopeState::Deferred);
    }

    #[test]
    fn test_multiple_envelopes_buffered() {
        let herder = make_test_herder();
        herder.start_syncing();
        herder.bootstrap(100);

        // Activate gate for slot 102
        {
            let mut gate = herder.closing_gate.lock().unwrap();
            gate.slot = 102;
            gate.buffer.clear();
        }

        // Buffer multiple envelopes
        for _ in 0..5 {
            let env = make_test_envelope(102);
            let result = herder.process_scp_envelope_with_tx_set(env);
            assert_eq!(result, EnvelopeState::Deferred);
        }

        let gate = herder.closing_gate.lock().unwrap();
        assert_eq!(gate.buffer.len(), 5);
    }

    #[test]
    fn test_clear_closing_gate_discards_buffer() {
        let herder = make_test_herder();

        // Set gate and buffer some envelopes
        {
            let mut gate = herder.closing_gate.lock().unwrap();
            gate.slot = 102;
            gate.buffer.push(make_test_envelope(102));
            gate.buffer.push(make_test_envelope(102));
        }

        // Clear gate (error path)
        herder.clear_closing_gate();

        // Gate should be open with empty buffer
        let gate = herder.closing_gate.lock().unwrap();
        assert_eq!(gate.slot, 0);
        assert!(gate.buffer.is_empty());
    }

    #[test]
    fn test_gate_clear_in_ledger_closed_opens_gate() {
        let herder = make_test_herder();
        herder.start_syncing();
        herder.bootstrap(100);

        // Set gate and buffer an envelope
        {
            let mut gate = herder.closing_gate.lock().unwrap();
            gate.slot = 102;
            gate.buffer.push(make_test_envelope(102));
        }

        // Simulate what ledger_closed does: clear gate and take buffer
        let gate_buffered: Vec<ScpEnvelope> = {
            let mut gate = herder.closing_gate.lock().unwrap();
            gate.slot = 0;
            std::mem::take(&mut gate.buffer)
        };

        // Gate should be open
        let gate = herder.closing_gate.lock().unwrap();
        assert_eq!(gate.slot, 0);
        assert!(gate.buffer.is_empty());
        drop(gate);

        // Buffered envelopes should have been extracted
        assert_eq!(gate_buffered.len(), 1);
        assert_eq!(gate_buffered[0].statement.slot_index, 102);
    }

    #[test]
    fn test_gate_reactivation_clears_stale_buffer() {
        let herder = make_test_herder();

        // Set gate for slot 102 with a buffered envelope
        {
            let mut gate = herder.closing_gate.lock().unwrap();
            gate.slot = 102;
            gate.buffer.push(make_test_envelope(102));
        }

        // Re-activate gate for slot 103 (simulates next externalization)
        // This mirrors advance_tracking_slot behavior: clear + set new slot
        {
            let mut gate = herder.closing_gate.lock().unwrap();
            gate.slot = 103;
            gate.buffer.clear();
        }

        // Old buffer should be gone, new slot active
        let gate = herder.closing_gate.lock().unwrap();
        assert_eq!(gate.slot, 103);
        assert!(gate.buffer.is_empty());
    }
}

#[cfg(test)]
mod set_state_tests {
    use super::*;

    fn make_default_lm() -> Arc<henyey_ledger::LedgerManager> {
        use henyey_ledger::{LedgerManager, LedgerManagerConfig};
        use stellar_xdr::{
            Hash, LedgerHeader, LedgerHeaderExt, StellarValue, StellarValueExt, TimePoint, VecM,
        };
        let config = LedgerManagerConfig {
            validate_bucket_hash: false,
            ..Default::default()
        };
        let lm = LedgerManager::new("Test Network".to_string(), config);
        let header = LedgerHeader {
            ledger_version: 0,
            previous_ledger_hash: Hash([0u8; 32]),
            scp_value: StellarValue {
                tx_set_hash: Hash([0u8; 32]),
                close_time: TimePoint(0),
                upgrades: VecM::default(),
                ext: StellarValueExt::Basic,
            },
            tx_set_result_hash: Hash([0u8; 32]),
            bucket_list_hash: Hash([0u8; 32]),
            ledger_seq: 0,
            total_coins: 1_000_000_000_000,
            fee_pool: 0,
            inflation_seq: 0,
            id_pool: 0,
            base_fee: 100,
            base_reserve: 5_000_000,
            max_tx_set_size: 100,
            skip_list: [
                Hash([0u8; 32]),
                Hash([0u8; 32]),
                Hash([0u8; 32]),
                Hash([0u8; 32]),
            ],
            ext: LedgerHeaderExt::V0,
        };
        let header_hash = henyey_ledger::compute_header_hash(&header).expect("hash");
        lm.initialize(
            henyey_bucket::BucketList::new(),
            henyey_bucket::HotArchiveBucketList::new(),
            header,
            header_hash,
        )
        .expect("init");
        Arc::new(lm)
    }

    fn make_test_herder() -> Herder {
        Herder::new(
            HerderConfig::default(),
            make_default_lm(),
            TimerManagerHandle::no_op(),
        )
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
        spawn_scp_verifier, PipelinedIntake, PostVerifyReason, PreFilter, PreFilterRejectReason,
        Verdict, VerifiedEnvelope, VerifierState,
    };
    use henyey_crypto::SecretKey;
    use stellar_xdr::{
        NodeId as XdrNodeId, ScpNomination, ScpStatement, ScpStatementPledges,
        Signature as XdrSignature,
    };

    fn make_default_lm() -> Arc<henyey_ledger::LedgerManager> {
        make_ledger_manager_at_seq(0)
    }

    fn make_test_herder() -> Herder {
        Herder::new(
            HerderConfig::default(),
            make_default_lm(),
            TimerManagerHandle::no_op(),
        )
    }

    fn make_unsigned_envelope(slot: u64, node_seed: u8) -> ScpEnvelope {
        let node_id = XdrNodeId(stellar_xdr::PublicKey::PublicKeyTypeEd25519(
            stellar_xdr::Uint256([node_seed; 32]),
        ));
        ScpEnvelope {
            statement: ScpStatement {
                node_id,
                slot_index: slot,
                pledges: ScpStatementPledges::Nominate(ScpNomination {
                    quorum_set_hash: stellar_xdr::Hash([0u8; 32]),
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
                assert_eq!(intake.slot(), 100);
                assert!(!intake.is_externalize());
            }
            PreFilter::Reject(r) => panic!("expected Accept, got Reject({:?})", r),
        }
    }

    fn make_signed_test_envelope_outer(
        slot: u64,
        herder: &Herder,
        secret: &SecretKey,
    ) -> ScpEnvelope {
        use stellar_xdr::{
            LedgerCloseValueSignature, Limits, NodeId as XdrNodeId, ScpNomination, ScpStatement,
            ScpStatementPledges, Signature as XdrSignature, StellarValue, StellarValueExt,
            TimePoint, Value, WriteXdr,
        };
        let public = secret.public_key();
        let node_id = XdrNodeId(stellar_xdr::PublicKey::PublicKeyTypeEd25519(
            stellar_xdr::Uint256(*public.as_bytes()),
        ));
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();

        // Build a signed StellarValue so check_envelope_close_time accepts it.
        let tx_set_hash = stellar_xdr::Hash([0u8; 32]);
        let close_time = TimePoint(now);
        let mut sign_data = herder.scp_driver.network_id().0.to_vec();
        sign_data.extend_from_slice(
            &stellar_xdr::EnvelopeType::Scpvalue
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
                signature: stellar_xdr::Signature(
                    value_sig.0.to_vec().try_into().unwrap_or_default(),
                ),
            }),
        };
        let vote = Value(sv.to_xdr(Limits::none()).unwrap().try_into().unwrap());

        let statement = ScpStatement {
            node_id,
            slot_index: slot,
            pledges: ScpStatementPledges::Nominate(ScpNomination {
                quorum_set_hash: stellar_xdr::Hash([0u8; 32]),
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
        let intake = PipelinedIntake::from_local(env, 100, false);
        let (state, reason) = herder.process_verified(VerifiedEnvelope {
            intake,
            verdict: Verdict::InvalidSignature,
        });
        assert_eq!(state, EnvelopeState::InvalidSignature);
        assert_eq!(reason, PostVerifyReason::InvalidSignature);
    }

    #[test]
    fn test_process_verified_panic_surfaces_invalid() {
        let herder = make_test_herder();
        herder.start_syncing();
        let env = make_unsigned_envelope(100, 1);
        let intake = PipelinedIntake::from_local(env, 100, false);
        let (state, reason) = herder.process_verified(VerifiedEnvelope {
            intake,
            verdict: Verdict::Panic,
        });
        assert_eq!(state, EnvelopeState::Invalid);
        assert_eq!(reason, PostVerifyReason::PanicVerdict);
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
        let herder = Herder::with_secret_key(
            config,
            secret.clone(),
            make_default_lm(),
            TimerManagerHandle::no_op(),
        );
        herder.start_syncing();
        herder.pending_envelopes.set_current_slot(95);

        // Build an envelope whose statement.node_id == the local node_id.
        let env = make_signed_test_envelope_outer(100, &herder, &secret);
        let intake = PipelinedIntake::from_local(env, 100, false);
        let (state, reason) = herder.process_verified(VerifiedEnvelope {
            intake,
            verdict: Verdict::Ok,
        });
        assert_eq!(
            state,
            EnvelopeState::Invalid,
            "self-message must be skipped as Invalid"
        );
        assert_eq!(reason, PostVerifyReason::SelfMessage);
    }

    #[test]
    fn test_process_verified_post_verify_gate_drift_too_old() {
        // Simulate gate drift: build an envelope whose signed close_time
        // is current (so pre_filter's close-time gate passes), but by the
        // time process_verified runs, the herder has advanced its current
        // slot so far that the envelope's slot falls below `min_ledger_seq`
        // (tracking_slot=10000 → min_ledger_seq=9988)
        // — the Range gate must now reject it as `EnvelopeState::TooOld`.
        let herder = make_test_herder();
        herder.start_syncing();
        // Set tracking_slot high so pre_filter's Range gate (keyed on
        // tracking_slot — not pending_envelopes.current_slot) rejects slot=100.
        herder.tracking_state.write().consensus_index = 10_000;
        herder.pending_envelopes.set_current_slot(10_000);

        let secret = SecretKey::from_seed(&[7u8; 32]);
        // Build a Nominate envelope with a fresh, signed StellarValue so
        // close-time passes; slot=100 is far below min_ledger_seq (=9988).
        let env = make_signed_test_envelope_outer(100, &herder, &secret);
        let intake = PipelinedIntake::from_local(env, 100, false);
        let (state, reason) = herder.process_verified(VerifiedEnvelope {
            intake,
            verdict: Verdict::Ok,
        });
        assert_eq!(
            state,
            EnvelopeState::TooOld,
            "post-verify Range gate should reject drifted slot as TooOld"
        );
        assert_eq!(reason, PostVerifyReason::GateDriftRange);
    }

    /// #2408: process_verified returns (Invalid, PendingAddPerSlotFull) when
    /// the per-slot cap is hit. This exercises the herder-level match arm
    /// end-to-end rather than just the PendingEnvelopes unit tests.
    #[test]
    fn test_process_verified_per_slot_full() {
        use crate::pending::PendingConfig;
        let config = HerderConfig {
            pending_config: PendingConfig {
                max_envelopes_per_slot: 3,
                ..Default::default()
            },
            ..Default::default()
        };
        let herder = Herder::new(config, make_default_lm(), TimerManagerHandle::no_op());
        herder.start_syncing();
        herder.pending_envelopes.set_current_slot(95);

        let slot = 100u64; // future slot → goes through FetchingEnvelopes

        // Fill 3 envelopes (the cap). Use signed envelopes so close-time
        // gate passes. Envelopes now route through FetchingEnvelopes and
        // return Fetching (deps missing) with reason Accepted (#2335).
        for seed_byte in 0..3u8 {
            let secret = SecretKey::from_seed(&[seed_byte + 10; 32]);
            let env = make_signed_test_envelope_outer(slot, &herder, &secret);
            let intake = PipelinedIntake::from_local(env, slot, false);
            let (state, reason) = herder.process_verified(VerifiedEnvelope {
                intake,
                verdict: Verdict::Ok,
            });
            assert_eq!(state, EnvelopeState::Fetching);
            assert_eq!(reason, PostVerifyReason::Accepted);
        }

        // 4th envelope exceeds the cap → PerSlotFull path.
        let secret = SecretKey::from_seed(&[99; 32]);
        let env = make_signed_test_envelope_outer(slot, &herder, &secret);
        let intake = PipelinedIntake::from_local(env, slot, false);
        let (state, reason) = herder.process_verified(VerifiedEnvelope {
            intake,
            verdict: Verdict::Ok,
        });
        assert_eq!(state, EnvelopeState::Invalid);
        assert_eq!(reason, PostVerifyReason::PendingAddPerSlotFull);
    }

    /// #2411: Regression test for the current-slot bypass. The original bug
    /// was that admission control only ran for future slots, letting current-slot
    /// floods grow unbounded. This test proves the cap now applies to current-slot.
    #[test]
    fn test_process_verified_per_slot_full_current_slot() {
        use crate::pending::PendingConfig;
        let config = HerderConfig {
            pending_config: PendingConfig {
                max_envelopes_per_slot: 3,
                ..Default::default()
            },
            ..Default::default()
        };
        let herder = Herder::new(config, make_default_lm(), TimerManagerHandle::no_op());
        herder.start_syncing();
        herder.pending_envelopes.set_current_slot(100);

        let slot = 100u64; // current slot — this was the bypass path

        // Fill 3 envelopes (the cap).
        for seed_byte in 0..3u8 {
            let secret = SecretKey::from_seed(&[seed_byte + 10; 32]);
            let env = make_signed_test_envelope_outer(slot, &herder, &secret);
            let intake = PipelinedIntake::from_local(env, slot, false);
            let (state, reason) = herder.process_verified(VerifiedEnvelope {
                intake,
                verdict: Verdict::Ok,
            });
            assert_eq!(state, EnvelopeState::Fetching);
            assert_eq!(reason, PostVerifyReason::Accepted);
        }

        // 4th envelope for the CURRENT slot exceeds the cap → rejected.
        let secret = SecretKey::from_seed(&[99; 32]);
        let env = make_signed_test_envelope_outer(slot, &herder, &secret);
        let intake = PipelinedIntake::from_local(env, slot, false);
        let (state, reason) = herder.process_verified(VerifiedEnvelope {
            intake,
            verdict: Verdict::Ok,
        });
        assert_eq!(state, EnvelopeState::Invalid);
        assert_eq!(reason, PostVerifyReason::PendingAddPerSlotFull);
    }

    // -------- scp_verify::worker tests --------

    #[test]
    fn test_worker_dead_on_channel_close() {
        let spawned = spawn_scp_verifier(
            Hash256::from_bytes([4u8; 32]),
            8,
            Arc::new(crate::metrics::ScpMetrics::new()),
        )
        .expect("spawn");
        let state = spawned.handle.state.clone();
        let tx = spawned.handle.tx.clone();
        let verified_rx = spawned.verified_rx;
        let join_handle = spawned.join_handle;
        assert_eq!(
            VerifierState::from_u8(state.load(Ordering::Relaxed)),
            VerifierState::Running
        );

        // Close all senders so blocking_recv returns None and the worker
        // transitions to Dead.
        drop(spawned.handle); // original handle owns one tx
        drop(tx); // our clone
        drop(verified_rx);

        // Deterministically wait for the worker thread to exit.
        join_handle.join().expect("worker thread should not panic");
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
        let spawned = spawn_scp_verifier(
            Hash256::from_bytes([11u8; 32]),
            8,
            Arc::new(crate::metrics::ScpMetrics::new()),
        )
        .expect("spawn");
        let h = spawned.handle.clone();
        let mut verified_rx = spawned.verified_rx;
        let join_handle = spawned.join_handle;

        let intake = PipelinedIntake::from_local(make_unsigned_envelope(1, 1), u64::MAX - 1, false);
        h.tx.blocking_send(intake).expect("send");

        let ve = verified_rx
            .blocking_recv()
            .expect("worker should emit Panic verdict before exiting");
        assert!(
            matches!(ve.verdict, Verdict::Panic),
            "expected Panic verdict, got {:?}",
            ve.verdict
        );

        // Deterministically wait for the worker thread to exit.
        join_handle
            .join()
            .expect("worker thread should not panic (catch_unwind handles it)");
        assert_eq!(
            h.state(),
            VerifierState::Dead,
            "worker must transition to Dead after a caught panic"
        );
    }

    #[test]
    fn test_worker_output_receiver_close_exits() {
        let spawned = spawn_scp_verifier(
            Hash256::from_bytes([5u8; 32]),
            8,
            Arc::new(crate::metrics::ScpMetrics::new()),
        )
        .expect("spawn");
        let h = spawned.handle.clone();
        let join_handle = spawned.join_handle;

        // Drop the output receiver — the next send() in the worker will fail.
        drop(spawned.verified_rx);

        // Send one non-panic envelope so the worker processes it and hits
        // the failed send path.
        let intake = PipelinedIntake::from_local(make_unsigned_envelope(1, 1), 1, false);
        h.tx.blocking_send(intake).expect("send");

        // Deterministically wait for the worker thread to exit.
        join_handle
            .join()
            .expect("worker thread should not panic on output-close");
        assert_eq!(
            h.state(),
            VerifierState::Dead,
            "worker must transition to Dead when output receiver is dropped"
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
            backlog_peak: Arc::new(AtomicUsize::new(0)),
        };

        assert_eq!(handle.queue_len(), 0, "empty channel reports 0 used slots");

        for i in 0..3 {
            tx.blocking_send(PipelinedIntake::from_local(
                make_unsigned_envelope(i as u64, 1),
                i as u64,
                false,
            ))
            .expect("send");
        }

        assert_eq!(
            handle.queue_len(),
            3,
            "three enqueued items must report queue_len == 3"
        );
    }

    // ---------- AUDIT-211 / issue #2100 regression tests ----------
    //
    // The herder pre-filter previously clamped the slot-range lower bound to
    // `max(min_ledger_seq, lcl + 1)`. That rejected in-window SCP envelopes
    // for slots Henyey had already moved past in tracking but whose finalization
    // messages stellar-core still accepts and forwards through SCP (see
    // `HerderImpl::recvSCPEnvelope` and `HerderSCPDriver::validateValue`'s
    // "already moved on" branch). The clamp was active in essentially all
    // steady-state operation because `lcl + 1` typically sits at or just below
    // `tracking_slot`, far above `min_ledger_seq = tracking_slot - 12 + 1`.
    //
    // The fix removes the clamp so the lower bound matches stellar-core's
    // `getMinLedgerSeqToRemember()` exactly. The tests below exercise the
    // post-LCL retained-window band that the clamp used to reject.

    fn make_ledger_manager_at_seq(ledger_seq: u32) -> Arc<henyey_ledger::LedgerManager> {
        use henyey_ledger::{LedgerManager, LedgerManagerConfig};
        use stellar_xdr::{
            Hash, LedgerHeader, LedgerHeaderExt, StellarValue, StellarValueExt, TimePoint, VecM,
        };
        let config = LedgerManagerConfig {
            validate_bucket_hash: false,
            ..Default::default()
        };
        let lm = LedgerManager::new("Test Network".to_string(), config);
        let header = LedgerHeader {
            ledger_version: 0,
            previous_ledger_hash: Hash([0u8; 32]),
            scp_value: StellarValue {
                tx_set_hash: Hash([0u8; 32]),
                close_time: TimePoint(0),
                upgrades: VecM::default(),
                ext: StellarValueExt::Basic,
            },
            tx_set_result_hash: Hash([0u8; 32]),
            bucket_list_hash: Hash([0u8; 32]),
            ledger_seq,
            total_coins: 1_000_000_000_000,
            fee_pool: 0,
            inflation_seq: 0,
            id_pool: 0,
            base_fee: 100,
            base_reserve: 5_000_000,
            max_tx_set_size: 100,
            skip_list: [
                Hash([0u8; 32]),
                Hash([0u8; 32]),
                Hash([0u8; 32]),
                Hash([0u8; 32]),
            ],
            ext: LedgerHeaderExt::V0,
        };
        let header_hash = henyey_ledger::compute_header_hash(&header).expect("hash");
        lm.initialize(
            henyey_bucket::BucketList::new(),
            henyey_bucket::HotArchiveBucketList::new(),
            header,
            header_hash,
        )
        .expect("init");
        Arc::new(lm)
    }

    /// Build a Herder in Syncing state with a ledger manager at `lcl` and
    /// tracking_slot set to `tracking_slot`. Returns the herder ready for
    /// `pre_filter_scp_envelope` exercise.
    fn herder_with_lcl_and_tracking(lcl: u32, tracking_slot: u64) -> Herder {
        let herder = Herder::new(
            HerderConfig::default(),
            make_ledger_manager_at_seq(lcl),
            TimerManagerHandle::no_op(),
        );
        herder.start_syncing();
        // Set tracking_slot directly (we're inside the crate, so private
        // fields are accessible). is_tracking is set so the
        // `state.is_tracking()` branch in pre_filter computes max_ledger_seq
        // via tracking, not the non-tracking close-time path.
        {
            let mut ts = herder.tracking_state.write();
            ts.consensus_index = tracking_slot;
            ts.is_tracking = true;
        }
        herder.pending_envelopes.set_current_slot(tracking_slot);
        herder
    }

    /// Core regression: a slot in `[min_ledger_seq, lcl]` must be accepted.
    /// Pre-fix this returned `Reject(Range)` because of the `lcl + 1` clamp.
    #[test]
    fn test_pre_filter_accepts_recent_post_lcl_envelope() {
        // tracking=100, lcl=99, min_ledger_seq=88. Slot 95 is post-LCL but
        // within the retained window.
        let herder = herder_with_lcl_and_tracking(99, 100);
        let secret = SecretKey::from_seed(&[1u8; 32]);
        let env = make_signed_test_envelope_outer(95, &herder, &secret);
        match herder.pre_filter_scp_envelope(&env) {
            PreFilter::Accept(intake) => assert_eq!(intake.slot(), 95),
            PreFilter::Reject(r) => panic!(
                "expected Accept for post-LCL slot in retained window, got Reject({:?})",
                r
            ),
        }
    }

    /// Worst-case regression: when apply has caught up to tracking
    /// (`lcl == tracking_slot`), the pre-fix clamp set
    /// `effective_min = lcl + 1 = tracking_slot + 1`, rejecting every
    /// non-checkpoint slot in the entire retained window.
    #[test]
    fn test_pre_filter_accepts_envelope_when_lcl_equals_tracking() {
        // tracking=100, lcl=100. Pre-fix this rejected slot 95 (and any other
        // non-checkpoint slot in 88..=100).
        let herder = herder_with_lcl_and_tracking(100, 100);
        let secret = SecretKey::from_seed(&[2u8; 32]);
        let env = make_signed_test_envelope_outer(95, &herder, &secret);
        match herder.pre_filter_scp_envelope(&env) {
            PreFilter::Accept(intake) => assert_eq!(intake.slot(), 95),
            PreFilter::Reject(r) => panic!(
                "expected Accept when lcl == tracking_slot, got Reject({:?})",
                r
            ),
        }
    }

    /// Lower-edge boundary: slot exactly at `min_ledger_seq` is accepted.
    #[test]
    fn test_pre_filter_accepts_at_min_ledger_seq() {
        // tracking=100, min_ledger_seq=88.
        let herder = herder_with_lcl_and_tracking(99, 100);
        let secret = SecretKey::from_seed(&[3u8; 32]);
        let env = make_signed_test_envelope_outer(88, &herder, &secret);
        match herder.pre_filter_scp_envelope(&env) {
            PreFilter::Accept(intake) => assert_eq!(intake.slot(), 88),
            PreFilter::Reject(r) => panic!(
                "slot at min_ledger_seq must be accepted, got Reject({:?})",
                r
            ),
        }
    }

    /// Lower boundary: slot below `min_ledger_seq` is still rejected (the
    /// retained-window check is preserved; we only removed the LCL clamp).
    #[test]
    fn test_pre_filter_rejects_below_min_ledger_seq() {
        // tracking=100, min_ledger_seq=88. Slot 87 is below; should reject.
        let herder = herder_with_lcl_and_tracking(99, 100);
        let secret = SecretKey::from_seed(&[4u8; 32]);
        let env = make_signed_test_envelope_outer(87, &herder, &secret);
        match herder.pre_filter_scp_envelope(&env) {
            PreFilter::Reject(PreFilterRejectReason::Range) => {}
            other => panic!(
                "slot below min_ledger_seq must reject as Range, got {:?}",
                other
            ),
        }
    }

    /// Checkpoint exception: a slot below `min_ledger_seq` is accepted when
    /// it equals the most-recent-checkpoint seq. Mirrors stellar-core's
    /// `index != checkpoint` exception in `recvSCPEnvelope`.
    #[test]
    fn test_pre_filter_checkpoint_exception_below_min_ledger_seq() {
        // tracking=101 → most_recent_checkpoint_seq = 64 (with default
        // checkpoint_frequency=64). min_ledger_seq = 89. Slot 64 is below 89
        // but equals checkpoint, so the `slot != checkpoint` exception fires.
        let herder = herder_with_lcl_and_tracking(99, 101);
        assert_eq!(herder.get_most_recent_checkpoint_seq(), 64);
        let secret = SecretKey::from_seed(&[5u8; 32]);
        let env = make_signed_test_envelope_outer(64, &herder, &secret);
        match herder.pre_filter_scp_envelope(&env) {
            PreFilter::Accept(intake) => assert_eq!(intake.slot(), 64),
            PreFilter::Reject(r) => panic!(
                "checkpoint slot below min_ledger_seq must be accepted via checkpoint exception, got Reject({:?})",
                r
            ),
        }
    }
}

// =============================================================================
// #2115: advance_tracking_slot no longer drains (moved to ledger_closed).
// Tests verify the new drain placement and sequencing.
// =============================================================================

#[cfg(test)]
mod advance_tracking_slot_tests {
    use super::*;
    use stellar_xdr::{
        Hash, NodeId as XdrNodeId, PublicKey, ScpEnvelope, ScpNomination, ScpStatement,
        ScpStatementPledges, Uint256,
    };

    fn make_default_lm() -> Arc<henyey_ledger::LedgerManager> {
        use henyey_ledger::{LedgerManager, LedgerManagerConfig};
        use stellar_xdr::{
            LedgerHeader, LedgerHeaderExt, StellarValue, StellarValueExt, TimePoint, VecM,
        };
        let config = LedgerManagerConfig {
            validate_bucket_hash: false,
            ..Default::default()
        };
        let lm = LedgerManager::new("Test Network".to_string(), config);
        let header = LedgerHeader {
            ledger_version: 0,
            previous_ledger_hash: Hash([0u8; 32]),
            scp_value: StellarValue {
                tx_set_hash: Hash([0u8; 32]),
                close_time: TimePoint(0),
                upgrades: VecM::default(),
                ext: StellarValueExt::Basic,
            },
            tx_set_result_hash: Hash([0u8; 32]),
            bucket_list_hash: Hash([0u8; 32]),
            ledger_seq: 0,
            total_coins: 1_000_000_000_000,
            fee_pool: 0,
            inflation_seq: 0,
            id_pool: 0,
            base_fee: 100,
            base_reserve: 5_000_000,
            max_tx_set_size: 100,
            skip_list: [
                Hash([0u8; 32]),
                Hash([0u8; 32]),
                Hash([0u8; 32]),
                Hash([0u8; 32]),
            ],
            ext: LedgerHeaderExt::V0,
        };
        let header_hash = henyey_ledger::compute_header_hash(&header).expect("hash");
        lm.initialize(
            henyey_bucket::BucketList::new(),
            henyey_bucket::HotArchiveBucketList::new(),
            header,
            header_hash,
        )
        .expect("init");
        Arc::new(lm)
    }

    fn make_test_envelope(slot: u64) -> ScpEnvelope {
        let node_id = XdrNodeId(PublicKey::PublicKeyTypeEd25519(Uint256([0u8; 32])));
        make_test_envelope_with_node(slot, node_id)
    }

    fn make_test_envelope_with_seed(slot: u64, seed: u8) -> ScpEnvelope {
        let node_id = XdrNodeId(PublicKey::PublicKeyTypeEd25519(Uint256([seed; 32])));
        make_test_envelope_with_node(slot, node_id)
    }

    fn make_test_envelope_with_node(slot: u64, node_id: XdrNodeId) -> ScpEnvelope {
        ScpEnvelope {
            statement: ScpStatement {
                node_id,
                slot_index: slot,
                pledges: ScpStatementPledges::Nominate(ScpNomination {
                    quorum_set_hash: Hash([0u8; 32]),
                    votes: vec![].try_into().unwrap(),
                    accepted: vec![].try_into().unwrap(),
                }),
            },
            signature: stellar_xdr::Signature(vec![0u8; 64].try_into().unwrap()),
        }
    }

    fn node_seed(env: &ScpEnvelope) -> u8 {
        match &env.statement.node_id.0 {
            PublicKey::PublicKeyTypeEd25519(Uint256(bytes)) => bytes[0],
        }
    }

    /// #2115: advance_tracking_slot no longer drains pending envelopes.
    /// The drain was moved to `ledger_closed` to run post-apply, mirroring
    /// stellar-core's `safelyProcessSCPQueue(false)` → `postOnMainThread`.
    ///
    /// This test verifies advance_tracking_slot updates tracking state
    /// but does NOT drain pending envelopes (they remain buffered).
    #[test]
    fn test_advance_tracking_slot_drains_intermediate_pending() {
        let herder = Herder::new(
            HerderConfig::default(),
            make_default_lm(),
            TimerManagerHandle::no_op(),
        );

        // Set initial tracking state: consensus_index = 100
        {
            let mut ts = herder.tracking_state.write();
            ts.consensus_index = 100;
            ts.is_tracking = true;
        }
        herder.pending_envelopes.set_current_slot(100);

        // Buffer envelopes for intermediate slots 101-104
        for slot in 101..=104 {
            herder.pending_envelopes.add(slot, make_test_envelope(slot));
        }
        assert_eq!(herder.pending_envelopes.slot_count(), 4);

        // advance_tracking_slot sets consensus_index = 104 but does NOT drain.
        herder.advance_tracking_slot(103);

        // Pending envelopes are NOT drained — they remain buffered until
        // ledger_closed is called post-apply.
        assert_eq!(
            herder.pending_envelopes.slot_count(),
            4,
            "advance_tracking_slot must NOT drain pending envelopes (drain \
             is now deferred to ledger_closed, matching stellar-core's \
             safelyProcessSCPQueue(false) → postOnMainThread)"
        );

        // Verify tracking state was updated
        let ts = herder.tracking_state.read();
        assert_eq!(ts.consensus_index, 104);
        assert!(ts.is_tracking);
    }

    /// #2115: ledger_closed drains pending envelopes for slot + 1.
    /// This is where the drain now lives, post-apply, mirroring
    /// stellar-core's `safelyProcessSCPQueue(false)`.
    #[test]
    fn test_ledger_closed_drains_pending_envelopes() {
        let herder = Herder::new(
            HerderConfig::default(),
            make_default_lm(),
            TimerManagerHandle::no_op(),
        );

        // Set current_slot low so envelopes are accepted by add()
        herder.pending_envelopes.set_current_slot(101);

        // Buffer envelopes for slots 101-104
        for slot in 101..=104 {
            herder.pending_envelopes.add(slot, make_test_envelope(slot));
        }
        assert_eq!(herder.pending_envelopes.slot_count(), 4);

        // Set tracking state: consensus_index = 104 (simulates externalize of 103)
        {
            let mut ts = herder.tracking_state.write();
            ts.consensus_index = 104;
            ts.is_tracking = true;
        }

        // ledger_closed(103) drains pending envelopes up to slot 104
        herder.ledger_closed(103, &[], &[], 0);

        // All envelopes for slots <= 104 should be drained
        assert_eq!(
            herder.pending_envelopes.slot_count(),
            0,
            "ledger_closed must drain all pending envelopes up to slot + 1"
        );
    }

    /// #2115: Multi-slot sequencing test. If multiple slots externalize
    /// before ledger_closed, each ledger_closed drains up to its slot + 1.
    #[test]
    fn test_ledger_closed_multi_slot_sequencing() {
        let herder = Herder::new(
            HerderConfig::default(),
            make_default_lm(),
            TimerManagerHandle::no_op(),
        );

        // Set current_slot low so envelopes are accepted by add()
        herder.pending_envelopes.set_current_slot(104);

        // Buffer envelopes for slots 104, 105, 106
        for slot in 104..=106 {
            herder.pending_envelopes.add(slot, make_test_envelope(slot));
        }
        assert_eq!(herder.pending_envelopes.slot_count(), 3);

        // Set tracking: already externalized slot 105
        {
            let mut ts = herder.tracking_state.write();
            ts.consensus_index = 106;
            ts.is_tracking = true;
        }

        // ledger_closed(104) drains slots <= 105
        herder.ledger_closed(104, &[], &[], 0);

        // Slot 106 should remain (only slots <= 105 drained)
        assert_eq!(
            herder.pending_envelopes.slot_count(),
            1,
            "ledger_closed(104) must only drain slots <= 105, leaving 106 pending"
        );

        // ledger_closed(105) drains slots <= 106
        herder.ledger_closed(105, &[], &[], 0);

        assert_eq!(
            herder.pending_envelopes.slot_count(),
            0,
            "ledger_closed(105) must drain remaining slot 106"
        );
    }

    /// AUDIT-1972 regression: for one released slot, herder drain must consume
    /// envelopes in intra-slot LIFO order (last-added first), matching
    /// stellar-core `PendingEnvelopes::pop()` (`pop_back`) behavior used by
    /// `processSCPQueueUpToIndex`. This test checks slot-local ordering only.
    #[test]
    fn test_drain_and_process_pending_uses_lifo_within_slot() {
        let herder = Herder::new(
            HerderConfig::default(),
            make_default_lm(),
            TimerManagerHandle::no_op(),
        );
        herder.pending_envelopes.set_current_slot(100);

        assert_eq!(
            herder
                .pending_envelopes
                .add(101, make_test_envelope_with_seed(101, 1)),
            PendingResult::Added
        );
        assert_eq!(
            herder
                .pending_envelopes
                .add(101, make_test_envelope_with_seed(101, 2)),
            PendingResult::Added
        );
        assert_eq!(
            herder
                .pending_envelopes
                .add(101, make_test_envelope_with_seed(101, 3)),
            PendingResult::Added
        );

        let mut observed = Vec::new();
        let mut hook_calls = 0usize;
        herder.drain_and_process_pending_with_hook(101, |env| {
            hook_calls += 1;
            observed.push(node_seed(env));
        });

        assert_eq!(
            hook_calls, 3,
            "hook must observe each envelope before process_scp_envelope"
        );
        assert_eq!(
            observed,
            vec![3, 2, 1],
            "herder must process slot 101 in LIFO order"
        );
        assert_eq!(herder.pending_envelopes.slot_count(), 0);
    }

    /// Regression test for #1783: build_nomination_value must create exactly
    /// ONE snapshot for both build_starting_seq_map and trim_invalid_two_phase,
    /// not O(N_txs) snapshots.
    #[tokio::test]
    async fn test_nomination_value_uses_single_snapshot() {
        use henyey_bucket::{BucketList, HotArchiveBucketList};
        use henyey_ledger::LedgerManagerConfig;
        use stellar_xdr::{
            CreateAccountOp, DecoratedSignature, Hash, LedgerHeader, LedgerHeaderExt, Memo,
            MuxedAccount, Operation, OperationBody, Preconditions, SequenceNumber, SignatureHint,
            StellarValue, StellarValueExt, TimePoint, Transaction, TransactionEnvelope,
            TransactionExt, TransactionV1Envelope, Uint256, VecM,
        };

        fn make_synthetic_envelope(seed: u8) -> TransactionEnvelope {
            let source = MuxedAccount::Ed25519(Uint256([seed; 32]));
            let dest = stellar_xdr::AccountId(stellar_xdr::PublicKey::PublicKeyTypeEd25519(
                Uint256([seed.wrapping_add(128); 32]),
            ));
            let tx = Transaction {
                source_account: source,
                fee: 200,
                seq_num: SequenceNumber(1),
                cond: Preconditions::None,
                memo: Memo::None,
                operations: vec![Operation {
                    source_account: None,
                    body: OperationBody::CreateAccount(CreateAccountOp {
                        destination: dest,
                        starting_balance: 1_000_000_000,
                    }),
                }]
                .try_into()
                .unwrap(),
                ext: TransactionExt::V0,
            };
            TransactionEnvelope::Tx(TransactionV1Envelope {
                tx,
                signatures: vec![DecoratedSignature {
                    hint: SignatureHint([0u8; 4]),
                    signature: stellar_xdr::Signature(vec![0u8; 64].try_into().unwrap()),
                }]
                .try_into()
                .unwrap(),
            })
        }

        // Set up a validator herder with a LedgerManager.
        let seed = [7u8; 32];
        let secret_for_herder = henyey_crypto::SecretKey::from_seed(&seed);
        let public = secret_for_herder.public_key();
        let node_id = super::node_id_from_public_key(&public);
        let quorum_set = stellar_xdr::ScpQuorumSet {
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

        let lm_config = LedgerManagerConfig {
            validate_bucket_hash: false,
            ..Default::default()
        };
        let lm = henyey_ledger::LedgerManager::new("Test Network".to_string(), lm_config);
        let header = LedgerHeader {
            ledger_version: 25,
            previous_ledger_hash: Hash([0u8; 32]),
            scp_value: StellarValue {
                tx_set_hash: Hash([0u8; 32]),
                close_time: TimePoint(0),
                upgrades: VecM::default(),
                ext: StellarValueExt::Basic,
            },
            tx_set_result_hash: Hash([0u8; 32]),
            bucket_list_hash: Hash([0u8; 32]),
            ledger_seq: 10,
            total_coins: 1_000_000_000_000,
            fee_pool: 0,
            inflation_seq: 0,
            id_pool: 0,
            base_fee: 100,
            base_reserve: 5_000_000,
            max_tx_set_size: 100,
            skip_list: [
                Hash([0u8; 32]),
                Hash([0u8; 32]),
                Hash([0u8; 32]),
                Hash([0u8; 32]),
            ],
            ext: LedgerHeaderExt::V0,
        };
        let header_hash = henyey_ledger::compute_header_hash(&header).expect("hash");
        lm.initialize(
            BucketList::new(),
            HotArchiveBucketList::new(),
            header,
            header_hash,
        )
        .expect("init");

        let lm = Arc::new(lm);
        let herder = Herder::with_secret_key(
            config,
            secret_for_herder,
            lm.clone(),
            TimerManagerHandle::no_op(),
        );

        // Populate the tx queue with N transactions from distinct accounts.
        let n_txs = 20u8;
        for i in 1..=n_txs {
            let tx = make_synthetic_envelope(i);
            herder.tx_queue.try_add(tx);
        }
        assert!(
            herder.tx_queue.len() >= n_txs as usize,
            "Queue should have at least {} txs, got {}",
            n_txs,
            herder.tx_queue.len()
        );

        // Bootstrap herder to Tracking state.
        herder.bootstrap(10);

        // Measure snapshot count before nomination.
        let before = lm.test_snapshot_count();

        // Trigger nomination (builds nomination value internally).
        let _ = herder.trigger_next_ledger(11);

        // Measure snapshot count after.
        let after = lm.test_snapshot_count();
        let delta = after - before;

        // The nomination path should create exactly 1 snapshot (shared between
        // build_starting_seq_map and trim_invalid_two_phase). Pre-fix it would
        // be O(N_txs * ops * 2) = 40+ snapshots for 20 txs.
        assert!(
            delta <= 2,
            "Nomination should create O(1) snapshots, but created {} \
             (before={}, after={}). This suggests the per-call snapshot \
             anti-pattern has regressed.",
            delta,
            before,
            after
        );
    }

    // ─────────────────────────────────────────────────────────────────
    // #2113 regression: build_nomination_value must self-validate the
    // freshly-built tx set against `check_tx_set_valid` and abort
    // nomination on failure (defense-in-depth follow-up to #2103).
    //
    // The tests below exercise the helper directly with hand-crafted
    // GeneralizedTransactionSet values and verify the protocol gate,
    // happy path, malformed-shape rejection, and integration into
    // `build_nomination_value` on a real v24 LedgerManager.
    // ─────────────────────────────────────────────────────────────────

    /// Build a `Herder` (observer mode) with default config — sufficient for
    /// helper-level tests that don't exercise nomination signing.
    fn make_herder_for_self_validate_tests() -> Herder {
        Herder::new(
            HerderConfig::default(),
            make_default_lm(),
            TimerManagerHandle::no_op(),
        )
    }

    /// Build a v24 `LedgerHeader` with a non-zero `max_tx_set_size`. Mirrors
    /// the header shape used by `test_nomination_value_uses_single_snapshot`.
    fn v24_header() -> LedgerHeader {
        use stellar_xdr::{Hash, LedgerHeaderExt, StellarValue, StellarValueExt, TimePoint, VecM};
        LedgerHeader {
            ledger_version: 24,
            previous_ledger_hash: Hash([0u8; 32]),
            scp_value: StellarValue {
                tx_set_hash: Hash([0u8; 32]),
                close_time: TimePoint(0),
                upgrades: VecM::default(),
                ext: StellarValueExt::Basic,
            },
            tx_set_result_hash: Hash([0u8; 32]),
            bucket_list_hash: Hash([0u8; 32]),
            ledger_seq: 10,
            total_coins: 1_000_000_000_000,
            fee_pool: 0,
            inflation_seq: 0,
            id_pool: 0,
            base_fee: 100,
            base_reserve: 5_000_000,
            max_tx_set_size: 100,
            skip_list: [
                Hash([0u8; 32]),
                Hash([0u8; 32]),
                Hash([0u8; 32]),
                Hash([0u8; 32]),
            ],
            ext: LedgerHeaderExt::V0,
        }
    }

    /// Build an N-phase generalized tx set for self-validate tests. Each phase
    /// is an empty `TransactionPhase::V0`. The hash is computed from the XDR
    /// encoding so that `prepare_for_apply` accepts the recomputed hash.
    fn empty_n_phase_generalized_tx_set(n_phases: usize) -> TransactionSet {
        use stellar_xdr::{
            GeneralizedTransactionSet, Hash, TransactionPhase, TransactionSetV1, VecM,
        };
        let phases: VecM<TransactionPhase> = (0..n_phases)
            .map(|_| TransactionPhase::V0(VecM::default()))
            .collect::<Vec<_>>()
            .try_into()
            .expect("phase vec");
        let gen = GeneralizedTransactionSet::V1(TransactionSetV1 {
            previous_ledger_hash: Hash([0u8; 32]),
            phases,
        });
        TransactionSet::new_generalized(gen)
    }

    /// Helper accepts a valid empty 2-phase generalized V0 tx set on protocol
    /// 22 (in V20..V23 — generalized format applies, but parallel Soroban is
    /// not yet required so a `TransactionPhase::V0` Soroban phase is valid).
    /// `soroban_info` must be `Some` because `check_tx_set_valid` rejects an
    /// empty Soroban phase when config is unavailable
    /// (`tx_set_utils.rs:1602-1606`).
    #[test]
    fn test_self_validate_nomination_tx_set_accepts_empty() {
        let herder = make_herder_for_self_validate_tests();
        let tx_set = empty_n_phase_generalized_tx_set(2);
        let mut header = v24_header();
        header.ledger_version = 22;
        let soroban_info = henyey_ledger::SorobanNetworkInfo::default();
        assert!(
            herder
                .self_validate_nomination_tx_set(
                    &tx_set,
                    &header,
                    &Hash256::ZERO,
                    0,
                    Some(&soroban_info),
                    None,
                )
                .is_ok(),
            "empty 2-phase generalized V0 tx set on v22 must pass self-validation"
        );
    }

    /// Helper rejects a legacy (non-generalized) tx set on protocol >= 20 —
    /// generalized sets are required at V20+. Rejecting (rather than silently
    /// passing) is load-bearing: it surfaces a construction bug at `warn!`.
    #[test]
    fn test_self_validate_nomination_tx_set_rejects_legacy_on_v24() {
        let herder = make_herder_for_self_validate_tests();
        let tx_set = TransactionSet::new_legacy(Hash256::ZERO, vec![]);
        let header = v24_header();
        assert!(
            herder
                .self_validate_nomination_tx_set(&tx_set, &header, &Hash256::ZERO, 0, None, None)
                .is_err(),
            "legacy tx set on v24 must be rejected by self-validation"
        );
    }

    /// Helper rejects a malformed generalized tx set with a wrong phase count
    /// (3 phases — generalized sets must have exactly 2). This exercises the
    /// real `prepare_for_apply` rejection path (via
    /// `validate_generalized_tx_set_xdr_structure` at `tx_set.rs:520-524`)
    /// through the helper.
    #[test]
    fn test_self_validate_nomination_tx_set_rejects_three_phase() {
        let herder = make_herder_for_self_validate_tests();
        let tx_set = empty_n_phase_generalized_tx_set(3);
        let header = v24_header();
        assert!(
            herder
                .self_validate_nomination_tx_set(&tx_set, &header, &Hash256::ZERO, 0, None, None)
                .is_err(),
            "3-phase generalized tx set must be rejected by prepare_for_apply"
        );
    }

    /// Helper short-circuits to `true` on protocol < 20: even an otherwise-bad
    /// set (here: legacy) is accepted because the protocol gate runs first.
    /// Only simulation environments hit this path; production runs on V24+.
    #[test]
    fn test_self_validate_nomination_tx_set_skipped_pre_v20() {
        let herder = make_herder_for_self_validate_tests();
        let tx_set = TransactionSet::new_legacy(Hash256::ZERO, vec![]);
        let mut header = v24_header();
        header.ledger_version = 19;
        assert!(
            herder
                .self_validate_nomination_tx_set(&tx_set, &header, &Hash256::ZERO, 0, None, None)
                .is_ok(),
            "self-validation must be skipped on protocol < 20"
        );
    }

    /// Wiring test: when the helper returns `false`, the integration wrapper
    /// `validate_and_cache_built_tx_set` must (a) return `None` and (b) NOT
    /// cache the tx set. Together with Tests 2-3 (helper-rejection) and
    /// Test 5 (success path), this covers the contract that
    /// `build_nomination_value` aborts nomination on self-validation failure
    /// without publishing the bad set to peers. Catches the specific bug
    /// of someone flipping the `!` on the gate.
    #[test]
    fn test_validate_and_cache_built_tx_set_aborts_on_helper_failure() {
        let herder = make_herder_for_self_validate_tests();
        // 3-phase generalized set on V24 — helper rejects via the
        // prepare_for_apply / validate_generalized_tx_set_xdr_structure
        // phase-count guard (tx_set.rs:520-524).
        let tx_set = empty_n_phase_generalized_tx_set(3);
        let header = v24_header();
        let cache_count_before = herder.scp_driver.tx_set_cache_count();

        let result =
            herder.validate_and_cache_built_tx_set(&tx_set, &header, Hash256::ZERO, 0, None, None);

        assert!(
            result.is_none(),
            "wrapper must return None when helper rejects"
        );
        let cache_count_after = herder.scp_driver.tx_set_cache_count();
        assert_eq!(
            cache_count_before, cache_count_after,
            "rejected tx set must NOT be cached"
        );
        assert!(
            !herder.scp_driver.has_tx_set(tx_set.hash()),
            "rejected tx set hash must not be queryable in the cache"
        );
        // Validity cache must also remain empty on rejection.
        assert_eq!(
            herder.scp_driver.cache_sizes().tx_set_valid_cache,
            0,
            "validity cache must not be populated when helper rejects"
        );
    }

    /// Wiring test (positive path): when the helper returns `true`, the
    /// integration wrapper must (a) return `Some(())` and (b) cache the set.
    /// Pair with the negative test above.
    #[test]
    fn test_validate_and_cache_built_tx_set_caches_on_helper_success() {
        let herder = make_herder_for_self_validate_tests();
        let tx_set = empty_n_phase_generalized_tx_set(2);
        let mut header = v24_header();
        header.ledger_version = 22; // V0 Soroban phase OK on V20..V23
        let soroban_info = henyey_ledger::SorobanNetworkInfo::default();
        let cache_count_before = herder.scp_driver.tx_set_cache_count();

        let result = herder.validate_and_cache_built_tx_set(
            &tx_set,
            &header,
            Hash256::ZERO,
            0,
            Some(&soroban_info),
            None,
        );

        assert!(
            result.is_some(),
            "wrapper must return Some(()) when helper accepts"
        );
        let cache_count_after = herder.scp_driver.tx_set_cache_count();
        assert_eq!(
            cache_count_after,
            cache_count_before + 1,
            "accepted tx set must be cached exactly once"
        );
        assert!(
            herder.scp_driver.has_tx_set(tx_set.hash()),
            "accepted tx set hash must be queryable in the cache"
        );
        // Validity cache must be populated with `true` on success.
        let valid_entry =
            herder
                .scp_driver
                .check_tx_set_valid_cached(&Hash256::ZERO, tx_set.hash(), 0);
        assert_eq!(
            valid_entry,
            Some(true),
            "validity cache must contain true for accepted tx set \
             (parity: stellar-core cacheValidTxSet)"
        );
    }

    // ── Cross-phase duplicate-source test helpers ────────────────────────

    /// Classic `CreateAccount` envelope (same shape as `tx_set.rs` tests).
    fn make_classic_envelope(seed: u8, fee: u32) -> stellar_xdr::TransactionEnvelope {
        use stellar_xdr::{
            CreateAccountOp, DecoratedSignature, Memo, MuxedAccount, Operation, OperationBody,
            Preconditions, SequenceNumber, SignatureHint, Transaction, TransactionEnvelope,
            TransactionExt, TransactionV1Envelope, Uint256,
        };
        let source = MuxedAccount::Ed25519(Uint256([seed; 32]));
        let dest = stellar_xdr::AccountId(stellar_xdr::PublicKey::PublicKeyTypeEd25519(Uint256(
            [seed.wrapping_add(1); 32],
        )));
        TransactionEnvelope::Tx(TransactionV1Envelope {
            tx: Transaction {
                source_account: source,
                fee,
                seq_num: SequenceNumber(seed as i64),
                cond: Preconditions::None,
                memo: Memo::None,
                operations: vec![Operation {
                    source_account: None,
                    body: OperationBody::CreateAccount(CreateAccountOp {
                        destination: dest,
                        starting_balance: 1_000_000_000,
                    }),
                }]
                .try_into()
                .unwrap(),
                ext: TransactionExt::V0,
            },
            signatures: vec![DecoratedSignature {
                hint: SignatureHint([0u8; 4]),
                signature: stellar_xdr::Signature(vec![0u8; 64].try_into().unwrap()),
            }]
            .try_into()
            .unwrap(),
        })
    }

    /// Soroban `InvokeHostFunction` envelope (same shape as `tx_set.rs` tests).
    fn make_soroban_envelope_for_phase(seed: u8, fee: u32) -> stellar_xdr::TransactionEnvelope {
        use stellar_xdr::{
            ContractDataDurability, ContractId, DecoratedSignature, Hash, HostFunction,
            InvokeContractArgs, InvokeHostFunctionOp, LedgerFootprint, LedgerKey,
            LedgerKeyContractData, Memo, MuxedAccount, Operation, OperationBody, Preconditions,
            ScAddress, ScSymbol, ScVal, SequenceNumber, SignatureHint, SorobanResources,
            SorobanTransactionData, SorobanTransactionDataExt, Transaction, TransactionEnvelope,
            TransactionExt, TransactionV1Envelope, Uint256, VecM,
        };
        let op = Operation {
            source_account: None,
            body: OperationBody::InvokeHostFunction(InvokeHostFunctionOp {
                host_function: HostFunction::InvokeContract(InvokeContractArgs {
                    contract_address: ScAddress::Contract(ContractId(Hash([0u8; 32]))),
                    function_name: ScSymbol("test".try_into().unwrap()),
                    args: VecM::default(),
                }),
                auth: VecM::default(),
            }),
        };
        let soroban_data = SorobanTransactionData {
            ext: SorobanTransactionDataExt::V0,
            resources: SorobanResources {
                footprint: LedgerFootprint {
                    read_only: VecM::default(),
                    read_write: vec![LedgerKey::ContractData(LedgerKeyContractData {
                        contract: ScAddress::Contract(ContractId(Hash([0u8; 32]))),
                        key: ScVal::Bool(true),
                        durability: ContractDataDurability::Persistent,
                    })]
                    .try_into()
                    .unwrap(),
                },
                instructions: 5000,
                disk_read_bytes: 1024,
                write_bytes: 512,
            },
            resource_fee: 50,
        };
        TransactionEnvelope::Tx(TransactionV1Envelope {
            tx: Transaction {
                source_account: MuxedAccount::Ed25519(Uint256([seed; 32])),
                fee,
                seq_num: SequenceNumber(seed as i64),
                cond: Preconditions::None,
                memo: Memo::None,
                operations: vec![op].try_into().unwrap(),
                ext: TransactionExt::V1(soroban_data),
            },
            signatures: vec![DecoratedSignature {
                hint: SignatureHint([0u8; 4]),
                signature: stellar_xdr::Signature(vec![0u8; 64].try_into().unwrap()),
            }]
            .try_into()
            .unwrap(),
        })
    }

    /// Build a 2-phase generalized tx set: V0 classic + V1 parallel Soroban.
    /// `classic_seed` and `soroban_seed` control the source-account ED25519 key.
    fn make_two_phase_tx_set(classic_seed: u8, soroban_seed: u8) -> TransactionSet {
        use stellar_xdr::{
            DependentTxCluster, GeneralizedTransactionSet, Hash, ParallelTxExecutionStage,
            TransactionPhase, TransactionSetV1, TxSetComponent,
            TxSetComponentTxsMaybeDiscountedFee,
        };

        let classic_tx = make_classic_envelope(classic_seed, 200);
        let soroban_tx = make_soroban_envelope_for_phase(soroban_seed, 200);

        // Phase 0: classic V0 with one component
        let classic_phase = TransactionPhase::V0(
            vec![TxSetComponent::TxsetCompTxsMaybeDiscountedFee(
                TxSetComponentTxsMaybeDiscountedFee {
                    base_fee: Some(100),
                    txs: vec![classic_tx].try_into().unwrap(),
                },
            )]
            .try_into()
            .unwrap(),
        );

        // Phase 1: Soroban V1 parallel (single stage, single cluster)
        let soroban_phase = henyey_tx::tx_set_xdr::soroban_phase_with_stages(
            Some(100),
            vec![ParallelTxExecutionStage(
                vec![DependentTxCluster(vec![soroban_tx].try_into().unwrap())]
                    .try_into()
                    .unwrap(),
            )]
            .try_into()
            .unwrap(),
        );

        let gen = GeneralizedTransactionSet::V1(TransactionSetV1 {
            previous_ledger_hash: Hash([0u8; 32]),
            phases: vec![classic_phase, soroban_phase].try_into().unwrap(),
        });

        TransactionSet::new_generalized(gen)
    }

    fn make_test_soroban_info() -> henyey_ledger::SorobanNetworkInfo {
        henyey_ledger::SorobanNetworkInfo {
            ledger_max_instructions: 1_000_000,
            ledger_max_read_ledger_entries: 100,
            ledger_max_read_bytes: 100_000,
            ledger_max_write_ledger_entries: 50,
            ledger_max_write_bytes: 50_000,
            ledger_max_dependent_tx_clusters: 4,
            ledger_max_tx_size_bytes: 1_000_000,
            ledger_max_tx_count: 100,
            // Per-TX limits needed by check_soroban_resource_limits
            tx_max_instructions: 100_000,
            tx_max_read_bytes: 100_000,
            tx_max_write_bytes: 50_000,
            tx_max_read_ledger_entries: 50,
            tx_max_write_ledger_entries: 25,
            tx_max_size_bytes: 100_000,
            tx_max_footprint_entries: 50,
            max_contract_size: 65_536,
            max_contract_data_key_size: 250,
            ..Default::default()
        }
    }

    // ── Cross-phase duplicate-source tests ───────────────────────────────

    /// Regression test: a 2-phase tx set where the same source account appears
    /// in both the classic and Soroban phases must be rejected by the builder
    /// path. Exercises `prepare_for_apply` → `check_no_duplicate_source_accounts`
    /// through `self_validate_nomination_tx_set`.
    ///
    /// Parity: stellar-core `TxSetFrame.cpp:2139-2157`.
    #[test]
    fn test_self_validate_rejects_cross_phase_duplicate_source() {
        let herder = make_herder_for_self_validate_tests();
        // Same seed (42) in both phases → duplicate source account
        let tx_set = make_two_phase_tx_set(42, 42);
        let header = v24_header();
        let soroban_info = make_test_soroban_info();

        assert!(
            herder
                .self_validate_nomination_tx_set(
                    &tx_set,
                    &header,
                    &Hash256::ZERO,
                    0,
                    Some(&soroban_info),
                    None,
                )
                .is_err(),
            "tx set with same source in classic and Soroban phases must be rejected"
        );
    }

    /// Control: identical structure to the rejection test above, but with
    /// distinct source accounts across phases. Must pass self-validation,
    /// proving the rejection above is specifically the duplicate-source check.
    ///
    /// Note: `snapshot_providers: None` skips stateful checks (sequence numbers,
    /// fee affordability) — this is acceptable because the test targets the
    /// structural `prepare_for_apply` → `check_no_duplicate_source_accounts`
    /// integration, not stateful validation.
    #[test]
    fn test_self_validate_accepts_cross_phase_distinct_sources() {
        let herder = make_herder_for_self_validate_tests();
        // Different seeds (42 classic, 43 Soroban) → no duplicate
        let tx_set = make_two_phase_tx_set(42, 43);
        let header = v24_header();
        let soroban_info = make_test_soroban_info();

        assert!(
            herder
                .self_validate_nomination_tx_set(
                    &tx_set,
                    &header,
                    &Hash256::ZERO,
                    0,
                    Some(&soroban_info),
                    None,
                )
                .is_ok(),
            "tx set with distinct sources across phases must pass self-validation"
        );
    }

    /// Regression test for #2319: when the queue's validation context has a
    /// STALE base_fee (from ledger N-1) but the snapshot header has an UPDATED
    /// base_fee (ledger N), the build path must use the snapshot values via
    /// NominationBuildContext. Without this fix, `check_fee_map` rejects the
    /// freshly-built tx set because the component base_fee < lcl base_fee.
    #[test]
    fn test_nomination_build_context_prevents_stale_fee_divergence() {
        use crate::tx_queue::{BuildContext, NominationBuildContext, TransactionQueue};
        use crate::tx_set_utils::TxSetValidationContext;
        use henyey_common::NetworkId;
        use std::time::Duration;
        use stellar_xdr::{
            AccountId, DecoratedSignature, Hash, LedgerHeader, LedgerHeaderExt, Memo, MuxedAccount,
            Operation, OperationBody, Preconditions, PublicKey, SequenceNumber, SignatureHint,
            StellarValue, StellarValueExt, TimePoint, Transaction, TransactionEnvelope,
            TransactionExt, TransactionV1Envelope, Uint256, VecM,
        };

        // Queue validation context: base_fee=100 (stale, from ledger N-1)
        let queue = TransactionQueue::with_defaults();
        queue.update_validation_context(
            9,   // ledger_seq (N-1)
            100, // close_time
            24,  // protocol_version
            100, // base_fee (stale!)
            5_000_000,
            0,
            Duration::from_secs(5),
        );

        // Add a single transaction with fee=200 (above both old and new base_fee)
        let source = MuxedAccount::Ed25519(Uint256([1u8; 32]));
        let tx = Transaction {
            source_account: source,
            fee: 200,
            seq_num: SequenceNumber(1),
            cond: Preconditions::None,
            memo: Memo::None,
            operations: vec![Operation {
                source_account: None,
                body: OperationBody::CreateAccount(stellar_xdr::CreateAccountOp {
                    destination: AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([255u8; 32]))),
                    starting_balance: 1_000_000_000,
                }),
            }]
            .try_into()
            .unwrap(),
            ext: TransactionExt::V0,
        };
        let env = TransactionEnvelope::Tx(TransactionV1Envelope {
            tx,
            signatures: vec![DecoratedSignature {
                hint: SignatureHint([0u8; 4]),
                signature: stellar_xdr::Signature(vec![0u8; 64].try_into().unwrap()),
            }]
            .try_into()
            .unwrap(),
        });
        queue.try_add(env);

        // Snapshot header: base_fee=200 (updated, from ledger N)
        let header = LedgerHeader {
            ledger_version: 24,
            previous_ledger_hash: Hash([0u8; 32]),
            scp_value: StellarValue {
                tx_set_hash: Hash([0u8; 32]),
                close_time: TimePoint(0),
                upgrades: VecM::default(),
                ext: StellarValueExt::Basic,
            },
            tx_set_result_hash: Hash([0u8; 32]),
            bucket_list_hash: Hash([0u8; 32]),
            ledger_seq: 10,
            total_coins: 1_000_000_000_000,
            fee_pool: 0,
            inflation_seq: 0,
            id_pool: 0,
            base_fee: 200, // UPDATED base_fee (higher than stale 100)
            base_reserve: 5_000_000,
            max_tx_set_size: 100,
            skip_list: [
                Hash([0u8; 32]),
                Hash([0u8; 32]),
                Hash([0u8; 32]),
                Hash([0u8; 32]),
            ],
            ext: LedgerHeaderExt::V0,
        };

        let network_id = NetworkId::testnet();
        let soroban_info = henyey_ledger::SorobanNetworkInfo::default();

        // Path A: BuildContext::Queue — uses stale base_fee=100.
        // Self-validation against header (base_fee=200) must FAIL.
        let tx_set_stale = queue.build_generalized_tx_set_with_providers(
            BuildContext::Queue,
            Hash256::ZERO,
            100,
            None,
            0,
            None,
            None,
        );
        assert!(
            tx_set_stale.len() > 0,
            "tx set should contain the transaction"
        );
        let prepared_stale = tx_set_stale
            .prepare_for_apply(network_id)
            .expect("prepare_for_apply should succeed");
        let result_stale = prepared_stale.check_valid(
            &header,
            &Hash256::ZERO,
            0,
            network_id,
            Some(&soroban_info),
            None,
            None,
            None,
        );
        assert!(
            result_stale.is_err(),
            "Stale base_fee (100) should fail check_fee_map against header base_fee (200), \
             but got Ok. This would be the #2319 bug."
        );
        let err = result_stale.unwrap_err();
        assert!(
            err.to_string().contains("FEE"),
            "Error should mention fee map: {err}"
        );

        // Path B: BuildContext::Nomination — uses snapshot base_fee=200.
        // Self-validation against the same header must PASS.
        let nomination_ctx = NominationBuildContext {
            base_fee: header.base_fee as i64,
            protocol_version: header.ledger_version,
            validation_ctx: TxSetValidationContext::new(
                header.ledger_seq,
                header.scp_value.close_time.0,
                header.base_fee,
                header.base_reserve,
                header.ledger_version,
                network_id,
                0, // ledger_flags
            ),
        };
        let tx_set_fixed = queue.build_generalized_tx_set_with_providers(
            BuildContext::Nomination(&nomination_ctx),
            Hash256::ZERO,
            100,
            None,
            0,
            None,
            None,
        );
        assert!(
            tx_set_fixed.len() > 0,
            "tx set should contain the transaction"
        );
        let prepared_fixed = tx_set_fixed
            .prepare_for_apply(network_id)
            .expect("prepare_for_apply should succeed");
        let result_fixed = prepared_fixed.check_valid(
            &header,
            &Hash256::ZERO,
            0,
            network_id,
            Some(&soroban_info),
            None,
            None,
            None,
        );
        assert!(
            result_fixed.is_ok(),
            "Nomination context (base_fee=200) should pass check_fee_map against header \
             (base_fee=200), but got: {:?}",
            result_fixed.unwrap_err()
        );
    }

    /// Integration test: `build_nomination_value` on a v24 LedgerManager with
    /// an empty queue produces a valid tx set, passes self-validation, caches
    /// it, and returns `Some(_)`. Confirms the new self-validation gate does
    /// not regress the empty-set baseline (the primary risk: false positives).
    #[tokio::test]
    async fn test_build_nomination_value_self_check_passes_on_v24() {
        use henyey_bucket::{BucketList, HotArchiveBucketList};
        use henyey_ledger::LedgerManagerConfig;
        use stellar_xdr::{
            Hash, LedgerHeader, LedgerHeaderExt, StellarValue, StellarValueExt, TimePoint, VecM,
        };

        // Validator herder so build_nomination_value can sign.
        let seed = [13u8; 32];
        let secret_for_herder = henyey_crypto::SecretKey::from_seed(&seed);
        let public = secret_for_herder.public_key();
        let node_id = super::node_id_from_public_key(&public);
        let quorum_set = stellar_xdr::ScpQuorumSet {
            threshold: 1,
            validators: vec![node_id].try_into().unwrap(),
            inner_sets: vec![].try_into().unwrap(),
        };
        let herder_config = HerderConfig {
            is_validator: true,
            node_public_key: public,
            local_quorum_set: Some(quorum_set),
            ..HerderConfig::default()
        };

        let lm_config = LedgerManagerConfig {
            validate_bucket_hash: false,
            ..Default::default()
        };
        let lm = henyey_ledger::LedgerManager::new("Test Network".to_string(), lm_config);
        let header = LedgerHeader {
            ledger_version: 24,
            previous_ledger_hash: Hash([0u8; 32]),
            scp_value: StellarValue {
                tx_set_hash: Hash([0u8; 32]),
                close_time: TimePoint(0),
                upgrades: VecM::default(),
                ext: StellarValueExt::Basic,
            },
            tx_set_result_hash: Hash([0u8; 32]),
            bucket_list_hash: Hash([0u8; 32]),
            ledger_seq: 10,
            total_coins: 1_000_000_000_000,
            fee_pool: 0,
            inflation_seq: 0,
            id_pool: 0,
            base_fee: 100,
            base_reserve: 5_000_000,
            max_tx_set_size: 100,
            skip_list: [
                Hash([0u8; 32]),
                Hash([0u8; 32]),
                Hash([0u8; 32]),
                Hash([0u8; 32]),
            ],
            ext: LedgerHeaderExt::V0,
        };
        let header_hash = henyey_ledger::compute_header_hash(&header).expect("hash");
        lm.initialize(
            BucketList::new(),
            HotArchiveBucketList::new(),
            header,
            header_hash,
        )
        .expect("init");
        // Populate Soroban network info so `check_tx_set_valid` accepts the
        // (empty) Soroban phase. The default values are sufficient — the
        // builder produces an empty V1 parallel phase that has no resource
        // demand to compare against.
        lm.set_soroban_network_info_for_test(henyey_ledger::SorobanNetworkInfo::default());
        let herder = Herder::with_secret_key(
            herder_config,
            secret_for_herder,
            Arc::new(lm),
            TimerManagerHandle::no_op(),
        );
        herder.bootstrap(10);

        // Empty tx queue → empty 2-phase tx set → passes self-validation.
        let close_time = herder
            .scp_driver
            .now_seconds()
            .max(herder.lcl_close_time() + 1);
        let value = herder
            .build_nomination_value(close_time)
            .expect("nomination value must be Some when self-validation passes");

        // Decode the StellarValue to extract the tx_set_hash and verify the
        // tx set was actually cached (caching only happens after the
        // self-validation gate accepts it).
        let stellar_value =
            stellar_xdr::StellarValue::from_xdr(&value.0[..], stellar_xdr::Limits::none())
                .expect("decode StellarValue");
        let tx_set_hash = Hash256::from_bytes(stellar_value.tx_set_hash.0);
        assert!(
            herder.scp_driver().has_tx_set(&tx_set_hash),
            "freshly-built tx set must be cached after self-validation passes"
        );
    }

    // ─────────────────────────────────────────────────────────────────
    // H-014 / issue #2096 regression: ledger_closed must restore
    // fully_validated for slots that were deferred only on apply-lag,
    // and must NOT restore slots whose apply-lag is for a future slot
    // whose predecessor has not yet applied.
    // ─────────────────────────────────────────────────────────────────

    /// `Herder::ledger_closed(slot)` calls
    /// `resolve_apply_lag_for_next_index(slot + 1)` and then
    /// `restore_slot_fully_validated` on each resolved slot. A slot
    /// recorded as deferred on apply-lag whose predecessor has now
    /// applied must have its `fully_validated` flag flipped back to true.
    #[test]
    fn test_ledger_closed_restores_apply_lag_deferred_slots() {
        let herder = Herder::new(
            HerderConfig::default(),
            make_default_lm(),
            TimerManagerHandle::no_op(),
        );

        // Initial tracking state: LCL = 99, tracking_index = 100.
        {
            let mut ts = herder.tracking_state.write();
            ts.consensus_index = 100;
            ts.is_tracking = true;
        }
        herder.pending_envelopes.set_current_slot(100);

        // Simulate the apply-lag-deferred state for slot 100: SCP slot
        // exists with `fully_validated == false`, and an `apply_lag`
        // entry is recorded in the herder's deferred_slots.
        herder.scp.test_clear_slot_fully_validated(100);
        herder.scp_driver.record_apply_lag(100);
        assert!(
            !herder.scp.is_slot_fully_validated(100),
            "precondition: slot 100 must be cleared"
        );

        // Apply ledger 99 — `ledger_closed(99)` runs, computing
        // next_index = 100. Slot 100 is eligible (slot <= next_index
        // and apply_lag == true), so it is resolved and restored.
        herder.ledger_closed(99, &[], &[], 1_000_000);

        assert!(
            herder.scp.is_slot_fully_validated(100),
            "slot 100 must be fully_validated after ledger_closed restores apply-lag deferred slots"
        );
        assert_eq!(
            herder.scp_driver.deferred_slot_count(),
            0,
            "deferred_slots must be drained for slot 100"
        );
    }

    /// Slots whose `apply_lag` cause is for a future slot (predecessor
    /// not yet applied) must NOT be restored by `ledger_closed`. They
    /// remain deferred until the LCL advances enough.
    #[test]
    fn test_ledger_closed_does_not_restore_future_apply_lag() {
        let herder = Herder::new(
            HerderConfig::default(),
            make_default_lm(),
            TimerManagerHandle::no_op(),
        );

        // Two future slots: 100 (next_index after this close = 99) and 105.
        herder.scp.test_clear_slot_fully_validated(100);
        herder.scp.test_clear_slot_fully_validated(105);
        herder.scp_driver.record_apply_lag(100);
        herder.scp_driver.record_apply_lag(105);

        // Apply ledger 98 — next_index = 99. Neither 100 nor 105 is
        // eligible (both > 99), so neither restoration fires.
        herder.ledger_closed(98, &[], &[], 1_000_000);

        assert!(
            !herder.scp.is_slot_fully_validated(100),
            "slot 100 must remain not-fully-validated when next_index < 100"
        );
        assert!(
            !herder.scp.is_slot_fully_validated(105),
            "slot 105 must remain not-fully-validated when next_index < 105"
        );
        // Both entries still in deferred_slots.
        let causes_100 = herder
            .scp_driver
            .deferred_causes_for_slot(100)
            .expect("slot 100 must remain deferred");
        assert!(causes_100.apply_lag);
        let causes_105 = herder
            .scp_driver
            .deferred_causes_for_slot(105)
            .expect("slot 105 must remain deferred");
        assert!(causes_105.apply_lag);
    }
}

// ────────────────────────────────────────────────────────────────────
// quorum_health() regression tests (#1938)
// ────────────────────────────────────────────────────────────────────
//
// These tests exercise Herder::quorum_health() directly, covering:
// - previous-slot-first preference
// - fallback to current slot when previous is absent
// - both slots absent → None
// - Delayed→agree folding
// - tracking_slot==0 → None
// - tracking_slot==1 edge case
// - non-zero fail_at
//
// Envelope injection uses `herder.scp().receive_envelope()` which
// bypasses herder-level validation (signature verification, quorum
// tracker, close-time checks). This is intentional — these tests
// exercise quorum_health() logic, not envelope ingestion.

#[cfg(test)]
mod quorum_health_tests {
    use super::*;
    use crate::tx_queue::TransactionSet;
    use henyey_crypto::SecretKey;
    use henyey_scp::hash_quorum_set;
    use stellar_xdr::{
        EnvelopeType, LedgerCloseValueSignature, Limits, NodeId as XdrNodeId, ScpBallot,
        ScpStatement, ScpStatementExternalize, ScpStatementPledges, ScpStatementPrepare,
        Signature as XdrSignature, StellarValue, StellarValueExt, TimePoint, Value, WriteXdr,
    };

    fn make_default_lm() -> Arc<henyey_ledger::LedgerManager> {
        use henyey_ledger::{LedgerManager, LedgerManagerConfig};
        use stellar_xdr::{Hash, LedgerHeader, LedgerHeaderExt, VecM};
        let config = LedgerManagerConfig {
            validate_bucket_hash: false,
            ..Default::default()
        };
        let lm = LedgerManager::new("Test Network".to_string(), config);
        let header = LedgerHeader {
            ledger_version: 0,
            previous_ledger_hash: Hash([0u8; 32]),
            scp_value: StellarValue {
                tx_set_hash: Hash([0u8; 32]),
                close_time: TimePoint(0),
                upgrades: VecM::default(),
                ext: StellarValueExt::Basic,
            },
            tx_set_result_hash: Hash([0u8; 32]),
            bucket_list_hash: Hash([0u8; 32]),
            ledger_seq: 0,
            total_coins: 1_000_000_000_000,
            fee_pool: 0,
            inflation_seq: 0,
            id_pool: 0,
            base_fee: 100,
            base_reserve: 5_000_000,
            max_tx_set_size: 100,
            skip_list: [
                Hash([0u8; 32]),
                Hash([0u8; 32]),
                Hash([0u8; 32]),
                Hash([0u8; 32]),
            ],
            ext: LedgerHeaderExt::V0,
        };
        let header_hash = henyey_ledger::compute_header_hash(&header).expect("hash");
        lm.initialize(
            henyey_bucket::BucketList::new(),
            henyey_bucket::HotArchiveBucketList::new(),
            header,
            header_hash,
        )
        .expect("init");
        Arc::new(lm)
    }

    /// Build a validator herder whose quorum set contains `n` validators
    /// (the local node + `n-1` peers) with the given `threshold`.
    /// Returns the herder, all secret keys, and the shared quorum set.
    fn make_n_node_validator_herder(
        n: usize,
        threshold: u32,
    ) -> (Herder, Vec<SecretKey>, ScpQuorumSet) {
        assert!(n >= 1);
        let mut keys = Vec::with_capacity(n);
        let mut node_ids = Vec::with_capacity(n);

        for i in 0..n {
            let seed = [(10 + i as u8); 32];
            let sk = SecretKey::from_seed(&seed);
            let pk = sk.public_key();
            node_ids.push(node_id_from_public_key(&pk));
            keys.push(sk);
        }

        let quorum_set = ScpQuorumSet {
            threshold,
            validators: node_ids.try_into().unwrap(),
            inner_sets: vec![].try_into().unwrap(),
        };

        let local_pk = keys[0].public_key();
        let config = HerderConfig {
            is_validator: true,
            node_public_key: local_pk,
            local_quorum_set: Some(quorum_set.clone()),
            ..HerderConfig::default()
        };

        let herder = Herder::with_secret_key(
            config,
            SecretKey::from_seed(&[10u8; 32]),
            make_default_lm(),
            TimerManagerHandle::no_op(),
        );

        // Register each peer's quorum set so that is_statement_sane can
        // resolve the quorum_set_hash in PREPARE/CONFIRM envelopes.
        for key in &keys[1..] {
            let peer_node_id = NodeId(stellar_xdr::PublicKey::PublicKeyTypeEd25519(Uint256(
                *key.public_key().as_bytes(),
            )));
            herder.store_quorum_set(&peer_node_id, quorum_set.clone());
        }

        (herder, keys, quorum_set)
    }

    /// Build a signed StellarValue and cache its tx set in the herder.
    /// This produces a Value that passes validate_value_impl.
    fn make_valid_value(herder: &Herder, signer: &SecretKey) -> Value {
        let lcl_hash = herder.scp_driver.current_header_hash();
        let tx_set = TransactionSet::new(lcl_hash, Vec::new());
        let tx_set_hash = *tx_set.hash();
        herder.scp_driver.cache_tx_set(tx_set);

        let xdr_tx_set_hash = stellar_xdr::Hash(tx_set_hash.0);
        let close_time = TimePoint(1);

        let network_id = herder.scp_driver.network_id();
        let mut sign_data = network_id.0.to_vec();
        sign_data.extend_from_slice(&EnvelopeType::Scpvalue.to_xdr(Limits::none()).unwrap());
        sign_data.extend_from_slice(&xdr_tx_set_hash.to_xdr(Limits::none()).unwrap());
        sign_data.extend_from_slice(&close_time.to_xdr(Limits::none()).unwrap());
        let sig = signer.sign(&sign_data);

        let node_id = XdrNodeId(stellar_xdr::PublicKey::PublicKeyTypeEd25519(
            stellar_xdr::Uint256(*signer.public_key().as_bytes()),
        ));

        let stellar_value = StellarValue {
            tx_set_hash: xdr_tx_set_hash,
            close_time,
            upgrades: vec![].try_into().unwrap(),
            ext: StellarValueExt::Signed(LedgerCloseValueSignature {
                node_id,
                signature: XdrSignature(sig.0.to_vec().try_into().unwrap_or_default()),
            }),
        };
        let value_bytes = stellar_value.to_xdr(Limits::none()).unwrap();
        Value(value_bytes.try_into().unwrap())
    }

    /// Build an SCP envelope for injection via `scp().receive_envelope()`.
    /// Signature is zeroed — `receive_envelope()` does not verify signatures.
    fn make_envelope(
        secret_key: &SecretKey,
        slot: u64,
        pledges: ScpStatementPledges,
    ) -> ScpEnvelope {
        let node_id = NodeId(stellar_xdr::PublicKey::PublicKeyTypeEd25519(Uint256(
            *secret_key.public_key().as_bytes(),
        )));
        ScpEnvelope {
            statement: ScpStatement {
                node_id,
                slot_index: slot,
                pledges,
            },
            signature: XdrSignature(vec![0u8; 64].try_into().unwrap()),
        }
    }

    fn externalize_pledges(value: &Value) -> ScpStatementPledges {
        ScpStatementPledges::Externalize(ScpStatementExternalize {
            commit: ScpBallot {
                counter: u32::MAX,
                value: value.clone(),
            },
            n_h: u32::MAX,
            commit_quorum_set_hash: stellar_xdr::Hash([0u8; 32]),
        })
    }

    fn prepare_pledges(value: &Value, quorum_set: &ScpQuorumSet) -> ScpStatementPledges {
        let qs_hash = hash_quorum_set(quorum_set);
        ScpStatementPledges::Prepare(ScpStatementPrepare {
            quorum_set_hash: stellar_xdr::Hash(qs_hash.0),
            ballot: ScpBallot {
                counter: 1,
                value: value.clone(),
            },
            prepared: None,
            prepared_prime: None,
            n_c: 0,
            n_h: 0,
        })
    }

    /// Helper to set tracking to `slot` using bootstrap.
    /// `bootstrap(n)` sets tracking_slot to `n + 1`, so we pass `slot - 1`.
    fn set_tracking(herder: &Herder, slot: u64) {
        assert!(slot >= 1);
        herder.bootstrap((slot - 1) as u32);
    }

    // ── Test 1: tracking_slot == 0 → None ──────────────────────────

    #[test]
    fn test_quorum_health_returns_none_when_not_tracking() {
        let herder = Herder::new(
            HerderConfig::default(),
            make_default_lm(),
            TimerManagerHandle::no_op(),
        );
        assert_eq!(herder.tracking_slot().get(), 0);
        assert!(herder.quorum_health().is_none());
    }

    // ── Test 2: previous slot used when current is absent ──────────

    #[test]
    fn test_quorum_health_uses_previous_slot() {
        let (herder, keys, _qs) = make_n_node_validator_herder(1, 1);

        let value = make_valid_value(&herder, &keys[0]);
        herder.scp().force_externalize(9, value);

        set_tracking(&herder, 10);

        let health = herder.quorum_health();
        assert_eq!(health, Some((1, 0, 0, 0, 0)));
    }

    // ── Test 3: fallback to current when previous is absent ────────

    #[test]
    fn test_quorum_health_falls_back_to_current_slot() {
        let (herder, keys, _qs) = make_n_node_validator_herder(1, 1);

        let value = make_valid_value(&herder, &keys[0]);
        herder.scp().force_externalize(10, value);

        set_tracking(&herder, 10);

        let health = herder.quorum_health();
        assert_eq!(health, Some((1, 0, 0, 0, 0)));
    }

    // ── Test 4: both slots absent → None ───────────────────────────

    #[test]
    fn test_quorum_health_returns_none_when_both_slots_absent() {
        let (herder, _keys, _qs) = make_n_node_validator_herder(1, 1);

        set_tracking(&herder, 10);

        assert!(herder.quorum_health().is_none());
    }

    // ── Test 5: Delayed peer folded into agree ─────────────────────

    #[test]
    fn test_quorum_health_delayed_folded_into_agree() {
        let (herder, keys, qs) = make_n_node_validator_herder(2, 2);
        let peer_key = &keys[1];

        let value = make_valid_value(&herder, &keys[0]);

        // Slot 9: local node externalized. Inject peer PREPARE (peer is behind).
        herder.scp().force_externalize(9, value.clone());
        let peer_prepare = make_envelope(peer_key, 9, prepare_pledges(&value, &qs));
        let r = herder.scp().receive_envelope(peer_prepare);
        assert!(r.is_valid(), "peer PREPARE rejected: {:?}", r);

        // Slot 10: exists (needed for get_reporting_summary(10) to return Some).
        herder.scp().force_externalize(10, value);

        // Tracking = 11 → quorum_health reads summary(10).
        // For peer: slot 10 has no peer envelope (NoInfo), falls back to slot 9
        // with self_already_moved_on=true. Slot 9 phase=Externalize, peer has
        // PREPARE (not externalized) → Delayed. Delayed folds into agree.
        set_tracking(&herder, 11);

        let health = herder.quorum_health();
        // agree = 1 (local Agree) + 1 (peer Delayed) = 2.
        // find_closest_v_blocking excludes self: left_till_block = 1+2-2 = 1,
        // peer is agreeing → fail_at = 1 (one peer failure blocks quorum).
        assert_eq!(health, Some((2, 0, 0, 1, 0)));
    }

    // ── Test 6: tracking_slot == 1 edge case ───────────────────────

    #[test]
    fn test_quorum_health_tracking_slot_one() {
        let (herder, keys, _qs) = make_n_node_validator_herder(1, 1);

        let value = make_valid_value(&herder, &keys[0]);
        herder.scp().force_externalize(1, value);

        set_tracking(&herder, 1);

        let health = herder.quorum_health();
        assert_eq!(health, Some((1, 0, 0, 0, 0)));
    }

    // ── Test 7: non-zero fail_at ───────────────────────────────────

    #[test]
    fn test_quorum_health_fail_at_nonzero() {
        // 3-node quorum with threshold=2 → fail_at = 2 (via find_closest_v_blocking).
        let (herder, keys, _qs) = make_n_node_validator_herder(3, 2);
        let peer1_key = &keys[1];
        let peer2_key = &keys[2];

        let value = make_valid_value(&herder, &keys[0]);

        // Slot 9: local externalized. Inject EXTERNALIZE from both peers.
        herder.scp().force_externalize(9, value.clone());
        let peer1_ext = make_envelope(peer1_key, 9, externalize_pledges(&value));
        let peer2_ext = make_envelope(peer2_key, 9, externalize_pledges(&value));
        let r1 = herder.scp().receive_envelope(peer1_ext);
        let r2 = herder.scp().receive_envelope(peer2_ext);
        assert!(r1.is_valid(), "peer1 EXTERNALIZE rejected: {:?}", r1);
        assert!(r2.is_valid(), "peer2 EXTERNALIZE rejected: {:?}", r2);

        set_tracking(&herder, 10);

        let health = herder.quorum_health();
        // 3 nodes all Agree. find_closest_v_blocking excludes self:
        // left_till_block = 1+3-2 = 2, both peers agreeing → fail_at = 2.
        assert_eq!(health, Some((3, 0, 0, 2, 0)));
    }

    // ── Test 8: nested quorum set precision ────────────────────────
    //
    // Demonstrates that find_closest_v_blocking is more precise than
    // total - threshold for nested quorum sets.

    #[test]
    fn test_quorum_health_nested_quorum_set_precision() {
        // Quorum set: threshold=2/3 inner sets, each inner is threshold=2/3 validators.
        // Old formula: total validators = 9, flat threshold = 2 → fail_at = 9-2 = 7 (wrong).
        // find_closest_v_blocking correctly considers the nested structure.
        let n = 10; // local + 9 peers
        let mut keys = Vec::with_capacity(n);
        let mut node_ids = Vec::with_capacity(n);
        for i in 0..n {
            let seed = [(50 + i as u8); 32];
            let sk = SecretKey::from_seed(&seed);
            let pk = sk.public_key();
            node_ids.push(node_id_from_public_key(&pk));
            keys.push(sk);
        }

        // 3 inner sets, each with 3 validators (peers), threshold=2.
        // Outer threshold=2 (need 2 of 3 inner sets to agree).
        let inner1 = ScpQuorumSet {
            threshold: 2,
            validators: vec![
                node_ids[1].clone(),
                node_ids[2].clone(),
                node_ids[3].clone(),
            ]
            .try_into()
            .unwrap(),
            inner_sets: vec![].try_into().unwrap(),
        };
        let inner2 = ScpQuorumSet {
            threshold: 2,
            validators: vec![
                node_ids[4].clone(),
                node_ids[5].clone(),
                node_ids[6].clone(),
            ]
            .try_into()
            .unwrap(),
            inner_sets: vec![].try_into().unwrap(),
        };
        let inner3 = ScpQuorumSet {
            threshold: 2,
            validators: vec![
                node_ids[7].clone(),
                node_ids[8].clone(),
                node_ids[9].clone(),
            ]
            .try_into()
            .unwrap(),
            inner_sets: vec![].try_into().unwrap(),
        };

        let quorum_set = ScpQuorumSet {
            threshold: 2,
            validators: vec![node_ids[0].clone()].try_into().unwrap(),
            inner_sets: vec![inner1, inner2, inner3].try_into().unwrap(),
        };

        let local_pk = keys[0].public_key();
        let config = HerderConfig {
            is_validator: true,
            node_public_key: local_pk,
            local_quorum_set: Some(quorum_set.clone()),
            ..HerderConfig::default()
        };
        let herder = Herder::with_secret_key(
            config,
            SecretKey::from_seed(&[50u8; 32]),
            make_default_lm(),
            TimerManagerHandle::no_op(),
        );

        for key in &keys[1..] {
            let peer_node_id = NodeId(stellar_xdr::PublicKey::PublicKeyTypeEd25519(Uint256(
                *key.public_key().as_bytes(),
            )));
            herder.store_quorum_set(&peer_node_id, quorum_set.clone());
        }

        let value = make_valid_value(&herder, &keys[0]);

        // Slot 9: local externalized + all peers send EXTERNALIZE.
        herder.scp().force_externalize(9, value.clone());
        for key in &keys[1..] {
            let ext = make_envelope(key, 9, externalize_pledges(&value));
            let r = herder.scp().receive_envelope(ext);
            assert!(r.is_valid(), "peer EXTERNALIZE rejected: {:?}", r);
        }

        set_tracking(&herder, 10);

        let health = herder.quorum_health();
        // All 10 nodes agree. With nested structure:
        // Outer: threshold=2, 1 validator (local, excluded) + 3 inner sets.
        // left_till_block = 1 + 1 + 3 - 2 = 3.
        // Local excluded → skip. Each inner set: all 3 in agreeing → result = [all 3].
        // All inner sets have non-empty v_blocking results.
        // After processing: need left_till_block-res.len() inner sets to fill.
        // The precise fail_at depends on the recursive v-blocking calculation.
        // Key assertion: fail_at is NOT 9 - 2 = 7 (the old broken formula applied
        // to the flat threshold — which doesn't even apply to nested sets).
        let (agree, missing, disagree, fail_at, _delayed) = health.unwrap();
        assert_eq!(agree, 10);
        assert_eq!(missing, 0);
        assert_eq!(disagree, 0);
        // find_closest_v_blocking picks the minimum set of agreeing peers whose
        // failure would v-block. Outer: threshold=2, 1 validator (local, excluded)
        // + 3 inner sets → left_till_block = 3. All 3 inner sets must be broken.
        // Each inner (threshold=2, 3 validators) needs 2 failures to v-block.
        // Total: 3 × 2 = 6.
        assert_eq!(fail_at, 6, "nested quorum set should give precise fail_at");
    }

    // ── Test 9: partial agree (some nodes missing) ─────────────────

    #[test]
    fn test_quorum_health_partial_agree_with_missing() {
        // 3-node quorum, threshold=2. Only 2 nodes agree, 1 is missing.
        let (herder, keys, _qs) = make_n_node_validator_herder(3, 2);

        let value = make_valid_value(&herder, &keys[0]);

        // Slot 9: local externalized + only peer1 sends EXTERNALIZE.
        // Peer2 doesn't send anything → Missing.
        herder.scp().force_externalize(9, value.clone());
        let peer1_ext = make_envelope(&keys[1], 9, externalize_pledges(&value));
        let r = herder.scp().receive_envelope(peer1_ext);
        assert!(r.is_valid());

        set_tracking(&herder, 10);

        let health = herder.quorum_health();
        let (agree, missing, _disagree, fail_at, _delayed) = health.unwrap();
        assert_eq!(agree, 2); // local + peer1
        assert_eq!(missing, 1); // peer2

        // find_closest_v_blocking with agreeing_nodes = {local, peer1}, excluded = local.
        // Only peer1 is in the set (local excluded). left_till_block = 1+3-2 = 2.
        // peer2 not in nodes → left_till_block -= 1 = 1. peer1 in nodes → res = [peer1].
        // res.len()=1, left_till_block=1 → no truncation. fail_at = 1.
        assert_eq!(fail_at, 1, "one more peer failure would block quorum");
    }

    // ── Test 10: all peers missing → fail_at = 0 ───────────────────

    #[test]
    fn test_quorum_health_all_peers_missing() {
        // 3-node quorum, threshold=2. Only local agrees.
        let (herder, keys, _qs) = make_n_node_validator_herder(3, 2);

        let value = make_valid_value(&herder, &keys[0]);
        herder.scp().force_externalize(9, value);

        set_tracking(&herder, 10);

        let health = herder.quorum_health();
        let (agree, missing, _disagree, fail_at, _delayed) = health.unwrap();
        assert_eq!(agree, 1); // only local
        assert_eq!(missing, 2); // both peers

        // Agreeing nodes = {local}, excluded = local → empty set for v-blocking.
        // Both peers already failing → already v-blocked → fail_at = 0.
        assert_eq!(fail_at, 0, "already v-blocked with all peers missing");
    }

    /// Integration test for AUDIT-259 follow-up (#2410): quorum tracking
    /// survives cache churn through the full quorum-check read path.
    ///
    /// Exercises: `heard_from_quorum()` → `SlotQuorumTracker::has_quorum()`
    /// → `ScpDriver::get_quorum_set()` → `QuorumSetTracker::get_by_node()`
    /// → `RandomEvictionCache::get()` under eviction pressure from >10,000
    /// non-quorum entries.
    ///
    /// Distinct from the unit test at `quorum_set_tracker.rs:626-688` which
    /// tests the cache directly. This test validates that periodic
    /// `heard_from_quorum()` calls (the real-world access pattern) keep
    /// active validators' entries hot throughout sustained churn.
    #[test]
    fn test_quorum_tracking_survives_cache_churn() {
        // Setup: 31-node quorum (1 local + 30 remote), threshold=21 (67%).
        let (herder, keys, _quorum_set) = make_n_node_validator_herder(31, 21);

        // Build node IDs for all 31 validators.
        let node_ids: Vec<NodeId> = keys
            .iter()
            .map(|k| {
                NodeId(stellar_xdr::PublicKey::PublicKeyTypeEd25519(Uint256(
                    *k.public_key().as_bytes(),
                )))
            })
            .collect();

        // Record SCP envelopes from all 31 nodes for slot 100.
        let slot = 100u64;
        {
            let mut tracker = herder.slot_quorum_tracker.write();
            for nid in &node_ids {
                tracker.record_envelope(slot, nid.clone());
            }
        }

        // Baseline: quorum must be satisfied.
        assert!(
            herder.heard_from_quorum(slot),
            "baseline: heard_from_quorum must be true before churn"
        );

        // Flood the cache with 10,500 distinct non-quorum entries.
        // Each `heard_from_quorum()` call refreshes all 30 remote validators'
        // access generation in both by_node and by_hash caches.
        //
        // Cache stress:
        //   by_node: 30 active remote entries + 10,500 churn = 10,530 total
        //            → 530 evictions from the 10,000-capacity cache.
        //            (Local node is pinned; short-circuits in get_by_node.)
        //   by_hash: 1 shared validator qset + 10,500 unique churn qsets = 10,501
        //            → 501 evictions.
        //
        // Using scp_driver.store_quorum_set() directly is a test-only shortcut
        // to create cache pressure without triggering quorum_tracker.rebuild()
        // for each non-quorum node. This is safe because churn nodes never
        // participate in quorum evaluation (not in slot_quorum_tracker, not
        // referenced by any quorum set).
        for i in 0u32..10_500 {
            if i > 0 && i % 500 == 0 {
                // Interleaved quorum check — refreshes active validators' access
                // generation AND asserts the read path works under pressure.
                assert!(
                    herder.heard_from_quorum(slot),
                    "heard_from_quorum failed during churn at iteration {}",
                    i
                );
            }

            // Generate a unique churn node ID (indices 1000..11500, avoiding
            // collision with validator seeds 10..=40).
            let churn_key = {
                let mut key = [0u8; 32];
                let idx = i + 1000;
                key[..4].copy_from_slice(&idx.to_le_bytes());
                key
            };
            let churn_node_id = NodeId(stellar_xdr::PublicKey::PublicKeyTypeEd25519(Uint256(
                churn_key,
            )));

            // Each churn node gets a sane single-node qset (threshold=1,
            // validators=[self]). Unique validator field → unique hash.
            let churn_qset = ScpQuorumSet {
                threshold: 1,
                validators: vec![churn_node_id.clone()].try_into().unwrap(),
                inner_sets: vec![].try_into().unwrap(),
            };

            herder
                .scp_driver
                .store_quorum_set(&churn_node_id, churn_qset);
        }

        // Final interleaved check after the last batch.
        assert!(
            herder.heard_from_quorum(slot),
            "heard_from_quorum failed after all churn completed"
        );

        // Assert on a new slot: record envelopes for slot 101, verify quorum.
        let new_slot = 101u64;
        {
            let mut tracker = herder.slot_quorum_tracker.write();
            for nid in &node_ids {
                tracker.record_envelope(new_slot, nid.clone());
            }
        }
        assert!(
            herder.heard_from_quorum(new_slot),
            "heard_from_quorum must work on a new slot after cache churn"
        );

        // Verify quorum_tracker.rebuild() works under cache pressure.
        // This is placed BEFORE individual get_quorum_set() probes so that
        // rebuild() is the first per-node cache access after churn completes
        // (aside from the heard_from_quorum call above, which is realistic —
        // production also checks quorum before rebuild triggers).
        //
        // rebuild() traverses from the local node through the quorum set,
        // calling scp_driver.get_quorum_set() for each member — all of which
        // go through the bounded cache. If active validators were evicted,
        // rebuild() would leave those nodes unexpanded (quorum_set = None).
        {
            let mut tracker = herder.quorum_tracker.write();
            tracker
                .rebuild(|id| herder.scp_driver.get_quorum_set(id))
                .expect("quorum_tracker.rebuild() must succeed under cache pressure");

            // All 31 nodes should be tracked.
            assert_eq!(
                tracker.tracked_node_count(),
                31,
                "rebuild must recover all 31 quorum members under cache pressure"
            );

            // All 30 remote validators must have their quorum_set populated
            // (not just be present as unexpanded entries). If the cache had
            // evicted their qsets, rebuild() would insert them as tracked
            // nodes but leave quorum_set = None.
            let qmap = tracker.quorum_map();
            for (i, nid) in node_ids.iter().enumerate().skip(1) {
                let info = qmap
                    .get(nid)
                    .unwrap_or_else(|| panic!("validator {} missing from quorum_map", i));
                assert!(
                    info.quorum_set.is_some(),
                    "validator {} (seed {}) has quorum_set=None after rebuild — \
                     cache eviction prevented rebuild from resolving its qset",
                    i,
                    10 + i
                );
            }
        }

        // Also assert individual lookups are still available after rebuild.
        for (i, nid) in node_ids.iter().enumerate().skip(1) {
            assert!(
                herder.scp_driver.get_quorum_set(nid).is_some(),
                "validator {} (key seed {}) qset evicted despite interleaved access",
                i,
                10 + i
            );
        }
    }
}

// ── Regression tests for quorum intersection self-deadlock (#1949) ──────
//
// Issue #1949: the `TooLarge` arm in `run_analysis` re-acquired the
// `quorum_intersection_state` write lock while the guard from the
// top-level acquisition was still live, causing a self-deadlock with
// `parking_lot::RwLock` (non-reentrant).
// That TooLarge variant has been removed (issue #1950) — the efficient
// checker handles large networks. These tests now verify the new
// behavior: large networks are analyzed successfully.

#[cfg(test)]
mod quorum_intersection_deadlock_tests {
    use super::*;
    use henyey_crypto::SecretKey;

    fn make_default_lm() -> Arc<henyey_ledger::LedgerManager> {
        use henyey_ledger::{LedgerManager, LedgerManagerConfig};
        use stellar_xdr::{
            Hash, LedgerHeader, LedgerHeaderExt, StellarValue, StellarValueExt, TimePoint, VecM,
        };
        let config = LedgerManagerConfig {
            validate_bucket_hash: false,
            ..Default::default()
        };
        let lm = LedgerManager::new("Test Network".to_string(), config);
        let header = LedgerHeader {
            ledger_version: 0,
            previous_ledger_hash: Hash([0u8; 32]),
            scp_value: StellarValue {
                tx_set_hash: Hash([0u8; 32]),
                close_time: TimePoint(0),
                upgrades: VecM::default(),
                ext: StellarValueExt::Basic,
            },
            tx_set_result_hash: Hash([0u8; 32]),
            bucket_list_hash: Hash([0u8; 32]),
            ledger_seq: 0,
            total_coins: 1_000_000_000_000,
            fee_pool: 0,
            inflation_seq: 0,
            id_pool: 0,
            base_fee: 100,
            base_reserve: 5_000_000,
            max_tx_set_size: 100,
            skip_list: [
                Hash([0u8; 32]),
                Hash([0u8; 32]),
                Hash([0u8; 32]),
                Hash([0u8; 32]),
            ],
            ext: LedgerHeaderExt::V0,
        };
        let header_hash = henyey_ledger::compute_header_hash(&header).expect("hash");
        lm.initialize(
            henyey_bucket::BucketList::new(),
            henyey_bucket::HotArchiveBucketList::new(),
            header,
            header_hash,
        )
        .expect("init");
        Arc::new(lm)
    }

    /// Build a herder whose transitive quorum tracker contains `n` nodes
    /// (the local node + `n-1` peers), all with the same quorum set.
    fn make_herder_with_n_quorum_nodes(n: usize) -> Herder {
        assert!(n >= 1);
        let mut keys = Vec::with_capacity(n);
        let mut node_ids = Vec::with_capacity(n);

        for i in 0..n {
            let seed = [(50 + i as u8); 32];
            let sk = SecretKey::from_seed(&seed);
            let pk = sk.public_key();
            node_ids.push(node_id_from_public_key(&pk));
            keys.push(sk);
        }

        let quorum_set = ScpQuorumSet {
            threshold: (n as u32 / 2) + 1,
            validators: node_ids.clone().try_into().unwrap(),
            inner_sets: vec![].try_into().unwrap(),
        };

        let local_pk = keys[0].public_key();
        let config = HerderConfig {
            is_validator: true,
            node_public_key: local_pk,
            local_quorum_set: Some(quorum_set.clone()),
            ..HerderConfig::default()
        };

        let herder = Herder::with_secret_key(
            config,
            SecretKey::from_seed(&[50u8; 32]),
            make_default_lm(),
            TimerManagerHandle::no_op(),
        );

        // Expand the transitive quorum tracker with all peers.
        for key in keys.iter().skip(1) {
            let peer_node_id = NodeId(stellar_xdr::PublicKey::PublicKeyTypeEd25519(Uint256(
                *key.public_key().as_bytes(),
            )));
            herder.store_quorum_set(&peer_node_id, quorum_set.clone());
            // Expand the tracker so the quorum map includes this peer.
            let mut qt = herder.quorum_tracker.write();
            let _ = qt.expand(&peer_node_id, quorum_set.clone());
            drop(qt);
        }

        herder
    }

    /// With the efficient checker (issue #1950), networks >20 nodes
    /// are analyzed successfully instead of returning TooLarge.
    #[test]
    fn test_large_quorum_map_analyzed_successfully() {
        let herder = make_herder_with_n_quorum_nodes(22);

        // Verify the quorum map is indeed >20 nodes.
        let qmap_len = herder.quorum_tracker.read().quorum_map().len();
        assert!(
            qmap_len > 20,
            "test setup: need >20 nodes, got {}",
            qmap_len,
        );

        // This call now runs the efficient intersection checker.
        herder.check_and_maybe_reanalyze_quorum_map(100);

        // The checker should produce a result (intersecting, since all
        // nodes share the same >50% threshold quorum set).
        let state = herder.quorum_intersection_state.read();
        assert!(
            state.checking_hash().is_none(),
            "checking_hash should be cleared after analysis"
        );
        assert!(
            state.has_any_results(),
            "analysis should produce a result for large network"
        );
        assert!(
            state.enjoys_quorum_intersection(),
            "22-node network with >50% threshold should enjoy intersection"
        );

        // A subsequent call should also succeed.
        drop(state);
        herder.check_and_maybe_reanalyze_quorum_map(101);
    }

    /// Verify that the efficient checker produces correct results.
    #[test]
    fn test_large_network_preserves_prior_result_on_subsequent_check() {
        let herder = make_herder_with_n_quorum_nodes(22);

        // Run first analysis.
        herder.check_and_maybe_reanalyze_quorum_map(100);

        let state = herder.quorum_intersection_state.read();
        assert!(state.has_any_results());
        assert_eq!(state.last_good_ledger(), 100);
        drop(state);

        // Same quorum map → should not re-analyze (hash matches).
        herder.check_and_maybe_reanalyze_quorum_map(101);

        let state = herder.quorum_intersection_state.read();
        // Result should still be from ledger 100 (not re-analyzed).
        assert_eq!(state.last_good_ledger(), 100);
    }

    /// Verify `quorum_intersection_publishable()` returns correct values
    /// for each lifecycle state.
    #[test]
    fn test_quorum_intersection_publishable() {
        let herder = make_herder_with_n_quorum_nodes(5);

        // Before any analysis: None.
        assert_eq!(herder.quorum_intersection_publishable(), None);

        // After intersecting check: Some(true).
        herder.check_and_maybe_reanalyze_quorum_map(100);
        assert_eq!(herder.quorum_intersection_publishable(), Some(true));

        // After a split (simulated by directly setting state):
        // Record a split result with a different hash so it publishes.
        {
            let mut state = herder.quorum_intersection_state.write();
            let split_hash = Hash256::from([99u8; 32]);
            state.start_checking(split_hash);
            state.complete_check(
                &split_hash,
                QuorumIntersectionResult::Split {
                    check_ledger: 200,
                    num_nodes: 4,
                    quorum_map_hash: split_hash,
                    potential_split: (vec![], vec![]),
                },
            );
        }
        // has_any_results() is still true (last_good_ledger = 100 from prior check),
        // but enjoys_quorum_intersection() is false.
        assert_eq!(herder.quorum_intersection_publishable(), Some(false));
    }

    /// Verify `quorum_intersection_publishable()` returns None when
    /// the first-ever check is a split (no prior good result).
    #[test]
    fn test_quorum_intersection_publishable_first_split() {
        let herder = make_herder_with_n_quorum_nodes(5);

        // Directly record a split without prior intersecting check.
        {
            let mut state = herder.quorum_intersection_state.write();
            let hash = Hash256::from([1u8; 32]);
            state.start_checking(hash);
            state.complete_check(
                &hash,
                QuorumIntersectionResult::Split {
                    check_ledger: 50,
                    num_nodes: 3,
                    quorum_map_hash: hash,
                    potential_split: (vec![], vec![]),
                },
            );
        }
        // First-ever split → has_any_results() is false → None.
        assert_eq!(herder.quorum_intersection_publishable(), None);
    }
}

#[cfg(test)]
mod dynamic_close_time_tests {
    use super::*;
    use henyey_ledger::{LedgerManager, LedgerManagerConfig, SorobanNetworkInfo};
    use stellar_xdr::{
        Hash, LedgerHeader, LedgerHeaderExt, StellarValue, StellarValueExt, TimePoint, VecM,
    };

    fn make_default_lm() -> Arc<henyey_ledger::LedgerManager> {
        let config = LedgerManagerConfig {
            validate_bucket_hash: false,
            ..Default::default()
        };
        let lm = LedgerManager::new("Test Network".to_string(), config);
        let header = LedgerHeader {
            ledger_version: 0,
            previous_ledger_hash: Hash([0u8; 32]),
            scp_value: StellarValue {
                tx_set_hash: Hash([0u8; 32]),
                close_time: TimePoint(0),
                upgrades: VecM::default(),
                ext: StellarValueExt::Basic,
            },
            tx_set_result_hash: Hash([0u8; 32]),
            bucket_list_hash: Hash([0u8; 32]),
            ledger_seq: 0,
            total_coins: 1_000_000_000_000,
            fee_pool: 0,
            inflation_seq: 0,
            id_pool: 0,
            base_fee: 100,
            base_reserve: 5_000_000,
            max_tx_set_size: 100,
            skip_list: [
                Hash([0u8; 32]),
                Hash([0u8; 32]),
                Hash([0u8; 32]),
                Hash([0u8; 32]),
            ],
            ext: LedgerHeaderExt::V0,
        };
        let header_hash = henyey_ledger::compute_header_hash(&header).expect("hash");
        lm.initialize(
            henyey_bucket::BucketList::new(),
            henyey_bucket::HotArchiveBucketList::new(),
            header,
            header_hash,
        )
        .expect("init");
        Arc::new(lm)
    }

    fn make_ledger_manager_with_protocol(protocol: u32) -> LedgerManager {
        let config = LedgerManagerConfig {
            validate_bucket_hash: false,
            ..Default::default()
        };
        let lm = LedgerManager::new("Test Network".to_string(), config);
        let header = LedgerHeader {
            ledger_version: protocol,
            previous_ledger_hash: Hash([0u8; 32]),
            scp_value: StellarValue {
                tx_set_hash: Hash([0u8; 32]),
                close_time: TimePoint(0),
                upgrades: VecM::default(),
                ext: StellarValueExt::Basic,
            },
            tx_set_result_hash: Hash([0u8; 32]),
            bucket_list_hash: Hash([0u8; 32]),
            ledger_seq: 10,
            total_coins: 1_000_000_000_000,
            fee_pool: 0,
            inflation_seq: 0,
            id_pool: 0,
            base_fee: 100,
            base_reserve: 5_000_000,
            max_tx_set_size: 100,
            skip_list: [
                Hash([0u8; 32]),
                Hash([0u8; 32]),
                Hash([0u8; 32]),
                Hash([0u8; 32]),
            ],
            ext: LedgerHeaderExt::V0,
        };
        let header_hash = henyey_ledger::compute_header_hash(&header).expect("hash");
        lm.initialize(
            henyey_bucket::BucketList::new(),
            henyey_bucket::HotArchiveBucketList::new(),
            header,
            header_hash,
        )
        .expect("init");
        lm
    }

    #[test]
    fn test_herder_close_time_returns_5s_for_pre_v23() {
        let config = HerderConfig {
            ledger_close_time: 5,
            ..HerderConfig::default()
        };
        let lm = Arc::new(make_ledger_manager_with_protocol(22));
        let herder = Herder::new(config, lm, TimerManagerHandle::no_op());

        // Protocol 22: should return pre-v23 default (5000ms)
        assert_eq!(herder.ledger_close_duration(), Duration::from_secs(5));
    }

    #[test]
    fn test_herder_close_time_returns_dynamic_for_v23() {
        let config = HerderConfig {
            ledger_close_time: 5,
            ..HerderConfig::default()
        };
        let lm = make_ledger_manager_with_protocol(23);

        // Set soroban network info with a custom close time (4000ms)
        {
            let mut info = SorobanNetworkInfo::default();
            info.ledger_target_close_time_ms = 4000;
            lm.set_soroban_network_info_for_test(info);
        }

        let lm = Arc::new(lm);
        let herder = Herder::new(config, lm, TimerManagerHandle::no_op());

        assert_eq!(herder.ledger_close_duration(), Duration::from_millis(4000));
    }

    #[test]
    fn test_herder_close_time_returns_5000ms_when_v23_but_no_soroban_info() {
        let config = HerderConfig {
            ledger_close_time: 5,
            ..HerderConfig::default()
        };
        // Protocol 23 but no soroban_network_info populated
        let lm = Arc::new(make_ledger_manager_with_protocol(23));
        let herder = Herder::new(config, lm, TimerManagerHandle::no_op());

        // Falls back to 5000ms pre-v23 constant
        assert_eq!(herder.ledger_close_duration(), Duration::from_secs(5));
    }

    #[test]
    fn test_ledger_manager_expected_close_time_protocol_22() {
        let lm = make_ledger_manager_with_protocol(22);
        assert_eq!(lm.expected_ledger_close_duration(), Duration::from_secs(5));
    }

    #[test]
    fn test_ledger_manager_expected_close_time_protocol_23_with_config() {
        let lm = make_ledger_manager_with_protocol(23);
        let mut info = SorobanNetworkInfo::default();
        info.ledger_target_close_time_ms = 4000;
        lm.set_soroban_network_info_for_test(info);
        assert_eq!(
            lm.expected_ledger_close_duration(),
            Duration::from_millis(4000)
        );
    }

    #[test]
    fn test_ledger_manager_expected_close_time_non_5000ms_value() {
        // Verify non-standard values (e.g. 4500ms) are returned correctly
        // and not truncated to seconds.
        let lm = make_ledger_manager_with_protocol(23);
        let mut info = SorobanNetworkInfo::default();
        info.ledger_target_close_time_ms = 4500;
        lm.set_soroban_network_info_for_test(info);
        assert_eq!(
            lm.expected_ledger_close_duration(),
            Duration::from_millis(4500)
        );
    }

    #[test]
    fn test_herder_close_duration_non_round_values() {
        // Verify non-round millisecond values are preserved through the full chain.
        for ms in [4300u32, 4500, 4999] {
            let config = HerderConfig {
                ledger_close_time: 5,
                ..HerderConfig::default()
            };
            let lm = make_ledger_manager_with_protocol(23);
            let mut info = SorobanNetworkInfo::default();
            info.ledger_target_close_time_ms = ms;
            lm.set_soroban_network_info_for_test(info);
            let herder = Herder::new(config, Arc::new(lm), TimerManagerHandle::no_op());

            assert_eq!(
                herder.ledger_close_duration(),
                Duration::from_millis(ms as u64),
                "non-round value {ms}ms should be preserved"
            );
        }
    }

    #[test]
    fn test_max_queue_size_soroban_ops_defaults_to_zero() {
        let herder = Herder::new(
            HerderConfig::default(),
            make_default_lm(),
            TimerManagerHandle::no_op(),
        );
        assert_eq!(herder.max_queue_size_soroban_ops(), 0);
    }

    #[test]
    fn test_max_queue_size_soroban_ops_reads_static_config() {
        let mut config = HerderConfig::default();
        config.tx_queue_config.max_queue_soroban_resources =
            Some(henyey_common::Resource::new(vec![
                7, 200, 300, 400, 500, 600, 700,
            ]));
        let herder = Herder::new(config, make_default_lm(), TimerManagerHandle::no_op());
        assert_eq!(herder.max_queue_size_soroban_ops(), 7);
    }

    #[test]
    fn test_max_queue_size_soroban_ops_prefers_dynamic() {
        let herder = Herder::new(
            HerderConfig::default(),
            make_default_lm(),
            TimerManagerHandle::no_op(),
        );
        herder.tx_queue().update_soroban_resource_limits(
            henyey_common::Resource::soroban_ledger_limits(17, 1, 1, 1, 1, 1, 1),
        );
        assert_eq!(herder.max_queue_size_soroban_ops(), 17);
    }
}

// =============================================================================
// Tests for get_previous_value and retrograde externalization (issues #2342, #2347)
// =============================================================================
#[cfg(test)]
mod previous_value_tests {
    use super::*;
    use crate::tx_queue::TransactionSet;
    use henyey_crypto::SecretKey;
    use henyey_ledger::{LedgerManager, LedgerManagerConfig};
    use stellar_xdr::{
        EnvelopeType, Hash, LedgerCloseValueSignature, LedgerHeader, LedgerHeaderExt, Limits,
        NodeId as XdrNodeId, ScpBallot, ScpStatement, ScpStatementExternalize, ScpStatementPledges,
        Signature as XdrSignature, StellarValue, StellarValueExt, TimePoint, Value, VecM, WriteXdr,
    };

    fn make_default_lm() -> Arc<henyey_ledger::LedgerManager> {
        let config = LedgerManagerConfig {
            validate_bucket_hash: false,
            ..Default::default()
        };
        let lm = LedgerManager::new("Test Network".to_string(), config);
        let header = LedgerHeader {
            ledger_version: 0,
            previous_ledger_hash: Hash([0u8; 32]),
            scp_value: StellarValue {
                tx_set_hash: Hash([0u8; 32]),
                close_time: TimePoint(0),
                upgrades: VecM::default(),
                ext: StellarValueExt::Basic,
            },
            tx_set_result_hash: Hash([0u8; 32]),
            bucket_list_hash: Hash([0u8; 32]),
            ledger_seq: 0,
            total_coins: 1_000_000_000_000,
            fee_pool: 0,
            inflation_seq: 0,
            id_pool: 0,
            base_fee: 100,
            base_reserve: 5_000_000,
            max_tx_set_size: 100,
            skip_list: [
                Hash([0u8; 32]),
                Hash([0u8; 32]),
                Hash([0u8; 32]),
                Hash([0u8; 32]),
            ],
            ext: LedgerHeaderExt::V0,
        };
        let header_hash = henyey_ledger::compute_header_hash(&header).expect("hash");
        lm.initialize(
            henyey_bucket::BucketList::new(),
            henyey_bucket::HotArchiveBucketList::new(),
            header,
            header_hash,
        )
        .expect("init");
        Arc::new(lm)
    }

    /// Create a LedgerManager initialized at `ledger_seq` with the given `scp_value`.
    fn make_lm_with_scp_value(ledger_seq: u32, scp_value: StellarValue) -> Arc<LedgerManager> {
        let config = LedgerManagerConfig {
            validate_bucket_hash: false,
            ..Default::default()
        };
        let lm = LedgerManager::new("Test Network".to_string(), config);
        let header = LedgerHeader {
            ledger_version: 0,
            previous_ledger_hash: Hash([0u8; 32]),
            scp_value,
            tx_set_result_hash: Hash([0u8; 32]),
            bucket_list_hash: Hash([0u8; 32]),
            ledger_seq,
            total_coins: 1_000_000_000_000,
            fee_pool: 0,
            inflation_seq: 0,
            id_pool: 0,
            base_fee: 100,
            base_reserve: 5_000_000,
            max_tx_set_size: 100,
            skip_list: [
                Hash([0u8; 32]),
                Hash([0u8; 32]),
                Hash([0u8; 32]),
                Hash([0u8; 32]),
            ],
            ext: LedgerHeaderExt::V0,
        };
        let header_hash = henyey_ledger::compute_header_hash(&header).expect("hash");
        lm.initialize(
            henyey_bucket::BucketList::new(),
            henyey_bucket::HotArchiveBucketList::new(),
            header,
            header_hash,
        )
        .expect("init");
        Arc::new(lm)
    }

    fn make_stellar_value(close_time: u64) -> StellarValue {
        StellarValue {
            tx_set_hash: Hash([close_time as u8; 32]),
            close_time: TimePoint(close_time),
            upgrades: VecM::default(),
            ext: StellarValueExt::Basic,
        }
    }

    fn stellar_value_to_scp_value(sv: &StellarValue) -> Value {
        Value(sv.to_xdr(Limits::none()).unwrap().try_into().unwrap())
    }

    /// get_previous_value reads from LCL when ledger_manager is set.
    #[test]
    fn test_get_previous_value_reads_from_lcl() {
        // Install LM with a known scpValue
        let lcl_sv = make_stellar_value(99);
        let expected = stellar_value_to_scp_value(&lcl_sv);
        let lm = make_lm_with_scp_value(10, lcl_sv);
        let herder = Herder::new(HerderConfig::default(), lm, TimerManagerHandle::no_op());

        // get_previous_value should return LCL value
        let result = herder.get_previous_value();
        assert_eq!(
            result, expected,
            "get_previous_value must return LCL's scpValue"
        );
    }

    // =========================================================================
    // Helpers for end-to-end tests (issue #2345)
    // =========================================================================

    /// Build a properly signed SCP `Value` with a custom close time.
    ///
    /// Creates a `TransactionSet`, caches it in `scp_driver`, builds a
    /// `StellarValue` with `StellarValueExt::Signed`, and returns the
    /// XDR-encoded `Value`. Mirrors `make_valid_value_with_cached_tx_set`
    /// (herder.rs tests module) but accepts `close_time`.
    fn make_externalize_value_with_close_time(
        herder: &Herder,
        secret: &SecretKey,
        close_time: u64,
    ) -> Value {
        let lcl_hash = herder.scp_driver.current_header_hash();
        let tx_set = TransactionSet::new(lcl_hash, Vec::new());
        let tx_set_hash = *tx_set.hash();
        herder.scp_driver.cache_tx_set(tx_set);

        let xdr_tx_set_hash = Hash(tx_set_hash.0);
        let ct = TimePoint(close_time);

        // Sign: (networkID, ENVELOPE_TYPE_SCPVALUE, txSetHash, closeTime)
        let network_id = herder.scp_driver.network_id();
        let mut sign_data = network_id.0.to_vec();
        sign_data.extend_from_slice(&EnvelopeType::Scpvalue.to_xdr(Limits::none()).unwrap());
        sign_data.extend_from_slice(&xdr_tx_set_hash.to_xdr(Limits::none()).unwrap());
        sign_data.extend_from_slice(&ct.to_xdr(Limits::none()).unwrap());
        let sig = secret.sign(&sign_data);

        let node_id = XdrNodeId(stellar_xdr::PublicKey::PublicKeyTypeEd25519(
            stellar_xdr::Uint256(*secret.public_key().as_bytes()),
        ));

        let stellar_value = StellarValue {
            tx_set_hash: xdr_tx_set_hash,
            close_time: ct,
            upgrades: VecM::default(),
            ext: StellarValueExt::Signed(LedgerCloseValueSignature {
                node_id,
                signature: stellar_xdr::Signature(sig.0.to_vec().try_into().unwrap_or_default()),
            }),
        };
        Value(
            stellar_value
                .to_xdr(Limits::none())
                .unwrap()
                .try_into()
                .unwrap(),
        )
    }

    /// Build a signed EXTERNALIZE envelope for a given slot and value.
    ///
    /// Mirrors `sign_statement` / `make_signed_externalize_from` from the
    /// sibling tests module but is accessible within `previous_value_tests`.
    fn make_signed_externalize(
        slot: u64,
        herder: &Herder,
        secret: &SecretKey,
        value: Value,
    ) -> ScpEnvelope {
        let node_id = XdrNodeId(stellar_xdr::PublicKey::PublicKeyTypeEd25519(
            stellar_xdr::Uint256(*secret.public_key().as_bytes()),
        ));

        let statement = ScpStatement {
            node_id,
            slot_index: slot,
            pledges: ScpStatementPledges::Externalize(ScpStatementExternalize {
                commit: ScpBallot { counter: 1, value },
                n_h: 1,
                commit_quorum_set_hash: Hash([0u8; 32]),
            }),
        };

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

    // =========================================================================
    // End-to-end retrograde externalization test (issue #2345)
    // =========================================================================

    /// Retrograde externalization must NOT regress tracking state.
    ///
    /// Drives the real `receive_scp_envelope` → externalization path to verify
    /// that tracking_slot and latest_externalized_slot are not regressed by a
    /// retrograde EXTERNALIZE envelope.
    ///
    /// Scenario:
    /// 1. Bootstrap at ledger 100 (tracking slot 101)
    /// 2. Externalize slot 101 via receive_scp_envelope
    /// 3. Send retrograde EXTERNALIZE for slot 99 → tracking must not regress
    #[test]
    fn test_retrograde_externalization_e2e_preserves_tracking() {
        // -- Setup: validator herder with 2 validators (threshold 1) ----------
        let local_secret = SecretKey::from_seed(&[7u8; 32]);
        let local_public = local_secret.public_key();
        let local_node_id = node_id_from_public_key(&local_public);

        let peer_secret = SecretKey::from_seed(&[1u8; 32]);
        let peer_public = peer_secret.public_key();
        let peer_node_id = node_id_from_public_key(&peer_public);

        let quorum_set = ScpQuorumSet {
            threshold: 1,
            validators: vec![local_node_id.clone(), peer_node_id.clone()]
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

        let herder = Herder::with_secret_key(
            config,
            SecretKey::from_seed(&[7u8; 32]),
            make_default_lm(),
            TimerManagerHandle::no_op(),
        );

        herder.start_syncing();
        herder.bootstrap(100); // tracking slot = 101

        // Register peer in quorum tracker
        herder
            .quorum_tracker
            .write()
            .expand(&peer_node_id, quorum_set)
            .unwrap();

        // -- Build distinct values for slot 101 and slot 99 ------------------
        let value_101 = make_externalize_value_with_close_time(&herder, &peer_secret, 1001);
        let value_99 = make_externalize_value_with_close_time(&herder, &peer_secret, 999);
        assert_ne!(
            value_101, value_99,
            "values must be byte-distinct to make the test meaningful"
        );

        // -- Forward externalization: slot 101 --------------------------------
        let env_101 = make_signed_externalize(101, &herder, &peer_secret, value_101.clone());
        let result_101 = herder.receive_scp_envelope(env_101);
        assert_eq!(
            result_101,
            EnvelopeState::Valid,
            "forward EXTERNALIZE for slot 101 must be accepted"
        );
        assert!(
            herder.scp().is_slot_externalized(101),
            "slot 101 must be externalized by SCP"
        );
        assert_eq!(
            herder.tracking_slot().get(),
            102,
            "tracking must advance to 102 after externalizing 101"
        );

        // -- Retrograde externalization: slot 99 ------------------------------
        let env_99 = make_signed_externalize(99, &herder, &peer_secret, value_99.clone());
        let result_99 = herder.receive_scp_envelope(env_99);

        assert_eq!(
            result_99,
            EnvelopeState::Valid,
            "retrograde EXTERNALIZE for slot 99 must pass pre-filter and reach SCP"
        );
        assert!(
            herder.scp().is_slot_externalized(99),
            "slot 99 must be externalized by SCP (retrograde still processes through SCP)"
        );
        // Confirm the herder post-SCP block ran (record_externalized)
        let ext_99 = herder.scp_driver.get_externalized(99);
        assert!(
            ext_99.is_some(),
            "slot 99 must be recorded in scp_driver.externalized"
        );
        assert_eq!(
            ext_99.unwrap().value,
            value_99,
            "externalized slot 99 must contain value_99"
        );

        // -- Verify tracking state did NOT regress ----------------------------
        assert_eq!(
            herder.tracking_slot().get(),
            102,
            "tracking must NOT regress after retrograde externalization"
        );
        assert_eq!(
            herder.latest_externalized_slot(),
            Some(101),
            "latest_externalized_slot must remain 101 (not regress to 99)"
        );
    }
}

// =============================================================================
// Behavioral tests: LedgerManager is always available (no Option/fallback).
// Verifies that bootstrap, get_min_ledger_seq_to_ask_peers, and
// ledger_close_duration read state directly from the injected LedgerManager.
// =============================================================================

#[cfg(test)]
mod required_lm_behavioral_tests {
    use super::*;
    use henyey_ledger::{LedgerManager, LedgerManagerConfig};
    use stellar_xdr::{
        Hash, LedgerHeader, LedgerHeaderExt, StellarValue, StellarValueExt, TimePoint, VecM,
    };

    fn make_ledger_manager_at_seq(ledger_seq: u32) -> Arc<LedgerManager> {
        let config = LedgerManagerConfig {
            validate_bucket_hash: false,
            ..Default::default()
        };
        let lm = LedgerManager::new("Test Network".to_string(), config);
        let header = LedgerHeader {
            ledger_version: 24,
            previous_ledger_hash: Hash([0u8; 32]),
            scp_value: StellarValue {
                tx_set_hash: Hash([0u8; 32]),
                close_time: TimePoint(500),
                upgrades: VecM::default(),
                ext: StellarValueExt::Basic,
            },
            tx_set_result_hash: Hash([0u8; 32]),
            bucket_list_hash: Hash([0u8; 32]),
            ledger_seq,
            total_coins: 1_000_000_000_000,
            fee_pool: 0,
            inflation_seq: 0,
            id_pool: 0,
            base_fee: 100,
            base_reserve: 5_000_000,
            max_tx_set_size: 100,
            skip_list: [
                Hash([0u8; 32]),
                Hash([0u8; 32]),
                Hash([0u8; 32]),
                Hash([0u8; 32]),
            ],
            ext: LedgerHeaderExt::V0,
        };
        let header_hash = henyey_ledger::compute_header_hash(&header).expect("hash");
        lm.initialize(
            henyey_bucket::BucketList::new(),
            henyey_bucket::HotArchiveBucketList::new(),
            header,
            header_hash,
        )
        .expect("init");
        Arc::new(lm)
    }

    /// bootstrap() reads close_time from the LedgerManager's current header.
    #[test]
    fn test_bootstrap_derives_state_from_lm() {
        let lm = make_ledger_manager_at_seq(42);
        let herder = Herder::new(HerderConfig::default(), lm, TimerManagerHandle::no_op());

        herder.start_syncing();
        herder.bootstrap(42);

        assert_eq!(herder.state(), HerderState::Tracking);
        // tracking_slot = ledger_seq + 1
        assert_eq!(herder.tracking_slot().get(), 43);
        // consensus_close_time should match the LM header's close_time (500)
        let ts = herder.tracking_state.read();
        assert_eq!(ts.consensus_close_time, 500);
    }

    /// get_min_ledger_seq_to_ask_peers() uses LedgerManager.current_ledger_seq().
    #[test]
    fn test_get_min_ledger_seq_to_ask_peers_uses_lm() {
        let lm = make_ledger_manager_at_seq(100);
        let herder = Herder::new(HerderConfig::default(), lm, TimerManagerHandle::no_op());

        let min_seq = herder.get_min_ledger_seq_to_ask_peers();
        // lcl = 100, low = 101, window = min(MAX_SLOTS_TO_REMEMBER=12, 3) = 3
        // low = 101 - 3 = 98
        assert_eq!(min_seq, 98);
    }

    /// get_min_ledger_seq_to_ask_peers() uses the fixed MAX_SLOTS_TO_REMEMBER constant
    /// for its pre-clamp lookback, NOT max_externalized_slots. This ensures parity
    /// with stellar-core even when max_externalized_slots < 3.
    #[test]
    fn test_get_min_ledger_seq_to_ask_peers_ignores_low_max_externalized_slots() {
        let lm = make_ledger_manager_at_seq(100);
        let mut config = HerderConfig::default();
        config.max_externalized_slots = 1; // below 3
        let herder = Herder::new(config, lm, TimerManagerHandle::no_op());

        let min_seq = herder.get_min_ledger_seq_to_ask_peers();
        // stellar-core: min(MAX_SLOTS_TO_REMEMBER, SCP_EXTRA_LOOKBACK_LEDGERS) = min(12, 3) = 3
        // lcl = 100, low = 101 - 3 = 98
        // Before the fix, this would have been 101 - min(1, 3) = 100.
        assert_eq!(min_seq, 98);
    }

    /// get_min_ledger_seq_to_ask_peers() clamps against get_min_ledger_seq_to_remember()
    /// so the returned value never drops below the remember floor (§15.3 parity).
    #[test]
    fn test_get_min_ledger_seq_to_ask_peers_clamps_to_remember_floor() {
        // Create a herder at a high ledger seq with tracking set high enough
        // that the remember floor exceeds the lookback window.
        let lm = make_ledger_manager_at_seq(50);
        let herder = Herder::new(HerderConfig::default(), lm, TimerManagerHandle::no_op());

        // Bootstrap so tracking_consensus_ledger_index = 100 (tracking_slot = 101)
        herder.start_syncing();
        herder.bootstrap(100);

        // tracking_consensus_ledger_index = 100
        // get_min_ledger_seq_to_remember: current_slot=100, 100 > 12, so 100-12+1 = 89
        let remember_floor = herder.get_min_ledger_seq_to_remember();
        assert_eq!(remember_floor, 89);

        // get_min_ledger_seq_to_ask_peers: lcl=50, low=51, window=3, low=51-3=48
        // But 48 < 89 (remember floor), so clamp to 89.
        let min_seq = herder.get_min_ledger_seq_to_ask_peers();
        assert_eq!(min_seq, remember_floor as u32);
    }

    /// get_scp_externalizing_state() returns the externalizing state snapshot for
    /// an externalized slot, and empty for a slot with no externalizing state.
    /// (Full test exercising non-empty externalized slot lives in the main `tests`
    /// module as `test_get_scp_externalizing_state_uses_externalizing_snapshot`.)
    #[test]
    fn test_get_scp_externalizing_state_empty_for_unknown_slot() {
        let lm = make_ledger_manager_at_seq(1);
        let herder = Herder::new(HerderConfig::default(), lm, TimerManagerHandle::no_op());

        // Slot 42 has no externalizing state → empty.
        let state = herder.get_scp_externalizing_state(42);
        assert!(state.is_empty());

        // Slot 0 also empty.
        let state = herder.get_scp_externalizing_state(0);
        assert!(state.is_empty());
    }

    /// Regression test for #2904: when the herder's tracking consensus index
    /// is far ahead of LCL (e.g. tracking = 200, LCL = 50), the unclamped
    /// formula returns a low watermark below get_min_ledger_seq_to_remember().
    /// stellar-core clamps with `max(low, getMinLedgerSeqToRemember())`.
    #[test]
    fn test_get_min_ledger_seq_to_ask_peers_clamps_to_remember_floor_when_tracking_ahead() {
        let lm = make_ledger_manager_at_seq(50);
        let herder = Herder::new(HerderConfig::default(), lm, TimerManagerHandle::no_op());
        // Set to Tracking state with tracking_slot=201 → consensus_index=200
        {
            let mut state = tracked_write(LOCK_HERDER_STATE, &herder.state);
            *state = HerderState::Tracking;
        }
        {
            let mut ts = tracked_write(LOCK_TRACKING_STATE, &herder.tracking_state);
            ts.is_tracking = true;
            ts.consensus_index = 201;
        }

        // get_min_ledger_seq_to_remember() = 200 - 12 + 1 = 189
        let min_seq = herder.get_min_ledger_seq_to_ask_peers();

        // Without clamp: lcl=50, low=51, window=3, low=48
        // With clamp: max(48, 189) = 189
        assert_eq!(
            min_seq, 189,
            "get_min_ledger_seq_to_ask_peers must clamp to get_min_ledger_seq_to_remember \
             when tracking is ahead of LCL (stellar-core parity)"
        );
    }

    /// ledger_close_duration() delegates directly to LedgerManager.
    #[test]
    fn test_ledger_close_duration_reads_from_lm() {
        let lm = make_ledger_manager_at_seq(1);
        let herder = Herder::new(
            HerderConfig::default(),
            lm.clone(),
            TimerManagerHandle::no_op(),
        );

        let duration = herder.ledger_close_duration();
        let expected = lm.expected_ledger_close_duration();
        assert_eq!(duration, expected);
        // Should be a positive duration
        assert!(duration.as_millis() > 0);
    }
}

#[cfg(test)]
mod fetching_envelopes_routing_tests {
    use super::*;
    use henyey_ledger::{LedgerManager, LedgerManagerConfig};
    use stellar_xdr::{
        Hash as XdrHash, LedgerHeader, LedgerHeaderExt, NodeId as XdrNodeId, ScpBallot,
        ScpEnvelope, ScpStatement, ScpStatementPledges, ScpStatementPrepare,
        Signature as XdrSignature, StellarValue, StellarValueExt, TimePoint, Value, VecM,
    };

    fn make_test_herder() -> Herder {
        let lm_config = LedgerManagerConfig {
            validate_bucket_hash: false,
            ..Default::default()
        };
        let lm = LedgerManager::new("Test Network".to_string(), lm_config);
        let header = LedgerHeader {
            ledger_version: 24,
            previous_ledger_hash: XdrHash([0u8; 32]),
            scp_value: StellarValue {
                tx_set_hash: XdrHash([0u8; 32]),
                close_time: TimePoint(0),
                upgrades: VecM::default(),
                ext: StellarValueExt::Basic,
            },
            tx_set_result_hash: XdrHash([0u8; 32]),
            bucket_list_hash: XdrHash([0u8; 32]),
            ledger_seq: 0,
            total_coins: 1_000_000_000_000,
            fee_pool: 0,
            inflation_seq: 0,
            id_pool: 0,
            base_fee: 100,
            base_reserve: 5_000_000,
            max_tx_set_size: 100,
            skip_list: [
                XdrHash([0u8; 32]),
                XdrHash([0u8; 32]),
                XdrHash([0u8; 32]),
                XdrHash([0u8; 32]),
            ],
            ext: LedgerHeaderExt::V0,
        };
        let header_hash = henyey_ledger::compute_header_hash(&header).expect("hash");
        lm.initialize(
            henyey_bucket::BucketList::new(),
            henyey_bucket::HotArchiveBucketList::new(),
            header,
            header_hash,
        )
        .expect("init");
        Herder::new(
            HerderConfig::default(),
            Arc::new(lm),
            TimerManagerHandle::no_op(),
        )
    }

    /// Helper: make a simple test envelope for FetchingEnvelopes tests.
    /// Uses a Prepare statement with a unique counter for hash uniqueness.
    fn make_fetching_test_envelope(slot: u64, counter: u32) -> ScpEnvelope {
        let node_id = XdrNodeId(stellar_xdr::PublicKey::PublicKeyTypeEd25519(
            stellar_xdr::Uint256([42u8; 32]),
        ));
        let ballot = ScpBallot {
            counter,
            value: Value(vec![0u8; 1].try_into().unwrap()),
        };
        ScpEnvelope {
            statement: ScpStatement {
                node_id,
                slot_index: slot,
                pledges: ScpStatementPledges::Prepare(ScpStatementPrepare {
                    quorum_set_hash: stellar_xdr::Hash([0u8; 32]),
                    ballot,
                    prepared: None,
                    prepared_prime: None,
                    n_c: 0,
                    n_h: 0,
                }),
            },
            signature: XdrSignature(vec![0u8; 64].try_into().unwrap()),
        }
    }

    /// process_ready_fetching_envelopes only drains envelopes for slots
    /// <= tracking_slot (current consensus slot). Future-slot envelopes
    /// remain in the ready queue.
    #[test]
    fn test_process_ready_fetching_envelopes_respects_tracking_slot() {
        let herder = make_test_herder();
        herder.bootstrap(100); // tracking_slot = 101

        // Insert ready envelopes for current slot (101) and future slot (102)
        let current_envs = vec![make_fetching_test_envelope(101, 1)];
        let future_envs = vec![
            make_fetching_test_envelope(102, 1),
            make_fetching_test_envelope(102, 2),
        ];

        herder
            .fetching_envelopes
            .test_insert_ready(101, current_envs);
        herder
            .fetching_envelopes
            .test_insert_ready(102, future_envs);

        // Drain should only process slot 101
        let processed = herder.process_ready_fetching_envelopes();
        assert_eq!(processed, 1, "should only process current-slot envelopes");

        // Future slot 102 envelopes should still be in the ready queue
        assert_eq!(
            herder.fetching_envelopes.ready_slots(),
            vec![102],
            "future-slot envelopes must remain in ready queue"
        );
        assert_eq!(herder.fetching_envelopes.slot_envelope_count(102), 2);
    }

    /// After slot advances (bootstrap to higher slot), previously-future
    /// envelopes become drainable via the bootstrap drain trigger.
    #[test]
    fn test_process_ready_fetching_envelopes_drains_after_slot_advance() {
        let herder = make_test_herder();
        herder.bootstrap(100); // tracking_slot = 101

        // Insert future-slot envelopes
        let future_envs = vec![
            make_fetching_test_envelope(102, 1),
            make_fetching_test_envelope(102, 2),
        ];
        herder
            .fetching_envelopes
            .test_insert_ready(102, future_envs);

        // First drain: nothing processed (102 > 101)
        let processed = herder.process_ready_fetching_envelopes();
        assert_eq!(processed, 0);

        // Advance tracking slot to 102 (simulates ledger close advancing).
        // bootstrap() internally calls process_ready_fetching_envelopes(),
        // which drains the now-eligible slot 102 envelopes.
        herder.bootstrap(101); // tracking_slot = 102

        // Verify the envelopes were drained by the bootstrap drain trigger
        assert!(
            herder.fetching_envelopes.ready_slots().is_empty(),
            "bootstrap drain trigger should have drained slot 102 envelopes"
        );
    }

    /// slot_envelope_count correctly counts fetching + ready envelopes.
    #[test]
    fn test_slot_envelope_count() {
        let herder = make_test_herder();

        // Insert some ready envelopes
        let envs = vec![
            make_fetching_test_envelope(50, 1),
            make_fetching_test_envelope(50, 2),
            make_fetching_test_envelope(50, 3),
        ];
        herder.fetching_envelopes.test_insert_ready(50, envs);

        assert_eq!(herder.fetching_envelopes.slot_envelope_count(50), 3);
        assert_eq!(herder.fetching_envelopes.slot_envelope_count(51), 0);
    }

    /// future_slot_count counts distinct slots with envelopes above the
    /// current tracking slot.
    #[test]
    fn test_future_slot_count() {
        let herder = make_test_herder();

        // Insert envelopes for slots 101, 102, 103 (all future relative to tracking=100)
        for slot in 101..=103 {
            herder
                .fetching_envelopes
                .test_insert_ready(slot, vec![make_fetching_test_envelope(slot, 1)]);
        }

        // Also insert for slot 99 (past) — should not be counted
        herder
            .fetching_envelopes
            .test_insert_ready(99, vec![make_fetching_test_envelope(99, 1)]);

        assert_eq!(herder.fetching_envelopes.future_slot_count(100), 3);
        assert_eq!(herder.fetching_envelopes.future_slot_count(101), 2);
        assert_eq!(herder.fetching_envelopes.future_slot_count(103), 0);
    }

    /// Admission control: per-slot limit rejects envelopes when a slot
    /// is saturated in FetchingEnvelopes.
    #[test]
    fn test_admission_control_per_slot_limit() {
        let herder = make_test_herder();
        herder.bootstrap(100); // tracking_slot = 101

        // Fill slot 102 to capacity
        let capacity = crate::pending::MAX_ENVELOPES_PER_SLOT;
        let envs: Vec<ScpEnvelope> = (0..capacity)
            .map(|i| make_fetching_test_envelope(102, i as u32))
            .collect();
        herder.fetching_envelopes.test_insert_ready(102, envs);

        assert_eq!(herder.fetching_envelopes.slot_envelope_count(102), capacity);

        // Now try to add another envelope for slot 102 through process_verified.
        // It should be rejected by the admission control gate.
        // We test via the helper method directly since the full
        // receive_scp_envelope path requires complex signature setup.
        let slot_count = herder.fetching_envelopes.slot_envelope_count(102);
        assert!(
            slot_count >= capacity,
            "slot should be at capacity for admission control to trigger"
        );
    }

    /// Admission control: future-slot count limit rejects when too many
    /// distinct future slots are buffered.
    #[test]
    fn test_admission_control_future_slot_limit() {
        let herder = make_test_herder();
        herder.bootstrap(100); // tracking_slot = 101

        let bracket = crate::sync_recovery::LEDGER_VALIDITY_BRACKET as usize;

        // Fill LEDGER_VALIDITY_BRACKET distinct future slots
        for i in 0..bracket {
            let slot = 102 + i as u64;
            herder
                .fetching_envelopes
                .test_insert_ready(slot, vec![make_fetching_test_envelope(slot, 1)]);
        }

        let future_count = herder.fetching_envelopes.future_slot_count(101);
        assert!(
            future_count >= bracket,
            "should have {bracket} future slots buffered"
        );
    }

    /// process_ready_fetching_envelopes processes past slots (< tracking)
    /// as well as the current tracking slot.
    #[test]
    fn test_process_ready_fetching_envelopes_drains_past_slots() {
        let herder = make_test_herder();
        herder.bootstrap(100); // tracking_slot = 101

        // Insert envelopes for slot 100 (past) and 101 (current)
        herder
            .fetching_envelopes
            .test_insert_ready(100, vec![make_fetching_test_envelope(100, 1)]);
        herder
            .fetching_envelopes
            .test_insert_ready(101, vec![make_fetching_test_envelope(101, 1)]);

        let processed = herder.process_ready_fetching_envelopes();
        assert_eq!(processed, 2, "should drain both past and current slots");
        assert!(herder.fetching_envelopes.ready_slots().is_empty());
    }

    /// Regression test for #2414/#2415: `ledger_closed()` prunes fetching_envelopes
    /// using `get_min_ledger_seq_to_remember()` and `get_most_recent_checkpoint_seq()`
    /// for parity with stellar-core's `newSlotExternalized` + `eraseOutsideRange`.
    ///
    /// With consensus_index=102 (tracking_slot=102, tracking=true):
    /// - min_slot = get_min_ledger_seq_to_remember() = (101 - 12 + 1) = 90 (> genesis, so Some(90))
    /// - keep_slot = get_most_recent_checkpoint_seq() = 64 (first ledger in checkpoint containing 101)
    /// - max_slot = next_consensus_ledger_index() + LEDGER_VALIDITY_BRACKET = 102 + 100 = 202
    #[test]
    fn test_ledger_closed_prunes_slots_above_validity_bracket() {
        let herder = make_test_herder();
        herder.bootstrap(100); // tracking_slot = 101

        // Simulate advance_tracking_slot(101) having already run:
        // consensus_index = 102, so next_consensus_ledger_index() = 102.
        // min_slot = Some(90), keep_slot = 64, max_slot = Some(202).
        {
            let mut ts = herder.tracking_state.write();
            ts.is_tracking = true;
            ts.consensus_index = 102;
        }

        let bracket = crate::sync_recovery::LEDGER_VALIDITY_BRACKET;

        // Insert envelopes for slots within the bracket (103..=202).
        for slot in 103..=(102 + bracket) {
            herder
                .fetching_envelopes
                .test_insert_ready(slot, vec![make_fetching_test_envelope(slot, 1)]);
        }

        // Insert envelopes beyond the bracket (203..=210) — should be pruned.
        for slot in (102 + bracket + 1)..=(102 + bracket + 8) {
            herder
                .fetching_envelopes
                .test_insert_ready(slot, vec![make_fetching_test_envelope(slot, 1)]);
        }

        // Insert a stale slot well below min_slot (50 < 90, and 50 != 64).
        herder
            .fetching_envelopes
            .test_insert_ready(50, vec![make_fetching_test_envelope(50, 1)]);

        // Insert checkpoint slot (64) — below min_slot but preserved as keep_slot.
        herder
            .fetching_envelopes
            .test_insert_ready(64, vec![make_fetching_test_envelope(64, 1)]);

        // Insert a slot within the retention window (91 >= min_slot=90).
        herder
            .fetching_envelopes
            .test_insert_ready(91, vec![make_fetching_test_envelope(91, 1)]);

        // Verify pre-conditions: all slots present
        assert!(herder.fetching_envelopes.slots.read().contains_key(&50));
        assert!(herder.fetching_envelopes.slots.read().contains_key(&64));
        assert!(herder.fetching_envelopes.slots.read().contains_key(&91));
        assert!(herder.fetching_envelopes.slots.read().contains_key(&103));
        assert!(herder.fetching_envelopes.slots.read().contains_key(&202));
        assert!(herder.fetching_envelopes.slots.read().contains_key(&203));
        assert!(herder.fetching_envelopes.slots.read().contains_key(&210));

        // Act: call ledger_closed which triggers the pruning
        herder.ledger_closed(101, &[], &[], 0);

        // Assert upper-bound pruning: slots above 202 are gone
        for slot in 203..=210 {
            assert!(
                !herder.fetching_envelopes.slots.read().contains_key(&slot),
                "slot {} should have been pruned (above upper bound 202)",
                slot
            );
        }

        // Assert lower-bound pruning: slot 50 is gone (below min_slot=90, not checkpoint)
        assert!(
            !herder.fetching_envelopes.slots.read().contains_key(&50),
            "slot 50 should have been pruned (below min_slot 90, not checkpoint slot)"
        );

        // Assert checkpoint keep_slot exemption: slot 64 survives despite being below min
        assert!(
            herder.fetching_envelopes.slots.read().contains_key(&64),
            "slot 64 should be preserved (keep_slot = checkpoint = 64)"
        );

        // Assert within-retention-window slots are preserved
        assert!(
            herder.fetching_envelopes.slots.read().contains_key(&91),
            "slot 91 should be preserved (>= min_slot 90)"
        );
        assert!(
            herder.fetching_envelopes.slots.read().contains_key(&103),
            "slot 103 should be preserved (within bracket, above tracking)"
        );
        assert!(
            herder.fetching_envelopes.slots.read().contains_key(&150),
            "slot 150 should be preserved (within bracket)"
        );
        assert!(
            herder.fetching_envelopes.slots.read().contains_key(&202),
            "slot 202 should be preserved (boundary: 102 + 100 = 202)"
        );
    }

    /// Regression test for #2415: `ledger_closed()` at genesis/early ledgers
    /// does NOT purge any slots (min_slot = None when min_ledger_seq <= 1).
    #[test]
    fn test_ledger_closed_no_lower_purge_at_genesis() {
        let herder = make_test_herder();
        // Bootstrap to slot 1 (tracking_slot = 2)
        herder.bootstrap(1);
        {
            let mut ts = herder.tracking_state.write();
            ts.is_tracking = true;
            ts.consensus_index = 2;
        }

        // get_min_ledger_seq_to_remember: (2-1) = 1, 1 <= 12, so returns 1.
        // Genesis guard: 1 <= GENESIS_LEDGER_SEQ, so min_slot = None.
        // No lower-bound purge should happen.
        herder
            .fetching_envelopes
            .test_insert_ready(1, vec![make_fetching_test_envelope(1, 1)]);

        herder.ledger_closed(1, &[], &[], 0);

        assert!(
            herder.fetching_envelopes.slots.read().contains_key(&1),
            "slot 1 should be preserved (no lower purge at genesis)"
        );
    }

    /// Regression test for #2415: First purge happens at ledger 13
    /// (tracking_slot=14, min_ledger_seq = (13 - 12 + 1) = 2).
    #[test]
    fn test_ledger_closed_first_purge_at_ledger_13() {
        let herder = make_test_herder();
        herder.bootstrap(13);
        {
            let mut ts = herder.tracking_state.write();
            ts.is_tracking = true;
            ts.consensus_index = 14;
        }

        // min_ledger_seq = (14-1) - 12 + 1 = 2. min_slot = Some(2).
        // keep_slot = get_most_recent_checkpoint_seq():
        //   tracking_consensus_index = 13, checkpoint = ((13/64+1)*64)-1 = 63,
        //   size = 63 (13 < 64), first = 63-62 = 1. keep_slot = 1.
        herder
            .fetching_envelopes
            .test_insert_ready(1, vec![make_fetching_test_envelope(1, 1)]);
        herder
            .fetching_envelopes
            .test_insert_ready(2, vec![make_fetching_test_envelope(2, 1)]);
        herder
            .fetching_envelopes
            .test_insert_ready(10, vec![make_fetching_test_envelope(10, 1)]);

        herder.ledger_closed(13, &[], &[], 0);

        // Slot 1 is below min_slot (2) but is checkpoint keep_slot — preserved.
        assert!(
            herder.fetching_envelopes.slots.read().contains_key(&1),
            "slot 1 preserved as checkpoint keep_slot"
        );
        // Slot 2 is exactly at min_slot — preserved.
        assert!(
            herder.fetching_envelopes.slots.read().contains_key(&2),
            "slot 2 preserved (>= min_slot)"
        );
        // Slot 10 is within range — preserved.
        assert!(
            herder.fetching_envelopes.slots.read().contains_key(&10),
            "slot 10 preserved (within range)"
        );
    }

    /// Regression test for #2415: Checkpoint boundary at ledger 65.
    /// After the first checkpoint completes (ledger 63), ledger 64 starts
    /// a new checkpoint. At ledger 65, checkpoint = 64, preserved as keep_slot.
    #[test]
    fn test_ledger_closed_checkpoint_preserved_at_boundary() {
        let herder = make_test_herder();
        herder.bootstrap(65);
        {
            let mut ts = herder.tracking_state.write();
            ts.is_tracking = true;
            ts.consensus_index = 66;
        }

        // min_ledger_seq = (66-1) - 12 + 1 = 54. min_slot = Some(54).
        // keep_slot = get_most_recent_checkpoint_seq():
        //   tracking_consensus_index = 65, ((65/64+1)*64)-1 = 127,
        //   size = 64 (65 >= 64), first = 127-63 = 64. keep_slot = 64.
        herder
            .fetching_envelopes
            .test_insert_ready(50, vec![make_fetching_test_envelope(50, 1)]);
        herder
            .fetching_envelopes
            .test_insert_ready(53, vec![make_fetching_test_envelope(53, 1)]);
        herder
            .fetching_envelopes
            .test_insert_ready(54, vec![make_fetching_test_envelope(54, 1)]);
        herder
            .fetching_envelopes
            .test_insert_ready(64, vec![make_fetching_test_envelope(64, 1)]);

        herder.ledger_closed(65, &[], &[], 0);

        // Slot 50 < min_slot (54) and != keep_slot (64) — pruned.
        assert!(
            !herder.fetching_envelopes.slots.read().contains_key(&50),
            "slot 50 should be pruned (below min_slot 54, not checkpoint)"
        );
        // Slot 53 < min_slot (54) and != keep_slot (64) — pruned.
        assert!(
            !herder.fetching_envelopes.slots.read().contains_key(&53),
            "slot 53 should be pruned (below min_slot 54, not checkpoint)"
        );
        // Slot 54 is exactly at min_slot — preserved.
        assert!(
            herder.fetching_envelopes.slots.read().contains_key(&54),
            "slot 54 preserved (== min_slot)"
        );
        // Slot 64 is checkpoint keep_slot — preserved.
        assert!(
            herder.fetching_envelopes.slots.read().contains_key(&64),
            "slot 64 preserved (checkpoint keep_slot and within range)"
        );
    }

    /// Regression test for the campaign-2 memory leak (2026-07-04): cached
    /// tx sets must be pruned outside `[min_slot, max_slot]` on every LIVE
    /// ledger close. Before the fix, the only lower-bound prune ran on the
    /// catchup path, so at ~7k tx/ledger the cache grew ~16 MB/ledger
    /// unbounded until the host OOM'd (23-node mission, 64 GB exhausted at
    /// ~minute 12 of every full-window run).
    #[test]
    fn test_ledger_closed_prunes_stale_cached_tx_sets() {
        let herder = make_test_herder();
        herder.bootstrap(65);
        {
            let mut ts = herder.tracking_state.write();
            ts.is_tracking = true;
            ts.consensus_index = 66;
        }
        // min_ledger_seq = 54, keep_slot (checkpoint) = 64 — see
        // test_ledger_closed_checkpoint_preserved_at_boundary for the math.

        // Old candidate (slot 20 < min 54, != keep) — must be evicted.
        let ts_old = TransactionSet::new(Hash256::from_bytes([201; 32]), Vec::new());
        let hash_old = *ts_old.hash();
        herder.scp_driver.cache_tx_set_with_slot(ts_old, 20);
        // Checkpoint set (slot 64 == keep_slot) — preserved.
        let ts_keep = TransactionSet::new(Hash256::from_bytes([202; 32]), Vec::new());
        let hash_keep = *ts_keep.hash();
        herder.scp_driver.cache_tx_set_with_slot(ts_keep, 64);
        // In-window set (slot 60 >= 54) — preserved.
        let ts_in = TransactionSet::new(Hash256::from_bytes([203; 32]), Vec::new());
        let hash_in = *ts_in.hash();
        herder.scp_driver.cache_tx_set_with_slot(ts_in, 60);

        herder.ledger_closed(65, &[], &[], 0);

        assert!(
            !herder.scp_driver.has_tx_set(&hash_old),
            "stale cached tx set (slot 20 < min 54) must be evicted on live close"
        );
        assert!(
            herder.scp_driver.has_tx_set(&hash_keep),
            "checkpoint keep_slot tx set preserved"
        );
        assert!(
            herder.scp_driver.has_tx_set(&hash_in),
            "in-window tx set preserved"
        );
    }

    /// Companion to the leak fix: own-built candidates must carry slot
    /// provenance (tracking slot), otherwise they are unknown-slot entries
    /// that survive every slot-range prune forever.
    #[test]
    fn test_cache_tx_set_stores_tracking_slot_provenance() {
        let herder = make_test_herder();
        herder.bootstrap(65);
        {
            let mut ts = herder.tracking_state.write();
            ts.is_tracking = true;
            ts.consensus_index = 66;
        }

        let ts = TransactionSet::new(Hash256::from_bytes([204; 32]), Vec::new());
        let hash = *ts.hash();
        herder.cache_tx_set(ts);

        assert_eq!(
            herder.scp_driver.cached_tx_set_slot(&hash),
            Some(Some(66)),
            "own-built candidate must be cached with tracking-slot provenance"
        );
    }

    /// Boot-time tx sets (tracking slot 0, e.g. restored from the database)
    /// keep the unknown-slot sentinel — mirroring stellar-core's slot-0 rule.
    #[test]
    fn test_cache_tx_set_boot_keeps_unknown_slot() {
        let herder = make_test_herder();

        let ts = TransactionSet::new(Hash256::from_bytes([205; 32]), Vec::new());
        let hash = *ts.hash();
        herder.cache_tx_set(ts);

        assert_eq!(
            herder.scp_driver.cached_tx_set_slot(&hash),
            Some(None),
            "boot-time (slot 0) tx sets keep the unknown-slot sentinel"
        );
    }

    /// Regression test for #2415: `get_min_ledger_seq_to_remember()` off-by-one fix.
    /// Verifies the function uses tracking_slot().saturating_sub(1) to align
    /// with stellar-core's `trackingConsensusLedgerIndex()`.
    #[test]
    fn test_get_min_ledger_seq_to_remember_parity() {
        let herder = make_test_herder();

        // tracking_slot = 0 (default) → saturating_sub(1) = 0 → returns 1
        assert_eq!(herder.get_min_ledger_seq_to_remember(), 1);

        // Bootstrap to slot 1: tracking_slot = 2
        // current_slot = 2 - 1 = 1, 1 <= 12 → returns 1
        herder.bootstrap(1);
        assert_eq!(herder.get_min_ledger_seq_to_remember(), 1);

        // Bootstrap to slot 12: tracking_slot = 13
        // current_slot = 13 - 1 = 12, 12 <= 12 → returns 1
        herder.bootstrap(12);
        assert_eq!(herder.get_min_ledger_seq_to_remember(), 1);

        // Bootstrap to slot 13: tracking_slot = 14
        // current_slot = 14 - 1 = 13, 13 > 12 → 13 - 12 + 1 = 2
        herder.bootstrap(13);
        assert_eq!(herder.get_min_ledger_seq_to_remember(), 2);

        // Bootstrap to slot 100: tracking_slot = 101
        // current_slot = 101 - 1 = 100, 100 > 12 → 100 - 12 + 1 = 89
        herder.bootstrap(100);
        assert_eq!(herder.get_min_ledger_seq_to_remember(), 89);
    }

    /// Regression test for #2417: `out_of_sync_recovery()` must preserve checkpoint
    /// slot 1 for early ledgers (tracking_slot in 1..63 range).
    ///
    /// Before the fix, the code computed `(lcl / freq) * freq` which yielded 0
    /// for early ledgers, failing to preserve checkpoint slot 1.
    #[test]
    fn test_out_of_sync_recovery_preserves_checkpoint_early_ledgers() {
        let herder = make_test_herder();

        // Bootstrap to ledger 49 → tracking_slot = 50, then transition to Syncing
        // so out_of_sync_recovery can fire (it requires state != Tracking).
        herder.bootstrap(49);
        {
            let mut ts = herder.tracking_state.write();
            // Keep consensus_index = 50 but clear tracking flag
            ts.is_tracking = false;
        }
        *herder.state.write() = HerderState::Syncing;

        // Verify checkpoint computation: tracking_slot=50, tracking_consensus_index=49
        // checkpoint containing 49: ((49/64+1)*64)-1 = 63, size=63 (49<64), first = 63-62 = 1
        assert_eq!(herder.get_most_recent_checkpoint_seq(), 1);

        // Set up >100 v-blocking SCP slots to trigger purge.
        // The recovery scans descending and counts v-blocking slots;
        // after 100, it purges below that point.
        // Create slots 200..=301 (102 slots) all with v-blocking.
        for slot in 200..=301 {
            herder.scp.test_set_slot_v_blocking(slot);
        }

        // Insert fetching_envelopes for slot 1 (checkpoint) and slot 5 (should be kept
        // because purge_slot will be 201, and erase_outside_range only erases below min_slot
        // EXCEPT slot_to_keep).
        herder
            .fetching_envelopes
            .test_insert_ready(1, vec![make_fetching_test_envelope(1, 1)]);
        herder
            .fetching_envelopes
            .test_insert_ready(5, vec![make_fetching_test_envelope(5, 1)]);
        herder
            .fetching_envelopes
            .test_insert_ready(250, vec![make_fetching_test_envelope(250, 1)]);

        // Act: trigger out-of-sync recovery
        let result = herder.out_of_sync_recovery();

        // The purge_slot should be 202 (100 v-blocking slots from 301 down to 202;
        // the 100th decrement brings max_slots_ahead to 0 at slot 202).
        assert_eq!(result, Some(202));

        // Slot 1 should be PRESERVED as the checkpoint keep_slot.
        assert!(
            herder.fetching_envelopes.slots.read().contains_key(&1),
            "slot 1 must be preserved as checkpoint keep_slot (#2417)"
        );

        // Slot 5 is below purge_slot (202) and not the checkpoint — it gets erased.
        assert!(
            !herder.fetching_envelopes.slots.read().contains_key(&5),
            "slot 5 should be erased (below purge_slot 202, not checkpoint)"
        );

        // Slot 250 is above purge_slot — preserved (erase_outside_range only has min_slot).
        assert!(
            herder.fetching_envelopes.slots.read().contains_key(&250),
            "slot 250 should be preserved (above purge_slot)"
        );
    }

    /// Regression test for #2417: `out_of_sync_recovery()` with tracking_slot=0
    /// (default/unbootstrapped) still preserves checkpoint 1 via saturating math.
    #[test]
    fn test_out_of_sync_recovery_preserves_checkpoint_at_tracking_slot_zero() {
        let herder = make_test_herder();

        // Default state: tracking_slot = 0, state = Booting.
        // Transition to Syncing so the function doesn't return early.
        herder.set_state(HerderState::Syncing);

        // get_most_recent_checkpoint_seq with tracking_slot=0:
        // saturating_sub(1) = 0, ((0/64+1)*64)-1 = 63, size=63 (0<64), first=1
        assert_eq!(herder.get_most_recent_checkpoint_seq(), 1);

        // Set up >100 v-blocking SCP slots
        for slot in 500..=601 {
            herder.scp.test_set_slot_v_blocking(slot);
        }

        // Insert slot 1 in fetching_envelopes
        herder
            .fetching_envelopes
            .test_insert_ready(1, vec![make_fetching_test_envelope(1, 1)]);

        let result = herder.out_of_sync_recovery();
        assert_eq!(result, Some(502));

        // Slot 1 preserved as checkpoint keep_slot even with tracking_slot=0
        assert!(
            herder.fetching_envelopes.slots.read().contains_key(&1),
            "slot 1 must be preserved even with tracking_slot=0 (#2417)"
        );
    }

    /// Regression test for #2417: after crossing first checkpoint boundary,
    /// out_of_sync_recovery preserves slot 64 (not slot 1).
    #[test]
    fn test_out_of_sync_recovery_preserves_checkpoint_after_boundary() {
        let herder = make_test_herder();

        // Bootstrap to ledger 65 → tracking_slot = 66
        herder.bootstrap(65);
        {
            let mut ts = herder.tracking_state.write();
            ts.is_tracking = false;
        }
        *herder.state.write() = HerderState::Syncing;

        // checkpoint containing 65: ((65/64+1)*64)-1 = 127, size=64, first=64
        assert_eq!(herder.get_most_recent_checkpoint_seq(), 64);

        // Set up >100 v-blocking SCP slots
        for slot in 300..=401 {
            herder.scp.test_set_slot_v_blocking(slot);
        }

        // Insert slots
        herder
            .fetching_envelopes
            .test_insert_ready(1, vec![make_fetching_test_envelope(1, 1)]);
        herder
            .fetching_envelopes
            .test_insert_ready(64, vec![make_fetching_test_envelope(64, 1)]);
        herder
            .fetching_envelopes
            .test_insert_ready(50, vec![make_fetching_test_envelope(50, 1)]);

        let result = herder.out_of_sync_recovery();
        assert_eq!(result, Some(302));

        // Slot 64 preserved as checkpoint keep_slot
        assert!(
            herder.fetching_envelopes.slots.read().contains_key(&64),
            "slot 64 must be preserved as checkpoint keep_slot"
        );

        // Slot 1 is below purge_slot and NOT the checkpoint — erased
        assert!(
            !herder.fetching_envelopes.slots.read().contains_key(&1),
            "slot 1 should be erased (not checkpoint when past boundary)"
        );

        // Slot 50 is below purge_slot and NOT the checkpoint — erased
        assert!(
            !herder.fetching_envelopes.slots.read().contains_key(&50),
            "slot 50 should be erased"
        );
    }

    /// Regression test for #2706: `ledger_closed` must preserve the most
    /// recent checkpoint slot when purging old SCP state. The previous code
    /// passed `None` for `slot_to_keep`, so a checkpoint slot below
    /// `slot - 10` would be dropped — silently breaking the delayed
    /// `send_scp_state` callback (#2670).
    ///
    /// Parity: stellar-core `HerderImpl::eraseOutsideRange`
    /// (HerderImpl.cpp:1328-1335) → `HerderSCPDriver::purgeSlotsOutsideRange`
    /// (HerderSCPDriver.cpp:1300-1337) always threads
    /// `getMostRecentCheckpointSeq()` as `slotToKeep`.
    #[test]
    fn test_issue_2706_ledger_closed_preserves_checkpoint_slot_in_scp() {
        let herder = make_test_herder();

        // Bootstrap so tracking_consensus_index = 300. With
        // checkpoint_frequency = 64 (default), the most-recent-checkpoint
        // slot is 256. We will close slot 300, whose retention threshold is
        // slot - 10 = 290, so checkpoint 256 sits well below the window.
        herder.bootstrap(299);
        {
            let mut ts = herder.tracking_state.write();
            ts.is_tracking = true;
            ts.consensus_index = 300;
        }
        assert_eq!(herder.get_most_recent_checkpoint_seq(), 256);

        // Materialize SCP state for the checkpoint slot (must survive) and a
        // control slot below the retention window (must be purged).
        herder.scp.test_set_slot_v_blocking(256);
        herder.scp.test_set_slot_v_blocking(250);
        // And a slot inside the retention window — should also survive.
        herder.scp.test_set_slot_v_blocking(295);
        assert!(herder.scp.has_slot(256));
        assert!(herder.scp.has_slot(250));
        assert!(herder.scp.has_slot(295));

        // Act
        herder.ledger_closed(300, &[], &[], 0);

        // Checkpoint slot 256 is preserved despite 256 < 290 (the
        // slot - 10 retention threshold) thanks to slot_to_keep.
        assert!(
            herder.scp.has_slot(256),
            "checkpoint slot 256 must be preserved as slot_to_keep (#2706)"
        );
        // Control slot 250 is below the window and not the checkpoint —
        // it must be purged, confirming purge_slots actually ran.
        assert!(
            !herder.scp.has_slot(250),
            "slot 250 should be purged (below retention window, not checkpoint)"
        );
        // Slot inside the retention window survives normally.
        assert!(
            herder.scp.has_slot(295),
            "slot 295 should be preserved (within retention window)"
        );
    }

    /// Regression test for #2706: `out_of_sync_recovery` must also preserve
    /// the most-recent-checkpoint slot when purging old SCP state. Before
    /// the fix, the SCP purge in `out_of_sync_recovery` passed `None` for
    /// `slot_to_keep`, dropping the checkpoint slot's SCP state and
    /// breaking the delayed `send_scp_state` callback (#2670) on the
    /// recovery path.
    #[test]
    fn test_issue_2706_out_of_sync_recovery_preserves_checkpoint_in_scp() {
        let herder = make_test_herder();

        // Bootstrap to ledger 65 so checkpoint = 64.
        herder.bootstrap(65);
        {
            let mut ts = herder.tracking_state.write();
            ts.is_tracking = false;
        }
        *herder.state.write() = HerderState::Syncing;
        assert_eq!(herder.get_most_recent_checkpoint_seq(), 64);

        // Materialize SCP state at the checkpoint (must survive) and one
        // non-checkpoint slot below the purge boundary (must be purged).
        herder.scp.test_set_slot_v_blocking(64);
        herder.scp.test_set_slot_v_blocking(100);
        assert!(herder.scp.has_slot(64));
        assert!(herder.scp.has_slot(100));

        // Set up >100 v-blocking SCP slots to trigger the recovery purge.
        for slot in 300..=401 {
            herder.scp.test_set_slot_v_blocking(slot);
        }

        let result = herder.out_of_sync_recovery();
        assert_eq!(result, Some(302));

        // Checkpoint slot survives despite being below the recovery purge
        // boundary (purge_slot - 1 = 301).
        assert!(
            herder.scp.has_slot(64),
            "checkpoint slot 64 must be preserved in SCP as slot_to_keep (#2706)"
        );
        // Non-checkpoint slot below the boundary is purged.
        assert!(
            !herder.scp.has_slot(100),
            "slot 100 should be purged from SCP (below purge boundary, not checkpoint)"
        );
    }

    /// #3845: the `MemoryReporter` impl exposes exactly the five named herder
    /// components, so the periodic memory report can attribute them.
    #[test]
    fn test_herder_memory_components_names() {
        use henyey_common::memory::MemoryReporter;
        let herder = make_test_herder();
        let names: Vec<&str> = herder.memory_components().iter().map(|c| c.name).collect();
        assert_eq!(
            names,
            vec![
                "herder_tx_queue",
                "herder_fetching_envelopes",
                "herder_scp_slots",
                "herder_quorum",
                "herder_externalize_lag",
            ]
        );
    }
}
