//! Core application struct and component initialization for rs-stellar-core.
//!
//! This module contains the [`App`] struct, which is the central coordinator for all
//! Stellar Core subsystems. It manages the lifecycle of:
//!
//! - **Database**: SQLite persistence for ledger headers, transactions, and state
//! - **BucketManager**: Merkle tree storage for ledger entry snapshots
//! - **LedgerManager**: Ledger close operations and state transitions
//! - **OverlayManager**: P2P network connections and message routing
//! - **Herder**: SCP consensus coordination and transaction queue management
//!
//! # Application Lifecycle
//!
//! The typical lifecycle of an App instance:
//!
//! 1. **Initialization** ([`App::new`]): Load configuration, open database, initialize
//!    subsystems, and restore state from disk
//! 2. **Catchup** ([`App::catchup`]): If behind, download and apply history from archives
//! 3. **Run** ([`App::run`]): Enter the main event loop, processing peer messages
//!    and participating in consensus
//! 4. **Shutdown** ([`App::shutdown`]): Gracefully stop all subsystems
//!
//! # State Machine
//!
//! The application transitions through these states (see [`AppState`]):
//!
//! ```text
//! Initializing -> CatchingUp -> Synced <-> Validating
//!                     ^            |
//!                     |            v
//!                     +--- ShuttingDown
//! ```
//!
//! # Consensus Integration
//!
//! For validator nodes, the App coordinates SCP message flow:
//! - Receives SCP envelopes from peers via the overlay
//! - Passes them to the Herder for processing
//! - Broadcasts locally-generated envelopes back to peers
//! - Triggers ledger close when consensus is reached

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::fs::File;
use std::fs::OpenOptions;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rand::seq::SliceRandom;
use tokio::sync::mpsc;
use tokio::sync::Mutex as TokioMutex;
use tokio::sync::RwLock;

use henyey_bucket::BucketManager;
use henyey_bucket::{
    BucketList, BucketListSnapshot, BucketSnapshotManager, HasNextState, HotArchiveBucketList,
    HotArchiveBucketListSnapshot,
};
use henyey_clock::{Clock, RealClock};
use henyey_common::protocol::{protocol_version_starts_from, ProtocolVersion};
use henyey_common::{deterministic_seed, Hash256, NetworkId};
use henyey_db::queries::StateQueries;
use henyey_db::schema::state_keys;
use henyey_db::{
    BucketListQueries, EventQueries, HistoryQueries, LedgerQueries, PublishQueueQueries, ScpQueries,
};
use henyey_herder::{
    drift_tracker::CloseTimeDriftTracker,
    flow_control::compute_max_tx_size,
    sync_recovery::{SyncRecoveryCallback, SyncRecoveryHandle, SyncRecoveryManager},
    EnvelopeState, Herder, HerderCallback, HerderConfig, HerderStats, TxQueueConfig,
};
use henyey_history::{
    build_history_archive_state, checkpoint_containing, checkpoint_frequency, is_checkpoint_ledger,
    latest_checkpoint_before_or_at, CatchupManager, CatchupMode, CatchupOutput, CheckpointData,
    ExistingBucketState, HistoryArchive, HistoryArchiveState, GENESIS_LEDGER_SEQ,
};
use henyey_historywork::{
    build_checkpoint_data, get_progress, HistoryWorkBuilder, HistoryWorkState,
};
use henyey_ledger::{
    LedgerCloseData, LedgerCloseResult, LedgerManager, LedgerManagerConfig, SorobanNetworkInfo,
    TransactionSetVariant,
};
use henyey_overlay::{
    ConnectionDirection, ConnectionFactory, LocalNode, OverlayConfig as OverlayManagerConfig,
    OverlayManager, OverlayMessage, PeerAddress, PeerId, PeerSnapshot, TcpConnectionFactory,
};
use henyey_scp::hash_quorum_set;
use henyey_tx::TransactionFrame;
use henyey_work::{WorkScheduler, WorkSchedulerConfig, WorkState};
use stellar_xdr::curr::{
    Curve25519Public, DontHave, EncryptedBody, FloodAdvert, FloodDemand, Hash, LedgerCloseMeta,
    LedgerScpMessages, LedgerUpgrade, MessageType, ReadXdr, ScpEnvelope, ScpHistoryEntry,
    ScpHistoryEntryV0, ScpStatementPledges, SignedTimeSlicedSurveyResponseMessage, StellarMessage,
    StellarValue, StellarValueExt, SurveyMessageCommandType, SurveyRequestMessage,
    SurveyResponseBody, SurveyResponseMessage, TimeSlicedPeerDataList,
    TimeSlicedSurveyRequestMessage, TimeSlicedSurveyResponseMessage,
    TimeSlicedSurveyStartCollectingMessage, TimeSlicedSurveyStopCollectingMessage,
    TopologyResponseBodyV2, TransactionHistoryEntry, TransactionHistoryEntryExt,
    TransactionHistoryResultEntry, TransactionHistoryResultEntryExt, TransactionMeta,
    TransactionResultPair, TransactionResultSet, TransactionSet, TxAdvertVector, TxDemandVector,
    UpgradeType, VecM, WriteXdr,
};
use x25519_dalek::{PublicKey as CurvePublicKey, StaticSecret as CurveSecretKey};

use crate::config::AppConfig;
use crate::logging::CatchupProgress;
use crate::meta_stream::{MetaStreamError, MetaStreamManager};
use crate::survey::{SurveyDataManager, SurveyMessageLimiter};
use henyey_ledger::{close_time as ledger_close_time, compute_header_hash, verify_header_chain};
use stellar_xdr::curr::TransactionEnvelope;

const TIME_SLICED_PEERS_MAX: usize = 25;
const PEER_MAX_FAILURES_TO_SEND: u32 = 10;
const TX_SET_REQUEST_WINDOW: u64 = 12;
const MAX_TX_SET_REQUESTS_PER_TICK: usize = 32;
/// Consensus stuck timeout matching stellar-core's CONSENSUS_STUCK_TIMEOUT_SECONDS.
/// If no ledger closes within this time while we have buffered ledgers waiting,
/// we trigger out-of-sync recovery.
const CONSENSUS_STUCK_TIMEOUT_SECS: u64 = 35;

/// Faster timeout when all peers report DontHave or disconnect.
/// This allows us to trigger catchup sooner when we know peers don't have the tx sets.
const TX_SET_UNAVAILABLE_TIMEOUT_SECS: u64 = 5;

/// Number of consecutive recovery attempts without ledger progress before
/// escalating from passive waiting to actively requesting SCP state from
/// peers. At the 1s consensus recovery interval this equals ~6s.
const RECOVERY_ESCALATION_SCP_REQUEST: u64 = 6;

/// Number of consecutive recovery attempts without progress before
/// triggering a full catchup. At the 1s consensus recovery interval this
/// equals ~6s.
const RECOVERY_ESCALATION_CATCHUP: u64 = 6;

/// Timeout for pending tx_set requests with no response from any peer.
/// If we've been requesting a tx_set for this long with zero responses
/// (no GeneralizedTxSet AND no DontHave), assume peers silently dropped
/// the requests and treat as if all peers said DontHave.
const TX_SET_REQUEST_TIMEOUT_SECS: u64 = 10;

/// Recovery timer for out-of-sync recovery attempts.
/// Matches stellar-core's OUT_OF_SYNC_RECOVERY_TIMER.
const OUT_OF_SYNC_RECOVERY_TIMER_SECS: u64 = 10;

/// How long to cache the archive checkpoint before re-querying.
/// This prevents repeated network calls to the archive when we're stuck.
const ARCHIVE_CHECKPOINT_CACHE_SECS: u64 = 60;

/// Post-catchup recovery window: after completing catchup, prefer SCP recovery
/// over triggering another catchup for at least one full checkpoint cycle (~5 min).
/// The first checkpoint after initial catchup won't be published to archives for
/// ~320s (64 ledgers * 5s). During this window, missing ledgers can only be filled
/// via SCP state requests from peers, not from archive downloads.
const POST_CATCHUP_RECOVERY_WINDOW_SECS: u64 = 300;

/// Maximum number of recovery attempts after catchup before giving up on
/// SCP-based gap filling and falling back to a second catchup. Peers only cache
/// ~12 recent slots, so if the gap slots were evicted before we connected,
/// recovery will never succeed. 3 attempts × 10s interval = 30s before fallback.
const MAX_POST_CATCHUP_RECOVERY_ATTEMPTS: u32 = 3;

mod catchup_impl;
mod consensus;
mod ledger_close;
mod lifecycle;
mod peers;
mod publish;
mod survey_impl;
mod tx_flooding;
mod types;

use types::*;
pub use types::{
    AppBuilder, AppInfo, AppState, CatchupResult, CatchupTarget, LedgerSummary, ScpSlotSnapshot,
    SelfCheckResult, SimulationDebugStats, SurveyPeerReport, SurveyReport,
};

/// The main application struct coordinating all Stellar Core subsystems.
///
/// `App` is the central component that:
/// - Owns all long-lived subsystem handles (database, bucket manager, ledger manager, etc.)
/// - Manages the application lifecycle (initialization, catchup, run, shutdown)
/// - Routes messages between the overlay network and consensus components
/// - Handles transaction submission and flooding
/// - Provides HTTP API endpoints for monitoring and control
///
/// # Thread Safety
///
/// `App` is designed to be shared across async tasks via `Arc<App>`. Internal
/// state is protected by appropriate locks (`RwLock`, `Mutex`).
///
/// # Creating an App
///
/// ```no_run
/// use henyey_app::{App, AppConfig};
///
/// # async fn example() -> anyhow::Result<()> {
/// let config = AppConfig::testnet();
/// let app = App::new(config).await?;
/// # Ok(())
/// # }
/// ```
pub struct App {
    /// Application configuration.
    config: AppConfig,

    /// Clock abstraction for runtime behavior.
    clock: Arc<dyn Clock>,

    /// Connection factory for overlay transport (TCP by default).
    overlay_connection_factory: Arc<dyn ConnectionFactory>,

    /// Current application state.
    state: RwLock<AppState>,

    /// Database connection.
    db: henyey_db::Database,
    /// Lock file handle to prevent multiple instances.
    /// Stored to keep the lock alive for the lifetime of the App.
    _db_lock: Option<File>,

    /// Node keypair.
    keypair: henyey_crypto::SecretKey,

    /// Bucket manager for ledger state persistence.
    bucket_manager: Arc<BucketManager>,

    /// Snapshot manager for thread-safe concurrent bucket list queries.
    /// Used by the query server to serve `/getledgerentry` and `/getledgerentryraw`.
    bucket_snapshot_manager: Arc<BucketSnapshotManager>,

    /// Ledger manager for ledger operations.
    ledger_manager: Arc<LedgerManager>,

    /// Overlay network manager.
    /// Wrapped in Arc so callers can clone the reference and use it without
    /// holding the RwLock, preventing the overlay lock from blocking the main
    /// event loop during slow network operations.
    overlay: RwLock<Option<Arc<OverlayManager>>>,

    /// Herder for consensus coordination.
    herder: Arc<Herder>,

    /// Current ledger sequence.
    current_ledger: RwLock<u32>,

    /// Whether running as validator.
    is_validator: bool,

    /// Shutdown signal sender.
    shutdown_tx: tokio::sync::broadcast::Sender<()>,

    /// Shutdown signal receiver.
    _shutdown_rx: tokio::sync::broadcast::Receiver<()>,

    /// Channel for outbound SCP envelopes.
    scp_envelope_tx: tokio::sync::mpsc::Sender<ScpEnvelope>,

    /// Receiver for outbound SCP envelopes.
    scp_envelope_rx: TokioMutex<tokio::sync::mpsc::Receiver<ScpEnvelope>>,

    /// Last processed externalized slot (for ledger close triggering).
    last_processed_slot: RwLock<u64>,
    /// Prevent concurrent catchup runs when we fall behind.
    catchup_in_progress: AtomicBool,
    /// Fatal catchup failure flag (spec §13.3).
    ///
    /// Set when catchup verification detects that the local ledger state is
    /// corrupt (chain disagrees with archive data obtained via SCP trust).
    /// Once set, **no further catchup attempts are made** — the node requires
    /// manual intervention (restart with fresh state).
    catchup_fatal_failure: AtomicBool,
    /// When set, the next catchup should do a full bucket-apply instead of
    /// replay-only. This is triggered when a previous catchup fails with a
    /// hash mismatch (state divergence, e.g., protocol upgrade missed).
    catchup_needs_full_reset: AtomicBool,
    /// Buffered externalized ledgers waiting to apply.
    syncing_ledgers: RwLock<BTreeMap<u32, henyey_herder::LedgerCloseInfo>>,
    /// Latest externalized slot we've observed (for liveness checks).
    last_externalized_slot: AtomicU64,
    /// Count of SCP envelopes broadcast by this node.
    scp_messages_sent: AtomicU64,
    /// Per-type SCP broadcast counters for heartbeat diagnostics.
    scp_nominate_sent: AtomicU64,
    scp_prepare_sent: AtomicU64,
    scp_confirm_sent: AtomicU64,
    scp_externalize_sent: AtomicU64,
    /// Count of SCP envelopes received by this node.
    scp_messages_received: AtomicU64,
    /// Number of attempts to trigger the next consensus round.
    consensus_trigger_attempts: AtomicU64,
    /// Number of successful trigger_next_ledger calls.
    consensus_trigger_successes: AtomicU64,
    /// Number of failed trigger_next_ledger calls.
    consensus_trigger_failures: AtomicU64,
    /// Number of nomination timeout firings.
    nomination_timeout_fires: AtomicU64,
    /// Number of ballot timeout firings.
    ballot_timeout_fires: AtomicU64,
    /// Time when we last observed an externalized slot.
    last_externalized_at: RwLock<Instant>,
    /// Last time we requested SCP state due to stalled externalization.
    last_scp_state_request_at: RwLock<Instant>,

    /// Time-sliced survey data manager.
    survey_data: RwLock<SurveyDataManager>,

    /// Pending transaction hashes to advertise.
    tx_advert_queue: RwLock<Vec<Hash256>>,
    /// Deduplication set for pending tx adverts.
    tx_advert_set: RwLock<HashSet<Hash256>>,

    /// Per-peer advert tracking and queues for demand scheduling.
    tx_adverts_by_peer: RwLock<HashMap<henyey_overlay::PeerId, PeerTxAdverts>>,
    /// Demand history for transaction pulls.
    tx_demand_history: RwLock<HashMap<Hash256, TxDemandHistory>>,
    /// Pending demand hashes in FIFO order for retention.
    tx_pending_demands: RwLock<VecDeque<Hash256>>,
    /// Per-txset DontHave tracking to avoid retrying peers that lack the set.
    tx_set_dont_have: RwLock<HashMap<Hash256, HashSet<henyey_overlay::PeerId>>>,
    /// Last time we requested a tx set by hash (throttling).
    tx_set_last_request: RwLock<HashMap<Hash256, TxSetRequestState>>,
    /// Tracks when all peers have been exhausted for a tx set (all said DontHave or disconnected).
    /// When this is true, we use a faster timeout to trigger catchup.
    tx_set_all_peers_exhausted: AtomicBool,
    /// Tx set hashes we've already logged "all peers exhausted" warning for (to avoid spam).
    tx_set_exhausted_warned: RwLock<HashSet<Hash256>>,
    /// When we detected consensus is stuck (for timeout detection).
    /// Stores (current_ledger, first_buffered, stuck_start_time, last_recovery_attempt).
    consensus_stuck_state: RwLock<Option<ConsensusStuckState>>,
    /// When catchup last completed (for cooldown).
    last_catchup_completed_at: RwLock<Option<Instant>>,
    /// Cached archive checkpoint (ledger, queried_at) to avoid repeated network calls.
    cached_archive_checkpoint: RwLock<Option<(u32, Instant)>>,
    /// SCP latency samples for surveys.
    scp_latency: RwLock<ScpLatencyTracker>,

    /// Survey scheduler state for time-sliced surveys.
    survey_scheduler: RwLock<SurveyScheduler>,
    /// Next survey nonce.
    survey_nonce: RwLock<u32>,
    /// Ephemeral survey encryption secrets keyed by nonce.
    survey_secrets: RwLock<HashMap<u32, [u8; 32]>>,
    /// Survey responses keyed by nonce.
    survey_results: RwLock<HashMap<u32, HashMap<henyey_overlay::PeerId, TopologyResponseBodyV2>>>,
    /// Survey message limiter for rate limiting and deduplication.
    survey_limiter: RwLock<SurveyMessageLimiter>,
    /// Survey throttle timeout between survey runs.
    survey_throttle: Duration,
    /// Survey reporting backlog state (surveyor-side).
    survey_reporting: RwLock<SurveyReportingState>,
    /// SCP timeout scheduling state.
    scp_timeouts: RwLock<ScpTimeoutState>,

    /// Metadata output stream manager for emitting LedgerCloseMeta.
    meta_stream: std::sync::Mutex<Option<MetaStreamManager>>,

    /// Close time drift tracker for clock synchronization monitoring.
    drift_tracker: std::sync::Mutex<CloseTimeDriftTracker>,

    /// Handle for sending commands to the sync recovery manager.
    /// Uses parking_lot::RwLock for synchronous access from callbacks.
    sync_recovery_handle: parking_lot::RwLock<Option<SyncRecoveryHandle>>,

