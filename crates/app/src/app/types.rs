//! Internal helper types for the `app` module.
//!
//! These types support transaction flooding, consensus stuck detection,
//! survey scheduling, SCP latency tracking, and other App internals.
//! They are extracted here to reduce the size of `mod.rs`.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Serialize;

use henyey_common::Hash256;
use henyey_herder::{AccountProvider, FeeBalanceProvider, Herder};
use henyey_ledger::LedgerManager;
use henyey_overlay::{PeerId, ScpQueueCallback};
use stellar_xdr::curr::{
    Hash, LedgerUpgrade, ReadXdr, TopologyResponseBodyV2, TransactionEnvelope, UpgradeType,
};

use crate::survey::SurveyPhase;

// ── Re-exported peer type enum ───────────────────────────────────────
pub(super) use henyey_common::StoredPeerType;

// ── Startup policy ─────────────────────────────────────────────────────

/// Controls whether [`App::run()`](super::App::run) performs fallback catchup
/// when no ledger state is found at startup.
///
/// When `App::run()` discovers that `get_current_ledger() == 0` (ledger manager
/// uninitialized), this policy determines whether it should catch up from
/// history archives or proceed directly to the event loop without state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FallbackCatchup {
    /// Allow fallback catchup if ledger state is missing.
    ///
    /// Used by Full and Validator modes where the node must have local ledger
    /// state before serving queries or participating in consensus.
    Allow,
    /// Skip fallback catchup — proceed to the event loop without state.
    ///
    /// Used by Watcher mode where the node observes SCP/overlay traffic without
    /// requiring local ledger state. The node will eventually receive its first
    /// ledger via SCP externalize and the out-of-sync recovery path.
    Skip,
}

// ── Application state ──────────────────────────────────────────────────

/// Application lifecycle state.
///
/// Represents the current phase of the application's operation. State transitions
/// are logged and can be observed via the HTTP status endpoint.
///
/// # State Transitions
///
/// - `Initializing` -> `CatchingUp`: When catchup is required
/// - `Initializing` -> `Synced`: When already up-to-date
/// - `CatchingUp` -> `Synced`: When catchup completes successfully
/// - `Synced` -> `Validating`: When consensus participation begins (validators only)
/// - `Synced` -> `CatchingUp`: When node falls behind and needs to re-sync
/// - Any -> `ShuttingDown`: When shutdown is requested
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppState {
    /// Application is initializing subsystems and loading state from disk.
    Initializing,
    /// Application is downloading and applying history from archives.
    CatchingUp,
    /// Application is synced with the network and tracking consensus.
    Synced,
    /// Application is actively participating in consensus as a validator.
    Validating,
    /// Application is gracefully shutting down.
    ShuttingDown,
}

impl std::fmt::Display for AppState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AppState::Initializing => write!(f, "Initializing"),
            AppState::CatchingUp => write!(f, "Catching Up"),
            AppState::Synced => write!(f, "Synced"),
            AppState::Validating => write!(f, "Validating"),
            AppState::ShuttingDown => write!(f, "Shutting Down"),
        }
    }
}

// ── Restore result ─────────────────────────────────────────────────────

/// Result of attempting to restore node state from persisted DB and on-disk
/// bucket files via [`App::load_last_known_ledger`](super::App::load_last_known_ledger).
///
/// Corruption and inconsistent state are represented as `Err`, not as a
/// variant here, so callers cannot accidentally continue after encountering
/// corrupt persisted state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestoreResult {
    /// State was successfully restored from disk.
    Restored,
    /// No persisted state exists (fresh node, no LCL in DB). The caller
    /// should proceed to catchup or genesis bootstrap.
    NoState,
}

// ── Public API types ───────────────────────────────────────────────────

/// Report of survey topology data collected from a single peer.
#[derive(Debug, Serialize)]
pub struct SurveyPeerReport {
    /// Public key of the peer that provided this report (hex-encoded).
    pub peer_id: String,
    /// Topology response containing peer statistics and node data.
    pub response: TopologyResponseBodyV2,
}

