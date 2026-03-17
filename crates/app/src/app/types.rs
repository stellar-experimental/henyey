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
use henyey_herder::Herder;
use henyey_overlay::{PeerId, ScpQueueCallback};
use stellar_xdr::curr::{
    Hash, LedgerUpgrade, ReadXdr, TopologyResponseBodyV2, TransactionEnvelope, UpgradeType,
};

use crate::config::AppConfig;
use crate::survey::SurveyPhase;
use henyey_ledger::TransactionSetVariant;

// ── Constants re-exported for sibling submodules ────────────────────────

pub(super) const PEER_TYPE_OUTBOUND: i32 = 1;
pub(super) const PEER_TYPE_PREFERRED: i32 = 2;
pub(super) const PEER_TYPE_INBOUND: i32 = 0;

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
    pub slot_is_nominating: Option<bool>,
    pub slot_is_externalized: Option<bool>,
    pub slot_ballot_phase: Option<String>,
    pub slot_ballot_round: Option<u32>,
    pub nomination_timeout_fires: u64,
    pub ballot_timeout_fires: u64,
    pub scp_messages_sent: u64,
    pub scp_messages_received: u64,
    pub consensus_trigger_attempts: u64,
    pub consensus_trigger_successes: u64,
    pub consensus_trigger_failures: u64,
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
}

impl std::fmt::Display for AppInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "henyey {}", self.version)?;
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
    pub slot_index: u64,
    pub is_externalized: bool,
    pub is_nominating: bool,
    pub ballot_phase: String,
    pub nomination_round: u32,
    pub ballot_round: Option<u32>,
    pub envelope_count: usize,
}

#[derive(Debug, Clone)]
pub struct SelfCheckResult {
    pub ok: bool,
    pub checked_ledgers: u32,
    pub last_checked_ledger: Option<u32>,
}

/// Application builder for more flexible initialization.
pub struct AppBuilder {
    config: Option<AppConfig>,
    config_path: Option<std::path::PathBuf>,
}

impl AppBuilder {
    /// Create a new application builder.
    pub fn new() -> Self {
        Self {
            config: None,
            config_path: None,
        }
    }

    /// Use the given configuration.
    pub fn with_config(mut self, config: AppConfig) -> Self {
        self.config = Some(config);
        self
    }

    /// Load configuration from a file.
    pub fn with_config_file(mut self, path: impl AsRef<std::path::Path>) -> Self {
        self.config_path = Some(path.as_ref().to_path_buf());
        self
    }

    /// Build the application.
    pub async fn build(self) -> anyhow::Result<super::App> {
        let config = if let Some(config) = self.config {
            config
        } else if let Some(path) = self.config_path {
            AppConfig::from_file_with_env(&path)?
        } else {
            AppConfig::default()
        };

        super::App::new(config).await
    }
}

impl Default for AppBuilder {
    fn default() -> Self {
        Self::new()
    }
}

// ── Internal types ─────────────────────────────────────────────────────