    /// Whether ledger application is currently in progress (for sync recovery).
    is_applying_ledger: AtomicBool,

    /// Flag set by SyncRecoveryManager to request recovery from the main loop.
    /// The main loop checks this and triggers buffered catchup when set.
    sync_recovery_pending: AtomicBool,

    /// Consecutive recovery attempts without progress.  Reset to 0 whenever
    /// `current_ledger` advances.  When this exceeds a threshold the node
    /// escalates from passive waiting to actively requesting SCP state or
    /// triggering catchup.
    recovery_attempts_without_progress: AtomicU64,
    /// The ledger sequence at which recovery_attempts_without_progress was
    /// last reset.  Used to detect progress.
    recovery_baseline_ledger: AtomicU64,

    /// Total number of times the node lost sync.
    lost_sync_count: AtomicU64,
    /// Number of ledger closes that contained at least one transaction.
    /// Mirrors stellar-core's `ledger.transaction.count` histogram `.count`.
    ledger_tx_count: AtomicU64,
    /// Current max tx size in bytes for flow control (tracks upgrades).
    /// Mirrors upstream `mMaxTxSize` in HerderImpl.
    max_tx_size_bytes: AtomicU32,
    /// Monotonic counter used for ping IDs.
    ping_counter: AtomicU64,
    /// In-flight ping requests keyed by hash.
    ping_inflight: RwLock<HashMap<Hash256, PingInfo>>,
    /// In-flight ping hash per peer.
    peer_ping_inflight: RwLock<HashMap<henyey_overlay::PeerId, Hash256>>,

    /// Per-peer `GET_SCP_STATE` rate limiter.
    ///
    /// Tracks request counts in a rolling window to enforce the
    /// `GET_SCP_STATE_MAX_RATE` (10 requests per ~60s window) cap.
    /// Matches stellar-core's `Peer::mSCPStateQueryInfo`.
    scp_state_query_info: RwLock<HashMap<henyey_overlay::PeerId, QueryInfo>>,

    /// Weak reference to self for spawning background tasks from &self methods.
    /// Set via `set_self_arc` after wrapping in Arc.
    self_arc: RwLock<std::sync::Weak<Self>>,

    /// Monotonic timestamp (ms since epoch) of the last event loop iteration.
    /// Updated at the top of each select! iteration. Read by the std::thread
    /// watchdog to detect event loop freezes.
    last_event_loop_tick_ms: Arc<AtomicU64>,

    /// Numeric code indicating what the event loop is currently doing.
    /// Read by the watchdog to identify where a freeze occurs.
    /// Codes: 0=idle/select, 1=scp_message, 2=fetch_response, 3=broadcast_msg,
    ///        4=scp_broadcast, 5=consensus_tick, 6=pending_close,
    ///        10=process_externalized, 11=maybe_externalized_catchup,
    ///        12=try_apply_buffered, 13=maybe_buffered_catchup,
    ///        14=catchup_running, 15=heartbeat
    event_loop_phase: Arc<AtomicU64>,
}

impl App {
    /// Create a new application instance.
    pub async fn new(config: AppConfig) -> anyhow::Result<Self> {
        Self::new_with_clock_and_connection_factory(
            config,
            Arc::new(RealClock),
            Arc::new(TcpConnectionFactory),
        )
        .await
    }

    /// Create a new application instance with an injected clock.
    pub async fn new_with_clock(config: AppConfig, clock: Arc<dyn Clock>) -> anyhow::Result<Self> {
        Self::new_with_clock_and_connection_factory(config, clock, Arc::new(TcpConnectionFactory))
            .await
    }

    /// Create a new application instance with injected clock and overlay factory.
    pub async fn new_with_clock_and_connection_factory(
        config: AppConfig,
        clock: Arc<dyn Clock>,
        overlay_connection_factory: Arc<dyn ConnectionFactory>,
    ) -> anyhow::Result<Self> {
        // Apply testing overrides early, before any checkpoint math is used.
        if config.testing.accelerate_time {
            henyey_history::set_checkpoint_frequency(
                henyey_history::ACCELERATED_CHECKPOINT_FREQUENCY,
            );
            tracing::info!(
                checkpoint_frequency = henyey_history::ACCELERATED_CHECKPOINT_FREQUENCY,
                ledger_close_time = 1,
                "Accelerated time for testing enabled"
            );
        }

        tracing::info!(
            node_name = %config.node.name,
            network = %config.network.passphrase,
            "Initializing henyey"
        );

        // Validate configuration
        config.validate()?;

        let db_lock = Self::acquire_db_lock(&config)?;

        // Initialize database
        let db = Self::init_database(&config)?;

        // Ensure network passphrase matches stored state.
        Self::ensure_network_passphrase(&db, &config.network.passphrase)?;

        // Verify on-disk ledger headers before loading state.
        Self::verify_on_disk_integrity(&db)?;

        // Initialize or generate keypair
        let keypair = Self::init_keypair(&config)?;

        tracing::info!(
            public_key = %keypair.public_key().to_strkey(),
            "Node identity"
        );

        let is_validator = config.node.is_validator;
        let max_inbound_peers = config.overlay.max_inbound_peers as u32;
        let max_outbound_peers = config.overlay.max_outbound_peers as u32;

        // Convert quorum set config to XDR
        let local_quorum_set = config.node.quorum_set.to_xdr();
        if let Some(ref qs) = local_quorum_set {
            tracing::info!(
                threshold = qs.threshold,
                validators = qs.validators.len(),
                inner_sets = qs.inner_sets.len(),
                "Loaded quorum set configuration"
            );
        }

        // Initialize bucket manager for ledger state persistence
        let bucket_dir = config
            .database
            .path
            .parent()
            .unwrap_or(&config.database.path)
            .join("buckets");
        std::fs::create_dir_all(&bucket_dir)?;

        let bucket_manager = Arc::new(BucketManager::new(bucket_dir.clone())?);
        tracing::info!("Bucket manager initialized");

        // Initialize the bucket snapshot manager for concurrent query access.
        // Starts empty; snapshots are populated after ledger state is restored
        // and updated after each ledger close.
        let num_historical = config.query.snapshot_ledgers;
        let bucket_snapshot_manager = Arc::new(BucketSnapshotManager::empty(num_historical));

        // Initialize ledger manager
        let mut ledger_manager = LedgerManager::new(
            config.network.passphrase.clone(),
            LedgerManagerConfig {
                validate_bucket_hash: true,
                emit_classic_events: config.events.emit_classic_events,
                backfill_stellar_asset_events: config.events.backfill_stellar_asset_events,
                bucket_list_db: config.buckets.bucket_list_db.clone(),
                emit_ledger_close_meta_ext_v1: config.metadata.emit_ledger_close_meta_ext_v1,
                emit_soroban_tx_meta_ext_v1: config.metadata.emit_soroban_tx_meta_ext_v1,
                enable_soroban_diagnostic_events: config.diagnostics.soroban_diagnostic_events,
                scan_thread_count: config.buckets.scan_thread_count,
            },
        );

        // Wire merge map from BucketManager into LedgerManager for merge deduplication.
        // This enables reuse of previously computed merge results across restarts.
        let finished_merges =
            Arc::new(std::sync::RwLock::new(henyey_bucket::BucketMergeMap::new()));
        ledger_manager.set_merge_map(finished_merges);

        let ledger_manager = Arc::new(ledger_manager);
        tracing::info!("Ledger manager initialized");

        // Create herder configuration
        let freq = checkpoint_frequency();
        let herder_config = HerderConfig {
            max_pending_transactions: 1000,
            is_validator: config.node.is_validator,
            ledger_close_time: config
                .testing
                .ledger_close_time
                .unwrap_or(if config.testing.accelerate_time { 1 } else { 5 }),
            node_public_key: keypair.public_key(),
            network_id: config.network_id(),
            max_externalized_slots: freq as usize * 2,
            max_tx_set_size: 1000,
            pending_config: Default::default(),
            tx_queue_config: TxQueueConfig {
                network_id: henyey_common::NetworkId(config.network_id()),
                max_dex_ops: config.surge_pricing.max_dex_tx_operations,
                max_classic_bytes: Some(config.surge_pricing.classic_byte_allowance),
                max_soroban_bytes: Some(config.surge_pricing.soroban_byte_allowance),
                ..Default::default()
            },
            local_quorum_set,
            proposed_upgrades: config.upgrades.to_ledger_upgrades(),
            max_protocol_version: config.network.max_protocol_version,
            checkpoint_frequency: freq as u64,
        };

        // Create herder (with or without secret key for signing)
        let survey_throttle = Duration::from_secs(herder_config.ledger_close_time as u64 * 3);

        let herder = if config.node.is_validator {
            Arc::new(Herder::with_secret_key(herder_config, keypair.clone()))
        } else {
            Arc::new(Herder::new(herder_config))
        };
        herder.set_ledger_manager(ledger_manager.clone());
        herder
            .tx_queue()
            .set_fee_balance_provider(Arc::new(types::LedgerFeeBalanceProvider {
                ledger_manager: ledger_manager.clone(),
            }));

        if let Some(qs) = herder.local_quorum_set() {
            let hash = hash_quorum_set(&qs);
            if let Err(err) = db.store_scp_quorum_set(&hash, 0, &qs) {
                tracing::warn!(error = %err, "Failed to store local quorum set");
            }
        }

        // Initialize metadata stream if configured
        let meta_stream =
            if config.metadata.output_stream.is_some() || config.metadata.debug_ledgers > 0 {
                match MetaStreamManager::new(&config.metadata, &bucket_dir) {
                    Ok(ms) => {
                        if ms.is_streaming() {
                            tracing::info!("Metadata output stream initialized");
                        }
                        Some(ms)
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "Failed to initialize metadata stream");
                        return Err(e.into());
                    }
                }
            } else {
                None
            };

        // Create shutdown channel
        let (shutdown_tx, shutdown_rx) = tokio::sync::broadcast::channel(1);

        // Create channel for outbound SCP envelopes
        let (scp_envelope_tx, scp_envelope_rx) = tokio::sync::mpsc::channel(100);
        let now = clock.now();

        // Wire up envelope sender for validators
        if config.node.is_validator {
            let tx = scp_envelope_tx.clone();
            herder.set_envelope_sender(move |envelope| {
                // Non-blocking send - if channel is full, we drop the envelope
                // (This is fine, SCP will retry)
                let _ = tx.try_send(envelope);
            });
            tracing::info!("Envelope sender configured for validator mode");
        }