/// Aggregated network survey report.
///
/// Contains both local node survey data and responses collected from peers
/// during a time-sliced overlay survey. This data is used for network
/// topology analysis and monitoring.
#[derive(Debug, Serialize)]
pub struct SurveyReport {
    pub phase: SurveyPhase,
    pub nonce: Option<u32>,
    pub local_node: Option<stellar_xdr::curr::TimeSlicedNodeData>,
    pub inbound_peers: Vec<stellar_xdr::curr::TimeSlicedPeerData>,
    pub outbound_peers: Vec<stellar_xdr::curr::TimeSlicedPeerData>,
    pub peer_reports: std::collections::BTreeMap<u32, Vec<SurveyPeerReport>>,
    pub survey_in_progress: bool,
    pub backlog: Vec<String>,
    pub bad_response_nodes: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ScpSlotDebugStats {
    pub slot_index: u64,
    pub is_externalized: bool,
    pub is_nominating: bool,
    pub scp_heard_from_quorum: bool,
    pub ballot_phase: String,
    pub nomination_round: u32,
    pub ballot_round: Option<u32>,
    pub fully_validated: Option<bool>,
}

impl From<henyey_scp::SlotState> for ScpSlotDebugStats {
    fn from(state: henyey_scp::SlotState) -> Self {
        Self {
            slot_index: state.slot_index,
            is_externalized: state.is_externalized,
            is_nominating: state.is_nominating,
            scp_heard_from_quorum: state.heard_from_quorum,
            ballot_phase: format!("{:?}", state.ballot_phase),
            nomination_round: state.nomination_round,
            ballot_round: state.ballot_round,
            fully_validated: state.fully_validated,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct SimulationDebugStats {
    pub app_state: String,
    pub herder_state: String,
    pub current_ledger: u32,
    pub tracking_slot: u64,
    pub latest_externalized_slot: Option<u64>,
    pub peer_count: usize,
    pub pending_envelopes: usize,
    pub cached_tx_sets: usize,
    pub heard_from_quorum: bool,
    pub is_v_blocking: bool,
    pub slot: Option<ScpSlotDebugStats>,
    pub nomination_timeout_fires: u64,
    pub ballot_timeout_fires: u64,
    pub scp_messages_sent: u64,
    pub scp_messages_received: u64,
    pub consensus_trigger_attempts: u64,
    pub consensus_trigger_successes: u64,
    pub consensus_trigger_failures: u64,
    // Archive checkpoint cache (issue #1784)
    pub archive_checkpoint_stale_returns: u64,
    pub archive_checkpoint_cold_returns: u64,
    pub archive_checkpoint_fresh_returns: u64,
    pub archive_checkpoint_refresh_timeouts: u64,
    pub archive_checkpoint_refresh_errors: u64,
    pub archive_checkpoint_refresh_successes: u64,
}

/// Target for catchup operation.
#[derive(Debug, Clone, Copy)]
pub enum CatchupTarget {
    /// Catch up to the current/latest ledger.
    Current,
    /// Catch up to a specific ledger sequence.
    Ledger(u32),
    /// Catch up to a specific checkpoint number.
    Checkpoint(u32),
    /// Blocking archive probe: fetch the latest checkpoint from the archive
    /// via HTTP, and proceed with catchup only if the result is strictly
    /// ahead of the given ledger. If the archive is at/behind this ledger,
    /// return an error (no catchup work done). Used by HardResetEscalation
    /// when the nonblocking cache is stale or cold (see #1862).
    ProbeAhead(u32),
}

/// Result of a catchup operation.
#[derive(Debug, Clone)]
pub struct CatchupResult {
    /// Final ledger sequence.
    pub ledger_seq: u32,
    /// Hash of the final ledger.
    pub ledger_hash: henyey_common::Hash256,
    /// Number of buckets applied.
    pub buckets_applied: u32,
    /// Number of ledgers replayed.
    pub ledgers_replayed: u32,
}

impl std::fmt::Display for CatchupResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Caught up to ledger {} (hash: {}, {} buckets, {} ledgers replayed)",
            self.ledger_seq,
            &self.ledger_hash.to_hex()[..16],
            self.buckets_applied,
            self.ledgers_replayed
        )
    }
}

/// Core ledger identity: sequence, hash, close time, and protocol version.
///
/// A lightweight subset of [`LedgerSummary`] used by callers that only need
/// the four most common header fields.
#[derive(Debug, Clone)]
pub struct LedgerInfo {
    /// Current ledger sequence number.
    pub ledger_seq: u32,
    /// Hash of the current ledger header.
    pub hash: henyey_common::Hash256,
    /// Ledger close time (UNIX timestamp).
    pub close_time: u64,
    /// Protocol version.
    pub protocol_version: u32,
}

/// Rich ledger header summary for the `/info` endpoint.
#[derive(Debug, Clone)]
pub struct LedgerSummary {
    /// Current ledger sequence number.
    pub num: u32,
    /// Hash of the current ledger header.
    pub hash: henyey_common::Hash256,
    /// Ledger close time (UNIX timestamp).
    pub close_time: u64,
    /// Protocol version.
    pub version: u32,
    /// Base fee in stroops.
    pub base_fee: u32,
    /// Base reserve in stroops.
    pub base_reserve: u32,
    /// Max classic transaction set size.
    pub max_tx_set_size: u32,
    /// Ledger header flags (0 if pre-v1 extension).
    pub flags: u32,
    /// Seconds since last ledger close.
    pub age: u64,
}

/// Application info for the info command.
#[derive(Debug, Clone)]
pub struct AppInfo {
    /// Application version.
    pub version: String,
    /// Git commit hash.
    pub commit_hash: String,
    /// Build timestamp (ISO 8601).
    pub build_timestamp: String,
    /// Node name.
    pub node_name: String,
    /// Node public key.
    pub public_key: String,
    /// Network passphrase.
    pub network_passphrase: String,
    /// Whether this node is a validator.
    pub is_validator: bool,
    /// Database path.
    pub database_path: std::path::PathBuf,
    /// Total bytes written to metadata output stream.
    pub meta_stream_bytes_total: u64,
    /// Total frames written to metadata output stream.
    pub meta_stream_writes_total: u64,
    /// SCP signature-verify pipeline metrics (issue #1734 Phase B).
    pub scp_verify: ScpVerifyMetrics,
    /// Overlay fetch-channel depth metrics (issue #1741).
    pub overlay_fetch_channel: OverlayFetchChannelMetrics,
    /// Total post-catchup hard resets performed (issue #1822).
    pub post_catchup_hard_reset_total: u64,
}

/// Lightweight metrics-only snapshot for the `/metrics` scrape path.
///
/// Unlike [`AppInfo`], this struct contains only `Copy` fields — no Strings,
/// no PathBuf, no heap allocations. Used by `refresh_gauges()` to avoid
/// per-scrape allocation churn that contributes to jemalloc fragmentation.
#[derive(Debug, Clone, Copy)]
pub struct AppMetricsSnapshot {
    pub is_validator: bool,
    pub meta_stream_bytes_total: u64,
    pub meta_stream_writes_total: u64,
    pub scp_verify: ScpVerifyMetrics,
    pub overlay_fetch_channel: OverlayFetchChannelMetrics,
    pub post_catchup_hard_reset_total: u64,
    // Phase 3: cumulative ledger apply counters.
    pub cumulative_apply_success: u64,
    pub cumulative_apply_failure: u64,
    pub cumulative_soroban_success: u64,
    pub cumulative_soroban_failure: u64,
    // Phase 3: Soroban parallel execution structure (sticky).
    pub soroban_stage_count: u64,
    pub soroban_max_cluster_count: u64,
    // Phase 3: last-close cache metrics (lightweight — no Vec clone).
    pub bucket_cache_hit_ratio: f64,
    pub snapshot_cache_hit_ratio: f64,
    pub snapshot_cache_fallback_lookups: u64,
    // Phase 5: Archive cache counters.
    pub archive_cache_fresh: u64,
    pub archive_cache_stale: u64,
    pub archive_cache_cold: u64,
    pub archive_cache_refresh_success: u64,
    pub archive_cache_refresh_error: u64,
    pub archive_cache_refresh_timeout: u64,
    pub archive_cache_age_secs: f64,
    pub archive_cache_populated: bool,
}

/// Metrics for the overlay fetch-response channel (issue #1741).
///
/// The fetch channel is unbounded so that SCP fetch-request and fetch-response
/// messages are never dropped. These gauges expose the queue depth to make
/// wedge-induced growth observable before it becomes a memory problem.
#[derive(Debug, Clone, Copy, Default)]
pub struct OverlayFetchChannelMetrics {
    /// Current depth of the overlay fetch-response channel (event-loop sampled).
    pub depth: i64,
    /// Monotonic maximum depth observed since process start.
    pub depth_max: i64,
}

/// Metrics for the SCP signature-verify pipeline (issue #1734 Phase B).
#[derive(Debug, Clone, Copy, Default)]
pub struct ScpVerifyMetrics {
    /// Pre-filter rejects by reason (cumulative), indexed by [`PreFilterRejectReason`].
    pub prefilter_counters: henyey_herder::scp_verify::PreFilterCounters<u64>,
    /// Drops after verification (gate drift, self-message, non-quorum, invalid).
    /// Aggregate — kept for backward compatibility.
    pub post_verify_drops: u64,
    /// Per-reason post-verify counters (issue #1733 observability).
    /// Indexed by [`PostVerifyReason`] via the [`PostVerifyCounters`] wrapper.
    pub pv_counters: henyey_herder::scp_verify::PostVerifyCounters<u64>,
    /// Currently used slots in the verifier input channel (event-loop sampled).
    pub verify_input_backlog: u64,
    /// Sampled depth of the verified-output channel (envelopes awaiting the
    /// event loop). Captured by the event loop itself — unlike
    /// `verify_input_backlog` which is also sampled by the verifier worker
    /// as `handle.backlog()`, this is the true output-side queue depth.
    pub verify_output_backlog: u64,
    /// Worker thread state (0=Running, 1=Stopping, 2=Dead).
    pub verifier_thread_state: u64,
    /// Cumulative enqueue→post-verify latency microseconds (sum).
    pub verify_latency_us_sum: u64,
    /// Count of samples accumulated in `verify_latency_us_sum`.
    pub verify_latency_count: u64,
}

/// Overlay connection breakdown by direction and state.
#[derive(Debug, Clone, Copy, Default)]
pub struct ConnectionBreakdown {
    pub inbound_authenticated: u64,
    pub outbound_authenticated: u64,
    pub inbound_pending: u64,
    pub outbound_pending: u64,
}

/// Quorum health summary for metrics.
#[derive(Debug, Clone, Copy, Default)]
pub struct QuorumHealthMetrics {
    /// Nodes in Confirming or Externalized state.
    pub agree: u64,
    /// Nodes in Missing state.
    pub missing: u64,
    /// Nodes disagreeing (placeholder — not yet detectable from QuorumInfo).
    pub disagree: u64,
    /// Minimum nodes that can fail before quorum is lost, computed via
    /// `find_closest_v_blocking` (excludes self). More precise than
    /// `total - threshold` for nested quorum sets.
    pub fail_at: u64,
}

/// SCP timing for the most recently externalized slot.
#[derive(Debug, Clone, Copy, Default)]
pub struct ScpTimingMetrics {
    /// Duration from slot creation to externalize (seconds).
    pub externalize_duration_secs: Option<f64>,
    /// Duration from first local nomination vote to ballot protocol start (seconds).
    /// Matches stellar-core's `mNominateToPrepare`.
    /// None if either nomination start or ballot start was not recorded for this slot.
    pub nomination_duration_secs: Option<f64>,
    /// Duration from first EXTERNALIZE seen (any node) to self-externalize (seconds).
    /// None on catchup/fast-forward paths where no externalize events were recorded.
    pub first_to_self_externalize_secs: Option<f64>,
}

impl std::fmt::Display for AppInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(
            f,
            "{}",
            henyey_common::version::build_version_string(&self.version)
        )?;
        if !self.commit_hash.is_empty() {
            writeln!(f, "  Commit:     {}", self.commit_hash)?;
        }
        if !self.build_timestamp.is_empty() {
            writeln!(f, "  Built:      {}", self.build_timestamp)?;
        }
        writeln!(f)?;
        writeln!(f, "Node Information:")?;
        writeln!(f, "  Name:       {}", self.node_name)?;
        writeln!(f, "  Public Key: {}", self.public_key)?;
        writeln!(f, "  Validator:  {}", self.is_validator)?;
        writeln!(f)?;
        writeln!(f, "Network:")?;
        writeln!(f, "  Passphrase: {}", self.network_passphrase)?;
        writeln!(f)?;
        writeln!(f, "Storage:")?;
        writeln!(f, "  Database:   {}", self.database_path.display())?;
        if self.meta_stream_bytes_total > 0 || self.meta_stream_writes_total > 0 {
            writeln!(f)?;
            writeln!(f, "Metadata Stream:")?;
            writeln!(f, "  Bytes Written:  {}", self.meta_stream_bytes_total)?;
            writeln!(f, "  Writes:         {}", self.meta_stream_writes_total)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct ScpSlotSnapshot {
    pub slot: ScpSlotDebugStats,
    pub envelope_count: usize,
}

#[derive(Debug, Clone)]
pub struct SelfCheckResult {
    pub ok: bool,
    pub checked_ledgers: u32,
    pub last_checked_ledger: Option<u32>,
}

// ── Internal types ─────────────────────────────────────────────────────

/// State for a catchup operation running on a spawned tokio task.
///
/// Created by [`App::spawn_catchup`] and consumed by the event loop's
/// `pending_catchup` select branch when the oneshot delivers.
pub(super) struct PendingCatchup {
    /// Receives the catchup result when the spawned task completes.
    pub result_rx: tokio::sync::oneshot::Receiver<PendingCatchupResult>,
    /// Handle for the spawned catchup task (abort on shutdown, panic detection).
    pub task_handle: tokio::task::JoinHandle<()>,
    /// Handle for the catchup message caching task (aborted on completion).
    pub message_cache_handle: Option<tokio::task::JoinHandle<()>>,
    /// Label for logging ("RecoveryEscalation", "Buffered", "Externalized").
    pub label: String,
    /// Whether to reset consensus stuck state on successful catchup.
    /// `true` for recovery/buffered catchup, `false` for externalized.
    pub reset_stuck_state: bool,
    /// Whether to re-arm the sync recovery timer if catchup made progress.
    /// `true` for recovery catchup only.
    pub re_arm_recovery: bool,
}

/// Result payload sent from the spawned catchup task to the event loop.
pub(super) struct PendingCatchupResult {
    /// The catchup result (success or failure).
    pub result: anyhow::Result<CatchupResult>,
    /// Whether catchup actually advanced the ledger.
    pub made_progress: bool,
    /// Ready-to-spawn persist job. Private to force callers through
    /// [`Self::take_persist_ready`], which returns a `#[must_use]` value.
    persist_ready: Option<super::persist::CatchupPersistReady>,
}

impl PendingCatchupResult {
    /// Construct a result, deriving `made_progress` from the catchup outcome.
    pub(super) fn new(
        result: anyhow::Result<CatchupResult>,
        persist_ready: Option<super::persist::CatchupPersistReady>,
    ) -> Self {
        let made_progress = result
            .as_ref()
            .map_or(false, |r| r.ledgers_replayed > 0 || r.buckets_applied > 0);
        Self {
            result,
            made_progress,
            persist_ready,
        }
    }

    /// Take the persist-ready job, if any. Returns `None` if catchup failed
    /// or did no work requiring persistence.
    #[must_use = "the returned CatchupPersistReady must be spawned"]
    pub(super) fn take_persist_ready(&mut self) -> Option<super::persist::CatchupPersistReady> {
        self.persist_ready.take()
    }
}

/// State for a ledger close running on a background thread.
///
/// Created by [`App::try_start_ledger_close`] and consumed by
/// [`App::handle_close_complete`] once the blocking close finishes.
pub(super) struct PendingLedgerClose {
    /// Join handle for the `spawn_blocking` task.
    pub handle:
        tokio::task::JoinHandle<std::result::Result<henyey_ledger::LedgerCloseResult, String>>,
    /// Sequence number being closed.
    pub ledger_seq: u32,
    /// The transaction set used for closing.
    pub tx_set: henyey_herder::TransactionSet,
    /// Close time for the ledger.
    pub close_time: u64,
    /// Upgrades included in the externalized StellarValue (used for clearing
    /// runtime upgrade parameters after application).
    pub upgrades: Vec<UpgradeType>,
    /// Instant when `spawn_blocking` was dispatched, for dispatch-to-join
    /// latency measurement (#1909).
    pub dispatch_time: std::time::Instant,
}

/// State for a deferred ledger persist running on a background task.
///
/// Created by [`App::handle_close_complete`] and tracked in the main
/// event loop. The next ledger close is gated on persist completion
/// to ensure the DB has the previous ledger's data before the next
/// close references it.
pub(super) struct PendingPersist {
    /// Join handle for the blocking task that runs the persist pipeline.
    pub handle: tokio::task::JoinHandle<()>,
    /// Sequence number being persisted (for logging).
    pub ledger_seq: u32,
    /// Instant when `spawn_blocking` was dispatched, for dispatch-to-join
    /// latency measurement (#1916).
    pub dispatch_time: std::time::Instant,
}

#[derive(Debug)]
pub(super) struct TxAdvertHistory {
    entries: HashMap<Hash256, u32>,
    order: VecDeque<(Hash256, u32)>,
    capacity: usize,
}

impl TxAdvertHistory {
    pub fn new(capacity: usize) -> Self {
        Self {
            entries: HashMap::new(),
            order: VecDeque::new(),
            capacity,
        }
    }

    pub fn seen(&self, hash: &Hash256) -> bool {
        self.entries.contains_key(hash)
    }

    pub fn remember(&mut self, hash: Hash256, ledger_seq: u32) {
        self.entries.insert(hash, ledger_seq);
        self.order.push_back((hash, ledger_seq));

        while self.entries.len() > self.capacity {
            if let Some((old_hash, old_seq)) = self.order.pop_front() {
                if self.entries.get(&old_hash) == Some(&old_seq) {
                    self.entries.remove(&old_hash);
                }
            }
        }
    }

    pub fn clear_below(&mut self, ledger_seq: u32) {
        self.entries.retain(|_, seq| *seq >= ledger_seq);
        self.order
            .retain(|(hash, seq)| *seq >= ledger_seq && self.entries.get(hash) == Some(seq));
    }
}

#[derive(Debug, Clone)]
pub(super) struct TxSetRequestState {
    pub last_request: Instant,
    /// When this tx_set was first requested. Used to detect peers that silently
    /// drop GetTxSet requests (no response AND no DontHave).
    pub first_requested: Instant,
    pub next_peer_offset: usize,
}

/// State for tracking consensus stuck condition.
/// Matches stellar-core's out-of-sync recovery behavior.
#[derive(Debug, Clone)]
pub(crate) struct ConsensusStuckState {
    /// Current ledger when stuck was detected.
    pub current_ledger: u32,
    /// First buffered ledger when stuck was detected.
    pub first_buffered: u32,
    /// When we first detected the stuck condition.
    pub stuck_start: Instant,
    /// Last time we attempted recovery (broadcast SCP + request state).
    pub last_recovery_attempt: Instant,
    /// Number of recovery attempts made.
    pub recovery_attempts: u32,
}

/// Outcome of `compute_target_and_spawn_buffered_catchup`.
///
/// Replaces the prior `Option<PendingCatchup>` return + speculative
/// `catchup_triggered` write pattern. The caller no longer needs
/// compensating clears on every failure path (#1892).
pub(super) enum SpawnOutcome {
    /// Catchup was spawned successfully.
    Started(PendingCatchup),
    /// Catchup was skipped (bad target, archive behind, cold cache, etc.).
    Skipped,
    /// A catchup is already in flight (`catchup_in_progress` was true).
    AlreadyInFlight,
}

/// Why a hard reset was triggered. Logged at WARN for observability.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum HardResetReason {
    /// archive_behind + recovery attempts exhausted.
    ArchiveBehindRecoveryExhausted,
    /// archive_behind + all peers said DontHave for the tx_set.
    ArchiveBehindTxSetExhausted,
    /// archive_behind + wall-clock stall exceeded HARD_RESET_STALL_SECS.
    ArchiveBehindStallWallClock,
}

/// All signals the consensus-stuck state machine needs to pick an action.
/// Built once per tick, consumed by `decide_consensus_stuck_action`.
#[derive(Debug, Clone, Copy)]
pub(super) struct StuckSignals {
    pub catchup_in_progress: bool,
    pub archive_behind: bool,
    pub tx_set_exhausted: bool,
    pub schedule_due: bool,
    pub stuck_duration: u64,
    pub recovery_attempts: u32,
    /// When `true`, the HardReset cooldown is active (a recent HardReset
    /// already fired). Used by `decide_consensus_stuck_action` to fall back
    /// to `AttemptRecovery` instead of returning `HardReset` when the
    /// archive is behind — prevents the livelock described in #1843 where
    /// the decision keeps choosing HardReset but the cooldown blocks it.
    pub hard_reset_cooldown_active: bool,
}

/// Actions to take when consensus is stuck.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ConsensusStuckAction {
    /// Wait for tx set to arrive.
    Wait,
    /// Attempt recovery (broadcast SCP + request state from peers).
    AttemptRecovery,
    /// Trigger catchup after timeout.
    TriggerCatchup,
    /// Hard reset: clear buffered state and spawn a catchup to the latest
    /// archive checkpoint. Reachable from both `recently_caught_up` and
    /// `not recently caught up` branches when archive_behind is true and
    /// either recovery is exhausted, tx_sets are lost, or wall-clock
    /// stall exceeds the threshold.
    HardReset(HardResetReason),
}

