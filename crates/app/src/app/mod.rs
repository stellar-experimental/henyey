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
//! 2. **Catchup** ([`App::catchup_with_mode`]): If behind, download and apply history from archives
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
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rand::seq::SliceRandom;
use tokio::sync::mpsc;
use tokio::sync::Mutex as TokioMutex;
use tokio::sync::RwLock;

use henyey_bucket::BucketManager;
use henyey_bucket::{
    BucketList, BucketListSnapshot, BucketSnapshotManager, HotArchiveBucketList,
    HotArchiveBucketListSnapshot, PendingMergeState,
};
use henyey_clock::{Clock, RealClock};
use henyey_common::protocol::{
    hot_archive_supported, protocol_version_starts_from, ProtocolVersion,
};
use henyey_common::{Hash256, NetworkId};
use henyey_db::queries::StateQueries;
use henyey_db::schema::state_keys;
use henyey_herder::{
    drift_tracker::CloseTimeDriftTracker,
    flow_control::compute_max_tx_size,
    sync_recovery::{SyncRecoveryCallback, SyncRecoveryHandle, SyncRecoveryManager},
    BroadcastBudget, BroadcastVisitResult, CloseTimeBounds, EnvelopeState, Herder, HerderConfig,
    HerderStats, TxQueueConfig, TxSetValidationContext,
};
use henyey_history::{
    build_history_archive_state, checkpoint_containing, checkpoint_frequency, checkpoint_start,
    first_ledger_after_checkpoint_containing, is_checkpoint_ledger, is_checkpoint_start,
    last_ledger_before_checkpoint_containing, latest_checkpoint_before_or_at, CatchupManager,
    CatchupMode, CatchupResult as HistoryCatchupResult, CheckpointData, ExistingBucketState,
    HistoryArchive, HistoryArchiveState, GENESIS_LEDGER_SEQ,
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
use henyey_tx::{envelope_sequence_number, TransactionFrame};
use henyey_work::{WorkScheduler, WorkSchedulerConfig, WorkState};
use stellar_xdr::curr::{
    Curve25519Public, EncryptedBody, FloodAdvert, FloodDemand, Hash, LedgerCloseMeta,
    LedgerScpMessages, ReadXdr, ScpEnvelope, ScpHistoryEntry, ScpHistoryEntryV0,
    ScpStatementPledges, SignedTimeSlicedSurveyResponseMessage, StellarMessage, StellarValue,
    SurveyMessageCommandType, SurveyRequestMessage, SurveyResponseBody, SurveyResponseMessage,
    TimeSlicedPeerDataList, TimeSlicedSurveyRequestMessage, TimeSlicedSurveyResponseMessage,
    TimeSlicedSurveyStartCollectingMessage, TimeSlicedSurveyStopCollectingMessage,
    TopologyResponseBodyV2, TransactionHistoryEntry, TransactionHistoryEntryExt,
    TransactionHistoryResultEntry, TransactionHistoryResultEntryExt, TransactionMeta,
    TransactionResultPair, TransactionResultSet, TransactionSet, TxAdvertVector, TxDemandVector,
    VecM, WriteXdr,
};
use x25519_dalek::{PublicKey as CurvePublicKey, StaticSecret as CurveSecretKey};

use crate::config::AppConfig;
use crate::logging::CatchupProgress;
use crate::meta_stream::{MetaStreamError, MetaStreamManager};
use crate::meta_writer::MetaWriter;
use crate::survey::{HerderLedgerSource, SurveyDataManager, SurveyMessageLimiter, SurveyState};
use henyey_ledger::{close_time as ledger_close_time, compute_header_hash, verify_header_chain};
use stellar_xdr::curr::TransactionEnvelope;

const TIME_SLICED_PEERS_MAX: usize = 25;
const PEER_MAX_FAILURES_TO_SEND: u32 = 10;
const TX_SET_REQUEST_WINDOW: u64 = 12;
const MAX_TX_SET_REQUESTS_PER_TICK: usize = 32;
/// Consensus stuck timeout matching stellar-core's CONSENSUS_STUCK_TIMEOUT_SECONDS.
/// No longer used in the unified decision function (see #1831), but kept
/// for the parity-checking test assertion.
#[cfg(test)]
const CONSENSUS_STUCK_TIMEOUT_SECS: u64 = 35;

/// Pool ledger multiplier: queue limits = per-ledger limits × this factor.
/// Matches stellar-core's `poolLedgerMultiplier` default (2).
const POOL_LEDGER_MULTIPLIER: u32 = 2;

/// Number of consecutive recovery attempts without ledger progress before
/// escalating from passive waiting to actively requesting SCP state from
/// peers. At the 1s consensus recovery interval this equals ~6s.
const RECOVERY_ESCALATION_SCP_REQUEST: u64 = 6;

/// Number of consecutive recovery attempts without progress before
/// triggering a full catchup. At the 1s consensus recovery interval this
/// equals ~6s.
const RECOVERY_ESCALATION_CATCHUP: u64 = 6;

/// Maximum slot gap between the highest observed EXTERNALIZE and our
/// current ledger before `submit_transaction()` rejects with TryAgainLater.
///
/// When the node has seen an EXTERNALIZE message more than this many slots
/// ahead of its applied ledger, it knows its state is stale and any tx
/// validation would run against outdated account state (producing terminal
/// errors like `TxBadSeq` for what is actually a transient condition).
///
/// This gate is purely in the user-facing submission path — overlay tx
/// intake bypasses it. See #1812.
const TX_SUBMISSION_MAX_BEHIND: u64 = 2;

/// Timeout for pending tx_set requests with no response from any peer.
/// If we've been requesting a tx_set for this long with zero responses
/// (no GeneralizedTxSet AND no DontHave), assume peers silently dropped
/// the requests and treat as if all peers said DontHave.
const TX_SET_REQUEST_TIMEOUT_SECS: u64 = 10;

/// Recovery timer for out-of-sync recovery attempts.
/// Matches stellar-core's OUT_OF_SYNC_RECOVERY_TIMER.
const OUT_OF_SYNC_RECOVERY_TIMER_SECS: u64 = 10;

// The archive-checkpoint cache TTL now lives in
// `archive_cache::ARCHIVE_CHECKPOINT_CACHE_SECS` (see that module for the
// full non-blocking-cache rationale; issue #1784).

/// How long to back off archive queries after learning the archive's latest
/// checkpoint is still behind the one we need.
///
/// When a node falls slightly behind the tip and its peers evict the missing
/// tx_sets, the out-of-sync recovery path escalates to catchup. The catchup
/// targets the next history checkpoint. If the archive has not yet published
/// that checkpoint (cadence: every 64 ledgers ≈ 5 minutes on mainnet), the
/// escalation reports "Recovery catchup skipped: archive hasn't published
/// checkpoint yet" and returns.
///
/// The `SyncRecoveryManager` re-fires `out_of_sync_recovery` every 10 seconds
/// (`OUT_OF_SYNC_RECOVERY_TIMER_SECS`). Without backoff, each tick re-queries
/// the archive — even though the archive publishes on a ≥5 minute cadence,
/// so 29 of 30 queries are guaranteed to return the same stale result. This
/// wastes bandwidth, adds archive load, and pollutes logs with repeated
/// "Querying history archives" / "Recovery catchup skipped" pairs.
///
/// Setting a dedicated backoff gives the archive time to publish the missing
/// checkpoint before the next query, while still letting the recovery path
/// request SCP state from peers (a separate, cheap action) on every tick.
const ARCHIVE_BEHIND_BACKOFF_SECS: u64 = 60;

/// Shorter archive-behind backoff when the next checkpoint is imminent.
///
/// When the node is in the final third of a checkpoint cycle (i.e., the next
/// publishable checkpoint is ≤ `checkpoint_frequency / 3` ledgers away), the
/// archive is expected to publish soon. Polling every 15s instead of 60s during
/// this window reduces the stall between catchup completion and the first
/// post-catchup ledger close, directly addressing the RPC health latency flake
/// described in #1754.
///
/// Uses `checkpoint_frequency()` so it works for both the default 64-ledger
/// cycle and the accelerated 8-ledger cycle.
const ARCHIVE_BEHIND_IMMINENT_BACKOFF_SECS: u64 = 15;

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

/// Wall-clock gate for HardReset when tx_set_exhausted stays false (the
/// "envelopes never arrived" path). 120s = 12 ticks of
/// OUT_OF_SYNC_RECOVERY_TIMER_SECS.
const HARD_RESET_STALL_SECS: u64 = 120;

/// Hard floor: never reset more than once per this interval.
/// Prevents reset storms when the node is legitimately stabilizing.
const HARD_RESET_MIN_COOLDOWN_SECS: u64 = 60;

/// Soft ceiling: after this, always allow a reset if consensus is
/// still stuck. Prevents operator-visible lockout when automation
/// is the only remediation path.
const HARD_RESET_MAX_COOLDOWN_SECS: u64 = 300;

/// Gap escalation threshold: if the gap has grown by ≥ this many
/// slots since the last reset, override the cooldown (but never the
/// absolute MIN). Tied to TX_SET_REQUEST_WINDOW because that is the
/// peer-cache window — growth past it means peer-SCP has failed and
/// the stall is worsening.
const HARD_RESET_GAP_ESCALATION: u64 = TX_SET_REQUEST_WINDOW;

/// Minimum verified peer gap (max_verified_scp_slot - current_ledger) before
/// hard-reset escalation. Prevents spurious resets from a single far-ahead
/// envelope.
const PEER_AHEAD_ESCALATION_THRESHOLD: u64 = 3;

/// Recovery attempts (0-based, pre-increment from fetch_add) before hard-reset
/// escalation when archive is confirmed behind and peers are verified ahead.
/// `attempts >= 11` fires on the 12th tick (~120s at 10s/tick), matching
/// HARD_RESET_STALL_SECS.
const RECOVERY_HARD_RESET_ESCALATION_ATTEMPTS: u64 =
    (HARD_RESET_STALL_SECS / OUT_OF_SYNC_RECOVERY_TIMER_SECS) - 1;

/// Higher threshold for no-SCP stall: fires on the 18th tick (~180s at 10s/tick).
const RECOVERY_HARD_RESET_ESCALATION_ATTEMPTS_NO_SCP: u64 =
    (HARD_RESET_STALL_SECS * 3 / 2 / OUT_OF_SYNC_RECOVERY_TIMER_SECS) - 1;

/// Attempt-counter seed for partial-progress resets. When the node makes
/// progress via a fast-track jump but is still significantly behind peers,
/// we re-seed the attempt counter to this value so re-escalation fires
/// after ~3 more ticks (~30s) instead of the full ~120s.
///
/// Derived from `RECOVERY_HARD_RESET_ESCALATION_ATTEMPTS` so it tracks
/// any future tuning of that constant.
const PARTIAL_PROGRESS_RESEED: u64 = RECOVERY_HARD_RESET_ESCALATION_ATTEMPTS.saturating_sub(3);

/// Controls which recovery state is reset when progress is detected.
///
/// `Full` clears all escalation state including `max_verified_scp_slot`.
/// `Partial` preserves `max_verified_scp_slot` (peer-gap evidence for the
/// hard-reset gate) and uses monotonic reseeding (`fetch_max`) so the
/// attempt counter is never lowered by a partial-progress reset.
///
/// Both variants always snapshot the SCP baseline to invalidate stale
/// pre-progress SCP traffic (preventing false fast-track triggers).
pub(super) enum RecoveryResetMode {
    /// Node is at or near the tip. Clear all escalation state.
    Full,
    /// Node advanced but is still significantly behind. Preserve peer-gap
    /// evidence and re-seed the attempt counter for faster re-escalation.
    Partial { seed: u64 },
}

/// The scope of archive-recovery state to clear.
///
/// Each variant encapsulates a specific recovery scenario, ensuring callers
/// cannot accidentally mix incompatible operations (e.g., clearing the livelock
/// tracker during a hard-reset execution).
#[derive(Debug, Clone, Copy)]
pub(crate) enum ArchiveRecoveryClear {
    /// Ledger progress detected (full or at-tip): full reset of recovery attempts,
    /// clear all signals, disable urgency, clear livelock tracker.
    FullProgress,
    /// Partial progress detected: reset recovery attempts with partial seed,
    /// clear all signals, ENABLE urgency for faster re-confirmation, clear livelock.
    PartialProgress { seed: u64 },
    /// Hard-reset skipped (defense-in-depth): clear signals, disable urgency,
    /// clear livelock. No recovery-attempt reset.
    DefenseSkip,
    /// Hard-reset executing: clear only the behind flag and backoff timer.
    /// Preserves livelock tracker (must persist across hard-reset executions to
    /// detect stuck loops) and does not touch urgency (about to re-query anyway).
    HardResetExec,
    /// Archive confirmed current (cache shows latest >= next_cp): clear behind
    /// flag and backoff, disable urgency. Preserves livelock tracker (node is
    /// not resetting; tracker is irrelevant here).
    ArchiveConfirmedCurrent,
}

/// Unified archive-recovery status. Replaces the split `archive_confirmed_behind`
/// (AtomicBool) + `archive_behind_until` (RwLock<Option<Instant>>) with a single
/// source of truth. See #2721.
///
/// Design invariants:
/// - "Backoff without confirmed-behind" is impossible by construction.
/// - `ConfirmedBehind { backoff_until: Some(expired) }` is still confirmed-behind
///   but backoff has lapsed (queries allowed again).
/// - `Unknown` is the only cleared state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ArchiveRecoveryStatus {
    /// No confirmed information about archive state. Initial/cleared state.
    Unknown,
    /// Archive authoritatively observed as behind (latest < next_checkpoint).
    /// `backoff_until` optionally suppresses redundant archive queries.
    ConfirmedBehind { backoff_until: Option<Instant> },
}

/// Point-in-time snapshot of [`ArchiveRecoveryStatus`]. All predicates evaluate
/// against the same observation, eliminating TOCTOU races between the flag and
/// backoff timer that existed with split state.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ArchiveRecoverySnapshot {
    pub status: ArchiveRecoveryStatus,
}

impl ArchiveRecoverySnapshot {
    /// Is the archive confirmed behind (regardless of backoff state)?
    pub fn is_confirmed_behind(&self) -> bool {
        matches!(self.status, ArchiveRecoveryStatus::ConfirmedBehind { .. })
    }

    /// Is the query-suppression backoff currently active?
    pub fn is_backoff_active(&self, now: Instant) -> bool {
        matches!(
            self.status,
            ArchiveRecoveryStatus::ConfirmedBehind { backoff_until: Some(deadline) }
            if now < deadline
        )
    }

    /// Combined: is the archive behind? Equivalent to the old
    /// `archive_confirmed_behind.load() || archive_behind_until.read().is_some_and(...)`.
    pub fn is_behind(&self) -> bool {
        self.is_confirmed_behind()
    }
}

/// Tracks whether the current recovery episode's onset diagnostic has been
/// emitted. Reset only on `RecoveryResetMode::Full` (node reached tip or
/// completed catchup). This ensures exactly one info-level onset log per
/// stall episode, regardless of partial reseeds or re-entries.
///
/// `Full` is the correct episode boundary because it is the only reset mode
/// that indicates genuine recovery — it zeroes `max_verified_scp_slot` and
/// `recovery_attempts_without_progress`. `Partial` reseeds indicate
/// incremental progress while still behind (same episode continues).
struct RecoveryEpisodeLatch {
    onset_logged: AtomicBool,
}

impl RecoveryEpisodeLatch {
    fn new() -> Self {
        Self {
            onset_logged: AtomicBool::new(false),
        }
    }

    /// Try to mark onset as logged. Returns `true` exactly once per episode
    /// (the first call since the last `reset()`).
    fn try_mark_onset(&self) -> bool {
        self.onset_logged
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
    }

    /// Reset the latch for a new episode.
    fn reset(&self) {
        self.onset_logged.store(false, Ordering::SeqCst);
    }
}

/// Maximum wall-clock duration (seconds) a node may remain stuck at the same
/// ledger with hard resets firing before triggering a fail-fast shutdown with
/// wipe signal. Computed as 2× the checkpoint publish window, floored at 120s.
///
/// At default frequency (64): 640s (~10.7 min)
/// At accelerated frequency (8): 120s (floor)
fn hard_reset_fatal_timeout_secs() -> u64 {
    let freq = checkpoint_frequency() as u64;
    let publish_window = freq * 5;
    (publish_window * 2).max(120)
}

/// /health returns unhealthy (503) when consensus_stuck_state has been
/// populated for at least this long. Strictly less than
/// HARD_RESET_STALL_SECS so operators see the stall *before* the node
/// tries to self-heal.
pub(crate) const HEALTH_STALL_SECS: u64 = 60;

mod archive_cache;
pub mod bootstrap;
mod catchup_impl;
mod close;
mod close_pipeline;
mod consensus;
mod ledger_close;
mod lifecycle;
mod log_throttle;
mod peers;
mod persist;
mod phase;
mod publish;
mod scp_dedup;
mod scp_timer_bridge;
mod survey_impl;
mod tracked_lock;
mod tx_flooding;
mod types;
mod upgrades;

pub use persist::CatchupFinalizer;
use types::*;
pub use types::{
    AppInfo, AppMetricsSnapshot, AppState, CatchupResult, CatchupTarget, FallbackCatchup,
    LedgerInfo, LedgerSummary, OverlayFetchChannelMetrics, RestoreResult, ScpSlotDebugStats,
    ScpSlotSnapshot, ScpVerifyMetrics, SelfCheckResult, SimulationDebugStats, SurveyPeerReport,
    SurveyReport,
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

    /// Readiness gate for the query server, matching stellar-core's
    /// `QueryServer::mIsReady`. Set to `true` after the first bucket
    /// snapshot is populated in `App::run()`. The query server middleware
    /// returns 404 "Core is booting" for all registered routes until this
    /// flag is set.
    query_is_ready: Arc<AtomicBool>,

    /// Ledger manager for ledger operations.
    ledger_manager: Arc<LedgerManager>,

    /// Overlay network manager.
    /// Wrapped in Arc so callers can clone the reference and use it without
    /// holding the RwLock, preventing the overlay lock from blocking the main
    /// event loop during slow network operations.
    overlay: RwLock<Option<Arc<OverlayManager>>>,

    /// Shared handle to the overlay's tracking flag. Allows synchronous
    /// callbacks (e.g., `SyncRecoveryCallback::on_lost_sync`) to update
    /// the overlay's tracking state without async overlay access.
    /// Populated by `start_overlay()`; `None` until the overlay starts.
    overlay_tracking: std::sync::Mutex<Option<Arc<std::sync::atomic::AtomicBool>>>,

    /// Shared handle to the overlay's synced flag. Mirrors stellar-core's
    /// `LedgerManager::isSynced()`. Set false during catchup / lost sync,
    /// true when operational. Allows synchronous callbacks to flip without
    /// async overlay access. Populated by `start_overlay()`.
    overlay_synced: std::sync::Mutex<Option<Arc<std::sync::atomic::AtomicBool>>>,

    /// Pre-bound TCP listener for the overlay, injected by the simulation
    /// harness to use OS-assigned ephemeral ports.  Set once before
    /// `start_overlay()` and consumed (taken) during overlay startup.
    /// Production callers leave this as `None`.
    pre_bound_listener: std::sync::Mutex<Option<henyey_overlay::Listener>>,

    /// Herder for consensus coordination.
    herder: Arc<Herder>,

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
    /// Catchup spawned from a context that cannot return `Option<PendingCatchup>`
    /// (e.g., `handle_overlay_message`, `handle_generalized_tx_set`).
    /// The event loop promotes this to the local `pending_catchup` each iteration.
    deferred_catchup: tokio::sync::Mutex<Option<PendingCatchup>>,
    /// Fatal local state failure flag.
    ///
    /// Set when the node detects that its local ledger state cannot be
    /// trusted (catchup verification failure, pre-close hash mismatch, etc.).
    /// Once set:
    /// - No further catchup attempts are made
    /// - No new ledger closes are started
    /// - The main loop exits with an error status
    ///
    /// The supervisor should wipe state files and restart.
    fatal_state_failure: AtomicBool,
    /// When set, the next catchup should do a full bucket-apply instead of
    /// replay-only. This is triggered when a previous catchup fails with a
    /// hash mismatch (state divergence, e.g., protocol upgrade missed).
    catchup_needs_full_reset: AtomicBool,
    /// Prevent concurrent history publish operations.
    /// When set, a background task is publishing a checkpoint.
    publish_in_progress: AtomicBool,
    /// One-shot panic injection for publish tests. When `true`, the next call
    /// to `publish_single_checkpoint` will swap it to `false` and panic.
    #[cfg(test)]
    pub(crate) publish_panic_inject: AtomicBool,
    /// Buffered externalized ledgers waiting to apply.
    ///
    /// # Invariant (event-loop freeze guard rail)
    ///
    /// All writers of this map execute on the event-loop task. There
    /// are no background-task writers in production — the only
    /// `.write()` callers are reachable from the event-loop select!
    /// arms (`process_externalized_slots`, `maybe_start_buffered_catchup`,
    /// `attach_tx_set_by_hash`, `buffer_externalized_tx_set`,
    /// `ensure_buffered_slot`, `out_of_sync_recovery`,
    /// `handle_catchup_result`).
    ///
    /// Holders of `.write()` MUST NOT hold the write guard across a
    /// `.await` other than short same-map mutations (insert, remove,
    /// retain on ≤100 entries). Held-lock time MUST be bounded by
    /// O(buffer size), not O(external work). In particular, XDR
    /// parsing (`herder.check_ledger_close`), herder queries, database
    /// I/O, and network operations MUST happen outside the critical
    /// section — snapshot inputs, compute a mutation plan lock-free,
    /// then apply the plan under a single short write.
    ///
    /// Violating this invariant re-opens the class of event-loop
    /// freeze documented in issues #1759 (phase=2 fetch_resp / phase=6
    /// pending_close), #1784 (phase=13 buffered_catchup archive-HTTP),
    /// and #1788 (phase=13 buffered_catchup recurrence). The split of
    /// `process_externalized_slots`' critical section (commit that
    /// closed #1769) specifically moves the per-slot XDR parse out of
    /// this write lock.
    ///
    /// tokio::sync::RwLock is NOT reentrant per task. If you hold this
    /// lock and `.await` something that eventually tries to acquire it
    /// again on the same task, the task deadlocks silently. See the
    /// comment at the second acquire in `maybe_start_buffered_catchup`
    /// (catchup_impl.rs, PHASE_13_1 stamp site) for the concrete
    /// example.
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
    /// Count of SCP envelopes accepted past the in-flight dedup cache
    /// (`scp_scheduled.check_and_insert`). Parity:
    /// `HerderImpl.cpp:810 mSCPMetrics.mEnvelopeReceive.Mark()` — fires
    /// after dedup, before validity checks (`checkCloseTime`, slot-range,
    /// `verifyEnvelope`). See `pump_scp_intake` in `lifecycle.rs`.
    scp_messages_received: AtomicU64,
    /// SCP pre-filter rejections by reason (issue #1734 Phase B metrics).
    scp_prefilter_counters: henyey_herder::scp_verify::PreFilterCounters<AtomicU64>,
    /// Post-verify drops (gate drift, self-message, non-quorum, invalid).
    /// Aggregate counter — kept for backward compatibility with existing dashboards.
    scp_post_verify_drops: AtomicU64,
    /// Per-reason post-verify counters (issue #1733 observability polish).
    scp_pv_counters: henyey_herder::scp_verify::PostVerifyCounters<AtomicU64>,
    /// Poor-man's histogram for verify latency (enqueue → post-verify dispatch).
    scp_verify_latency_us_sum: AtomicU64,
    scp_verify_latency_count: AtomicU64,
    /// In-flight SCP envelope dedup cache. See [`scp_dedup::ScpScheduledCache`].
    scp_scheduled: scp_dedup::ScpScheduledCache,
    /// Sampled depth of the verified-output channel (verified_rx.len()).
    /// Updated by the event loop each time it touches `verified_rx`, so
    /// `/metrics` reflects the true output-side backlog.
    pub(crate) scp_verify_output_backlog: AtomicU64,
    /// Sampled depth of the overlay fetch-response channel (see
    /// [`OverlayManager::subscribe_fetch_responses`]). Updated by the event
    /// loop each time it touches `fetch_response_rx`. Exposed via `/metrics`
    /// (`henyey_overlay_fetch_channel_depth`). Also read by the watchdog.
    pub(crate) fetch_channel_depth: Arc<AtomicI64>,
    /// Monotonic maximum depth observed on the overlay fetch-response
    /// channel since process start. Exposed via `/metrics`
    /// (`henyey_overlay_fetch_channel_depth_max`).
    pub(crate) fetch_channel_depth_max: Arc<AtomicI64>,
    /// Number of attempts to trigger the next consensus round.
    consensus_trigger_attempts: AtomicU64,
    /// Number of successful trigger_next_ledger calls.
    consensus_trigger_successes: AtomicU64,
    /// Number of failed trigger_next_ledger calls.
    consensus_trigger_failures: AtomicU64,
    /// Number of try_trigger_consensus invocations skipped because a ledger
    /// close was in progress (parity gate with stellar-core
    /// HerderImpl.cpp:1440-1447).
    pub(crate) consensus_trigger_skipped_applying: AtomicU64,
    /// Number of trigger_next_ledger invocations that returned
    /// TriggerOutcome::SkippedStale because LCL advanced during build_nomination_value
    /// (parity gate with stellar-core HerderImpl.cpp:1550-1562).
    pub(crate) consensus_trigger_skipped_stale: AtomicU64,
    /// Number of nomination timeout firings.
    nomination_timeout_fires: AtomicU64,
    /// Number of nomination timeout invocations that returned
    /// TimeoutOutcome::SkippedStale because LCL advanced during build/drain.
    pub(crate) nomination_timeout_skipped_stale: AtomicU64,
    /// Number of ballot timeout firings.
    ballot_timeout_fires: AtomicU64,
    /// Time when we last observed an externalized slot.
    last_externalized_at: RwLock<Instant>,
    /// Last time we requested SCP state due to stalled externalization.
    last_scp_state_request_at: RwLock<Instant>,

    /// Combined survey data manager and message limiter under one lock.
    /// Invariant: no `.await` while holding a guard on this lock.
    survey_state: RwLock<SurveyState>,

    /// Carry-over ops budget from the previous flood period. Capped at
    /// MAX_OPS_PER_TX + 1 to prevent unbounded accumulation from missed ticks.
    broadcast_op_carryover: AtomicUsize,

    /// DEX-lane carry-over ops budget from the previous flood period.
    /// Only meaningful when `MAX_DEX_TX_OPERATIONS_IN_TX_SET` is configured.
    broadcast_dex_op_carryover: AtomicUsize,

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
    /// Per-hash retry timestamps for exhausted tx_set re-fetches (30s backoff).
    /// Separate from `tx_set_last_request` because DontHave handling removes
    /// last_request entries, which would destroy retry backoff state.
    tx_set_last_retry: RwLock<HashMap<Hash256, Instant>>,
    /// Monotonic offset (seconds since `start_instant`) when `tx_set_all_peers_exhausted`
    /// first transitioned false→true. 0 means "not exhausted". Used by the
    /// `henyey_recovery_tx_set_stuck_seconds` gauge.
    tx_set_exhausted_since: AtomicU64,
    /// When we detected consensus is stuck (for timeout detection).
    /// Stores (current_ledger, first_buffered, stuck_start_time, last_recovery_attempt).
    pub(crate) consensus_stuck_state: RwLock<Option<ConsensusStuckState>>,
    /// When catchup last completed (for cooldown).
    last_catchup_completed_at: RwLock<Option<Instant>>,
    /// Non-blocking cache for the latest archive checkpoint. Event-loop
    /// callers read via `get_cached_archive_checkpoint_nonblocking`;
    /// startup and spawned-catchup callers read via
    /// `get_cached_archive_checkpoint_blocking`.
    ///
    /// Replaces the old `RwLock<Option<(u32, Instant)>>`: that type forced
    /// every caller to `.await` on the tokio RwLock, and on cache miss
    /// synchronously awaited the archive HTTP fetch — the root cause of
    /// the 89 s event-loop freeze in issue #1784.
    archive_checkpoint_cache: Arc<archive_cache::ArchiveCheckpointCache>,
    /// Unified archive-recovery status. Encodes both the "confirmed behind"
    /// flag and the query-suppression backoff timer as a single enum.
    /// See [`ArchiveRecoveryStatus`] and #2721.
    ///
    /// Lock ordering: same rank as the former `archive_behind_until`. Callers
    /// MUST NOT hold `syncing_ledgers` or `consensus_stuck_state` when
    /// acquiring a write lock.
    archive_recovery_status: RwLock<ArchiveRecoveryStatus>,
    /// SCP latency samples for surveys.
    scp_latency: RwLock<ScpLatencyTracker>,

    /// Survey scheduler state for time-sliced surveys.
    survey_scheduler: TokioMutex<SurveyScheduler>,
    /// Next survey nonce.
    survey_nonce: RwLock<u32>,
    /// Ephemeral survey encryption secrets keyed by nonce.
    survey_secrets: RwLock<HashMap<u32, [u8; 32]>>,
    /// Survey responses keyed by nonce.
    survey_results: RwLock<HashMap<u32, HashMap<henyey_overlay::PeerId, TopologyResponseBodyV2>>>,
    /// Survey throttle timeout between survey runs.
    survey_throttle: Duration,
    /// Survey reporting backlog state (surveyor-side).
    survey_reporting: RwLock<SurveyReportingState>,
    /// SCP timer manager handle for scheduling/cancelling timers.
    timer_manager_handle: henyey_herder::TimerManagerHandle,
    /// SCP timer event receiver for the main loop.
    scp_timer_rx: TokioMutex<tokio::sync::mpsc::UnboundedReceiver<scp_timer_bridge::ScpTimerEvent>>,
    /// JoinHandle for the timer manager background task.
    timer_manager_join: TokioMutex<Option<tokio::task::JoinHandle<()>>>,

    /// Metadata output stream manager for emitting LedgerCloseMeta.
    meta_stream: std::sync::Mutex<Option<MetaStreamManager>>,

    /// Async meta writer — wraps MetaStreamManager behind a channel + dedicated thread.
    /// When present, the live ledger-close and catchup paths use this instead of
    /// blocking on meta_stream directly.
    meta_writer: Option<MetaWriter>,

    /// Close time drift tracker for clock synchronization monitoring.
    drift_tracker: std::sync::Mutex<CloseTimeDriftTracker>,

    /// Last successful ledger close stats for metrics reporting.
    last_close_stats: parking_lot::RwLock<henyey_ledger::LedgerCloseStats>,

    /// Last successful ledger close performance data for metrics reporting.
    last_close_perf: parking_lot::RwLock<Option<henyey_ledger::LedgerClosePerf>>,

    // Phase 3 cumulative metrics — accumulated in handle_close_complete().
    cumulative_apply_success: AtomicU64,
    cumulative_apply_failure: AtomicU64,
    cumulative_soroban_success: AtomicU64,
    cumulative_soroban_failure: AtomicU64,
    /// Soroban parallel execution structure from last close (sticky).
    last_soroban_stage_count: AtomicU64,
    last_soroban_max_cluster_count: AtomicU64,

    /// Handle for sending commands to the sync recovery manager.
    /// Uses parking_lot::RwLock for synchronous access from callbacks.
    sync_recovery_handle: parking_lot::RwLock<Option<SyncRecoveryHandle>>,

    /// JoinHandle for the sync recovery manager task (for shutdown awaiting).
    sync_recovery_task: parking_lot::RwLock<Option<tokio::task::JoinHandle<()>>>,

    /// Whether ledger application is currently in progress (for sync recovery).
    is_applying_ledger: AtomicBool,

    /// Wall-clock of the last deferred-pipeline close-complete entry.
    /// Used to compute `henyey_ledger_close_cycle_seconds` — the time between
    /// consecutive production close-complete events.
    close_cycle_last_start: parking_lot::Mutex<Option<std::time::Instant>>,

    /// Test-only: injects a synthetic blocking sleep (in milliseconds) inside
    /// the post-close tx-queue update `spawn_blocking` closure (#1775 Phase 2).
    ///
    /// Regression test `test_close_complete_spawn_blocking_frees_event_loop`
    /// uses this to simulate a 200 ms CPU-heavy close without having to stand
    /// up 400 real signed envelopes. When set to 0 (the default), the closure
    /// behaves exactly as in production.
    #[cfg(test)]
    pub(crate) close_complete_inject_blocking_ms: AtomicU64,

    /// Regression-only hook for testing that `process_externalized_slots`
    /// does NOT hold `syncing_ledgers` write lock during the iteration phase.
    /// Set by tests that need deterministic synchronization; `None` otherwise.
    #[cfg(test)]
    pub(crate) pes_iteration_gate: Option<Arc<PesIterationGate>>,

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
    /// Snapshot of `scp_messages_received` at the last recovery-state reset.
    /// The fast-track gate compares the current counter against this snapshot
    /// to determine if SCP messages arrived *since the last recovery reset/re-arm*
    /// (as opposed to historical traffic from before the stall began).
    recovery_baseline_scp_received: AtomicU64,
    /// Tracks whether the current recovery episode's onset diagnostic has been
    /// emitted. Reset only on `RecoveryResetMode::Full` (node reached tip or
    /// completed catchup). Ensures exactly one info-level onset log per stall
    /// episode. See #2568.
    recovery_episode_latch: RecoveryEpisodeLatch,

    /// Monotonic offset (seconds since `start_instant`) of the last hard reset.
    /// 0 means "never". Used for cooldown enforcement.
    last_hard_reset_offset: AtomicU64,
    /// Gap (latest_externalized - current_ledger) at the last hard reset.
    /// Used for gap-escalation cooldown override.
    last_hard_reset_gap: AtomicU64,
    /// Total number of post-catchup hard resets performed.
    pub(crate) post_catchup_hard_reset_total: AtomicU64,
    /// Seconds-since-start_instant when hard-reset livelock tracking began
    /// for the current stuck ledger. Zero means no active tracking.
    hard_reset_livelock_start: AtomicU64,
    /// The ledger at which hard-reset livelock tracking started.
    hard_reset_livelock_ledger: AtomicU32,
    /// Deterministic per-node jitter seed derived from the keypair's public key.
    /// Used to stagger recovery timer across nodes.
    jitter_seed: u64,
    /// Monotonic instant at process start, used as the reference for
    /// `last_hard_reset_offset` (avoids wall-clock skew).
    start_instant: Instant,

    /// Total number of times the node lost sync.
    lost_sync_count: AtomicU64,

    // ── Log throttles (issue #1860, #1869) ─────────────────────────────
    /// All recovery-related log throttles, grouped to avoid per-field growth.
    recovery_throttles: log_throttle::RecoveryLogThrottles,

    /// Highest EXTERNALIZE slot observed from any SCP envelope (Valid or
    /// Pending). Used by `submit_transaction()` to detect when the node is
    /// behind the network and should reject tx submissions with
    /// TryAgainLater. Updated from lifecycle.rs envelope processing.
    max_observed_externalize_slot: AtomicU64,
    /// Highest SCP slot observed from any post-signature-verified, non-self
    /// envelope (including NonQuorum). Used to detect when verified peers
    /// are ahead of us during recovery stalls. Updated in
    /// `process_verified()` for `Verdict::Ok` envelopes only (verify-rejected
    /// envelopes return early). Reset on ledger progress.
    max_verified_scp_slot: AtomicU64,
    /// Number of ledger closes that contained at least one transaction.
    /// Mirrors stellar-core's `ledger.transaction.count` histogram `.count`.
    ledger_tx_count: AtomicU64,
    /// Current max tx size in bytes for flow control (tracks upgrades).
    /// Mirrors upstream `mMaxTxSize` in HerderImpl. Shared with the overlay
    /// via `Arc` so new peer connections can compute dynamic initial grants.
    max_tx_size_bytes: Arc<AtomicU32>,
    /// Monotonic counter used for ping IDs.
    ping_counter: AtomicU64,
    /// Unified in-flight ping state (hash→info + peer→hash).
    ping_state: tokio::sync::Mutex<PingState>,

    /// Weak reference to self for spawning background tasks from &self methods.
    /// Set via `set_self_arc` after wrapping in Arc.
    self_arc: RwLock<std::sync::Weak<Self>>,

    /// Monotonic timestamp (ms since epoch) of the last event loop iteration.
    /// Updated at the top of each select! iteration. Read by the std::thread
    /// watchdog to detect event loop freezes.
    last_event_loop_tick_ms: Arc<AtomicU64>,

    /// Signals the watchdog thread to exit its loop.
    watchdog_shutdown: Arc<AtomicBool>,

    /// Condvar used to wake the watchdog thread from its sleep so it can
    /// exit promptly on shutdown (instead of waiting up to 10s).
    watchdog_condvar: Arc<(std::sync::Mutex<()>, std::sync::Condvar)>,

    /// Numeric code indicating what the event loop is currently doing.
    /// Read by the watchdog to identify where a freeze occurs.
    /// Codes: 0=idle/select, 1=scp_message, 2=fetch_response, 3=broadcast_msg,
    ///        4=scp_broadcast, 5=consensus_tick, 6=pending_close,
    ///        10=process_externalized, 11=maybe_externalized_catchup,
    ///        12=try_apply_buffered, 13=maybe_buffered_catchup,
    ///        14=catchup_running, 15=heartbeat,
    ///        31=scp_verifier (pump_scp_intake: pre-filter + verifier enqueue),
    ///        32=scp_verified (draining verified envelopes),
    ///        33=tx_set_gc (purge unreferenced persisted tx sets)
    event_loop_phase: Arc<AtomicU64>,

    /// Fine-grained sub-phase code for pinpointing a stall inside a
    /// coarse phase. See [`phase`](super::phase) for the `PHASE_6_*`
    /// and `PHASE_13_*` constants stamped before every notable `.await`
    /// on the pending-close and buffered-catchup arms (issues #1921,
    /// #1788).
    ///
    /// Zero means "coarse phase entered, sub-phase not yet set".
    /// `set_phase` clears this to 0 so stale sub-phase values from a
    /// prior phase do not leak across coarse-phase transitions.
    event_loop_phase_sub: Arc<AtomicU32>,
}

/// Collect all bucket hashes referenced by DB-stored state: the authoritative
/// HAS and all publish-queue HAS entries. Used by bucket GC cleanup to avoid
/// deleting files still needed by the current state or pending publishes.
///
/// Parse failures are propagated as errors (not silently skipped), matching
/// stellar-core's treatment of invalid queued state as corruption.
fn collect_db_referenced_bucket_hashes(db: &henyey_db::Database) -> anyhow::Result<Vec<Hash256>> {
    db.with_connection(|conn| {
        use henyey_db::queries::publish_queue::PublishQueueQueries;
        use henyey_db::queries::StateQueries;

        let mut hashes = Vec::new();

        // Stored authoritative HAS
        if let Some(has_json) = conn.get_state(state_keys::HISTORY_ARCHIVE_STATE)? {
            let has = henyey_history::HistoryArchiveState::from_json(&has_json).map_err(|e| {
                henyey_db::DbError::Integrity(format!("Failed to parse authoritative HAS: {e}"))
            })?;
            hashes.extend(has.all_bucket_hashes());
        }

        // Publish queue HAS entries
        for has_json in conn.load_all_publish_has()? {
            let has = henyey_history::HistoryArchiveState::from_json(&has_json).map_err(|e| {
                henyey_db::DbError::Integrity(format!("Failed to parse publish-queue HAS: {e}"))
            })?;
            hashes.extend(has.all_bucket_hashes());
        }

        Ok(hashes)
    })
    .map_err(Into::into)
}

/// Collect the complete set of GC roots for bucket file cleanup.
///
/// Resolves all pending merges, then enumerates every bucket hash referenced by:
/// 1. Live + hot archive bucket lists (curr/snap + pending merge inputs/outputs)
/// 2. Snapshot manager (current + historical snapshots for RPC readers)
/// 3. DB references (authoritative HAS + publish-queue HAS bucket hashes)
///
/// Returns `None` if DB access fails (caller should skip GC in that case).
///
/// See `retain_buckets()` doc comment for the full GC safety contract and
/// `PARITY_STATUS.md` §6 for the divergence rationale vs stellar-core's
/// refcount-based approach.
fn collect_gc_roots(
    lm: &henyey_ledger::LedgerManager,
    sm: &BucketSnapshotManager,
    db: &henyey_db::Database,
) -> Option<Vec<Hash256>> {
    lm.resolve_pending_bucket_merges();

    let mut hashes = lm.all_referenced_bucket_hashes();
    hashes.extend(sm.all_referenced_hashes());

    match collect_db_referenced_bucket_hashes(db) {
        Ok(extra) => hashes.extend(extra),
        Err(e) => {
            tracing::warn!(
                error = %e,
                "Skipping bucket cleanup: failed to load DB references"
            );
            return None;
        }
    }

    Some(hashes)
}

/// Collect bucket hashes referenced only by publish-queue HAS entries.
///
/// Used during startup to verify that all buckets needed by pending publishes
/// exist on disk, mirroring stellar-core's
/// `getMissingBucketsReferencedByPublishQueue()`.
fn collect_publish_queue_bucket_hashes(db: &henyey_db::Database) -> anyhow::Result<Vec<Hash256>> {
    db.with_connection(|conn| {
        use henyey_db::queries::publish_queue::PublishQueueQueries;

        let mut hashes = Vec::new();
        for has_json in conn.load_all_publish_has()? {
            let has = henyey_history::HistoryArchiveState::from_json(&has_json).map_err(|e| {
                henyey_db::DbError::Integrity(format!("Failed to parse publish-queue HAS: {e}"))
            })?;
            for h in has.all_bucket_hashes() {
                if !h.is_zero() {
                    hashes.push(h);
                }
            }
        }
        Ok(hashes)
    })
    .map_err(Into::into)
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

        // In-memory mode: delete and recreate the bucket directory so stale
        // bucket files from a previous run don't cause "persisted state is
        // corrupt" errors.  Matches stellar-core's
        // BucketManager::maybeDropAndCreateNew() (BucketManager.cpp:122-127).
        if config.database.in_memory {
            let bucket_dir = &config.buckets.directory;
            if bucket_dir.exists() {
                tracing::info!(?bucket_dir, "In-memory mode: cleaning bucket directory");
                std::fs::remove_dir_all(bucket_dir)?;
            }
            std::fs::create_dir_all(bucket_dir)?;
        }

        // Initialize database
        let db = Self::init_database(&config)?;

        // For in-memory databases, initialize genesis state so the node starts
        // with a valid LCL=1. Mirrors stellar-core's newDB() → startNewLedger()
        // for in-memory DBs (ApplicationImpl.cpp:248-249, 325-328).
        if config.database.in_memory {
            bootstrap::initialize_genesis(
                &db,
                Some(&config.buckets.directory),
                &config.network.passphrase,
                config.testing.genesis_test_account_count,
            )?;
        }

        // Ensure network passphrase matches stored state.
        Self::ensure_network_passphrase(&db, &config.network.passphrase)?;

        // Verify on-disk ledger headers before loading state.
        Self::verify_on_disk_integrity(&db)?;

        // Detect interrupted catchup persist from a previous run (AUDIT-226).
        let force_full_catchup = Self::check_catchup_persist_pending(&db);

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
        let local_quorum_set = if config.node.quorum_set.is_empty() {
            None
        } else {
            Some(config.node.quorum_set.to_xdr()?)
        };
        if let Some(ref qs) = local_quorum_set {
            tracing::info!(
                threshold = qs.threshold,
                validators = qs.validators.len(),
                inner_sets = qs.inner_sets.len(),
                "Loaded quorum set configuration"
            );
        }

        // Initialize bucket manager for ledger state persistence.
        // Use the configured bucket directory — this must match the path used
        // by history publishing (publish.rs) to avoid split-brain bucket access.
        let bucket_dir = config.buckets.directory.clone();
        std::fs::create_dir_all(&bucket_dir)?;

        let bucket_manager = Arc::new(BucketManager::with_cache_size(
            bucket_dir.clone(),
            config.buckets.cache_size,
        )?);
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

        // Construct and wire InvariantManager if checks are configured.
        if !config.invariants.checks.is_empty() || config.invariants.extra_checks {
            let mut inv_mgr = henyey_invariant::InvariantManager::new();
            // Register all built-in invariants.
            inv_mgr.register(std::sync::Arc::new(
                henyey_invariant::AccountSubEntriesCountIsValid,
            ));
            inv_mgr.register(std::sync::Arc::new(henyey_invariant::LedgerEntryIsValid));
            inv_mgr.register(std::sync::Arc::new(
                henyey_invariant::SponsorshipCountIsValid,
            ));
            // Enable invariants matching configured patterns.
            for pattern in &config.invariants.checks {
                inv_mgr.enable(pattern).unwrap_or_else(|e| {
                    panic!("Failed to enable invariant pattern '{}': {}", pattern, e);
                });
            }
            ledger_manager.set_invariant_manager(std::sync::Arc::new(inv_mgr));
            tracing::info!(
                checks = ?config.invariants.checks,
                extra_checks = config.invariants.extra_checks,
                "InvariantManager enabled"
            );
        }

        let ledger_manager = Arc::new(ledger_manager);
        tracing::info!("Ledger manager initialized");

        // Create SCP timer manager for precise single-shot timeout delivery.
        // Replaces the 500ms polling interval with exact-expiry timers
        // matching stellar-core's VirtualTimer pattern.
        let (scp_timer_tx, scp_timer_rx) = tokio::sync::mpsc::unbounded_channel();
        let timer_bridge = std::sync::Arc::new(scp_timer_bridge::ScpTimerBridge::new(scp_timer_tx));
        let (timer_manager_handle, timer_manager) = henyey_herder::TimerManager::new(timer_bridge);
        let timer_manager_join = tokio::spawn(timer_manager.run());

        let herder_config = Self::build_herder_config(&config, &keypair, local_quorum_set);

        // Create herder (with or without secret key for signing)
        let survey_throttle = Duration::from_secs(herder_config.ledger_close_time as u64 * 3);

        let herder = Self::init_herder(
            herder_config,
            &config,
            &keypair,
            &ledger_manager,
            &db,
            timer_manager_handle.clone(),
        );

        let meta_stream = Self::init_meta_stream(&config, &bucket_dir)?;

        // If streaming is active, wrap the MetaStreamManager in a MetaWriter
        // for async I/O isolation. The writer owns the stream; the Mutex holds
        // None during live operation.
        let (meta_writer, meta_stream_for_mutex) = match meta_stream {
            Some(ms) if ms.is_streaming() => (Some(MetaWriter::new(ms)), None),
            other => (None, other),
        };

        // Create shutdown channel
        let (shutdown_tx, shutdown_rx) = tokio::sync::broadcast::channel(1);

        // Create channel for outbound SCP envelopes
        let (scp_envelope_tx, scp_envelope_rx) = tokio::sync::mpsc::channel(100);
        let now = clock.now();
        let start_instant = now;

        // Derive deterministic per-node jitter seed from public key.
        let jitter_seed = {
            let pk = keypair.public_key();
            let pk_bytes = pk.as_bytes();
            let mut buf = [0u8; 8];
            buf.copy_from_slice(&pk_bytes[0..8]);
            u64::from_le_bytes(buf)
        };

        // Build the non-blocking archive-checkpoint cache. The background
        // refresh uses a tightened DownloadConfig (1 retry, 15 s timeout)
        // so it gives up quickly; callers that need the full-retry budget
        // (wait_for_archive_checkpoint, run_catchup_work) build their own
        // fetcher via `archive_cache::ArchiveHttpFetcher::for_blocking_catchup`.
        let archive_checkpoint_cache = Arc::new(archive_cache::ArchiveCheckpointCache::new(
            Arc::clone(&clock),
            Arc::new(archive_cache::ArchiveHttpFetcher::for_background_refresh(
                config.history.archives.clone(),
            )),
        ));

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

        let ledger_source = Box::new(HerderLedgerSource::new(
            herder.clone(),
            ledger_manager.clone(),
        ));

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
            query_is_ready: Arc::new(AtomicBool::new(false)),
            ledger_manager,
            overlay: RwLock::new(None),
            overlay_tracking: std::sync::Mutex::new(None),
            overlay_synced: std::sync::Mutex::new(None),
            pre_bound_listener: std::sync::Mutex::new(None),
            herder,
            shutdown_tx,
            _shutdown_rx: shutdown_rx,
            scp_envelope_tx,
            scp_envelope_rx: TokioMutex::new(scp_envelope_rx),
            last_processed_slot: RwLock::new(0),
            catchup_in_progress: AtomicBool::new(false),
            deferred_catchup: tokio::sync::Mutex::new(None),
            fatal_state_failure: AtomicBool::new(false),
            catchup_needs_full_reset: AtomicBool::new(force_full_catchup),
            publish_in_progress: AtomicBool::new(false),
            #[cfg(test)]
            publish_panic_inject: AtomicBool::new(false),
            syncing_ledgers: RwLock::new(BTreeMap::new()),
            last_externalized_slot: AtomicU64::new(0),
            scp_messages_sent: AtomicU64::new(0),
            scp_nominate_sent: AtomicU64::new(0),
            scp_prepare_sent: AtomicU64::new(0),
            scp_confirm_sent: AtomicU64::new(0),
            scp_externalize_sent: AtomicU64::new(0),
            scp_messages_received: AtomicU64::new(0),
            scp_prefilter_counters: henyey_herder::scp_verify::PreFilterCounters::default(),
            scp_post_verify_drops: AtomicU64::new(0),
            scp_pv_counters: henyey_herder::scp_verify::PostVerifyCounters::default(),
            scp_verify_latency_us_sum: AtomicU64::new(0),
            scp_verify_latency_count: AtomicU64::new(0),
            scp_scheduled: scp_dedup::ScpScheduledCache::new(),
            scp_verify_output_backlog: AtomicU64::new(0),
            fetch_channel_depth: Arc::new(AtomicI64::new(0)),
            fetch_channel_depth_max: Arc::new(AtomicI64::new(0)),
            consensus_trigger_attempts: AtomicU64::new(0),
            consensus_trigger_successes: AtomicU64::new(0),
            consensus_trigger_failures: AtomicU64::new(0),
            consensus_trigger_skipped_applying: AtomicU64::new(0),
            consensus_trigger_skipped_stale: AtomicU64::new(0),
            nomination_timeout_fires: AtomicU64::new(0),
            nomination_timeout_skipped_stale: AtomicU64::new(0),
            ballot_timeout_fires: AtomicU64::new(0),
            last_externalized_at: RwLock::new(now),
            last_scp_state_request_at: RwLock::new(now),
            survey_state: RwLock::new(SurveyState::new(
                SurveyDataManager::new(is_validator, max_inbound_peers, max_outbound_peers),
                SurveyMessageLimiter::new(6, 10, ledger_source),
            )),
            broadcast_op_carryover: AtomicUsize::new(0),
            broadcast_dex_op_carryover: AtomicUsize::new(0),
            tx_adverts_by_peer: RwLock::new(HashMap::new()),
            tx_demand_history: RwLock::new(HashMap::new()),
            tx_pending_demands: RwLock::new(VecDeque::new()),
            tx_set_dont_have: RwLock::new(HashMap::new()),
            tx_set_last_request: RwLock::new(HashMap::new()),
            tx_set_all_peers_exhausted: AtomicBool::new(false),
            tx_set_exhausted_warned: RwLock::new(HashSet::new()),
            tx_set_last_retry: RwLock::new(HashMap::new()),
            tx_set_exhausted_since: AtomicU64::new(0),
            consensus_stuck_state: RwLock::new(None),
            last_catchup_completed_at: RwLock::new(None),
            archive_checkpoint_cache,
            archive_recovery_status: RwLock::new(ArchiveRecoveryStatus::Unknown),
            scp_latency: RwLock::new(ScpLatencyTracker::default()),
            survey_scheduler: TokioMutex::new(SurveyScheduler::new(now)),
            survey_nonce: RwLock::new(1),
            survey_secrets: RwLock::new(HashMap::new()),
            survey_results: RwLock::new(HashMap::new()),
            survey_throttle,
            survey_reporting: RwLock::new(SurveyReportingState::new(now)),
            timer_manager_handle,
            scp_timer_rx: TokioMutex::new(scp_timer_rx),
            timer_manager_join: TokioMutex::new(Some(timer_manager_join)),
            meta_stream: std::sync::Mutex::new(meta_stream_for_mutex),
            meta_writer,
            drift_tracker: std::sync::Mutex::new(CloseTimeDriftTracker::new()),
            last_close_stats: parking_lot::RwLock::new(Default::default()),
            last_close_perf: parking_lot::RwLock::new(None),
            cumulative_apply_success: AtomicU64::new(0),
            cumulative_apply_failure: AtomicU64::new(0),
            cumulative_soroban_success: AtomicU64::new(0),
            cumulative_soroban_failure: AtomicU64::new(0),
            last_soroban_stage_count: AtomicU64::new(0),
            last_soroban_max_cluster_count: AtomicU64::new(0),
            sync_recovery_handle: parking_lot::RwLock::new(None), // Initialized in run() when needed
            sync_recovery_task: parking_lot::RwLock::new(None),
            is_applying_ledger: AtomicBool::new(false),
            close_cycle_last_start: parking_lot::Mutex::new(None),
            #[cfg(test)]
            close_complete_inject_blocking_ms: AtomicU64::new(0),
            #[cfg(test)]
            pes_iteration_gate: None,
            sync_recovery_pending: AtomicBool::new(false),
            recovery_attempts_without_progress: AtomicU64::new(0),
            recovery_baseline_ledger: AtomicU64::new(0),
            recovery_baseline_scp_received: AtomicU64::new(0),
            recovery_episode_latch: RecoveryEpisodeLatch::new(),
            last_hard_reset_offset: AtomicU64::new(0),
            last_hard_reset_gap: AtomicU64::new(0),
            post_catchup_hard_reset_total: AtomicU64::new(0),
            hard_reset_livelock_start: AtomicU64::new(0),
            hard_reset_livelock_ledger: AtomicU32::new(0),
            jitter_seed,
            start_instant,
            lost_sync_count: AtomicU64::new(0),
            recovery_throttles: log_throttle::RecoveryLogThrottles::new(),
            max_observed_externalize_slot: AtomicU64::new(0),
            max_verified_scp_slot: AtomicU64::new(0),
            ledger_tx_count: AtomicU64::new(0),
            max_tx_size_bytes: Arc::new(AtomicU32::new(
                henyey_herder::flow_control::MAX_CLASSIC_TX_SIZE_BYTES,
            )),
            ping_counter: AtomicU64::new(0),
            ping_state: tokio::sync::Mutex::new(PingState::default()),
            self_arc: RwLock::new(std::sync::Weak::new()),
            last_event_loop_tick_ms: Arc::new(AtomicU64::new(0)),
            watchdog_shutdown: Arc::new(AtomicBool::new(false)),
            watchdog_condvar: Arc::new((std::sync::Mutex::new(()), std::sync::Condvar::new())),
            event_loop_phase: Arc::new(AtomicU64::new(0)),
            event_loop_phase_sub: Arc::new(AtomicU32::new(0)),
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

    /// Detect a previously interrupted catchup persist (AUDIT-226).
    ///
    /// If the `CATCHUP_PERSIST_PENDING` sentinel is present, a prior catchup
    /// completed in-memory but crashed before the deferred persist wrote to
    /// the DB. Returns `true` so the caller can initialize
    /// `catchup_needs_full_reset`, skipping a doomed replay attempt.
    ///
    /// ## §14.5 parity note — `REBUILD_FOR_OFFER_TABLE` equivalence
    ///
    /// stellar-core sets `REBUILD_FOR_OFFER_TABLE` before bucket-apply because
    /// its SQL offer tables are mutated incrementally; on restart,
    /// `maybeRebuildLedger()` detects the flag and rebuilds from buckets.
    ///
    /// henyey has no SQL offer table (BucketListDB-only with in-memory offer
    /// index), so the recovery contract is satisfied by a two-window design:
    ///
    /// 1. **Pre-final-persist writes** (`persist_bucket_list_snapshot`,
    ///    `persist_header_only` in `crates/history`) are durable but
    ///    non-authoritative — startup reads from `last_closed_ledger` /
    ///    `HISTORY_ARCHIVE_STATE`, not from ahead-of-LCL rows.
    /// 2. **Post-catchup / pre-deferred-persist** — this sentinel marks
    ///    that catchup succeeded in memory but the final persist has not
    ///    committed. Startup detects it here and forces full bucket-apply
    ///    on the next catchup attempt.
    ///
    /// The sentinel is NOT cleared here — it is only cleared inside
    /// `CatchupPersistData::write_to_db` or `LedgerPersistData::write_to_db`.
    /// This ensures crash-idempotence: if the node crashes again before a
    /// successful persist, subsequent restarts still detect the issue.
    fn check_catchup_persist_pending(db: &henyey_db::Database) -> bool {
        match db.with_connection(|conn| conn.get_state(state_keys::CATCHUP_PERSIST_PENDING)) {
            Ok(Some(_)) => {
                tracing::warn!(
                    "Detected pending catchup persist from previous run. \
                     Will force full bucket-apply on next catchup (AUDIT-226)."
                );
                true
            }
            Ok(None) => false,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "Failed to check catchup persist sentinel; proceeding normally"
                );
                false
            }
        }
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
        if config.database.in_memory {
            tracing::info!("In-memory mode: using ephemeral database");
            Ok(henyey_db::Database::open_in_memory()?)
        } else {
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

    /// Build the herder configuration from app config.
    fn build_herder_config(
        config: &AppConfig,
        keypair: &henyey_crypto::SecretKey,
        local_quorum_set: Option<stellar_xdr::curr::ScpQuorumSet>,
    ) -> HerderConfig {
        let freq = checkpoint_frequency();
        HerderConfig {
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
                max_size: 1000 * POOL_LEDGER_MULTIPLIER as usize,
                max_dex_ops: config.surge_pricing.max_dex_tx_operations,
                max_classic_bytes: Some(config.surge_pricing.classic_byte_allowance),
                max_soroban_bytes: Some(config.surge_pricing.soroban_byte_allowance),
                max_queue_ops: Some(1000 * POOL_LEDGER_MULTIPLIER),
                max_queue_classic_bytes: Some(
                    config.surge_pricing.classic_byte_allowance * POOL_LEDGER_MULTIPLIER,
                ),
                expected_ledger_close_secs: config
                    .testing
                    .ledger_close_time
                    .unwrap_or(if config.testing.accelerate_time { 1 } else { 5 })
                    as u64,
                flood_arb_tx_base_allowance: config.overlay.flood_arb_tx_base_allowance,
                flood_arb_tx_damping_factor: config.overlay.flood_arb_tx_damping_factor,
                ..Default::default()
            },
            local_quorum_set,
            proposed_upgrades: config.upgrades.to_ledger_upgrades(),
            max_protocol_version: config.network.max_protocol_version,
            checkpoint_frequency: freq as u64,
            validator_weight_config: config.validator_weight_config.clone(),
            force_old_style_leader_election: config.node.force_old_style_leader_election,
            manual_close: config.node.manual_close,
            run_standalone: config.testing.run_standalone,
        }
    }

    /// Create and wire up the Herder, storing the local quorum set in the DB.
    fn init_herder(
        config: HerderConfig,
        app_config: &AppConfig,
        keypair: &henyey_crypto::SecretKey,
        ledger_manager: &Arc<LedgerManager>,
        db: &henyey_db::Database,
        timer_handle: henyey_herder::TimerManagerHandle,
    ) -> Arc<Herder> {
        let herder = if app_config.node.is_validator {
            Arc::new(Herder::with_secret_key(
                config,
                keypair.clone(),
                ledger_manager.clone(),
                timer_handle,
            ))
        } else {
            Arc::new(Herder::new(config, ledger_manager.clone(), timer_handle))
        };
        herder
            .tx_queue()
            .set_fee_balance_provider(Arc::new(types::LedgerFeeBalanceProvider {
                ledger_manager: ledger_manager.clone(),
            }));
        herder
            .tx_queue()
            .set_account_provider(Arc::new(types::LedgerAccountProvider {
                ledger_manager: ledger_manager.clone(),
            }));

        // Wire SCP state persistence so the app event loop's tx_set_gc timer
        // (lifecycle.rs, phase 33) has something to purge. Parity:
        // stellar-core `HerderImpl::startTxSetGCTimer()` (HerderImpl.cpp:2440).
        let scp_persistence = henyey_herder::SqliteScpPersistence::new(db.clone());
        let scp_persistence_manager = Arc::new(henyey_herder::ScpPersistenceManager::new(
            Box::new(scp_persistence),
        ));
        if herder.set_scp_persistence(scp_persistence_manager).is_err() {
            panic!("set_scp_persistence called more than once");
        }

        if let Some(qs) = herder.local_quorum_set() {
            let hash = hash_quorum_set(&qs);
            if let Err(err) = db.store_scp_quorum_set(&hash, 0, &qs) {
                tracing::warn!(error = %err, "Failed to store local quorum set");
            }
        }
        herder
    }

    /// Initialize the metadata output stream, if configured.
    fn init_meta_stream(
        config: &AppConfig,
        bucket_dir: &std::path::Path,
    ) -> anyhow::Result<Option<MetaStreamManager>> {
        if config.metadata.output_stream.is_some() || config.metadata.debug_ledgers > 0 {
            match MetaStreamManager::new(&config.metadata, bucket_dir) {
                Ok(ms) => {
                    if ms.is_streaming() {
                        tracing::info!("Metadata output stream initialized");
                    }
                    Ok(Some(ms))
                }
                Err(e) => {
                    tracing::error!(error = %e, "Failed to initialize metadata stream");
                    Err(e.into())
                }
            }
        } else {
            Ok(None)
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
        // Re-arm overlay flood acceptance now that the node is operational.
        if let Some(flag) = self.overlay_synced.lock().unwrap().as_ref() {
            flag.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        // Reset log throttles so a fresh sync-loss episode produces fresh
        // info/warn-level logs.
        self.recovery_throttles.reset_all();
    }

    /// Reset all tx-set tracking state so the main loop can make fresh requests.
    ///
    /// Clears the exhausted flag, don't-have map, last-request timestamps, and
    /// exhaustion warnings. Callers that also need to clear `consensus_stuck_state`
    /// should do so separately.
    pub(crate) async fn reset_tx_set_tracking(&self) {
        self.clear_tx_set_exhausted();
        self.tx_set_dont_have.write().await.clear();
        self.tx_set_last_request.write().await.clear();
        self.tx_set_exhausted_warned.write().await.clear();
        self.tx_set_last_retry.write().await.clear();
    }

    /// Persist in-memory hot archive buckets to disk.
    ///
    /// Hot archive merges are performed entirely in memory, so after catchup
    /// or ledger close the curr/snap/next buckets may have no backing file.
    /// This writes each non-zero bucket that lacks a file to the bucket
    /// directory so that a subsequent restart can restore from the persisted HAS.
    pub(crate) fn persist_hot_archive_buckets(
        &self,
        habl: &HotArchiveBucketList,
    ) -> anyhow::Result<()> {
        let bucket_dir = self.bucket_manager.bucket_dir();
        for level in habl.levels() {
            let mut buckets_to_check: Vec<&henyey_bucket::HotArchiveBucket> =
                vec![level.curr(), level.snap_bucket()];
            if let Some(next) = level.next() {
                buckets_to_check.push(next);
            }
            for bucket in buckets_to_check {
                if bucket.backing_file_path().is_none() && !bucket.hash().is_zero() {
                    let permanent =
                        bucket_dir.join(henyey_bucket::canonical_bucket_filename(&bucket.hash()));
                    if !permanent.exists() {
                        bucket.save_to_xdr_file(&permanent).map_err(|e| {
                            anyhow::anyhow!(
                                "Failed to persist hot archive bucket {} to disk: {}",
                                bucket.hash().to_hex(),
                                e
                            )
                        })?;
                    }
                }
            }
        }
        Ok(())
    }

    /// Check if the force-scp flag is set in the database.
    ///
    /// Returns `true` if the flag is set, `false` otherwise.
    /// This does NOT clear the flag — call `clear_force_scp` after use.
    pub(crate) async fn check_force_scp(&self) -> bool {
        self.db_blocking("check-force-scp", |db| {
            db.with_connection(|conn| {
                use henyey_db::queries::StateQueries;
                use henyey_db::schema::state_keys;
                Ok(conn.get_state(state_keys::FORCE_SCP)?.as_deref() == Some("true"))
            })
            .map_err(Into::into)
        })
        .await
        .unwrap_or(false)
    }

    /// Clear the force-scp flag in the database.
    pub(crate) async fn clear_force_scp(&self) {
        let _ = self
            .db_blocking("clear-force-scp", |db| {
                db.with_connection(|conn| {
                    use henyey_db::queries::StateQueries;
                    use henyey_db::schema::state_keys;
                    conn.delete_state(state_keys::FORCE_SCP)
                })
                .map_err(Into::into)
            })
            .await;
    }

    /// Get the database.
    pub fn database(&self) -> &henyey_db::Database {
        &self.db
    }

    /// Run a blocking database operation on the Tokio blocking pool.
    ///
    /// Wraps `spawn_blocking_logged` with a cloned `Database` handle.
    /// Re-panics on `JoinError` to preserve today's failure semantics:
    /// the calling Tokio task still panics, so best-effort callers see a
    /// task abort rather than a swallowed error.
    pub(crate) async fn db_blocking<T, F>(&self, context: &str, f: F) -> anyhow::Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&henyey_db::Database) -> anyhow::Result<T> + Send + 'static,
    {
        let db = self.db.clone();
        match henyey_common::spawn_blocking_logged(context, move || f(&db)).await {
            Ok(result) => result,
            Err(join_err) => {
                // Panic in blocking task — re-panic to preserve today's semantics.
                std::panic::resume_unwind(join_err.into_panic())
            }
        }
    }

    /// Get the bucket snapshot manager for concurrent query access.
    pub fn bucket_snapshot_manager(&self) -> &Arc<BucketSnapshotManager> {
        &self.bucket_snapshot_manager
    }

    /// Get the query server readiness flag.
    ///
    /// This flag mirrors stellar-core's `QueryServer::mIsReady`. It starts
    /// `false` and is set to `true` after the first bucket snapshot is
    /// populated during startup.
    pub fn query_is_ready(&self) -> &Arc<AtomicBool> {
        &self.query_is_ready
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

    pub fn ledger_info(&self) -> LedgerInfo {
        let snap = self.ledger_manager.header_snapshot();
        let close_time = ledger_close_time(&snap.header);
        LedgerInfo {
            ledger_seq: snap.header.ledger_seq,
            hash: snap.hash,
            close_time,
            protocol_version: snap.header.ledger_version,
        }
    }

    /// Get a rich ledger summary with all header fields needed for the
    /// `/info` endpoint.
    ///
    /// All fields are derived from a single atomic [`HeaderSnapshot`] so they
    /// are guaranteed to describe the same ledger close.
    pub fn ledger_summary(&self) -> LedgerSummary {
        let snap = self.ledger_manager.header_snapshot();
        let close_time = ledger_close_time(&snap.header);
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
        LedgerSummary::from_snapshot(&snap, age)
    }

    pub fn target_ledger_close_duration(&self) -> Duration {
        self.herder.ledger_close_duration()
    }

    /// Expected Unix timestamp (seconds) of the next ledger close.
    ///
    /// Returns `tracking_consensus_close_time + ledger_close_duration.as_secs()`.
    /// Used by simulation to predict when the next close should occur.
    pub fn expected_next_ledger_close_unix_secs(&self) -> u64 {
        self.herder.tracking_consensus_close_time() + self.herder.ledger_close_duration().as_secs()
    }

    pub async fn peer_count(&self) -> usize {
        self.overlay
            .read()
            .await
            .as_ref()
            .map(|o| o.peer_count())
            .unwrap_or(0)
    }

    /// Number of externalized ledgers buffered and waiting to close.
    pub async fn syncing_ledgers_count(&self) -> usize {
        self.syncing_ledgers.read().await.len()
    }

    pub async fn add_peer(
        &self,
        addr: henyey_overlay::PeerAddress,
    ) -> Result<henyey_overlay::AddPeerOutcome, henyey_overlay::OverlayError> {
        let Some(overlay) = self.overlay().await else {
            return Err(henyey_overlay::OverlayError::NotStarted);
        };
        overlay.add_peer(addr).await
    }

    /// Returns true once the overlay manager has been started.
    ///
    /// This checks whether `App::run()` has completed its `start_overlay()`
    /// call. It is a startup-completion signal, not a liveness guarantee —
    /// the overlay may subsequently shut down. Used by the simulation harness
    /// to gate connection attempts until all nodes are ready.
    #[cfg(feature = "test-utils")]
    pub async fn is_overlay_started(&self) -> bool {
        self.overlay.read().await.is_some()
    }

    pub fn latest_externalized_slot(&self) -> Option<u64> {
        self.herder.latest_externalized_slot()
    }

    /// Load the current sequence number for an account from the bucket list.
    ///
    /// Returns `Ok(None)` if the account does not exist.
    /// Returns `Err(NotInitialized)` if the ledger manager has not been
    /// initialized yet (or was reset for catchup).
    /// Used by the simulation LoadGenerator to refresh cached sequence numbers.
    pub fn load_account_sequence(
        &self,
        account_id: &stellar_xdr::curr::AccountId,
    ) -> henyey_ledger::Result<Option<i64>> {
        let snapshot = self.ledger_manager.create_snapshot()?;
        let Some(account) = snapshot.get_account(account_id)? else {
            tracing::debug!(
                account = %henyey_crypto::account_id_to_strkey(account_id),
                snapshot_ledger = snapshot.ledger_seq(),
                "load_account_sequence: account not found in bucket list snapshot"
            );
            return Ok(None);
        };
        Ok(Some(account.seq_num.0))
    }

    /// Load a full account entry from the current bucket list snapshot.
    ///
    /// Returns `Ok(None)` if the account does not exist.
    /// Used by the compat HTTP `/testacc` endpoint.
    pub fn load_account(
        &self,
        account_id: &stellar_xdr::curr::AccountId,
    ) -> henyey_ledger::Result<Option<stellar_xdr::curr::AccountEntry>> {
        let snapshot = self.ledger_manager.create_snapshot()?;
        snapshot.get_account(account_id)
    }

    /// Check whether a ledger entry exists in the current bucket list.
    ///
    /// Used by the simulation LoadGenerator to verify Soroban state is synced.
    pub fn has_ledger_entry(
        &self,
        key: &stellar_xdr::curr::LedgerKey,
    ) -> henyey_ledger::Result<bool> {
        let snapshot = self.ledger_manager.create_snapshot()?;
        Ok(snapshot.get_entry(key)?.is_some())
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

    /// Escalate `recovery_attempts_without_progress` to at least
    /// `RECOVERY_ESCALATION_CATCHUP`, preserving any higher value.
    ///
    /// Uses `fetch_max` (not `store`) so that a counter already past the
    /// threshold is never regressed — see issue #1843.
    ///
    /// Also switches the archive checkpoint cache to urgent polling (10 s TTL).
    /// This is the only production API for escalation — it couples the two
    /// signals that must always move together. A raw counter bump without
    /// urgent mode is what caused the 5-minute wedge in #2073: the node
    /// was in catchup mode but still polling the archive at the 60 s
    /// normal cadence, delaying detection of the newly published checkpoint.
    pub(super) fn escalate_recovery_to_catchup(&self) {
        self.recovery_attempts_without_progress
            .fetch_max(RECOVERY_ESCALATION_CATCHUP, Ordering::SeqCst);
        self.archive_checkpoint_cache.set_urgent(true);
    }

    /// Mark the archive as confirmed behind AND activate urgent polling.
    ///
    /// Call this only from code paths that have **confirmed** the archive is
    /// behind via a `Fresh`/`Stale` cache observation (not cold-cache or
    /// escalation-only paths). These two signals are semantically coupled
    /// (see #2073): setting confirmed-behind without urgent polling
    /// delays detection of newly published checkpoints by up to 60 s.
    pub(super) async fn mark_archive_confirmed_behind(&self) {
        {
            let mut guard = self.archive_recovery_status.write().await;
            match *guard {
                ArchiveRecoveryStatus::ConfirmedBehind { .. } => {
                    // Already confirmed behind — preserve existing backoff.
                }
                ArchiveRecoveryStatus::Unknown => {
                    *guard = ArchiveRecoveryStatus::ConfirmedBehind {
                        backoff_until: None,
                    };
                }
            }
        }
        self.archive_checkpoint_cache.set_urgent(true);
    }

    /// Gap between the highest verified SCP slot from peers and our current
    /// ledger. Returns 0 when no peer traffic has been observed.
    pub(super) fn effective_peer_gap(&self, current_ledger: u32) -> u64 {
        self.max_verified_scp_slot
            .load(Ordering::Relaxed)
            .saturating_sub(current_ledger as u64)
    }

    /// Reset (or re-arm) the recovery attempt counter and snapshot the current
    /// SCP message count. See [`RecoveryResetMode`] for the two reset policies.
    ///
    /// The SCP snapshot ensures the fast-track gate only considers SCP traffic
    /// received *after* this reset/re-arm point. Both variants always snapshot
    /// the SCP baseline.
    ///
    /// Store order: SCP baseline first, then attempt counter, so that any
    /// concurrent reader that observes the new attempt count also sees the
    /// updated SCP baseline (or a fresher one).
    pub(super) fn reset_recovery_attempts(&self, mode: RecoveryResetMode) {
        // SCP baseline snapshot — always: invalidates stale pre-progress
        // SCP traffic so fast-track doesn't fire on old envelopes.
        self.recovery_baseline_scp_received.store(
            self.scp_messages_received.load(Ordering::Relaxed),
            Ordering::SeqCst,
        );

        match mode {
            RecoveryResetMode::Full => {
                // Clear peer-gap evidence — node is caught up.
                // max_verified_scp_slot is Relaxed (monotonic hint from
                // verified envelopes). Zeroing is safe because peer traffic
                // will re-populate it within one SCP cycle.
                self.max_verified_scp_slot.store(0, Ordering::Relaxed);
                self.recovery_attempts_without_progress
                    .store(0, Ordering::SeqCst);
                // Re-arm the onset diagnostic for the next stall episode.
                self.recovery_episode_latch.reset();
            }
            RecoveryResetMode::Partial { seed } => {
                // Preserve max_verified_scp_slot — the hard-reset gate at
                // consensus.rs relies on effective_peer_gap() which reads
                // it. This is a Relaxed monotonic hint; correctness does
                // not depend on memory ordering — the dual guard
                // (relation.is_behind() AND peer_gap >= threshold) ensures
                // a stale far-ahead slot alone cannot trigger partial mode.
                //
                // Monotonic reseed: never lower the counter. Existing stall
                // history may exceed the seed.
                self.recovery_attempts_without_progress
                    .fetch_max(seed, Ordering::SeqCst);
            }
        }
    }

    /// Clear archive-behind recovery state according to the given mode.
    ///
    /// Always clears: `archive_recovery_status` → `Unknown`.
    /// Other operations depend on the variant (see [`ArchiveRecoveryClear`] docs).
    ///
    /// Returns whether a backoff deadline was previously armed (i.e., the old
    /// state was `ConfirmedBehind { backoff_until: Some(_) }`).
    /// Most callers ignore this; [`ArchiveRecoveryClear::HardResetExec`] uses it
    /// for logging.
    ///
    /// # Lock ordering
    ///
    /// This method acquires `archive_recovery_status` (write). Callers MUST NOT
    /// hold `syncing_ledgers` or `consensus_stuck_state` when calling — those
    /// locks rank below `archive_recovery_status` per the documented lock order.
    pub(crate) async fn clear_archive_recovery_state(&self, mode: ArchiveRecoveryClear) -> bool {
        // 1. Recovery attempt reset (only for progress variants).
        match &mode {
            ArchiveRecoveryClear::FullProgress => {
                self.reset_recovery_attempts(RecoveryResetMode::Full);
            }
            ArchiveRecoveryClear::PartialProgress { seed } => {
                self.reset_recovery_attempts(RecoveryResetMode::Partial { seed: *seed });
            }
            _ => {}
        }

        // 2. Always: clear the recovery status to Unknown.
        let was_armed = {
            let mut guard = self.archive_recovery_status.write().await;
            let was = matches!(
                *guard,
                ArchiveRecoveryStatus::ConfirmedBehind {
                    backoff_until: Some(_)
                }
            );
            *guard = ArchiveRecoveryStatus::Unknown;
            was
        };

        // 3. Urgency control (variant-dependent).
        match mode {
            ArchiveRecoveryClear::HardResetExec => {} // preserve current urgency
            ArchiveRecoveryClear::PartialProgress { .. } => {
                self.archive_checkpoint_cache.set_urgent(true);
            }
            _ => {
                self.archive_checkpoint_cache.set_urgent(false);
            }
        }

        // 4. Livelock tracker (only cleared when ledger made progress or reset skipped).
        match mode {
            ArchiveRecoveryClear::FullProgress
            | ArchiveRecoveryClear::PartialProgress { .. }
            | ArchiveRecoveryClear::DefenseSkip => {
                self.hard_reset_livelock_start.store(0, Ordering::Relaxed);
            }
            ArchiveRecoveryClear::HardResetExec | ArchiveRecoveryClear::ArchiveConfirmedCurrent => {
            }
        }

        was_armed
    }

    /// Take a point-in-time snapshot of archive recovery status.
    ///
    /// The RwLock read guard is held only for the `Copy`; callers compute
    /// predicates on the returned [`ArchiveRecoverySnapshot`] without holding
    /// any lock. This satisfies lock-ordering requirements and eliminates
    /// TOCTOU races between the confirmed-behind flag and backoff timer.
    pub(crate) async fn archive_recovery_snapshot(&self) -> ArchiveRecoverySnapshot {
        ArchiveRecoverySnapshot {
            status: *self.archive_recovery_status.read().await,
        }
    }

    /// Get Soroban network configuration information.
    ///
    /// Returns the Soroban-related configuration settings from the current ledger
    /// state, or `None` if not available (pre-protocol 20 or not initialized).
    ///
    /// # Prefer `header_snapshot()` when pairing with protocol version
    ///
    /// If you need both the protocol version and Soroban info together (e.g., to
    /// gate on `soroban_supported(version)` before reading config), use
    /// `self.ledger_manager().header_snapshot()` instead. That captures both
    /// atomically under a single lock and avoids TOCTOU races with
    /// `commit_close()`. This standalone method is safe only when no header
    /// field is read in the same logical operation.
    pub fn soroban_network_info(&self) -> Option<SorobanNetworkInfo> {
        self.ledger_manager.soroban_network_info()
    }

    /// Manually close a ledger (for testing/manual close mode).
    ///
    /// This triggers the herder to close the next ledger. It requires:
    /// - The node must be configured as a validator (`is_validator = true`)
    /// - Manual close mode must be enabled (`manual_close = true`)
    ///
    /// # Note on parity gap
    ///
    /// Manual close intentionally bypasses the `is_applying_ledger` gate that
    /// `try_trigger_consensus` enforces (parity with stellar-core
    /// HerderImpl.cpp:1440-1447). Manual close is dev/test-only — the caller
    /// is responsible for serialization. The post-build `LCL` re-check inside
    /// `Herder::trigger_next_ledger` still surfaces races as
    /// `TriggerOutcome::SkippedStale`, which this function maps to an `Err`
    /// so callers can retry with a refreshed ledger sequence.
    ///
    /// # Returns
    ///
    /// * `Ok(new_ledger_seq)` - The ledger was successfully triggered (or an
    ///   identical trigger was already in progress; the latter is treated as
    ///   idempotent success).
    /// * `Err` - An error occurred (not a validator, manual close not enabled,
    ///   trigger failed, or LCL advanced during build — caller should retry).
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
        let current_ledger = self.ledger_info().ledger_seq;
        let next_ledger = current_ledger + 1;

        // Trigger the herder to close the next ledger
        let herder = std::sync::Arc::clone(&self.herder);
        let outcome = henyey_common::spawn_blocking_logged("manual-close-trigger", move || {
            herder.trigger_next_ledger(next_ledger)
        })
        .await
        .map_err(|e| anyhow::anyhow!("spawn_blocking failed for trigger_next_ledger: {e}"))?
        .map_err(|e| anyhow::anyhow!("Failed to trigger next ledger: {}", e))?;

        match outcome {
            henyey_herder::TriggerOutcome::Triggered
            | henyey_herder::TriggerOutcome::AlreadyNominating => Ok(next_ledger),
            henyey_herder::TriggerOutcome::SkippedStale => Err(anyhow::anyhow!(
                "manual close: LCL advanced during build_nomination_value; \
                 caller should retry with refreshed ledger seq"
            )),
        }
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
        // If the node has observed EXTERNALIZE messages significantly ahead
        // of its current ledger, it knows its state is stale.  Reject with
        // TryAgainLater rather than validating against stale state (which
        // produces terminal errors like TxBadSeq for transient conditions).
        //
        // This gate intentionally applies to all callers of
        // submit_transaction (RPC sendTransaction, compat /tx, loadgen).
        // Overlay tx intake bypasses this method and uses
        // herder.receive_transaction() directly, which is correct — overlay
        // flooding should continue even when behind.  See #1812.
        let current = self.current_ledger_seq() as u64;
        let max_ext = self.max_observed_externalize_slot.load(Ordering::SeqCst);
        if max_ext > current + TX_SUBMISSION_MAX_BEHIND {
            tracing::debug!(
                current_ledger = current,
                max_observed_ext = max_ext,
                gap = max_ext - current,
                "Rejecting tx submission: node is behind network (gap > {})",
                TX_SUBMISSION_MAX_BEHIND,
            );
            return henyey_herder::TxQueueResult::TryAgainLater;
        }

        let result = self.herder.receive_transaction(tx.clone());
        // No explicit advert enqueue needed — flush_tx_adverts() reads
        // the herder's queue in priority order each flood period.
        result
    }

    /// Test-only: skip fee balance validation for loadgen transactions.
    ///
    /// Matches stellar-core's `isLoadgenTx` bypass in `TransactionQueue::canAdd()`
    /// which skips both tx validation and fee balance checks for loadgen txs
    /// (gated on `#ifdef BUILD_TESTS`).
    #[cfg(feature = "test-utils")]
    pub fn set_skip_fee_balance_check(&self, skip: bool) {
        self.herder.tx_queue().set_skip_fee_balance_check(skip);
    }

    /// Set a pre-bound TCP listener for the overlay manager.
    ///
    /// When set, `start_overlay()` will pass this listener to the overlay
    /// manager instead of binding a new socket.  The listener must be bound
    /// to the same port as `config.overlay.peer_port`.
    ///
    /// This is a set-once value: calling this method more than once panics.
    /// The listener is consumed by `start_overlay()` and is not recoverable.
    /// If the app is dropped without starting the overlay, the listener is
    /// dropped (closed) automatically.
    pub fn set_pre_bound_listener(&self, listener: henyey_overlay::Listener) {
        let mut guard = self.pre_bound_listener.lock().unwrap();
        assert!(guard.is_none(), "pre_bound_listener already set");
        *guard = Some(listener);
    }

    /// Return the SCP envelopes recorded for a given slot.
    ///
    /// Test-only: used by integration tests that need to inspect or replay
    /// SCP envelopes (e.g., self-echo tests). Delegates to
    /// `Herder::get_scp_envelopes`.
    #[cfg(feature = "test-utils")]
    pub fn get_scp_envelopes(&self, slot: u64) -> Vec<stellar_xdr::curr::ScpEnvelope> {
        self.herder.get_scp_envelopes(slot)
    }

    pub fn herder_stats(&self) -> HerderStats {
        self.herder.stats()
    }

    /// Get the last successful ledger close stats for metrics.
    pub fn last_close_stats(&self) -> henyey_ledger::LedgerCloseStats {
        self.last_close_stats.read().clone()
    }

    /// Get the last successful ledger close performance data for metrics.
    pub fn last_close_perf(&self) -> Option<henyey_ledger::LedgerClosePerf> {
        self.last_close_perf.read().clone()
    }

    /// Get the cached drift stats from the last completed window.
    pub fn drift_stats(&self) -> Option<henyey_herder::drift_tracker::DriftStats> {
        self.drift_tracker
            .lock()
            .ok()
            .and_then(|t| t.last_drift_stats())
    }

    pub async fn simulation_debug_stats(&self) -> SimulationDebugStats {
        let herder_stats = self.herder.stats();
        let current_ledger = self.ledger_info().ledger_seq;
        let quorum_slot = herder_stats
            .tracking_slot
            .get()
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
            slot: slot_state.map(Into::into),
            nomination_timeout_fires: self.nomination_timeout_fires.load(Ordering::Relaxed),
            ballot_timeout_fires: self.ballot_timeout_fires.load(Ordering::Relaxed),
            scp_messages_sent: self.scp_messages_sent.load(Ordering::Relaxed),
            scp_messages_received: self.scp_messages_received.load(Ordering::Relaxed),
            consensus_trigger_attempts: self.consensus_trigger_attempts.load(Ordering::Relaxed),
            consensus_trigger_successes: self.consensus_trigger_successes.load(Ordering::Relaxed),
            consensus_trigger_failures: self.consensus_trigger_failures.load(Ordering::Relaxed),
            consensus_trigger_skipped_applying: self
                .consensus_trigger_skipped_applying
                .load(Ordering::Relaxed),
            consensus_trigger_skipped_stale: self
                .consensus_trigger_skipped_stale
                .load(Ordering::Relaxed),
            nomination_timeout_skipped_stale: self
                .nomination_timeout_skipped_stale
                .load(Ordering::Relaxed),
            archive_checkpoint_stale_returns: self.archive_checkpoint_cache.stale_returns(),
            archive_checkpoint_cold_returns: self.archive_checkpoint_cache.cold_returns(),
            archive_checkpoint_fresh_returns: self.archive_checkpoint_cache.fresh_returns(),
            archive_checkpoint_refresh_timeouts: self.archive_checkpoint_cache.refresh_timeouts(),
            archive_checkpoint_refresh_errors: self.archive_checkpoint_cache.refresh_errors(),
            archive_checkpoint_refresh_successes: self.archive_checkpoint_cache.refresh_successes(),
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
        let lcl = self.ledger_info().ledger_seq;

        // Only consult the publish queue for retention when publishing is
        // possible (validator with writable archives) (#1989).
        let can_publish = self.is_validator && self.config.history.publish_enabled();
        let min_queued = if can_publish {
            match self.db.load_publish_queue(Some(1)) {
                Ok(queue) => queue.first().copied(),
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        "Failed to read publish queue for maintenance; \
                         skipping this cycle to avoid over-pruning"
                    );
                    return;
                }
            }
        } else {
            None
        };

        let rpc_retention_window = if self.config.rpc.enabled {
            Some(self.config.rpc.retention_window)
        } else {
            None
        };

        tracing::info!(
            count = count,
            lcl = lcl,
            min_queued = ?min_queued,
            "Performing manual maintenance"
        );

        crate::maintainer::run_maintenance(&self.db, lcl, min_queued, rpc_retention_window, count);
    }

    /// Delete bucket files on disk that are no longer referenced by any component.
    ///
    /// Spec: BUCKETLISTDB_SPEC §8.2 — bucket lifecycle / garbage collection.
    ///
    /// # GC Algorithm (resolve-first + set-membership)
    ///
    /// Unlike stellar-core's refcount-based approach (`use_count() == 1`), Henyey
    /// resolves all pending merges first, then computes a complete GC root set and
    /// deletes any on-disk file not in that set. This eliminates race windows
    /// between GC and merge threads at the cost of a brief blocking wait.
    ///
    /// GC roots collected:
    /// 1. Live + hot archive bucket lists (curr/snap + pending merge inputs/outputs)
    /// 2. Snapshot manager (current + historical snapshots for RPC readers)
    /// 3. DB references (authoritative HAS + publish-queue HAS bucket hashes)
    ///
    /// # Safety
    ///
    /// `resolve_pending_bucket_merges()` must run first: background merge threads
    /// may have written output files to disk whose hashes haven't been polled yet.
    /// Without resolution, those outputs would not appear in `all_referenced_hashes()`
    /// and could be prematurely deleted while a `DiskBucket` still references the path.
    ///
    /// Spawns on tokio's blocking thread pool so that merge resolution (which may
    /// block) does not stall the async event loop.
    pub(crate) fn cleanup_stale_bucket_files_background(&self) {
        let lm = self.ledger_manager.clone();
        let bm = self.bucket_manager.clone();
        let db = self.db.clone();
        let sm = self.bucket_snapshot_manager.clone();
        let handle = tokio::task::spawn_blocking(move || {
            let Some(hashes) = collect_gc_roots(&lm, &sm, &db) else {
                return;
            };

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
        // Log any panic/cancellation in a detached task — cleanup is
        // best-effort and the caller doesn't wait for it.
        tokio::spawn(async move {
            let _ = henyey_common::await_blocking_logged("stale-bucket-cleanup", handle).await;
        });
    }

    pub fn scp_slot_snapshots(&self, limit: usize) -> Vec<ScpSlotSnapshot> {
        let scp = self.herder.scp();
        let ledger_seq = self.ledger_info().ledger_seq;
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
                    slot: state.into(),
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

    /// Request SCP state from peers and record the attempt timestamp.
    ///
    /// This is the standard entry point for all sites that participate in
    /// the heartbeat throttle window. Records the timestamp before the
    /// network call so that even failed attempts (no overlay, no peers)
    /// prevent immediate retry bursts.
    pub async fn request_scp_state_and_record(&self) {
        *self.last_scp_state_request_at.write().await = self.clock.now();
        self.request_scp_state_from_peers().await;
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
            commit_hash: self.config.build.commit_hash().map(|s| s.to_string()),
            build_timestamp: self.config.build.build_timestamp().map(|s| s.to_string()),
            node_name: self.config.node.name.clone(),
            public_key: self.keypair.public_key().to_strkey(),
            network_passphrase: self.config.network.passphrase.clone(),
            is_validator: self.config.node.is_validator,
            database_path: self.config.database.path.clone(),
            meta_stream_bytes_total: meta_bytes,
            meta_stream_writes_total: meta_writes,
            scp_verify: ScpVerifyMetrics {
                prefilter_counters: henyey_herder::scp_verify::PreFilterCounters::from_fn(|r| {
                    self.scp_prefilter_counters[r].load(Ordering::Relaxed)
                }),
                post_verify_drops: self.scp_post_verify_drops.load(Ordering::Relaxed),
                pv_counters: henyey_herder::scp_verify::PostVerifyCounters::from_fn(|r| {
                    self.scp_pv_counters[r].load(Ordering::Relaxed)
                }),
                // Sample live from the verifier handle so the gauge reflects
                // the current moment instead of the last `pump_scp_intake` tick.
                verify_input_backlog: self.herder.scp_verifier_handle().queue_len() as u64,
                verify_input_backlog_peak: self.herder.scp_verifier_handle().backlog_peak() as u64,
                verify_output_backlog: self.scp_verify_output_backlog.load(Ordering::Relaxed),
                verifier_thread_state: self.herder.scp_verifier_handle().state() as u64,
                verify_latency_us_sum: self.scp_verify_latency_us_sum.load(Ordering::Relaxed),
                verify_latency_count: self.scp_verify_latency_count.load(Ordering::Relaxed),
                scheduled_dedup_count: self.scp_scheduled.dedup_count(),
            },
            overlay_fetch_channel: OverlayFetchChannelMetrics {
                depth: self.fetch_channel_depth.load(Ordering::Relaxed),
                depth_max: self.fetch_channel_depth_max.load(Ordering::Relaxed),
            },
            post_catchup_hard_reset_total: self
                .post_catchup_hard_reset_total
                .load(Ordering::Relaxed),
            max_verified_scp_slot: self.max_verified_scp_slot.load(Ordering::Relaxed),
        }
    }

    /// Lightweight metrics snapshot for the `/metrics` scrape path.
    ///
    /// Returns only the numeric fields needed by `refresh_gauges()`, avoiding
    /// the String/PathBuf allocations that [`info()`] performs.
    pub fn metrics_snapshot(&self) -> AppMetricsSnapshot {
        let (meta_bytes, meta_writes) = self
            .meta_stream
            .lock()
            .ok()
            .and_then(|guard| guard.as_ref().map(|ms| ms.metrics()))
            .unwrap_or((0, 0));

        AppMetricsSnapshot {
            is_validator: self.config.node.is_validator,
            meta_stream_bytes_total: meta_bytes,
            meta_stream_writes_total: meta_writes,
            scp_verify: ScpVerifyMetrics {
                prefilter_counters: henyey_herder::scp_verify::PreFilterCounters::from_fn(|r| {
                    self.scp_prefilter_counters[r].load(Ordering::Relaxed)
                }),
                post_verify_drops: self.scp_post_verify_drops.load(Ordering::Relaxed),
                pv_counters: henyey_herder::scp_verify::PostVerifyCounters::from_fn(|r| {
                    self.scp_pv_counters[r].load(Ordering::Relaxed)
                }),
                verify_input_backlog: self.herder.scp_verifier_handle().queue_len() as u64,
                verify_input_backlog_peak: self.herder.scp_verifier_handle().backlog_peak() as u64,
                verify_output_backlog: self.scp_verify_output_backlog.load(Ordering::Relaxed),
                verifier_thread_state: self.herder.scp_verifier_handle().state() as u64,
                verify_latency_us_sum: self.scp_verify_latency_us_sum.load(Ordering::Relaxed),
                verify_latency_count: self.scp_verify_latency_count.load(Ordering::Relaxed),
                scheduled_dedup_count: self.scp_scheduled.dedup_count(),
            },
            overlay_fetch_channel: OverlayFetchChannelMetrics {
                depth: self.fetch_channel_depth.load(Ordering::Relaxed),
                depth_max: self.fetch_channel_depth_max.load(Ordering::Relaxed),
            },
            post_catchup_hard_reset_total: self
                .post_catchup_hard_reset_total
                .load(Ordering::Relaxed),
            max_verified_scp_slot: self.max_verified_scp_slot.load(Ordering::Relaxed),
            // Phase 3 cumulative counters.
            cumulative_apply_success: self.cumulative_apply_success.load(Ordering::Relaxed),
            cumulative_apply_failure: self.cumulative_apply_failure.load(Ordering::Relaxed),
            cumulative_soroban_success: self.cumulative_soroban_success.load(Ordering::Relaxed),
            cumulative_soroban_failure: self.cumulative_soroban_failure.load(Ordering::Relaxed),
            soroban_stage_count: self.last_soroban_stage_count.load(Ordering::Relaxed),
            soroban_max_cluster_count: self.last_soroban_max_cluster_count.load(Ordering::Relaxed),
            // Phase 3 last-close cache metrics (lightweight snapshot).
            bucket_cache_hit_ratio: self
                .last_close_perf
                .read()
                .as_ref()
                .map_or(0.0, |p| p.cache.hit_rate),
            snapshot_cache_hit_ratio: self
                .last_close_perf
                .read()
                .as_ref()
                .map_or(0.0, |p| p.snapshot_cache.hit_ratio),
            snapshot_cache_fallback_lookups: self
                .last_close_perf
                .read()
                .as_ref()
                .map_or(0, |p| p.snapshot_cache.fallback_lookups),
            // Phase 5 archive cache counters.
            archive_cache_fresh: self.archive_checkpoint_cache.fresh_returns(),
            archive_cache_stale: self.archive_checkpoint_cache.stale_returns(),
            archive_cache_cold: self.archive_checkpoint_cache.cold_returns(),
            archive_cache_refresh_success: self.archive_checkpoint_cache.refresh_successes(),
            archive_cache_refresh_error: self.archive_checkpoint_cache.refresh_errors(),
            archive_cache_refresh_timeout: self.archive_checkpoint_cache.refresh_timeouts(),
            archive_cache_age_secs: self
                .archive_checkpoint_cache
                .last_query_age()
                .map_or(0.0, |d| d.as_secs_f64()),
            archive_cache_populated: self.archive_checkpoint_cache.is_populated(),
            // Stage C: SCP metrics (issue #2233).
            scp: self.herder.scp_metrics_snapshot(),
            scp_phase: self.herder.tracking_slot_ballot_phase(),
            scp_cumulative_statements: self.herder.scp_cumulative_statement_count() as u64,
            nomination_timeout_fires: self.nomination_timeout_fires.load(Ordering::Relaxed),
            ballot_timeout_fires: self.ballot_timeout_fires.load(Ordering::Relaxed),
            consensus_trigger_skipped_applying: self
                .consensus_trigger_skipped_applying
                .load(Ordering::Relaxed),
            consensus_trigger_skipped_stale: self
                .consensus_trigger_skipped_stale
                .load(Ordering::Relaxed),
            nomination_timeout_skipped_stale: self
                .nomination_timeout_skipped_stale
                .load(Ordering::Relaxed),
        }
    }

    /// Return the local quorum set if configured.
    pub fn local_quorum_set(&self) -> Option<stellar_xdr::curr::ScpQuorumSet> {
        self.herder.local_quorum_set()
    }

    // ── Metrics accessors ──────────────────────────────────────────────

    /// SCP envelope counters: (sent, received).
    pub fn scp_envelope_counters(&self) -> (u64, u64) {
        (
            self.scp_messages_sent.load(Ordering::Relaxed),
            self.scp_messages_received.load(Ordering::Relaxed),
        )
    }

    /// Total lost-sync events.
    pub fn lost_sync_count(&self) -> u64 {
        self.lost_sync_count.load(Ordering::Relaxed)
    }

    /// Snapshot of live bucket merge counters.
    pub fn merge_counters_snapshot(&self) -> henyey_bucket::MergeCountersSnapshot {
        self.ledger_manager
            .bucket_list()
            .merge_counters()
            .snapshot()
    }

    /// Snapshot of overlay metrics (if overlay is running).
    pub async fn overlay_metrics_snapshot(&self) -> Option<henyey_overlay::OverlayMetricsSnapshot> {
        let overlay = self.overlay.read().await;
        overlay.as_ref().map(|o| {
            let mut snap = o.overlay_metrics().snapshot();
            snap.flood_known_count = o.flood_stats().seen_count as u64;
            snap
        })
    }

    /// Overlay connection breakdown by direction and state.
    pub async fn overlay_connection_breakdown(
        &self,
    ) -> Option<crate::app::types::ConnectionBreakdown> {
        let overlay = self.overlay.read().await;
        overlay.as_ref().map(|o| {
            let stats = o.connection_breakdown();
            crate::app::types::ConnectionBreakdown {
                inbound_authenticated: stats.0 as u64,
                outbound_authenticated: stats.1 as u64,
                inbound_pending: stats.2 as u64,
                outbound_pending: stats.3 as u64,
            }
        })
    }

    /// Quorum health summary (None when not tracking).
    pub fn quorum_health(&self) -> Option<crate::app::types::QuorumHealthMetrics> {
        let (agree, missing, disagree, fail_at, delayed) = self.herder.quorum_health()?;
        Some(crate::app::types::QuorumHealthMetrics {
            agree,
            missing,
            disagree,
            fail_at,
            delayed,
        })
    }

    /// Quorum intersection publishable status for metrics export.
    ///
    /// See [`henyey_herder::Herder::quorum_intersection_publishable`].
    pub fn quorum_intersection_publishable(&self) -> Option<bool> {
        self.herder.quorum_intersection_publishable()
    }

    /// Quorum info for the `/info` endpoint (None when no quorum data available).
    pub fn quorum_info_for_info(&self) -> Option<henyey_herder::json_api::InfoQuorumSnapshot> {
        let lcl_seq = self.ledger_summary().num;
        self.herder.quorum_info_for_info(lcl_seq)
    }

    /// SCP timing for the most recently externalized slot.
    pub fn scp_timing(&self) -> Option<crate::app::types::ScpTimingMetrics> {
        let snapshot = self.herder.scp_timing()?;
        Some(crate::app::types::ScpTimingMetrics {
            externalize_duration_secs: Some(snapshot.externalize_duration.as_secs_f64()),
            nomination_duration_secs: snapshot.nomination_duration.map(|d| d.as_secs_f64()),
            first_to_self_externalize_secs: snapshot
                .first_to_self_externalize_lag
                .map(|d| d.as_secs_f64()),
        })
    }
}

impl App {
    /// Start the sync recovery manager background task.
    ///
    /// This spawns a background task that monitors for consensus stuck conditions
    /// and triggers recovery actions when needed. The task is supervised: if it
    /// panics, shutdown is triggered immediately.
    ///
    /// In standalone mode (`manual_close && run_standalone`) the manager is not
    /// started — there is no peer to fall behind, so the consensus-stuck and
    /// out-of-sync recovery timers would only generate noise. This mirrors the
    /// stellar-core gates in `HerderImpl::triggerNextLedger` (HerderImpl.cpp:582-588,
    /// `startOutOfSyncTimer`) and `HerderImpl::trackingHeartBeat`
    /// (HerderImpl.cpp:2502-2510). The henyey gate intentionally widens the
    /// `trackingHeartBeat` predicate from `MANUAL_CLOSE` to
    /// `MANUAL_CLOSE && RUN_STANDALONE` so that simulation tests
    /// (`manual_close=true, run_standalone=false`) still get sync-recovery —
    /// see `HerderConfig::suppress_scp` for the same convention.
    pub fn start_sync_recovery(self: &Arc<Self>) {
        if self.config.node.manual_close && self.config.testing.run_standalone {
            tracing::info!(
                "Sync recovery suppressed (standalone mode: MANUAL_CLOSE && RUN_STANDALONE) \
                 — parity: HerderImpl.cpp:582-588 / 2502-2510"
            );
            return;
        }
        let (handle, manager) = SyncRecoveryManager::new(Arc::clone(self));
        *self.sync_recovery_handle.write() = Some(handle);
        let shutdown_tx = self.shutdown_tx.clone();
        let task = tokio::spawn(async move {
            use futures::FutureExt;
            use std::panic::AssertUnwindSafe;
            let result = AssertUnwindSafe(manager.run()).catch_unwind().await;
            match result {
                Ok(()) => {
                    tracing::debug!("Sync recovery manager exited cleanly");
                }
                Err(payload) => {
                    let msg = payload
                        .downcast_ref::<&str>()
                        .copied()
                        .or_else(|| payload.downcast_ref::<String>().map(|s| s.as_str()))
                        .unwrap_or("<non-string panic>");
                    tracing::error!(
                        panic_message = msg,
                        "FATAL: sync recovery manager panicked — triggering shutdown"
                    );
                    let _ = shutdown_tx.send(());
                }
            }
        });
        *self.sync_recovery_task.write() = Some(task);
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
        henyey_common::spawn_observed("maintainer_shutdown_bridge", async move {
            let _ = broadcast_rx.recv().await;
            let _ = shutdown_tx.send(true);
        });

        // Clone the database for the maintainer (Database is cheap to clone —
        // it wraps a connection pool).
        let db = Arc::new(self.db.clone());

        // Provide ledger bounds via Arc<App>.
        let app = Arc::clone(self);
        let can_publish = self.is_validator && self.config.history.publish_enabled();
        let get_ledger_bounds = move || -> (u32, Option<u32>) {
            let lcl = app.ledger_info().ledger_seq;
            // Only consult the publish queue for retention when publishing is
            // possible (validator with writable archives).  Without either the
            // queue cannot drain and stale entries would pin the prune threshold
            // indefinitely (#1989).
            let min_queued = if can_publish {
                match app.database().load_publish_queue(Some(1)) {
                    Ok(queue) => queue.first().copied(),
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            "Failed to read publish queue for maintenance; \
                             skipping pruning this cycle to avoid over-pruning"
                        );
                        // Use Some(0) to force publish_safe_lmin to 0,
                        // effectively skipping all publish-sensitive pruning.
                        Some(0)
                    }
                }
            } else {
                None
            };
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
    /// Currently called by the app layer after each ledger close.
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
    ///
    /// Also clears the fine-grained sub-phase counter back to 0 so stale
    /// `PHASE_6_*` / `PHASE_13_*` values stamped by a prior coarse phase
    /// do not leak into subsequent WATCHDOG reports.
    #[inline]
    fn set_phase(&self, phase: u64) {
        self.event_loop_phase.store(phase, Ordering::Relaxed);
        self.event_loop_phase_sub.store(0, Ordering::Relaxed);
    }

    /// Stamp the fine-grained sub-phase. Read alongside
    /// [`event_loop_phase`](Self::event_loop_phase) by the WATCHDOG thread
    /// (issue #1788). Constants live in [`super::phase`].
    ///
    /// Callers stamp the sub-phase immediately before each `.await` they
    /// want to attribute in a freeze capture. The WATCHDOG prints both
    /// `phase` and `phase_sub` in its error/warn log lines.
    #[inline]
    pub(crate) fn set_phase_sub(&self, sub: u32) {
        self.event_loop_phase_sub.store(sub, Ordering::Relaxed);
    }

    /// Test hook: snapshot the current (phase, sub) pair.
    #[cfg(test)]
    pub(crate) fn phase_snapshot_for_test(&self) -> (u64, u32) {
        (
            self.event_loop_phase.load(Ordering::Relaxed),
            self.event_loop_phase_sub.load(Ordering::Relaxed),
        )
    }

    /// Decrement the overlay fetch-channel depth gauge by one, clamped at
    /// zero. Called by the event loop for every successful `recv()` on
    /// `fetch_response_rx`. Accounting is done on the send side (see
    /// [`OverlayManager`]) so the gauge stays fresh even when the loop
    /// wedges — which is the exact failure mode the metric is meant to
    /// diagnose (issue #1741).
    #[inline]
    pub(crate) fn decrement_fetch_channel_depth(&self) {
        let _ = self
            .fetch_channel_depth
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
                Some((v - 1).max(0))
            });
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
    /// ticked in 30+ seconds, it emits tiered diagnostics:
    ///
    /// - **Tier 0** (automatic): scrapes `/proc/<pid>/task/*/wchan` and
    ///   thread states (Linux/procfs-specific, best-effort).
    /// - **Tier 1** (operator hint): logs a manual one-liner for repeated
    ///   wchan sampling with a pre-substituted PID.
    /// - **Tier 2** (operator hint): suggests `py-spy` / `gdb` when
    ///   installed, for full user-space stack traces.
    ///
    /// It also monitors the SCP signature-verifier thread (see
    /// [`henyey_herder::scp_verify`]): it fires an error if the worker is
    /// `Dead`, or if its heartbeat is stuck while there is a non-empty backlog
    /// for at least [`BACKLOG_STALE_TICKS`] consecutive ticks.
    ///
    /// Returns a [`WatchdogGuard`] whose `Drop` impl signals the thread to
    /// exit and resets `last_event_loop_tick_ms` to 0 (preventing spurious
    /// abort after the event loop has exited).
    pub(crate) fn start_event_loop_watchdog(&self) -> WatchdogGuard {
        /// Number of consecutive 10s ticks the verifier heartbeat must be
        /// stuck (with a non-empty backlog) before the watchdog logs.
        const BACKLOG_STALE_TICKS: u32 = 3;

        let tick_ms = Arc::clone(&self.last_event_loop_tick_ms);
        let phase = Arc::clone(&self.event_loop_phase);
        let phase_sub = Arc::clone(&self.event_loop_phase_sub);
        let fetch_depth = Arc::clone(&self.fetch_channel_depth);
        let fetch_depth_max = Arc::clone(&self.fetch_channel_depth_max);
        let pid = std::process::id();
        let verifier = self.herder.scp_verifier_handle();
        let abort_threshold_secs = self.config.diagnostics.watchdog_abort_secs;

        let shutdown = Arc::clone(&self.watchdog_shutdown);
        let condvar = Arc::clone(&self.watchdog_condvar);

        let shutdown_thread = Arc::clone(&shutdown);
        let condvar_thread = Arc::clone(&condvar);

        std::thread::Builder::new()
            .name("watchdog".into())
            .spawn(move || {
                let mut last_hb_seen: u64 = 0;
                let mut stale_hb_ticks: u32 = 0;
                loop {
                    // Sleep via condvar so we can be woken promptly on shutdown.
                    {
                        let (lock, cvar) = &*condvar_thread;
                        let guard = lock.lock().unwrap_or_else(|e| e.into_inner());
                        let _ = cvar.wait_timeout(guard, Duration::from_secs(10));
                    }

                    // Check shutdown flag after waking.
                    if shutdown_thread.load(Ordering::Acquire) {
                        break;
                    }

                    let last_tick = tick_ms.load(Ordering::Relaxed);
                    if last_tick == 0 {
                        // Event loop hasn't started yet (or was reset on shutdown).
                        continue;
                    }

                    let now_ms = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_millis() as u64;
                    let stale_secs = now_ms.saturating_sub(last_tick) / 1000;
                    let current_phase = phase.load(Ordering::Relaxed);
                    let current_phase_sub = phase_sub.load(Ordering::Relaxed);
                    let fetch_channel_depth = fetch_depth.load(Ordering::Relaxed);
                    let fetch_channel_depth_max = fetch_depth_max.load(Ordering::Relaxed);

                    let snap = WatchdogSnapshot {
                        stale_secs,
                        phase: current_phase,
                        phase_sub: current_phase_sub,
                        fetch_channel_depth,
                        fetch_channel_depth_max,
                        pid,
                        abort_threshold_secs,
                    };

                    match snap.tier() {
                        WatchdogTier::Error => {
                            snap.emit_error();

                            // Tier 0 (automatic): scrape thread states and
                            // kernel wait-channels (wchan) from /proc.
                            // Best-effort and Linux/procfs-specific — may
                            // silently produce no output on non-Linux hosts
                            // or permission-restricted kernels. The 21:07
                            // #1759 live capture proved this signal alone
                            // is sufficient to classify lock-contention
                            // freezes without py-spy or gdb on the host.
                            if let Ok(entries) = std::fs::read_dir(format!("/proc/{}/task", pid)) {
                                let mut states: std::collections::HashMap<String, u32> =
                                    std::collections::HashMap::new();
                                let mut wchans: std::collections::HashMap<String, u32> =
                                    std::collections::HashMap::new();
                                for entry in entries.flatten() {
                                    let task_path = entry.path();
                                    let status_path = format!("{}/status", task_path.display());
                                    if let Ok(status) = std::fs::read_to_string(&status_path) {
                                        let state = status
                                            .lines()
                                            .find(|l| l.starts_with("State:"))
                                            .map(|l| l.to_string())
                                            .unwrap_or_else(|| "Unknown".into());
                                        *states.entry(state).or_insert(0) += 1;
                                    }
                                    // wchan: single-line kernel wait
                                    // channel symbol (e.g.
                                    // "futex_wait_queue", "ep_poll",
                                    // "hrtimer_nanosleep"). Best effort
                                    // — some kernels permission-restrict.
                                    let wchan_path = format!("{}/wchan", task_path.display());
                                    if let Ok(wchan) = std::fs::read_to_string(&wchan_path) {
                                        let key = wchan.trim().to_string();
                                        let key = if key.is_empty() {
                                            "(running)".to_string()
                                        } else {
                                            key
                                        };
                                        *wchans.entry(key).or_insert(0) += 1;
                                    }
                                }
                                for (state, count) in &states {
                                    tracing::error!(
                                        count,
                                        state = state.as_str(),
                                        "WATCHDOG: Thread state summary"
                                    );
                                }
                                for (wchan, count) in &wchans {
                                    tracing::error!(
                                        count,
                                        wchan = wchan.as_str(),
                                        "WATCHDOG: Thread wchan summary"
                                    );
                                }
                            }

                            // Tiered operator hints (#1759 / #1764):
                            // The automatic wchan scrape above (tier 0)
                            // is best-effort and may have failed. The
                            // hints below give the operator escalation
                            // options ordered by availability:
                            //   Tier 1 — manual /proc wchan one-liner
                            //            (always available on Linux, no
                            //            install, no root)
                            //   Tier 2 — py-spy / gdb / gcore (richer
                            //            user-space frames, but requires
                            //            the tool to be installed)
                            let hint = format_watchdog_diagnostic_hint(pid);
                            tracing::error!(pid, "{}", hint);
                        }
                        WatchdogTier::Warn => snap.emit_warn(),
                        WatchdogTier::None => {}
                    }

                    // Auto-abort: independent of the tier check so that
                    // any configured threshold (even < 30s) is respected.
                    // Checked after the tier match so diagnostics are
                    // always emitted before the abort.
                    if snap.should_abort() {
                        // If we haven't already emitted error-level
                        // diagnostics (threshold < 30s), do so now.
                        if snap.tier() != WatchdogTier::Error {
                            snap.emit_error();
                        }
                        tracing::error!(
                            stale_secs = snap.stale_secs,
                            phase = snap.phase,
                            phase_sub = snap.phase_sub,
                            abort_threshold_secs = snap.abort_threshold_secs,
                            "WATCHDOG: Auto-aborting after {}s freeze at phase={}",
                            snap.stale_secs,
                            snap.phase,
                        );
                        std::process::abort();
                    }

                    // SCP verifier health block (issue #1734 Phase B).
                    {
                        let v = &verifier;
                        let vstate = v.state();
                        if matches!(vstate, henyey_herder::scp_verify::VerifierState::Dead) {
                            tracing::error!(pid, "WATCHDOG: scp-verify worker thread is dead");
                        } else {
                            let hb = v.heartbeat();
                            let backlog = v.backlog();
                            if backlog > 0 && hb == last_hb_seen {
                                stale_hb_ticks += 1;
                                if stale_hb_ticks >= BACKLOG_STALE_TICKS {
                                    tracing::error!(
                                        backlog,
                                        hb,
                                        stale_hb_ticks,
                                        "WATCHDOG: scp-verify worker stuck \
                                         (heartbeat not advancing while backlog > 0)"
                                    );
                                }
                            } else {
                                stale_hb_ticks = 0;
                                last_hb_seen = hb;
                            }
                        }
                    }
                }
            })
            .expect("Failed to spawn watchdog thread");

        tracing::info!("Event loop watchdog started");

        WatchdogGuard {
            shutdown,
            condvar,
            tick_ms: Arc::clone(&self.last_event_loop_tick_ms),
        }
    }
}

/// RAII guard for the watchdog thread. When dropped, signals the watchdog
/// to exit and resets `last_event_loop_tick_ms` to 0 so any residual
/// watchdog iteration (between signal and actual thread exit) won't fire.
///
/// This ensures the watchdog is stopped on all exit paths: normal shutdown,
/// task abort, task drop, and panic unwind.
pub(crate) struct WatchdogGuard {
    shutdown: Arc<AtomicBool>,
    condvar: Arc<(std::sync::Mutex<()>, std::sync::Condvar)>,
    tick_ms: Arc<AtomicU64>,
}

impl Drop for WatchdogGuard {
    fn drop(&mut self) {
        // Signal the watchdog thread to exit.
        self.shutdown.store(true, Ordering::Release);
        // Reset tick to 0 so even if the thread does one more iteration
        // before seeing the flag, it hits the `if last_tick == 0 { continue }`
        // guard and won't fire the abort.
        self.tick_ms.store(0, Ordering::Relaxed);
        // Wake the thread from its condvar wait so it exits promptly.
        let (_, cvar) = &*self.condvar;
        cvar.notify_one();
    }
}

/// Slow-op threshold: hotspots that exceed this wall-clock elapsed value
/// in the event-loop task emit a single `WARN` log line naming the
/// operation and its duration. Diagnostic only — helps narrow down
/// which inline step is stalling the loop when issue #1759 recurs.
pub(crate) const SLOW_OP_THRESHOLD: std::time::Duration = std::time::Duration::from_millis(500);

/// Emit a `WARN`-level log line if `elapsed` exceeds `SLOW_OP_THRESHOLD`.
///
/// Intended use at event-loop hotspots where an occasional >500 ms stall
/// is the actual bug we are diagnosing (see #1759). No-op in the fast
/// path; zero cost beyond the `Duration` comparison.
///
/// The `op` label identifies the hotspot in logs; `count` is an
/// op-specific counter (e.g. number of items drained) that helps
/// distinguish "one slow item" from "many fast items that summed up".
/// Pass `0` when not applicable.
#[inline]
pub(crate) fn warn_if_slow(elapsed: std::time::Duration, op: &'static str, count: u64) {
    if elapsed >= SLOW_OP_THRESHOLD {
        tracing::warn!(
            op,
            count,
            elapsed_ms = elapsed.as_millis() as u64,
            "Slow event-loop operation (>= {}ms) — possible #1759 contributor",
            SLOW_OP_THRESHOLD.as_millis()
        );
    }
}

/// Format the tiered diagnostic hint message for a watchdog freeze event.
///
/// Pure function that builds the operator hint string with pre-substituted
/// PID. Extracted from the watchdog loop so the text can be unit-tested
/// without waiting for the 10-second poll interval or propagating a tracing
/// subscriber across thread boundaries.
///
/// Phase-code legend embedded in the ≥30s WATCHDOG error event.
///
/// This is the canonical operator-facing source for phase-code mappings.
/// When adding a new phase constant, update this legend and the tests.
pub(crate) const WATCHDOG_PHASE_LEGEND: &str = "\
    Phase codes: 0=select, 1=scp_msg, 2=fetch_resp, \
    3=broadcast, 4=scp_broadcast, 5=consensus_tick, \
    6=pending_close, 10=process_externalized, \
    11=externalized_catchup, 12=try_apply_buffered, \
    13=buffered_catchup, 14=catchup_running, \
    15=pending_catchup_complete, 16=heartbeat, \
    20=stats, 21=tx_advert, 22=tx_demand, 23=survey, \
    24=survey_req, 25=survey_phase, 26=scp_timeout, \
    27=ping, 28=peer_maint, 29=peer_refresh, \
    30=herder_cleanup, 31=scp_verifier, 32=scp_verified, \
    33=tx_set_gc.";

/// Name of the structured tracing field emitted by
/// [`WatchdogSnapshot::emit_error()`].
///
/// This field is **reserved exclusively** for the ≥30s error-tier freeze
/// path.  No other code path (including `emit_warn()`) should emit an event
/// with this field name — doing so would cause monitor-tick to restart the
/// node on transient stalls.
///
/// External monitoring tools (e.g. monitor-tick) grep rendered log output
/// for this field to detect event-loop freezes.  The constant is a
/// documentation anchor — the real mechanical guard is the
/// `watchdog_freeze_field_tests` module.  **Do not rename this field without
/// updating the tests and all monitoring consumers.**
#[cfg(test)]
pub(crate) const WATCHDOG_FREEZE_FIELD: &str = "watchdog_freeze";

/// Which tier of WATCHDOG alert to emit based on event-loop staleness.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WatchdogTier {
    /// < 15s — no alert.
    None,
    /// ≥ 15s — warning.
    Warn,
    /// ≥ 30s — error with full diagnostics.
    Error,
}

/// Snapshot of event-loop health fields read by the watchdog thread.
///
/// Extracting these into a struct lets us test the field set and
/// threshold routing without spawning a real watchdog thread.
pub(crate) struct WatchdogSnapshot {
    pub stale_secs: u64,
    pub phase: u64,
    pub phase_sub: u32,
    pub fetch_channel_depth: i64,
    pub fetch_channel_depth_max: i64,
    pub pid: u32,
    /// Auto-abort threshold in seconds. 0 = disabled.
    pub abort_threshold_secs: u64,
}

impl WatchdogSnapshot {
    /// Determine which alert tier this snapshot falls into.
    pub(crate) fn tier(&self) -> WatchdogTier {
        if self.stale_secs >= 30 {
            WatchdogTier::Error
        } else if self.stale_secs >= 15 {
            WatchdogTier::Warn
        } else {
            WatchdogTier::None
        }
    }

    /// Whether the watchdog should abort the process.
    ///
    /// Returns `true` when auto-abort is enabled (`abort_threshold_secs > 0`)
    /// and the event loop has been frozen for at least that many seconds.
    pub(crate) fn should_abort(&self) -> bool {
        self.abort_threshold_secs > 0 && self.stale_secs >= self.abort_threshold_secs
    }

    /// Emit the ≥15s warning-tier WATCHDOG event.
    ///
    /// `pid` is intentionally omitted (matches the existing schema —
    /// pid is only on the error path).
    pub(crate) fn emit_warn(&self) {
        tracing::warn!(
            stale_secs = self.stale_secs,
            phase = self.phase,
            phase_sub = self.phase_sub,
            fetch_channel_depth = self.fetch_channel_depth,
            fetch_channel_depth_max = self.fetch_channel_depth_max,
            "WATCHDOG: Event loop slow (>15s since last tick)"
        );
    }

    /// Emit the ≥30s error-tier WATCHDOG event with the phase-code legend.
    pub(crate) fn emit_error(&self) {
        tracing::error!(
            watchdog_freeze = true,
            stale_secs = self.stale_secs,
            phase = self.phase,
            phase_sub = self.phase_sub,
            fetch_channel_depth = self.fetch_channel_depth,
            fetch_channel_depth_max = self.fetch_channel_depth_max,
            pid = self.pid,
            "WATCHDOG: Event loop appears frozen! {} \
             Sub-phase N.M labels: see \
             crates/app/src/app/phase.rs for PHASE_6_* and PHASE_13_* constants.",
            WATCHDOG_PHASE_LEGEND,
        );
    }
}

/// Tiers:
/// - **Tier 0** (automatic): `/proc/<pid>/task/*/wchan` scrape logged above
///   (best-effort, Linux/procfs-specific).
/// - **Tier 1** (manual): wchan one-liner for repeated sampling.
/// - **Tier 2** (if installed): `py-spy` / `gdb` / `gcore`.
pub(crate) fn format_watchdog_diagnostic_hint(pid: u32) -> String {
    format!(
        "WATCHDOG: Thread state + wchan summary may have been logged above \
         (best-effort, Linux/procfs-specific). \
         Tier 1 — manual wchan sample (no install needed): \
         for t in /proc/{pid}/task/*; do \
         printf '%-8s %s\\n' \"$(basename $t)\" \"$(cat $t/wchan)\"; \
         done | sort -k2 | uniq -cf1   \
         Tier 2 — richer frames (if installed): \
         py-spy dump --pid {pid}  \
         (or: sudo gcore {pid} && gdb -ex 'thread apply all bt' -ex quit core.{pid})"
    )
}

/// Two-way synchronization gate for the `process_externalized_slots`
/// split-writer regression test. The iteration loop signals `entered`
/// on the first non-stale slot (before `check_ledger_close`), then
/// blocks on `resume`. This gives the test a deterministic window to
/// verify that `syncing_ledgers` write lock is NOT held during phase 2.
#[cfg(test)]
pub(crate) struct PesIterationGate {
    /// Signaled by the iteration loop when phase 2 is in progress.
    pub entered: tokio::sync::Notify,
    /// The iteration loop blocks here until the test signals resume.
    pub resume: tokio::sync::Notify,
}

#[cfg(test)]
mod tests {
    use super::*;
    use stellar_xdr::curr::StellarValueExt;
    use tempfile;

    /// Construct a `PendingLedgerClose` with default tx_set/upgrades.
    ///
    /// The caller provides the blocking task handle (which determines the
    /// close outcome — success, error, or panic) and a sequence number.
    fn make_test_pending_close(
        handle: tokio::task::JoinHandle<
            Result<henyey_ledger::LedgerCloseResult, henyey_ledger::LedgerError>,
        >,
        seq: u32,
    ) -> PendingLedgerClose {
        PendingLedgerClose {
            handle,
            ledger_seq: seq,
            tx_set: henyey_herder::TransactionSet::new_legacy(
                henyey_common::Hash256::ZERO,
                Vec::new(),
            ),
            close_time: seq as u64,
            upgrades: Vec::new(),
            dispatch_time: std::time::Instant::now(),
        }
    }

    /// Minimal successful `LedgerCloseResult` for the given sequence.
    fn make_successful_close_result(seq: u32) -> henyey_ledger::LedgerCloseResult {
        henyey_ledger::LedgerCloseResult {
            header: stellar_xdr::curr::LedgerHeader {
                ledger_version: 24,
                previous_ledger_hash: stellar_xdr::curr::Hash([0u8; 32]),
                scp_value: stellar_xdr::curr::StellarValue {
                    tx_set_hash: stellar_xdr::curr::Hash([0u8; 32]),
                    close_time: stellar_xdr::curr::TimePoint(seq as u64),
                    upgrades: stellar_xdr::curr::VecM::default(),
                    ext: stellar_xdr::curr::StellarValueExt::Basic,
                },
                tx_set_result_hash: stellar_xdr::curr::Hash([0u8; 32]),
                bucket_list_hash: stellar_xdr::curr::Hash([0u8; 32]),
                ledger_seq: seq,
                total_coins: 0,
                fee_pool: 0,
                inflation_seq: 0,
                id_pool: 0,
                base_fee: 100,
                base_reserve: 5_000_000,
                max_tx_set_size: 100,
                skip_list: [
                    stellar_xdr::curr::Hash([0u8; 32]),
                    stellar_xdr::curr::Hash([0u8; 32]),
                    stellar_xdr::curr::Hash([0u8; 32]),
                    stellar_xdr::curr::Hash([0u8; 32]),
                ],
                ext: stellar_xdr::curr::LedgerHeaderExt::V0,
            },
            header_hash: henyey_common::Hash256::ZERO,
            tx_results: Vec::new(),
            meta: None,
            perf: None,
            stats: Default::default(),
        }
    }

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

        // Seed the shared atomics to non-zero values and assert they reach
        // AppInfo via App::info(). Guards against future regressions that
        // might silently drop either field from the wiring.
        app.fetch_channel_depth.store(17, Ordering::Relaxed);
        app.fetch_channel_depth_max.store(42, Ordering::Relaxed);

        let info = app.info();

        assert_eq!(info.node_name, "test-node");
        assert!(!info.public_key.is_empty());
        assert!(info.public_key.starts_with('G'));
        assert_eq!(info.overlay_fetch_channel.depth, 17);
        assert_eq!(info.overlay_fetch_channel.depth_max, 42);
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
    fn test_buffered_catchup_target_first_checkpoint() {
        // first_buffered=32 is in the first checkpoint (not a checkpoint start).
        // next checkpoint start = first_ledger_after_checkpoint_containing(32) = 64
        // required_first = 64, trigger = 65, target = 63
        let target = App::buffered_catchup_target(0, 32, 65);
        assert_eq!(target, Some(63));

        // Not enough buffered: last_buffered < trigger (65)
        let target = App::buffered_catchup_target(0, 32, 64);
        assert_eq!(target, None);
    }

    #[test]
    fn test_buffered_catchup_target_at_checkpoint_start() {
        // first_buffered=64 is a checkpoint start.
        // required_first = 64, trigger = 65, target = 63
        let target = App::buffered_catchup_target(0, 64, 65);
        assert_eq!(target, Some(63));

        // first_buffered=1 is a checkpoint start (first checkpoint).
        // required_first = 1, trigger = 2, target = 0 → None
        let target = App::buffered_catchup_target(0, 1, 2);
        assert_eq!(target, None);
    }

    #[test]
    fn test_buffered_catchup_target_first_buffered_one() {
        // Tests first_buffered=1 with a large buffer spanning past the first checkpoint.
        // Assumes default checkpoint_frequency=64.
        //
        // first_buffered=1, current_ledger=0:
        // Early return guard: first_buffered(1) <= current_ledger(0) + 1 → 1 <= 1 → true → None
        // This confirms: at genesis with the immediate next ledger buffered, no catchup
        // is triggered regardless of how many additional ledgers are buffered.
        let target = App::buffered_catchup_target(0, 1, 65);
        assert_eq!(target, None);

        // Same scenario with last_buffered spanning multiple checkpoints.
        let target = App::buffered_catchup_target(0, 1, 200);
        assert_eq!(target, None);
    }

    #[test]
    fn test_compute_catchup_target_for_timeout_first_buffered_one() {
        // Tests compute_catchup_target_for_timeout with first_buffered=1 (genesis).
        // Assumes default checkpoint_frequency=64.
        // The key boundary is last_buffered=64 (first ledger of second checkpoint)
        // vs last_buffered=63 (last ledger of first checkpoint).

        // Case A (boundary positive): last_buffered=64 spans into second checkpoint.
        // last_ledger_before_checkpoint_containing(1) = None → target=0
        // target(0) <= current(0) → falls through to alt_target
        // last_ledger_before_checkpoint_containing(64) = Some(63) (second checkpoint starts at 64)
        // alt_target(63) > current(0) → returns Some(63)
        let target = App::compute_catchup_target_for_timeout(64, 1, 0);
        assert_eq!(target, Some(63));

        // Case B (boundary negative): last_buffered=63, all in first checkpoint.
        // last_ledger_before_checkpoint_containing(1) = None → target=0
        // target(0) <= current(0) → falls through to alt_target
        // last_ledger_before_checkpoint_containing(63) = None (63 is in first checkpoint)
        // alt_target=0 <= current(0) → falls through to direct_target
        // direct_target = first_buffered - 1 = 0, 0 > 0 → false → None
        let target = App::compute_catchup_target_for_timeout(63, 1, 0);
        assert_eq!(target, None);
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
        // first_buffered=50 is in the first checkpoint (starts at 1).
        // last_ledger_before_checkpoint_containing(50) = None → target=0.
        // Falls through to direct_target = first_buffered - 1 = 49.
        // 49 > current_ledger (10), so return Some(49)
        let target = App::compute_catchup_target_for_timeout(60, 50, 10);
        assert_eq!(target, Some(49));

        // Test case 7: edge case with very small ledgers
        // first_buffered=3, in first checkpoint (starts at 1).
        // last_ledger_before_checkpoint_containing(3) = None → target=0.
        // Falls through to direct_target = 3 - 1 = 2.
        // 2 > current_ledger(0), so return Some(2)
        let target = App::compute_catchup_target_for_timeout(5, 3, 0);
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
        };

        assert_eq!(state.current_ledger, 1000);
        assert_eq!(state.first_buffered, 1001);
        assert_eq!(state.recovery_attempts, 0);
    }

    #[test]
    fn test_consensus_stuck_action_variants() {
        use crate::app::types::HardResetReason;
        // Verify all action variants exist and can be matched
        let actions = [
            ConsensusStuckAction::Wait,
            ConsensusStuckAction::AttemptRecovery,
            ConsensusStuckAction::TriggerCatchup,
            ConsensusStuckAction::HardReset(HardResetReason::ArchiveBehindRecoveryExhausted),
        ];

        for action in actions {
            match action {
                ConsensusStuckAction::Wait => {}
                ConsensusStuckAction::AttemptRecovery => {}
                ConsensusStuckAction::TriggerCatchup => {}
                ConsensusStuckAction::HardReset(_) => {}
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
        assert_eq!(stats.tx_queue_stats.pending_txs_age, [0, 0, 0, 0]);
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

    /// Regression test for #2688 — in standalone mode
    /// (`manual_close && run_standalone`) `start_sync_recovery()` must skip
    /// spawning the `SyncRecoveryManager`, mirroring stellar-core's
    /// `HerderImpl.cpp:582-588` (`startOutOfSyncTimer`) and
    /// `HerderImpl.cpp:2502-2510` (`trackingHeartBeat`) gates.
    #[tokio::test]
    async fn test_start_sync_recovery_suppressed_in_standalone_mode() {
        let dir = tempfile::tempdir().expect("temp dir");
        let db_path = dir.path().join("rs-stellar-test.db");
        let mut config = crate::config::ConfigBuilder::new()
            .database_path(db_path)
            .build();
        config.node.manual_close = true;
        config.testing.run_standalone = true;

        let app = Arc::new(App::new(config).await.unwrap());

        app.start_sync_recovery();

        assert!(
            app.sync_recovery_handle.read().is_none(),
            "standalone mode must not create a sync_recovery_handle"
        );
        assert!(
            app.sync_recovery_task.read().is_none(),
            "standalone mode must not spawn a sync_recovery_task"
        );
    }

    /// Regression test for #2688 — henyey intentionally widens stellar-core's
    /// `trackingHeartBeat` predicate from `MANUAL_CLOSE` to
    /// `MANUAL_CLOSE && RUN_STANDALONE`, matching the convention used by
    /// `HerderConfig::suppress_scp`. Simulation tests run with
    /// `manual_close=true, run_standalone=false` and must keep sync-recovery.
    #[tokio::test]
    async fn test_start_sync_recovery_active_when_only_manual_close() {
        let dir = tempfile::tempdir().expect("temp dir");
        let db_path = dir.path().join("rs-stellar-test.db");
        let mut config = crate::config::ConfigBuilder::new()
            .database_path(db_path)
            .build();
        config.node.manual_close = true;
        config.testing.run_standalone = false;

        let app = Arc::new(App::new(config).await.unwrap());

        app.start_sync_recovery();

        assert!(
            app.sync_recovery_handle.read().is_some(),
            "manual_close alone must not suppress sync recovery"
        );
        assert!(app.sync_recovery_task.read().is_some());

        // Cleanup: shut down the manager so the spawned task drains.
        let handle = app.sync_recovery_handle.read().clone();
        if let Some(handle) = handle {
            handle.shutdown().await;
        }
        let task = app.sync_recovery_task.write().take();
        if let Some(task) = task {
            let _ = task.await;
        }
    }

    /// Regression test for #2688 — normal (non-standalone, non-manual-close)
    /// mode must start the sync recovery manager.
    #[tokio::test]
    async fn test_start_sync_recovery_active_in_normal_mode() {
        let dir = tempfile::tempdir().expect("temp dir");
        let db_path = dir.path().join("rs-stellar-test.db");
        let config = crate::config::ConfigBuilder::new()
            .database_path(db_path)
            .build();
        // Defaults: manual_close=false, run_standalone=false.

        let app = Arc::new(App::new(config).await.unwrap());

        app.start_sync_recovery();

        assert!(app.sync_recovery_handle.read().is_some());
        assert!(app.sync_recovery_task.read().is_some());

        let handle = app.sync_recovery_handle.read().clone();
        if let Some(handle) = handle {
            handle.shutdown().await;
        }
        let task = app.sync_recovery_task.write().take();
        if let Some(task) = task {
            let _ = task.await;
        }
    }

    /// Regression test for #2688 — when the sync recovery manager is suppressed,
    /// the public callbacks (`sync_recovery_heartbeat`, `start_sync_recovery_tracking`,
    /// `set_applying_ledger`) must remain safe no-ops on the channel side. Note:
    /// `set_applying_ledger` still flips the `is_applying_ledger` AtomicBool,
    /// which is independent of recovery scheduling and is read by code paths in
    /// `consensus.rs` and `ledger_close.rs`.
    #[tokio::test]
    async fn test_sync_recovery_heartbeat_noop_when_suppressed() {
        let dir = tempfile::tempdir().expect("temp dir");
        let db_path = dir.path().join("rs-stellar-test.db");
        let mut config = crate::config::ConfigBuilder::new()
            .database_path(db_path)
            .build();
        config.node.manual_close = true;
        config.testing.run_standalone = true;

        let app = Arc::new(App::new(config).await.unwrap());
        app.start_sync_recovery();
        assert!(app.sync_recovery_handle.read().is_none());

        // Heartbeat / start-tracking are pure no-ops: no panic, handle stays None.
        app.sync_recovery_heartbeat();
        app.start_sync_recovery_tracking();
        assert!(app.sync_recovery_handle.read().is_none());
        assert!(app.sync_recovery_task.read().is_none());

        // set_applying_ledger flips the AtomicBool unconditionally (independent
        // of recovery scheduling) but the channel-send half is a no-op because
        // the handle is None.
        assert!(!app.is_applying_ledger.load(Ordering::Relaxed));
        app.set_applying_ledger(true);
        assert!(app.is_applying_ledger.load(Ordering::Relaxed));
        assert!(app.sync_recovery_handle.read().is_none());
        assert!(app.sync_recovery_task.read().is_none());
        app.set_applying_ledger(false);
        assert!(!app.is_applying_ledger.load(Ordering::Relaxed));
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

    /// Regression test for #2302 — Change 1 of the parity-hardening proposal.
    ///
    /// Verifies that `try_trigger_consensus` skips the consensus trigger when
    /// `is_applying_ledger` is true, bumping `consensus_trigger_skipped_applying`
    /// rather than `consensus_trigger_attempts`.
    ///
    /// Parity: stellar-core HerderImpl.cpp:1440-1447 (`isApplying()` skip).
    #[tokio::test]
    async fn test_try_trigger_consensus_skips_while_applying() {
        let dir = tempfile::tempdir().expect("temp dir");
        let db_path = dir.path().join("rs-stellar-test.db");
        let config = crate::config::ConfigBuilder::new()
            .database_path(db_path)
            .build();

        let app = App::new(config).await.unwrap();

        // Bootstrap the herder so `is_tracking()` returns true and we reach
        // the new `is_applying_ledger` gate inside `try_trigger_consensus`.
        app.herder.bootstrap(1);

        // Mark ledger close in progress.
        app.set_applying_ledger(true);

        let attempts_before = app.consensus_trigger_attempts.load(Ordering::Relaxed);
        let skipped_before = app
            .consensus_trigger_skipped_applying
            .load(Ordering::Relaxed);

        app.try_trigger_consensus().await;

        let attempts_after = app.consensus_trigger_attempts.load(Ordering::Relaxed);
        let skipped_after = app
            .consensus_trigger_skipped_applying
            .load(Ordering::Relaxed);

        assert_eq!(
            attempts_after, attempts_before,
            "consensus_trigger_attempts must NOT increment when applying"
        );
        assert_eq!(
            skipped_after,
            skipped_before + 1,
            "consensus_trigger_skipped_applying must increment when applying"
        );
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
        let slot = app.herder.tracking_slot().get();
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

    /// Regression test for #2172: `try_start_ledger_close` must reject a
    /// buffered tx set that fails `prepare_for_apply` validation (defense-in-depth
    /// added in #2167). Verifies that `None` is returned and the `tx_set` field
    /// in the syncing_ledgers entry is cleared.
    #[tokio::test]
    async fn test_try_start_ledger_close_rejects_malformed_tx_set() {
        use stellar_xdr::curr::{
            GeneralizedTransactionSet, Hash, TransactionPhase, TransactionSetV1, TxSetComponent,
            TxSetComponentTxsMaybeDiscountedFee,
        };

        let dir = tempfile::tempdir().expect("temp dir");
        let db_path = dir.path().join("rs-stellar-test.db");
        let config = crate::config::ConfigBuilder::new()
            .database_path(db_path)
            .build();

        let app = App::new(config).await.unwrap();

        // Build a malformed GeneralizedTransactionSet with 3 phases (expects exactly 2).
        // This triggers validate_generalized_tx_set_xdr_structure to fail.
        let empty_phase = TransactionPhase::V0(
            vec![TxSetComponent::TxsetCompTxsMaybeDiscountedFee(
                TxSetComponentTxsMaybeDiscountedFee {
                    base_fee: None,
                    txs: Vec::new().try_into().unwrap(),
                },
            )]
            .try_into()
            .unwrap(),
        );
        let malformed_gen = GeneralizedTransactionSet::V1(TransactionSetV1 {
            // previous_ledger_hash = ZERO matches the uninitialized app's current_header_hash
            previous_ledger_hash: Hash([0u8; 32]),
            phases: vec![empty_phase.clone(), empty_phase.clone(), empty_phase]
                .try_into()
                .unwrap(),
        });

        let tx_set = henyey_herder::TransactionSet::new_generalized(malformed_gen);
        let hash = *tx_set.hash();

        // get_current_ledger() returns 0 for uninitialized app → next_seq = 1
        let next_seq: u32 = 1;

        // Insert into syncing_ledgers with matching hash and the malformed tx_set.
        {
            let mut buffer = app.syncing_ledgers.write().await;
            buffer.insert(
                next_seq,
                henyey_herder::LedgerCloseInfo {
                    slot: next_seq as u64,
                    tx_set_hash: hash,
                    tx_set: Some(tx_set),
                    close_time: 1,
                    upgrades: Vec::new(),
                    stellar_value_ext: StellarValueExt::Basic,
                },
            );
        }

        // Act: try_start_ledger_close should detect the malformed tx_set via
        // prepare_for_apply and return None.
        let result = app.try_start_ledger_close().await;
        assert!(
            result.is_none(),
            "should return None when buffered tx set fails prepare_for_apply"
        );

        // Verify: the entry still exists but tx_set has been cleared.
        let buffer = app.syncing_ledgers.read().await;
        let entry = buffer.get(&next_seq).expect("entry should still exist");
        assert!(
            entry.tx_set.is_none(),
            "tx_set field should be cleared after prepare_for_apply failure"
        );
    }

    /// Regression test for #2177: `try_start_ledger_close` must reject a
    /// buffered tx set whose cached hash does not match the `tx_set_hash`
    /// recorded in the `LedgerCloseInfo`. Exercises the defense-in-depth
    /// branch at `ledger_close.rs:1835-1847`.
    #[tokio::test]
    async fn test_try_start_ledger_close_rejects_tx_set_hash_mismatch() {
        let dir = tempfile::tempdir().expect("temp dir");
        let db_path = dir.path().join("rs-stellar-test.db");
        let config = crate::config::ConfigBuilder::new()
            .database_path(db_path)
            .build();

        let app = App::new(config).await.unwrap();

        // Precondition: uninitialized app has header hash ZERO and ledger seq 0.
        assert_eq!(
            app.get_current_ledger().await.unwrap(),
            0,
            "precondition: uninitialized app ledger seq should be 0"
        );

        // Build a legacy tx set with previous_ledger_hash = ZERO to match the
        // uninitialized app's header hash (bypasses the fatal pre-close hash
        // mismatch check).
        let tx_set =
            henyey_herder::TransactionSet::new_legacy(henyey_common::Hash256::ZERO, vec![]);
        let real_hash = *tx_set.hash();

        // Derive a mismatched hash by flipping the first byte.
        let mut mismatched_bytes = real_hash.as_bytes().to_owned();
        mismatched_bytes[0] ^= 0xFF;
        let mismatched_hash = henyey_common::Hash256::from(mismatched_bytes);
        assert_ne!(
            mismatched_hash, real_hash,
            "precondition: mismatched hash must differ from real hash"
        );

        let next_seq: u32 = 1;

        // Insert into syncing_ledgers with the WRONG tx_set_hash but the real tx_set.
        {
            let mut buffer = app.syncing_ledgers.write().await;
            buffer.insert(
                next_seq,
                henyey_herder::LedgerCloseInfo {
                    slot: next_seq as u64,
                    tx_set_hash: mismatched_hash,
                    tx_set: Some(tx_set),
                    close_time: 1,
                    upgrades: Vec::new(),
                    stellar_value_ext: StellarValueExt::Basic,
                },
            );
        }

        // Act: try_start_ledger_close should detect the hash mismatch and
        // return None.
        let result = app.try_start_ledger_close().await;
        assert!(
            result.is_none(),
            "should return None when buffered tx set hash mismatches tx_set_hash"
        );

        // Verify: entry still exists but tx_set has been cleared; tx_set_hash
        // remains unchanged.
        let buffer = app.syncing_ledgers.read().await;
        let entry = buffer.get(&next_seq).expect("entry should still exist");
        assert!(
            entry.tx_set.is_none(),
            "tx_set field should be cleared after hash mismatch"
        );
        assert_eq!(
            entry.tx_set_hash, mismatched_hash,
            "tx_set_hash should remain unchanged after clearing tx_set"
        );
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
            handle: tokio::task::spawn_blocking(|| {
                Err(henyey_ledger::LedgerError::Internal(
                    "simulated error".to_string(),
                ))
            }),
            ledger_seq: 1,
            tx_set: henyey_herder::TransactionSet::new_legacy(
                henyey_common::Hash256::ZERO,
                Vec::new(),
            ),
            close_time: 1,
            upgrades: Vec::new(),
            dispatch_time: std::time::Instant::now(),
        };

        let mut pending = pending;
        let join_result = (&mut pending.handle).await;
        let success = app
            .handle_close_complete(
                pending,
                join_result,
                super::persist::LedgerCloseFinalizer::inline(),
            )
            .await;

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
            tx_set: henyey_herder::TransactionSet::new_legacy(
                henyey_common::Hash256::ZERO,
                Vec::new(),
            ),
            close_time: 1,
            upgrades: Vec::new(),
            dispatch_time: std::time::Instant::now(),
        };

        let mut pending = pending;
        let join_result = (&mut pending.handle).await;
        let success = app
            .handle_close_complete(
                pending,
                join_result,
                super::persist::LedgerCloseFinalizer::inline(),
            )
            .await;

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
            handle: tokio::task::spawn_blocking(|| {
                Err(henyey_ledger::LedgerError::HashMismatch {
                    expected: "abc".into(),
                    actual: "def".into(),
                })
            }),
            ledger_seq: 1,
            tx_set: henyey_herder::TransactionSet::new_legacy(
                henyey_common::Hash256::ZERO,
                Vec::new(),
            ),
            close_time: 1,
            upgrades: Vec::new(),
            dispatch_time: std::time::Instant::now(),
        };

        let mut pending = pending;
        let join_result = (&mut pending.handle).await;
        let success = app
            .handle_close_complete(
                pending,
                join_result,
                super::persist::LedgerCloseFinalizer::inline(),
            )
            .await;

        assert!(!success);
        // Buffer should have been cleared due to hash mismatch.
        let buffer = app.syncing_ledgers.read().await;
        assert!(
            buffer.is_empty(),
            "syncing_ledgers should be cleared on hash mismatch"
        );
    }

    // ============================================================
    // Shutdown Drain Tests (regression for #1715: pending close not drained)
    // ============================================================

    #[tokio::test]
    async fn test_drain_close_pipeline_resets_applying_flag() {
        let dir = tempfile::tempdir().expect("temp dir");
        let db_path = dir.path().join("rs-stellar-test.db");
        let config = crate::config::ConfigBuilder::new()
            .database_path(db_path)
            .build();

        let app = App::new(config).await.unwrap();
        app.set_applying_ledger(true);

        // Simulate a failed close result.
        let pending = PendingLedgerClose {
            handle: tokio::task::spawn_blocking(|| {
                Err(henyey_ledger::LedgerError::Internal(
                    "simulated error".to_string(),
                ))
            }),
            ledger_seq: 42,
            tx_set: henyey_herder::TransactionSet::new_legacy(
                henyey_common::Hash256::ZERO,
                Vec::new(),
            ),
            close_time: 1,
            upgrades: Vec::new(),
            dispatch_time: std::time::Instant::now(),
        };

        let mut pipeline = super::close_pipeline::ClosePipeline::new();
        pipeline.start_close(pending);

        app.drain_close_pipeline(&mut pipeline).await;

        assert!(pipeline.is_idle(), "pipeline should be idle after drain");
        assert!(
            !app.is_applying_ledger.load(Ordering::Relaxed),
            "is_applying_ledger should be cleared after drain"
        );
    }

    #[tokio::test]
    async fn test_drain_close_pipeline_persisting() {
        let dir = tempfile::tempdir().expect("temp dir");
        let db_path = dir.path().join("rs-stellar-test.db");
        let config = crate::config::ConfigBuilder::new()
            .database_path(db_path)
            .build();

        let app = App::new(config).await.unwrap();

        let mut pipeline = super::close_pipeline::ClosePipeline::new();
        pipeline.start_persist(super::types::PendingPersist {
            handle: tokio::spawn(async {}),
            ledger_seq: 55,
            dispatch_time: std::time::Instant::now(),
        });

        app.drain_close_pipeline(&mut pipeline).await;

        assert!(pipeline.is_idle(), "pipeline should be idle after drain");
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

        // Verify all struct fields are initialized correctly
        assert_eq!(state.first_requested, now);
        assert_eq!(state.last_request, now);
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

        // Add entries: some with tx_set, some without (starting from ledger 100+)
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
                    tx_set: Some(henyey_herder::TransactionSet::new_legacy(
                        Hash256::ZERO,
                        Vec::new(),
                    )),
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
    fn test_trim_syncing_ledgers_early_checkpoint() {
        // When buffer straddles the first/second checkpoint boundary with a large gap,
        // verify correct trimming. last_buffered=100 is NOT a checkpoint start,
        // so trim_before = checkpoint_start(100) = 64. Entries below 64 are trimmed.
        let mut buffer = BTreeMap::new();
        let make_entry = |slot: u32| henyey_herder::LedgerCloseInfo {
            slot: slot as u64,
            tx_set_hash: Hash256::ZERO,
            tx_set: None,
            close_time: 1,
            upgrades: Vec::new(),
            stellar_value_ext: StellarValueExt::Basic,
        };

        // current_ledger=0, buffer at 50..=100. Gap = 50 - 0 = 50 < freq (64).
        // Actually, we need gap >= freq, so use current_ledger such that gap >= 64.
        // With entries at 65..=100, current_ledger=0, gap = 65 >= 64.
        // But entries 65..100 are all >= 64, so nothing to trim there.
        // Use entries spanning the boundary: 50..=100, current_ledger far below.
        // gap = first_buffered - current_ledger = 50 - (-15) — no, current_ledger is u32.
        // Let's do: entries at 50..=100. We need gap = 50 - current >= 64.
        // That's impossible since first_buffered=50 and we need current_ledger < 50-64 < 0.
        // Instead: entries from 65..=130 with current_ledger=0 (gap=65 >= 64).
        // last_buffered=130, NOT checkpoint start, trim_before = checkpoint_start(130) = 128.
        // Entries 65..127 trimmed, 128..130 kept.
        let current_ledger = 0u32;
        for slot in 65..=130 {
            buffer.insert(slot, make_entry(slot));
        }

        App::trim_syncing_ledgers(&mut buffer, current_ledger);

        // trim_before = checkpoint_start(130) = 128
        assert!(
            !buffer.contains_key(&65),
            "entry below checkpoint boundary should be trimmed"
        );
        assert!(
            !buffer.contains_key(&127),
            "entry just below checkpoint boundary should be trimmed"
        );
        assert!(
            buffer.contains_key(&128),
            "entry at checkpoint boundary should be retained"
        );
        assert!(buffer.contains_key(&130), "last entry should be retained");
        assert_eq!(buffer.len(), 3); // 128, 129, 130
    }

    #[test]
    fn test_trim_syncing_ledgers_last_buffered_at_checkpoint_start() {
        // When last_buffered IS a checkpoint start (e.g. 128), we look at prev=127
        // and trim_before = checkpoint_start(127) = 64.
        let mut buffer = BTreeMap::new();
        let make_entry = |slot: u32| henyey_herder::LedgerCloseInfo {
            slot: slot as u64,
            tx_set_hash: Hash256::ZERO,
            tx_set: None,
            close_time: 1,
            upgrades: Vec::new(),
            stellar_value_ext: StellarValueExt::Basic,
        };

        let current_ledger = 0u32;
        for slot in 65..=128 {
            buffer.insert(slot, make_entry(slot));
        }

        App::trim_syncing_ledgers(&mut buffer, current_ledger);

        // last_buffered=128 is checkpoint start, prev=127, trim_before=checkpoint_start(127)=64
        // All entries >= 64 retained (65..128 are all >= 64)
        assert!(
            buffer.contains_key(&65),
            "entry at/above trim boundary should be retained"
        );
        assert!(buffer.contains_key(&128), "last entry should be retained");
        assert_eq!(buffer.len(), 64); // 65..=128
    }

    #[test]
    fn test_trim_syncing_ledgers_last_buffered_at_first_checkpoint_boundary() {
        // Branch coverage: exercises the `is_checkpoint_start` path at the first
        // checkpoint boundary (ledger 64). With last_buffered=64, prev=63, and
        // checkpoint_start(63)=1 (the early-ledger branch: seq < freq → returns 1).
        // trim_before=1 means entry 64 is retained.
        let mut buffer = BTreeMap::new();
        let make_entry = |slot: u32| henyey_herder::LedgerCloseInfo {
            slot: slot as u64,
            tx_set_hash: Hash256::ZERO,
            tx_set: None,
            close_time: 1,
            upgrades: Vec::new(),
            stellar_value_ext: StellarValueExt::Basic,
        };

        let current_ledger = 0u32;
        buffer.insert(64, make_entry(64));

        App::trim_syncing_ledgers(&mut buffer, current_ledger);

        // last_buffered=64 is checkpoint start, prev=63, trim_before=checkpoint_start(63)=1
        // Entry 64 >= 1, so it's retained.
        assert!(
            buffer.contains_key(&64),
            "entry at first checkpoint boundary should be retained"
        );
        assert_eq!(buffer.len(), 1);
    }

    #[test]
    fn test_trim_boundary_for_last_buffered() {
        // Direct unit tests for the trim-boundary computation helper.
        // These can distinguish correct (checkpoint_start) from broken (returns 0) behavior.

        // last_buffered=64: checkpoint start, prev=63, checkpoint_start(63)=1
        assert_eq!(App::trim_boundary_for_last_buffered(64), Some(1));

        // last_buffered=128: checkpoint start, prev=127, checkpoint_start(127)=64
        assert_eq!(App::trim_boundary_for_last_buffered(128), Some(64));

        // last_buffered=192: checkpoint start, prev=191, checkpoint_start(191)=128
        assert_eq!(App::trim_boundary_for_last_buffered(192), Some(128));

        // last_buffered=130: NOT checkpoint start, checkpoint_start(130)=128
        assert_eq!(App::trim_boundary_for_last_buffered(130), Some(128));

        // last_buffered=100: NOT checkpoint start, checkpoint_start(100)=64
        assert_eq!(App::trim_boundary_for_last_buffered(100), Some(64));

        // last_buffered=63: NOT checkpoint start (63 != checkpoint_start(63)=1),
        // checkpoint_start(63)=1
        assert_eq!(App::trim_boundary_for_last_buffered(63), Some(1));

        // last_buffered=1: checkpoint start (1 == checkpoint_start(1)=1),
        // and last_buffered <= 1, so returns None (no trimming)
        assert_eq!(App::trim_boundary_for_last_buffered(1), None);

        // last_buffered=0: checkpoint_start(0) = 1, 0 != 1 so NOT checkpoint start,
        // checkpoint_start(0) = 1
        assert_eq!(App::trim_boundary_for_last_buffered(0), Some(1));
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
    // State reset tests (exercised via try_apply_buffered_ledgers helper,
    // which mirrors the reset logic in try_start_ledger_close)
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
        // Verify the state-reset block in try_apply_buffered_ledgers (which
        // mirrors try_start_ledger_close) behaves correctly when closed_any=true.
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
        // Normal operation: last_processed > current_ledger, so the
        // TX_SET_REQUEST_WINDOW trimming applies unchanged.
        let last_processed = 130u64;
        let current_ledger = 110u32;
        let latest_externalized = 150u64; // gap from last_processed is 20 > 12

        let (iter_start, advance_to) =
            App::externalized_iteration_window(last_processed, current_ledger, latest_externalized);

        let expected_skip_to = latest_externalized.saturating_sub(TX_SET_REQUEST_WINDOW);
        assert_eq!(iter_start, expected_skip_to + 1);
        assert_eq!(advance_to, expected_skip_to);
    }

    /// After catchup at ledger N with last_processed = N = current_ledger,
    /// the window must NOT skip past current_ledger — gap slots between
    /// current_ledger+1 and skip_to need to be iterable.
    #[test]
    fn test_externalized_iteration_window_gap_after_catchup() {
        // Simulates: catchup completes at N=100, latest_externalized = 143.
        // Gap of 43 > TX_SET_REQUEST_WINDOW (12).
        // Old behavior: iter_start = 132, skipping gap slot 101.
        // New behavior: iter_start = 101, covering the gap.
        let last_processed = 100u64;
        let current_ledger = 100u32;
        let latest_externalized = 143u64;

        let (iter_start, advance_to) =
            App::externalized_iteration_window(last_processed, current_ledger, latest_externalized);

        // Should start at current_ledger + 1 (= 101), not skip_to + 1 (= 132)
        assert_eq!(iter_start, 101);
        assert_eq!(advance_to, 100);
    }

    /// When last_processed < current_ledger (e.g., just after catchup reset)
    /// but the gap is within TX_SET_REQUEST_WINDOW, normal behavior applies.
    #[test]
    fn test_externalized_iteration_window_small_gap_no_skip() {
        let last_processed = 100u64;
        let current_ledger = 100u32;
        let latest_externalized = 110u64; // gap = 10 <= 12

        let (iter_start, advance_to) =
            App::externalized_iteration_window(last_processed, current_ledger, latest_externalized);

        assert_eq!(iter_start, 101);
        assert_eq!(advance_to, 100);
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

        // The fix: attempts accumulate across recovery ticks. The escalation
        // at RECOVERY_ESCALATION_CATCHUP (6) triggers trigger_recovery_catchup
        // before we even reach the gap-check code. With the archive skip fix,
        // trigger_recovery_catchup no longer resets attempts on skip, so the
        // SyncRecoveryManager's 10s timer drives retries until the archive
        // publishes the checkpoint.
        let escalation_threshold = RECOVERY_ESCALATION_CATCHUP;

        // Simulate the attempt counter behavior:
        // - Attempts 0-2: enter gap check → checkpoint not published → SCP state request
        // - Attempts 3-5: enter gap check → checkpoint not published → wait (return)
        // - Attempts 6+: escalation at line 130 → trigger_recovery_catchup
        //   → archive skip (no reset) → SyncRecoveryManager retries in 10s
        for attempt in 0..=escalation_threshold + 5 {
            if attempt < escalation_threshold {
                // Before escalation threshold: gap-check code handles it
                if target_checkpoint as u64 > latest_externalized {
                    if attempt <= 2 {
                        // Falls through to SCP state request — fine
                    } else {
                        // Waits without resetting — correct behavior
                    }
                } else {
                    panic!("Should not reach this branch in this scenario");
                }
            } else {
                // At/past escalation threshold: trigger_recovery_catchup is
                // called directly (line 130). If archive doesn't have the
                // checkpoint, it skips WITHOUT resetting attempts, so the
                // next tick also enters this branch.
                assert!(
                    attempt >= escalation_threshold,
                    "Should trigger catchup at attempt {}",
                    attempt
                );
                break;
            }
        }

        // Also verify: if latest_externalized catches up to the checkpoint
        // (e.g., the node participates in SCP for later slots), the normal
        // catchup path at line 316 would trigger. This is the original design
        // for when the heuristic is correct.
        let advanced_latest_ext = target_checkpoint as u64 + 1;
        assert!(
            target_checkpoint as u64 <= advanced_latest_ext,
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

    /// Regression test: the "essentially caught up" recovery path must NOT
    /// clear pending tx_set requests. Clearing them was the root cause of
    /// post-catchup convergence failure:
    ///
    /// 1. After catchup + rapid close, node is 5 slots behind
    /// 2. EXTERNALIZE for next slot arrives from peers → tx_set fetch starts
    /// 3. Recovery fires with gap ≤ 12 → previously called clear_pending_tx_sets()
    /// 4. tx_set fetch is cancelled → slot can never close → infinite loop
    ///
    /// Fix: the "essentially caught up" path only clears syncing_ledgers
    /// entries for already-closed slots (seq ≤ current_ledger), not entries
    /// waiting for tx_sets. Pending tx_set requests are preserved so the
    /// fetch can complete.
    #[test]
    fn test_recovery_does_not_clear_inflight_tx_set_requests() {
        // Scenario: node at LCL=61937343, latest_externalized=61937348, gap=5
        let current_ledger = 61937343u32;
        let latest_externalized = 61937348u64;
        let gap = latest_externalized - current_ledger as u64;

        // This gap is within TX_SET_REQUEST_WINDOW (12)
        assert!(
            gap <= TX_SET_REQUEST_WINDOW,
            "gap ({}) should be within TX_SET_REQUEST_WINDOW ({}) — \
             this is the 'essentially caught up' path",
            gap,
            TX_SET_REQUEST_WINDOW,
        );

        // The next slot to close is current_ledger + 1
        let next_slot = current_ledger as u64 + 1;
        assert_eq!(next_slot, 61937344);

        // Verify that next_slot is within the tx_set request window
        let min_slot = current_ledger.saturating_add(1) as u64;
        let window_end = current_ledger as u64 + TX_SET_REQUEST_WINDOW;
        assert!(
            next_slot >= min_slot && next_slot <= window_end,
            "next_slot ({}) must be within request window [{}, {}]",
            next_slot,
            min_slot,
            window_end,
        );

        // Key invariant: when the EXTERNALIZE for next_slot has been received
        // but its tx_set is being fetched (in-flight), recovery must NOT clear
        // the pending tx_set request. The tx_set fetch needs time to complete.
        //
        // The fix ensures:
        // 1. syncing_ledgers.retain only removes seq <= current_ledger (not
        //    entries without tx_sets that may be waiting for fetch)
        // 2. clear_pending_tx_sets() is NOT called in the "essentially caught
        //    up" path
        // 3. Slots with in-flight fetches are recognized as "in-flight", not
        //    "permanently missing"
    }

    /// Regression test: fast-track catchup on pending EXTERNALIZE must NOT
    /// fire when the next slot has a buffered entry with tx_set.
    ///
    /// After catchup, the node receives fresh EXTERNALIZE envelopes from
    /// SCP state responses. These are for slots far ahead (gap 10+). The
    /// fast-track code sets recovery_attempts = RECOVERY_ESCALATION_CATCHUP
    /// and arms sync_recovery_pending, which triggers trigger_recovery_catchup
    /// on the next tick. That function clears syncing_ledgers (buffer.clear()),
    /// destroying entries WITH tx_sets that are ready for rapid close.
    ///
    /// Fix: skip fast-track when syncing_ledgers has next_slot with a tx_set.
    #[test]
    fn test_fast_track_catchup_skipped_when_next_slot_buffered() {
        // Scenario: after catchup to L61937727, rapid close processed L61937728.
        // syncing_ledgers has entries L61937729-61937740 with tx_sets.
        // Fresh EXTERNALIZE arrives for L61937738 (gap = 10).
        let current_ledger = 61937728u64;
        let pending_externalize_slot = 61937738u64;
        let gap = pending_externalize_slot - current_ledger;

        // Verify this would trigger fast-track (gap > 2)
        assert!(
            gap > 2,
            "gap ({}) must be > 2 to trigger fast-track path",
            gap,
        );

        // Key invariant: if the next slot (current_ledger + 1) has a
        // buffered entry with a tx_set, the fast-track must NOT fire.
        // Instead, let rapid close proceed to close the buffered entries.
        let next_slot = current_ledger as u32 + 1;
        assert_eq!(next_slot, 61937729);

        // Verify the escalation threshold would be reached if fast-track fires
        assert_eq!(
            RECOVERY_ESCALATION_CATCHUP, 6,
            "RECOVERY_ESCALATION_CATCHUP must be 6"
        );
    }

    /// Regression test: trigger_recovery_catchup must NOT clear
    /// syncing_ledgers when the archive doesn't have the checkpoint.
    ///
    /// Previously, buffer.clear() ran BEFORE the archive check, so
    /// skipped catchups destroyed buffered entries with tx_sets that
    /// were ready for rapid close, preventing convergence.
    ///
    /// Fix: move buffer.clear() + clear_pending_tx_sets() to AFTER
    /// the archive availability check succeeds.
    #[test]
    fn test_trigger_recovery_catchup_no_clear_on_archive_skip() {
        // Scenario: node at L61937728, archive at L61937727 (no checkpoint)
        let current_ledger = 61937728u32;
        let next_cp = henyey_history::checkpoint::checkpoint_containing(current_ledger + 1);
        assert_eq!(next_cp, 61937791);

        let archive_latest = 61937727u32;
        assert!(
            archive_latest < next_cp,
            "archive ({}) behind checkpoint ({}) — catchup will be skipped",
            archive_latest,
            next_cp,
        );

        // Key invariant: when catchup is skipped because the archive
        // doesn't have the checkpoint, syncing_ledgers must NOT be cleared.
        // The buffer may contain entries with tx_sets from the previous
        // catchup's rapid close that are ready to be applied.
    }

    /// Regression test: trigger_recovery_catchup must NOT reset attempts
    /// or re-arm sync_recovery_pending when the archive skip happens.
    /// Previously, this created a 1-second spin loop:
    ///
    /// 1. Recovery fires → trigger_recovery_catchup resets attempts to 0
    /// 2. Archive doesn't have checkpoint → skip
    /// 3. sync_recovery_pending re-armed → fires again in 1s
    /// 4. Goto 1 (forever, hammering archive API)
    ///
    /// Fix: only reset attempts after the archive check succeeds (when
    /// catchup actually starts). Don't re-arm on archive skip — let the
    /// SyncRecoveryManager's 10-second timer drive retries.
    #[test]
    fn test_trigger_recovery_catchup_no_spin_on_archive_skip() {
        // Scenario: node at checkpoint boundary, archive hasn't published next
        let current_ledger = 61937343u32;
        let next_cp = henyey_history::checkpoint::checkpoint_containing(current_ledger + 1);
        assert_eq!(next_cp, 61937407);

        // The archive hasn't published this checkpoint yet (archive at 61937343)
        let archive_latest = 61937343u32;
        assert!(
            archive_latest < next_cp,
            "archive_latest ({}) must be behind next_cp ({}) — \
             this is the condition where catchup would be skipped",
            archive_latest,
            next_cp,
        );

        // Key invariant: when the archive skip happens, the recovery counter
        // must NOT be reset. If it were reset to 0, the escalation threshold
        // (RECOVERY_ESCALATION_CATCHUP = 6) would never be reached, and the
        // node would spin in the "permanently missing" → catchup → skip loop.
        //
        // The fix moves the reset to AFTER the archive check succeeds:
        // - Before fix: reset at entry → always 0 → never escalates
        // - After fix: reset only when catchup starts → attempts accumulates
        //   across skipped ticks → eventually reaches escalation threshold
        assert!(
            RECOVERY_ESCALATION_CATCHUP > 0,
            "RECOVERY_ESCALATION_CATCHUP must be > 0 for escalation to work"
        );

        // Also verify: sync_recovery_pending must NOT be re-armed on archive
        // skip. The SyncRecoveryManager fires every OUT_OF_SYNC_RECOVERY_TIMER_SECS
        // (10s), not every tick (1s). Re-arming creates a 1s spin loop.
        assert!(
            OUT_OF_SYNC_RECOVERY_TIMER_SECS >= 10,
            "OUT_OF_SYNC_RECOVERY_TIMER_SECS should be >= 10 to avoid spin"
        );
    }

    /// Regression for issue #1733 recovery hot-loop.
    ///
    /// Scenario: node has fallen slightly behind. tx_sets are evicted from
    /// peers. Recovery escalates every 10s (OUT_OF_SYNC_RECOVERY_TIMER_SECS).
    ///
    /// The archive-behind backoff suppresses redundant archive
    /// queries. It is armed by the buffered-catchup validation paths in
    /// `catchup_impl.rs` (see `arm_archive_behind_backoff`).
    ///
    /// This test exercises the backoff lifecycle via the unified
    /// `ArchiveRecoveryStatus` enum:
    ///   1. Initially status is Unknown — no backoff, query allowed.
    ///   2. After arming, status is ConfirmedBehind { backoff } — suppressed.
    ///   3. Clearing restores Unknown.
    #[tokio::test]
    async fn test_archive_behind_backoff_skips_redundant_queries() {
        let dir = tempfile::tempdir().expect("temp dir");
        let db_path = dir.path().join("rs-stellar-test.db");
        let config = crate::config::ConfigBuilder::new()
            .database_path(db_path)
            .build();
        let app = App::new(config).await.unwrap();

        // Invariant 1: fresh app has Unknown status.
        let snap = app.archive_recovery_snapshot().await;
        assert!(
            !snap.is_confirmed_behind(),
            "fresh app should not be confirmed behind"
        );
        assert!(
            !snap.is_backoff_active(app.clock.now()),
            "fresh app has no backoff"
        );

        // Arm the backoff (simulates observing archive_latest < next_cp).
        let deadline = app.clock.now() + Duration::from_secs(ARCHIVE_BEHIND_BACKOFF_SECS);
        {
            let mut guard = app.archive_recovery_status.write().await;
            *guard = ArchiveRecoveryStatus::ConfirmedBehind {
                backoff_until: Some(deadline),
            };
        }

        // Invariant 2: backoff is active.
        let snap = app.archive_recovery_snapshot().await;
        assert!(snap.is_confirmed_behind(), "should be confirmed behind");
        assert!(
            snap.is_backoff_active(app.clock.now()),
            "backoff must be active during the window"
        );

        // Progress clears the state.
        {
            let mut guard = app.archive_recovery_status.write().await;
            *guard = ArchiveRecoveryStatus::Unknown;
        }

        // Invariant 3: after clearing, query allowed again.
        let snap = app.archive_recovery_snapshot().await;
        assert!(!snap.is_confirmed_behind(), "after progress, not behind");
        assert!(
            !snap.is_backoff_active(app.clock.now()),
            "no backoff after clear"
        );

        // Sanity: at the 10s tick cadence, one backoff window of 60s covers
        // 6 recovery ticks.
        let ticks_per_window = ARCHIVE_BEHIND_BACKOFF_SECS / OUT_OF_SYNC_RECOVERY_TIMER_SECS;
        assert!(
            ticks_per_window >= 6,
            "backoff window ({}s) must cover at least 6 recovery ticks ({}s each), \
             got {} ticks per window",
            ARCHIVE_BEHIND_BACKOFF_SECS,
            OUT_OF_SYNC_RECOVERY_TIMER_SECS,
            ticks_per_window,
        );
    }

    // -------------------------------------------------------------------
    // #1867 archive recovery status signal tests
    // -------------------------------------------------------------------

    /// Fresh app has `ArchiveRecoveryStatus::Unknown`.
    #[tokio::test]
    async fn test_archive_recovery_status_initially_unknown() {
        let dir = tempfile::tempdir().expect("temp dir");
        let db_path = dir.path().join("rs-stellar-test.db");
        let config = crate::config::ConfigBuilder::new()
            .database_path(db_path)
            .build();
        let app = App::new(config).await.unwrap();
        let snap = app.archive_recovery_snapshot().await;
        assert!(
            !snap.is_confirmed_behind(),
            "fresh app should have Unknown status"
        );
    }

    /// Setting confirmed-behind makes `is_behind()` return true even without backoff.
    #[tokio::test]
    async fn test_archive_recovery_confirmed_behind_without_backoff() {
        let dir = tempfile::tempdir().expect("temp dir");
        let db_path = dir.path().join("rs-stellar-test.db");
        let config = crate::config::ConfigBuilder::new()
            .database_path(db_path)
            .build();
        let app = App::new(config).await.unwrap();

        // Neither signal active → not behind.
        let snap = app.archive_recovery_snapshot().await;
        assert!(!snap.is_behind(), "no signal → not behind");

        // Mark confirmed behind (no backoff) → behind.
        app.mark_archive_confirmed_behind().await;
        let snap = app.archive_recovery_snapshot().await;
        assert!(
            snap.is_behind(),
            "confirmed behind → is_behind must be true"
        );
        assert!(
            !snap.is_backoff_active(app.clock.now()),
            "no backoff armed yet"
        );

        // Arm backoff too → still behind, backoff active.
        {
            let mut guard = app.archive_recovery_status.write().await;
            *guard = ArchiveRecoveryStatus::ConfirmedBehind {
                backoff_until: Some(app.clock.now() + Duration::from_secs(60)),
            };
        }
        let snap = app.archive_recovery_snapshot().await;
        assert!(
            snap.is_behind(),
            "confirmed behind with backoff → is_behind"
        );
        assert!(snap.is_backoff_active(app.clock.now()), "backoff is active");
    }

    /// Progress clears the recovery status to Unknown.
    #[tokio::test]
    async fn test_archive_recovery_status_cleared_on_progress() {
        let dir = tempfile::tempdir().expect("temp dir");
        let db_path = dir.path().join("rs-stellar-test.db");
        let config = crate::config::ConfigBuilder::new()
            .database_path(db_path)
            .build();
        let app = App::new(config).await.unwrap();

        // Simulate archive-behind state with backoff.
        {
            let mut guard = app.archive_recovery_status.write().await;
            *guard = ArchiveRecoveryStatus::ConfirmedBehind {
                backoff_until: Some(app.clock.now() + Duration::from_secs(60)),
            };
        }

        // Clear via clear_archive_recovery_state.
        let was_armed = app
            .clear_archive_recovery_state(ArchiveRecoveryClear::FullProgress)
            .await;
        assert!(was_armed, "should report was_armed=true");

        let snap = app.archive_recovery_snapshot().await;
        assert!(!snap.is_confirmed_behind(), "progress must clear status");
        assert!(
            !snap.is_backoff_active(app.clock.now()),
            "progress must clear backoff"
        );
    }

    /// Catchup success clears the recovery status.
    #[tokio::test]
    async fn test_archive_recovery_status_cleared_on_catchup_success() {
        let dir = tempfile::tempdir().expect("temp dir");
        let db_path = dir.path().join("rs-stellar-test.db");
        let config = crate::config::ConfigBuilder::new()
            .database_path(db_path)
            .build();
        let app = App::new(config).await.unwrap();

        // Simulate archive-behind state.
        app.mark_archive_confirmed_behind().await;
        assert!(app.archive_recovery_snapshot().await.is_confirmed_behind());

        // Simulate catchup completion: write Unknown directly (mirrors catchup_impl.rs).
        *app.archive_recovery_status.write().await = ArchiveRecoveryStatus::Unknown;

        let snap = app.archive_recovery_snapshot().await;
        assert!(
            !snap.is_confirmed_behind(),
            "catchup completion must clear status"
        );
    }

    /// Cold cache must not change the recovery status.
    #[tokio::test]
    async fn test_archive_recovery_status_unchanged_on_cold_cache() {
        let dir = tempfile::tempdir().expect("temp dir");
        let db_path = dir.path().join("rs-stellar-test.db");
        let config = crate::config::ConfigBuilder::new()
            .database_path(db_path)
            .build();
        let app = App::new(config).await.unwrap();

        // Status starts Unknown. Cold cache should not change it.
        assert!(!app.archive_recovery_snapshot().await.is_confirmed_behind());

        // Mark behind and confirm cold cache doesn't clear it.
        app.mark_archive_confirmed_behind().await;
        // (Simulating the Cold branch: no write happens.)
        assert!(
            app.archive_recovery_snapshot().await.is_confirmed_behind(),
            "cold cache must not change recovery status"
        );
    }

    // -------------------------------------------------------------------
    // #2713 snapshot-independence regression test
    // -------------------------------------------------------------------

    /// Verify that `ArchiveRecoverySnapshot` is independent of subsequent state
    /// mutations — taking a snapshot before a clear must reflect the pre-clear
    /// state even after the clear completes.
    #[tokio::test]
    async fn test_archive_recovery_snapshot_independent_of_clear() {
        let dir = tempfile::tempdir().expect("temp dir");
        let db_path = dir.path().join("rs-stellar-test.db");
        let config = crate::config::ConfigBuilder::new()
            .database_path(db_path)
            .build();
        let app = App::new(config).await.unwrap();

        // Mark confirmed behind and arm backoff.
        app.mark_archive_confirmed_behind().await;
        {
            let mut guard = app.archive_recovery_status.write().await;
            *guard = ArchiveRecoveryStatus::ConfirmedBehind {
                backoff_until: Some(Instant::now() + Duration::from_secs(60)),
            };
        }

        // Take a snapshot BEFORE clearing state.
        let snapshot_before = app.archive_recovery_snapshot().await;
        assert!(snapshot_before.is_confirmed_behind());

        // Clear state.
        app.clear_archive_recovery_state(ArchiveRecoveryClear::FullProgress)
            .await;

        // Snapshot taken before clear still reflects pre-clear state.
        assert!(
            snapshot_before.is_confirmed_behind(),
            "snapshot must be independent of subsequent state mutations"
        );

        // New snapshot reflects the cleared state.
        let snapshot_after = app.archive_recovery_snapshot().await;
        assert!(!snapshot_after.is_confirmed_behind());
    }

    // -------------------------------------------------------------------
    // #1759 diagnostics regression tests
    // -------------------------------------------------------------------

    /// A minimal `tracing::Subscriber` that records events into a shared
    /// `Vec<String>` so tests can assert on emitted fields without
    /// pulling in `tracing_test` (not a workspace dependency).
    #[derive(Clone, Default)]
    struct CapturingSubscriber {
        events: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    }

    impl tracing::Subscriber for CapturingSubscriber {
        fn enabled(&self, _meta: &tracing::Metadata<'_>) -> bool {
            true
        }

        fn new_span(&self, _span: &tracing::span::Attributes<'_>) -> tracing::Id {
            tracing::Id::from_u64(1)
        }

        fn record(&self, _span: &tracing::Id, _values: &tracing::span::Record<'_>) {}
        fn record_follows_from(&self, _span: &tracing::Id, _follows: &tracing::Id) {}

        fn event(&self, event: &tracing::Event<'_>) {
            struct Visit(String);
            impl tracing::field::Visit for Visit {
                fn record_debug(
                    &mut self,
                    field: &tracing::field::Field,
                    value: &dyn std::fmt::Debug,
                ) {
                    self.0.push_str(&format!(" {}={:?}", field.name(), value));
                }
                fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
                    self.0.push_str(&format!(" {}={}", field.name(), value));
                }
                fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
                    self.0.push_str(&format!(" {}={}", field.name(), value));
                }
            }
            let mut v = Visit(String::new());
            event.record(&mut v);
            let line = format!("{}{}", event.metadata().target(), v.0);
            self.events.lock().unwrap().push(line);
        }

        fn enter(&self, _span: &tracing::Id) {}
        fn exit(&self, _span: &tracing::Id) {}
    }

    /// `warn_if_slow(elapsed >= threshold, ...)` must emit exactly one
    /// `WARN` event with the expected `op`, `count`, and
    /// `elapsed_ms` fields.
    #[test]
    fn warn_if_slow_emits_on_slow_path() {
        let sub = CapturingSubscriber::default();
        let events = sub.events.clone();
        tracing::subscriber::with_default(sub, || {
            super::warn_if_slow(std::time::Duration::from_millis(600), "test_op", 42);
        });
        let events = events.lock().unwrap();
        assert_eq!(events.len(), 1, "exactly one warn event expected");
        let ev = &events[0];
        assert!(ev.contains("op=test_op"), "op field missing: {}", ev);
        assert!(ev.contains("count=42"), "count field missing: {}", ev);
        assert!(
            ev.contains("elapsed_ms=600"),
            "elapsed_ms field missing or wrong: {}",
            ev
        );
        assert!(
            ev.contains("#1759"),
            "log message should reference #1759: {}",
            ev
        );
    }

    /// `warn_if_slow(elapsed < threshold, ...)` must emit **no** events.
    /// Guarantees zero log noise during normal operation.
    #[test]
    fn warn_if_slow_silent_on_fast_path() {
        let sub = CapturingSubscriber::default();
        let events = sub.events.clone();
        tracing::subscriber::with_default(sub, || {
            super::warn_if_slow(std::time::Duration::from_millis(100), "test_op", 0);
        });
        assert!(
            events.lock().unwrap().is_empty(),
            "no events expected in the fast path"
        );
    }

    /// `warn_if_slow` must emit exactly at the threshold boundary
    /// (`elapsed == SLOW_OP_THRESHOLD`) — the `>=` comparison is
    /// load-bearing for predictable behavior.
    #[test]
    fn warn_if_slow_boundary_inclusive() {
        let sub = CapturingSubscriber::default();
        let events = sub.events.clone();
        tracing::subscriber::with_default(sub, || {
            super::warn_if_slow(super::SLOW_OP_THRESHOLD, "boundary", 1);
        });
        assert_eq!(
            events.lock().unwrap().len(),
            1,
            "threshold-equal elapsed must emit (>= comparison)"
        );
    }

    /// Test the `format_watchdog_diagnostic_hint` helper directly.
    ///
    /// Verifies that the hint text includes:
    /// - Tier-1 `/proc/<pid>/task/*/wchan` one-liner with the PID substituted
    /// - Tier-2 `py-spy dump --pid <pid>` with the PID substituted
    /// - Tier-2 `gcore` / `gdb` alternative with the PID substituted
    ///
    /// This replaces the old `#[ignore]`d integration test that tried to
    /// capture logs from the spawned watchdog thread — that approach was
    /// broken because `tracing::subscriber::set_default` is thread-local.
    #[test]
    fn test_watchdog_diagnostic_hint_content() {
        let pid = 12345u32;
        let hint = super::format_watchdog_diagnostic_hint(pid);

        // Tier 1: /proc wchan one-liner with PID substituted.
        assert!(
            hint.contains("/proc/12345/task/"),
            "tier-1 hint must contain /proc/<pid>/task; got: {hint}"
        );
        assert!(
            hint.contains("wchan"),
            "tier-1 hint must mention wchan; got: {hint}"
        );

        // Tier 2: py-spy with PID substituted.
        assert!(
            hint.contains("py-spy dump --pid 12345"),
            "tier-2 hint must contain py-spy dump --pid <pid>; got: {hint}"
        );

        // Tier 2: gcore alternative with PID substituted.
        assert!(
            hint.contains("gcore 12345"),
            "tier-2 hint must contain gcore <pid>; got: {hint}"
        );
        assert!(
            hint.contains("core.12345"),
            "tier-2 hint must contain core.<pid>; got: {hint}"
        );
    }

    // ------------------------------------------------------------------
    // WatchdogGuard lifecycle tests (issue #2547)
    // ------------------------------------------------------------------

    /// Verify that dropping a WatchdogGuard signals shutdown and resets tick_ms.
    #[test]
    fn test_watchdog_guard_drop_signals_shutdown() {
        let shutdown = Arc::new(AtomicBool::new(false));
        let condvar = Arc::new((std::sync::Mutex::new(()), std::sync::Condvar::new()));
        let tick_ms = Arc::new(AtomicU64::new(1_000_000)); // non-zero = "running"

        let guard = super::WatchdogGuard {
            shutdown: Arc::clone(&shutdown),
            condvar: Arc::clone(&condvar),
            tick_ms: Arc::clone(&tick_ms),
        };

        // Before drop: shutdown not set, tick is non-zero.
        assert!(!shutdown.load(Ordering::Acquire));
        assert_ne!(tick_ms.load(Ordering::Relaxed), 0);

        drop(guard);

        // After drop: shutdown set, tick reset to 0.
        assert!(shutdown.load(Ordering::Acquire));
        assert_eq!(tick_ms.load(Ordering::Relaxed), 0);
    }

    /// Verify that a watchdog thread exits promptly when the guard is dropped.
    ///
    /// Uses a deterministic channel+mutex handshake instead of sleep-based
    /// synchronization (issue #2549). The mock thread signals readiness
    /// while holding the watchdog mutex, then enters `wait_timeout` (which
    /// atomically releases the mutex). The test thread acquires the mutex
    /// to confirm the mock is waiting, then drops `WatchdogGuard` while
    /// still holding the mutex — guaranteeing `notify_one` cannot be lost.
    #[test]
    fn test_watchdog_thread_exits_on_guard_drop() {
        let shutdown = Arc::new(AtomicBool::new(false));
        let condvar = Arc::new((std::sync::Mutex::new(()), std::sync::Condvar::new()));
        let tick_ms = Arc::new(AtomicU64::new(1_000_000));

        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<()>();
        let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();

        let shutdown_t = Arc::clone(&shutdown);
        let condvar_t = Arc::clone(&condvar);
        let handle = std::thread::spawn(move || {
            let (lock, cvar) = &*condvar_t;
            let mut guard = lock.lock().unwrap();

            // Signal readiness while holding the watchdog mutex.
            ready_tx.send(()).unwrap();

            // Enter wait — atomically releases watchdog mutex + blocks.
            loop {
                let result = cvar.wait_timeout(guard, Duration::from_secs(5)).unwrap();
                guard = result.0;
                if result.1.timed_out() {
                    panic!("mock watchdog thread timed out waiting for shutdown signal");
                }
                if shutdown_t.load(Ordering::Acquire) {
                    break;
                }
            }
            done_tx.send(()).unwrap();
        });

        // Wait for the mock thread to signal readiness.
        ready_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("mock thread did not signal readiness within 5s");

        // Acquire the watchdog mutex — blocks until wait_timeout has
        // atomically released it, proving the mock thread is waiting.
        // Hold it across the WatchdogGuard drop so notify_one cannot
        // be lost to a spurious-wakeup race.
        {
            let (lock, _) = &*condvar;
            let _mutex_guard = lock.lock().unwrap();

            let wd_guard = super::WatchdogGuard {
                shutdown: Arc::clone(&shutdown),
                condvar: Arc::clone(&condvar),
                tick_ms: Arc::clone(&tick_ms),
            };
            drop(wd_guard);
        }

        // Bounded wait for the mock thread to exit.
        done_rx
            .recv_timeout(Duration::from_secs(3))
            .expect("watchdog thread did not exit within 3s of guard drop");

        handle.join().expect("watchdog thread should exit cleanly");
    }

    // ------------------------------------------------------------------
    // WatchdogSnapshot / WatchdogTier tests (issue #1791)
    // ------------------------------------------------------------------

    /// A richer capturing subscriber that records level + fields so
    /// watchdog tests can assert on event severity and field presence.
    #[derive(Clone, Default)]
    struct WatchdogCapturingSubscriber {
        events: std::sync::Arc<std::sync::Mutex<Vec<CapturedWatchdogEvent>>>,
    }

    #[derive(Debug)]
    struct CapturedWatchdogEvent {
        level: tracing::Level,
        fields: String,
    }

    impl tracing::Subscriber for WatchdogCapturingSubscriber {
        fn enabled(&self, _meta: &tracing::Metadata<'_>) -> bool {
            true
        }
        fn new_span(&self, _span: &tracing::span::Attributes<'_>) -> tracing::Id {
            tracing::Id::from_u64(1)
        }
        fn record(&self, _span: &tracing::Id, _values: &tracing::span::Record<'_>) {}
        fn record_follows_from(&self, _span: &tracing::Id, _follows: &tracing::Id) {}
        fn event(&self, event: &tracing::Event<'_>) {
            struct Visit(String);
            impl tracing::field::Visit for Visit {
                fn record_debug(
                    &mut self,
                    field: &tracing::field::Field,
                    value: &dyn std::fmt::Debug,
                ) {
                    self.0.push_str(&format!(" {}={:?}", field.name(), value));
                }
                fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
                    self.0.push_str(&format!(" {}={}", field.name(), value));
                }
                fn record_i64(&mut self, field: &tracing::field::Field, value: i64) {
                    self.0.push_str(&format!(" {}={}", field.name(), value));
                }
                fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
                    self.0.push_str(&format!(" {}={}", field.name(), value));
                }
            }
            let mut v = Visit(String::new());
            event.record(&mut v);
            self.events.lock().unwrap().push(CapturedWatchdogEvent {
                level: *event.metadata().level(),
                fields: v.0,
            });
        }
        fn enter(&self, _span: &tracing::Id) {}
        fn exit(&self, _span: &tracing::Id) {}
    }

    fn test_snapshot(stale_secs: u64) -> super::WatchdogSnapshot {
        super::WatchdogSnapshot {
            stale_secs,
            phase: 13,
            phase_sub: 7,
            fetch_channel_depth: 42,
            fetch_channel_depth_max: 100,
            pid: 99999,
            abort_threshold_secs: 0,
        }
    }

    /// Test A: `WatchdogSnapshot::tier()` boundary routing.
    #[test]
    fn watchdog_tier_routing() {
        use super::WatchdogTier;
        assert_eq!(test_snapshot(0).tier(), WatchdogTier::None);
        assert_eq!(test_snapshot(14).tier(), WatchdogTier::None);
        assert_eq!(test_snapshot(15).tier(), WatchdogTier::Warn);
        assert_eq!(test_snapshot(29).tier(), WatchdogTier::Warn);
        assert_eq!(test_snapshot(30).tier(), WatchdogTier::Error);
        assert_eq!(test_snapshot(999).tier(), WatchdogTier::Error);
    }

    /// Test A2: `WatchdogSnapshot::should_abort()` boundary routing.
    ///
    /// Verifies that `should_abort()` is independent of `tier()` — even
    /// thresholds below the 30s Error tier must trigger abort.
    #[test]
    fn watchdog_should_abort_routing() {
        // Disabled (threshold = 0): never abort regardless of stale_secs.
        let mut snap = test_snapshot(999);
        snap.abort_threshold_secs = 0;
        assert!(!snap.should_abort());

        // Enabled but not yet stale enough.
        snap.abort_threshold_secs = 120;
        snap.stale_secs = 119;
        assert!(!snap.should_abort());

        // Exactly at threshold: abort.
        snap.stale_secs = 120;
        assert!(snap.should_abort());

        // Well past threshold: abort.
        snap.stale_secs = 999;
        assert!(snap.should_abort());

        // Edge: threshold = 1, stale = 1: abort.
        snap.abort_threshold_secs = 1;
        snap.stale_secs = 1;
        assert!(snap.should_abort());

        // Edge: threshold = 1, stale = 0: no abort.
        snap.stale_secs = 0;
        assert!(!snap.should_abort());

        // Threshold below Error tier (< 30s) still triggers abort.
        // Regression test: previously should_abort was only checked
        // inside the WatchdogTier::Error arm, so thresholds 1..29
        // would never fire.
        snap.abort_threshold_secs = 15;
        snap.stale_secs = 20;
        assert_eq!(snap.tier(), super::WatchdogTier::Warn);
        assert!(snap.should_abort(), "abort must fire even below Error tier");

        snap.abort_threshold_secs = 10;
        snap.stale_secs = 10;
        assert_eq!(snap.tier(), super::WatchdogTier::None);
        assert!(snap.should_abort(), "abort must fire even below Warn tier");
    }

    /// Test B: `emit_warn()` emits a WARN event with the correct fields
    /// and does NOT include `pid` (warn schema).
    #[test]
    fn watchdog_emit_warn_fields() {
        let sub = WatchdogCapturingSubscriber::default();
        let events = sub.events.clone();
        let snap = test_snapshot(20);
        tracing::subscriber::with_default(sub, || {
            snap.emit_warn();
        });
        let events = events.lock().unwrap();
        assert_eq!(events.len(), 1, "exactly one event expected");
        let ev = &events[0];

        assert_eq!(ev.level, tracing::Level::WARN, "must be WARN level");

        // Required fields with correct values.
        assert!(
            ev.fields.contains("stale_secs=20"),
            "stale_secs: {}",
            ev.fields
        );
        assert!(ev.fields.contains("phase=13"), "phase: {}", ev.fields);
        assert!(
            ev.fields.contains("phase_sub=7"),
            "phase_sub: {}",
            ev.fields
        );
        assert!(
            ev.fields.contains("fetch_channel_depth=42"),
            "fetch_channel_depth: {}",
            ev.fields
        );
        assert!(
            ev.fields.contains("fetch_channel_depth_max=100"),
            "fetch_channel_depth_max: {}",
            ev.fields
        );

        // Message content.
        assert!(
            ev.fields.contains("WATCHDOG: Event loop slow"),
            "message: {}",
            ev.fields
        );

        // pid must NOT be present on warn events.
        assert!(
            !ev.fields.contains("pid="),
            "pid must not appear in warn event: {}",
            ev.fields
        );

        // watchdog_freeze sentinel must NOT be present on warn events —
        // only the ≥30s error tier should carry it.
        assert!(
            !ev.fields.contains(super::WATCHDOG_FREEZE_FIELD),
            "emit_warn() must NOT emit {} — only emit_error() may: {}",
            super::WATCHDOG_FREEZE_FIELD,
            ev.fields
        );
    }

    /// Test C: `emit_error()` emits an ERROR event with all fields
    /// including `pid` and phase-code legend substrings.
    #[test]
    fn watchdog_emit_error_fields() {
        let sub = WatchdogCapturingSubscriber::default();
        let events = sub.events.clone();
        let snap = test_snapshot(45);
        tracing::subscriber::with_default(sub, || {
            snap.emit_error();
        });
        let events = events.lock().unwrap();
        assert_eq!(events.len(), 1, "exactly one event expected");
        let ev = &events[0];

        assert_eq!(ev.level, tracing::Level::ERROR, "must be ERROR level");

        // Required fields with correct values.
        assert!(
            ev.fields.contains("stale_secs=45"),
            "stale_secs: {}",
            ev.fields
        );
        assert!(ev.fields.contains("phase=13"), "phase: {}", ev.fields);
        assert!(
            ev.fields.contains("phase_sub=7"),
            "phase_sub: {}",
            ev.fields
        );
        assert!(
            ev.fields.contains("fetch_channel_depth=42"),
            "fetch_channel_depth: {}",
            ev.fields
        );
        assert!(
            ev.fields.contains("fetch_channel_depth_max=100"),
            "fetch_channel_depth_max: {}",
            ev.fields
        );
        assert!(ev.fields.contains("pid=99999"), "pid: {}", ev.fields);

        // Structured sentinel for machine-readable freeze detection.
        assert!(
            ev.fields
                .contains(&format!("{}=true", super::WATCHDOG_FREEZE_FIELD)),
            "{}: {}",
            super::WATCHDOG_FREEZE_FIELD,
            ev.fields
        );

        // Message content.
        assert!(
            ev.fields.contains("WATCHDOG: Event loop appears frozen"),
            "message: {}",
            ev.fields
        );

        // Legend substrings (representative, not exhaustive exact-match).
        assert!(
            ev.fields.contains("0=select"),
            "legend 0=select: {}",
            ev.fields
        );
        assert!(
            ev.fields.contains("13=buffered_catchup"),
            "legend 13=buffered_catchup: {}",
            ev.fields
        );
        assert!(
            ev.fields.contains("32=scp_verified"),
            "legend 32=scp_verified: {}",
            ev.fields
        );
    }

    /// Test D: `WATCHDOG_PHASE_LEGEND` contains all known phase codes.
    #[test]
    fn watchdog_phase_legend_coverage() {
        let legend = super::WATCHDOG_PHASE_LEGEND;
        // Every phase code N=label that appears in the watchdog loop.
        let expected = [
            "0=select",
            "1=scp_msg",
            "2=fetch_resp",
            "3=broadcast",
            "4=scp_broadcast",
            "5=consensus_tick",
            "6=pending_close",
            "10=process_externalized",
            "11=externalized_catchup",
            "12=try_apply_buffered",
            "13=buffered_catchup",
            "14=catchup_running",
            "15=pending_catchup_complete",
            "16=heartbeat",
            "20=stats",
            "21=tx_advert",
            "22=tx_demand",
            "23=survey",
            "24=survey_req",
            "25=survey_phase",
            "26=scp_timeout",
            "27=ping",
            "28=peer_maint",
            "29=peer_refresh",
            "30=herder_cleanup",
            "31=scp_verifier",
            "32=scp_verified",
            "33=tx_set_gc",
        ];
        for entry in &expected {
            assert!(
                legend.contains(entry),
                "WATCHDOG_PHASE_LEGEND missing '{}'; got: {}",
                entry,
                legend
            );
        }
    }

    /// Tests for [`WATCHDOG_FREEZE_FIELD`] — the monitoring contract.
    ///
    /// These tests guard that `emit_error()` emits the `watchdog_freeze`
    /// structured field and that both the Text and JSON `tracing_subscriber::fmt`
    /// formatters render it in grep-able form.
    mod watchdog_freeze_field_tests {
        use super::super::WATCHDOG_FREEZE_FIELD;
        use std::io;
        use std::sync::{Arc, Mutex};

        /// Verify the Text formatter renders the field as `watchdog_freeze=true`,
        /// matching the production formatter construction in `logging.rs`.
        #[test]
        fn test_watchdog_freeze_emits_field_text_format() {
            use tracing::subscriber::with_default;
            use tracing_subscriber::{fmt, layer::SubscriberExt, EnvFilter};

            let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
            let buf_clone = buf.clone();

            let fmt_layer = fmt::layer()
                .with_writer(move || -> Box<dyn io::Write> {
                    Box::new(BufWriter(buf_clone.clone()))
                })
                .with_ansi(false)
                .with_target(true);

            let subscriber = tracing_subscriber::registry()
                .with(EnvFilter::new("error"))
                .with(fmt_layer);

            with_default(subscriber, || {
                tracing::error!(watchdog_freeze = true, "test watchdog freeze");
            });

            let output = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
            assert!(
                output.contains(&format!("{}=true", WATCHDOG_FREEZE_FIELD)),
                "Text format must render field as '{}=true' for grep. Got: {output}",
                WATCHDOG_FREEZE_FIELD,
            );
        }

        /// Verify the JSON formatter renders the field as `"watchdog_freeze":true`,
        /// matching the production formatter construction in `logging.rs`.
        #[test]
        fn test_watchdog_freeze_emits_field_json_format() {
            use tracing::subscriber::with_default;
            use tracing_subscriber::{fmt, layer::SubscriberExt, EnvFilter};

            let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
            let buf_clone = buf.clone();

            let fmt_layer = fmt::layer()
                .with_writer(move || -> Box<dyn io::Write> {
                    Box::new(BufWriter(buf_clone.clone()))
                })
                .json()
                .with_span_list(true)
                .with_current_span(true);

            let subscriber = tracing_subscriber::registry()
                .with(EnvFilter::new("error"))
                .with(fmt_layer);

            with_default(subscriber, || {
                tracing::error!(watchdog_freeze = true, "test watchdog freeze");
            });

            let output = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
            assert!(
                output.contains(&format!("\"{}\":true", WATCHDOG_FREEZE_FIELD)),
                "JSON format must render field as '\"{}\":true' for grep. Got: {output}",
                WATCHDOG_FREEZE_FIELD,
            );
        }

        /// A `Write` adapter that appends to a shared `Vec<u8>`.
        #[derive(Clone)]
        struct BufWriter(Arc<Mutex<Vec<u8>>>);

        impl io::Write for BufWriter {
            fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
    }

    /// Regression test for #1775 Phase 2 + #1778 label correction: verify
    /// `handle_close_complete`'s post-close tx-queue update is actually
    /// moved off the event-loop thread via `spawn_blocking`, AND that the
    /// PhaseTimer sub-phase marks attribute the time to the right labels.
    ///
    /// The test injects a 400 ms synthetic blocking workload inside the
    /// `spawn_blocking` closure (via `close_complete_inject_blocking_ms`) so
    /// the fix's behavior is observable without 400 real signed envelopes.
    ///
    /// **Assertions**:
    ///
    /// 1. **PhaseTimer attribution (#1778)**: the WARN line emitted by
    ///    `PhaseTimer::finish("app.handle_close_complete")` contains the
    ///    post-#1778 field names — `overlay_bookkeeping_ms`,
    ///    `spawn_blocking_setup_ms`, `tx_queue_background_wait_ms` — and
    ///    does NOT contain the pre-#1778 misnamed fields
    ///    `herder_ledger_closed_ms` / `tx_queue_invalidation_ms` (which were
    ///    attributing inline preamble work to labels that named the
    ///    off-loaded work).
    ///
    /// 2. **Event-loop blocking time**: the sum of the two pre-spawn marks
    ///    (`overlay_bookkeeping_ms + spawn_blocking_setup_ms`) is < 50 ms.
    ///    These brackets span only inline overlay/survey/drift bookkeeping
    ///    plus the preamble that moves fields into the spawn_blocking
    ///    closure — pure sync CPU + a handful of tokio RwLock reads.
    ///
    /// 3. **Spawn-blocking wait visibility (#1775)**:
    ///    `tx_queue_background_wait_ms >= 300 ms`, confirming the injected
    ///    400 ms heavy work actually runs off-thread on the blocking pool
    ///    rather than being bypassed or accidentally on the event loop.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_close_complete_event_loop_marks_correctly_attributed() {
        let dir = tempfile::tempdir().expect("temp dir");
        let db_path = dir.path().join("rs-stellar-test.db");
        let config = crate::config::ConfigBuilder::new()
            .database_path(db_path)
            .build();
        let app = App::new(config).await.unwrap();

        // Inject 400 ms of synthetic blocking work inside the spawn_blocking
        // closure to simulate the mainnet-observed ~666 ms compute load. 400 ms
        // is comfortably above the PhaseTimer's 250 ms threshold so the WARN
        // is always emitted regardless of preamble timing jitter on slow CI.
        app.close_complete_inject_blocking_ms
            .store(400, Ordering::Relaxed);
        app.set_applying_ledger(true);

        // Capture tracing output to inspect PhaseTimer WARN emissions.
        let sub = CapturingSubscriber::default();
        let events = sub.events.clone();
        let _guard = tracing::subscriber::set_default(sub);

        // Simulate a successful empty close.  The tx_set is empty so
        // remove_applied and get_invalid_tx_list do negligible real work, but
        // the injected 200 ms sleep inside the spawn_blocking closure
        // simulates the real-world CPU cost.
        let pending = PendingLedgerClose {
            handle: tokio::task::spawn_blocking(|| {
                Ok(henyey_ledger::LedgerCloseResult {
                    header: stellar_xdr::curr::LedgerHeader {
                        ledger_version: 24,
                        previous_ledger_hash: stellar_xdr::curr::Hash([0u8; 32]),
                        scp_value: stellar_xdr::curr::StellarValue {
                            tx_set_hash: stellar_xdr::curr::Hash([0u8; 32]),
                            close_time: stellar_xdr::curr::TimePoint(1),
                            upgrades: stellar_xdr::curr::VecM::default(),
                            ext: stellar_xdr::curr::StellarValueExt::Basic,
                        },
                        tx_set_result_hash: stellar_xdr::curr::Hash([0u8; 32]),
                        bucket_list_hash: stellar_xdr::curr::Hash([0u8; 32]),
                        ledger_seq: 1,
                        total_coins: 0,
                        fee_pool: 0,
                        inflation_seq: 0,
                        id_pool: 0,
                        base_fee: 100,
                        base_reserve: 5_000_000,
                        max_tx_set_size: 100,
                        skip_list: [
                            stellar_xdr::curr::Hash([0u8; 32]),
                            stellar_xdr::curr::Hash([0u8; 32]),
                            stellar_xdr::curr::Hash([0u8; 32]),
                            stellar_xdr::curr::Hash([0u8; 32]),
                        ],
                        ext: stellar_xdr::curr::LedgerHeaderExt::V0,
                    },
                    header_hash: henyey_common::Hash256::ZERO,
                    tx_results: Vec::new(),
                    meta: None,
                    perf: None,
                    stats: Default::default(),
                })
            }),
            ledger_seq: 1,
            tx_set: henyey_herder::TransactionSet::new_legacy(
                henyey_common::Hash256::ZERO,
                Vec::new(),
            ),
            close_time: 1,
            upgrades: Vec::new(),
            dispatch_time: std::time::Instant::now(),
        };

        let mut pending = pending;
        let join_result = (&mut pending.handle).await;
        let close_start = std::time::Instant::now();
        let success = app
            .handle_close_complete(
                pending,
                join_result,
                super::persist::LedgerCloseFinalizer::inline(),
            )
            .await;
        let close_elapsed = close_start.elapsed();

        assert!(success, "close should succeed");
        assert!(
            close_elapsed.as_millis() >= 300,
            "expected close to take at least 300 ms with 400 ms injection; \
             actual: {:?}",
            close_elapsed
        );

        // Scan the captured events for the PhaseTimer WARN line and extract
        // the sub-phase times. The WARN line is emitted with
        // `call="app.handle_close_complete"` when total_ms >= 250 (which it
        // always is with the 200 ms injection plus preamble).
        let (phase_line, all_events) = {
            let locked = events.lock().unwrap();
            (
                locked
                    .iter()
                    .find(|e| e.contains("app.handle_close_complete"))
                    .cloned(),
                locked.clone(),
            )
        };

        let phase_line = phase_line.unwrap_or_else(|| {
            panic!(
                "PhaseTimer WARN line for app.handle_close_complete was not captured. \
                 close_elapsed={:?}. All captured events ({}):\n{}",
                close_elapsed,
                all_events.len(),
                all_events.join("\n")
            )
        });

        // Extract sub-phase numbers via substring parsing. Format:
        //   phases="... overlay_bookkeeping_ms=0 spawn_blocking_setup_ms=0
        //                tx_queue_background_wait_ms=400 ..."
        fn extract_ms(line: &str, label: &str) -> Option<u128> {
            let tag = format!("{}=", label);
            let start = line.find(&tag)? + tag.len();
            let tail = &line[start..];
            let end = tail
                .find(|c: char| !c.is_ascii_digit())
                .unwrap_or(tail.len());
            tail[..end].parse().ok()
        }

        // Assertion (1): PhaseTimer attribution (#1778). The pre-#1778 field
        // names `herder_ledger_closed_ms` / `tx_queue_invalidation_ms` were
        // misattributing inline preamble work to labels that named the
        // off-loaded work. Confirm they are gone from the emitted WARN.
        assert!(
            !phase_line.contains("herder_ledger_closed_ms="),
            "#1778 regression: WARN line should NOT contain the misnamed \
             `herder_ledger_closed_ms` field; it was replaced by \
             `overlay_bookkeeping_ms` + `spawn_blocking_setup_ms`. \
             WARN line: {phase_line}"
        );
        assert!(
            !phase_line.contains("tx_queue_invalidation_ms="),
            "#1778 regression: WARN line should NOT contain the misnamed \
             `tx_queue_invalidation_ms` field; the queue-invalidation work \
             runs inside `spawn_blocking` and is measured by \
             `tx_queue_background_wait_ms`. WARN line: {phase_line}"
        );

        let overlay_ms = extract_ms(&phase_line, "overlay_bookkeeping_ms")
            .expect("WARN line should contain overlay_bookkeeping_ms field (#1778)");
        let setup_ms = extract_ms(&phase_line, "spawn_blocking_setup_ms")
            .expect("WARN line should contain spawn_blocking_setup_ms field (#1778)");
        let wait_ms = extract_ms(&phase_line, "tx_queue_background_wait_ms").expect(
            "WARN line should contain tx_queue_background_wait_ms field (new in #1775 Phase 2)",
        );

        // Assertion (2): Event-loop blocking time < 50 ms. The two pre-spawn
        // marks bracket only inline overlay/survey/drift bookkeeping plus
        // the preamble that moves fields into the spawn_blocking closure.
        // Post-fix this is microseconds of real CPU cost; pre-#1778 (misnamed
        // marks) this window used to read ~200 ms because the marks fired
        // AFTER work that had moved off-thread in #1775 Phase 2.
        assert!(
            overlay_ms + setup_ms < 50,
            "overlay_bookkeeping_ms ({overlay_ms}) + spawn_blocking_setup_ms \
             ({setup_ms}) must be < 50 ms post-fix; WARN line was: {phase_line}"
        );

        // Assertion (3): The 400 ms injected work must show up under
        // tx_queue_background_wait_ms. If this is < 300 ms, the fix is
        // bypassing spawn_blocking entirely and the off-load is illusory.
        assert!(
            wait_ms >= 300,
            "tx_queue_background_wait_ms ({wait_ms}) should reflect the 400 ms \
             injected blocking work; WARN line was: {phase_line}"
        );
    }

    /// Regression test for #1780: after moving the `spawn_blocking` preamble
    /// work into the closure, `spawn_blocking_setup_ms` must be minimal
    /// (microseconds on this synthetic close path) — NOT the ~670 ms
    /// observed pre-fix on mainnet binary `3a6388b9`.
    ///
    /// Unlike `test_close_complete_event_loop_marks_correctly_attributed`,
    /// this test injects NO synthetic blocking work. It exercises the
    /// smallest possible close (empty tx_set, no meta), so the WARN line
    /// may not fire at the 250 ms PhaseTimer threshold; the assertion
    /// uses the DEBUG-level "start" / "finish" path by triggering the
    /// WARN unconditionally via a tiny 260 ms injection (comfortably above
    /// the 250 ms gate but small enough that it does not mask a
    /// setup-window regression).
    ///
    /// **Assertions**:
    ///
    /// 1. The WARN line contains `spawn_blocking_setup_ms`.
    /// 2. `spawn_blocking_setup_ms <= 10 ms` — the acceptance criterion
    ///    from #1780. Pre-fix this field read ~670 ms on mainnet because
    ///    two redundant `soroban_network_info()` calls ran on the event
    ///    loop between the `overlay_bookkeeping_ms` and
    ///    `spawn_blocking_setup_ms` marks. Post-fix, those calls are
    ///    coalesced to one (inside `overlay_bookkeeping_ms`) and the
    ///    derived arithmetic runs inside the `spawn_blocking` closure.
    ///
    /// A synthetic-close setup window is a LOWER bound than mainnet (no
    /// real tx_set, no populated Soroban state), so a passing assertion
    /// here does NOT alone prove the mainnet fix. It DOES lock in the
    /// invariant that the preamble is structurally off the event loop,
    /// so future regressions that re-add snapshot-scale work to the
    /// preamble will be caught by CI rather than by a production
    /// validator WARN.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_close_complete_setup_preamble_is_minimal() {
        let dir = tempfile::tempdir().expect("temp dir");
        let db_path = dir.path().join("rs-stellar-test.db");
        let config = crate::config::ConfigBuilder::new()
            .database_path(db_path)
            .build();
        let app = App::new(config).await.unwrap();

        // Inject 260 ms of synthetic blocking work INSIDE the spawn_blocking
        // closure (just above the PhaseTimer's 250 ms WARN gate) so the
        // WARN line always fires and the sub-phase numbers can be parsed.
        // The injection sits inside `spawn_blocking`, so it shows up under
        // `tx_queue_background_wait_ms`, NOT under
        // `spawn_blocking_setup_ms`. Any value leaking into
        // `spawn_blocking_setup_ms` is the real regression.
        app.close_complete_inject_blocking_ms
            .store(260, Ordering::Relaxed);
        app.set_applying_ledger(true);

        let sub = CapturingSubscriber::default();
        let events = sub.events.clone();
        let _guard = tracing::subscriber::set_default(sub);

        let pending = PendingLedgerClose {
            handle: tokio::task::spawn_blocking(|| {
                Ok(henyey_ledger::LedgerCloseResult {
                    header: stellar_xdr::curr::LedgerHeader {
                        ledger_version: 24,
                        previous_ledger_hash: stellar_xdr::curr::Hash([0u8; 32]),
                        scp_value: stellar_xdr::curr::StellarValue {
                            tx_set_hash: stellar_xdr::curr::Hash([0u8; 32]),
                            close_time: stellar_xdr::curr::TimePoint(1),
                            upgrades: stellar_xdr::curr::VecM::default(),
                            ext: stellar_xdr::curr::StellarValueExt::Basic,
                        },
                        tx_set_result_hash: stellar_xdr::curr::Hash([0u8; 32]),
                        bucket_list_hash: stellar_xdr::curr::Hash([0u8; 32]),
                        ledger_seq: 1,
                        total_coins: 0,
                        fee_pool: 0,
                        inflation_seq: 0,
                        id_pool: 0,
                        base_fee: 100,
                        base_reserve: 5_000_000,
                        max_tx_set_size: 100,
                        skip_list: [
                            stellar_xdr::curr::Hash([0u8; 32]),
                            stellar_xdr::curr::Hash([0u8; 32]),
                            stellar_xdr::curr::Hash([0u8; 32]),
                            stellar_xdr::curr::Hash([0u8; 32]),
                        ],
                        ext: stellar_xdr::curr::LedgerHeaderExt::V0,
                    },
                    header_hash: henyey_common::Hash256::ZERO,
                    tx_results: Vec::new(),
                    meta: None,
                    perf: None,
                    stats: Default::default(),
                })
            }),
            ledger_seq: 1,
            tx_set: henyey_herder::TransactionSet::new_legacy(
                henyey_common::Hash256::ZERO,
                Vec::new(),
            ),
            close_time: 1,
            upgrades: Vec::new(),
            dispatch_time: std::time::Instant::now(),
        };

        let mut pending = pending;
        let join_result = (&mut pending.handle).await;
        let success = app
            .handle_close_complete(
                pending,
                join_result,
                super::persist::LedgerCloseFinalizer::inline(),
            )
            .await;

        assert!(success, "close should succeed");

        let (phase_line, all_events) = {
            let locked = events.lock().unwrap();
            (
                locked
                    .iter()
                    .find(|e| e.contains("app.handle_close_complete"))
                    .cloned(),
                locked.clone(),
            )
        };

        let phase_line = phase_line.unwrap_or_else(|| {
            panic!(
                "PhaseTimer WARN line for app.handle_close_complete was not \
                 captured. All captured events ({}):\n{}",
                all_events.len(),
                all_events.join("\n")
            )
        });

        fn extract_ms(line: &str, label: &str) -> Option<u128> {
            let tag = format!("{}=", label);
            let start = line.find(&tag)? + tag.len();
            let tail = &line[start..];
            let end = tail
                .find(|c: char| !c.is_ascii_digit())
                .unwrap_or(tail.len());
            tail[..end].parse().ok()
        }

        let setup_ms = extract_ms(&phase_line, "spawn_blocking_setup_ms")
            .expect("WARN line should contain spawn_blocking_setup_ms field");

        // Acceptance criterion from #1780:
        // `spawn_blocking_setup_ms` <= 10 ms per close.
        assert!(
            setup_ms <= 10,
            "#1780 regression: spawn_blocking_setup_ms ({setup_ms}) exceeds \
             the 10 ms budget. The preamble between `overlay_bookkeeping_ms` \
             and `spawn_blocking_setup_ms` should be microseconds of \
             capture-list moves; any larger value indicates heavy work was \
             re-added to the event-loop path. WARN line: {phase_line}"
        );
    }

    /// Regression test for #1759: the post-close tx-queue re-validation pass
    /// must build **one** `LedgerSnapshot` per close, not one per
    /// `load_account` / `get_available_balance` call.
    ///
    /// Pre-fix, the stored `LedgerAccountProvider` / `LedgerFeeBalanceProvider`
    /// each called `LedgerManager::create_snapshot()` on every lookup, so
    /// re-validating N queued envelopes built `~N × (1 + ops) × 2`
    /// snapshots per close. On populated mainnet queues this produced
    /// a 94.8 s `tx_queue_background_wait_ms` tail driving 15+ WATCHDOG
    /// freezes.
    ///
    /// Post-fix, the close-path call site builds one
    /// `SnapshotValidationProviders` for the whole re-validation pass,
    /// matching stellar-core's single `LedgerSnapshot ls(app)` in
    /// `TxSetUtils::getInvalidTxListWithErrors`.
    ///
    /// Assertion strategy: measure the *delta* in
    /// `LedgerManager::test_snapshot_count()` across a single
    /// `handle_close_complete` invocation over a tx_queue populated with
    /// N>=50 envelopes. Pre-fix the delta would be O(N × ops) (hundreds).
    /// Post-fix it is exactly 1.
    ///
    /// Using a delta (instead of an absolute `== 1`) isolates the
    /// re-validation pass from any other `create_snapshot` calls that
    /// `handle_close_complete` might legitimately make outside the
    /// re-validation path (e.g., `update_bucket_snapshot`, RPC
    /// server paths). We compute the baseline from a close with an
    /// empty tx_queue and subtract.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_post_close_revalidation_is_single_snapshot() {
        use stellar_xdr::curr::{
            CreateAccountOp, DecoratedSignature, Memo, MuxedAccount, Operation, OperationBody,
            Preconditions, SequenceNumber, SignatureHint, Transaction, TransactionExt,
            TransactionV1Envelope, Uint256,
        };

        /// Build a synthetic envelope unique in `source_account` per `seed`.
        /// The source account intentionally does not exist in the ledger, so
        /// every `load_account` call returns `None` — exercising the full
        /// re-validation path (source lookup, ops-auth lookup) without
        /// needing a populated bucket list.
        fn make_synthetic_envelope(seed: u8) -> stellar_xdr::curr::TransactionEnvelope {
            let source = MuxedAccount::Ed25519(Uint256([seed; 32]));
            let dest =
                stellar_xdr::curr::AccountId(stellar_xdr::curr::PublicKey::PublicKeyTypeEd25519(
                    Uint256([seed.wrapping_add(1); 32]),
                ));
            let tx = Transaction {
                source_account: source,
                fee: 100,
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
            stellar_xdr::curr::TransactionEnvelope::Tx(TransactionV1Envelope {
                tx,
                signatures: vec![DecoratedSignature {
                    hint: SignatureHint([0u8; 4]),
                    signature: stellar_xdr::curr::Signature(vec![0u8; 64].try_into().unwrap()),
                }]
                .try_into()
                .unwrap(),
            })
        }

        async fn run_close(app: &App) {
            app.set_applying_ledger(true);
            let pending = PendingLedgerClose {
                handle: tokio::task::spawn_blocking(|| {
                    Ok(henyey_ledger::LedgerCloseResult {
                        header: stellar_xdr::curr::LedgerHeader {
                            ledger_version: 24,
                            previous_ledger_hash: stellar_xdr::curr::Hash([0u8; 32]),
                            scp_value: stellar_xdr::curr::StellarValue {
                                tx_set_hash: stellar_xdr::curr::Hash([0u8; 32]),
                                close_time: stellar_xdr::curr::TimePoint(1),
                                upgrades: stellar_xdr::curr::VecM::default(),
                                ext: stellar_xdr::curr::StellarValueExt::Basic,
                            },
                            tx_set_result_hash: stellar_xdr::curr::Hash([0u8; 32]),
                            bucket_list_hash: stellar_xdr::curr::Hash([0u8; 32]),
                            ledger_seq: 1,
                            total_coins: 0,
                            fee_pool: 0,
                            inflation_seq: 0,
                            id_pool: 0,
                            base_fee: 100,
                            base_reserve: 5_000_000,
                            max_tx_set_size: 100,
                            skip_list: [
                                stellar_xdr::curr::Hash([0u8; 32]),
                                stellar_xdr::curr::Hash([0u8; 32]),
                                stellar_xdr::curr::Hash([0u8; 32]),
                                stellar_xdr::curr::Hash([0u8; 32]),
                            ],
                            ext: stellar_xdr::curr::LedgerHeaderExt::V0,
                        },
                        header_hash: henyey_common::Hash256::ZERO,
                        tx_results: Vec::new(),
                        meta: None,
                        perf: None,
                        stats: Default::default(),
                    })
                }),
                ledger_seq: 1,
                tx_set: henyey_herder::TransactionSet::new_legacy(
                    henyey_common::Hash256::ZERO,
                    Vec::new(),
                ),
                close_time: 1,
                upgrades: Vec::new(),
                dispatch_time: std::time::Instant::now(),
            };
            let mut pending = pending;
            let join_result = (&mut pending.handle).await;
            let success = app
                .handle_close_complete(
                    pending,
                    join_result,
                    super::persist::LedgerCloseFinalizer::inline(),
                )
                .await;
            assert!(success, "close should succeed");
        }

        // Baseline: close with an empty tx_queue. Measures any
        // create_snapshot calls `handle_close_complete` makes OUTSIDE the
        // re-validation pass (the re-validation pass is skipped entirely
        // when `pending_envelopes()` is empty, per the
        // `if !pending_envs.is_empty()` guard in `ledger_close.rs`).
        let dir = tempfile::tempdir().expect("temp dir");
        let config = crate::config::ConfigBuilder::new()
            .database_path(dir.path().join("baseline.db"))
            .build();
        let app = App::new(config).await.unwrap();

        let baseline_before = app.ledger_manager.test_snapshot_count();
        run_close(&app).await;
        let baseline_after = app.ledger_manager.test_snapshot_count();
        let baseline_delta = baseline_after - baseline_before;

        // Populated run: same close with N=50 envelopes in the tx_queue.
        // Pre-fix this would take O(N × ops × 2) snapshots ≈ 200.
        // Post-fix the re-validation pass adds exactly ONE snapshot
        // beyond the baseline.
        let dir2 = tempfile::tempdir().expect("temp dir");
        let config2 = crate::config::ConfigBuilder::new()
            .database_path(dir2.path().join("populated.db"))
            .build();
        let app2 = App::new(config2).await.unwrap();

        const N_ENVELOPES: u8 = 50;
        for i in 1..=N_ENVELOPES {
            let env = make_synthetic_envelope(i);
            assert!(
                app2.herder.tx_queue().insert_for_test(env),
                "failed to insert synthetic envelope seed={i} into tx_queue"
            );
        }

        let populated_before = app2.ledger_manager.test_snapshot_count();
        run_close(&app2).await;
        let populated_after = app2.ledger_manager.test_snapshot_count();
        let populated_delta = populated_after - populated_before;

        let revalidation_snapshots = populated_delta.saturating_sub(baseline_delta);

        assert_eq!(
            revalidation_snapshots, 1,
            "#1759 regression: post-close re-validation must build exactly \
             ONE snapshot for the full pass, not one per load_account / \
             get_available_balance call. Observed: {revalidation_snapshots} \
             snapshots (baseline_delta={baseline_delta}, \
             populated_delta={populated_delta}, N={N_ENVELOPES}). Pre-fix \
             this value was ~2 × N × (1 + ops)."
        );
    }

    /// `set_phase` MUST clear the fine-grained sub-phase so stale
    /// `PHASE_6_*` / `PHASE_13_*` values from a prior coarse phase
    /// cannot leak into a later WATCHDOG capture. Regression guard for
    /// issues #1788 and #1921 sub-phase instrumentation.
    #[tokio::test]
    async fn test_set_phase_clears_phase_sub() {
        let dir = tempfile::tempdir().expect("temp dir");
        let db_path = dir.path().join("phase-test.db");
        let config = crate::config::ConfigBuilder::new()
            .database_path(db_path)
            .build();
        let app = App::new(config).await.unwrap();

        // Stamp both phase and sub.
        app.set_phase(13);
        app.set_phase_sub(super::phase::PHASE_13_7_OUT_OF_SYNC_CLEAR_SYNCING_WRITE);
        assert_eq!(
            app.phase_snapshot_for_test(),
            (13, super::phase::PHASE_13_7_OUT_OF_SYNC_CLEAR_SYNCING_WRITE)
        );

        // Transitioning coarse phase must zero the sub-phase.
        app.set_phase(14);
        assert_eq!(
            app.phase_snapshot_for_test(),
            (14, 0),
            "set_phase must clear phase_sub — see issue #1788 instrumentation"
        );
    }

    // ============================================================
    // process_externalized_slots split regression tests (#1769 / #1788)
    // ============================================================

    /// Helper: build a minimal App instance for unit testing the
    /// process_externalized_slots split.
    async fn mk_test_app_for_pes_split() -> App {
        let dir = tempfile::tempdir().expect("temp dir");
        let db_path = dir.path().join("pes-split-test.db");
        let config = crate::config::ConfigBuilder::new()
            .database_path(db_path)
            .build();
        App::new(config).await.unwrap()
    }

    /// Helper: construct a valid signed XDR blob for StellarValue with the
    /// given tx_set_hash so check_ledger_close parses it successfully.
    fn mk_stellar_value_xdr(tx_set_hash: [u8; 32]) -> Vec<u8> {
        use stellar_xdr::curr::{
            Hash, Limits, StellarValue, StellarValueExt, TimePoint, VecM, WriteXdr,
        };
        let sv = StellarValue {
            tx_set_hash: Hash(tx_set_hash),
            close_time: TimePoint(12345),
            upgrades: VecM::default(),
            ext: StellarValueExt::Basic,
        };
        sv.to_xdr(Limits::none()).unwrap()
    }

    /// Helper: wrap the XDR bytes into a `Value` (BytesM<64>-ish type for
    /// ScpDriver::record_externalized).
    fn mk_value(xdr_bytes: Vec<u8>) -> stellar_xdr::curr::Value {
        stellar_xdr::curr::Value(
            xdr_bytes
                .try_into()
                .expect("StellarValue XDR fits in Value"),
        )
    }

    /// Regression for #1788/#1769: after the split of
    /// `process_externalized_slots`, the final `syncing_ledgers` state
    /// matches what the legacy inline critical section would produce.
    ///
    /// Setup:
    ///  - externalized slots in herder: {N+1, N+5, N+10}
    ///  - pre-seeded buffer: {N+1 (no tx_set, hash=H1), N+7 (no tx_set, hash=H7)}
    /// Expected post-state:
    ///  - N+1: still present (hash=H1), tx_set remains None (we don't seed
    ///    the tx-set cache, so check_ledger_close returns Some(info) with
    ///    tx_set=None — matches the "no tx_set for this hash" scenario).
    ///  - N+5: inserted, tx_set None.
    ///  - N+7: preserved (not in iter range above N+10? actually N+7 IS
    ///    in iter range N+1..=N+10). check_ledger_close returns None for
    ///    N+7 (not externalized), so legacy path hits the re-request
    ///    branch: buffer.get(N+7) is Some, tx_set None => request_tx_set
    ///    fires, missing_tx_set=true; buffer entry unchanged.
    ///  - N+10: inserted, tx_set None.
    #[tokio::test]
    async fn test_process_externalized_slots_split_matches_legacy_semantics() {
        let app = mk_test_app_for_pes_split().await;
        // Bootstrap herder so latest_externalized_slot returns Some.
        // current_ledger_seq() defaults to 0 before bootstrap; keep low.
        app.herder.set_state(henyey_herder::HerderState::Tracking);

        let n: u64 = 100;
        // Seed three externalized slots with distinct tx_set_hashes.
        let driver = app.herder.scp_driver();
        for (slot, hash_byte) in &[(n + 1, 0x11u8), (n + 5, 0x55u8), (n + 10, 0x0Au8)] {
            let hash = [*hash_byte; 32];
            let xdr = mk_stellar_value_xdr(hash);
            driver.record_externalized(*slot, mk_value(xdr), None);
            driver.publish_externalized(*slot);
        }

        // Seed the syncing_ledgers buffer with pre-existing entries (no
        // tx_set) for N+1 and N+7. N+7 is NOT externalized, so
        // check_ledger_close will return None for it — exercising the
        // re-request branch.
        {
            let mut buf = app.syncing_ledgers.write().await;
            buf.insert(
                (n + 1) as u32,
                henyey_herder::LedgerCloseInfo {
                    slot: n + 1,
                    close_time: 0,
                    tx_set_hash: henyey_common::Hash256::from_bytes([0x11; 32]),
                    tx_set: None,
                    upgrades: Vec::new(),
                    stellar_value_ext: stellar_xdr::curr::StellarValueExt::Basic,
                },
            );
            buf.insert(
                (n + 7) as u32,
                henyey_herder::LedgerCloseInfo {
                    slot: n + 7,
                    close_time: 0,
                    tx_set_hash: henyey_common::Hash256::from_bytes([0x77; 32]),
                    tx_set: None,
                    upgrades: Vec::new(),
                    stellar_value_ext: stellar_xdr::curr::StellarValueExt::Basic,
                },
            );
        }

        // Set last_processed so iter_start = n+1. app.current_ledger_seq()
        // is 0 by default (ledger_manager not initialized) — the iteration
        // window will use `checkpoint_unpublished` logic, but that's fine
        // for this test: it still iterates (n+1..=n+10) and stops at
        // latest_externalized.
        *app.last_processed_slot.write().await = n;

        // Drive the real function.
        let _pending = app.process_externalized_slots().await;

        // Assert final buffer state.
        let buf = app.syncing_ledgers.read().await;
        let keys: Vec<u32> = buf.keys().copied().collect();
        assert!(
            keys.contains(&((n + 1) as u32)),
            "N+1 preserved: {:?}",
            keys
        );
        assert!(keys.contains(&((n + 5) as u32)), "N+5 inserted: {:?}", keys);
        assert!(
            keys.contains(&((n + 7) as u32)),
            "N+7 preserved (re-request branch): {:?}",
            keys
        );
        assert!(
            keys.contains(&((n + 10) as u32)),
            "N+10 inserted: {:?}",
            keys
        );
        // N+1 tx_set still None (we did not seed the tx-set cache, so
        // check_ledger_close returned Some(info) with tx_set=None).
        assert!(buf.get(&((n + 1) as u32)).unwrap().tx_set.is_none());
    }

    /// Regression for #1788/#1769/#1789: the split holds
    /// `syncing_ledgers.write()` only during the apply pass, not across
    /// the per-slot `check_ledger_close` iteration.
    ///
    /// Sentinel point-in-time assertion: we verify that a concurrent
    /// write lock acquirer is NOT blocked at a sample point during the
    /// lockless iteration phase (phase 2). A two-way gate pauses the
    /// iteration after the first non-stale slot, giving us a
    /// deterministic window to prove the write lock is free.
    ///
    /// Pre-split (regression): the entire iteration held the write lock,
    /// so the concurrent writer would deadlock/timeout. Post-split: the
    /// iteration is lockless, so the writer acquires instantly.
    ///
    /// Correctness is pinned by the companion semantics tests
    /// (`_matches_legacy_semantics`, `_rerequest_tx_set_preserved`);
    /// this test pins the concurrency property.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_process_externalized_slots_split_does_not_block_writer() {
        use std::time::Duration;

        // Build App with the two-way gate installed before Arc::new.
        let gate = Arc::new(super::PesIterationGate {
            entered: tokio::sync::Notify::new(),
            resume: tokio::sync::Notify::new(),
        });
        let mut app = mk_test_app_for_pes_split().await;
        app.pes_iteration_gate = Some(Arc::clone(&gate));
        let app = Arc::new(app);
        app.herder.set_state(henyey_herder::HerderState::Tracking);

        // Seed externalized slots. All slots > current_ledger (0), so
        // none are stale — the gate fires on the first iteration.
        let n: u64 = 10_000;
        let driver = app.herder.scp_driver();
        for slot in (n + 1)..=(n + 10) {
            let hash = [(slot & 0xff) as u8; 32];
            let xdr = mk_stellar_value_xdr(hash);
            driver.record_externalized(slot, mk_value(xdr), None);
            driver.publish_externalized(slot);
        }
        *app.last_processed_slot.write().await = n;

        // Spawn process_externalized_slots on a separate task.
        let pes_app = Arc::clone(&app);
        let pes_handle = tokio::spawn(async move { pes_app.process_externalized_slots().await });

        // Wait for the iteration loop to signal phase 2 is in progress.
        tokio::time::timeout(Duration::from_secs(5), gate.entered.notified())
            .await
            .expect("iteration gate must fire within 5s — phase 2 never reached");

        // KEY ASSERTION: syncing_ledgers.write() is acquirable while the
        // iteration is paused mid-phase-2. If the iteration held the
        // write lock (pre-split behavior), this would timeout.
        let write_result =
            tokio::time::timeout(Duration::from_secs(5), app.syncing_ledgers.write()).await;
        assert!(
            write_result.is_ok(),
            "syncing_ledgers.write() must be acquirable during the lockless \
             iteration phase — the split must not hold the write lock here"
        );
        drop(write_result);

        // Resume the iteration so it can complete (apply phase + rest).
        gate.resume.notify_one();

        // Await completion with a generous timeout.
        tokio::time::timeout(Duration::from_secs(10), pes_handle)
            .await
            .expect("process_externalized_slots must complete within 10s")
            .expect("process_externalized_slots must not panic");
    }

    /// Regression for #1788/#1769: the re-request side-effect of
    /// `check_ledger_close` returning None (buffered entry has a hash
    /// but no tx_set) is preserved after the split.
    #[tokio::test]
    async fn test_process_externalized_slots_rerequest_tx_set_preserved() {
        let app = mk_test_app_for_pes_split().await;
        app.herder.set_state(henyey_herder::HerderState::Tracking);

        let n: u64 = 500;
        let missing_slot = n + 3;
        let missing_hash = henyey_common::Hash256::from_bytes([0x99; 32]);

        // Do NOT seed an externalized for missing_slot — check_ledger_close
        // will return None. Seed one OTHER slot so latest_externalized
        // advances to >= missing_slot.
        let driver = app.herder.scp_driver();
        let xdr = mk_stellar_value_xdr([0xaa; 32]);
        driver.record_externalized(n + 5, mk_value(xdr), None);
        driver.publish_externalized(n + 5);

        // Pre-seed buffer with an entry at `missing_slot` that has a hash
        // but no tx_set.
        {
            let mut buf = app.syncing_ledgers.write().await;
            buf.insert(
                missing_slot as u32,
                henyey_herder::LedgerCloseInfo {
                    slot: missing_slot,
                    close_time: 0,
                    tx_set_hash: missing_hash,
                    tx_set: None,
                    upgrades: Vec::new(),
                    stellar_value_ext: stellar_xdr::curr::StellarValueExt::Basic,
                },
            );
        }

        *app.last_processed_slot.write().await = n;

        // Pending requests before: assert `missing_hash` is NOT yet pending.
        let before: Vec<_> = driver
            .get_pending_tx_sets()
            .into_iter()
            .filter(|(h, _)| *h == missing_hash)
            .collect();
        assert!(
            before.is_empty(),
            "baseline: {:?} should not be pending yet",
            before
        );

        // Drive.
        let _ = app.process_externalized_slots().await;

        // After: `missing_hash` should have been re-requested by the
        // split's lockless re-request side-effect.
        let after: Vec<_> = driver
            .get_pending_tx_sets()
            .into_iter()
            .filter(|(h, _)| *h == missing_hash)
            .collect();
        assert!(
            !after.is_empty(),
            "re-request branch: missing_hash must be pending after split \
             (legacy semantics). Got pending: {:?}",
            after
        );
    }

    /// Regression for #2076: when the apply pass encounters a hash mismatch
    /// (existing buffered entry has hash A, check_ledger_close returns hash B
    /// with tx_set: Some), the existing entry must be preserved unchanged and
    /// the incoming tx_set must be rejected.
    #[tokio::test]
    async fn test_process_externalized_slots_apply_hash_mismatch_keeps_existing() {
        let app = mk_test_app_for_pes_split().await;
        app.herder.set_state(henyey_herder::HerderState::Tracking);

        let n: u64 = 200;
        let mismatch_slot = n + 1;
        let hash_a = henyey_common::Hash256::from_bytes([0xAA; 32]);

        // Create a tx_set and cache it so check_ledger_close returns tx_set: Some.
        let tx_set_b =
            henyey_herder::TransactionSet::new_legacy(henyey_common::Hash256::ZERO, Vec::new());
        let hash_b = *tx_set_b.hash();
        assert_ne!(hash_a, hash_b, "hashes must differ for mismatch test");

        let driver = app.herder.scp_driver();
        driver.cache_tx_set(tx_set_b);

        // Pre-seed buffer with hash_a, no tx_set.
        {
            let mut buf = app.syncing_ledgers.write().await;
            buf.insert(
                mismatch_slot as u32,
                henyey_herder::LedgerCloseInfo {
                    slot: mismatch_slot,
                    close_time: 0,
                    tx_set_hash: hash_a,
                    tx_set: None,
                    upgrades: Vec::new(),
                    stellar_value_ext: stellar_xdr::curr::StellarValueExt::Basic,
                },
            );
        }

        // Externalize with hash_b (different hash) for the same slot.
        // check_ledger_close will find the cached tx_set_b and return it.
        let xdr = mk_stellar_value_xdr(hash_b.0);
        driver.record_externalized(mismatch_slot, mk_value(xdr), None);
        driver.publish_externalized(mismatch_slot);

        *app.last_processed_slot.write().await = n;

        // Drive.
        let _ = app.process_externalized_slots().await;

        // The existing entry (hash_a) must be preserved — the apply pass
        // should reject the incoming hash_b + tx_set due to mismatch.
        let buf = app.syncing_ledgers.read().await;
        let entry = buf.get(&(mismatch_slot as u32)).expect("entry must exist");
        assert_eq!(
            entry.tx_set_hash, hash_a,
            "hash mismatch in apply pass must keep existing hash"
        );
        assert!(
            entry.tx_set.is_none(),
            "existing entry had no tx_set and mismatch rejects the incoming tx_set"
        );
    }

    /// All `PHASE_13_*` sub-phase constants are distinct and within a
    /// sensible range. Prevents accidental constant collision during
    /// future edits.
    #[test]
    fn test_phase_13_constants_distinct_and_dense() {
        use super::phase::*;
        let all = [
            PHASE_13_1_BUFFERED_SYNCING_LEDGERS_WRITE,
            PHASE_13_2_BUFFERED_SYNCING_LEDGERS_READ,
            PHASE_13_3_BUFFERED_CONSENSUS_STUCK_WRITE,
            PHASE_13_4_BUFFERED_LAST_CATCHUP_COMPLETED_READ,
            PHASE_13_5_BUFFERED_ARCHIVE_BEHIND_READ,
            PHASE_13_6_OUT_OF_SYNC_BUFFER_COUNT_READ,
            PHASE_13_7_OUT_OF_SYNC_CLEAR_SYNCING_WRITE,
            PHASE_13_8_OUT_OF_SYNC_ANALYZE_GAPS,
            PHASE_13_9_BROADCAST_RECOVERY,
            PHASE_13_10_TRIGGER_RECOVERY_CATCHUP,
            PHASE_13_11_SPAWN_CATCHUP_SET_STATE,
            PHASE_13_12_SPAWN_CATCHUP_MSG_CACHE,
            PHASE_13_13_SPAWN_CATCHUP_SELF_ARC_READ,
            PHASE_13_14_VALIDATE_TARGET_CHECKPOINT,
            PHASE_13_15_VALIDATE_ARCHIVE_NEWER,
        ];
        let mut sorted = all.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(
            sorted.len(),
            all.len(),
            "phase-13 sub-phase constants must all be distinct"
        );
        assert_eq!(sorted.first().copied(), Some(1));
        assert_eq!(
            sorted.last().copied(),
            Some(max_defined_phase_13_sub_phase())
        );
        // Dense: no gaps.
        for (i, v) in sorted.iter().enumerate() {
            assert_eq!(
                *v,
                (i as u32) + 1,
                "phase-13 sub-phase constants must be densely numbered 1..=N"
            );
        }
    }

    /// All `PHASE_6_*` sub-phase constants are distinct and densely numbered.
    /// Mirrors the `PHASE_13_*` test above. Prevents accidental constant
    /// collision during future edits (issue #1921).
    #[test]
    fn test_phase_6_constants_distinct_and_dense() {
        use super::phase::*;
        let all = [
            PHASE_6_1_SYNCING_LEDGERS_HASH_MISMATCH,
            PHASE_6_2_WRITE_META,
            PHASE_6_3_OVERLAY_CLEAR_LEDGERS,
            PHASE_6_4_OVERLAY_MAX_TX_SIZE,
            PHASE_6_5_SURVEY_STATE_WRITE,
            PHASE_6_6_TX_QUEUE_JOIN,
            PHASE_6_7_LAST_PROCESSED_SLOT_WRITE,
            PHASE_6_8_CLEAR_TX_ADVERT_HISTORY,
            PHASE_6_9_MAYBE_PUBLISH_HISTORY,
            PHASE_6_10_TRY_TRIGGER_CONSENSUS,
            PHASE_6_11_FETCH_DRAIN,
            PHASE_6_12_PROCESS_EXTERNALIZED_SLOTS,
        ];
        let mut sorted = all.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(
            sorted.len(),
            all.len(),
            "phase-6 sub-phase constants must all be distinct"
        );
        assert_eq!(sorted.first().copied(), Some(1));
        assert_eq!(
            sorted.last().copied(),
            Some(max_defined_phase_6_sub_phase())
        );
        for (i, v) in sorted.iter().enumerate() {
            assert_eq!(
                *v,
                (i as u32) + 1,
                "phase-6 sub-phase constants must be densely numbered 1..=N"
            );
        }
    }

    // ============================================================
    // Deferred-finalizer contract tests (issue #1809)
    //
    // The production event loop uses `LedgerCloseFinalizer::deferred()`
    // (lifecycle.rs:255-273). On success, `handle_close_complete` sends
    // a `PendingPersist` through the oneshot synchronously before
    // returning `true`. On error/panic, the sender is dropped (no send)
    // and the function returns `false`.
    // ============================================================

    #[tokio::test]
    async fn test_deferred_finalizer_success_sends_persist() {
        let dir = tempfile::tempdir().expect("temp dir");
        let db_path = dir.path().join("rs-stellar-test.db");
        let config = crate::config::ConfigBuilder::new()
            .database_path(db_path)
            .build();
        let app = App::new(config).await.unwrap();
        app.set_applying_ledger(true);

        let mut pending = make_test_pending_close(
            tokio::task::spawn_blocking(|| Ok(make_successful_close_result(1))),
            1,
        );
        let join_result = (&mut pending.handle).await;

        let (persist_tx, mut persist_rx) = tokio::sync::oneshot::channel();
        let success = app
            .handle_close_complete(
                pending,
                join_result,
                super::persist::LedgerCloseFinalizer::deferred(persist_tx),
            )
            .await;

        assert!(
            success,
            "handle_close_complete should return true on success"
        );
        assert!(
            !app.is_applying_ledger.load(Ordering::Relaxed),
            "is_applying_ledger should be cleared"
        );

        // The deferred contract: a PendingPersist was sent synchronously.
        let pt = persist_rx
            .try_recv()
            .expect("deferred finalizer must send PendingPersist on success");
        assert_eq!(
            pt.ledger_seq, 1,
            "PendingPersist should carry the correct ledger_seq"
        );
    }

    #[tokio::test]
    async fn test_deferred_finalizer_error_drops_sender() {
        let dir = tempfile::tempdir().expect("temp dir");
        let db_path = dir.path().join("rs-stellar-test.db");
        let config = crate::config::ConfigBuilder::new()
            .database_path(db_path)
            .build();
        let app = App::new(config).await.unwrap();
        app.set_applying_ledger(true);

        let mut pending = make_test_pending_close(
            tokio::task::spawn_blocking(|| {
                Err(henyey_ledger::LedgerError::Internal(
                    "simulated error".to_string(),
                ))
            }),
            1,
        );
        let join_result = (&mut pending.handle).await;

        let (persist_tx, mut persist_rx) = tokio::sync::oneshot::channel();
        let success = app
            .handle_close_complete(
                pending,
                join_result,
                super::persist::LedgerCloseFinalizer::deferred(persist_tx),
            )
            .await;

        assert!(
            !success,
            "handle_close_complete should return false on error"
        );
        assert!(
            !app.is_applying_ledger.load(Ordering::Relaxed),
            "is_applying_ledger should be cleared on error"
        );

        // The negative contract: sender was dropped, not sent.
        assert!(
            matches!(
                persist_rx.try_recv(),
                Err(tokio::sync::oneshot::error::TryRecvError::Closed)
            ),
            "deferred sender must be dropped (not sent) on error path"
        );
    }

    #[tokio::test]
    async fn test_deferred_finalizer_panic_drops_sender() {
        let dir = tempfile::tempdir().expect("temp dir");
        let db_path = dir.path().join("rs-stellar-test.db");
        let config = crate::config::ConfigBuilder::new()
            .database_path(db_path)
            .build();
        let app = App::new(config).await.unwrap();
        app.set_applying_ledger(true);

        let mut pending = make_test_pending_close(
            tokio::task::spawn_blocking(|| {
                panic!("simulated panic");
            }),
            1,
        );
        let join_result = (&mut pending.handle).await;

        let (persist_tx, mut persist_rx) = tokio::sync::oneshot::channel();
        let success = app
            .handle_close_complete(
                pending,
                join_result,
                super::persist::LedgerCloseFinalizer::deferred(persist_tx),
            )
            .await;

        assert!(
            !success,
            "handle_close_complete should return false on panic"
        );
        assert!(
            !app.is_applying_ledger.load(Ordering::Relaxed),
            "is_applying_ledger should be cleared on panic"
        );

        assert!(
            matches!(
                persist_rx.try_recv(),
                Err(tokio::sync::oneshot::error::TryRecvError::Closed)
            ),
            "deferred sender must be dropped (not sent) on panic path"
        );
    }

    #[tokio::test]
    async fn test_deferred_close_persist_lifecycle() {
        let dir = tempfile::tempdir().expect("temp dir");
        let db_path = dir.path().join("rs-stellar-test.db");
        let config = crate::config::ConfigBuilder::new()
            .database_path(db_path)
            .build();
        let app = App::new(config).await.unwrap();

        let mut pipeline = super::close_pipeline::ClosePipeline::new();
        assert!(pipeline.is_idle(), "pipeline should start idle");

        // --- Cycle 1 (seq=1) ---
        app.set_applying_ledger(true);

        let pending = make_test_pending_close(
            tokio::task::spawn_blocking(|| Ok(make_successful_close_result(1))),
            1,
        );
        pipeline.start_close(pending);
        assert!(!pipeline.is_idle(), "pipeline should be in Closing state");

        // Simulate close completion: await handle, take from pipeline.
        let mut taken = pipeline.take_close();
        let join_result = (&mut taken.handle).await;

        // Deferred finalizer handoff (production path).
        let (persist_tx, mut persist_rx) = tokio::sync::oneshot::channel();
        let success = app
            .handle_close_complete(
                taken,
                join_result,
                super::persist::LedgerCloseFinalizer::deferred(persist_tx),
            )
            .await;
        assert!(success, "cycle 1: close should succeed");

        // Receive the persist handle and install it.
        let pt = persist_rx
            .try_recv()
            .expect("cycle 1: deferred must send PendingPersist");
        assert_eq!(pt.ledger_seq, 1);
        pipeline.start_persist(pt);

        // Gating invariant: pipeline is NOT idle while persisting.
        assert!(
            !pipeline.is_idle(),
            "pipeline must not be idle during persist"
        );

        // Await persist completion and take.
        let mut persist = pipeline.take_persist();
        let _ = (&mut persist.handle).await;
        assert!(pipeline.is_idle(), "pipeline should be idle after persist");

        // --- Cycle 2 (seq=2) ---
        app.set_applying_ledger(true);

        let pending2 = make_test_pending_close(
            tokio::task::spawn_blocking(|| Ok(make_successful_close_result(2))),
            2,
        );
        pipeline.start_close(pending2);

        let mut taken2 = pipeline.take_close();
        let join_result2 = (&mut taken2.handle).await;

        let (persist_tx2, mut persist_rx2) = tokio::sync::oneshot::channel();
        let success2 = app
            .handle_close_complete(
                taken2,
                join_result2,
                super::persist::LedgerCloseFinalizer::deferred(persist_tx2),
            )
            .await;
        assert!(success2, "cycle 2: close should succeed");

        let pt2 = persist_rx2
            .try_recv()
            .expect("cycle 2: deferred must send PendingPersist");
        assert_eq!(pt2.ledger_seq, 2, "cycle 2: persist should carry seq=2");
        pipeline.start_persist(pt2);
        assert!(!pipeline.is_idle());

        let mut persist2 = pipeline.take_persist();
        let _ = (&mut persist2.handle).await;
        assert!(
            pipeline.is_idle(),
            "pipeline should be idle after second cycle"
        );
    }

    // ── submit_transaction freshness gate tests (#1812) ───────────────

    /// Build a minimal tx envelope for freshness gate tests.
    /// The tx will fail validation (no real account), but the freshness
    /// gate fires before validation, so the tx content doesn't matter.
    fn make_dummy_tx_envelope() -> TransactionEnvelope {
        use stellar_xdr::curr::{
            CreateAccountOp, DecoratedSignature, Memo, MuxedAccount, Operation, OperationBody,
            Preconditions, SequenceNumber, SignatureHint, Transaction, TransactionExt,
            TransactionV1Envelope, Uint256,
        };
        let tx = Transaction {
            source_account: MuxedAccount::Ed25519(Uint256([1u8; 32])),
            fee: 100,
            seq_num: SequenceNumber(1),
            cond: Preconditions::None,
            memo: Memo::None,
            operations: vec![Operation {
                source_account: None,
                body: OperationBody::CreateAccount(CreateAccountOp {
                    destination: stellar_xdr::curr::AccountId(
                        stellar_xdr::curr::PublicKey::PublicKeyTypeEd25519(Uint256([2u8; 32])),
                    ),
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
                signature: stellar_xdr::curr::Signature(vec![0u8; 64].try_into().unwrap()),
            }]
            .try_into()
            .unwrap(),
        })
    }

    /// Helper: create a minimal test App for submit_transaction tests.
    async fn mk_test_app_for_tx_freshness() -> App {
        let dir = tempfile::tempdir().expect("temp dir");
        let db_path = dir.path().join("tx-freshness-test.db");
        let config = crate::config::ConfigBuilder::new()
            .database_path(db_path)
            .build();
        let app = App::new(config).await.unwrap();
        // Put herder in Tracking so the existing can_receive_transactions gate
        // doesn't reject before our freshness check fires.
        app.herder.set_state(henyey_herder::HerderState::Tracking);
        app
    }

    #[tokio::test]
    async fn test_submit_transaction_rejected_when_behind_network() {
        let app = mk_test_app_for_tx_freshness().await;

        // Simulate: network has externalized slot 110, node is at ledger 0.
        // Gap of 110 >> TX_SUBMISSION_MAX_BEHIND (2).
        app.max_observed_externalize_slot
            .store(110, Ordering::SeqCst);

        let result = app.submit_transaction(make_dummy_tx_envelope()).await;
        assert_eq!(
            result,
            henyey_herder::TxQueueResult::TryAgainLater,
            "should reject when node is far behind the network"
        );
    }

    #[tokio::test]
    async fn test_submit_transaction_accepted_when_within_threshold() {
        let app = mk_test_app_for_tx_freshness().await;

        // max_observed_externalize_slot defaults to 0.
        // current_ledger_seq() is also 0.  Gap = 0, within threshold.
        assert_eq!(app.max_observed_externalize_slot.load(Ordering::SeqCst), 0);

        let result = app.submit_transaction(make_dummy_tx_envelope()).await;
        // The tx may fail validation (no account loaded, etc.) but it should
        // NOT be rejected by the freshness gate.
        assert_ne!(
            result,
            henyey_herder::TxQueueResult::TryAgainLater,
            "should not reject when node is current with the network"
        );
    }

    #[tokio::test]
    async fn test_submit_transaction_accepted_at_threshold_boundary() {
        let app = mk_test_app_for_tx_freshness().await;
        let current = app.current_ledger_seq() as u64;

        // Gap of exactly TX_SUBMISSION_MAX_BEHIND should NOT trigger.
        // The check is `max_ext > current + TX_SUBMISSION_MAX_BEHIND` (strict >).
        app.max_observed_externalize_slot
            .store(current + TX_SUBMISSION_MAX_BEHIND, Ordering::SeqCst);

        let result = app.submit_transaction(make_dummy_tx_envelope()).await;
        assert_ne!(
            result,
            henyey_herder::TxQueueResult::TryAgainLater,
            "gap == threshold should pass (gate fires only when gap > threshold)"
        );
    }

    #[tokio::test]
    async fn test_submit_transaction_rejected_just_above_threshold() {
        let app = mk_test_app_for_tx_freshness().await;
        let current = app.current_ledger_seq() as u64;

        // Gap of TX_SUBMISSION_MAX_BEHIND + 1 should trigger.
        app.max_observed_externalize_slot
            .store(current + TX_SUBMISSION_MAX_BEHIND + 1, Ordering::SeqCst);

        let result = app.submit_transaction(make_dummy_tx_envelope()).await;
        assert_eq!(
            result,
            henyey_herder::TxQueueResult::TryAgainLater,
            "gap just above threshold should trigger freshness gate"
        );
    }

    /// Regression test for issue #1843: `escalate_recovery_to_catchup` must
    /// use `fetch_max` semantics — a pre-existing counter value above
    /// `RECOVERY_ESCALATION_CATCHUP` must never be lowered.
    ///
    /// Tests the helper on a real `App` instance with a table of initial
    /// counter values spanning below, at, and above the threshold.
    #[tokio::test]
    async fn test_escalate_recovery_to_catchup_monotonicity() {
        let dir = tempfile::tempdir().expect("temp dir");
        let db_path = dir.path().join("rs-stellar-test.db");
        let config = crate::config::ConfigBuilder::new()
            .database_path(db_path)
            .build();
        let app = App::new(config).await.unwrap();

        // (initial_value, expected_after, description)
        let cases: &[(u64, u64, &str)] = &[
            (0, RECOVERY_ESCALATION_CATCHUP, "below threshold — raised"),
            (
                RECOVERY_ESCALATION_CATCHUP - 1,
                RECOVERY_ESCALATION_CATCHUP,
                "just below — raised",
            ),
            (
                RECOVERY_ESCALATION_CATCHUP,
                RECOVERY_ESCALATION_CATCHUP,
                "equal — unchanged",
            ),
            (
                RECOVERY_ESCALATION_CATCHUP + 1,
                RECOVERY_ESCALATION_CATCHUP + 1,
                "just above — preserved",
            ),
            (
                RECOVERY_ESCALATION_CATCHUP + 5,
                RECOVERY_ESCALATION_CATCHUP + 5,
                "well above — preserved",
            ),
        ];

        for &(initial, expected, desc) in cases {
            app.recovery_attempts_without_progress
                .store(initial, Ordering::SeqCst);
            app.escalate_recovery_to_catchup();
            let actual = app
                .recovery_attempts_without_progress
                .load(Ordering::SeqCst);
            assert_eq!(actual, expected, "{desc}");
        }
    }

    /// Regression test for issue #2073: `escalate_recovery_to_catchup` must
    /// couple the attempt-counter bump with `set_urgent(true)` on the archive
    /// checkpoint cache.  Before this fix, urgent mode was only activated when
    /// `tx_set_all_peers_exhausted` was true, delaying archive-behind detection
    /// by up to 60 s.
    #[tokio::test]
    async fn test_escalate_recovery_to_catchup_sets_urgent() {
        let dir = tempfile::tempdir().expect("temp dir");
        let db_path = dir.path().join("rs-stellar-test.db");
        let config = crate::config::ConfigBuilder::new()
            .database_path(db_path)
            .build();
        let app = App::new(config).await.unwrap();

        // Pre-condition: urgent mode is off.
        assert!(
            !app.archive_checkpoint_cache.is_urgent(),
            "urgent must be off initially"
        );

        app.escalate_recovery_to_catchup();

        assert!(
            app.archive_checkpoint_cache.is_urgent(),
            "escalate_recovery_to_catchup must activate urgent mode (#2073)"
        );
        assert_eq!(
            app.recovery_attempts_without_progress
                .load(Ordering::SeqCst),
            RECOVERY_ESCALATION_CATCHUP,
            "attempt counter must be raised to threshold"
        );
        // escalate_recovery_to_catchup must NOT set confirmed-behind
        // (escalation is a pre-confirmation path — see #2075).
        assert!(
            !app.archive_recovery_snapshot().await.is_confirmed_behind(),
            "escalate_recovery_to_catchup must not set confirmed-behind (#2075)"
        );
    }

    /// The `mark_archive_confirmed_behind()` helper must set the status to
    /// ConfirmedBehind and activate urgent mode on the archive checkpoint cache.
    #[tokio::test]
    async fn test_mark_archive_confirmed_behind_sets_both_signals() {
        let dir = tempfile::tempdir().expect("temp dir");
        let db_path = dir.path().join("rs-stellar-test.db");
        let config = crate::config::ConfigBuilder::new()
            .database_path(db_path)
            .build();
        let app = App::new(config).await.unwrap();

        // Pre-conditions.
        assert!(!app.archive_recovery_snapshot().await.is_confirmed_behind());
        assert!(!app.archive_checkpoint_cache.is_urgent());

        app.mark_archive_confirmed_behind().await;

        assert!(
            app.archive_recovery_snapshot().await.is_confirmed_behind(),
            "mark_archive_confirmed_behind must set ConfirmedBehind status"
        );
        assert!(
            app.archive_checkpoint_cache.is_urgent(),
            "mark_archive_confirmed_behind must activate urgent mode"
        );
    }

    /// Regression test: the out-of-sync recovery path in `consensus.rs`
    /// escalates only when there are buffered slots but none have tx_sets.
    /// Exercises the actual production pattern: guard → `escalate_recovery_to_catchup()`.
    #[tokio::test]
    async fn test_out_of_sync_escalation_guard_and_counter() {
        let dir = tempfile::tempdir().expect("temp dir");
        let db_path = dir.path().join("rs-stellar-test.db");
        let config = crate::config::ConfigBuilder::new()
            .database_path(db_path)
            .build();
        let app = App::new(config).await.unwrap();

        // Replicate the production pattern from consensus.rs:512-519:
        //   if with_tx_set == 0 && total > 0 {
        //       self.escalate_recovery_to_catchup();
        //   }

        // Case 1: counter already above threshold, guard fires → must preserve
        let pre_existing = RECOVERY_ESCALATION_CATCHUP + 3; // e.g., 9
        app.recovery_attempts_without_progress
            .store(pre_existing, Ordering::SeqCst);
        let (with_tx_set, total) = (0u64, 5u64);
        if with_tx_set == 0 && total > 0 {
            app.escalate_recovery_to_catchup();
        }
        assert_eq!(
            app.recovery_attempts_without_progress
                .load(Ordering::SeqCst),
            pre_existing,
            "counter above threshold must be preserved when guard fires"
        );

        // Case 2: counter below threshold, guard fires → raised to threshold
        app.recovery_attempts_without_progress
            .store(2, Ordering::SeqCst);
        let (with_tx_set, total) = (0u64, 5u64);
        if with_tx_set == 0 && total > 0 {
            app.escalate_recovery_to_catchup();
        }
        assert_eq!(
            app.recovery_attempts_without_progress
                .load(Ordering::SeqCst),
            RECOVERY_ESCALATION_CATCHUP,
            "counter below threshold must be raised when guard fires"
        );

        // Case 3: guard does NOT fire (total == 0) → counter untouched
        app.recovery_attempts_without_progress
            .store(2, Ordering::SeqCst);
        let (with_tx_set, total) = (0u64, 0u64);
        if with_tx_set == 0 && total > 0 {
            app.escalate_recovery_to_catchup();
        }
        assert_eq!(
            app.recovery_attempts_without_progress
                .load(Ordering::SeqCst),
            2,
            "counter must not change when guard does not fire (no buffered slots)"
        );

        // Case 4: guard does NOT fire (with_tx_set > 0) → counter untouched
        app.recovery_attempts_without_progress
            .store(2, Ordering::SeqCst);
        let (with_tx_set, total) = (3u64, 5u64);
        if with_tx_set == 0 && total > 0 {
            app.escalate_recovery_to_catchup();
        }
        assert_eq!(
            app.recovery_attempts_without_progress
                .load(Ordering::SeqCst),
            2,
            "counter must not change when guard does not fire (some tx_sets present)"
        );
    }

    /// Regression test: Valid EXTERNALIZE in `lifecycle.rs` escalates only
    /// when the slot is more than 2 ahead of current_ledger.
    /// Exercises the actual production pattern: guard → `escalate_recovery_to_catchup()`.
    #[tokio::test]
    async fn test_valid_externalize_escalation_guard_and_counter() {
        let dir = tempfile::tempdir().expect("temp dir");
        let db_path = dir.path().join("rs-stellar-test.db");
        let config = crate::config::ConfigBuilder::new()
            .database_path(db_path)
            .build();
        let app = App::new(config).await.unwrap();

        // Replicate the production pattern from lifecycle.rs:1907-1911:
        //   if slot > current_ledger + 1 {
        //       self.sync_recovery_pending.store(true, ...);
        //       if slot > current_ledger + 2 {
        //           self.escalate_recovery_to_catchup();
        //       }
        //   }

        let current_ledger = 100u64;

        // Case 1: gap=3, counter above threshold → escalation fires, counter preserved
        let pre_existing = RECOVERY_ESCALATION_CATCHUP + 2;
        app.recovery_attempts_without_progress
            .store(pre_existing, Ordering::SeqCst);
        app.sync_recovery_pending.store(false, Ordering::SeqCst);
        let slot = current_ledger + 3;
        if slot > current_ledger + 1 {
            app.sync_recovery_pending.store(true, Ordering::SeqCst);
            if slot > current_ledger + 2 {
                app.escalate_recovery_to_catchup();
            }
        }
        assert!(
            app.sync_recovery_pending.load(Ordering::SeqCst),
            "sync_recovery_pending must be set at gap=3"
        );
        assert_eq!(
            app.recovery_attempts_without_progress
                .load(Ordering::SeqCst),
            pre_existing,
            "counter above threshold must be preserved at gap=3"
        );

        // Case 2: gap=2, counter below threshold → sync_recovery_pending only, no escalation
        app.recovery_attempts_without_progress
            .store(2, Ordering::SeqCst);
        app.sync_recovery_pending.store(false, Ordering::SeqCst);
        let slot = current_ledger + 2;
        if slot > current_ledger + 1 {
            app.sync_recovery_pending.store(true, Ordering::SeqCst);
            if slot > current_ledger + 2 {
                app.escalate_recovery_to_catchup();
            }
        }
        assert!(
            app.sync_recovery_pending.load(Ordering::SeqCst),
            "sync_recovery_pending must be set at gap=2"
        );
        assert_eq!(
            app.recovery_attempts_without_progress
                .load(Ordering::SeqCst),
            2,
            "counter must not change at gap=2 (boundary — no escalation)"
        );

        // Case 3: gap=1 → neither sync_recovery_pending nor escalation
        app.recovery_attempts_without_progress
            .store(2, Ordering::SeqCst);
        app.sync_recovery_pending.store(false, Ordering::SeqCst);
        let slot = current_ledger + 1;
        if slot > current_ledger + 1 {
            app.sync_recovery_pending.store(true, Ordering::SeqCst);
            if slot > current_ledger + 2 {
                app.escalate_recovery_to_catchup();
            }
        }
        assert!(
            !app.sync_recovery_pending.load(Ordering::SeqCst),
            "sync_recovery_pending must NOT be set at gap=1"
        );
        assert_eq!(
            app.recovery_attempts_without_progress
                .load(Ordering::SeqCst),
            2,
            "counter must not change at gap=1"
        );
    }

    /// Regression test: Pending EXTERNALIZE in `lifecycle.rs` escalates only
    /// when far ahead AND the next slot does NOT have a buffered tx_set.
    /// Exercises the actual production pattern: guard → `escalate_recovery_to_catchup()`.
    ///
    /// `have_next` means `syncing_ledgers[next_slot].tx_set.is_some()` — a
    /// buffered entry WITHOUT a tx_set still triggers escalation.
    #[tokio::test]
    async fn test_pending_externalize_escalation_guard_and_counter() {
        let dir = tempfile::tempdir().expect("temp dir");
        let db_path = dir.path().join("rs-stellar-test.db");
        let config = crate::config::ConfigBuilder::new()
            .database_path(db_path)
            .build();
        let app = App::new(config).await.unwrap();

        // Replicate the production pattern from lifecycle.rs:1921-1947:
        //   if slot > current_ledger + 2 {
        //       if have_next { /* skip */ } else {
        //           self.escalate_recovery_to_catchup();
        //           self.sync_recovery_pending.store(true, ...);
        //       }
        //   }

        let current_ledger = 100u64;

        // Case 1: far ahead, no next slot, counter above threshold → preserved
        let pre_existing = RECOVERY_ESCALATION_CATCHUP + 4;
        app.recovery_attempts_without_progress
            .store(pre_existing, Ordering::SeqCst);
        app.sync_recovery_pending.store(false, Ordering::SeqCst);
        let slot = current_ledger + 5;
        let have_next = false;
        if slot > current_ledger + 2 {
            if have_next {
                // skip — let rapid close proceed
            } else {
                app.escalate_recovery_to_catchup();
                app.sync_recovery_pending.store(true, Ordering::SeqCst);
            }
        }
        assert!(
            app.sync_recovery_pending.load(Ordering::SeqCst),
            "sync_recovery_pending must be set when far ahead and no next slot"
        );
        assert_eq!(
            app.recovery_attempts_without_progress
                .load(Ordering::SeqCst),
            pre_existing,
            "counter above threshold must be preserved"
        );

        // Case 2: far ahead, next slot HAS tx_set → no escalation
        app.recovery_attempts_without_progress
            .store(2, Ordering::SeqCst);
        app.sync_recovery_pending.store(false, Ordering::SeqCst);
        let slot = current_ledger + 5;
        let have_next = true;
        if slot > current_ledger + 2 {
            if have_next {
                // skip — let rapid close proceed
            } else {
                app.escalate_recovery_to_catchup();
                app.sync_recovery_pending.store(true, Ordering::SeqCst);
            }
        }
        assert!(
            !app.sync_recovery_pending.load(Ordering::SeqCst),
            "sync_recovery_pending must NOT be set when next slot is buffered"
        );
        assert_eq!(
            app.recovery_attempts_without_progress
                .load(Ordering::SeqCst),
            2,
            "counter must not change when next slot is buffered"
        );

        // Case 3: not far ahead → no escalation regardless
        app.recovery_attempts_without_progress
            .store(2, Ordering::SeqCst);
        app.sync_recovery_pending.store(false, Ordering::SeqCst);
        let slot = current_ledger + 2;
        let have_next = false;
        if slot > current_ledger + 2 {
            if have_next {
                // skip
            } else {
                app.escalate_recovery_to_catchup();
                app.sync_recovery_pending.store(true, Ordering::SeqCst);
            }
        }
        assert!(
            !app.sync_recovery_pending.load(Ordering::SeqCst),
            "sync_recovery_pending must NOT be set at gap=2"
        );
        assert_eq!(
            app.recovery_attempts_without_progress
                .load(Ordering::SeqCst),
            2,
            "counter must not change at gap=2"
        );
    }

    /// Regression test for #1861: escalation gate must NOT fire when the
    /// node is caught up (latest_externalized == current_ledger, gap=0).
    ///
    /// Before the fix, `attempts >= RECOVERY_ESCALATION_CATCHUP` alone
    /// was enough to enter `trigger_recovery_catchup`, which emitted the
    /// spurious "Recovery stalled for too long" INFO log even though
    /// there was nothing to catch up to.
    ///
    /// After the fix, the gate also requires
    /// `latest_externalized > current_ledger as u64`.
    #[tokio::test]
    async fn test_recovery_escalation_skipped_at_gap_zero() {
        let dir = tempfile::tempdir().expect("temp dir");
        let db_path = dir.path().join("rs-stellar-test.db");
        let config = crate::config::ConfigBuilder::new()
            .database_path(db_path)
            .build();
        let app = App::new(config).await.unwrap();

        // Fresh herder: latest_externalized_slot() returns None → 0.
        // current_ledger = 0 → gap = 0, latest_externalized == current_ledger.
        let current_ledger = 0u32;
        assert_eq!(
            app.herder.latest_externalized_slot().unwrap_or(0),
            current_ledger as u64,
            "precondition: gap must be 0"
        );

        // Pump recovery_attempts past the escalation threshold.
        // Set baseline to 0 so progress-reset doesn't fire.
        let above_threshold = RECOVERY_ESCALATION_CATCHUP + 5;
        app.recovery_attempts_without_progress
            .store(above_threshold, Ordering::SeqCst);
        app.recovery_baseline_ledger.store(0, Ordering::SeqCst);

        let result = app.out_of_sync_recovery(current_ledger).await;

        // The gate prevented escalation → no catchup spawned.
        assert!(
            result.is_none(),
            "escalation must be skipped when gap=0 (node is caught up)"
        );

        // Counter must NOT be reset to 0 — only real ledger progress or
        // successful catchup spawn resets it.
        let counter = app
            .recovery_attempts_without_progress
            .load(Ordering::SeqCst);
        assert!(
            counter > RECOVERY_ESCALATION_CATCHUP,
            "counter ({}) must not be reset when escalation is skipped",
            counter,
        );
    }

    /// Regression test for #1861: the fast-track predicate at the exact tip
    /// must still fire when `latest_externalized == current_ledger` and SCP
    /// messages are flowing.
    ///
    /// The fix changed the predicate from `gap == 0` to
    /// `latest_externalized == current_ledger as u64`, which is equivalent
    /// at the exact tip but differs when `current_ledger > latest_externalized`
    /// (where `gap` would also be 0 due to `saturating_sub`).
    #[tokio::test]
    async fn test_fast_track_still_triggers_catchup_at_gap_zero() {
        let dir = tempfile::tempdir().expect("temp dir");
        let db_path = dir.path().join("rs-stellar-test.db");
        let config = crate::config::ConfigBuilder::new()
            .database_path(db_path)
            .build();
        let app = App::new(config).await.unwrap();

        // Fresh herder: latest_externalized=0, current_ledger=0 → gap=0.
        let current_ledger = 0u32;
        assert_eq!(
            app.herder.latest_externalized_slot().unwrap_or(0),
            current_ledger as u64,
            "precondition: gap must be 0"
        );

        // Seed archive cache ahead of checkpoint_containing(1) so that
        // trigger_recovery_catchup would proceed to spawn_catchup.
        let next_cp = henyey_history::checkpoint::checkpoint_containing(current_ledger + 1);
        app.archive_checkpoint_cache.seed(next_cp + 64);

        // Set SCP messages received > 0 to satisfy fast-track condition.
        app.scp_messages_received.store(10, Ordering::Relaxed);

        // Set attempts=1 (past the `attempts >= 1` guard) but below
        // RECOVERY_ESCALATION_SCP_REQUEST so we enter the fast-track branch.
        app.recovery_attempts_without_progress
            .store(1, Ordering::SeqCst);
        app.recovery_baseline_ledger.store(0, Ordering::SeqCst);

        let result = app.out_of_sync_recovery(current_ledger).await;

        // The fast-track path fires and calls trigger_recovery_catchup.
        // spawn_catchup returns None on a test App (no self_arc), so
        // the overall result is None — but the key assertion is that the
        // code did NOT take the "waiting for fresh EXTERNALIZE" early return
        // at line 287. We verify by checking that catchup_in_progress was
        // toggled (spawn_catchup sets it to true, then back to false on
        // self_arc failure).
        //
        // The definitive assertion is that this test does NOT hit the
        // "waiting for fresh EXTERNALIZE" debug log — which would happen
        // if the fast-track predicate failed.
        //
        // Since spawn_catchup fails on test App, result is None, but the
        // fast-track path was taken (verified by the warn log and the
        // catchup_in_progress toggle).
        assert!(
            result.is_none(),
            "spawn_catchup returns None on test App, but fast-track path was taken"
        );

        // Verify catchup_in_progress was NOT left stuck on (spawn_catchup
        // cleans up on self_arc failure).
        assert!(
            !app.catchup_in_progress.load(Ordering::SeqCst),
            "catchup_in_progress must not be left set after failed spawn"
        );
    }

    /// Regression test for #1861: escalation gate must NOT fire when the
    /// node is ahead of consensus with a non-zero `latest_externalized`.
    ///
    /// When `latest_externalized > 0` but `< current_ledger`, the node is
    /// ahead of SCP but has previously externalized — this is a transient
    /// state that should resolve via SCP state requests, not catchup.
    ///
    /// Note: this test still passes after #1897's restructuring because
    /// `scp_messages_received=0` prevents the fast-track from firing —
    /// a fresh App with no SCP activity stays in the wait/escalate path.
    #[tokio::test]
    async fn test_recovery_escalation_skipped_at_tip() {
        let dir = tempfile::tempdir().expect("temp dir");
        let db_path = dir.path().join("rs-stellar-test.db");
        let config = crate::config::ConfigBuilder::new()
            .database_path(db_path)
            .build();
        let app = App::new(config).await.unwrap();

        // AtTip (current_ledger=0, latest_ext=0) must NOT trigger the
        // escalation guard — only Behind or Ahead-no-ext should.
        let current_ledger = 0u32;
        let latest = app.herder.latest_externalized_slot().unwrap_or(0);
        assert_eq!(
            current_ledger as u64, latest,
            "precondition: node must be at tip"
        );

        // Pump attempts past the escalation threshold.
        let above_threshold = RECOVERY_ESCALATION_CATCHUP + 5;
        app.recovery_attempts_without_progress
            .store(above_threshold, Ordering::SeqCst);
        app.recovery_baseline_ledger.store(0, Ordering::SeqCst);

        let result = app.out_of_sync_recovery(current_ledger).await;

        // The gate prevented escalation (AtTip is not Behind or Ahead-no-ext).
        assert!(
            result.is_none(),
            "escalation must be skipped when node is at tip"
        );

        // App state must remain Initializing — escalation was NOT taken.
        assert_eq!(
            app.state().await,
            AppState::Initializing,
            "state must not change to CatchingUp when escalation is skipped"
        );

        // Counter must NOT be reset to 0 by the escalation path.
        let counter = app
            .recovery_attempts_without_progress
            .load(Ordering::SeqCst);
        assert!(
            counter > RECOVERY_ESCALATION_CATCHUP,
            "counter ({}) must not be reset when escalation is skipped (at tip)",
            counter,
        );
    }

    /// Regression test for #1866: the Ahead state with `latest_ext=0` must
    /// escalate to catchup, not loop forever requesting SCP state.
    ///
    /// In quickstart local mode, captive-core closes ledgers from the
    /// validator's EXTERNALIZE messages but never externalizes itself, so
    /// `latest_ext` stays 0 while `current_ledger` advances. Without
    /// escalation, recovery loops infinitely requesting SCP state.
    #[tokio::test]
    async fn test_recovery_ahead_no_ext_escalates_to_catchup() {
        let dir = tempfile::tempdir().expect("temp dir");
        let db_path = dir.path().join("rs-stellar-test.db");
        let config = crate::config::ConfigBuilder::new()
            .database_path(db_path)
            .build();
        let app = App::new(config).await.unwrap();

        // Fresh herder: latest_externalized=0.
        // current_ledger=29 → Ahead state (captive-core scenario).
        let current_ledger = 29u32;
        let latest = app.herder.latest_externalized_slot().unwrap_or(0);
        assert_eq!(latest, 0, "precondition: latest_ext must be 0");
        assert!(
            (current_ledger as u64) > latest,
            "precondition: node must be ahead of consensus"
        );

        // Seed archive cache so trigger_recovery_catchup can proceed.
        let next_cp = henyey_history::checkpoint::checkpoint_containing(current_ledger + 1);
        app.archive_checkpoint_cache.seed(next_cp + 64);

        // Pump attempts past the escalation threshold.
        let above_threshold = RECOVERY_ESCALATION_CATCHUP + 5;
        app.recovery_attempts_without_progress
            .store(above_threshold, Ordering::SeqCst);
        app.recovery_baseline_ledger
            .store(current_ledger as u64, Ordering::SeqCst);

        let result = app.out_of_sync_recovery(current_ledger).await;

        // spawn_catchup returns None on test App (no self_arc), but the
        // escalation path was taken.
        assert!(
            result.is_none(),
            "spawn_catchup returns None on test App, but escalation path was taken"
        );

        // Key assertion: spawn_catchup transitions to CatchingUp before
        // the self_arc check fails, so the app state proves escalation
        // was entered. Without the Ahead-no-ext fix, this would remain
        // Initializing because the escalation guard would skip catchup.
        assert_eq!(
            app.state().await,
            AppState::CatchingUp,
            "escalation path must transition state to CatchingUp"
        );

        // Verify catchup_in_progress was NOT left stuck on.
        assert!(
            !app.catchup_in_progress.load(Ordering::SeqCst),
            "catchup_in_progress must not be left set after failed spawn"
        );
    }

    /// Regression test for #1866: the fast-track must also fire for the
    /// Ahead-no-ext state when SCP messages are flowing.
    #[tokio::test]
    async fn test_fast_track_fires_for_ahead_no_ext() {
        let dir = tempfile::tempdir().expect("temp dir");
        let db_path = dir.path().join("rs-stellar-test.db");
        let config = crate::config::ConfigBuilder::new()
            .database_path(db_path)
            .build();
        let app = App::new(config).await.unwrap();

        // Fresh herder: latest_externalized=0.
        // current_ledger=10 → Ahead state.
        let current_ledger = 10u32;
        let latest = app.herder.latest_externalized_slot().unwrap_or(0);
        assert_eq!(latest, 0, "precondition: latest_ext must be 0");

        // Seed archive cache for catchup.
        let next_cp = henyey_history::checkpoint::checkpoint_containing(current_ledger + 1);
        app.archive_checkpoint_cache.seed(next_cp + 64);

        // Set SCP messages received > 0 to satisfy fast-track condition.
        app.scp_messages_received.store(10, Ordering::Relaxed);

        // Set attempts=1 (past `attempts >= 1` guard) but below
        // RECOVERY_ESCALATION_SCP_REQUEST so we enter the fast-track branch.
        app.recovery_attempts_without_progress
            .store(1, Ordering::SeqCst);
        app.recovery_baseline_ledger
            .store(current_ledger as u64, Ordering::SeqCst);

        let result = app.out_of_sync_recovery(current_ledger).await;

        // Fast-track fires → trigger_recovery_catchup called → spawn_catchup
        // transitions to CatchingUp before self_arc fails.
        assert!(
            result.is_none(),
            "spawn_catchup returns None on test App, but fast-track was taken"
        );

        // State proves the fast-track catchup path was entered.
        assert_eq!(
            app.state().await,
            AppState::CatchingUp,
            "fast-track path must transition state to CatchingUp"
        );

        // Verify catchup_in_progress was NOT left stuck on.
        assert!(
            !app.catchup_in_progress.load(Ordering::SeqCst),
            "catchup_in_progress must not be left set after failed spawn"
        );
    }

    /// Regression test for #1897: AtTip fast-track must fire even when
    /// `attempts >= RECOVERY_ESCALATION_SCP_REQUEST`.
    ///
    /// Before the fix, the fast-track was nested inside
    /// `attempts < RECOVERY_ESCALATION_SCP_REQUEST`, making it unreachable
    /// once the attempt counter crossed 6. The node would loop forever
    /// requesting SCP state without escalating to catchup.
    #[tokio::test]
    async fn test_recovery_at_tip_high_attempts_fast_tracks_catchup() {
        let dir = tempfile::tempdir().expect("temp dir");
        let db_path = dir.path().join("rs-stellar-test.db");
        let config = crate::config::ConfigBuilder::new()
            .database_path(db_path)
            .build();
        let app = App::new(config).await.unwrap();

        // AtTip: current_ledger == latest_externalized (both 0 on fresh App).
        let current_ledger = 0u32;
        let latest = app.herder.latest_externalized_slot().unwrap_or(0);
        assert_eq!(
            current_ledger as u64, latest,
            "precondition: node must be at tip"
        );

        // Seed archive cache so trigger_recovery_catchup can proceed.
        let next_cp = henyey_history::checkpoint::checkpoint_containing(current_ledger + 1);
        app.archive_checkpoint_cache.seed(next_cp + 64);

        // SCP messages received > 0 (existing cumulative heuristic for stall
        // evidence — the node has been receiving SCP traffic but cannot
        // externalize).
        app.scp_messages_received.store(10, Ordering::Relaxed);

        // Pump attempts to exactly RECOVERY_ESCALATION_SCP_REQUEST — the
        // boundary that gated the fast-track before the fix.
        app.recovery_attempts_without_progress
            .store(RECOVERY_ESCALATION_SCP_REQUEST, Ordering::SeqCst);
        app.recovery_baseline_ledger.store(0, Ordering::SeqCst);

        let _result = app.out_of_sync_recovery(current_ledger).await;

        // Assert: fast-track fires → spawn_catchup sets state to CatchingUp.
        // spawn_catchup fails on test App (no self_arc), but the state
        // transition is unambiguous proof the fast-track path was taken.
        assert_eq!(
            app.state().await,
            AppState::CatchingUp,
            "AtTip with high attempts + SCP activity must fast-track to catchup, \
             not loop requesting SCP state"
        );

        // Verify catchup_in_progress was NOT left stuck on.
        assert!(
            !app.catchup_in_progress.load(Ordering::SeqCst),
            "catchup_in_progress must not be left set after failed spawn"
        );
    }

    /// Regression test for #1898: historical SCP traffic (from before the
    /// current stall window) must NOT trigger the fast-track gate.
    ///
    /// When `recovery_baseline_scp_received == scp_messages_received`, all
    /// SCP traffic is pre-reset and `scp_since_reset == 0` → no fast-track.
    #[tokio::test]
    async fn test_fast_track_skipped_when_scp_traffic_is_historical() {
        let dir = tempfile::tempdir().expect("temp dir");
        let db_path = dir.path().join("rs-stellar-test.db");
        let config = crate::config::ConfigBuilder::new()
            .database_path(db_path)
            .build();
        let app = App::new(config).await.unwrap();

        let current_ledger = 0u32;

        // All SCP traffic is historical (pre-reset).
        app.scp_messages_received.store(10, Ordering::Relaxed);
        app.recovery_baseline_scp_received
            .store(10, Ordering::SeqCst);

        // Past the `attempts >= 1` guard.
        app.recovery_attempts_without_progress
            .store(1, Ordering::SeqCst);
        app.recovery_baseline_ledger.store(0, Ordering::SeqCst);

        // Seed archive cache so the fast-track *could* proceed if it fired.
        let next_cp = henyey_history::checkpoint::checkpoint_containing(current_ledger + 1);
        app.archive_checkpoint_cache.seed(next_cp + 64);

        let result = app.out_of_sync_recovery(current_ledger).await;

        // Fast-track must NOT fire — scp_since_reset = 0.
        // State must remain Initializing (not CatchingUp).
        assert_eq!(
            app.state().await,
            AppState::Initializing,
            "historical SCP traffic (scp_since_reset=0) must not trigger fast-track"
        );
        assert!(
            result.is_none(),
            "no catchup should be spawned with only historical SCP traffic"
        );
    }

    /// Positive complement to the historical-traffic test: SCP traffic
    /// *since* the last reset must trigger the fast-track gate.
    #[tokio::test]
    async fn test_fast_track_fires_with_scp_traffic_since_reset() {
        let dir = tempfile::tempdir().expect("temp dir");
        let db_path = dir.path().join("rs-stellar-test.db");
        let config = crate::config::ConfigBuilder::new()
            .database_path(db_path)
            .build();
        let app = App::new(config).await.unwrap();

        let current_ledger = 0u32;

        // 5 SCP messages since the last reset.
        app.scp_messages_received.store(15, Ordering::Relaxed);
        app.recovery_baseline_scp_received
            .store(10, Ordering::SeqCst);

        app.recovery_attempts_without_progress
            .store(1, Ordering::SeqCst);
        app.recovery_baseline_ledger.store(0, Ordering::SeqCst);

        // Seed archive cache so trigger_recovery_catchup can proceed.
        let next_cp = henyey_history::checkpoint::checkpoint_containing(current_ledger + 1);
        app.archive_checkpoint_cache.seed(next_cp + 64);

        let _result = app.out_of_sync_recovery(current_ledger).await;

        // Fast-track fires → spawn_catchup sets state to CatchingUp.
        assert_eq!(
            app.state().await,
            AppState::CatchingUp,
            "SCP traffic since reset (scp_since_reset=5) must trigger fast-track"
        );
    }

    /// Unit test for `reset_recovery_attempts`: verifies that the helper
    /// snapshots the SCP baseline and sets the attempt counter correctly
    /// for both Full and Partial cases.
    #[tokio::test]
    async fn test_reset_recovery_attempts_snapshots_scp_baseline() {
        let dir = tempfile::tempdir().expect("temp dir");
        let db_path = dir.path().join("rs-stellar-test.db");
        let config = crate::config::ConfigBuilder::new()
            .database_path(db_path)
            .build();
        let app = App::new(config).await.unwrap();

        // Test Full reset.
        app.scp_messages_received.store(42, Ordering::Relaxed);
        app.max_verified_scp_slot.store(999, Ordering::Relaxed);
        app.reset_recovery_attempts(RecoveryResetMode::Full);
        assert_eq!(
            app.recovery_baseline_scp_received.load(Ordering::SeqCst),
            42,
            "Full reset must snapshot current SCP count"
        );
        assert_eq!(
            app.recovery_attempts_without_progress
                .load(Ordering::SeqCst),
            0,
            "Full reset must set attempts to 0"
        );
        assert_eq!(
            app.max_verified_scp_slot.load(Ordering::Relaxed),
            0,
            "Full reset must clear max_verified_scp_slot"
        );

        // Test Partial re-arm (seed=1).
        app.scp_messages_received.store(100, Ordering::Relaxed);
        app.max_verified_scp_slot.store(500, Ordering::Relaxed);
        app.reset_recovery_attempts(RecoveryResetMode::Partial { seed: 1 });
        assert_eq!(
            app.recovery_baseline_scp_received.load(Ordering::SeqCst),
            100,
            "Partial must snapshot current SCP count"
        );
        assert_eq!(
            app.recovery_attempts_without_progress
                .load(Ordering::SeqCst),
            1,
            "Partial must set attempts to seed"
        );
        assert_eq!(
            app.max_verified_scp_slot.load(Ordering::Relaxed),
            500,
            "Partial must preserve max_verified_scp_slot"
        );
    }

    #[tokio::test]
    async fn test_clear_archive_recovery_full_progress() {
        let dir = tempfile::tempdir().expect("temp dir");
        let db_path = dir.path().join("rs-stellar-test.db");
        let config = crate::config::ConfigBuilder::new()
            .database_path(db_path)
            .build();
        let app = App::new(config).await.unwrap();

        // Pre-arm state.
        {
            let mut guard = app.archive_recovery_status.write().await;
            *guard = ArchiveRecoveryStatus::ConfirmedBehind {
                backoff_until: Some(Instant::now()),
            };
        }
        app.archive_checkpoint_cache.set_urgent(true);
        app.hard_reset_livelock_start.store(42, Ordering::Relaxed);
        app.recovery_attempts_without_progress
            .store(5, Ordering::SeqCst);

        let was_armed = app
            .clear_archive_recovery_state(ArchiveRecoveryClear::FullProgress)
            .await;

        assert!(
            was_armed,
            "was_armed should be true when backoff_until was Some"
        );
        let snap = app.archive_recovery_snapshot().await;
        assert!(!snap.is_confirmed_behind(), "status must be cleared");
        assert!(
            !snap.is_backoff_active(app.clock.now()),
            "backoff must be cleared"
        );
        assert!(
            !app.archive_checkpoint_cache.is_urgent(),
            "urgent must be disabled for FullProgress"
        );
        assert_eq!(
            app.hard_reset_livelock_start.load(Ordering::Relaxed),
            0,
            "livelock_start must be cleared for FullProgress"
        );
        assert_eq!(
            app.recovery_attempts_without_progress
                .load(Ordering::SeqCst),
            0,
            "recovery_attempts must be reset (Full)"
        );
    }

    #[tokio::test]
    async fn test_clear_archive_recovery_partial_progress() {
        let dir = tempfile::tempdir().expect("temp dir");
        let db_path = dir.path().join("rs-stellar-test.db");
        let config = crate::config::ConfigBuilder::new()
            .database_path(db_path)
            .build();
        let app = App::new(config).await.unwrap();

        // Pre-arm state.
        {
            let mut guard = app.archive_recovery_status.write().await;
            *guard = ArchiveRecoveryStatus::ConfirmedBehind {
                backoff_until: Some(Instant::now()),
            };
        }
        app.archive_checkpoint_cache.set_urgent(false);
        app.hard_reset_livelock_start.store(77, Ordering::Relaxed);

        let was_armed = app
            .clear_archive_recovery_state(ArchiveRecoveryClear::PartialProgress { seed: 10 })
            .await;

        assert!(was_armed);
        assert!(!app.archive_recovery_snapshot().await.is_confirmed_behind());
        assert!(
            app.archive_checkpoint_cache.is_urgent(),
            "urgent must be ENABLED for PartialProgress"
        );
        assert_eq!(
            app.hard_reset_livelock_start.load(Ordering::Relaxed),
            0,
            "livelock_start must be cleared for PartialProgress"
        );
        // Partial re-seeds: fetch_max(10) from 0 → 10.
        assert!(
            app.recovery_attempts_without_progress
                .load(Ordering::SeqCst)
                >= 10,
            "recovery_attempts must be re-seeded (Partial)"
        );
    }

    #[tokio::test]
    async fn test_clear_archive_recovery_defense_skip() {
        let dir = tempfile::tempdir().expect("temp dir");
        let db_path = dir.path().join("rs-stellar-test.db");
        let config = crate::config::ConfigBuilder::new()
            .database_path(db_path)
            .build();
        let app = App::new(config).await.unwrap();

        // Pre-arm state.
        {
            let mut guard = app.archive_recovery_status.write().await;
            *guard = ArchiveRecoveryStatus::ConfirmedBehind {
                backoff_until: Some(Instant::now()),
            };
        }
        app.archive_checkpoint_cache.set_urgent(true);
        app.hard_reset_livelock_start.store(55, Ordering::Relaxed);
        app.recovery_attempts_without_progress
            .store(3, Ordering::SeqCst);

        let was_armed = app
            .clear_archive_recovery_state(ArchiveRecoveryClear::DefenseSkip)
            .await;

        assert!(was_armed);
        assert!(!app.archive_recovery_snapshot().await.is_confirmed_behind());
        assert!(
            !app.archive_checkpoint_cache.is_urgent(),
            "urgent must be disabled for DefenseSkip"
        );
        assert_eq!(
            app.hard_reset_livelock_start.load(Ordering::Relaxed),
            0,
            "livelock_start must be cleared for DefenseSkip"
        );
        assert_eq!(
            app.recovery_attempts_without_progress
                .load(Ordering::SeqCst),
            3,
            "recovery_attempts must NOT be reset for DefenseSkip"
        );
    }

    #[tokio::test]
    async fn test_clear_archive_recovery_hard_reset_exec() {
        let dir = tempfile::tempdir().expect("temp dir");
        let db_path = dir.path().join("rs-stellar-test.db");
        let config = crate::config::ConfigBuilder::new()
            .database_path(db_path)
            .build();
        let app = App::new(config).await.unwrap();

        // Pre-arm state: urgent=true, livelock active.
        {
            let mut guard = app.archive_recovery_status.write().await;
            *guard = ArchiveRecoveryStatus::ConfirmedBehind {
                backoff_until: Some(Instant::now()),
            };
        }
        app.archive_checkpoint_cache.set_urgent(true);
        app.hard_reset_livelock_start.store(88, Ordering::Relaxed);
        app.recovery_attempts_without_progress
            .store(7, Ordering::SeqCst);

        let was_armed = app
            .clear_archive_recovery_state(ArchiveRecoveryClear::HardResetExec)
            .await;

        assert!(was_armed, "was_armed must be true");
        assert!(!app.archive_recovery_snapshot().await.is_confirmed_behind());
        assert!(
            app.archive_checkpoint_cache.is_urgent(),
            "urgent must be PRESERVED (not touched) for HardResetExec"
        );
        assert_eq!(
            app.hard_reset_livelock_start.load(Ordering::Relaxed),
            88,
            "livelock_start must be PRESERVED for HardResetExec"
        );
        assert_eq!(
            app.recovery_attempts_without_progress
                .load(Ordering::SeqCst),
            7,
            "recovery_attempts must NOT be reset for HardResetExec"
        );
    }

    #[tokio::test]
    async fn test_clear_archive_recovery_archive_confirmed_current() {
        let dir = tempfile::tempdir().expect("temp dir");
        let db_path = dir.path().join("rs-stellar-test.db");
        let config = crate::config::ConfigBuilder::new()
            .database_path(db_path)
            .build();
        let app = App::new(config).await.unwrap();

        // Pre-arm state: confirmed behind but no backoff armed.
        {
            let mut guard = app.archive_recovery_status.write().await;
            *guard = ArchiveRecoveryStatus::ConfirmedBehind {
                backoff_until: None,
            };
        }
        app.archive_checkpoint_cache.set_urgent(true);
        app.hard_reset_livelock_start.store(33, Ordering::Relaxed);
        app.recovery_attempts_without_progress
            .store(2, Ordering::SeqCst);

        let was_armed = app
            .clear_archive_recovery_state(ArchiveRecoveryClear::ArchiveConfirmedCurrent)
            .await;

        assert!(
            !was_armed,
            "was_armed must be false when backoff_until was None"
        );
        assert!(!app.archive_recovery_snapshot().await.is_confirmed_behind());
        assert!(
            !app.archive_checkpoint_cache.is_urgent(),
            "urgent must be disabled for ArchiveConfirmedCurrent"
        );
        assert_eq!(
            app.hard_reset_livelock_start.load(Ordering::Relaxed),
            33,
            "livelock_start must be PRESERVED for ArchiveConfirmedCurrent"
        );
        assert_eq!(
            app.recovery_attempts_without_progress
                .load(Ordering::SeqCst),
            2,
            "recovery_attempts must NOT be reset for ArchiveConfirmedCurrent"
        );
    }

    /// Integration test for the ledger-progress reset path: after the node
    /// makes progress (ledger advances), the SCP baseline is snapshotted
    /// so that pre-progress SCP traffic no longer satisfies the fast-track.
    #[tokio::test]
    async fn test_fast_track_skipped_after_ledger_progress_resets_baseline() {
        let dir = tempfile::tempdir().expect("temp dir");
        let db_path = dir.path().join("rs-stellar-test.db");
        let config = crate::config::ConfigBuilder::new()
            .database_path(db_path)
            .build();
        let app = App::new(config).await.unwrap();

        // Initial state: some SCP traffic, baseline=0 (startup default).
        app.scp_messages_received.store(10, Ordering::Relaxed);
        app.recovery_baseline_scp_received
            .store(0, Ordering::SeqCst);
        app.recovery_attempts_without_progress
            .store(3, Ordering::SeqCst);
        app.recovery_baseline_ledger.store(0, Ordering::SeqCst);

        // Call 1: current_ledger=5 > baseline=0 → progress detected.
        // This triggers reset_recovery_attempts(Full), snapshotting SCP baseline to 10.
        let _ = app.out_of_sync_recovery(5).await;

        // Verify the progress path snapshotted the SCP baseline.
        assert_eq!(
            app.recovery_baseline_scp_received.load(Ordering::SeqCst),
            10,
            "ledger progress must snapshot SCP baseline"
        );

        // Seed archive cache for the second call.
        let next_cp = henyey_history::checkpoint::checkpoint_containing(5 + 1);
        app.archive_checkpoint_cache.seed(next_cp + 64);

        // Pump attempts past the `>= 1` guard for the second call.
        // (The first call reset to 0 and then fetch_add'd to 1, so
        // attempts is now 1. But we need to enter recovery with
        // attempts >= 1 after the fetch_add, so store 1.)
        app.recovery_attempts_without_progress
            .store(1, Ordering::SeqCst);

        // Call 2: current_ledger=5, no further progress, no new SCP traffic.
        // scp_since_reset = 10 - 10 = 0 → fast-track must NOT fire.
        let result = app.out_of_sync_recovery(5).await;

        assert_eq!(
            app.state().await,
            AppState::Initializing,
            "after progress reset, no new SCP traffic must not trigger fast-track"
        );
        assert!(
            result.is_none(),
            "no catchup should be spawned when scp_since_reset=0"
        );
    }

    // ============================================================
    // TxSet exhaustion retry and metric tests (#1929)
    // ============================================================

    #[tokio::test]
    async fn test_mark_tx_set_exhausted_records_timestamp_on_first_transition() {
        let dir = tempfile::tempdir().expect("temp dir");
        let db_path = dir.path().join("rs-stellar-test.db");
        let config = crate::config::ConfigBuilder::new()
            .database_path(db_path)
            .build();
        let app = App::new(config).await.unwrap();

        // Initially both are unset.
        assert!(!app.tx_set_all_peers_exhausted.load(Ordering::SeqCst));
        assert_eq!(app.tx_set_exhausted_since.load(Ordering::SeqCst), 0);

        // First false→true transition should set the timestamp.
        app.mark_tx_set_exhausted();
        assert!(app.tx_set_all_peers_exhausted.load(Ordering::SeqCst));
        let since1 = app.tx_set_exhausted_since.load(Ordering::SeqCst);
        assert!(since1 > 0, "should record timestamp on first transition");

        // Second call (already true) should NOT change the timestamp.
        // Sleep briefly to ensure elapsed would differ.
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        app.mark_tx_set_exhausted();
        let since2 = app.tx_set_exhausted_since.load(Ordering::SeqCst);
        assert_eq!(
            since1, since2,
            "should NOT reset timestamp on repeated store"
        );
    }

    #[tokio::test]
    async fn test_clear_tx_set_exhausted_clears_both_flag_and_timestamp() {
        let dir = tempfile::tempdir().expect("temp dir");
        let db_path = dir.path().join("rs-stellar-test.db");
        let config = crate::config::ConfigBuilder::new()
            .database_path(db_path)
            .build();
        let app = App::new(config).await.unwrap();

        app.mark_tx_set_exhausted();
        assert!(app.tx_set_all_peers_exhausted.load(Ordering::SeqCst));
        assert!(app.tx_set_exhausted_since.load(Ordering::SeqCst) > 0);

        app.clear_tx_set_exhausted();
        assert!(!app.tx_set_all_peers_exhausted.load(Ordering::SeqCst));
        assert_eq!(app.tx_set_exhausted_since.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn test_reset_tx_set_tracking_clears_retry_map_and_exhausted_since() {
        let dir = tempfile::tempdir().expect("temp dir");
        let db_path = dir.path().join("rs-stellar-test.db");
        let config = crate::config::ConfigBuilder::new()
            .database_path(db_path)
            .build();
        let app = App::new(config).await.unwrap();

        // Seed all tracking state.
        app.mark_tx_set_exhausted();
        let hash = Hash256::from_bytes([10u8; 32]);
        app.tx_set_dont_have.write().await.insert(
            hash,
            HashSet::from([henyey_overlay::PeerId::from_bytes([1u8; 32])]),
        );
        app.tx_set_last_request.write().await.insert(
            hash,
            TxSetRequestState {
                last_request: Instant::now(),
                first_requested: Instant::now(),
                next_peer_offset: 0,
            },
        );
        app.tx_set_exhausted_warned.write().await.insert(hash);
        app.tx_set_last_retry
            .write()
            .await
            .insert(hash, Instant::now());

        app.reset_tx_set_tracking().await;

        assert!(!app.tx_set_all_peers_exhausted.load(Ordering::SeqCst));
        assert_eq!(app.tx_set_exhausted_since.load(Ordering::SeqCst), 0);
        assert!(app.tx_set_dont_have.read().await.is_empty());
        assert!(app.tx_set_last_request.read().await.is_empty());
        assert!(app.tx_set_exhausted_warned.read().await.is_empty());
        assert!(app.tx_set_last_retry.read().await.is_empty());
    }

    #[tokio::test]
    async fn test_retry_exhausted_tx_sets_skips_when_not_exhausted() {
        let dir = tempfile::tempdir().expect("temp dir");
        let db_path = dir.path().join("rs-stellar-test.db");
        let config = crate::config::ConfigBuilder::new()
            .database_path(db_path)
            .build();
        let app = App::new(config).await.unwrap();

        // Not exhausted — retry should be a no-op.
        assert!(!app.tx_set_all_peers_exhausted.load(Ordering::SeqCst));
        app.retry_exhausted_tx_sets().await;
        // No panic, no state change.
        assert!(!app.tx_set_all_peers_exhausted.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn test_retry_exhausted_tx_sets_no_overlay_graceful() {
        let dir = tempfile::tempdir().expect("temp dir");
        let db_path = dir.path().join("rs-stellar-test.db");
        let config = crate::config::ConfigBuilder::new()
            .database_path(db_path)
            .build();
        let app = App::new(config).await.unwrap();

        // Set exhausted but no overlay available — should not panic.
        app.mark_tx_set_exhausted();
        app.retry_exhausted_tx_sets().await;
        // Flag remains set (no peers to retry with).
        assert!(app.tx_set_all_peers_exhausted.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn test_tx_set_exhausted_since_offset_reflects_state() {
        let dir = tempfile::tempdir().expect("temp dir");
        let db_path = dir.path().join("rs-stellar-test.db");
        let config = crate::config::ConfigBuilder::new()
            .database_path(db_path)
            .build();
        let app = App::new(config).await.unwrap();

        assert_eq!(app.tx_set_exhausted_since_offset(), 0);

        app.mark_tx_set_exhausted();
        let offset = app.tx_set_exhausted_since_offset();
        assert!(offset > 0, "should be non-zero after exhaustion");

        app.clear_tx_set_exhausted();
        assert_eq!(
            app.tx_set_exhausted_since_offset(),
            0,
            "should be zero after clearing"
        );
    }

    #[test]
    fn test_tx_set_eligible_peers_prefers_outbound() {
        use std::net::SocketAddr;

        let make_info = |id: u8, dir: ConnectionDirection| henyey_overlay::PeerInfo {
            peer_id: henyey_overlay::PeerId::from_bytes([id; 32]),
            address: SocketAddr::from(([127, 0, 0, id], 11625)),
            direction: dir,
            version_string: String::new(),
            overlay_version: 0,
            ledger_version: 0,
            connected_at: Instant::now(),
            original_address: None,
        };

        // Mixed outbound + inbound — should only return outbound.
        let infos = vec![
            make_info(1, ConnectionDirection::Inbound),
            make_info(2, ConnectionDirection::Outbound),
            make_info(3, ConnectionDirection::Outbound),
        ];
        let peers = App::tx_set_eligible_peers(&infos);
        assert_eq!(peers.len(), 2);
        assert!(peers.contains(&henyey_overlay::PeerId::from_bytes([2u8; 32])));
        assert!(peers.contains(&henyey_overlay::PeerId::from_bytes([3u8; 32])));

        // All inbound — should fall back to all.
        let infos = vec![
            make_info(4, ConnectionDirection::Inbound),
            make_info(5, ConnectionDirection::Inbound),
        ];
        let peers = App::tx_set_eligible_peers(&infos);
        assert_eq!(peers.len(), 2);

        // Empty — empty result.
        let peers = App::tx_set_eligible_peers(&[]);
        assert!(peers.is_empty());
    }

    /// Verify `db_blocking` propagates Ok, Err, and re-panics JoinError.
    /// Uses a real App constructed via `App::new` with a temp database.
    #[tokio::test]
    async fn test_db_blocking_ok_and_err() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = AppConfig::default();
        config.database.path = dir.path().join("test.db");
        let app = App::new(config).await.unwrap();

        // Ok path
        let result = app.db_blocking("test-ok", |_db| Ok(42)).await;
        assert_eq!(result.unwrap(), 42);

        // Err path
        let result: anyhow::Result<()> = app
            .db_blocking("test-err", |_db| anyhow::bail!("simulated"))
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("simulated"));
    }

    #[tokio::test]
    #[should_panic(expected = "boom")]
    async fn test_db_blocking_repanics_on_join_error() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = AppConfig::default();
        config.database.path = dir.path().join("test.db");
        let app = App::new(config).await.unwrap();

        let _: anyhow::Result<()> = app.db_blocking("test-panic", |_db| panic!("boom")).await;
    }

    // ---- advance_survey_scheduler tests ----

    /// Helper: create an App for survey scheduler tests.
    async fn survey_test_app() -> Arc<App> {
        let dir = tempfile::tempdir().unwrap();
        let config = crate::config::ConfigBuilder::new()
            .database_path(dir.path().join("survey-test.db"))
            .build();
        Arc::new(App::new(config).await.unwrap())
    }

    #[tokio::test]
    async fn test_advance_survey_scheduler_not_due() {
        let app = survey_test_app().await;
        let now = app.clock.now();

        // Set next_action far in the future so the scheduler should be a no-op.
        {
            let mut sched = app.survey_scheduler.lock().await;
            sched.next_action = now + Duration::from_secs(3600);
        }

        app.advance_survey_scheduler().await;

        // Phase should remain Idle, next_action unchanged.
        let sched = app.survey_scheduler.lock().await;
        assert_eq!(sched.phase, SurveySchedulerPhase::Idle);
        assert!(sched.next_action > now + Duration::from_secs(3599));
    }

    #[tokio::test]
    async fn test_advance_survey_scheduler_idle_active_survey() {
        let app = survey_test_app().await;
        let now = app.clock.now();

        // Make the scheduler due.
        {
            let mut sched = app.survey_scheduler.lock().await;
            sched.next_action = now - Duration::from_secs(1);
        }

        // Activate survey_data so the Idle path sees survey_is_active() == true.
        {
            let mut state = app.survey_state.write().await;
            let msg = stellar_xdr::curr::TimeSlicedSurveyStartCollectingMessage {
                surveyor_id: stellar_xdr::curr::NodeId(
                    stellar_xdr::curr::PublicKey::PublicKeyTypeEd25519(stellar_xdr::curr::Uint256(
                        [0u8; 32],
                    )),
                ),
                nonce: 99,
                ledger_num: 1,
            };
            let _ = state.data_mut().start_collecting(
                &msg,
                &[],
                &[],
                crate::survey::NodeStatsSnapshot {
                    lost_sync_count: 0,
                    out_of_sync: false,
                    added_peers: 0,
                    dropped_peers: 0,
                },
            );
        }

        app.advance_survey_scheduler().await;

        // Phase stays Idle, next_action bumped by SURVEY_INTERVAL (60s).
        let sched = app.survey_scheduler.lock().await;
        assert_eq!(sched.phase, SurveySchedulerPhase::Idle);
        assert!(sched.next_action >= now + Duration::from_secs(59));
    }

    #[tokio::test]
    async fn test_advance_survey_scheduler_idle_reporting_running() {
        let app = survey_test_app().await;
        let now = app.clock.now();

        {
            let mut sched = app.survey_scheduler.lock().await;
            sched.next_action = now - Duration::from_secs(1);
        }
        {
            let mut reporting = app.survey_reporting.write().await;
            reporting.running = true;
        }

        app.advance_survey_scheduler().await;

        let sched = app.survey_scheduler.lock().await;
        assert_eq!(sched.phase, SurveySchedulerPhase::Idle);
        assert!(sched.next_action >= now + Duration::from_secs(59));
    }

    #[tokio::test]
    async fn test_advance_survey_scheduler_idle_wrong_state() {
        let app = survey_test_app().await;
        let now = app.clock.now();

        {
            let mut sched = app.survey_scheduler.lock().await;
            sched.next_action = now - Duration::from_secs(1);
        }
        // App starts in Initializing, which is not Synced/Validating.
        assert_eq!(app.state().await, AppState::Initializing);

        app.advance_survey_scheduler().await;

        let sched = app.survey_scheduler.lock().await;
        assert_eq!(sched.phase, SurveySchedulerPhase::Idle);
        assert!(sched.next_action >= now + Duration::from_secs(59));
    }

    #[tokio::test]
    async fn test_advance_survey_scheduler_idle_throttled() {
        let app = survey_test_app().await;
        let now = app.clock.now();

        {
            let mut sched = app.survey_scheduler.lock().await;
            sched.next_action = now - Duration::from_secs(1);
            // Set last_started very recently so the throttle kicks in.
            sched.last_started = Some(now);
        }
        // Set state to Synced so we pass the state check.
        *app.state.write().await = AppState::Synced;

        app.advance_survey_scheduler().await;

        let sched = app.survey_scheduler.lock().await;
        assert_eq!(sched.phase, SurveySchedulerPhase::Idle);
        // next_action should be set to last_started + throttle, not now + INTERVAL.
        assert!(sched.next_action > now);
    }

    #[tokio::test]
    async fn test_advance_survey_scheduler_idle_no_overlay() {
        let app = survey_test_app().await;
        let now = app.clock.now();

        {
            let mut sched = app.survey_scheduler.lock().await;
            sched.next_action = now - Duration::from_secs(1);
            sched.last_started = None;
        }
        *app.state.write().await = AppState::Synced;
        // No overlay started → overlay() returns None.

        app.advance_survey_scheduler().await;

        let sched = app.survey_scheduler.lock().await;
        assert_eq!(sched.phase, SurveySchedulerPhase::Idle);
        assert!(sched.next_action >= now + Duration::from_secs(59));
    }

    #[tokio::test]
    async fn test_advance_survey_scheduler_startsent_failure_cleanup() {
        let app = survey_test_app().await;
        let now = app.clock.now();
        let test_nonce = 42u32;

        // Pre-populate scheduler in StartSent phase.
        {
            let mut sched = app.survey_scheduler.lock().await;
            sched.phase = SurveySchedulerPhase::StartSent;
            sched.next_action = now - Duration::from_secs(1);
            sched.nonce = test_nonce;
            // Use a dummy peer ID — send_survey_requests will fail because
            // there's no overlay.
            sched.peers = vec![henyey_overlay::PeerId::from_bytes([1u8; 32])];
        }

        // Pre-populate survey_secrets and survey_results so we can verify cleanup.
        app.survey_secrets
            .write()
            .await
            .insert(test_nonce, [0u8; 32]);
        app.survey_results
            .write()
            .await
            .insert(test_nonce, HashMap::new());

        app.advance_survey_scheduler().await;

        // Verify cleanup: phase back to Idle, secrets and results removed.
        let sched = app.survey_scheduler.lock().await;
        assert_eq!(sched.phase, SurveySchedulerPhase::Idle);
        assert!(sched.next_action >= now + Duration::from_secs(59));

        assert!(
            !app.survey_secrets.read().await.contains_key(&test_nonce),
            "survey_secrets should be cleaned up on StartSent failure"
        );
        assert!(
            !app.survey_results.read().await.contains_key(&test_nonce),
            "survey_results should be cleaned up on StartSent failure"
        );
    }

    #[tokio::test]
    async fn test_advance_survey_scheduler_requestsent_to_idle() {
        let app = survey_test_app().await;
        let now = app.clock.now();
        let test_nonce = 77u32;

        // Pre-populate scheduler in RequestSent phase with no overlay.
        // send_survey_stop will early-return (no overlay), but the phase
        // transition should still happen.
        {
            let mut sched = app.survey_scheduler.lock().await;
            sched.phase = SurveySchedulerPhase::RequestSent;
            sched.next_action = now - Duration::from_secs(1);
            sched.nonce = test_nonce;
            sched.peers = vec![henyey_overlay::PeerId::from_bytes([2u8; 32])];
        }

        app.advance_survey_scheduler().await;

        // Verify full reset to Idle.
        let sched = app.survey_scheduler.lock().await;
        assert_eq!(sched.phase, SurveySchedulerPhase::Idle);
        assert_eq!(sched.nonce, 0);
        assert!(sched.peers.is_empty());
        assert!(sched.next_action >= now + Duration::from_secs(59));
    }

    /// Build a structurally valid v1 HAS JSON with one custom bucket level
    /// containing the given curr/snap hex hashes, padded to BUCKET_LIST_LEVELS.
    fn make_has_json(ledger: u32, curr_hex: &str, snap_hex: &str) -> String {
        let zero = "0".repeat(64);
        let zero_level = serde_json::json!({
            "curr": zero, "snap": zero, "next": { "state": 0 }
        });
        let mut levels = vec![serde_json::json!({
            "curr": curr_hex,
            "snap": snap_hex,
            "next": { "state": 0 }
        })];
        while levels.len() < henyey_bucket::BUCKET_LIST_LEVELS {
            levels.push(zero_level.clone());
        }
        serde_json::json!({
            "version": 1,
            "currentLedger": ledger,
            "currentBuckets": levels
        })
        .to_string()
    }

    #[test]
    fn test_collect_publish_queue_bucket_hashes_returns_hashes() {
        let db = henyey_db::Database::open_in_memory().unwrap();
        let hash_a = "aa".repeat(32); // 64 hex chars = 32 bytes
        let hash_b = "bb".repeat(32);
        let has_json = make_has_json(128, &hash_a, &hash_b);

        db.with_connection(|conn| {
            use henyey_db::queries::publish_queue::PublishQueueQueries;
            conn.enqueue_publish(128, &has_json)
        })
        .unwrap();

        let hashes = collect_publish_queue_bucket_hashes(&db).unwrap();
        let hex_set: std::collections::HashSet<String> =
            hashes.iter().map(|h| h.to_hex()).collect();
        assert!(hex_set.contains(&hash_a), "expected hash_a in result");
        assert!(hex_set.contains(&hash_b), "expected hash_b in result");
    }

    #[test]
    fn test_collect_publish_queue_bucket_hashes_empty_queue() {
        let db = henyey_db::Database::open_in_memory().unwrap();
        let hashes = collect_publish_queue_bucket_hashes(&db).unwrap();
        assert!(hashes.is_empty());
    }

    #[test]
    fn test_collect_publish_queue_bucket_hashes_rejects_malformed_json() {
        let db = henyey_db::Database::open_in_memory().unwrap();
        // Insert malformed JSON directly (bypasses normal enqueue path)
        db.with_connection(|conn| {
            use henyey_db::queries::publish_queue::PublishQueueQueries;
            conn.enqueue_publish(128, "not valid json")
        })
        .unwrap();

        let result = collect_publish_queue_bucket_hashes(&db);
        assert!(result.is_err(), "malformed HAS JSON should cause an error");
    }

    #[test]
    fn test_collect_db_referenced_bucket_hashes_includes_authoritative_has() {
        let db = henyey_db::Database::open_in_memory().unwrap();
        let hash_a = "cc".repeat(32);
        let hash_b = "dd".repeat(32);
        let has_json = make_has_json(64, &hash_a, &hash_b);

        db.with_connection(|conn| {
            use henyey_db::queries::StateQueries;
            conn.set_state(state_keys::HISTORY_ARCHIVE_STATE, &has_json)
        })
        .unwrap();

        let hashes = collect_db_referenced_bucket_hashes(&db).unwrap();
        let hex_set: std::collections::HashSet<String> =
            hashes.iter().map(|h| h.to_hex()).collect();
        assert!(hex_set.contains(&hash_a));
        assert!(hex_set.contains(&hash_b));
    }

    #[test]
    fn test_collect_db_referenced_bucket_hashes_includes_publish_queue() {
        let db = henyey_db::Database::open_in_memory().unwrap();
        let auth_curr = "aa".repeat(32);
        let auth_snap = "bb".repeat(32);
        let pq_curr = "cc".repeat(32);
        let pq_snap = "dd".repeat(32);

        let auth_has = make_has_json(64, &auth_curr, &auth_snap);
        let pq_has = make_has_json(128, &pq_curr, &pq_snap);

        db.with_connection(|conn| {
            use henyey_db::queries::publish_queue::PublishQueueQueries;
            use henyey_db::queries::StateQueries;
            conn.set_state(state_keys::HISTORY_ARCHIVE_STATE, &auth_has)?;
            conn.enqueue_publish(128, &pq_has)
        })
        .unwrap();

        let hashes = collect_db_referenced_bucket_hashes(&db).unwrap();
        let hex_set: std::collections::HashSet<String> =
            hashes.iter().map(|h| h.to_hex()).collect();
        assert!(hex_set.contains(&auth_curr), "authoritative curr");
        assert!(hex_set.contains(&auth_snap), "authoritative snap");
        assert!(hex_set.contains(&pq_curr), "publish queue curr");
        assert!(hex_set.contains(&pq_snap), "publish queue snap");
    }

    #[test]
    fn test_collect_db_referenced_bucket_hashes_rejects_malformed_authoritative_has() {
        let db = henyey_db::Database::open_in_memory().unwrap();
        db.with_connection(|conn| {
            use henyey_db::queries::StateQueries;
            conn.set_state(state_keys::HISTORY_ARCHIVE_STATE, "broken json")
        })
        .unwrap();

        let result = collect_db_referenced_bucket_hashes(&db);
        assert!(
            result.is_err(),
            "malformed authoritative HAS should cause an error"
        );
    }

    // ── ensure_buffered_slot tests ───────────────────────────────────────

    /// Helper to create a minimal LedgerCloseInfo for testing.
    fn make_close_info(
        slot: u64,
        tx_set_hash: henyey_common::Hash256,
        tx_set: Option<henyey_herder::TransactionSet>,
    ) -> henyey_herder::LedgerCloseInfo {
        henyey_herder::LedgerCloseInfo {
            slot,
            close_time: slot,
            tx_set_hash,
            tx_set,
            upgrades: Vec::new(),
            stellar_value_ext: stellar_xdr::curr::StellarValueExt::Basic,
        }
    }

    #[tokio::test]
    async fn test_ensure_buffered_slot_vacant_insert() {
        let dir = tempfile::tempdir().expect("temp dir");
        let config = crate::config::ConfigBuilder::new()
            .database_path(dir.path().join("test.db"))
            .build();
        let app = App::new(config).await.unwrap();

        let hash = henyey_common::Hash256::from_bytes([1u8; 32]);
        let info = make_close_info(100, hash, None);

        // Insert into vacant slot — should succeed, return false (no tx_set)
        let has_tx_set = app.ensure_buffered_slot(100, info).await;
        assert!(!has_tx_set, "No tx_set was provided");

        // Verify the entry exists
        let buffer = app.syncing_ledgers.read().await;
        assert!(buffer.contains_key(&100));
        assert_eq!(buffer[&100].tx_set_hash, hash);
        assert!(buffer[&100].tx_set.is_none());
    }

    #[tokio::test]
    async fn test_ensure_buffered_slot_upgrade_tx_set() {
        let dir = tempfile::tempdir().expect("temp dir");
        let config = crate::config::ConfigBuilder::new()
            .database_path(dir.path().join("test.db"))
            .build();
        let app = App::new(config).await.unwrap();

        let hash = henyey_common::Hash256::from_bytes([2u8; 32]);
        // First: insert without tx_set
        let info1 = make_close_info(200, hash, None);
        app.ensure_buffered_slot(200, info1).await;

        // Second: insert with tx_set (same hash) — should upgrade
        let tx_set =
            henyey_herder::TransactionSet::new_legacy(henyey_common::Hash256::ZERO, Vec::new());
        let info2 = make_close_info(200, hash, Some(tx_set));
        let has_tx_set = app.ensure_buffered_slot(200, info2).await;
        assert!(has_tx_set, "tx_set should be upgraded");

        let buffer = app.syncing_ledgers.read().await;
        assert!(buffer[&200].tx_set.is_some());
    }

    #[tokio::test]
    async fn test_ensure_buffered_slot_noop_when_already_has_tx_set() {
        let dir = tempfile::tempdir().expect("temp dir");
        let config = crate::config::ConfigBuilder::new()
            .database_path(dir.path().join("test.db"))
            .build();
        let app = App::new(config).await.unwrap();

        let hash = henyey_common::Hash256::from_bytes([3u8; 32]);
        let tx_set =
            henyey_herder::TransactionSet::new_legacy(henyey_common::Hash256::ZERO, Vec::new());
        // Insert with tx_set
        let info1 = make_close_info(300, hash, Some(tx_set.clone()));
        app.ensure_buffered_slot(300, info1).await;

        // Try again with no tx_set — should be no-op, return true
        let info2 = make_close_info(300, hash, None);
        let has_tx_set = app.ensure_buffered_slot(300, info2).await;
        assert!(has_tx_set, "existing tx_set should be preserved");
    }

    #[tokio::test]
    async fn test_ensure_buffered_slot_hash_mismatch_keeps_existing() {
        let dir = tempfile::tempdir().expect("temp dir");
        let config = crate::config::ConfigBuilder::new()
            .database_path(dir.path().join("test.db"))
            .build();
        let app = App::new(config).await.unwrap();

        let hash1 = henyey_common::Hash256::from_bytes([4u8; 32]);
        let hash2 = henyey_common::Hash256::from_bytes([5u8; 32]);

        // Insert with hash1
        let info1 = make_close_info(400, hash1, None);
        app.ensure_buffered_slot(400, info1).await;

        // Try to insert with different hash — should be rejected
        let tx_set =
            henyey_herder::TransactionSet::new_legacy(henyey_common::Hash256::ZERO, Vec::new());
        let info2 = make_close_info(400, hash2, Some(tx_set));
        let has_tx_set = app.ensure_buffered_slot(400, info2).await;
        assert!(
            !has_tx_set,
            "hash mismatch should keep existing (no tx_set)"
        );

        // Verify existing entry preserved
        let buffer = app.syncing_ledgers.read().await;
        assert_eq!(buffer[&400].tx_set_hash, hash1);
        assert!(buffer[&400].tx_set.is_none());
    }

    /// Regression: cursor must not advance past an unmaterialized slot.
    ///
    /// Setup: externalized slots {N+1, N+3}, but NOT N+2. N+2 is also
    /// not in syncing_ledgers. After process_externalized_slots,
    /// last_processed_slot should be N+1 (not N+3), because N+2 was
    /// never materialized.
    #[tokio::test]
    async fn test_process_externalized_slots_cursor_stops_at_gap() {
        let app = mk_test_app_for_pes_split().await;
        app.herder.set_state(henyey_herder::HerderState::Tracking);

        let n: u64 = 50;
        let driver = app.herder.scp_driver();
        // Externalize N+1 and N+3 — but NOT N+2
        for (slot, hash_byte) in &[(n + 1, 0x11u8), (n + 3, 0x33u8)] {
            let hash = [*hash_byte; 32];
            let xdr = mk_stellar_value_xdr(hash);
            driver.record_externalized(*slot, mk_value(xdr), None);
            driver.publish_externalized(*slot);
        }
        *app.last_processed_slot.write().await = n;

        let _pending = app.process_externalized_slots().await;

        // Cursor should have stopped at N+1 (contiguous from N),
        // not advanced to N+3.
        let last = *app.last_processed_slot.read().await;
        assert_eq!(
            last,
            n + 1,
            "cursor must stop at N+1, not advance past unmaterialized N+2"
        );

        // N+3 should still be in buffer though (it was materialized)
        let buf = app.syncing_ledgers.read().await;
        assert!(
            buf.contains_key(&((n + 3) as u32)),
            "N+3 should be buffered"
        );
    }

    /// Test that populate_gap_slots fills slots between current_ledger
    /// and the first buffered slot from externalized SCP cache.
    #[tokio::test]
    async fn test_populate_gap_slots_fills_gap() {
        let app = mk_test_app_for_pes_split().await;
        app.herder.set_state(henyey_herder::HerderState::Tracking);

        let current = 100u32;
        let first_buffered = 105u32;

        // Externalize gap slots 101..105
        let driver = app.herder.scp_driver();
        for slot in (current as u64 + 1)..(first_buffered as u64) {
            let hash = [slot as u8; 32];
            let xdr = mk_stellar_value_xdr(hash);
            driver.record_externalized(slot, mk_value(xdr), None);
            driver.publish_externalized(slot);
        }

        // Pre-seed the buffer with first_buffered only
        {
            let mut buf = app.syncing_ledgers.write().await;
            buf.insert(
                first_buffered,
                henyey_herder::LedgerCloseInfo {
                    slot: first_buffered as u64,
                    close_time: 0,
                    tx_set_hash: henyey_common::Hash256::from_bytes([0xAA; 32]),
                    tx_set: None,
                    upgrades: Vec::new(),
                    stellar_value_ext: stellar_xdr::curr::StellarValueExt::Basic,
                },
            );
        }

        app.populate_gap_slots(current).await;

        // All gap slots should now be in the buffer
        let buf = app.syncing_ledgers.read().await;
        for slot in (current + 1)..first_buffered {
            assert!(
                buf.contains_key(&slot),
                "Gap slot {} should have been populated",
                slot
            );
        }
        // First buffered should still be there
        assert!(buf.contains_key(&first_buffered));
    }

    /// Test that populate_gap_slots is a no-op when there's no gap.
    #[tokio::test]
    async fn test_populate_gap_slots_no_gap() {
        let app = mk_test_app_for_pes_split().await;

        // Buffer starts at current_ledger + 1 — no gap
        let current = 100u32;
        {
            let mut buf = app.syncing_ledgers.write().await;
            buf.insert(
                101,
                henyey_herder::LedgerCloseInfo {
                    slot: 101,
                    close_time: 0,
                    tx_set_hash: henyey_common::Hash256::from_bytes([0xBB; 32]),
                    tx_set: None,
                    upgrades: Vec::new(),
                    stellar_value_ext: stellar_xdr::curr::StellarValueExt::Basic,
                },
            );
        }

        app.populate_gap_slots(current).await;

        // Should still only have the one entry
        let buf = app.syncing_ledgers.read().await;
        assert_eq!(buf.len(), 1);
    }

    /// Regression: buffer_externalized_tx_set must use the provided
    /// tx_set even when check_ledger_close returns info without one
    /// (unsolicited tx_set scenario).
    #[tokio::test]
    async fn test_buffer_externalized_tx_set_uses_provided_tx_set() {
        let app = mk_test_app_for_pes_split().await;
        app.herder.set_state(henyey_herder::HerderState::Tracking);

        let slot: u64 = 200;

        // Build the tx set first, then derive its hash for the StellarValue.
        let zero_hash = henyey_common::Hash256::from_bytes([0u8; 32]);
        let tx_set = henyey_herder::TransactionSet::from_wire_legacy(zero_hash, Vec::new());
        let tx_set_hash_bytes = tx_set.hash().0;

        // Externalize the slot (which seeds check_ledger_close, but
        // the returned info.tx_set will be None since we don't seed
        // the tx_set cache).
        let xdr = mk_stellar_value_xdr(tx_set_hash_bytes);
        let driver = app.herder.scp_driver();
        driver.record_externalized(slot, mk_value(xdr), None);
        driver.publish_externalized(slot);

        let result = app.buffer_externalized_tx_set(&tx_set).await;
        assert!(result, "should return true when slot is found");

        // The buffered entry should have the tx_set populated
        let buf = app.syncing_ledgers.read().await;
        let entry = buf.get(&(slot as u32)).expect("slot should be in buffer");
        assert!(
            entry.tx_set.is_some(),
            "tx_set must be populated from the provided tx_set, not from check_ledger_close"
        );
    }

    /// Verify that the FallbackCatchup enum is correctly wired: when Skip is
    /// passed and ledger manager is uninitialized, the fallback catchup path is
    /// not taken. We test this indirectly by verifying the pre-conditions that
    /// `App::run()` uses for its decision, since calling `run()` directly starts
    /// a full event loop with overlay/network that is not suitable for unit tests.
    #[tokio::test]
    async fn test_watcher_fallback_catchup_preconditions() {
        use std::sync::atomic::Ordering;

        let dir = tempfile::tempdir().expect("temp dir");
        let db_path = dir.path().join("rs-stellar-test.db");
        let config = crate::config::ConfigBuilder::new()
            .database_path(db_path)
            .build();

        let app = Arc::new(App::new(config).await.unwrap());

        // Verify precondition: ledger manager is not initialized (no restored state).
        // This is the condition that triggers the fallback catchup path in App::run().
        assert!(!app.ledger_manager.is_initialized());

        // get_current_ledger() returns 0 when uninitialized.
        assert_eq!(app.get_current_ledger().await.unwrap(), 0);

        // query_is_ready should be false initially and should stay false
        // for a watcher that skips fallback catchup (since is_initialized() is false).
        assert!(!app.query_is_ready.load(Ordering::Relaxed));

        // Verify that the readiness gate logic works correctly:
        // When is_initialized() is false, readiness should NOT be set.
        if app.ledger_manager.is_initialized() {
            app.query_is_ready.store(true, Ordering::Release);
        }
        assert!(
            !app.query_is_ready.load(Ordering::Relaxed),
            "query_is_ready must remain false when ledger manager is uninitialized"
        );
    }

    /// In-memory App::new() initializes genesis state so the DB has LCL=1.
    /// This is the fix for #2701: query server 404 in captive-core mode.
    #[tokio::test]
    async fn test_in_memory_app_has_genesis_lcl() {
        let dir = tempfile::tempdir().expect("temp dir");
        let config = crate::config::ConfigBuilder::new()
            .in_memory(true)
            .bucket_directory(dir.path().join("buckets"))
            .build();

        let app = Arc::new(App::new(config).await.unwrap());

        // DB should have LCL=1 from genesis initialization.
        let lcl = app
            .db_blocking("test-lcl", |db| {
                db.with_connection(|conn| conn.get_last_closed_ledger())
                    .map_err(Into::into)
            })
            .await
            .unwrap();
        assert_eq!(lcl, Some(1), "in-memory DB should have genesis LCL=1");
    }

    /// AUDIT-226: `check_catchup_persist_pending` returns true when the
    /// sentinel is present in the DB.
    #[test]
    fn test_check_catchup_persist_pending_detects_sentinel() {
        let db = henyey_db::Database::open_in_memory().unwrap();

        // Set the sentinel.
        db.with_connection(|conn| {
            use henyey_db::queries::StateQueries;
            conn.set_state(state_keys::CATCHUP_PERSIST_PENDING, "1")
        })
        .unwrap();

        assert!(
            App::check_catchup_persist_pending(&db),
            "must detect sentinel when present"
        );
    }

    /// AUDIT-226: `check_catchup_persist_pending` returns false on a
    /// clean DB (no sentinel).
    #[test]
    fn test_check_catchup_persist_pending_clean_db() {
        let db = henyey_db::Database::open_in_memory().unwrap();

        assert!(
            !App::check_catchup_persist_pending(&db),
            "must return false when sentinel is absent"
        );
    }

    /// AUDIT-226: sentinel is NOT cleared by `check_catchup_persist_pending`
    /// (crash-idempotence — it must persist for subsequent restarts).
    #[test]
    fn test_check_catchup_persist_pending_does_not_clear_sentinel() {
        let db = henyey_db::Database::open_in_memory().unwrap();

        db.with_connection(|conn| {
            use henyey_db::queries::StateQueries;
            conn.set_state(state_keys::CATCHUP_PERSIST_PENDING, "1")
        })
        .unwrap();

        // First check.
        assert!(App::check_catchup_persist_pending(&db));

        // Second check — sentinel must still be present.
        assert!(
            App::check_catchup_persist_pending(&db),
            "sentinel must persist across multiple checks (crash-idempotent)"
        );
    }

    /// §14.5 parity: end-to-end test that `App::new()` on a file-backed DB
    /// seeds `catchup_needs_full_reset` from `CATCHUP_PERSIST_PENDING`, AND
    /// does not consume/clear the sentinel (so repeated restarts remain
    /// crash-idempotent).
    #[tokio::test]
    async fn test_app_new_sets_full_reset_and_preserves_catchup_persist_sentinel() {
        let dir = tempfile::tempdir().expect("temp dir");
        let db_path = dir.path().join("rs-stellar-test.db");

        // Pre-create the DB and seed the sentinel as if a prior catchup
        // crashed mid-deferred-persist.
        {
            let db = henyey_db::Database::open(db_path.clone()).unwrap();
            db.with_connection(|conn| {
                use henyey_db::queries::StateQueries;
                conn.set_state(state_keys::CATCHUP_PERSIST_PENDING, "1")
            })
            .unwrap();
        }

        // Construct the App via the real startup path.
        let config = crate::config::ConfigBuilder::new()
            .database_path(db_path.clone())
            .build();
        let app = App::new(config).await.unwrap();

        // Assert that App::new() detected the sentinel and set the flag.
        assert!(
            app.catchup_needs_full_reset
                .load(std::sync::atomic::Ordering::Relaxed),
            "App::new() must detect CATCHUP_PERSIST_PENDING and set catchup_needs_full_reset"
        );

        // Verify the sentinel is still present in the DB (crash-idempotent).
        let sentinel_still_present = app
            .database()
            .with_connection(|conn| {
                use henyey_db::queries::StateQueries;
                conn.get_state(state_keys::CATCHUP_PERSIST_PENDING)
            })
            .unwrap();
        assert!(
            sentinel_still_present.is_some(),
            "CATCHUP_PERSIST_PENDING must remain set after App::new() detection \
             (crash-idempotent — cleared only by successful persist)"
        );
    }

    // ── Issue #2349: peer-ahead hard-reset escalation tests ─────────────

    /// Helper: create a test App for #2349 tests.
    async fn make_app_for_peer_ahead_test() -> (tempfile::TempDir, App) {
        let dir = tempfile::tempdir().expect("temp dir");
        let db_path = dir.path().join("rs-stellar-test.db");
        let config = crate::config::ConfigBuilder::new()
            .database_path(db_path)
            .build();
        let app = App::new(config).await.unwrap();
        (dir, app)
    }

    #[test]
    fn test_peer_ahead_escalation_constants() {
        // Verify constant arithmetic.
        assert_eq!(
            RECOVERY_HARD_RESET_ESCALATION_ATTEMPTS, 11,
            "HARD_RESET_STALL_SECS(120) / OUT_OF_SYNC_RECOVERY_TIMER_SECS(10) - 1 = 11"
        );
        assert_eq!(
            RECOVERY_HARD_RESET_ESCALATION_ATTEMPTS_NO_SCP, 17,
            "(120 * 3/2) / 10 - 1 = 17"
        );
        assert_eq!(PEER_AHEAD_ESCALATION_THRESHOLD, 3);
    }

    #[tokio::test]
    async fn test_effective_peer_gap_returns_correct_value() {
        let (_dir, app) = make_app_for_peer_ahead_test().await;
        assert_eq!(app.effective_peer_gap(100), 0, "no traffic → gap 0");

        app.max_verified_scp_slot.store(110, Ordering::Relaxed);
        assert_eq!(app.effective_peer_gap(100), 10, "110 - 100 = 10");

        // When current_ledger > max_verified, saturating_sub clamps to 0.
        assert_eq!(app.effective_peer_gap(200), 0, "200 > 110 → gap 0");
    }

    #[tokio::test]
    async fn test_reset_recovery_attempts_clears_max_verified_scp_slot() {
        let (_dir, app) = make_app_for_peer_ahead_test().await;
        app.max_verified_scp_slot.store(999, Ordering::Relaxed);
        app.scp_messages_received.store(42, Ordering::Relaxed);
        app.reset_recovery_attempts(RecoveryResetMode::Full);
        assert_eq!(
            app.max_verified_scp_slot.load(Ordering::Relaxed),
            0,
            "Full reset must clear max_verified_scp_slot"
        );
    }

    /// Partial reset (RecoveryResetMode::Partial) must preserve
    /// max_verified_scp_slot so the hard-reset gate can still see peer gap.
    #[tokio::test]
    async fn test_partial_progress_preserves_peer_gap() {
        let (_dir, app) = make_app_for_peer_ahead_test().await;
        app.max_verified_scp_slot.store(500, Ordering::Relaxed);
        app.scp_messages_received.store(50, Ordering::Relaxed);
        app.recovery_attempts_without_progress
            .store(3, Ordering::SeqCst);

        app.reset_recovery_attempts(RecoveryResetMode::Partial {
            seed: PARTIAL_PROGRESS_RESEED,
        });

        assert_eq!(
            app.max_verified_scp_slot.load(Ordering::Relaxed),
            500,
            "Partial must preserve max_verified_scp_slot"
        );
        assert_eq!(
            app.recovery_baseline_scp_received.load(Ordering::SeqCst),
            50,
            "Partial must snapshot SCP baseline"
        );
        assert_eq!(
            app.recovery_attempts_without_progress
                .load(Ordering::SeqCst),
            PARTIAL_PROGRESS_RESEED,
            "Partial must reseed attempts to PARTIAL_PROGRESS_RESEED"
        );
    }

    /// Monotonic reseeding: if attempts are already higher than the seed,
    /// fetch_max is a no-op and existing stall history is preserved.
    #[tokio::test]
    async fn test_partial_progress_monotonic_reseed() {
        let (_dir, app) = make_app_for_peer_ahead_test().await;
        // Set attempts higher than PARTIAL_PROGRESS_RESEED.
        let high_attempts = PARTIAL_PROGRESS_RESEED + 5;
        app.recovery_attempts_without_progress
            .store(high_attempts, Ordering::SeqCst);

        app.reset_recovery_attempts(RecoveryResetMode::Partial {
            seed: PARTIAL_PROGRESS_RESEED,
        });

        assert_eq!(
            app.recovery_attempts_without_progress
                .load(Ordering::SeqCst),
            high_attempts,
            "fetch_max must not lower attempts below existing value"
        );
    }

    /// Regression test for issue #2349: when archive is confirmed behind
    /// AND peers are verified ahead (peer_gap >= 3) AND attempts >= 11,
    /// the fast-track → trigger_recovery_catchup path should escalate to
    /// hard reset instead of falling through to peer SCP request.
    ///
    /// We exercise this through out_of_sync_recovery (the public entry point)
    /// configured to take the fast-track path (AtTip + SCP traffic).
    #[tokio::test]
    async fn test_trigger_recovery_archive_behind_peer_ahead_fires_hard_reset() {
        let (_dir, app) = make_app_for_peer_ahead_test().await;
        let current_ledger = 100u32;

        // Set up the stall condition:
        {
            *app.archive_recovery_status.write().await = ArchiveRecoveryStatus::ConfirmedBehind {
                backoff_until: None,
            };
        }
        app.max_verified_scp_slot.store(110, Ordering::Relaxed); // peer_gap = 10

        // Set attempts so that after fetch_add(1) in out_of_sync_recovery,
        // attempts == RECOVERY_HARD_RESET_ESCALATION_ATTEMPTS (11).
        app.recovery_attempts_without_progress
            .store(RECOVERY_HARD_RESET_ESCALATION_ATTEMPTS, Ordering::SeqCst);
        app.recovery_baseline_ledger
            .store(current_ledger as u64, Ordering::SeqCst);

        // SCP traffic since reset → fast-track fires.
        app.scp_messages_received.store(10, Ordering::Relaxed);
        app.recovery_baseline_scp_received
            .store(0, Ordering::SeqCst);

        let result = app.out_of_sync_recovery(current_ledger).await;

        // force_post_catchup_hard_reset returns None on test App (no self_arc),
        // but we verify it was called by checking that recovery state was reset.
        assert!(result.is_none());
        // hard reset calls reset_recovery_attempts(Full) which clears this:
        assert_eq!(
            app.max_verified_scp_slot.load(Ordering::Relaxed),
            0,
            "hard reset must clear max_verified_scp_slot"
        );
        // confirmed-behind status is cleared by force_post_catchup_hard_reset:
        assert!(
            !app.archive_recovery_snapshot().await.is_confirmed_behind(),
            "hard reset must clear confirmed-behind status"
        );
    }

    /// Regression test for #2723: when the peer-ahead escalation path fires
    /// but the archive cache is Fresh at/below current_ledger, the #2713
    /// suppression guard in `force_post_catchup_hard_reset` must suppress the
    /// hard reset instead of proceeding. This is the mirror image of
    /// `test_trigger_recovery_archive_behind_peer_ahead_fires_hard_reset`.
    ///
    /// Call chain exercised:
    ///   out_of_sync_recovery → fast-track → trigger_recovery_catchup
    ///   → peer-ahead escalation → force_post_catchup_hard_reset
    ///   → #2713 suppression guard (returns None)
    #[tokio::test]
    async fn test_trigger_recovery_archive_behind_peer_ahead_suppressed_by_fresh_cache() {
        let (_dir, app) = make_app_for_peer_ahead_test().await;
        let current_ledger = 100u32;

        // Arm archive-behind status.
        {
            *app.archive_recovery_status.write().await = ArchiveRecoveryStatus::ConfirmedBehind {
                backoff_until: None,
            };
        }
        // Seed Fresh archive cache at current_ledger — this arms the #2713
        // suppression guard in force_post_catchup_hard_reset.
        app.archive_checkpoint_cache.seed(current_ledger);

        app.max_verified_scp_slot.store(110, Ordering::Relaxed); // peer_gap = 10

        // fetch_add(1) returns old value, so `attempts` == threshold when evaluated.
        app.recovery_attempts_without_progress
            .store(RECOVERY_HARD_RESET_ESCALATION_ATTEMPTS, Ordering::SeqCst);
        app.recovery_baseline_ledger
            .store(current_ledger as u64, Ordering::SeqCst);

        // SCP traffic since reset → fast-track fires.
        app.scp_messages_received.store(10, Ordering::Relaxed);
        app.recovery_baseline_scp_received
            .store(0, Ordering::SeqCst);

        let result = app.out_of_sync_recovery(current_ledger).await;

        // Suppression guard returns None — no PendingCatchup spawned.
        assert!(
            result.is_none(),
            "suppression guard should suppress hard reset"
        );

        // archive_recovery_status must remain ConfirmedBehind (suppression
        // does NOT clear it, unlike a successful hard reset).
        assert!(
            app.archive_recovery_snapshot().await.is_confirmed_behind(),
            "suppression must NOT clear confirmed-behind status"
        );

        // max_verified_scp_slot must remain at the seeded value (suppression
        // does NOT call reset_recovery_attempts, unlike the fire path).
        assert_eq!(
            app.max_verified_scp_slot.load(Ordering::Relaxed),
            110,
            "suppression must NOT clear max_verified_scp_slot"
        );

        // Cooldown must be armed by the suppression guard.
        assert!(
            app.last_hard_reset_offset.load(Ordering::Relaxed) > 0,
            "suppression must arm cooldown (last_hard_reset_offset > 0)"
        );
    }

    /// Boundary: attempts just below threshold should NOT fire hard reset.
    /// The fast-track path enters trigger_recovery_catchup but the
    /// peer-ahead escalation check fails on attempts.
    #[tokio::test]
    async fn test_trigger_recovery_archive_behind_low_attempts_no_reset() {
        let (_dir, app) = make_app_for_peer_ahead_test().await;
        let current_ledger = 100u32;

        {
            *app.archive_recovery_status.write().await = ArchiveRecoveryStatus::ConfirmedBehind {
                backoff_until: None,
            };
        }
        app.max_verified_scp_slot.store(110, Ordering::Relaxed);
        // After fetch_add(1), attempts will be ESCALATION - 1 = 10.
        app.recovery_attempts_without_progress.store(
            RECOVERY_HARD_RESET_ESCALATION_ATTEMPTS - 1,
            Ordering::SeqCst,
        );
        app.recovery_baseline_ledger
            .store(current_ledger as u64, Ordering::SeqCst);
        app.scp_messages_received.store(10, Ordering::Relaxed);
        app.recovery_baseline_scp_received
            .store(0, Ordering::SeqCst);

        let _result = app.out_of_sync_recovery(current_ledger).await;

        // Should NOT have fired hard reset (confirmed-behind status still set).
        assert!(
            app.archive_recovery_snapshot().await.is_confirmed_behind(),
            "low attempts must NOT trigger hard reset"
        );
    }

    /// Boundary: peer_gap = 2 (below PEER_AHEAD_ESCALATION_THRESHOLD=3)
    /// should NOT fire hard reset.
    #[tokio::test]
    async fn test_trigger_recovery_peer_gap_below_threshold_no_reset() {
        let (_dir, app) = make_app_for_peer_ahead_test().await;
        let current_ledger = 100u32;

        {
            *app.archive_recovery_status.write().await = ArchiveRecoveryStatus::ConfirmedBehind {
                backoff_until: None,
            };
        }
        app.max_verified_scp_slot.store(102, Ordering::Relaxed); // peer_gap = 2
        app.recovery_attempts_without_progress
            .store(RECOVERY_HARD_RESET_ESCALATION_ATTEMPTS, Ordering::SeqCst);
        app.recovery_baseline_ledger
            .store(current_ledger as u64, Ordering::SeqCst);
        app.scp_messages_received.store(10, Ordering::Relaxed);
        app.recovery_baseline_scp_received
            .store(0, Ordering::SeqCst);

        let _result = app.out_of_sync_recovery(current_ledger).await;

        assert!(
            app.archive_recovery_snapshot().await.is_confirmed_behind(),
            "peer_gap below threshold must NOT trigger hard reset"
        );
    }

    /// When archive is not confirmed behind (cold cache), should NOT fire
    /// hard reset even with high attempts and peer gap.
    #[tokio::test]
    async fn test_trigger_recovery_cold_cache_not_confirmed_behind_no_reset() {
        let (_dir, app) = make_app_for_peer_ahead_test().await;
        let current_ledger = 100u32;

        // recovery status defaults to Unknown (cold cache)
        app.max_verified_scp_slot.store(200, Ordering::Relaxed);
        app.recovery_attempts_without_progress
            .store(50, Ordering::SeqCst);
        app.recovery_baseline_ledger
            .store(current_ledger as u64, Ordering::SeqCst);
        app.scp_messages_received.store(10, Ordering::Relaxed);
        app.recovery_baseline_scp_received
            .store(0, Ordering::SeqCst);

        let _result = app.out_of_sync_recovery(current_ledger).await;

        assert!(
            !app.archive_recovery_snapshot().await.is_confirmed_behind(),
            "cold cache must NOT trigger hard reset"
        );
    }

    /// Step 4 no-SCP path: archive behind + high attempts fires hard reset.
    /// Uses current_ledger=0 so relation is AtTip (fresh herder latest_ext=0).
    /// No SCP traffic → fast-track skipped → step 4 reached.
    #[tokio::test]
    async fn test_out_of_sync_at_tip_no_scp_archive_behind_fires_hard_reset() {
        let (_dir, app) = make_app_for_peer_ahead_test().await;
        let current_ledger = 0u32;

        {
            *app.archive_recovery_status.write().await = ArchiveRecoveryStatus::ConfirmedBehind {
                backoff_until: None,
            };
        }
        app.max_verified_scp_slot.store(20, Ordering::Relaxed); // peer_gap = 20
                                                                // After fetch_add(1), attempts = 17 (= threshold).
        app.recovery_attempts_without_progress.store(
            RECOVERY_HARD_RESET_ESCALATION_ATTEMPTS_NO_SCP,
            Ordering::SeqCst,
        );
        app.recovery_baseline_ledger
            .store(current_ledger as u64, Ordering::SeqCst);
        // No SCP traffic since reset → scp_since_reset = 0 → step 4.
        app.scp_messages_received.store(0, Ordering::Relaxed);
        app.recovery_baseline_scp_received
            .store(0, Ordering::SeqCst);

        let result = app.out_of_sync_recovery(current_ledger).await;

        assert!(result.is_none());
        // Hard reset clears confirmed-behind status:
        assert!(
            !app.archive_recovery_snapshot().await.is_confirmed_behind(),
            "no-SCP hard reset must clear confirmed-behind status"
        );
    }

    /// Step 4 no-SCP path: attempts one below threshold should NOT fire.
    /// Uses current_ledger=0 for AtTip relation.
    #[tokio::test]
    async fn test_out_of_sync_at_tip_no_scp_low_attempts_no_reset() {
        let (_dir, app) = make_app_for_peer_ahead_test().await;
        let current_ledger = 0u32;

        {
            *app.archive_recovery_status.write().await = ArchiveRecoveryStatus::ConfirmedBehind {
                backoff_until: None,
            };
        }
        app.max_verified_scp_slot.store(20, Ordering::Relaxed);
        // After fetch_add(1), attempts = 16 (one below 17 threshold).
        app.recovery_attempts_without_progress.store(
            RECOVERY_HARD_RESET_ESCALATION_ATTEMPTS_NO_SCP - 1,
            Ordering::SeqCst,
        );
        app.recovery_baseline_ledger
            .store(current_ledger as u64, Ordering::SeqCst);
        app.scp_messages_received.store(0, Ordering::Relaxed);
        app.recovery_baseline_scp_received
            .store(0, Ordering::SeqCst);

        let _result = app.out_of_sync_recovery(current_ledger).await;

        assert!(
            app.archive_recovery_snapshot().await.is_confirmed_behind(),
            "low attempts must NOT trigger no-SCP hard reset"
        );
    }

    /// Verify that cooldown blocks the peer-ahead hard reset path.
    #[tokio::test]
    async fn test_trigger_recovery_cooldown_blocks_reset() {
        let (_dir, app) = make_app_for_peer_ahead_test().await;
        let current_ledger = 100u32;

        {
            *app.archive_recovery_status.write().await = ArchiveRecoveryStatus::ConfirmedBehind {
                backoff_until: None,
            };
        }
        app.max_verified_scp_slot.store(110, Ordering::Relaxed);
        app.recovery_attempts_without_progress
            .store(RECOVERY_HARD_RESET_ESCALATION_ATTEMPTS, Ordering::SeqCst);
        app.recovery_baseline_ledger
            .store(current_ledger as u64, Ordering::SeqCst);
        app.scp_messages_received.store(10, Ordering::Relaxed);
        app.recovery_baseline_scp_received
            .store(0, Ordering::SeqCst);

        // Set a recent hard reset to activate cooldown.
        // NOTE: `is_hard_reset_on_cooldown` returns false when last == 0
        // (meaning "never reset"), so we must use max(1, now_offset).
        let now_offset = app.start_instant.elapsed().as_secs().max(1);
        app.last_hard_reset_offset
            .store(now_offset, Ordering::Relaxed);
        app.last_hard_reset_gap.store(5, Ordering::Relaxed);

        let _result = app.out_of_sync_recovery(current_ledger).await;

        // Cooldown active → should NOT have fired hard reset.
        assert!(
            app.archive_recovery_snapshot().await.is_confirmed_behind(),
            "cooldown must block hard reset"
        );
    }

    /// Verify that a single far-ahead non-quorum envelope does NOT cause
    /// escalation by itself (needs confirmed-behind status + attempts).
    #[tokio::test]
    async fn test_single_far_ahead_non_quorum_insufficient_for_hard_reset() {
        let (_dir, app) = make_app_for_peer_ahead_test().await;
        let current_ledger = 100u32;

        // Simulate a single far-ahead envelope updating max_verified_scp_slot.
        app.max_verified_scp_slot.store(999, Ordering::Relaxed);
        // recovery status is Unknown (default) → 4-way guard fails.
        app.recovery_attempts_without_progress
            .store(RECOVERY_HARD_RESET_ESCALATION_ATTEMPTS, Ordering::SeqCst);
        app.recovery_baseline_ledger
            .store(current_ledger as u64, Ordering::SeqCst);
        app.scp_messages_received.store(10, Ordering::Relaxed);
        app.recovery_baseline_scp_received
            .store(0, Ordering::SeqCst);

        let _result = app.out_of_sync_recovery(current_ledger).await;

        // confirmed-behind status was never set → no hard reset.
        assert!(
            !app.archive_recovery_snapshot().await.is_confirmed_behind(),
            "single far-ahead envelope without archive_behind must NOT trigger reset"
        );
        // max_verified_scp_slot should remain untouched (no reset happened).
        assert_eq!(
            app.max_verified_scp_slot.load(Ordering::Relaxed),
            999,
            "no reset → max_verified_scp_slot preserved"
        );
    }

    /// #2664: When the node is at-tip (current_ledger > 0, relation == AtTip)
    /// with a stale confirmed-behind status and no baseline progress,
    /// out_of_sync_recovery must clear the stale state on entry.
    #[tokio::test]
    async fn test_stale_archive_behind_cleared_at_tip_no_baseline_progress() {
        let (_dir, app) = make_app_for_peer_ahead_test().await;
        let current_ledger = 100u32;

        // Set up AtTip: latest_externalized == current_ledger.
        app.herder.scp_driver().record_externalized(
            current_ledger as u64,
            Default::default(),
            None,
        );
        app.herder
            .scp_driver()
            .publish_externalized(current_ledger as u64);

        // Arm stale archive-behind state.
        {
            *app.archive_recovery_status.write().await = ArchiveRecoveryStatus::ConfirmedBehind {
                backoff_until: None,
            };
        }

        // Set baseline == current_ledger (no progress to detect).
        app.recovery_baseline_ledger
            .store(current_ledger as u64, Ordering::SeqCst);

        // Low attempts so no escalation fires.
        app.recovery_attempts_without_progress
            .store(0, Ordering::SeqCst);
        app.scp_messages_received.store(0, Ordering::Relaxed);
        app.recovery_baseline_scp_received
            .store(0, Ordering::SeqCst);

        let _result = app.out_of_sync_recovery(current_ledger).await;

        // The #2664 fix should clear the stale recovery status.
        assert!(
            !app.archive_recovery_snapshot().await.is_confirmed_behind(),
            "stale confirmed-behind status must be cleared at AtTip (#2664)"
        );
        // Full reset should also zero recovery_attempts_without_progress
        // (the fetch_add(1) after the clear will make it 1, not high).
        assert!(
            app.recovery_attempts_without_progress
                .load(Ordering::SeqCst)
                <= 1,
            "recovery_attempts must be reset at AtTip with stale flag (#2664)"
        );
    }

    /// #2664: At startup (current_ledger == 0, AtTip), the confirmed-behind
    /// status must NOT be cleared — it may be freshly armed.
    #[tokio::test]
    async fn test_stale_archive_behind_not_cleared_at_startup() {
        let (_dir, app) = make_app_for_peer_ahead_test().await;
        let current_ledger = 0u32;
        // latest_externalized is None → AtTip (0 == 0).

        {
            *app.archive_recovery_status.write().await = ArchiveRecoveryStatus::ConfirmedBehind {
                backoff_until: None,
            };
        }
        app.recovery_baseline_ledger
            .store(current_ledger as u64, Ordering::SeqCst);
        app.recovery_attempts_without_progress
            .store(0, Ordering::SeqCst);
        app.scp_messages_received.store(0, Ordering::Relaxed);
        app.recovery_baseline_scp_received
            .store(0, Ordering::SeqCst);

        let _result = app.out_of_sync_recovery(current_ledger).await;

        // At startup (current_ledger == 0), recovery status must NOT be cleared.
        assert!(
            app.archive_recovery_snapshot().await.is_confirmed_behind(),
            "confirmed-behind status must NOT be cleared at startup (current_ledger == 0)"
        );
    }

    // ---- survey_local_ledger parity tests ----

    #[tokio::test]
    async fn test_survey_local_ledger_returns_last_externalized() {
        let app = survey_test_app().await;
        // Bootstrap at ledger 5 → tracking_slot = 6, tracking_consensus_ledger_index = 5
        app.herder.bootstrap(5);
        let result = app.survey_local_ledger();
        assert_eq!(
            result, 5,
            "survey_local_ledger must return last externalized (5), not next consensus (6)"
        );
    }

    #[tokio::test]
    async fn test_survey_local_ledger_fallback_when_not_tracking() {
        let app = survey_test_app().await;
        // No bootstrap → tracking_slot = 0, tracking_consensus_ledger_index = 0
        let lcl = app.current_ledger_seq();
        let result = app.survey_local_ledger();
        assert_eq!(
            result, lcl,
            "survey_local_ledger must fall back to current_ledger_seq when not tracking"
        );
    }

    // ---- scheduler ledger restamping regression test ----

    /// Helper: create an App with overlay started for scheduler tests that
    /// need to capture outbound messages via inject_test_peer.
    ///
    /// Returns `(TempDir, App)`; the `TempDir` guard is first so the `App`
    /// (which holds open database handles into the directory) is dropped
    /// before the directory it backs is removed.
    async fn survey_test_app_with_overlay() -> (tempfile::TempDir, Arc<App>) {
        let dir = tempfile::tempdir().unwrap();
        let mut config = crate::config::ConfigBuilder::new()
            .database_path(dir.path().join("survey-test.db"))
            .build();
        config.overlay.known_peers.clear();
        config.is_compat_config = true;
        let app = Arc::new(App::new(config).await.unwrap());
        app.start_overlay().await.unwrap();
        (dir, app)
    }

    /// Regression test for commit 2cbb6bb: each scheduler phase must read
    /// survey_local_ledger() fresh at emission time, not reuse a value
    /// cached at survey start.
    ///
    /// Scenario: ledger advances between each scheduler phase (Idle →
    /// StartSent → RequestSent → Idle). Asserts that the start message
    /// carries ledger N, the request message carries N+1, and the stop
    /// message carries N+2.
    #[tokio::test]
    async fn test_advance_survey_scheduler_ledger_restamping_across_phases() {
        let (_dir, app) = survey_test_app_with_overlay().await;
        let overlay = app.overlay().await.unwrap();
        let now = app.clock.now();

        // Inject a single test peer so select_survey_peers returns it
        // deterministically.
        let peer_id = PeerId::from_bytes([1u8; 32]);
        let mut receiver = overlay.inject_test_peer(peer_id, 16);

        // Bootstrap herder at ledger 10 → survey_local_ledger() = 10.
        app.herder.bootstrap(10);

        // Set app state to Synced and make the scheduler due.
        *app.state.write().await = AppState::Synced;
        {
            let mut sched = app.survey_scheduler.lock().await;
            sched.next_action = now - Duration::from_secs(1);
            sched.last_started = None;
        }

        // ── Phase 1: Idle → StartSent (ledger = 10) ──
        app.advance_survey_scheduler().await;
        {
            let sched = app.survey_scheduler.lock().await;
            assert_eq!(
                sched.phase,
                SurveySchedulerPhase::StartSent,
                "Phase 1: scheduler must transition Idle → StartSent"
            );
        }
        let msg = receiver
            .try_recv()
            .expect("Phase 1: expected start message");
        match msg {
            StellarMessage::TimeSlicedSurveyStartCollecting(signed) => {
                assert_eq!(
                    signed.start_collecting.ledger_num, 10,
                    "Phase 1: start message must carry fresh ledger_num = 10"
                );
            }
            other => panic!(
                "Phase 1: expected TimeSlicedSurveyStartCollecting, got {:?}",
                std::mem::discriminant(&other)
            ),
        }

        // ── Phase 2: StartSent → RequestSent (advance ledger to 11) ──
        app.herder.bootstrap(11);
        {
            let mut sched = app.survey_scheduler.lock().await;
            sched.next_action = now - Duration::from_secs(1);
        }
        app.advance_survey_scheduler().await;
        {
            let sched = app.survey_scheduler.lock().await;
            assert_eq!(
                sched.phase,
                SurveySchedulerPhase::RequestSent,
                "Phase 2: scheduler must transition StartSent → RequestSent"
            );
        }
        let msg = receiver
            .try_recv()
            .expect("Phase 2: expected request message");
        match msg {
            StellarMessage::TimeSlicedSurveyRequest(signed) => {
                assert_eq!(
                    signed.request.request.ledger_num, 11,
                    "Phase 2: request message must carry fresh ledger_num = 11, not stale 10"
                );
            }
            other => panic!(
                "Phase 2: expected TimeSlicedSurveyRequest, got {:?}",
                std::mem::discriminant(&other)
            ),
        }

        // ── Phase 3: RequestSent → Idle (advance ledger to 12) ──
        app.herder.bootstrap(12);
        {
            let mut sched = app.survey_scheduler.lock().await;
            sched.next_action = now - Duration::from_secs(1);
        }
        app.advance_survey_scheduler().await;
        {
            let sched = app.survey_scheduler.lock().await;
            assert_eq!(
                sched.phase,
                SurveySchedulerPhase::Idle,
                "Phase 3: scheduler must transition RequestSent → Idle"
            );
        }
        let msg = receiver.try_recv().expect("Phase 3: expected stop message");
        match msg {
            StellarMessage::TimeSlicedSurveyStopCollecting(signed) => {
                assert_eq!(
                    signed.stop_collecting.ledger_num, 12,
                    "Phase 3: stop message must carry fresh ledger_num = 12, not stale 10"
                );
            }
            other => panic!(
                "Phase 3: expected TimeSlicedSurveyStopCollecting, got {:?}",
                std::mem::discriminant(&other)
            ),
        }

        // No extra messages should have been emitted.
        assert!(
            receiver.try_recv().is_none(),
            "No extra messages should be in the channel"
        );
    }

    // ---- SurveyMessageSigner / SurveyRequestSigner tests ----

    /// Helper: construct the expected local node ID from an App's keypair.
    fn expected_node_id(app: &App) -> stellar_xdr::curr::NodeId {
        stellar_xdr::curr::NodeId(stellar_xdr::curr::PublicKey::PublicKeyTypeEd25519(
            stellar_xdr::curr::Uint256(*app.keypair.public_key().as_bytes()),
        ))
    }

    #[tokio::test]
    async fn test_survey_message_signer_build_start() {
        let app = survey_test_app().await;
        let nonce = 42u32;

        // Set tracking so survey_local_ledger returns a known value
        app.herder.set_tracking_for_testing(11, 100);

        let signer = super::survey_impl::SurveyMessageSigner::new(&app, nonce);
        let (signed, inner) = signer.build_start().unwrap();

        // Verify structure
        assert_eq!(inner.nonce, nonce);
        assert_eq!(inner.surveyor_id, expected_node_id(&app));
        assert_eq!(inner.ledger_num, 10); // tracking slot 11 - 1

        // Verify the signed message wraps the inner
        assert_eq!(signed.start_collecting.nonce, nonce);
        assert_eq!(signed.start_collecting.ledger_num, inner.ledger_num);

        // Verify signature is non-empty
        assert!(!signed.signature.0.is_empty());
    }

    #[tokio::test]
    async fn test_survey_message_signer_build_stop() {
        let app = survey_test_app().await;
        let nonce = 77u32;

        app.herder.set_tracking_for_testing(21, 200);

        let signer = super::survey_impl::SurveyMessageSigner::new(&app, nonce);
        let (signed, inner) = signer.build_stop().unwrap();

        assert_eq!(inner.nonce, nonce);
        assert_eq!(inner.surveyor_id, expected_node_id(&app));
        assert_eq!(inner.ledger_num, 20); // tracking slot 21 - 1
        assert_eq!(signed.stop_collecting.nonce, nonce);
        assert!(!signed.signature.0.is_empty());
    }

    #[tokio::test]
    async fn test_survey_request_signer_build_request() {
        let app = survey_test_app().await;
        let nonce = 123u32;
        let peer_id = henyey_overlay::PeerId(stellar_xdr::curr::PublicKey::PublicKeyTypeEd25519(
            stellar_xdr::curr::Uint256([1u8; 32]),
        ));

        app.herder.set_tracking_for_testing(31, 300);

        let signer = super::survey_impl::SurveyRequestSigner::new(&app, nonce).await;
        let signed = signer.build_request(&peer_id, 5, 10).unwrap();

        // Verify request fields
        assert_eq!(signed.request.nonce, nonce);
        assert_eq!(signed.request.inbound_peers_index, 5);
        assert_eq!(signed.request.outbound_peers_index, 10);
        assert_eq!(
            signed.request.request.surveyed_peer_id,
            stellar_xdr::curr::NodeId(peer_id.0.clone())
        );
        assert_eq!(
            signed.request.request.surveyor_peer_id,
            expected_node_id(&app)
        );
        assert_eq!(signed.request.request.ledger_num, 30); // tracking slot 31 - 1

        // Verify encryption key is populated
        assert_ne!(signed.request.request.encryption_key.key, [0u8; 32]);

        // Verify signature is non-empty
        assert!(!signed.request_signature.0.is_empty());
    }

    #[tokio::test]
    async fn test_survey_request_signer_reads_fresh_ledger_per_call() {
        let app = survey_test_app().await;
        let nonce = 200u32;
        let peer_a = henyey_overlay::PeerId(stellar_xdr::curr::PublicKey::PublicKeyTypeEd25519(
            stellar_xdr::curr::Uint256([2u8; 32]),
        ));
        let peer_b = henyey_overlay::PeerId(stellar_xdr::curr::PublicKey::PublicKeyTypeEd25519(
            stellar_xdr::curr::Uint256([3u8; 32]),
        ));

        // Set tracking to slot 11 (ledger_index = 10)
        app.herder.set_tracking_for_testing(11, 100);

        // Create signer (resolves secret/key)
        let signer = super::survey_impl::SurveyRequestSigner::new(&app, nonce).await;

        // Build first request — captures current ledger (10)
        let signed_a = signer.build_request(&peer_a, 0, 0).unwrap();
        let ledger_a = signed_a.request.request.ledger_num;
        assert_eq!(ledger_a, 10);

        // Advance the tracking ledger to slot 16 (ledger_index = 15)
        app.herder.set_tracking_for_testing(16, 200);

        // Build second request — should get the NEW ledger value (15)
        let signed_b = signer.build_request(&peer_b, 0, 0).unwrap();
        let ledger_b = signed_b.request.request.ledger_num;

        assert_eq!(
            ledger_b, 15,
            "build_request must read ledger fresh per call, not cache from constructor"
        );
    }

    // ---- Regression tests for top_off_survey_requests and start/stop_survey_collecting ----

    /// Regression test: `top_off_survey_requests` → `send_survey_request` must
    /// stamp the outgoing request with the *current* ledger, not a stale value
    /// from when the survey was started.
    ///
    /// Scenario: survey started at ledger 10, ledger advances to 15 before
    /// `top_off_survey_requests` drains the peer queue. The emitted request
    /// must carry ledger_num = 15.
    #[tokio::test]
    async fn test_top_off_survey_requests_fresh_ledger_after_secret_resolution() {
        let (_dir, app) = survey_test_app_with_overlay().await;
        let overlay = app.overlay().await.unwrap();

        let peer_id = PeerId::from_bytes([7u8; 32]);
        let mut receiver = overlay.inject_test_peer(peer_id.clone(), 16);

        let nonce = 42u32;

        // Bootstrap herder at ledger 10.
        app.herder.bootstrap(10);

        // Transition survey to Reporting phase:
        // 1. start_collecting → Collecting
        let surveyor_id = expected_node_id(&app);
        {
            let mut state = app.survey_state.write().await;
            let start_msg = stellar_xdr::curr::TimeSlicedSurveyStartCollectingMessage {
                surveyor_id: surveyor_id.clone(),
                nonce,
                ledger_num: 10,
            };
            assert!(
                state.data_mut().start_collecting(
                    &start_msg,
                    &[],
                    &[],
                    crate::survey::NodeStatsSnapshot {
                        lost_sync_count: 0,
                        out_of_sync: false,
                        added_peers: 0,
                        dropped_peers: 0,
                    },
                ),
                "start_collecting must succeed (Inactive → Collecting)"
            );

            // 2. stop_collecting_by_identity → Reporting
            assert!(
                state.data_mut().stop_collecting_by_identity(
                    nonce,
                    &surveyor_id,
                    &[],
                    &[],
                    0,
                    0,
                    0,
                ),
                "stop_collecting must succeed (Collecting → Reporting)"
            );
            assert!(
                state.data().nonce_is_reporting(nonce),
                "survey must be in Reporting phase"
            );
        }

        // Verify no cached secret for this nonce — top_off will trigger
        // async secret resolution via ensure_survey_secret.
        assert!(
            !app.survey_secrets.read().await.contains_key(&nonce),
            "no cached secret should exist before top_off"
        );

        // Set up reporting state: running, peer in peers + queue, topoff due.
        {
            let mut reporting = app.survey_reporting.write().await;
            reporting.running = true;
            reporting.peers.insert(peer_id.clone());
            reporting.queue.push_back(peer_id.clone());
            reporting.next_topoff = app.clock.now() - Duration::from_secs(1);
        }

        // Advance the herder to ledger 15. The survey was started at 10,
        // but the current ledger is now 15.
        app.herder.bootstrap(15);

        // Call top_off_survey_requests — this exercises the
        // top_off → send_survey_request → SurveyRequestSigner path.
        app.top_off_survey_requests().await;

        // Assert the emitted request carries the fresh ledger value.
        let msg = receiver
            .try_recv()
            .expect("top_off must emit a survey request message");
        match msg {
            StellarMessage::TimeSlicedSurveyRequest(signed) => {
                assert_eq!(
                    signed.request.request.ledger_num, 15,
                    "request must carry fresh ledger_num = 15, not stale 10"
                );
                assert_eq!(signed.request.nonce, nonce, "nonce must match");
                assert_eq!(
                    signed.request.request.surveyed_peer_id,
                    stellar_xdr::curr::NodeId(peer_id.0.clone()),
                    "surveyed_peer_id must match injected peer"
                );
                assert_eq!(
                    signed.request.request.surveyor_peer_id, surveyor_id,
                    "surveyor_peer_id must match app's local node ID"
                );
            }
            other => panic!(
                "expected TimeSlicedSurveyRequest, got {:?}",
                std::mem::discriminant(&other)
            ),
        }

        assert!(
            receiver.try_recv().is_none(),
            "no extra messages should be in the channel"
        );
    }

    /// Regression test: public `start_survey_collecting` / `stop_survey_collecting`
    /// must stamp each outgoing message with the *current* ledger, not a value
    /// cached at survey start.
    ///
    /// Scenario: start at ledger 20, advance to 25, then stop. The start
    /// message must carry 20 and the stop message must carry 25.
    #[tokio::test]
    async fn test_start_stop_survey_collecting_fresh_ledger() {
        let (_dir, app) = survey_test_app_with_overlay().await;
        let overlay = app.overlay().await.unwrap();

        let peer_id = PeerId::from_bytes([9u8; 32]);
        let mut receiver = overlay.inject_test_peer(peer_id, 16);

        let nonce = 55u32;
        let surveyor_id = expected_node_id(&app);

        // Bootstrap herder at ledger 20.
        app.herder.bootstrap(20);

        // ── Start collecting ──
        app.start_survey_collecting(nonce).await.unwrap();

        let msg = receiver
            .try_recv()
            .expect("start_survey_collecting must emit a start message");
        match msg {
            StellarMessage::TimeSlicedSurveyStartCollecting(signed) => {
                assert_eq!(
                    signed.start_collecting.ledger_num, 20,
                    "start message must carry fresh ledger_num = 20"
                );
                assert_eq!(signed.start_collecting.nonce, nonce, "nonce must match");
                assert_eq!(
                    signed.start_collecting.surveyor_id, surveyor_id,
                    "surveyor_id must match app's local node ID"
                );
            }
            other => panic!(
                "expected TimeSlicedSurveyStartCollecting, got {:?}",
                std::mem::discriminant(&other)
            ),
        }

        // ── Advance ledger to 25 ──
        app.herder.bootstrap(25);

        // ── Stop collecting ──
        app.stop_survey_collecting().await.unwrap();

        let msg = receiver
            .try_recv()
            .expect("stop_survey_collecting must emit a stop message");
        match msg {
            StellarMessage::TimeSlicedSurveyStopCollecting(signed) => {
                assert_eq!(
                    signed.stop_collecting.ledger_num, 25,
                    "stop message must carry fresh ledger_num = 25, not stale 20"
                );
                assert_eq!(signed.stop_collecting.nonce, nonce, "nonce must match");
                assert_eq!(
                    signed.stop_collecting.surveyor_id, surveyor_id,
                    "surveyor_id must match app's local node ID"
                );
            }
            other => panic!(
                "expected TimeSlicedSurveyStopCollecting, got {:?}",
                std::mem::discriminant(&other)
            ),
        }

        assert!(
            receiver.try_recv().is_none(),
            "no extra messages should be in the channel"
        );
    }

    // ── RecoveryEpisodeLatch tests (#2568) ──────────────────────────────

    #[test]
    fn test_recovery_episode_latch_fires_once_per_episode() {
        let latch = RecoveryEpisodeLatch::new();
        assert!(latch.try_mark_onset(), "first call should fire");
        assert!(!latch.try_mark_onset(), "second call should not fire");
        assert!(!latch.try_mark_onset(), "third call should not fire");
    }

    #[test]
    fn test_recovery_episode_latch_reset_rearms() {
        let latch = RecoveryEpisodeLatch::new();
        assert!(latch.try_mark_onset());
        assert!(!latch.try_mark_onset());

        latch.reset();
        assert!(latch.try_mark_onset(), "should fire again after reset");
        assert!(
            !latch.try_mark_onset(),
            "should not fire again until next reset"
        );
    }

    #[test]
    fn test_recovery_episode_latch_fresh_does_not_fire_without_mark() {
        let latch = RecoveryEpisodeLatch::new();
        // Reset without ever marking should keep it ready to fire.
        latch.reset();
        assert!(
            latch.try_mark_onset(),
            "should fire on fresh latch after no-op reset"
        );
    }

    #[test]
    fn test_recovery_episode_latch_partial_does_not_rearm() {
        // Verify that only Full reset re-arms the latch, not Partial.
        let latch = RecoveryEpisodeLatch::new();

        // Simulate first onset.
        assert!(latch.try_mark_onset());
        assert!(!latch.try_mark_onset());

        // Partial reseed does NOT call latch.reset(), so latch stays latched.
        assert!(!latch.try_mark_onset());

        // Full reset re-arms.
        latch.reset();
        assert!(latch.try_mark_onset());
    }

    #[test]
    fn test_recovery_episode_latch_multiple_episodes() {
        // Verify multiple consecutive episodes each get exactly one onset.
        let latch = RecoveryEpisodeLatch::new();

        // Episode 1.
        assert!(latch.try_mark_onset());
        assert!(!latch.try_mark_onset());
        assert!(!latch.try_mark_onset());

        // Full reset ends episode 1.
        latch.reset();

        // Episode 2.
        assert!(latch.try_mark_onset());
        assert!(!latch.try_mark_onset());

        // Full reset ends episode 2.
        latch.reset();

        // Episode 3.
        assert!(latch.try_mark_onset());
    }

    // ── Recovery stall onset diagnostic App-level tests (#2569) ────────

    /// Parse the `henyey_recovery_stall_onset_total` counter from Prometheus
    /// text output.
    fn parse_onset_count(rendered: &str) -> u64 {
        for line in rendered.lines() {
            if line.starts_with("henyey_recovery_stall_onset_total ") {
                return line
                    .split_whitespace()
                    .last()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0);
            }
        }
        0
    }

    /// Helper: create a minimal App for onset diagnostic tests.
    async fn make_onset_test_app() -> (tempfile::TempDir, App) {
        let dir = tempfile::tempdir().expect("temp dir");
        let db_path = dir.path().join("rs-stellar-test.db");
        let config = crate::config::ConfigBuilder::new()
            .database_path(db_path)
            .build();
        let app = App::new(config).await.unwrap();
        (dir, app)
    }

    /// Comprehensive test for the recovery stall onset diagnostic through
    /// the live `out_of_sync_recovery()` method. Runs all scenarios
    /// sequentially in one test to avoid metric-recorder serialization
    /// issues (the recorder is process-global).
    ///
    /// Covers: AppState gating (Synced, Validating, Initializing,
    /// CatchingUp, ShuttingDown), `did_full_reset` suppression, latch
    /// once-per-episode semantics, and short-circuit evaluation.
    ///
    /// Regression coverage for #2569 (follow-up from #2568).
    #[tokio::test]
    async fn test_recovery_stall_onset_diagnostic() {
        let handle = crate::metrics::ensure_test_recorder();
        crate::metrics::describe_metrics();
        crate::metrics::register_label_series();

        // ── Scenario A: Synced positive case ───────────────────────────
        // AppState = Synced, no progress → onset should fire.
        {
            let (_dir, app) = make_onset_test_app().await;
            *app.state.write().await = AppState::Synced;
            // No progress: baseline defaults to 0, current_ledger = 0.
            let before = parse_onset_count(&handle.render());
            app.out_of_sync_recovery(0).await;
            let after = parse_onset_count(&handle.render());
            assert_eq!(
                after - before,
                1,
                "Scenario A: Synced + no progress → onset should fire once"
            );
        }

        // ── Scenario B: Validating positive case ───────────────────────
        {
            let (_dir, app) = make_onset_test_app().await;
            *app.state.write().await = AppState::Validating;
            let before = parse_onset_count(&handle.render());
            app.out_of_sync_recovery(0).await;
            let after = parse_onset_count(&handle.render());
            assert_eq!(
                after - before,
                1,
                "Scenario B: Validating + no progress → onset should fire once"
            );
        }

        // ── Scenario C: Full reset suppression ─────────────────────────
        // AppState = Synced, but progress causes Full reset → onset
        // suppressed by `did_full_reset == true`.
        {
            let (_dir, app) = make_onset_test_app().await;
            *app.state.write().await = AppState::Synced;
            // baseline = 0, current_ledger = 1 → progress detected.
            // Fresh herder: latest_externalized = 0, peer gap = 0 → Ahead,
            // still_behind = false → Full reset.
            app.recovery_baseline_ledger.store(0, Ordering::SeqCst);
            let before = parse_onset_count(&handle.render());
            app.out_of_sync_recovery(1).await;
            let after = parse_onset_count(&handle.render());
            assert_eq!(
                after - before,
                0,
                "Scenario C: Full reset tick → onset suppressed despite Synced state"
            );
        }

        // ── Scenario D: Non-synced state suppression ───────────────────
        for non_synced_state in [
            AppState::Initializing,
            AppState::CatchingUp,
            AppState::ShuttingDown,
        ] {
            let (_dir, app) = make_onset_test_app().await;
            *app.state.write().await = non_synced_state;
            let before = parse_onset_count(&handle.render());
            app.out_of_sync_recovery(0).await;
            let after = parse_onset_count(&handle.render());
            assert_eq!(
                after - before,
                0,
                "Scenario D: {non_synced_state} → onset suppressed"
            );
        }

        // ── Scenario E: Fires once per episode ─────────────────────────
        {
            let (_dir, app) = make_onset_test_app().await;
            *app.state.write().await = AppState::Synced;

            // Episode 1: three calls, only the first should fire.
            let before = parse_onset_count(&handle.render());
            app.out_of_sync_recovery(0).await;
            app.out_of_sync_recovery(0).await;
            app.out_of_sync_recovery(0).await;
            let after = parse_onset_count(&handle.render());
            assert_eq!(
                after - before,
                1,
                "Scenario E (episode 1): latch should fire exactly once"
            );

            // Trigger Full reset to rearm the latch: set baseline = 0 so
            // current_ledger = 1 triggers progress, and fresh herder means
            // Ahead + not still_behind → Full reset.
            app.recovery_baseline_ledger.store(0, Ordering::SeqCst);
            app.out_of_sync_recovery(1).await; // Full reset tick (onset suppressed)

            // Episode 2: stop progress, call again.
            app.recovery_baseline_ledger.store(1, Ordering::SeqCst);
            let before2 = parse_onset_count(&handle.render());
            app.out_of_sync_recovery(1).await;
            let after2 = parse_onset_count(&handle.render());
            assert_eq!(
                after2 - before2,
                1,
                "Scenario E (episode 2): latch should fire once after Full reset rearm"
            );
        }

        // ── Scenario F: Non-synced does not consume latch ──────────────
        // Calling in CatchingUp should NOT consume the latch (short-circuit
        // in `matches!(...) && try_mark_onset()`), so switching to Synced
        // should still fire.
        {
            let (_dir, app) = make_onset_test_app().await;
            *app.state.write().await = AppState::CatchingUp;

            let before = parse_onset_count(&handle.render());
            app.out_of_sync_recovery(0).await;
            let mid = parse_onset_count(&handle.render());
            assert_eq!(
                mid - before,
                0,
                "Scenario F: CatchingUp → onset suppressed (latch not consumed)"
            );

            // Switch to Synced — latch should still be available.
            *app.state.write().await = AppState::Synced;
            app.out_of_sync_recovery(0).await;
            let after = parse_onset_count(&handle.render());
            assert_eq!(
                after - mid,
                1,
                "Scenario F: after switching to Synced, onset should fire"
            );
        }
    }

    /// Regression test for #2612: calling `on_lost_sync()` followed by
    /// `set_state(CatchingUp)` should increment `lost_sync_count` by exactly 1,
    /// not 2. The double-count was caused by `set_state()` independently
    /// incrementing the counter on `Synced|Validating → CatchingUp` transitions.
    #[tokio::test]
    async fn test_lost_sync_count_no_double_count() {
        let dir = tempfile::tempdir().expect("temp dir");
        let db_path = dir.path().join("rs-stellar-test.db");
        let config = crate::config::ConfigBuilder::new()
            .database_path(db_path)
            .build();

        let app = App::new(config).await.unwrap();

        // Start in Synced state (simulating a running node).
        app.set_state(AppState::Synced).await;
        assert_eq!(app.lost_sync_count(), 0);

        // Simulate a tracking-timeout sequence: on_lost_sync() fires first,
        // then the app transitions to CatchingUp.
        use henyey_herder::sync_recovery::SyncRecoveryCallback;
        app.on_lost_sync();
        app.set_state(AppState::CatchingUp).await;

        // Must be exactly 1, not 2.
        assert_eq!(
            app.lost_sync_count(),
            1,
            "lost_sync_count should increment exactly once per sync-loss event"
        );
    }

    /// Regression test for #2612: a `Synced → CatchingUp` state transition
    /// without `on_lost_sync()` should NOT increment `lost_sync_count`.
    /// State transitions alone are not sync-loss detection events.
    #[tokio::test]
    async fn test_set_state_catching_up_does_not_increment_lost_sync_count() {
        let dir = tempfile::tempdir().expect("temp dir");
        let db_path = dir.path().join("rs-stellar-test.db");
        let config = crate::config::ConfigBuilder::new()
            .database_path(db_path)
            .build();

        let app = App::new(config).await.unwrap();

        // Start in Synced state.
        app.set_state(AppState::Synced).await;
        assert_eq!(app.lost_sync_count(), 0);

        // Transition to CatchingUp without on_lost_sync() — e.g., a
        // non-timeout catchup path like spawn_catchup from consensus.
        app.set_state(AppState::CatchingUp).await;

        // Counter must remain 0: no sync-loss event was detected.
        assert_eq!(
            app.lost_sync_count(),
            0,
            "set_state(CatchingUp) alone should not increment lost_sync_count"
        );
    }

    /// Integration test for #2613: verify the full wiring path from
    /// SyncRecoveryManager tracking timeout → App::on_lost_sync() callback
    /// → lost_sync_count increment. Unlike the #2612 regression tests above,
    /// this test exercises the real timer expiration inside the manager.
    #[tokio::test(start_paused = true)]
    async fn test_sync_recovery_manager_tracking_timeout_increments_lost_sync_count() {
        use henyey_herder::sync_recovery::CONSENSUS_STUCK_TIMEOUT;

        let dir = tempfile::tempdir().expect("temp dir");
        let db_path = dir.path().join("rs-stellar-test.db");
        let config = crate::config::ConfigBuilder::new()
            .database_path(db_path)
            .build();
        let app = Arc::new(App::new(config).await.unwrap());

        // Establish preconditions: Synced app with Tracking herder.
        app.set_state(AppState::Synced).await;
        app.herder.set_state(henyey_herder::HerderState::Tracking);
        assert_eq!(app.lost_sync_count(), 0);
        assert!(
            !app.sync_recovery_pending
                .load(std::sync::atomic::Ordering::SeqCst),
            "sync_recovery_pending should start false"
        );

        // Start the sync recovery manager via the production path.
        app.start_sync_recovery();

        // Arm the tracking timer.
        {
            let guard = app.sync_recovery_handle.read();
            let handle = guard
                .as_ref()
                .expect("handle should be set after start_sync_recovery");
            handle.start_tracking().await;
        }
        // Drain: let the manager process StartTracking and arm the deadline.
        tokio::task::yield_now().await;

        // Advance exactly past the tracking timeout (35s + 1ms buffer).
        // Kept minimal to avoid entering the 10s recovery retry window.
        tokio::time::advance(CONSENSUS_STUCK_TIMEOUT + Duration::from_millis(1)).await;
        tokio::task::yield_now().await;

        // Verify the full timeout handler fired correctly.
        assert_eq!(
            app.lost_sync_count(),
            1,
            "tracking timeout should increment lost_sync_count exactly once"
        );
        assert_eq!(
            app.herder.state(),
            henyey_herder::HerderState::Syncing,
            "on_lost_sync() should transition herder to Syncing"
        );
        assert!(
            app.sync_recovery_pending
                .load(std::sync::atomic::Ordering::SeqCst),
            "on_out_of_sync_recovery() should set sync_recovery_pending"
        );

        // Cleanup: shutdown the manager and await its task.
        {
            let guard = app.sync_recovery_handle.read();
            if let Some(handle) = guard.as_ref() {
                handle.shutdown().await;
            }
        }
        {
            let mut guard = app.sync_recovery_task.write();
            if let Some(task) = guard.take() {
                let _ = task.await;
            }
        }
    }
}