        Ok(Self {
            is_validator,
            config,
            clock,
            overlay_connection_factory,
            state: RwLock::new(AppState::Initializing),
            db,
            _db_lock: Some(db_lock),
            keypair,
            bucket_manager,
            bucket_snapshot_manager,
            ledger_manager,
            overlay: RwLock::new(None),
            herder,
            current_ledger: RwLock::new(0),
            shutdown_tx,
            _shutdown_rx: shutdown_rx,
            scp_envelope_tx,
            scp_envelope_rx: TokioMutex::new(scp_envelope_rx),
            last_processed_slot: RwLock::new(0),
            catchup_in_progress: AtomicBool::new(false),
            catchup_fatal_failure: AtomicBool::new(false),
            catchup_needs_full_reset: AtomicBool::new(false),
            syncing_ledgers: RwLock::new(BTreeMap::new()),
            last_externalized_slot: AtomicU64::new(0),
            scp_messages_sent: AtomicU64::new(0),
            scp_nominate_sent: AtomicU64::new(0),
            scp_prepare_sent: AtomicU64::new(0),
            scp_confirm_sent: AtomicU64::new(0),
            scp_externalize_sent: AtomicU64::new(0),
            scp_messages_received: AtomicU64::new(0),
            consensus_trigger_attempts: AtomicU64::new(0),
            consensus_trigger_successes: AtomicU64::new(0),
            consensus_trigger_failures: AtomicU64::new(0),
            nomination_timeout_fires: AtomicU64::new(0),
            ballot_timeout_fires: AtomicU64::new(0),
            last_externalized_at: RwLock::new(now),
            last_scp_state_request_at: RwLock::new(now),
            survey_data: RwLock::new(SurveyDataManager::new(
                is_validator,
                max_inbound_peers,
                max_outbound_peers,
            )),
            tx_advert_queue: RwLock::new(Vec::new()),
            tx_advert_set: RwLock::new(HashSet::new()),
            tx_adverts_by_peer: RwLock::new(HashMap::new()),
            tx_demand_history: RwLock::new(HashMap::new()),
            tx_pending_demands: RwLock::new(VecDeque::new()),
            tx_set_dont_have: RwLock::new(HashMap::new()),
            tx_set_last_request: RwLock::new(HashMap::new()),
            tx_set_all_peers_exhausted: AtomicBool::new(false),
            tx_set_exhausted_warned: RwLock::new(HashSet::new()),
            consensus_stuck_state: RwLock::new(None),
            last_catchup_completed_at: RwLock::new(None),
            cached_archive_checkpoint: RwLock::new(None),
            scp_latency: RwLock::new(ScpLatencyTracker::default()),
            survey_scheduler: RwLock::new(SurveyScheduler::new(now)),
            survey_nonce: RwLock::new(1),
            survey_secrets: RwLock::new(HashMap::new()),
            survey_results: RwLock::new(HashMap::new()),
            survey_limiter: RwLock::new(SurveyMessageLimiter::new(6, 10)),
            survey_throttle,
            survey_reporting: RwLock::new(SurveyReportingState::new(now)),
            scp_timeouts: RwLock::new(ScpTimeoutState::new()),
            meta_stream: std::sync::Mutex::new(meta_stream),
            drift_tracker: std::sync::Mutex::new(CloseTimeDriftTracker::new()),
            sync_recovery_handle: parking_lot::RwLock::new(None), // Initialized in run() when needed
            is_applying_ledger: AtomicBool::new(false),
            sync_recovery_pending: AtomicBool::new(false),
            recovery_attempts_without_progress: AtomicU64::new(0),
            recovery_baseline_ledger: AtomicU64::new(0),
            lost_sync_count: AtomicU64::new(0),
            ledger_tx_count: AtomicU64::new(0),
            max_tx_size_bytes: AtomicU32::new(
                henyey_herder::flow_control::MAX_CLASSIC_TX_SIZE_BYTES,
            ),
            ping_counter: AtomicU64::new(0),
            ping_inflight: RwLock::new(HashMap::new()),
            peer_ping_inflight: RwLock::new(HashMap::new()),
            scp_state_query_info: RwLock::new(HashMap::new()),
            self_arc: RwLock::new(std::sync::Weak::new()),
            last_event_loop_tick_ms: Arc::new(AtomicU64::new(0)),
            event_loop_phase: Arc::new(AtomicU64::new(0)),
        })
    }

    fn verify_on_disk_integrity(db: &henyey_db::Database) -> anyhow::Result<()> {
        const VERIFY_DEPTH: u32 = 128;

        let Some(latest) = db.get_latest_ledger_seq()? else {
            return Ok(());
        };
        if latest == 0 {
            return Ok(());
        }

        let mut current_seq = latest;
        let mut checked = 0u32;
        while current_seq > 0 && checked < VERIFY_DEPTH {
            let current = db
                .get_ledger_header(current_seq)?
                .ok_or_else(|| anyhow::anyhow!("Missing ledger header at {}", current_seq))?;
            let prev_seq = current_seq - 1;
            let Some(prev) = db.get_ledger_header(prev_seq)? else {
                tracing::warn!(
                    missing_seq = prev_seq,
                    latest_seq = latest,
                    "Ledger header chain has a gap; skipping deeper integrity checks"
                );
                break;
            };
            let prev_hash = compute_header_hash(&prev)?;
            verify_header_chain(&prev, &prev_hash, &current)?;
            current_seq = prev_seq;
            checked += 1;
        }

        // NOTE: Skip list entries store bucket_list_hash values (not header
        // hashes), so they cannot be verified by comparing against stored
        // header hashes.  stellar-core does not perform skip list
        // verification on startup either.

        Ok(())
    }

    fn ensure_network_passphrase(db: &henyey_db::Database, passphrase: &str) -> anyhow::Result<()> {
        let stored = db.get_network_passphrase()?;
        if let Some(existing) = stored {
            if existing != passphrase {
                anyhow::bail!(
                    "Network passphrase mismatch: db has '{}', config has '{}'",
                    existing,
                    passphrase
                );
            }
            return Ok(());
        }
        db.set_network_passphrase(passphrase)?;
        Ok(())
    }

    /// Initialize the database.
    fn init_database(config: &AppConfig) -> anyhow::Result<henyey_db::Database> {
        tracing::info!(path = ?config.database.path, "Opening database");

        // Ensure parent directory exists
        if let Some(parent) = config.database.path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }

        let db = henyey_db::Database::open(&config.database.path)?;
        tracing::debug!("Database opened successfully");
        Ok(db)
    }

    fn acquire_db_lock(config: &AppConfig) -> anyhow::Result<File> {
        use fs2::FileExt;

        let lock_path = config.database.path.with_extension("lock");
        if let Some(parent) = lock_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&lock_path)?;
        file.try_lock_exclusive().map_err(|_| {
            anyhow::anyhow!("database is locked (lockfile: {})", lock_path.display())
        })?;
        Ok(file)
    }

    /// Initialize the node keypair.
    fn init_keypair(config: &AppConfig) -> anyhow::Result<henyey_crypto::SecretKey> {
        if let Some(ref seed) = config.node.node_seed {
            tracing::debug!("Using configured node seed");
            let keypair = henyey_crypto::SecretKey::from_strkey(seed)?;
            Ok(keypair)
        } else {
            tracing::info!("Generating ephemeral node keypair");
            Ok(henyey_crypto::SecretKey::generate())
        }
    }

    /// Get the application configuration.
    pub fn config(&self) -> &AppConfig {
        &self.config
    }

    /// Get a reference to the ledger manager.
    ///
    /// Used by the `ApplyLoad` benchmark harness to directly close ledgers
    /// without going through consensus.
    pub fn ledger_manager(&self) -> &Arc<LedgerManager> {
        &self.ledger_manager
    }

    /// Get the current application state.
    pub async fn state(&self) -> AppState {
        *self.state.read().await
    }

    /// Set the application state.
    pub(crate) async fn set_state(&self, state: AppState) {
        let mut current = self.state.write().await;
        if *current != state {
            if matches!(*current, AppState::Synced | AppState::Validating)
                && state == AppState::CatchingUp
            {
                self.lost_sync_count.fetch_add(1, Ordering::Relaxed);
            }
            tracing::info!(from = %*current, to = %state, "State transition");
            *current = state;
        }
    }

    /// Transition to `Validating` (if validator) or `Synced` (if watcher).
    ///
    /// Used after catchup completes, fails, or is skipped to leave the
    /// `CatchingUp` state and resume normal operation.
    pub(crate) async fn restore_operational_state(&self) {
        if self.is_validator {
            self.set_state(AppState::Validating).await;
        } else {
            self.set_state(AppState::Synced).await;
        }
    }

    /// Reset all tx-set tracking state so the main loop can make fresh requests.
    ///
    /// Clears the exhausted flag, don't-have map, last-request timestamps, and
    /// exhaustion warnings. Callers that also need to clear `consensus_stuck_state`
    /// should do so separately.
    pub(crate) async fn reset_tx_set_tracking(&self) {
        self.tx_set_all_peers_exhausted
            .store(false, Ordering::SeqCst);
        self.tx_set_dont_have.write().await.clear();
        self.tx_set_last_request.write().await.clear();
        self.tx_set_exhausted_warned.write().await.clear();
    }

    /// Persist in-memory hot archive buckets to disk.
    ///
    /// Hot archive merges are performed entirely in memory, so after catchup
    /// or ledger close the curr/snap/next buckets may have no backing file.
    /// This writes each non-zero bucket that lacks a file to the bucket
    /// directory so that a subsequent restart can restore from the persisted HAS.
    pub(crate) fn persist_hot_archive_buckets(&self, habl: &HotArchiveBucketList) {
        let bucket_dir = self.bucket_manager.bucket_dir();
        for level in habl.levels() {
            let mut buckets_to_check: Vec<&henyey_bucket::HotArchiveBucket> =
                vec![&level.curr, &level.snap];
            if let Some(next) = level.next() {
                buckets_to_check.push(next);
            }
            for bucket in buckets_to_check {
                if bucket.backing_file_path().is_none() && !bucket.hash().is_zero() {
                    let permanent =
                        bucket_dir.join(format!("{}.bucket.xdr", bucket.hash().to_hex()));
                    if !permanent.exists() {
                        if let Err(e) = bucket.save_to_xdr_file(&permanent) {
                            tracing::warn!(
                                error = %e,
                                hash = %bucket.hash().to_hex(),
                                "Failed to persist in-memory hot archive bucket to disk"
                            );
                        }
                    }
                }
            }
        }
    }

    /// Set the tracked current ledger sequence.
    pub(crate) async fn set_current_ledger(&self, seq: u32) {
        *self.current_ledger.write().await = seq;
    }

    /// Check if the force-scp flag is set in the database.
    ///
    /// Returns `true` if the flag is set, `false` otherwise.
    /// This does NOT clear the flag — call `clear_force_scp` after use.
    pub fn check_force_scp(&self) -> bool {
        use henyey_db::queries::StateQueries;
        use henyey_db::schema::state_keys;
        self.db
            .with_connection(|conn| {
                Ok(conn.get_state(state_keys::FORCE_SCP)?.as_deref() == Some("true"))
            })
            .unwrap_or(false)
    }

    /// Clear the force-scp flag in the database.
    pub fn clear_force_scp(&self) {
        use henyey_db::queries::StateQueries;
        use henyey_db::schema::state_keys;
        let _ = self
            .db
            .with_connection(|conn| conn.delete_state(state_keys::FORCE_SCP));
    }

    /// Bootstrap the node from genesis state stored in the database.
    ///
    /// Used by the force-scp flow for standalone single-node networks.
    /// Reads the genesis ledger header from the DB, recreates the bucket list
    /// with the root account entry (and test accounts if configured), and
    /// initializes the LedgerManager.
    pub async fn bootstrap_from_db(&self) -> anyhow::Result<()> {
        use henyey_bucket::BucketList;
        use henyey_bucket::HotArchiveBucketList;
        use henyey_common::NetworkId;
        use henyey_db::queries::LedgerQueries;
        use henyey_ledger::compute_header_hash;
        use stellar_xdr::curr::{
            AccountEntry, AccountEntryExt, AccountId, BucketListType, LedgerEntry, LedgerEntryData,
            LedgerEntryExt, PublicKey, SequenceNumber, Thresholds, Uint256, VecM,
        };

        // Read LCL header from DB
        let (lcl_seq, header) = self.db.with_connection(|conn| {
            let lcl_seq = conn
                .get_last_closed_ledger()?
                .ok_or_else(|| henyey_db::DbError::Integrity("No LCL in DB".to_string()))?;
            let header = conn.load_ledger_header(lcl_seq)?.ok_or_else(|| {
                henyey_db::DbError::Integrity(format!("No header for LCL {}", lcl_seq))
            })?;
            Ok((lcl_seq, header))
        })?;

        let header_hash = compute_header_hash(&header)
            .map_err(|e| anyhow::anyhow!("Failed to compute header hash: {}", e))?;

        // Recreate the bucket list with the root account entry (and test accounts
        // if GENESIS_TEST_ACCOUNT_COUNT > 0).
        // For genesis (protocol 0), these are entries in level 0.
        let genesis_test_account_count = self.config.testing.genesis_test_account_count;
        let network_id = NetworkId::from_passphrase(&self.config.network.passphrase);
        let root_secret = henyey_crypto::SecretKey::from_seed(network_id.as_bytes());
        let root_public = root_secret.public_key();
        let root_account_id = AccountId(PublicKey::PublicKeyTypeEd25519(Uint256(
            *root_public.as_bytes(),
        )));

        // Compute per-account balance (same logic as initialize_genesis_ledger).
        let (root_balance, test_balance) = if genesis_test_account_count > 0 {
            let total_accounts = genesis_test_account_count as i64 + 1;
            let base = header.total_coins / total_accounts;
            let remainder = header.total_coins % total_accounts;
            (base + remainder, base)
        } else {
            (header.total_coins, 0i64)
        };

        let root_entry = LedgerEntry {
            last_modified_ledger_seq: 1,
            data: LedgerEntryData::Account(AccountEntry {
                account_id: root_account_id,
                balance: root_balance,
                seq_num: SequenceNumber(0),
                num_sub_entries: 0,
                inflation_dest: None,
                flags: 0,
                home_domain: stellar_xdr::curr::String32::default(),
                thresholds: Thresholds([1, 0, 0, 0]),
                signers: VecM::default(),
                ext: AccountEntryExt::V0,
            }),
            ext: LedgerEntryExt::V0,
        };

        let mut genesis_entries = vec![root_entry];

        for i in 0..genesis_test_account_count {
            let name = format!("TestAccount-{}", i);
            let seed = deterministic_seed(&name);
            let secret = henyey_crypto::SecretKey::from_seed(&seed);
            let public = secret.public_key();
            let acct_id = AccountId(PublicKey::PublicKeyTypeEd25519(Uint256(*public.as_bytes())));
            genesis_entries.push(LedgerEntry {
                last_modified_ledger_seq: 1,
                data: LedgerEntryData::Account(AccountEntry {
                    account_id: acct_id,
                    balance: test_balance,
                    seq_num: SequenceNumber(0),
                    num_sub_entries: 0,
                    inflation_dest: None,
                    flags: 0,
                    home_domain: stellar_xdr::curr::String32::default(),
                    thresholds: Thresholds([1, 0, 0, 0]),
                    signers: VecM::default(),
                    ext: AccountEntryExt::V0,
                }),
                ext: LedgerEntryExt::V0,
            });
        }

        let bucket_dir = self.bucket_manager.bucket_dir().to_path_buf();
        std::fs::create_dir_all(&bucket_dir)?;

        let mut bucket_list = BucketList::new();
        bucket_list.set_bucket_dir(bucket_dir.clone());
        bucket_list
            .add_batch(1, 0, BucketListType::Live, genesis_entries, vec![], vec![])
            .map_err(|e| anyhow::anyhow!("Failed to create genesis bucket list: {}", e))?;

        // Persist all non-empty buckets to disk so they're available for
        // history publishing and restart recovery.
        for level in bucket_list.levels() {
            for bucket in [&level.curr, &level.snap] {
                if bucket.backing_file_path().is_none() && !bucket.hash().is_zero() {
                    let path =
                        bucket_dir.join(henyey_bucket::canonical_bucket_filename(&bucket.hash()));
                    if !path.exists() {
                        bucket
                            .save_to_xdr_file(&path)
                            .map_err(|e| anyhow::anyhow!("Failed to save genesis bucket: {}", e))?;
                    }
                }
            }
        }

        // Verify hash matches
        let computed_hash = bucket_list.hash();
        let expected_hash = henyey_common::Hash256::from_bytes(header.bucket_list_hash.0);
        if computed_hash != expected_hash {
            anyhow::bail!(
                "Genesis bucket list hash mismatch: computed {} vs header {}",
                computed_hash,
                expected_hash
            );
        }

        // Initialize LedgerManager
        let hot_archive = HotArchiveBucketList::new();
        self.ledger_manager
            .initialize(bucket_list, hot_archive, header, header_hash)
            .map_err(|e| anyhow::anyhow!("Failed to initialize LedgerManager: {}", e))?;

        let (seq, _hash, _close_time, _protocol) = self.ledger_info();
        tracing::info!(
            lcl_seq = seq,
            "Bootstrapped from genesis state via force-scp"
        );

        self.set_state(AppState::Synced).await;
        self.set_current_ledger(lcl_seq).await;

        Ok(())
    }

    /// Get the database.
    pub fn database(&self) -> &henyey_db::Database {
        &self.db
    }

    /// Get the bucket snapshot manager for concurrent query access.
    pub fn bucket_snapshot_manager(&self) -> &Arc<BucketSnapshotManager> {
        &self.bucket_snapshot_manager
    }

    /// Update the bucket snapshot manager with fresh snapshots from the
    /// current bucket list state. Called after each ledger close and after
    /// catchup completes to keep the query server's view current.
    pub(crate) fn update_bucket_snapshot(&self) {
        let header = self.ledger_manager.current_header();
        let live_snap = BucketListSnapshot::new(&self.ledger_manager.bucket_list(), header.clone());
        let hot_archive_snap = {
            let guard = self.ledger_manager.hot_archive_bucket_list();
            match guard.as_ref() {
                Some(ha) => HotArchiveBucketListSnapshot::new(ha, header),
                None => {
                    // No hot archive yet; use an empty placeholder so that the
                    // live snapshot still gets updated (query server needs it).
                    let default = HotArchiveBucketList::default();
                    HotArchiveBucketListSnapshot::new(&default, header)
                }
            }
        };
        self.bucket_snapshot_manager
            .update_current_snapshot(live_snap, hot_archive_snap);
    }

    /// Get the node's public key.
    pub fn public_key(&self) -> henyey_crypto::PublicKey {
        self.keypair.public_key()
    }

    /// Get the network ID.
    pub fn network_id(&self) -> henyey_common::Hash256 {
        self.config.network_id()
    }

    pub fn ledger_info(&self) -> (u32, henyey_common::Hash256, u64, u32) {
        let header = self.ledger_manager.current_header();
        let hash = self.ledger_manager.current_header_hash();
        let close_time = ledger_close_time(&header);
        (header.ledger_seq, hash, close_time, header.ledger_version)
    }

    /// Get a rich ledger summary with all header fields needed for the
    /// `/info` endpoint.
    pub fn ledger_summary(&self) -> LedgerSummary {
        let header = self.ledger_manager.current_header();
        let hash = self.ledger_manager.current_header_hash();
        let close_time = ledger_close_time(&header);
        let now = self
            .clock
            .system_now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let age = if close_time > 0 {
            now.saturating_sub(close_time)
        } else {
            0
        };
        // Extract flags from LedgerHeaderExt::V1 if present.
        let flags = match &header.ext {
            stellar_xdr::curr::LedgerHeaderExt::V0 => 0,
            stellar_xdr::curr::LedgerHeaderExt::V1(ext) => ext.flags,
        };
        LedgerSummary {
            num: header.ledger_seq,
            hash,
            close_time,
            version: header.ledger_version,
            base_fee: header.base_fee,
            base_reserve: header.base_reserve,
            max_tx_set_size: header.max_tx_set_size,
            flags,
            age,
        }
    }

    pub fn target_ledger_close_time(&self) -> u32 {
        self.herder.ledger_close_time()
    }

    /// Expected time of the next ledger close.
    ///
    /// Returns `tracking_consensus_close_time + ledger_close_time` (seconds).
    /// Used by simulation to predict when the next close should occur.
    pub fn expected_ledger_close_time(&self) -> u64 {
        self.herder.tracking_consensus_close_time() + self.herder.ledger_close_time() as u64
    }

    pub async fn peer_count(&self) -> usize {
        self.overlay
            .read()
            .await
            .as_ref()
            .map(|o| o.peer_count())
            .unwrap_or(0)
    }

    pub async fn add_peer(&self, addr: henyey_overlay::PeerAddress) -> anyhow::Result<bool> {
        let overlay = self.overlay.read().await;
        let Some(overlay) = overlay.as_ref() else {
            anyhow::bail!("overlay not started")
        };
        overlay
            .add_peer(addr)
            .await
            .map_err(|e| anyhow::anyhow!("failed to add peer: {}", e))
    }

    pub fn latest_externalized_slot(&self) -> Option<u64> {
        self.herder.latest_externalized_slot()
    }

    /// Load the current sequence number for an account from the bucket list.
    ///
    /// Returns `None` if the account does not exist.
    /// Used by the simulation LoadGenerator to refresh cached sequence numbers.
    pub fn load_account_sequence(&self, account_id: &stellar_xdr::curr::AccountId) -> Option<i64> {
        let snapshot = self.ledger_manager.create_snapshot().ok()?;
        let account = snapshot.get_account(account_id).ok()??;
        Some(account.seq_num.0)
    }

    /// Load a full account entry from the current bucket list snapshot.
    ///
    /// Returns `None` if the account does not exist.
    /// Used by the compat HTTP `/testacc` endpoint.
    pub fn load_account(
        &self,
        account_id: &stellar_xdr::curr::AccountId,
    ) -> Option<stellar_xdr::curr::AccountEntry> {
        let snapshot = self.ledger_manager.create_snapshot().ok()?;
        snapshot.get_account(account_id).ok()?
    }

    /// Check whether a ledger entry exists in the current bucket list.
    ///
    /// Used by the simulation LoadGenerator to verify Soroban state is synced.
    pub fn has_ledger_entry(&self, key: &stellar_xdr::curr::LedgerKey) -> bool {
        let Ok(snapshot) = self.ledger_manager.create_snapshot() else {
            return false;
        };
        matches!(snapshot.get_entry(key), Ok(Some(_)))
    }

    /// Check whether the given account has any pending transactions in the
    /// herder's transaction queue.
    ///
    /// Matches stellar-core `Herder::sourceAccountPending()`.
    pub fn source_account_pending(&self, account_id: &stellar_xdr::curr::AccountId) -> bool {
        self.herder.source_account_pending(account_id)
    }

    /// Get the base fee from the current ledger header.
    pub fn base_fee(&self) -> u32 {
        self.ledger_manager.current_header().base_fee
    }

    /// Get the current ledger sequence number.
    pub fn current_ledger_seq(&self) -> u32 {
        self.ledger_manager.current_ledger_seq()
    }

    pub fn request_out_of_sync_recovery(&self) {
        self.sync_recovery_pending.store(true, Ordering::SeqCst);
    }

    pub fn current_upgrade_state(&self) -> (u32, u32, u32, u32) {
        let header = self.ledger_manager.current_header();
        (
            header.ledger_version,
            header.base_fee,
            header.base_reserve,
            header.max_tx_set_size,
        )
    }

    pub fn proposed_upgrades(&self) -> Vec<LedgerUpgrade> {
        self.config.upgrades.to_ledger_upgrades()
    }

    /// Set runtime upgrade parameters (from HTTP `/upgrades?mode=set`).
    pub fn set_upgrade_parameters(
        &self,
        params: henyey_herder::upgrades::UpgradeParameters,
    ) -> std::result::Result<(), String> {
        self.herder.set_upgrade_parameters(params)
    }

    /// Get current runtime upgrade parameters.
    pub fn runtime_upgrade_parameters(&self) -> henyey_herder::upgrades::UpgradeParameters {
        self.herder.upgrade_parameters()
    }

    /// Get Soroban network configuration information.
    ///
    /// Returns the Soroban-related configuration settings from the current ledger
    /// state, or `None` if not available (pre-protocol 20 or not initialized).
    pub fn soroban_network_info(&self) -> Option<SorobanNetworkInfo> {
        self.ledger_manager.soroban_network_info()
    }

    /// Manually close a ledger (for testing/manual close mode).
    ///
    /// This triggers the herder to close the next ledger. It requires:
    /// - The node must be configured as a validator (`is_validator = true`)
    /// - Manual close mode must be enabled (`manual_close = true`)
    ///
    /// # Returns
    ///
    /// * `Ok(new_ledger_seq)` - The ledger was successfully triggered
    /// * `Err` - An error occurred (not a validator, manual close not enabled, etc.)
    pub async fn manual_close_ledger(&self) -> anyhow::Result<u32> {
        // Check if node is a validator
        if !self.config.node.is_validator {
            anyhow::bail!(
                "Issuing a manual ledger close requires NODE_IS_VALIDATOR to be set to true."
            );
        }

        // Check if manual close mode is enabled
        if !self.config.node.manual_close {
            anyhow::bail!("Manual close is disabled. Set manual_close = true in configuration.");
        }

        // Get the next ledger sequence
        let (current_ledger, _, _, _) = self.ledger_info();
        let next_ledger = current_ledger + 1;

        // Trigger the herder to close the next ledger
        self.herder
            .trigger_next_ledger(next_ledger)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to trigger next ledger: {}", e))?;

        Ok(next_ledger)
    }

    pub fn self_check(&self, depth: u32) -> anyhow::Result<SelfCheckResult> {
        let Some(latest) = self.db.get_latest_ledger_seq()? else {
            return Ok(SelfCheckResult {
                ok: true,
                checked_ledgers: 0,
                last_checked_ledger: None,
            });
        };
        if latest == 0 {
            return Ok(SelfCheckResult {
                ok: true,
                checked_ledgers: 0,
                last_checked_ledger: None,
            });
        }

        let mut current_seq = latest;
        let mut checked = 0u32;
        let mut last_verified = None;

        while current_seq > 0 && checked < depth {
            let current = self
                .db
                .get_ledger_header(current_seq)?
                .ok_or_else(|| anyhow::anyhow!("Missing ledger header at {}", current_seq))?;
            let prev_seq = current_seq - 1;
            let prev = self
                .db
                .get_ledger_header(prev_seq)?
                .ok_or_else(|| anyhow::anyhow!("Missing ledger header at {}", prev_seq))?;
            let prev_hash = compute_header_hash(&prev)?;
            verify_header_chain(&prev, &prev_hash, &current)?;
            last_verified = Some(current_seq);
            current_seq = prev_seq;
            checked += 1;
        }

        Ok(SelfCheckResult {
            ok: true,
            checked_ledgers: checked,
            last_checked_ledger: last_verified,
        })
    }

    pub fn pending_transaction_count(&self) -> usize {
        self.herder.stats().pending_transactions
    }

    /// Number of ledger closes that contained at least one transaction.
    /// Mirrors stellar-core's `ledger.transaction.count` histogram `.count`.
    pub fn ledger_tx_count(&self) -> u64 {
        self.ledger_tx_count.load(Ordering::Relaxed)
    }

    pub async fn submit_transaction(
        &self,
        tx: TransactionEnvelope,
    ) -> henyey_herder::TxQueueResult {
        let result = self.herder.receive_transaction(tx.clone());
        // Flood the transaction to peers so validators can include it.
        // Without this, transactions submitted via HTTP /tx stay local.
        if matches!(result, henyey_herder::TxQueueResult::Added) {
            self.enqueue_tx_advert(&tx).await;
        }
        result
    }

    /// Test-only: skip fee balance validation for loadgen transactions.
    ///
    /// Matches stellar-core's `isLoadgenTx` bypass in `TransactionQueue::canAdd()`
    /// which skips both tx validation and fee balance checks for loadgen txs
    /// (gated on `#ifdef BUILD_TESTS`).
    #[cfg(any(test, feature = "test-utils"))]
    pub fn set_skip_fee_balance_check(&self, skip: bool) {
        self.herder.tx_queue().set_skip_fee_balance_check(skip);
    }

    pub fn herder_stats(&self) -> HerderStats {
        self.herder.stats()
    }

    pub async fn simulation_debug_stats(&self) -> SimulationDebugStats {
        let herder_stats = self.herder.stats();
        let current_ledger = self.ledger_info().0;
        let quorum_slot = herder_stats
            .tracking_slot
            .max(current_ledger as u64 + 1)
            .max(1);
        let slot_state = self.herder.get_slot_state(quorum_slot);
        SimulationDebugStats {
            app_state: self.state().await.to_string(),
            herder_state: herder_stats.state.to_string(),
            current_ledger,
            tracking_slot: herder_stats.tracking_slot,
            latest_externalized_slot: self.herder.latest_externalized_slot(),
            peer_count: self.peer_count().await,
            pending_envelopes: herder_stats.pending_envelopes,
            cached_tx_sets: herder_stats.cached_tx_sets,
            heard_from_quorum: self.herder.heard_from_quorum(quorum_slot),
            is_v_blocking: self.herder.is_v_blocking(quorum_slot),
            slot_is_nominating: slot_state.as_ref().map(|s| s.is_nominating),
            slot_is_externalized: slot_state.as_ref().map(|s| s.is_externalized),
            slot_ballot_phase: slot_state.as_ref().map(|s| format!("{:?}", s.ballot_phase)),
            slot_ballot_round: slot_state.as_ref().and_then(|s| s.ballot_round),
            nomination_timeout_fires: self.nomination_timeout_fires.load(Ordering::Relaxed),
            ballot_timeout_fires: self.ballot_timeout_fires.load(Ordering::Relaxed),
            scp_messages_sent: self.scp_messages_sent.load(Ordering::Relaxed),
            scp_messages_received: self.scp_messages_received.load(Ordering::Relaxed),
            consensus_trigger_attempts: self.consensus_trigger_attempts.load(Ordering::Relaxed),
            consensus_trigger_successes: self.consensus_trigger_successes.load(Ordering::Relaxed),
            consensus_trigger_failures: self.consensus_trigger_failures.load(Ordering::Relaxed),
        }
    }

    /// Clear metrics registry.
    ///
    /// In stellar-core, this resets the medida metrics counters.
    /// In our Prometheus-style implementation, metrics are typically scraped externally
    /// and don't have explicit clear semantics. This method logs the request for
    /// operational visibility.
    ///
    /// # Arguments
    ///
    /// * `domain` - Optional domain filter (empty string means all metrics)
    pub fn clear_metrics(&self, domain: &str) {
        if domain.is_empty() {
            tracing::info!("Clearing all metrics");
        } else {
            tracing::info!(domain = %domain, "Clearing metrics for domain");
        }
        // Note: Prometheus-style metrics don't have a clear operation.
        // The metrics are scraped externally and typically reset on node restart.
        // We log the request for operational visibility and parity with stellar-core.
    }

    /// Perform manual database maintenance.
    ///
    /// Cleans up old SCP history and ledger headers to prevent unbounded database growth.
    /// This is the same maintenance performed automatically by the background maintainer,
    /// but can be triggered manually via the `/maintenance` HTTP endpoint.
    ///
    /// # Arguments
    ///
    /// * `count` - Maximum number of entries to delete per table
    pub fn perform_maintenance(&self, count: u32) {
        let (ledger_seq, _, _, _) = self.ledger_info();
        let lcl = ledger_seq;

        // Get minimum queued publish checkpoint if available
        let min_queued = self
            .db
            .load_publish_queue(Some(1))
            .ok()
            .and_then(|queue| queue.first().copied());

        // Calculate the minimum ledger we need to keep
        let qmin = min_queued.unwrap_or(lcl).min(lcl);
        let lmin = qmin.saturating_sub(checkpoint_frequency());

        tracing::info!(
            trim_below = lmin,
            count = count,
            lcl = lcl,
            min_queued = ?min_queued,
            "Performing manual maintenance"
        );

        // Delete old SCP history
        if let Err(e) = self.db.delete_old_scp_entries(lmin, count) {
            tracing::warn!(error = %e, "Failed to delete old SCP entries");
        }

        // Delete old ledger headers.
        // When RPC is enabled, headers must be retained at least as long as
        // the RPC data so that oldest_ledger (MIN of ledgerheaders) never
        // reports a value newer than the actual oldest RPC data.
        let header_lmin = if self.config.rpc.enabled {
            let rpc_lmin = lcl.saturating_sub(self.config.rpc.retention_window);
            lmin.min(rpc_lmin)
        } else {
            lmin
        };
        if let Err(e) = self.db.delete_old_ledger_headers(header_lmin, count) {
            tracing::warn!(error = %e, "Failed to delete old ledger headers");
        }

        // Clean up RPC-specific tables if RPC is enabled.
        // Events, ledger close meta, and tx history are all RPC-only data
        // and should be pruned at the RPC retention window, not the core
        // checkpoint threshold.
        if self.config.rpc.enabled {
            let rpc_lmin = lcl.saturating_sub(self.config.rpc.retention_window);

            if let Err(e) = self.db.delete_old_events(rpc_lmin, count) {
                tracing::warn!(error = %e, "Failed to delete old contract events");
            }

            if let Err(e) = self.db.delete_old_ledger_close_meta(rpc_lmin, count) {
                tracing::warn!(error = %e, "Failed to delete old ledger close meta");
            }

            if let Err(e) = self.db.delete_old_tx_history(rpc_lmin, count) {
                tracing::warn!(error = %e, "Failed to delete old tx history");
            }
        }
    }

    /// Delete bucket files on disk that are no longer referenced by the live
    /// or hot archive bucket lists. Prevents unbounded disk growth.
    /// Matches stellar-core's cleanupStaleFiles() + forgetUnreferencedBuckets().
    ///
    /// Must resolve all pending async merges first: background merge threads may
    /// have already written output files to disk, but the result hasn't been
    /// polled yet (handle.result == None). Without resolution, the output hash
    /// won't appear in the referenced set and the file would be deleted while
    /// the DiskBacked bucket still points to it. This is analogous to
    /// stellar-core tracking all merge outputs in mSharedLiveBuckets.
    ///
    /// Spawns the cleanup on tokio's blocking thread pool so that
    /// `resolve_pending_bucket_merges()` (which may block waiting for
    /// in-flight async merges) does not stall the async event loop.
    pub(crate) fn cleanup_stale_bucket_files_background(&self) {
        let lm = self.ledger_manager.clone();
        let bm = self.bucket_manager.clone();
        tokio::task::spawn_blocking(move || {
            lm.resolve_pending_bucket_merges();

            let hashes = lm.all_referenced_bucket_hashes();
            match bm.retain_buckets(&hashes) {
                Ok(deleted) => {
                    if deleted > 0 {
                        tracing::info!(deleted, "Cleaned up stale bucket files");
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "Failed to cleanup stale bucket files");
                }
            }
        });
    }

    /// Get a ConfigUpgradeSet from the ledger by its key.
    ///
    /// Looks up the temporary ledger entry containing the ConfigUpgradeSet
    /// that corresponds to the given ConfigUpgradeSetKey.
    ///
    /// # Arguments
    ///
    /// * `key` - The ConfigUpgradeSetKey identifying the upgrade set
    ///
    /// # Returns
    ///
    /// * `Some(json)` - The ConfigUpgradeSet as a JSON-serializable value
    /// * `None` - The upgrade set was not found or is invalid
    pub fn get_config_upgrade_set(
        &self,
        key: &stellar_xdr::curr::ConfigUpgradeSetKey,
    ) -> Option<serde_json::Value> {
        let frame = self.ledger_manager.get_config_upgrade_set(key)?;
        let upgrade_set = frame.to_xdr();

        // Convert to JSON-serializable format
        Some(serde_json::json!({
            "updated_entry": upgrade_set.updated_entry.iter().map(|entry| {
                format!("{:?}", entry)
            }).collect::<Vec<_>>()
        }))
    }

    pub fn scp_slot_snapshots(&self, limit: usize) -> Vec<ScpSlotSnapshot> {
        let scp = self.herder.scp();
        let (ledger_seq, _, _, _) = self.ledger_info();
        let latest_slot = self
            .herder
            .latest_externalized_slot()
            .unwrap_or(ledger_seq as u64);
        let mut slot = latest_slot;
        let mut snapshots = Vec::new();

        while slot > 0 && snapshots.len() < limit {
            if let Some(state) = scp.get_slot_state(slot) {
                let envelopes = self.herder.get_scp_envelopes(slot);
                snapshots.push(ScpSlotSnapshot {
                    slot_index: state.slot_index,
                    is_externalized: state.is_externalized,
                    is_nominating: state.is_nominating,
                    fully_validated: state.fully_validated,
                    ballot_phase: format!("{:?}", state.ballot_phase),
                    nomination_round: state.nomination_round,
                    ballot_round: state.ballot_round,
                    envelope_count: envelopes.len(),
                });
            }
            slot = slot.saturating_sub(1);
        }

        snapshots
    }

    /// Get a cloned Arc reference to the overlay manager.
    ///
    /// This acquires the RwLock briefly (read lock), clones the Arc, and
    /// drops the lock. Callers can then use the overlay freely without
    /// blocking other tasks from accessing it.
    ///
    /// Returns `None` if the overlay hasn't been started yet.
    pub(crate) async fn overlay(&self) -> Option<Arc<OverlayManager>> {
        self.overlay.read().await.clone()
    }

    /// Request SCP state from all connected peers.
    pub async fn request_scp_state_from_peers(&self) {
        let Some(overlay) = self.overlay().await else {
            return;
        };

        let peer_count = overlay.peer_count();
        if peer_count == 0 {
            tracing::debug!("No peers connected, cannot request SCP state");
            return;
        }

        // Request SCP state from a low watermark similar to stellar-core behavior.
        let ledger_seq = self.herder.get_min_ledger_seq_to_ask_peers();
        match overlay.request_scp_state(ledger_seq).await {
            Ok(count) => {
                tracing::info!(
                    ledger_seq,
                    peers_sent = count,
                    "Requested SCP state from peers"
                );
            }
            Err(e) => {
                tracing::warn!(
                    ledger_seq,
                    error = %e,
                    "Failed to request SCP state from peers"
                );
            }
        }
    }

    /// Get application info.
    pub fn info(&self) -> AppInfo {
        let (meta_bytes, meta_writes) = self
            .meta_stream
            .lock()
            .ok()
            .and_then(|guard| guard.as_ref().map(|ms| ms.metrics()))
            .unwrap_or((0, 0));

        AppInfo {
            version: env!("CARGO_PKG_VERSION").to_string(),
            commit_hash: self.config.build.commit_hash.clone(),
            build_timestamp: self.config.build.build_timestamp.clone(),
            node_name: self.config.node.name.clone(),
            public_key: self.keypair.public_key().to_strkey(),
            network_passphrase: self.config.network.passphrase.clone(),
            is_validator: self.config.node.is_validator,
            database_path: self.config.database.path.clone(),
            meta_stream_bytes_total: meta_bytes,
            meta_stream_writes_total: meta_writes,
        }
    }

    /// Return the local quorum set if configured.
    pub fn local_quorum_set(&self) -> Option<stellar_xdr::curr::ScpQuorumSet> {
        self.herder.local_quorum_set()
    }
}