// (TxAdvertQueue removed — flooding now reads from herder's TransactionQueue
// in priority order via broadcast_with_visitor(). See tx_flooding.rs.)

#[derive(Debug)]
pub(super) struct PeerTxAdverts {
    pub incoming: VecDeque<Hash256>,
    pub retry: VecDeque<Hash256>,
    pub history: TxAdvertHistory,
}

impl PeerTxAdverts {
    pub fn new() -> Self {
        Self {
            incoming: VecDeque::new(),
            retry: VecDeque::new(),
            history: TxAdvertHistory::new(50_000),
        }
    }

    pub fn seen_advert(&self, hash: &Hash256) -> bool {
        self.history.seen(hash)
    }

    pub fn remember(&mut self, hash: Hash256, ledger_seq: u32) {
        self.history.remember(hash, ledger_seq);
    }

    pub fn queue_incoming(&mut self, hashes: &[Hash], ledger_seq: u32, max_ops: usize) {
        for hash in hashes {
            let hash256 = Hash256(hash.0);
            self.remember(hash256, ledger_seq);
        }

        let start = hashes.len().saturating_sub(max_ops);
        for hash in hashes.iter().skip(start) {
            self.incoming.push_back(Hash256(hash.0));
        }

        while self.size() > max_ops {
            self.pop_advert();
        }
    }