/// State for a ledger close running on a background thread.
///
/// Created by [`App::try_start_ledger_close`] and consumed by
/// [`App::handle_close_complete`] once the blocking close finishes.
pub(super) struct PendingLedgerClose {
    /// Join handle for the `spawn_blocking` task.
    pub handle: tokio::task::JoinHandle<std::result::Result<henyey_ledger::LedgerCloseResult, String>>,
    /// Sequence number being closed.
    pub ledger_seq: u32,
    /// The transaction set used for closing.
    pub tx_set: henyey_herder::TransactionSet,
    /// Variant of the tx set (classic or generalized).
    pub tx_set_variant: TransactionSetVariant,
    /// Close time for the ledger.
    pub close_time: u64,
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
pub(super) struct ConsensusStuckState {
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
    /// Whether we've already triggered catchup for this stuck state.
    /// Set to true when catchup is triggered, prevents repeated catchup attempts
    /// when archive has no newer checkpoint available.
    pub catchup_triggered: bool,
}

/// Actions to take when consensus is stuck.
#[derive(Debug, Clone, Copy)]
pub(super) enum ConsensusStuckAction {
    /// Wait for tx set to arrive.
    Wait,
    /// Attempt recovery (broadcast SCP + request state from peers).
    AttemptRecovery,
    /// Trigger catchup after timeout.
    TriggerCatchup,
}

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
            let hash256 = Hash256::from(hash.clone());
            self.remember(hash256, ledger_seq);
        }

        let start = hashes.len().saturating_sub(max_ops);
        for hash in hashes.iter().skip(start) {
            self.incoming.push_back(Hash256::from(hash.clone()));
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

// ── Free functions ─────────────────────────────────────────────────────

pub(super) fn build_generalized_tx_set(
    tx_set: &henyey_herder::TransactionSet,
) -> Option<stellar_xdr::curr::GeneralizedTransactionSet> {
    use stellar_xdr::curr::{
        GeneralizedTransactionSet, TransactionPhase, TransactionSetV1, TxSetComponent,
        TxSetComponentTxsMaybeDiscountedFee,
    };

    let component =
        TxSetComponent::TxsetCompTxsMaybeDiscountedFee(TxSetComponentTxsMaybeDiscountedFee {
            base_fee: None,
            txs: tx_set.transactions.clone().try_into().ok()?,
        });
    let phase = TransactionPhase::V0(vec![component].try_into().ok()?);
    Some(GeneralizedTransactionSet::V1(TransactionSetV1 {
        previous_ledger_hash: stellar_xdr::curr::Hash(tx_set.previous_ledger_hash.0),
        phases: vec![phase].try_into().ok()?,
    }))
}

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
                        TxSetComponent::TxsetCompTxsMaybeDiscountedFee(comp) => {
                            comp.txs.to_vec()
                        }
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

/// Map a `PeerType` to a DB peer-type integer, preserving any existing
/// preferred or outbound classification.
pub(super) fn map_peer_type(peer_type: henyey_overlay::PeerType, existing_type: i32) -> i32 {
    match peer_type {
        henyey_overlay::PeerType::Inbound => match existing_type {
            PEER_TYPE_PREFERRED => PEER_TYPE_PREFERRED,
            PEER_TYPE_OUTBOUND => PEER_TYPE_OUTBOUND,
            _ => PEER_TYPE_INBOUND,
        },
        henyey_overlay::PeerType::Outbound => {
            if existing_type == PEER_TYPE_PREFERRED {
                PEER_TYPE_PREFERRED
            } else {
                PEER_TYPE_OUTBOUND
            }
        }
    }
}

pub(super) fn update_peer_record(db: &henyey_db::Database, event: henyey_overlay::PeerEvent) {
    let now = current_epoch_seconds();
    match event {
        henyey_overlay::PeerEvent::Connected(addr, peer_type) => {
            let existing = db.load_peer(&addr.host, addr.port).ok().flatten();
            let existing_type = existing.map(|r| r.peer_type).unwrap_or(PEER_TYPE_INBOUND);
            let mapped = map_peer_type(peer_type, existing_type);
            let record = henyey_db::queries::PeerRecord::new(now, 0, mapped);
            let _ = db.store_peer(&addr.host, addr.port, record);
        }
        henyey_overlay::PeerEvent::Failed(addr, peer_type) => {
            let existing = db.load_peer(&addr.host, addr.port).ok().flatten();
            let mut failures = existing.map(|r| r.num_failures).unwrap_or(0);
            failures = failures.saturating_add(1);
            let backoff = compute_peer_backoff_secs(failures);
            let next_attempt = now.saturating_add(backoff);
            let existing_type = existing.map(|r| r.peer_type).unwrap_or(PEER_TYPE_INBOUND);
            let mapped = map_peer_type(peer_type, existing_type);
            let record = henyey_db::queries::PeerRecord::new(next_attempt, failures, mapped);
            let _ = db.store_peer(&addr.host, addr.port, record);
        }
    }
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