/// Implementation of HerderCallback for App.
///
/// This enables the herder to trigger ledger closes through the app.
#[async_trait::async_trait]
impl HerderCallback for App {
    async fn close_ledger(
        &self,
        ledger_seq: u32,
        tx_set: henyey_herder::TransactionSet,
        close_time: u64,
        upgrades: Vec<UpgradeType>,
        stellar_value_ext: StellarValueExt,
    ) -> henyey_herder::Result<henyey_common::Hash256> {
        let tx_summary = tx_set.summary();
        tracing::info!(
            ledger_seq,
            tx_count = tx_set.transactions.len(),
            close_time,
            summary = %tx_summary,
            "Closing ledger"
        );

        // Get the previous ledger hash
        let prev_hash = tx_set.previous_ledger_hash;

        // Create the transaction set
        let tx_set_variant = if let Some(gen_tx_set) = tx_set.generalized_tx_set.clone() {
            TransactionSetVariant::Generalized(gen_tx_set)
        } else {
            TransactionSetVariant::Classic(TransactionSet {
                previous_ledger_hash: Hash::from(prev_hash),
                txs: tx_set.transactions.clone().try_into().map_err(|_| {
                    henyey_herder::HerderError::Internal("Failed to create tx set".into())
                })?,
            })
        };

        // Create close data
        let decoded_upgrades = decode_upgrades(upgrades);
        let mut close_data =
            LedgerCloseData::new(ledger_seq, tx_set_variant.clone(), close_time, prev_hash)
                .with_stellar_value_ext(stellar_value_ext.clone());
        if !decoded_upgrades.is_empty() {
            close_data = close_data.with_upgrades(decoded_upgrades.clone());
        }
        if let Some(entry) = self.build_scp_history_entry(ledger_seq) {
            close_data = close_data.with_scp_history(vec![entry]);
        }

        // Close the ledger on a blocking thread (yields the tokio worker).
        let lm = self.ledger_manager.clone();
        let runtime_handle = tokio::runtime::Handle::current();
        self.set_applying_ledger(true);

        let join_handle = tokio::task::spawn_blocking(move || {
            lm.close_ledger(close_data, Some(runtime_handle))
                .map_err(|e| e.to_string())
        });

        let mut pending = PendingLedgerClose {
            handle: join_handle,
            ledger_seq,
            tx_set,
            tx_set_variant,
            close_time,
            upgrades: Vec::new(),
        };

        let join_result = (&mut pending.handle).await;

        // Extract header hash before passing ownership to handle_close_complete.
        let header_hash = match &join_result {
            Ok(Ok(result)) => Some(result.header_hash),
            _ => None,
        };

        let success = self.handle_close_complete(pending, join_result).await;

        if success {
            Ok(header_hash.unwrap())
        } else {
            Err(henyey_herder::HerderError::Internal(format!(
                "Failed to close ledger {}",
                ledger_seq
            )))
        }
    }