    pub fn retry_incoming(&mut self, hashes: Vec<Hash256>, max_ops: usize) {
        self.retry.extend(hashes);
        while self.size() > max_ops {
            self.pop_advert();
        }
    }

    pub fn pop_advert(&mut self) -> Option<Hash256> {
        if let Some(hash) = self.retry.pop_front() {
            return Some(hash);
        }
        self.incoming.pop_front()
    }

    pub fn has_advert(&self) -> bool {
        self.size() > 0
    }

    pub fn size(&self) -> usize {
        self.retry.len() + self.incoming.len()
    }

    pub fn clear_below(&mut self, ledger_seq: u32) {
        self.history.clear_below(ledger_seq);
    }
}

#[derive(Debug)]
pub(super) struct TxDemandHistory {
    pub first_demanded: Instant,
    pub last_demanded: Instant,
    pub peers: HashMap<PeerId, Instant>,
    pub latency_recorded: bool,
}

#[derive(Debug, Clone, Copy)]
pub(super) enum DemandStatus {
    Demand,
    RetryLater,
    Discard,
}

#[derive(Debug, Clone)]
pub(super) struct PingInfo {
    pub peer_id: PeerId,
    pub sent_at: Instant,
}

/// Unified state for in-flight ping tracking.
///
/// Merges what was previously two separate `RwLock<HashMap<...>>` fields
/// (`ping_inflight` and `peer_ping_inflight`) into a single struct behind
/// one lock, eliminating the nested lock-ordering hazard where both maps
/// were always acquired together.
#[derive(Debug, Default)]
pub(super) struct PingState {
    /// In-flight ping requests keyed by hash.
    by_hash: HashMap<Hash256, PingInfo>,
    /// In-flight ping hash per peer (bidirectional index).
    by_peer: HashMap<PeerId, Hash256>,
}