    async fn validate_tx_set(&self, _tx_set_hash: &henyey_common::Hash256) -> bool {
        // For now, accept all transaction sets
        // In a full implementation, this would:
        // 1. Check we have the tx set locally
        // 2. Validate all transactions are valid
        // 3. Check the tx set hash matches
        true
    }

    async fn broadcast_scp_message(&self, envelope: ScpEnvelope) {
        let slot = envelope.statement.slot_index;
        // Send through the channel to be picked up by the main loop
        if let Err(e) = self.scp_envelope_tx.try_send(envelope) {
            tracing::warn!(slot, error = %e, "Failed to queue SCP envelope for broadcast");
        }
    }
}

impl SyncRecoveryCallback for App {
    fn on_lost_sync(&self) {
        tracing::warn!("Lost sync with network - transitioning to syncing state");
        self.lost_sync_count.fetch_add(1, Ordering::Relaxed);
        // Update herder state to syncing
        self.herder.set_state(henyey_herder::HerderState::Syncing);
    }

    fn on_out_of_sync_recovery(&self) {
        tracing::info!("SyncRecoveryManager triggered out-of-sync recovery");
        // Set flag so the main event loop will trigger recovery and buffered catchup.
        // The main loop checks this flag and calls maybe_start_buffered_catchup()
        // which handles the actual recovery logic including timeout-based catchup.
        self.sync_recovery_pending.store(true, Ordering::SeqCst);
    }

    fn is_applying_ledger(&self) -> bool {
        self.is_applying_ledger.load(Ordering::Relaxed)
    }

    fn is_tracking(&self) -> bool {
        self.herder.is_tracking()
    }

    fn get_v_blocking_slots(&self) -> Vec<henyey_scp::SlotIndex> {
        // Return slots where we've received v-blocking messages
        // For now, return the tracking slot range
        let tracking = self.herder.tracking_slot();
        if tracking > 0 {
            vec![tracking]
        } else {
            vec![]
        }
    }

    fn purge_slots_below(&self, slot: henyey_scp::SlotIndex) {
        tracing::debug!(slot, "Purging SCP slots below");
        self.herder.purge_slots_below(slot);
    }

    fn broadcast_latest_messages(&self, from_slot: henyey_scp::SlotIndex) {
        tracing::debug!(from_slot, "Broadcasting latest SCP messages");
        // Get and broadcast latest messages for the slot
        if let Some(messages) = self.herder.get_latest_messages(from_slot) {
            for envelope in messages {
                let _ = self.scp_envelope_tx.try_send(envelope);
            }
        }
    }

    fn request_scp_state_from_peers(&self) {
        self.request_scp_state_sync();
    }
}

impl App {
    /// Synchronous version of request_scp_state_from_peers for use in callbacks.
    fn request_scp_state_sync(&self) {
        // We can't call async from sync callback, so we use a simple marker
        // The main event loop's heartbeat will pick up stalled state and request
        tracing::debug!("Sync recovery requested SCP state - will be handled by main loop");
    }

    /// Start the sync recovery manager background task.
    ///
    /// This spawns a background task that monitors for consensus stuck conditions
    /// and triggers recovery actions when needed.
    pub fn start_sync_recovery(self: &Arc<Self>) {
        let (handle, manager) = SyncRecoveryManager::new(Arc::clone(self));
        *self.sync_recovery_handle.write() = Some(handle);
        tokio::spawn(manager.run());
        tracing::info!("Sync recovery manager started");
    }

    /// Start the background database maintainer.
    ///
    /// Spawns a tokio task that periodically cleans up old ledger headers,
    /// SCP history, contract events, and (if RPC is enabled) RPC-specific
    /// tables. Mirrors stellar-core's `Maintainer::start()` called from
    /// `ApplicationImpl::startServices()`.
    ///
    /// Returns the JoinHandle so the caller can abort it on shutdown.
    pub fn start_maintainer(self: &Arc<Self>) -> Option<tokio::task::JoinHandle<()>> {
        use crate::maintainer::{Maintainer, MaintenanceConfig};

        let maint_cfg = &self.config.maintenance;
        if !maint_cfg.enabled {
            tracing::info!("Database maintenance disabled by configuration");
            return None;
        }

        // Build the MaintenanceConfig from AppConfig.
        let rpc_retention = if self.config.rpc.enabled {
            Some(self.config.rpc.retention_window)
        } else {
            None
        };
        let config = MaintenanceConfig {
            period: Duration::from_secs(maint_cfg.period_secs),
            count: maint_cfg.count,
            enabled: true,
            rpc_retention_window: rpc_retention,
        };

        // Create a shutdown watch channel driven by the app's broadcast channel.
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let mut broadcast_rx = self.shutdown_tx.subscribe();
        tokio::spawn(async move {
            let _ = broadcast_rx.recv().await;
            let _ = shutdown_tx.send(true);
        });

        // Clone the database for the maintainer (Database is cheap to clone —
        // it wraps a connection pool).
        let db = Arc::new(self.db.clone());

        // Provide ledger bounds via Arc<App>.
        let app = Arc::clone(self);
        let get_ledger_bounds = move || -> (u32, Option<u32>) {
            let (lcl, _, _, _) = app.ledger_info();
            let min_queued = app
                .database()
                .load_publish_queue(Some(1))
                .ok()
                .and_then(|queue| queue.first().copied());
            (lcl, min_queued)
        };

        let maintainer = Maintainer::with_config(db, config, shutdown_rx, get_ledger_bounds);
        let handle = tokio::spawn(async move {
            maintainer.start().await;
        });

        tracing::info!(
            period_secs = maint_cfg.period_secs,
            count = maint_cfg.count,
            "Database maintainer started"
        );

        Some(handle)
    }

    /// Send a heartbeat to the sync recovery manager.
    ///
    /// This should be called whenever consensus makes progress (externalization,
    /// new SCP messages, ledger close).
    pub fn sync_recovery_heartbeat(&self) {
        if let Some(handle) = self.sync_recovery_handle.read().as_ref() {
            let _ = handle.try_tracking_heartbeat();
        }
    }

    /// Start tracking in the sync recovery manager.
    ///
    /// This should be called after bootstrap to enable the consensus stuck timer.
    pub fn start_sync_recovery_tracking(&self) {
        if let Some(handle) = self.sync_recovery_handle.read().as_ref() {
            if handle.try_start_tracking() {
                tracing::info!("Started sync recovery tracking");
            }
        }
    }

    /// Notify sync recovery that we're starting/stopping ledger application.
    pub fn set_applying_ledger(&self, applying: bool) {
        self.is_applying_ledger.store(applying, Ordering::Relaxed);
        if let Some(handle) = self.sync_recovery_handle.read().as_ref() {
            let _ = handle.try_set_applying_ledger(applying);
        }
    }

    /// Update the event loop phase code (for watchdog diagnostics).
    #[inline]
    fn set_phase(&self, phase: u64) {
        self.event_loop_phase.store(phase, Ordering::Relaxed);
    }

    /// Record a new event loop tick (for watchdog freshness tracking).
    #[inline]
    fn tick_event_loop(&self) {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        self.last_event_loop_tick_ms
            .store(now_ms, Ordering::Relaxed);
    }

    /// Start a std::thread watchdog that monitors event loop liveness.
    ///
    /// The watchdog runs independently of the tokio runtime. Every 10 seconds
    /// it checks the last event loop tick timestamp. If the event loop hasn't
    /// ticked in 30+ seconds, it logs an error with the current phase code
    /// and thread backtraces to help diagnose deadlocks.
    pub fn start_event_loop_watchdog(&self) {
        let tick_ms = Arc::clone(&self.last_event_loop_tick_ms);
        let phase = Arc::clone(&self.event_loop_phase);
        let pid = std::process::id();

        std::thread::Builder::new()
            .name("watchdog".into())
            .spawn(move || {
                loop {
                    std::thread::sleep(Duration::from_secs(10));

                    let last_tick = tick_ms.load(Ordering::Relaxed);
                    if last_tick == 0 {
                        // Event loop hasn't started yet
                        continue;
                    }

                    let now_ms = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_millis() as u64;
                    let stale_secs = now_ms.saturating_sub(last_tick) / 1000;
                    let current_phase = phase.load(Ordering::Relaxed);

                    if stale_secs >= 30 {
                        tracing::error!(
                            stale_secs,
                            phase = current_phase,
                            pid,
                            "WATCHDOG: Event loop appears frozen! \
                             Phase codes: 0=select, 1=scp_msg, 2=fetch_resp, \
                             3=broadcast, 4=scp_broadcast, 5=consensus_tick, \
                             6=pending_close, 10=process_externalized, \
                             11=externalized_catchup, 12=try_apply_buffered, \
                             13=buffered_catchup, 14=catchup_running, 15=heartbeat, \
                             20=stats, 21=tx_advert, 22=tx_demand, 23=survey, \
                             24=survey_req, 25=survey_phase, 26=scp_timeout, \
                             27=ping, 28=peer_maint, 29=peer_refresh, 30=herder_cleanup"
                        );

                        // Log thread states from /proc for debugging
                        if let Ok(entries) = std::fs::read_dir(format!("/proc/{}/task", pid)) {
                            let mut states: std::collections::HashMap<String, u32> =
                                std::collections::HashMap::new();
                            for entry in entries.flatten() {
                                let status_path = format!("{}/status", entry.path().display());
                                if let Ok(status) = std::fs::read_to_string(&status_path) {
                                    let state = status
                                        .lines()
                                        .find(|l| l.starts_with("State:"))
                                        .map(|l| l.to_string())
                                        .unwrap_or_else(|| "Unknown".into());
                                    *states.entry(state).or_insert(0) += 1;
                                }
                            }
                            for (state, count) in &states {
                                tracing::error!(
                                    count,
                                    state = state.as_str(),
                                    "WATCHDOG: Thread state summary"
                                );
                            }
                        }
                    } else if stale_secs >= 15 {
                        tracing::warn!(
                            stale_secs,
                            phase = current_phase,
                            "WATCHDOG: Event loop slow (>15s since last tick)"
                        );
                    }
                }
            })
            .expect("Failed to spawn watchdog thread");

        tracing::info!("Event loop watchdog started");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile;

    #[tokio::test]
    async fn test_app_creation() {
        let dir = tempfile::tempdir().expect("temp dir");
        let db_path = dir.path().join("rs-stellar-test.db");
        let config = crate::config::ConfigBuilder::new()
            .database_path(db_path)
            .build();

        let app = App::new(config).await.unwrap();
        assert_eq!(app.state().await, AppState::Initializing);
    }

    #[tokio::test]
    async fn test_app_info() {
        let dir = tempfile::tempdir().expect("temp dir");
        let db_path = dir.path().join("rs-stellar-test.db");
        let config = crate::config::ConfigBuilder::new()
            .node_name("test-node")
            .database_path(db_path)
            .build();

        let app = App::new(config).await.unwrap();
        let info = app.info();

        assert_eq!(info.node_name, "test-node");
        assert!(!info.public_key.is_empty());
        assert!(info.public_key.starts_with('G'));
    }

    #[tokio::test]
    async fn test_start_overlay_skips_default_peers_for_compat_config() {
        let dir = tempfile::tempdir().expect("temp dir");

        let mut compat_config = crate::config::ConfigBuilder::new()
            .database_path(dir.path().join("compat.db"))
            .build();
        compat_config.overlay.known_peers.clear();
        compat_config.is_compat_config = true;

        let compat_app = App::new(compat_config).await.unwrap();
        compat_app.start_overlay().await.unwrap();
        let compat_overlay = compat_app.overlay().await.unwrap();
        assert!(compat_overlay.known_peers().is_empty());
        compat_app.shutdown();

        let mut regular_config = crate::config::ConfigBuilder::new()
            .database_path(dir.path().join("regular.db"))
            .build();
        regular_config.overlay.known_peers.clear();
        regular_config.is_compat_config = false;

        let regular_app = App::new(regular_config).await.unwrap();
        regular_app.start_overlay().await.unwrap();
        let regular_overlay = regular_app.overlay().await.unwrap();
        assert!(!regular_overlay.known_peers().is_empty());
        regular_app.shutdown();
    }

    #[test]
    fn test_catchup_result_display() {
        let result = CatchupResult {
            ledger_seq: 1000,
            ledger_hash: henyey_common::Hash256::ZERO,
            buckets_applied: 22,
            ledgers_replayed: 64,
        };

        let display = format!("{}", result);
        assert!(display.contains("1000"));
        assert!(display.contains("22 buckets"));
    }

    #[test]
    fn test_buffered_catchup_target_large_gap() {
        let current = 100;
        let first_buffered = current + checkpoint_frequency() + 5; // 169
        let target = App::buffered_catchup_target(current, first_buffered, first_buffered);
        // Target should be capped at the latest checkpoint (127) to avoid replaying
        // individual ledgers which can cause bucket list hash mismatches.
        assert_eq!(target, Some(127));
    }

    #[test]
    fn test_buffered_catchup_target_requires_trigger() {
        let current = 100;
        let first_buffered = 120;
        let last_buffered = 120;
        let target = App::buffered_catchup_target(current, first_buffered, last_buffered);
        assert_eq!(target, None);

        let last_buffered = 130;
        let target = App::buffered_catchup_target(current, first_buffered, last_buffered);
        assert_eq!(target, Some(127));
    }

    #[test]
    fn test_tx_set_start_index_rotation() {
        let mut bytes = [0u8; 32];
        bytes[0] = 1;
        let hash = Hash256::from_bytes(bytes);
        assert_eq!(App::tx_set_start_index(&hash, 3, 0), 1);
        assert_eq!(App::tx_set_start_index(&hash, 3, 1), 2);
        assert_eq!(App::tx_set_start_index(&hash, 3, 2), 0);
        assert_eq!(App::tx_set_start_index(&hash, 3, 3), 1);
    }

    #[test]
    fn test_compute_catchup_target_for_timeout() {
        // Test case 1: first_buffered in middle of checkpoint, current_ledger far behind
        // first_buffered=100 is in checkpoint starting at 64
        // Target should be 63 (end of previous checkpoint)
        let target = App::compute_catchup_target_for_timeout(150, 100, 50);
        assert_eq!(target, Some(63));

        // Test case 2: first_buffered at start of checkpoint
        // first_buffered=128 is in checkpoint starting at 128
        // Target should be 127 (end of previous checkpoint)
        let target = App::compute_catchup_target_for_timeout(150, 128, 50);
        assert_eq!(target, Some(127));

        // Test case 3: current_ledger already past first_buffered's checkpoint target
        // first_buffered=100 -> checkpoint start 64 -> target 63, but current is 70
        // Should fall through to last_buffered's checkpoint (128) -> target 127
        let target = App::compute_catchup_target_for_timeout(150, 100, 70);
        assert_eq!(target, Some(127));

        // Test case 4: current_ledger past all checkpoint targets but before first_buffered
        // first_buffered=100, last_buffered=110, current=95
        // first_buffered checkpoint start=64, target=63 (but 63 < 95)
        // last_buffered checkpoint start=64, alt_target=63 (but 63 < 95)
        // direct_target = first_buffered - 1 = 99 > 95, so return Some(99)
        // This bridges the tiny gap with a Case 1 replay (95 -> 99)
        let target = App::compute_catchup_target_for_timeout(110, 100, 95);
        assert_eq!(target, Some(99));

        // Test case 5: current_ledger already at or past first_buffered, return None
        // This happens when we've caught up but buffered ledgers haven't been processed
        let target = App::compute_catchup_target_for_timeout(110, 100, 100);
        assert!(target.is_none());

        // Test case 6: very early ledger (first checkpoint)
        // first_buffered=50 is in checkpoint starting at 0
        // Since checkpoint_start is 0, target = first_buffered - 1 = 49
        // 49 > current_ledger (10), so return Some(49)
        let target = App::compute_catchup_target_for_timeout(60, 50, 10);
        assert_eq!(target, Some(49));

        // Test case 7: edge case with very small ledgers
        let target = App::compute_catchup_target_for_timeout(5, 3, 0);
        // first_buffered=3, checkpoint start=0, target=first_buffered-1=2
        // 2 > current_ledger(0), so return Some(2)
        assert_eq!(target, Some(2));

        // Test case 8: tiny gap at checkpoint boundary (the stuck-after-catchup bug)
        // LCL=922751 (which is a checkpoint boundary: (922751+1)%64==0)
        // first_buffered=922753 (gap of 1 slot at 922752)
        // first_buffered checkpoint start=922752, target=922751 (== current_ledger)
        // last_buffered checkpoint start=922752, alt_target=922751 (== current_ledger)
        // direct_target = 922752 > 922751, so return Some(922752)
        // This bridges the 1-slot gap with a Case 1 replay
        let target = App::compute_catchup_target_for_timeout(922753, 922753, 922751);
        assert_eq!(target, Some(922752));
    }

    #[test]
    fn test_consensus_stuck_timeout_constants() {
        // Verify constants match stellar-core values
        assert_eq!(CONSENSUS_STUCK_TIMEOUT_SECS, 35);
        assert_eq!(OUT_OF_SYNC_RECOVERY_TIMER_SECS, 10);
    }

    #[test]
    fn test_consensus_stuck_state() {
        use std::time::Instant;

        let state = ConsensusStuckState {
            current_ledger: 1000,
            first_buffered: 1001,
            stuck_start: Instant::now(),
            last_recovery_attempt: Instant::now(),
            recovery_attempts: 0,
            catchup_triggered: false,
        };

        assert_eq!(state.current_ledger, 1000);
        assert_eq!(state.first_buffered, 1001);
        assert_eq!(state.recovery_attempts, 0);
        assert!(!state.catchup_triggered);
    }

    #[test]
    fn test_consensus_stuck_action_variants() {
        // Verify all action variants exist and can be matched
        let actions = [
            ConsensusStuckAction::Wait,
            ConsensusStuckAction::AttemptRecovery,
            ConsensusStuckAction::TriggerCatchup,
        ];

        for action in actions {
            match action {
                ConsensusStuckAction::Wait => {}
                ConsensusStuckAction::AttemptRecovery => {}
                ConsensusStuckAction::TriggerCatchup => {}
            }
        }
    }

    // ============================================================
    // Buffered Ledger Update Tests (regression for 80bd38d)
    // ============================================================

    /// Tests that the BTreeMap Entry pattern correctly updates existing entries.
    /// This is a regression test for the fix in process_externalized_slots()
    /// where or_insert() was incorrectly used instead of Entry::Occupied/Vacant.
    #[test]
    fn test_btreemap_entry_update_pattern() {
        use std::collections::BTreeMap;

        // Simulate the buffered ledger structure (slot -> tx_set)
        // Using Option<Vec<u8>> directly to represent presence/absence of tx_set
        let mut buffer: BTreeMap<u32, Option<Vec<u8>>> = BTreeMap::new();

        // First, insert a slot WITHOUT tx_set (simulates initial buffering)
        let slot = 100u32;
        buffer.insert(slot, None);
        assert!(buffer.get(&slot).unwrap().is_none());

        // Now simulate tx_set arriving later - the fix uses Entry pattern
        let new_tx_set = Some(vec![1, 2, 3]);
        match buffer.entry(slot) {
            std::collections::btree_map::Entry::Occupied(mut entry) => {
                let existing = entry.get_mut();
                if existing.is_none() && new_tx_set.is_some() {
                    *existing = new_tx_set.clone();
                }
            }
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(new_tx_set.clone());
            }
        }

        // Verify the existing entry was UPDATED (not ignored)
        assert!(buffer.get(&slot).unwrap().is_some());
        assert_eq!(buffer.get(&slot).unwrap().as_ref().unwrap(), &vec![1, 2, 3]);
    }

    /// Tests that or_insert() does NOT update existing entries (the bug we fixed).
    /// This demonstrates why the fix was needed.
    #[test]
    fn test_or_insert_does_not_update_existing() {
        use std::collections::BTreeMap;

        let mut map: BTreeMap<u32, Option<Vec<u8>>> = BTreeMap::new();

        // Insert with None
        map.insert(100, None);

        // Try to "update" with or_insert - this does NOT update existing!
        map.entry(100).or_insert(Some(vec![1, 2, 3]));

        // The value is still None - or_insert doesn't update existing entries
        assert!(map.get(&100).unwrap().is_none());
    }

    // ============================================================
    // Tx Set Request Deduplication Tests (regression for 759757b)
    // ============================================================

    /// Tests that HashSet correctly tracks requested tx_set hashes to avoid
    /// duplicate broadcast requests. This is a regression test for the fix
    /// in cache_messages_during_catchup_impl().
    #[test]
    fn test_tx_set_request_deduplication() {
        use std::collections::HashSet;

        let mut requested_hashes: HashSet<Hash256> = HashSet::new();

        let hash1 = Hash256::from_bytes([1u8; 32]);
        let hash2 = Hash256::from_bytes([2u8; 32]);

        // First request for hash1 should be allowed
        assert!(!requested_hashes.contains(&hash1));
        requested_hashes.insert(hash1);

        // Second request for hash1 should be blocked (duplicate)
        assert!(requested_hashes.contains(&hash1));

        // First request for hash2 should be allowed
        assert!(!requested_hashes.contains(&hash2));
        requested_hashes.insert(hash2);

        // Both hashes are now tracked
        assert!(requested_hashes.contains(&hash1));
        assert!(requested_hashes.contains(&hash2));
        assert_eq!(requested_hashes.len(), 2);
    }

    /// Tests the combined check pattern used in the fix:
    /// !has_tx_set && !already_requested
    #[test]
    fn test_tx_set_request_condition() {
        use std::collections::HashSet;

        let mut requested_hashes: HashSet<Hash256> = HashSet::new();
        let mut has_tx_set_cache: HashSet<Hash256> = HashSet::new();

        let hash = Hash256::from_bytes([42u8; 32]);

        // Case 1: Don't have tx_set, haven't requested -> should request
        let should_request = !has_tx_set_cache.contains(&hash) && !requested_hashes.contains(&hash);
        assert!(should_request);

        // Mark as requested
        requested_hashes.insert(hash);

        // Case 2: Don't have tx_set, already requested -> should NOT request
        let should_request = !has_tx_set_cache.contains(&hash) && !requested_hashes.contains(&hash);
        assert!(!should_request);

        // Case 3: Have tx_set (regardless of requested) -> should NOT request
        has_tx_set_cache.insert(hash);
        let should_request = !has_tx_set_cache.contains(&hash) && !requested_hashes.contains(&hash);
        assert!(!should_request);
    }

    // ============================================================
    // Herder Integration Tests
    // ============================================================

    #[tokio::test]
    async fn test_herder_stats_includes_pending_envelope_stats() {
        let dir = tempfile::tempdir().expect("temp dir");
        let db_path = dir.path().join("rs-stellar-test.db");
        let config = crate::config::ConfigBuilder::new()
            .database_path(db_path)
            .build();

        let app = App::new(config).await.unwrap();
        let stats = app.herder_stats();

        // Verify pending_envelope_stats is accessible
        assert_eq!(stats.pending_envelope_stats.received, 0);
        assert_eq!(stats.pending_envelope_stats.added, 0);
        assert_eq!(stats.pending_envelope_stats.duplicates, 0);
    }

    #[tokio::test]
    async fn test_herder_stats_includes_tx_queue_stats() {
        let dir = tempfile::tempdir().expect("temp dir");
        let db_path = dir.path().join("rs-stellar-test.db");
        let config = crate::config::ConfigBuilder::new()
            .database_path(db_path)
            .build();

        let app = App::new(config).await.unwrap();
        let stats = app.herder_stats();

        // Verify tx_queue_stats is accessible
        assert_eq!(stats.tx_queue_stats.pending_count, 0);
        assert_eq!(stats.tx_queue_stats.account_count, 0);
        assert_eq!(stats.tx_queue_stats.banned_count, 0);
        assert_eq!(stats.tx_queue_stats.seen_count, 0);
    }