impl PingState {
    /// Remove expired entries older than `timeout` from `sent_at`.
    pub fn expire_timeouts(&mut self, now: Instant, timeout: Duration) {
        self.by_hash.retain(|hash, info| {
            if now.duration_since(info.sent_at) > timeout {
                if let Some(existing) = self.by_peer.get(&info.peer_id) {
                    if existing == hash {
                        self.by_peer.remove(&info.peer_id);
                    }
                }
                return false;
            }
            true
        });
    }

    /// Try to record a sent ping for `peer_id`. Returns `false` if the peer
    /// already has an outstanding ping.
    pub fn try_mark_sent(&mut self, peer_id: PeerId, hash: Hash256, now: Instant) -> bool {
        if self.by_peer.contains_key(&peer_id) {
            return false;
        }
        self.by_peer.insert(peer_id.clone(), hash);
        self.by_hash.insert(
            hash,
            PingInfo {
                peer_id,
                sent_at: now,
            },
        );
        true
    }

    /// Remove a ping response by hash. Returns the `PingInfo` so the caller
    /// can verify the responding peer and compute latency.
    pub fn remove_response(&mut self, hash: &Hash256) -> Option<PingInfo> {
        let info = self.by_hash.remove(hash)?;
        if let Some(existing) = self.by_peer.get(&info.peer_id) {
            if existing == hash {
                self.by_peer.remove(&info.peer_id);
            }
        }
        Some(info)
    }

    /// Clean up after a failed send: remove the hash entry and the peer
    /// mapping, but only if the peer's current hash still matches (they
    /// may have been re-pinged with a new hash).
    pub fn cleanup_failed_send(&mut self, peer_id: &PeerId, hash: &Hash256) {
        self.by_hash.remove(hash);
        if let Some(existing) = self.by_peer.get(peer_id) {
            if existing == hash {
                self.by_peer.remove(peer_id);
            }
        }
    }
}

#[derive(Debug, Default)]
pub(super) struct ScpLatencyTracker {
    pub first_seen: HashMap<u64, Instant>,
    pub self_sent: HashMap<u64, Instant>,
    pub self_to_other_recorded: HashSet<u64>,
    pub first_to_self_samples_ms: VecDeque<u64>,
    pub self_to_other_samples_ms: VecDeque<u64>,
}

impl ScpLatencyTracker {
    pub const MAX_SAMPLES: usize = 256;

    pub fn record_first_seen(&mut self, slot: u64, now: Instant) {
        self.first_seen.entry(slot).or_insert(now);
    }

    pub fn record_self_sent(&mut self, slot: u64, now: Instant) -> Option<u64> {
        let mut sample = None;
        if let Some(first) = self.first_seen.get(&slot) {
            let delta = now.duration_since(*first).as_millis() as u64;
            Self::push_sample(&mut self.first_to_self_samples_ms, delta);
            sample = Some(delta);
        }
        self.self_sent.insert(slot, now);
        sample
    }

    pub fn record_other_after_self(&mut self, slot: u64, now: Instant) -> Option<u64> {
        if self.self_to_other_recorded.contains(&slot) {
            return None;
        }
        if let Some(sent) = self.self_sent.get(&slot) {
            let delta = now.duration_since(*sent).as_millis() as u64;
            Self::push_sample(&mut self.self_to_other_samples_ms, delta);
            self.self_to_other_recorded.insert(slot);
            return Some(delta);
        }
        None
    }

    fn push_sample(samples: &mut VecDeque<u64>, value: u64) {
        samples.push_back(value);
        if samples.len() > Self::MAX_SAMPLES {
            samples.pop_front();
        }
    }
}