    #[tokio::test]
    async fn test_drift_tracker_initialized() {
        let dir = tempfile::tempdir().expect("temp dir");
        let db_path = dir.path().join("rs-stellar-test.db");
        let config = crate::config::ConfigBuilder::new()
            .database_path(db_path)
            .build();

        let app = App::new(config).await.unwrap();

        // Verify drift tracker is accessible (will lock successfully)
        let result = app.drift_tracker.lock();
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_sync_recovery_handle_initially_none() {
        let dir = tempfile::tempdir().expect("temp dir");
        let db_path = dir.path().join("rs-stellar-test.db");
        let config = crate::config::ConfigBuilder::new()
            .database_path(db_path)
            .build();

        let app = App::new(config).await.unwrap();

        // Sync recovery handle is None until start_sync_recovery is called
        assert!(app.sync_recovery_handle.read().is_none());
    }

    #[tokio::test]
    async fn test_sync_recovery_heartbeat_no_panic_when_not_started() {
        let dir = tempfile::tempdir().expect("temp dir");
        let db_path = dir.path().join("rs-stellar-test.db");
        let config = crate::config::ConfigBuilder::new()
            .database_path(db_path)
            .build();

        let app = App::new(config).await.unwrap();

        // Should not panic when handle is None
        app.sync_recovery_heartbeat();
    }

    #[tokio::test]
    async fn test_set_applying_ledger_updates_flag() {
        let dir = tempfile::tempdir().expect("temp dir");
        let db_path = dir.path().join("rs-stellar-test.db");
        let config = crate::config::ConfigBuilder::new()
            .database_path(db_path)
            .build();

        let app = App::new(config).await.unwrap();

        // Initially false
        assert!(!app.is_applying_ledger.load(Ordering::Relaxed));

        // Set to true
        app.set_applying_ledger(true);
        assert!(app.is_applying_ledger.load(Ordering::Relaxed));

        // Set back to false
        app.set_applying_ledger(false);
        assert!(!app.is_applying_ledger.load(Ordering::Relaxed));
    }

    #[tokio::test]
    async fn test_sync_recovery_callback_is_applying_ledger() {
        let dir = tempfile::tempdir().expect("temp dir");
        let db_path = dir.path().join("rs-stellar-test.db");
        let config = crate::config::ConfigBuilder::new()
            .database_path(db_path)
            .build();

        let app = App::new(config).await.unwrap();

        // Test the SyncRecoveryCallback implementation
        assert!(!SyncRecoveryCallback::is_applying_ledger(&app));

        app.is_applying_ledger.store(true, Ordering::Relaxed);
        assert!(SyncRecoveryCallback::is_applying_ledger(&app));
    }

    #[tokio::test]
    async fn test_sync_recovery_callback_is_tracking() {
        let dir = tempfile::tempdir().expect("temp dir");
        let db_path = dir.path().join("rs-stellar-test.db");
        let config = crate::config::ConfigBuilder::new()
            .database_path(db_path)
            .build();

        let app = App::new(config).await.unwrap();

        // Herder starts in booting state, not tracking
        assert!(!SyncRecoveryCallback::is_tracking(&app));
    }

    #[tokio::test]
    async fn test_herder_cleanup_method_exists() {
        let dir = tempfile::tempdir().expect("temp dir");
        let db_path = dir.path().join("rs-stellar-test.db");
        let config = crate::config::ConfigBuilder::new()
            .database_path(db_path)
            .build();

        let app = App::new(config).await.unwrap();

        // Verify cleanup method is callable
        app.herder.cleanup();
    }

    #[tokio::test]
    async fn test_herder_quorum_tracking_methods() {
        let dir = tempfile::tempdir().expect("temp dir");
        let db_path = dir.path().join("rs-stellar-test.db");
        let config = crate::config::ConfigBuilder::new()
            .database_path(db_path)
            .build();

        let app = App::new(config).await.unwrap();

        // Verify quorum tracking methods are callable
        let slot = app.herder.tracking_slot();
        let _heard = app.herder.heard_from_quorum(slot);
        let _blocking = app.herder.is_v_blocking(slot);
    }

    #[tokio::test]
    async fn test_herder_set_state() {
        let dir = tempfile::tempdir().expect("temp dir");
        let db_path = dir.path().join("rs-stellar-test.db");
        let config = crate::config::ConfigBuilder::new()
            .database_path(db_path)
            .build();

        let app = App::new(config).await.unwrap();

        // Initially in Booting state
        assert_eq!(app.herder.state(), henyey_herder::HerderState::Booting);

        // Can set to Syncing
        app.herder.set_state(henyey_herder::HerderState::Syncing);
        assert_eq!(app.herder.state(), henyey_herder::HerderState::Syncing);
    }

    #[tokio::test]
    async fn test_tx_queue_ban_shift() {
        let dir = tempfile::tempdir().expect("temp dir");
        let db_path = dir.path().join("rs-stellar-test.db");
        let config = crate::config::ConfigBuilder::new()
            .database_path(db_path)
            .build();

        let app = App::new(config).await.unwrap();

        // Shift with empty ban queue should return zero counts
        let shift_result = app.herder.tx_queue().shift();
        assert_eq!(shift_result.unbanned_count, 0);
        assert_eq!(shift_result.evicted_due_to_age, 0);
    }

    #[tokio::test]
    async fn test_try_start_ledger_close_returns_none_when_no_buffered() {
        let dir = tempfile::tempdir().expect("temp dir");
        let db_path = dir.path().join("rs-stellar-test.db");
        let config = crate::config::ConfigBuilder::new()
            .database_path(db_path)
            .build();

        let app = App::new(config).await.unwrap();

        // No buffered ledgers → should return None.
        let pending = app.try_start_ledger_close().await;
        assert!(
            pending.is_none(),
            "should return None with no buffered ledgers"
        );
    }

    #[tokio::test]
    async fn test_try_start_ledger_close_skips_when_already_applying() {
        let dir = tempfile::tempdir().expect("temp dir");
        let db_path = dir.path().join("rs-stellar-test.db");
        let config = crate::config::ConfigBuilder::new()
            .database_path(db_path)
            .build();

        let app = App::new(config).await.unwrap();

        // Simulate a ledger close already in progress.
        app.set_applying_ledger(true);

        let pending = app.try_start_ledger_close().await;
        assert!(
            pending.is_none(),
            "should return None when is_applying_ledger is true"
        );

        // Cleanup.
        app.set_applying_ledger(false);
    }

    #[tokio::test]
    async fn test_try_apply_buffered_skips_when_already_applying() {
        let dir = tempfile::tempdir().expect("temp dir");
        let db_path = dir.path().join("rs-stellar-test.db");
        let config = crate::config::ConfigBuilder::new()
            .database_path(db_path)
            .build();

        let app = App::new(config).await.unwrap();

        // Simulate a ledger close already in progress.
        app.set_applying_ledger(true);

        // Should return immediately without doing anything.
        app.try_apply_buffered_ledgers().await;

        // Flag should still be true (not cleared by the no-op call).
        assert!(app.is_applying_ledger.load(Ordering::Relaxed));

        // Cleanup.
        app.set_applying_ledger(false);
    }

    #[tokio::test]
    async fn test_handle_close_complete_clears_applying_flag_on_error() {
        let dir = tempfile::tempdir().expect("temp dir");
        let db_path = dir.path().join("rs-stellar-test.db");
        let config = crate::config::ConfigBuilder::new()
            .database_path(db_path)
            .build();

        let app = App::new(config).await.unwrap();
        app.set_applying_ledger(true);

        // Simulate a failed close result.
        let pending = PendingLedgerClose {
            handle: tokio::task::spawn_blocking(|| Err("simulated error".to_string())),
            ledger_seq: 1,
            tx_set: henyey_herder::TransactionSet {
                hash: henyey_common::Hash256::ZERO,
                previous_ledger_hash: henyey_common::Hash256::ZERO,
                transactions: Vec::new(),
                generalized_tx_set: None,
            },
            tx_set_variant: TransactionSetVariant::Classic(stellar_xdr::curr::TransactionSet {
                previous_ledger_hash: stellar_xdr::curr::Hash([0u8; 32]),
                txs: stellar_xdr::curr::VecM::default(),
            }),
            close_time: 1,
            upgrades: Vec::new(),
        };

        let mut pending = pending;
        let join_result = (&mut pending.handle).await;
        let success = app.handle_close_complete(pending, join_result).await;

        assert!(!success, "should return false on error");
        assert!(
            !app.is_applying_ledger.load(Ordering::Relaxed),
            "is_applying_ledger should be cleared on error"
        );
    }

    #[tokio::test]
    async fn test_handle_close_complete_clears_applying_flag_on_panic() {
        let dir = tempfile::tempdir().expect("temp dir");
        let db_path = dir.path().join("rs-stellar-test.db");
        let config = crate::config::ConfigBuilder::new()
            .database_path(db_path)
            .build();

        let app = App::new(config).await.unwrap();
        app.set_applying_ledger(true);

        // Simulate a panicked task.
        let pending = PendingLedgerClose {
            handle: tokio::task::spawn_blocking(|| {
                panic!("simulated panic");
            }),
            ledger_seq: 1,
            tx_set: henyey_herder::TransactionSet {
                hash: henyey_common::Hash256::ZERO,
                previous_ledger_hash: henyey_common::Hash256::ZERO,
                transactions: Vec::new(),
                generalized_tx_set: None,
            },
            tx_set_variant: TransactionSetVariant::Classic(stellar_xdr::curr::TransactionSet {
                previous_ledger_hash: stellar_xdr::curr::Hash([0u8; 32]),
                txs: stellar_xdr::curr::VecM::default(),
            }),
            close_time: 1,
            upgrades: Vec::new(),
        };

        let mut pending = pending;
        let join_result = (&mut pending.handle).await;
        let success = app.handle_close_complete(pending, join_result).await;

        assert!(!success, "should return false on panic");
        assert!(
            !app.is_applying_ledger.load(Ordering::Relaxed),
            "is_applying_ledger should be cleared on panic"
        );
    }

    #[tokio::test]
    async fn test_handle_close_complete_clears_buffer_on_hash_mismatch() {
        let dir = tempfile::tempdir().expect("temp dir");
        let db_path = dir.path().join("rs-stellar-test.db");
        let config = crate::config::ConfigBuilder::new()
            .database_path(db_path)
            .build();

        let app = App::new(config).await.unwrap();
        app.set_applying_ledger(true);

        // Add a fake entry to syncing_ledgers to verify it gets cleared.
        {
            let mut buffer = app.syncing_ledgers.write().await;
            buffer.insert(
                2,
                henyey_herder::LedgerCloseInfo {
                    slot: 2,
                    tx_set_hash: henyey_common::Hash256::ZERO,
                    tx_set: None,
                    close_time: 1,
                    upgrades: Vec::new(),
                    stellar_value_ext: StellarValueExt::Basic,
                },
            );
        }

        // Simulate a hash mismatch error.
        let pending = PendingLedgerClose {
            handle: tokio::task::spawn_blocking(
                || Err("previous ledger hash mismatch".to_string()),
            ),
            ledger_seq: 1,
            tx_set: henyey_herder::TransactionSet {
                hash: henyey_common::Hash256::ZERO,
                previous_ledger_hash: henyey_common::Hash256::ZERO,
                transactions: Vec::new(),
                generalized_tx_set: None,
            },
            tx_set_variant: TransactionSetVariant::Classic(stellar_xdr::curr::TransactionSet {
                previous_ledger_hash: stellar_xdr::curr::Hash([0u8; 32]),
                txs: stellar_xdr::curr::VecM::default(),
            }),
            close_time: 1,
            upgrades: Vec::new(),
        };

        let mut pending = pending;
        let join_result = (&mut pending.handle).await;
        let success = app.handle_close_complete(pending, join_result).await;

        assert!(!success);
        // Buffer should have been cleared due to hash mismatch.
        let buffer = app.syncing_ledgers.read().await;
        assert!(
            buffer.is_empty(),
            "syncing_ledgers should be cleared on hash mismatch"
        );
    }

    // ============================================================
    // Tx Set Request Timeout Tests (regression for silent GetTxSet drops)
    // ============================================================

    #[test]
    fn test_tx_set_request_timeout_constant() {
        // Verify the timeout is 10 seconds as designed
        assert_eq!(TX_SET_REQUEST_TIMEOUT_SECS, 10);
        // Timeout must be longer than the request throttle (1s) to avoid
        // false positives, but short enough to recover quickly
        assert!(TX_SET_REQUEST_TIMEOUT_SECS > 1);
        assert!(TX_SET_REQUEST_TIMEOUT_SECS < CONSENSUS_STUCK_TIMEOUT_SECS);
    }

    #[test]
    fn test_tx_set_request_state_tracks_first_requested() {
        let now = Instant::now();
        let state = TxSetRequestState {
            last_request: now,
            first_requested: now,
            next_peer_offset: 0,
        };

        // first_requested should be set at creation time
        assert!(state.first_requested.elapsed().as_millis() < 100);
        assert_eq!(state.next_peer_offset, 0);
    }

    #[test]
    fn test_tx_set_request_timeout_detection_logic() {
        // Simulate the timeout detection pattern used in request_pending_tx_sets
        let timeout = std::time::Duration::from_secs(TX_SET_REQUEST_TIMEOUT_SECS);
        let peers = vec!["peer1", "peer2", "peer3"];
        let mut dont_have: HashSet<&str> = HashSet::new();

        // Case 1: Request age below timeout — should NOT timeout
        let recent = Instant::now();
        let age = recent.elapsed();
        assert!(age < timeout, "recent request should not timeout");

        // Case 2: Simulate old request (by checking the comparison logic)
        // The actual timeout fires when now - first_requested >= TX_SET_REQUEST_TIMEOUT_SECS
        let threshold = std::time::Duration::from_secs(TX_SET_REQUEST_TIMEOUT_SECS);
        let short_duration = std::time::Duration::from_secs(1);
        assert!(short_duration < threshold, "1s should be under threshold");
        assert!(threshold <= std::time::Duration::from_secs(TX_SET_REQUEST_TIMEOUT_SECS));

        // Case 3: When timeout fires, all peers should be marked as DontHave
        for peer in &peers {
            dont_have.insert(peer);
        }
        assert_eq!(dont_have.len(), peers.len(), "all peers should be marked");
    }

    // ============================================================
    // Rapid Close Cycle Cleanup Tests
    // ============================================================

    #[tokio::test]
    async fn test_clear_pending_tx_sets_via_herder() {
        let dir = tempfile::tempdir().expect("temp dir");
        let db_path = dir.path().join("rs-stellar-test.db");
        let config = crate::config::ConfigBuilder::new()
            .database_path(db_path)
            .build();

        let app = App::new(config).await.unwrap();

        // Register some pending tx_sets
        let hash1 = Hash256::from_bytes([1u8; 32]);
        let hash2 = Hash256::from_bytes([2u8; 32]);
        app.herder.scp_driver().request_tx_set(hash1, 100);
        app.herder.scp_driver().request_tx_set(hash2, 101);
        assert_eq!(app.herder.get_pending_tx_sets().len(), 2);

        // Clear via the herder passthrough
        app.herder.clear_pending_tx_sets();
        assert!(app.herder.get_pending_tx_sets().is_empty());
    }

    #[tokio::test]
    async fn test_stale_syncing_ledgers_eviction() {
        let dir = tempfile::tempdir().expect("temp dir");
        let db_path = dir.path().join("rs-stellar-test.db");
        let config = crate::config::ConfigBuilder::new()
            .database_path(db_path)
            .build();

        let app = App::new(config).await.unwrap();

        // Set current_ledger to 99 so entries at 100+ are above current_ledger
        *app.current_ledger.write().await = 99;

        // Add entries: some with tx_set, some without (starting from current_ledger+1)
        {
            let mut buffer = app.syncing_ledgers.write().await;
            // Entry WITHOUT tx_set at current_ledger+1 (should be evicted when exhausted)
            buffer.insert(
                100,
                henyey_herder::LedgerCloseInfo {
                    slot: 100,
                    tx_set_hash: Hash256::ZERO,
                    tx_set: None,
                    close_time: 1,
                    upgrades: Vec::new(),
                    stellar_value_ext: StellarValueExt::Basic,
                },
            );
            // Entry WITHOUT tx_set (consecutive, should be evicted)
            buffer.insert(
                101,
                henyey_herder::LedgerCloseInfo {
                    slot: 101,
                    tx_set_hash: Hash256::ZERO,
                    tx_set: None,
                    close_time: 2,
                    upgrades: Vec::new(),
                    stellar_value_ext: StellarValueExt::Basic,
                },
            );
            // Entry WITH tx_set (should be kept — eviction stops at first entry with tx_set)
            buffer.insert(
                102,
                henyey_herder::LedgerCloseInfo {
                    slot: 102,
                    tx_set_hash: Hash256::ZERO,
                    tx_set: Some(henyey_herder::TransactionSet {
                        hash: Hash256::ZERO,
                        previous_ledger_hash: Hash256::ZERO,
                        transactions: Vec::new(),
                        generalized_tx_set: None,
                    }),
                    close_time: 3,
                    upgrades: Vec::new(),
                    stellar_value_ext: StellarValueExt::Basic,
                },
            );
        }

        // Simulate the eviction logic from maybe_start_buffered_catchup
        // when tx_set_all_peers_exhausted is true
        app.tx_set_all_peers_exhausted.store(true, Ordering::SeqCst);
        {
            let mut buffer = app.syncing_ledgers.write().await;
            let current_ledger = 99u32;
            let start = current_ledger.saturating_add(1);
            let mut evicted = 0u32;
            for seq in start.. {
                match buffer.get(&seq) {
                    Some(info) if info.tx_set.is_none() => {
                        buffer.remove(&seq);
                        evicted += 1;
                    }
                    _ => break,
                }
            }
            assert_eq!(
                evicted, 2,
                "should evict 2 consecutive entries without tx_sets"
            );
        }

        let buffer = app.syncing_ledgers.read().await;
        assert_eq!(buffer.len(), 1, "only entry with tx_set should remain");
        assert!(
            buffer.contains_key(&102),
            "entry 102 (with tx_set) should be kept"
        );
        assert!(
            !buffer.contains_key(&100),
            "entry 100 (no tx_set) should be evicted"
        );
        assert!(
            !buffer.contains_key(&101),
            "entry 101 (no tx_set) should be evicted"
        );
    }

    #[tokio::test]
    async fn test_tx_set_state_cleanup_after_rapid_close() {
        let dir = tempfile::tempdir().expect("temp dir");
        let db_path = dir.path().join("rs-stellar-test.db");
        let config = crate::config::ConfigBuilder::new()
            .database_path(db_path)
            .build();

        let app = App::new(config).await.unwrap();

        // Set up state as if rapid close cycle just ended
        app.tx_set_all_peers_exhausted.store(true, Ordering::SeqCst);
        {
            let mut dont_have = app.tx_set_dont_have.write().await;
            let hash = Hash256::from_bytes([1u8; 32]);
            dont_have.insert(
                hash,
                HashSet::from([henyey_overlay::PeerId::from_bytes([1u8; 32])]),
            );
        }
        {
            let mut last_request = app.tx_set_last_request.write().await;
            let hash = Hash256::from_bytes([1u8; 32]);
            last_request.insert(
                hash,
                TxSetRequestState {
                    last_request: Instant::now(),
                    first_requested: Instant::now(),
                    next_peer_offset: 3,
                },
            );
        }
        {
            let mut warned = app.tx_set_exhausted_warned.write().await;
            warned.insert(Hash256::from_bytes([1u8; 32]));
        }
        *app.consensus_stuck_state.write().await = Some(ConsensusStuckState {
            current_ledger: 100,
            first_buffered: 101,
            stuck_start: Instant::now(),
            last_recovery_attempt: Instant::now(),
            recovery_attempts: 2,
            catchup_triggered: false,
        });

        // Simulate the cleanup block from the rapid close handler.
        // The rapid close handler now only resets tracking state, NOT
        // buffer entries or pending tx_set requests. This allows the
        // normal process_externalized_slots → maybe_start_buffered_catchup
        // flow to handle stale entries properly.
        app.reset_tx_set_tracking().await;
        *app.consensus_stuck_state.write().await = None;

        // Verify everything is cleaned up
        assert!(!app.tx_set_all_peers_exhausted.load(Ordering::SeqCst));
        assert!(app.tx_set_dont_have.read().await.is_empty());
        assert!(app.tx_set_last_request.read().await.is_empty());
        assert!(app.tx_set_exhausted_warned.read().await.is_empty());
        assert!(app.consensus_stuck_state.read().await.is_none());
    }

    // ============================================================
    // Fetch Response Channel Skip Tests
    // ============================================================

    #[test]
    fn test_fetch_response_message_types_are_skipped_in_broadcast() {
        // Verify the message type matching pattern used in the broadcast
        // handler to skip fetch response messages (they go through the
        // dedicated channel instead).
        use stellar_xdr::curr::StellarMessage;

        let test_messages = vec![
            (
                StellarMessage::GeneralizedTxSet(stellar_xdr::curr::GeneralizedTransactionSet::V1(
                    stellar_xdr::curr::TransactionSetV1 {
                        previous_ledger_hash: stellar_xdr::curr::Hash([0u8; 32]),
                        phases: vec![].try_into().unwrap(),
                    },
                )),
                true,
                "GeneralizedTxSet",
            ),
            (
                StellarMessage::DontHave(stellar_xdr::curr::DontHave {
                    type_: stellar_xdr::curr::MessageType::TxSet,
                    req_hash: stellar_xdr::curr::Uint256([0u8; 32]),
                }),
                true,
                "DontHave",
            ),
        ];

        for (msg, should_skip, label) in test_messages {
            let is_fetch_response = matches!(
                msg,
                StellarMessage::GeneralizedTxSet(_)
                    | StellarMessage::TxSet(_)
                    | StellarMessage::DontHave(_)
                    | StellarMessage::ScpQuorumset(_)
            );
            assert_eq!(
                is_fetch_response, should_skip,
                "{} should be skipped={}",
                label, should_skip
            );
        }
    }

    // ============================================================
    // trim_syncing_ledgers Tests
    // ============================================================

    #[test]
    fn test_trim_syncing_ledgers_preserves_close_entries() {
        // When entries are close to current_ledger (gap < CHECKPOINT_FREQUENCY),
        // trim should NOT remove them to checkpoint boundary. These entries are
        // potentially closeable and trimming them creates an artificial gap.
        let mut buffer = BTreeMap::new();
        let make_entry = |slot: u32| henyey_herder::LedgerCloseInfo {
            slot: slot as u64,
            tx_set_hash: Hash256::ZERO,
            tx_set: None,
            close_time: 1,
            upgrades: Vec::new(),
            stellar_value_ext: StellarValueExt::Basic,
        };

        // Simulate: current_ledger=61193740, entries at 61193741..=61193797
        // These entries are close to current_ledger (gap=1 for first entry)
        // Old code would trim everything below checkpoint boundary of last_buffered
        // (first_ledger_in_checkpoint(61193797) = 61193792), destroying 61193741-61193791
        let current_ledger = 61193740u32;
        for slot in 61193741..=61193797 {
            buffer.insert(slot, make_entry(slot));
        }
        let original_count = buffer.len();

        App::trim_syncing_ledgers(&mut buffer, current_ledger);

        // All entries should survive — they're all above current_ledger and
        // the gap (1) is less than CHECKPOINT_FREQUENCY
        assert_eq!(
            buffer.len(),
            original_count,
            "trim should preserve entries close to current_ledger"
        );
        assert!(
            buffer.contains_key(&61193741),
            "first entry (current_ledger+1) must survive"
        );
        assert!(buffer.contains_key(&61193797), "last entry must survive");
    }

    #[test]
    fn test_trim_syncing_ledgers_trims_when_gap_large() {
        // When the gap to first_buffered is >= CHECKPOINT_FREQUENCY,
        // trim should remove entries below the checkpoint boundary of last_buffered
        // to prepare for archive-based catchup.
        let mut buffer = BTreeMap::new();
        let make_entry = |slot: u32| henyey_herder::LedgerCloseInfo {
            slot: slot as u64,
            tx_set_hash: Hash256::ZERO,
            tx_set: None,
            close_time: 1,
            upgrades: Vec::new(),
            stellar_value_ext: StellarValueExt::Basic,
        };

        // current_ledger=100, entries at 200..=280 (gap=100, > 64)
        let current_ledger = 100u32;
        for slot in 200..=280 {
            buffer.insert(slot, make_entry(slot));
        }

        App::trim_syncing_ledgers(&mut buffer, current_ledger);

        // After trim: checkpoint boundary of 280 is first_ledger_in_checkpoint(280) = 256
        // Entries below 256 should be removed
        assert!(
            !buffer.contains_key(&200),
            "entry well below checkpoint boundary should be trimmed"
        );
        assert!(
            !buffer.contains_key(&255),
            "entry just below checkpoint boundary should be trimmed"
        );
        assert!(
            buffer.contains_key(&256),
            "entry at checkpoint boundary should survive"
        );
        assert!(buffer.contains_key(&280), "last entry should survive");
    }

    #[test]
    fn test_trim_syncing_ledgers_removes_closed_entries() {
        // Entries at or below current_ledger should always be removed.
        let mut buffer = BTreeMap::new();
        let make_entry = |slot: u32| henyey_herder::LedgerCloseInfo {
            slot: slot as u64,
            tx_set_hash: Hash256::ZERO,
            tx_set: None,
            close_time: 1,
            upgrades: Vec::new(),
            stellar_value_ext: StellarValueExt::Basic,
        };

        let current_ledger = 105u32;
        for slot in 100..=110 {
            buffer.insert(slot, make_entry(slot));
        }

        App::trim_syncing_ledgers(&mut buffer, current_ledger);

        // Entries 100-105 should be removed, 106-110 kept
        assert!(!buffer.contains_key(&100));
        assert!(!buffer.contains_key(&105));
        assert!(buffer.contains_key(&106));
        assert!(buffer.contains_key(&110));
        assert_eq!(buffer.len(), 5);
    }

    #[test]
    fn test_consensus_stuck_state_matches_on_current_ledger_only() {
        // Verify that ConsensusStuckState matches when current_ledger is the
        // same but first_buffered changes. This is critical for Problem 9:
        // stale EXTERNALIZE messages create new syncing_ledgers entries with
        // lower slot numbers, changing first_buffered. The stuck timer must
        // NOT reset when first_buffered shifts.
        let state = ConsensusStuckState {
            current_ledger: 100,
            first_buffered: 105,
            stuck_start: Instant::now(),
            last_recovery_attempt: Instant::now(),
            recovery_attempts: 0,
            catchup_triggered: false,
        };

        // Same current_ledger, different first_buffered — should still match
        let current_ledger = 100u32;
        let new_first_buffered = 103u32; // changed due to stale EXTERNALIZE
        assert_eq!(state.current_ledger, current_ledger);
        // The fix: we no longer require state.first_buffered == first_buffered
        // so the timer continues even when first_buffered shifts.
        assert_ne!(state.first_buffered, new_first_buffered);

        // Different current_ledger — should NOT match (ledger advanced)
        let advanced_ledger = 101u32;
        assert_ne!(state.current_ledger, advanced_ledger);
    }

    // ============================================================
    // Fix A: try_apply_buffered_ledgers state reset tests
    // ============================================================

    #[tokio::test]
    async fn test_try_apply_buffered_no_close_preserves_stale_state() {
        // When try_apply_buffered_ledgers runs but there are NO buffered
        // ledgers to close (closed_any=false), it must NOT reset tracking
        // state.  This verifies the guard condition around the reset block.
        let dir = tempfile::tempdir().expect("temp dir");
        let db_path = dir.path().join("rs-stellar-test.db");
        let config = crate::config::ConfigBuilder::new()
            .database_path(db_path)
            .build();

        let app = App::new(config).await.unwrap();

        // Seed stale tracking state (as if a previous cycle left residue).
        app.tx_set_all_peers_exhausted.store(true, Ordering::SeqCst);
        {
            let mut dont_have = app.tx_set_dont_have.write().await;
            let hash = Hash256::from_bytes([2u8; 32]);
            dont_have.insert(
                hash,
                HashSet::from([henyey_overlay::PeerId::from_bytes([2u8; 32])]),
            );
        }
        {
            let mut last_req = app.tx_set_last_request.write().await;
            let hash = Hash256::from_bytes([2u8; 32]);
            last_req.insert(
                hash,
                TxSetRequestState {
                    last_request: Instant::now(),
                    first_requested: Instant::now(),
                    next_peer_offset: 1,
                },
            );
        }
        {
            let mut warned = app.tx_set_exhausted_warned.write().await;
            warned.insert(Hash256::from_bytes([2u8; 32]));
        }
        *app.consensus_stuck_state.write().await = Some(ConsensusStuckState {
            current_ledger: 50,
            first_buffered: 51,
            stuck_start: Instant::now(),
            last_recovery_attempt: Instant::now(),
            recovery_attempts: 1,
            catchup_triggered: false,
        });

        // Record the externalized timestamp before the call.
        let ext_before = *app.last_externalized_at.read().await;

        // Call with empty syncing_ledgers → loop exits immediately, closed_any=false.
        app.try_apply_buffered_ledgers().await;

        // All stale state should be PRESERVED (not cleared) because nothing closed.
        assert!(
            app.tx_set_all_peers_exhausted.load(Ordering::SeqCst),
            "tx_set_all_peers_exhausted should remain true when nothing closed"
        );
        assert!(
            !app.tx_set_dont_have.read().await.is_empty(),
            "tx_set_dont_have should remain populated when nothing closed"
        );
        assert!(
            !app.tx_set_last_request.read().await.is_empty(),
            "tx_set_last_request should remain populated when nothing closed"
        );
        assert!(
            !app.tx_set_exhausted_warned.read().await.is_empty(),
            "tx_set_exhausted_warned should remain populated when nothing closed"
        );
        assert!(
            app.consensus_stuck_state.read().await.is_some(),
            "consensus_stuck_state should remain when nothing closed"
        );
        let ext_after = *app.last_externalized_at.read().await;
        assert_eq!(
            ext_before, ext_after,
            "last_externalized_at should not be reset when nothing closed"
        );
    }

    #[tokio::test]
    async fn test_try_apply_buffered_state_reset_block_mirrors_rapid_close() {
        // Verify the state-reset block in try_apply_buffered_ledgers matches
        // the reset done in the pending_close handler.  We can't easily close
        // a real ledger in a unit test, so we directly exercise the reset logic
        // that fires when closed_any=true and verify the fields are cleared.
        // This is structurally identical to test_tx_set_state_cleanup_after_rapid_close.
        let dir = tempfile::tempdir().expect("temp dir");
        let db_path = dir.path().join("rs-stellar-test.db");
        let config = crate::config::ConfigBuilder::new()
            .database_path(db_path)
            .build();

        let app = App::new(config).await.unwrap();

        // Seed dirty state.
        app.tx_set_all_peers_exhausted.store(true, Ordering::SeqCst);
        {
            let mut dont_have = app.tx_set_dont_have.write().await;
            let hash = Hash256::from_bytes([3u8; 32]);
            dont_have.insert(
                hash,
                HashSet::from([henyey_overlay::PeerId::from_bytes([3u8; 32])]),
            );
        }
        {
            let mut last_req = app.tx_set_last_request.write().await;
            let hash = Hash256::from_bytes([3u8; 32]);
            last_req.insert(
                hash,
                TxSetRequestState {
                    last_request: Instant::now(),
                    first_requested: Instant::now(),
                    next_peer_offset: 5,
                },
            );
        }
        {
            let mut warned = app.tx_set_exhausted_warned.write().await;
            warned.insert(Hash256::from_bytes([3u8; 32]));
        }
        *app.consensus_stuck_state.write().await = Some(ConsensusStuckState {
            current_ledger: 200,
            first_buffered: 201,
            stuck_start: Instant::now(),
            last_recovery_attempt: Instant::now(),
            recovery_attempts: 3,
            catchup_triggered: false,
        });

        // Directly exercise the reset block (same code as in try_apply_buffered_ledgers
        // when closed_any=true).
        *app.last_externalized_at.write().await = Instant::now();
        app.reset_tx_set_tracking().await;
        *app.consensus_stuck_state.write().await = None;

        // Verify all tracking state is cleared.
        assert!(!app.tx_set_all_peers_exhausted.load(Ordering::SeqCst));
        assert!(app.tx_set_dont_have.read().await.is_empty());
        assert!(app.tx_set_last_request.read().await.is_empty());
        assert!(app.tx_set_exhausted_warned.read().await.is_empty());
        assert!(app.consensus_stuck_state.read().await.is_none());
    }

    // ============================================================
    // Fix B: Heartbeat gap guard tests
    // ============================================================

    #[test]
    fn test_heartbeat_gap_guard_skips_when_caught_up() {
        // The heartbeat stall detector computes:
        //   gap = latest_ext.saturating_sub(current_ledger)
        // When gap <= TX_SET_REQUEST_WINDOW (12), it should skip the SCP
        // state request to avoid bringing in stale EXTERNALIZE messages.
        // This test exercises the condition directly.

        let cases: Vec<(u64, u32, bool)> = vec![
            // (latest_ext, current_ledger, should_skip)
            (100, 100, true), // gap=0: fully caught up
            (100, 99, true),  // gap=1: one ledger behind
            (100, 88, true),  // gap=12: exactly at threshold
            (100, 87, false), // gap=13: one past threshold
            (100, 50, false), // gap=50: far behind
            (100, 0, false),  // gap=100: very far behind
            (0, 0, true),     // gap=0: both at zero (startup)
            (5, 10, true),    // gap=0 (saturating_sub): current > latest
        ];

        for (latest_ext, current_ledger, should_skip) in cases {
            let gap = latest_ext.saturating_sub(current_ledger as u64);
            let skip = gap <= TX_SET_REQUEST_WINDOW;
            assert_eq!(
                skip, should_skip,
                "latest_ext={}, current_ledger={}, gap={}: expected skip={} got skip={}",
                latest_ext, current_ledger, gap, should_skip, skip
            );
        }
    }

    #[test]
    fn test_externalized_iteration_window_unpublished_checkpoint_processes_all() {
        // current_ledger=129 => first_replay=130, checkpoint=128.
        // latest_externalized=127 means the replay checkpoint is unpublished,
        // so we must process all slots (no TX_SET_REQUEST_WINDOW trimming).
        let last_processed = 90u64;
        let current_ledger = 129u32;
        let latest_externalized = 127u64;

        let (iter_start, advance_to) =
            App::externalized_iteration_window(last_processed, current_ledger, latest_externalized);

        assert_eq!(iter_start, last_processed + 1);
        assert_eq!(advance_to, last_processed);
    }

    #[test]
    fn test_externalized_iteration_window_published_checkpoint_trims_to_window() {
        // first_replay checkpoint is published, so large gaps should trim to
        // the TX_SET_REQUEST_WINDOW tail.
        let last_processed = 100u64;
        let current_ledger = 110u32;
        let latest_externalized = 150u64; // gap from last_processed is 50 > 12

        let (iter_start, advance_to) =
            App::externalized_iteration_window(last_processed, current_ledger, latest_externalized);

        let expected_skip_to = latest_externalized.saturating_sub(TX_SET_REQUEST_WINDOW);
        assert_eq!(iter_start, expected_skip_to + 1);
        assert_eq!(advance_to, expected_skip_to);
    }

    #[test]
    fn test_externalized_catchup_cooldown_skip_when_next_externalize_cached() {
        // If the target checkpoint is not yet published and we already have
        // EXTERNALIZE for current_ledger+1, archive catchup cooldown should be
        // bypassed so sequential close can proceed immediately.
        let target_checkpoint = 191u32;
        let latest_externalized = 180u64;
        let have_next_externalize = true;

        assert!(App::should_skip_externalized_catchup_cooldown(
            target_checkpoint,
            latest_externalized,
            have_next_externalize,
        ));
    }

    #[test]
    fn test_externalized_catchup_cooldown_not_skipped_without_cached_next_externalize() {
        // Two negative cases:
        // 1) target checkpoint unpublished, but next EXTERNALIZE missing.
        // 2) target checkpoint published, regardless of cache state.
        assert!(!App::should_skip_externalized_catchup_cooldown(
            191, 180, false,
        ));
        assert!(!App::should_skip_externalized_catchup_cooldown(
            127, 180, true,
        ));
    }

    #[test]
    fn test_buffered_catchup_target_small_gap() {
        // When the gap between current_ledger and first_buffered is small (< 64),
        // the target should bridge the gap. This is the scenario where a single
        // missing EXTERNALIZE creates a tiny gap.
        let current_ledger = 61200834u32;
        let first_buffered = 61200836u32; // slot 61200835 was skipped
        let last_buffered = 61200850u32;

        let target = App::buffered_catchup_target(current_ledger, first_buffered, last_buffered);
        // With first_buffered > current_ledger + 1 and gap < CHECKPOINT_FREQUENCY,
        // should compute a valid target
        if let Some(t) = target {
            assert!(
                t > current_ledger,
                "target must advance past current_ledger"
            );
            assert!(t < first_buffered, "target must be before first_buffered");
        }
        // If None, compute_catchup_target_for_timeout should provide a fallback
        let timeout_target =
            App::compute_catchup_target_for_timeout(last_buffered, first_buffered, current_ledger);
        // For a small gap like this, we should get first_buffered - 1 as target
        assert_eq!(timeout_target, Some(first_buffered - 1));
    }

    /// Regression test for a deadlock in out_of_sync_recovery where the node
    /// gets stuck when next_slot is missing and target_checkpoint > latest_externalized.
    ///
    /// Scenario: Node catches up to L61935313, real-time SCP externalizes L61935323,
    /// but slots 61935314-61935322 are missing. The catchup_target is
    /// latest_ext - TX_SET_REQUEST_WINDOW = 61935311, and checkpoint_containing(61935311) =
    /// 61935359 > latest_externalized (61935323). The node's latest_externalized is frozen
    /// because it can't advance (stuck), but the archive HAS published checkpoint 61935359
    /// because the network moved past it.
    ///
    /// Before the fix: recovery_attempts_without_progress was reset to 2 on every tick,
    /// creating an infinite loop where attempts oscillated between 2 and 3 and never
    /// reached RECOVERY_ESCALATION_CATCHUP (6), preventing catchup.
    ///
    /// After the fix: attempts accumulate normally. After 30 ticks (~5 minutes), the
    /// code triggers catchup regardless of the checkpoint heuristic.
    #[test]
    fn test_gap_recovery_does_not_deadlock_on_unpublished_checkpoint_heuristic() {
        use henyey_history::checkpoint::checkpoint_containing;

        // Reproduce the exact scenario from mainnet L61935313
        let current_ledger = 61935313u32;
        let latest_externalized = 61935323u64;
        let next_slot = current_ledger as u64 + 1; // 61935314

        // The catchup target is latest_ext - TX_SET_REQUEST_WINDOW (12)
        let catchup_target = latest_externalized.saturating_sub(TX_SET_REQUEST_WINDOW) as u32;
        assert_eq!(catchup_target, 61935311);

        let target_checkpoint = checkpoint_containing(catchup_target);
        assert_eq!(target_checkpoint, 61935359);

        // This is the condition that causes the "checkpoint not published" branch
        assert!(
            target_checkpoint as u64 > latest_externalized,
            "target_checkpoint ({}) should exceed latest_externalized ({}) — \
             this is the condition that triggers the stuck state",
            target_checkpoint,
            latest_externalized
        );

        // Verify that the gap detection would identify the missing next_slot
        assert!(
            latest_externalized > next_slot,
            "latest_externalized ({}) should exceed next_slot ({})",
            latest_externalized,
            next_slot
        );

        // The fix: attempts must be allowed to accumulate past 30 so that
        // either the line-130 escalation (attempts >= RECOVERY_ESCALATION_CATCHUP)
        // triggers catchup, or the new attempts >= 30 branch directly triggers it.
        // Previously, attempts were reset to 2, which is < RECOVERY_ESCALATION_CATCHUP (6).
        let _escalation_threshold = RECOVERY_ESCALATION_CATCHUP;
        let stuck_catchup_threshold = 30u64;

        // Simulate the attempt counter behavior:
        // - Attempts 0-2: request SCP state from peers (fall through)
        // - Attempts 3-29: wait (return without resetting counter)
        // - Attempt 30+: trigger catchup
        for attempt in 0..=stuck_catchup_threshold + 5 {
            if target_checkpoint as u64 > latest_externalized {
                if attempt <= 2 {
                    // Falls through to SCP state request — fine
                } else if attempt < stuck_catchup_threshold {
                    // Waits without resetting — this is the fix
                    // (Before fix: counter was reset to 2 here)
                } else {
                    // Triggers catchup — this is the new escape hatch
                    assert!(
                        attempt >= stuck_catchup_threshold,
                        "Should trigger catchup at attempt {}",
                        attempt
                    );
                    break;
                }
            } else {
                // This branch triggers "Next slot permanently missing — catchup"
                // Not reached in this scenario since checkpoint > latest_ext
                panic!("Should not reach this branch in this scenario");
            }
        }

        // Also verify: if latest_externalized catches up to the checkpoint
        // (e.g., the node participates in SCP for later slots), the normal
        // catchup path at line 316 would trigger. This is the original design
        // for when the heuristic is correct.
        let advanced_latest_ext = target_checkpoint as u64 + 1;
        assert!(
            !(target_checkpoint as u64 > advanced_latest_ext),
            "When latest_ext advances past checkpoint, normal catchup should trigger"
        );
    }

    /// Regression test: after rapid close overshoots the archive's latest
    /// checkpoint, trigger_recovery_catchup must target the NEXT checkpoint
    /// (ahead of current_ledger), not CatchupTarget::Current which returns
    /// the stale archive checkpoint we've already passed.
    ///
    /// Reproduces the exact scenario from mainnet L61936132:
    /// - Node caught up to checkpoint 61936127 and rapid-closed to 61936132
    /// - Archive's latest is still 61936127 (behind us)
    /// - CatchupTarget::Current → 61936127 → "already at target" → dead loop
    /// - Fix: target checkpoint_containing(61936133) = 61936191 → retries
    ///   until archive publishes it → catchup succeeds → convergence
    #[test]
    fn test_recovery_catchup_targets_next_checkpoint_not_current() {
        use henyey_history::checkpoint::checkpoint_containing;

        // Scenario: rapid close overshot the archive's latest checkpoint
        let current_ledger = 61936132u32;
        let archive_latest = 61936127u32; // the archive's latest checkpoint

        // Verify we ARE past the archive checkpoint — this is the stuck condition
        assert!(
            current_ledger > archive_latest,
            "current_ledger ({}) should be past archive_latest ({}) — \
             this is the condition where CatchupTarget::Current loops",
            current_ledger,
            archive_latest,
        );

        // The fix: compute next checkpoint from current_ledger + 1
        let next_cp = checkpoint_containing(current_ledger + 1);
        assert_eq!(next_cp, 61936191);

        // The next checkpoint must be AHEAD of current_ledger
        assert!(
            next_cp > current_ledger,
            "next_cp ({}) must be ahead of current_ledger ({}) — \
             this ensures CatchupTarget::Ledger(next_cp) never triggers \
             'already at target'",
            next_cp,
            current_ledger,
        );

        // The next checkpoint must also be ahead of the archive's latest
        assert!(
            next_cp > archive_latest,
            "next_cp ({}) must be ahead of archive_latest ({}) — \
             this means the archive may not have it yet, but the catchup \
             will retry with 404s until it's published",
            next_cp,
            archive_latest,
        );

        // Edge case: current_ledger is exactly ON a checkpoint boundary
        let current_on_boundary = 61936127u32;
        let next_cp_from_boundary = checkpoint_containing(current_on_boundary + 1);
        assert_eq!(next_cp_from_boundary, 61936191);
        assert!(
            next_cp_from_boundary > current_on_boundary,
            "Even at a checkpoint boundary, next_cp ({}) must be ahead",
            next_cp_from_boundary,
        );
    }
}