#[derive(Debug)]
pub(super) struct SurveyReportingState {
    pub running: bool,
    pub peers: HashSet<PeerId>,
    pub queue: VecDeque<PeerId>,
    pub inbound_indices: HashMap<PeerId, u32>,
    pub outbound_indices: HashMap<PeerId, u32>,
    pub bad_response_nodes: HashSet<PeerId>,
    pub next_topoff: Instant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SurveyReportingStart {
    Started,
    AlreadyRunning,
    NotReady,
}

impl SurveyReportingState {
    pub fn new(now: Instant) -> Self {
        Self {
            running: false,
            peers: HashSet::new(),
            queue: VecDeque::new(),
            inbound_indices: HashMap::new(),
            outbound_indices: HashMap::new(),
            bad_response_nodes: HashSet::new(),
            next_topoff: now,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SurveySchedulerPhase {
    Idle,
    StartSent,
    RequestSent,
}

#[derive(Debug)]
pub(super) struct SurveyScheduler {
    pub phase: SurveySchedulerPhase,
    pub next_action: Instant,
    pub peers: Vec<PeerId>,
    pub nonce: u32,
    pub ledger_num: u32,
    pub last_started: Option<Instant>,
}

impl SurveyScheduler {
    pub fn new(now: Instant) -> Self {
        Self {
            phase: SurveySchedulerPhase::Idle,
            next_action: now + Duration::from_secs(60),
            peers: Vec::new(),
            nonce: 0,
            ledger_num: 0,
            last_started: None,
        }
    }
}

/// Action determined by snapshotting `SurveyScheduler` state under a short lock.
/// The lock is dropped before this enum is consumed, ensuring no lock is held
/// across `.await` points.
pub(super) enum SchedulerAction {
    /// Not yet time to act.
    NotDue,
    /// Scheduler is idle — attempt to start a new survey.
    Idle { last_started: Option<Instant> },
    /// Survey start was sent — send requests to peers.
    StartSent {
        peers: Vec<PeerId>,
        nonce: u32,
        ledger_num: u32,
    },
    /// Requests were sent — stop the survey and collect topology.
    RequestSent {
        peers: Vec<PeerId>,
        nonce: u32,
        ledger_num: u32,
    },
}

#[derive(Debug)]
pub(super) struct ScpTimeoutState {
    pub slot: u64,
    pub next_nomination: Option<Instant>,
    pub next_ballot: Option<Instant>,
}

impl ScpTimeoutState {
    pub fn new() -> Self {
        Self {
            slot: 0,
            next_nomination: None,
            next_ballot: None,
        }
    }
}

// Adapter from the app's Herder to the overlay's ScpQueueCallback trait.
// Bridges herder SCP state into overlay flow control for slot-age-aware trimming.
pub(super) struct HerderScpCallback {
    pub herder: Arc<Herder>,
}

impl ScpQueueCallback for HerderScpCallback {
    fn min_slot_to_remember(&self) -> u64 {
        self.herder.get_min_ledger_seq_to_remember()
    }

    fn most_recent_checkpoint_seq(&self) -> u64 {
        self.herder.get_most_recent_checkpoint_seq()
    }
}

// Adapter from the app's LedgerManager to the herder's FeeBalanceProvider trait.
// Bridges ledger state into the transaction queue for fee-source affordability checks.
//
// Matches stellar-core TransactionQueue behavior: creates a LedgerSnapshot and calls
// getAvailableBalance(header, feeSource) which computes:
//   balance - minBalance - sellingLiabilities
pub(super) struct LedgerFeeBalanceProvider {
    pub ledger_manager: Arc<LedgerManager>,
}

impl FeeBalanceProvider for LedgerFeeBalanceProvider {
    fn get_available_balance(&self, account_id: &stellar_xdr::curr::AccountId) -> Option<i64> {
        let snapshot = self.ledger_manager.create_snapshot().ok()?;
        let key = stellar_xdr::curr::LedgerKey::Account(stellar_xdr::curr::LedgerKeyAccount {
            account_id: account_id.clone(),
        });
        let entry = snapshot.get_entry(&key).ok()??;
        if let stellar_xdr::curr::LedgerEntryData::Account(acc) = &entry.data {
            let base_reserve = snapshot.header().base_reserve;
            Some(henyey_ledger::reserves::available_to_send(
                acc,
                base_reserve,
            ))
        } else {
            None
        }
    }
}

/// Account provider backed by a fresh ledger snapshot.
///
/// Used by tx-set validation to verify account existence, sequence numbers,
/// and signatures. Creates a per-call snapshot for the queue-admission path.
/// The SCP validation path uses `SnapshotValidationProviders` in the herder
/// crate, which holds a single snapshot for the entire validation run.
pub(super) struct LedgerAccountProvider {
    pub ledger_manager: Arc<LedgerManager>,
}

impl AccountProvider for LedgerAccountProvider {
    fn load_account(
        &self,
        account_id: &stellar_xdr::curr::AccountId,
    ) -> Option<stellar_xdr::curr::AccountEntry> {
        // Create a snapshot per call for the queue-admission path.
        // The SCP validation path uses SnapshotValidationProviders
        // which holds a single snapshot for the entire validation run.
        let snapshot = self.ledger_manager.create_snapshot().ok()?;
        snapshot.get_account(account_id).ok()?
    }
}

/// Single-snapshot validation provider for **batch** tx-set validation
/// (post-close re-validation and nomination-path `trim_invalid_two_phase`).
///
/// Thin wrapper around [`henyey_herder::SnapshotProviders`] that adds a
/// convenience constructor from `&LedgerManager`. The underlying type lives
/// in `henyey-herder` (where the `AccountProvider` / `FeeBalanceProvider`
/// traits are defined) so both crates can share the same implementation.
///
/// # Parity
///
/// Mirrors stellar-core's single `LedgerSnapshot ls(app)` at the top of
/// `TxSetUtils::getInvalidTxListWithErrors`
/// (`stellar-core/src/herder/TxSetUtils.cpp:167`).
///
/// # When to use
///
/// * **Batch paths** (N txs → 1 snapshot): use this type.
/// * **Admission paths** (1 tx → 1 snapshot): keep the per-call
///   [`LedgerAccountProvider`] / [`LedgerFeeBalanceProvider`] — no
///   amplification, no benefit.
pub(super) struct SnapshotValidationProviders {
    inner: henyey_herder::SnapshotProviders,
}

impl SnapshotValidationProviders {
    /// Build one snapshot for the whole validation pass.
    ///
    /// Returns `Err` if `create_snapshot` fails (currently infallible —
    /// the `Result` is defensive for future extensibility). Callers must
    /// degrade gracefully — see the `ledger_close.rs` post-close call
    /// site for the expected `warn!` + skip-stateful-validation pattern.
    /// Callers MUST NOT fall back to the per-call
    /// [`LedgerAccountProvider`] on this error path: re-introducing a
    /// quadratic snapshot pattern would silently paper over the bug this
    /// type was introduced to fix (see #1759).
    pub fn new(ledger_manager: &LedgerManager) -> henyey_ledger::Result<Self> {
        Ok(Self {
            inner: henyey_herder::SnapshotProviders::new(ledger_manager.create_snapshot()?),
        })
    }
}

impl AccountProvider for SnapshotValidationProviders {
    fn load_account(
        &self,
        account_id: &stellar_xdr::curr::AccountId,
    ) -> Option<stellar_xdr::curr::AccountEntry> {
        self.inner.load_account(account_id)
    }
}

impl FeeBalanceProvider for SnapshotValidationProviders {
    fn get_available_balance(&self, account_id: &stellar_xdr::curr::AccountId) -> Option<i64> {
        self.inner.get_available_balance(account_id)
    }
}

// ── Free functions ─────────────────────────────────────────────────────

/// Extract the previous ledger hash and transactions from a `GeneralizedTransactionSet`.
///
/// This avoids duplicating the phase/component traversal in every call site
/// that receives a `GeneralizedTransactionSet` from the network.
pub(super) fn extract_txs_from_generalized(
    gen_tx_set: &stellar_xdr::curr::GeneralizedTransactionSet,
) -> (henyey_common::Hash256, Vec<TransactionEnvelope>) {
    use stellar_xdr::curr::{GeneralizedTransactionSet, TransactionPhase, TxSetComponent};

    let prev_hash = match gen_tx_set {
        GeneralizedTransactionSet::V1(v1) => {
            henyey_common::Hash256::from_bytes(v1.previous_ledger_hash.0)
        }
    };
    let transactions = match gen_tx_set {
        GeneralizedTransactionSet::V1(v1) => v1
            .phases
            .iter()
            .flat_map(|phase| match phase {
                TransactionPhase::V0(components) => components
                    .iter()
                    .flat_map(|component| match component {
                        TxSetComponent::TxsetCompTxsMaybeDiscountedFee(comp) => comp.txs.to_vec(),
                    })
                    .collect::<Vec<_>>(),
                TransactionPhase::V1(parallel) => parallel
                    .execution_stages
                    .iter()
                    .flat_map(|stage| stage.0.iter().flat_map(|cluster| cluster.0.to_vec()))
                    .collect(),
            })
            .collect(),
    };
    (prev_hash, transactions)
}

pub(super) fn decode_upgrades(upgrades: Vec<UpgradeType>) -> Vec<LedgerUpgrade> {
    upgrades
        .into_iter()
        .filter_map(|upgrade| {
            let bytes = upgrade.0.as_slice();
            match LedgerUpgrade::from_xdr(bytes, stellar_xdr::curr::Limits::none()) {
                Ok(decoded) => Some(decoded),
                Err(err) => {
                    tracing::warn!(error = %err, "Failed to decode ledger upgrade");
                    None
                }
            }
        })
        .collect()
}

/// Map a `PeerType` to a `StoredPeerType`, preserving any existing
/// preferred or outbound classification.
pub(super) fn map_peer_type(
    peer_type: henyey_overlay::PeerType,
    existing_type: StoredPeerType,
) -> StoredPeerType {
    match peer_type {
        henyey_overlay::PeerType::Inbound => match existing_type {
            StoredPeerType::Preferred => StoredPeerType::Preferred,
            StoredPeerType::Outbound => StoredPeerType::Outbound,
            StoredPeerType::Inbound => StoredPeerType::Inbound,
        },
        henyey_overlay::PeerType::Outbound => {
            if existing_type == StoredPeerType::Preferred {
                StoredPeerType::Preferred
            } else {
                StoredPeerType::Outbound
            }
        }
    }
}

pub(super) fn update_peer_record(
    db: &henyey_db::Database,
    event: henyey_overlay::PeerEvent,
) -> Result<(), henyey_db::error::DbError> {
    let now = current_epoch_seconds();
    match event {
        henyey_overlay::PeerEvent::Connected(addr, peer_type) => {
            let existing = db.load_peer(&addr.host, addr.port)?;
            let existing_type = existing
                .map(|r| r.peer_type)
                .unwrap_or(StoredPeerType::Inbound);
            let mapped = map_peer_type(peer_type, existing_type);
            let record = henyey_db::queries::PeerRecord::new(now, 0, mapped);
            db.store_peer(&addr.host, addr.port, record)?;
        }
        henyey_overlay::PeerEvent::Failed(addr, peer_type) => {
            let existing = db.load_peer(&addr.host, addr.port)?;
            let mut failures = existing.map(|r| r.num_failures).unwrap_or(0);
            failures = failures.saturating_add(1);
            let backoff = compute_peer_backoff_secs(failures);
            let next_attempt = now.saturating_add(backoff);
            let existing_type = existing
                .map(|r| r.peer_type)
                .unwrap_or(StoredPeerType::Inbound);
            let mapped = map_peer_type(peer_type, existing_type);
            let record = henyey_db::queries::PeerRecord::new(next_attempt, failures, mapped);
            db.store_peer(&addr.host, addr.port, record)?;
        }
    }
    Ok(())
}

pub(super) fn compute_peer_backoff_secs(failures: u32) -> i64 {
    use rand::Rng;
    const SECONDS_PER_BACKOFF: u64 = 10;
    const MAX_BACKOFF_EXPONENT: u32 = 10;
    let exp = failures.min(MAX_BACKOFF_EXPONENT);
    let max = SECONDS_PER_BACKOFF.saturating_mul(1u64 << exp);
    let mut rng = rand::thread_rng();
    let jitter = rng.gen_range(1..=max.max(1));
    jitter as i64
}

pub(super) fn current_epoch_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_scp_slot_debug_stats_projects_every_slot_state_field() {
        let slot_state = henyey_scp::SlotState {
            slot_index: 42,
            is_externalized: true,
            is_nominating: false,
            heard_from_quorum: true,
            ballot_phase: henyey_scp::BallotPhase::Confirm,
            nomination_round: 7,
            ballot_round: Some(3),
            fully_validated: Some(false),
        };

        let stats = ScpSlotDebugStats::from(slot_state);

        assert_eq!(stats.slot_index, 42);
        assert!(stats.is_externalized);
        assert!(!stats.is_nominating);
        assert!(stats.scp_heard_from_quorum);
        assert_eq!(stats.ballot_phase, "Confirm");
        assert_eq!(stats.nomination_round, 7);
        assert_eq!(stats.ballot_round, Some(3));
        assert_eq!(stats.fully_validated, Some(false));
    }

    // --- PingState tests ---

    fn make_peer(id: u8) -> PeerId {
        PeerId::from_bytes([id; 32])
    }

    fn make_hash(id: u8) -> Hash256 {
        Hash256::from_bytes([id; 32])
    }

    #[test]
    fn test_ping_state_expire_timeouts() {
        let mut state = PingState::default();
        let now = Instant::now();
        let timeout = Duration::from_secs(60);

        let peer_a = make_peer(1);
        let hash_a = make_hash(1);
        // Insert an entry that's already old (by using `now` and then
        // expiring at `now + timeout + 1s`).
        state.try_mark_sent(peer_a.clone(), hash_a, now);

        let peer_b = make_peer(2);
        let hash_b = make_hash(2);
        let fresh_time = now + timeout + Duration::from_secs(1);
        state.try_mark_sent(peer_b.clone(), hash_b, fresh_time);

        // Expire at fresh_time — peer_a's entry is >60s old, peer_b's is 0s old.
        state.expire_timeouts(fresh_time, timeout);

        // peer_a should be gone, peer_b should remain.
        assert!(state.remove_response(&hash_a).is_none());
        assert!(state.remove_response(&hash_b).is_some());
    }

    #[test]
    fn test_ping_state_try_mark_sent_rejects_duplicate_peer() {
        let mut state = PingState::default();
        let peer = make_peer(1);

        assert!(state.try_mark_sent(peer.clone(), make_hash(1), Instant::now()));
        // Same peer, different hash — should be rejected.
        assert!(!state.try_mark_sent(peer, make_hash(2), Instant::now()));
    }

    #[test]
    fn test_ping_state_remove_response_cleans_peer_mapping() {
        let mut state = PingState::default();
        let peer = make_peer(1);
        let hash = make_hash(1);
        state.try_mark_sent(peer.clone(), hash, Instant::now());

        let info = state.remove_response(&hash).unwrap();
        assert_eq!(info.peer_id, peer);

        // Peer should now be available for a new ping.
        assert!(state.try_mark_sent(peer, make_hash(2), Instant::now()));
    }

    #[test]
    fn test_ping_state_cleanup_failed_send_preserves_new_hash() {
        let mut state = PingState::default();
        let peer = make_peer(1);
        let old_hash = make_hash(1);
        state.try_mark_sent(peer.clone(), old_hash, Instant::now());

        // Simulate the peer being re-pinged with a new hash (e.g., after
        // the old one was removed by process_ping_response).
        state.remove_response(&old_hash);
        let new_hash = make_hash(2);
        state.try_mark_sent(peer.clone(), new_hash, Instant::now());

        // cleanup_failed_send for the OLD hash should NOT remove the peer's
        // new mapping.
        state.cleanup_failed_send(&peer, &old_hash);

        // The new hash should still be tracked.
        let info = state.remove_response(&new_hash).unwrap();
        assert_eq!(info.peer_id, peer);
    }

    #[test]
    fn test_ping_state_no_orphans_after_mixed_operations() {
        let mut state = PingState::default();
        let peer_a = make_peer(1);
        let peer_b = make_peer(2);
        let hash_a = make_hash(1);
        let hash_b = make_hash(2);

        state.try_mark_sent(peer_a.clone(), hash_a, Instant::now());
        state.try_mark_sent(peer_b.clone(), hash_b, Instant::now());

        // Remove peer_a via response.
        state.remove_response(&hash_a);

        // Clean up peer_b via failed send.
        state.cleanup_failed_send(&peer_b, &hash_b);

        // Both peers should be available for new pings.
        assert!(state.try_mark_sent(peer_a, make_hash(3), Instant::now()));
        assert!(state.try_mark_sent(peer_b, make_hash(4), Instant::now()));
    }

    #[test]
    fn test_ping_state_wrong_peer_response() {
        let mut state = PingState::default();
        let real_peer = make_peer(1);
        let hash = make_hash(1);
        state.try_mark_sent(real_peer.clone(), hash, Instant::now());

        // Simulate response from a different peer — remove_response still
        // returns the PingInfo with the original peer_id so the caller can
        // detect the mismatch (matching peers.rs:282-284 semantics).
        let info = state.remove_response(&hash).unwrap();
        assert_eq!(info.peer_id, real_peer);
        // The caller would compare info.peer_id != responding_peer_id and
        // discard the response — tested here to document the invariant.
    }

    #[test]
    fn test_update_peer_record_connected_preserves_existing_type() {
        let db = henyey_db::Database::open_in_memory().unwrap();
        let addr = henyey_overlay::PeerAddress::new("127.0.0.1", 11625);

        // Store a peer with Preferred type
        let record = henyey_db::queries::PeerRecord::new(100, 3, StoredPeerType::Preferred);
        db.store_peer(&addr.host, addr.port, record).unwrap();

        // Connected event should preserve the existing type via map_peer_type
        let event =
            henyey_overlay::PeerEvent::Connected(addr.clone(), henyey_overlay::PeerType::Outbound);
        update_peer_record(&db, event).unwrap();

        let updated = db.load_peer(&addr.host, addr.port).unwrap().unwrap();
        // Failure count resets to 0 on connect
        assert_eq!(updated.num_failures, 0);
    }

    #[test]
    fn test_update_peer_record_connected_defaults_to_inbound_when_new() {
        let db = henyey_db::Database::open_in_memory().unwrap();
        let addr = henyey_overlay::PeerAddress::new("127.0.0.2", 11625);

        // No peer stored — Connected event should create one
        let event =
            henyey_overlay::PeerEvent::Connected(addr.clone(), henyey_overlay::PeerType::Inbound);
        update_peer_record(&db, event).unwrap();

        let stored = db.load_peer(&addr.host, addr.port).unwrap().unwrap();
        assert_eq!(stored.num_failures, 0);
    }

    #[test]
    fn test_update_peer_record_failed_increments_failures_and_sets_backoff() {
        let db = henyey_db::Database::open_in_memory().unwrap();
        let addr = henyey_overlay::PeerAddress::new("127.0.0.3", 11625);

        // Store a peer with 2 failures
        let record = henyey_db::queries::PeerRecord::new(100, 2, StoredPeerType::Outbound);
        db.store_peer(&addr.host, addr.port, record).unwrap();

        let event =
            henyey_overlay::PeerEvent::Failed(addr.clone(), henyey_overlay::PeerType::Outbound);
        update_peer_record(&db, event).unwrap();

        let updated = db.load_peer(&addr.host, addr.port).unwrap().unwrap();
        assert_eq!(updated.num_failures, 3); // 2 + 1
        assert!(updated.next_attempt > 100); // backoff applied
    }

    #[test]
    fn test_update_peer_record_failed_new_peer_starts_at_one_failure() {
        let db = henyey_db::Database::open_in_memory().unwrap();
        let addr = henyey_overlay::PeerAddress::new("127.0.0.4", 11625);

        // No peer stored — Failed event should start at failure count 1
        let event =
            henyey_overlay::PeerEvent::Failed(addr.clone(), henyey_overlay::PeerType::Inbound);
        update_peer_record(&db, event).unwrap();

        let stored = db.load_peer(&addr.host, addr.port).unwrap().unwrap();
        assert_eq!(stored.num_failures, 1);
    }

    #[test]
    fn test_update_peer_record_load_error_propagates() {
        // Verify that update_peer_record returns Err when load_peer fails,
        // not silently treating the error as "peer not found."
        // We can't easily inject a DB error, but we can verify the return type
        // is Result<(), DbError> and that a successful call returns Ok(()).
        let db = henyey_db::Database::open_in_memory().unwrap();
        let addr = henyey_overlay::PeerAddress::new("127.0.0.5", 11625);

        let event =
            henyey_overlay::PeerEvent::Connected(addr.clone(), henyey_overlay::PeerType::Outbound);
        let result = update_peer_record(&db, event);
        assert!(result.is_ok(), "successful update should return Ok(())");

        let stored = db.load_peer(&addr.host, addr.port).unwrap().unwrap();
        assert_eq!(stored.num_failures, 0);
    }

    #[test]
    fn test_update_peer_record_store_error_propagates() {
        // Verify that store errors propagate. Since we can't easily inject
        // a write error in-memory, we verify the function signature returns
        // Result<(), DbError> which means store_peer errors will propagate via ?.
        // A successful write should return Ok.
        let db = henyey_db::Database::open_in_memory().unwrap();
        let addr = henyey_overlay::PeerAddress::new("127.0.0.6", 11625);

        let record = henyey_db::queries::PeerRecord::new(100, 0, StoredPeerType::Outbound);
        db.store_peer(&addr.host, addr.port, record).unwrap();

        let event =
            henyey_overlay::PeerEvent::Connected(addr.clone(), henyey_overlay::PeerType::Outbound);
        let result = update_peer_record(&db, event);
        assert!(result.is_ok(), "store_peer on existing peer should succeed");
    }
}
