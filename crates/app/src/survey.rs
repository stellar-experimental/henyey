//! Survey data manager for time-sliced overlay network surveys.
//!
//! This module implements the time-sliced survey protocol used to collect
//! network topology data from Stellar nodes. Surveys provide visibility into:
//!
//! - Network connectivity (peer counts, connection directions)
//! - Message latency between nodes
//! - Bandwidth usage and message statistics
//! - Node health indicators (sync status, dropped peers)
//!
//! # Survey Phases
//!
//! A survey operates in two phases:
//!
//! 1. **Collecting** ([`SurveyPhase::Collecting`]): Nodes accumulate statistics
//!    about their peers over a time window (up to 30 minutes)
//!
//! 2. **Reporting** ([`SurveyPhase::Reporting`]): The surveyor requests topology
//!    data from participating nodes (up to 3 hours)
//!
//! # Protocol Flow
//!
//! ```text
//! Surveyor                    Nodes
//!    |                          |
//!    |--- StartCollecting ----->|  Begin accumulating stats
//!    |         ...              |  (time passes)
//!    |--- StopCollecting ------>|  Finalize stats, enter reporting
//!    |                          |
//!    |<-- TopologyRequest ----->|  Exchange peer data
//!    |<-- TopologyResponse -----|
//! ```
//!
//! # Security
//!
//! - Surveys are authenticated using node signatures
//! - Nodes can configure allowed surveyor keys
//! - Rate limiting prevents abuse

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use henyey_herder::Herder;
use henyey_ledger::LedgerManager;
use henyey_overlay::{PeerId, PeerSnapshot};
use serde::Serialize;
use stellar_xdr::{
    NodeId, PeerStats, SurveyMessageCommandType, SurveyRequestMessage, SurveyResponseMessage,
    TimeSlicedNodeData, TimeSlicedPeerData, TimeSlicedPeerDataList, TimeSlicedSurveyRequestMessage,
    TimeSlicedSurveyStartCollectingMessage, TimeSlicedSurveyStopCollectingMessage,
    TopologyResponseBodyV2,
};

/// Maximum duration for the collecting phase of a time-sliced survey.
/// Matches stellar-core `SurveyManager.cpp` `COLLECTING_PHASE_MAX_DURATION`.
const COLLECTING_PHASE_MAX_DURATION: Duration = Duration::from_secs(30 * 60);
/// Maximum duration for the reporting phase of a time-sliced survey.
/// Matches stellar-core `SurveyManager.cpp` `REPORTING_PHASE_MAX_DURATION`.
const REPORTING_PHASE_MAX_DURATION: Duration = Duration::from_secs(3 * 60 * 60);
/// Default number of histogram samples for latency measurements.
/// Matches stellar-core `SurveyManager::NUM_HISTOGRAM_SAMPLES`.
const DEFAULT_HISTOGRAM_SAMPLES: usize = 1024;
/// Maximum number of peers included in a time-sliced survey response.
/// Matches stellar-core `SurveyManager::NUM_SURVEYED_PEERS`.
const TIME_SLICED_PEERS_MAX: usize = 25;

// --- Ledger Source ---

/// Trait for providing the current ledger number to the survey message limiter.
///
/// Mirrors stellar-core's internal `mApp.getHerder().trackingConsensusLedgerIndex()` call
/// in `SurveyMessageLimiter::surveyLedgerNumValid()` (SurveyMessageLimiter.cpp:195-200).
pub(crate) trait LedgerSource: Send + Sync + std::fmt::Debug {
    fn current_ledger(&self) -> u32;
}

/// Production ledger source using herder + ledger manager.
///
/// Reads the tracking consensus ledger index, falling back to last-closed-ledger (LCL)
/// when the herder is in boot/syncing state. The boot-state fallback is a henyey-specific
/// divergence — stellar-core asserts non-boot in `trackingConsensusLedgerIndex()`.
pub(crate) struct HerderLedgerSource {
    herder: Arc<Herder>,
    ledger_manager: Arc<LedgerManager>,
}

impl std::fmt::Debug for HerderLedgerSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HerderLedgerSource").finish_non_exhaustive()
    }
}

impl Clone for HerderLedgerSource {
    fn clone(&self) -> Self {
        Self {
            herder: self.herder.clone(),
            ledger_manager: self.ledger_manager.clone(),
        }
    }
}

impl HerderLedgerSource {
    pub fn new(herder: Arc<Herder>, ledger_manager: Arc<LedgerManager>) -> Self {
        Self {
            herder,
            ledger_manager,
        }
    }
}

impl LedgerSource for HerderLedgerSource {
    fn current_ledger(&self) -> u32 {
        let tracking = self.herder.tracking_consensus_ledger_index();
        if tracking.is_boot() {
            self.ledger_manager.current_ledger_seq()
        } else {
            tracking.as_u32()
        }
    }
}

/// Current phase of a time-sliced survey.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum SurveyPhase {
    /// Actively collecting peer statistics during the survey window.
    Collecting,
    /// Survey collection complete; responding to topology requests.
    Reporting,
    /// No survey is active.
    Inactive,
}

/// Rate limiter and deduplication tracker for survey messages.
///
/// Prevents survey abuse by:
/// - Limiting the number of survey requests per ledger per surveyor
/// - Tracking which request/response pairs have been processed
/// - Ignoring messages with stale ledger numbers
///
/// Reads the current ledger internally via [`LedgerSource`], matching
/// stellar-core's `SurveyMessageLimiter` which reads from `mApp` internally.
pub(crate) struct SurveyMessageLimiter {
    /// Number of ledgers after which messages are considered stale.
    num_ledgers_before_ignore: u32,
    /// Maximum requests allowed per surveyor per ledger.
    max_request_limit: u32,
    /// Tracks (ledger -> surveyor -> surveyed -> seen) for deduplication.
    record_map: BTreeMap<u32, HashMap<NodeId, HashMap<NodeId, bool>>>,
    /// Source for the current ledger number.
    ledger_source: Box<dyn LedgerSource>,
}

impl std::fmt::Debug for SurveyMessageLimiter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SurveyMessageLimiter")
            .field("num_ledgers_before_ignore", &self.num_ledgers_before_ignore)
            .field("max_request_limit", &self.max_request_limit)
            .field("record_map_len", &self.record_map.len())
            .field("ledger_source", &self.ledger_source)
            .finish()
    }
}

impl SurveyMessageLimiter {
    pub(crate) fn new(
        num_ledgers_before_ignore: u32,
        max_request_limit: u32,
        ledger_source: Box<dyn LedgerSource>,
    ) -> Self {
        Self {
            num_ledgers_before_ignore,
            max_request_limit,
            record_map: BTreeMap::new(),
            ledger_source,
        }
    }

    pub(crate) fn add_and_validate_request<F: FnOnce() -> bool>(
        &mut self,
        request: &SurveyRequestMessage,
        local_node_id: &NodeId,
        on_success_validation: F,
    ) -> bool {
        if request.command_type != SurveyMessageCommandType::TimeSlicedSurveyTopology {
            return false;
        }

        if !self.survey_ledger_num_valid(request.ledger_num) {
            return false;
        }

        let surveyor_is_self = &request.surveyor_peer_id == local_node_id;
        let ledger_entry = self.record_map.entry(request.ledger_num).or_default();

        let ledger_entry_len = ledger_entry.len() as u32;
        let surveyor_entry = ledger_entry.entry(request.surveyor_peer_id.clone());
        match surveyor_entry {
            std::collections::hash_map::Entry::Vacant(entry) => {
                if !surveyor_is_self && ledger_entry_len >= self.max_request_limit {
                    return false;
                }
                if !on_success_validation() {
                    return false;
                }
                let mut surveyed_map = HashMap::new();
                surveyed_map.insert(request.surveyed_peer_id.clone(), false);
                entry.insert(surveyed_map);
                true
            }
            std::collections::hash_map::Entry::Occupied(mut entry) => {
                let surveyed_map = entry.get_mut();
                if !surveyor_is_self && surveyed_map.len() as u32 >= self.max_request_limit {
                    return false;
                }
                match surveyed_map.entry(request.surveyed_peer_id.clone()) {
                    std::collections::hash_map::Entry::Vacant(entry) => {
                        if !on_success_validation() {
                            return false;
                        }
                        entry.insert(false);
                        true
                    }
                    std::collections::hash_map::Entry::Occupied(_) => false,
                }
            }
        }
    }

    pub(crate) fn record_and_validate_response<F: FnOnce() -> bool>(
        &mut self,
        response: &SurveyResponseMessage,
        on_success_validation: F,
    ) -> bool {
        if !self.survey_ledger_num_valid(response.ledger_num) {
            return false;
        }

        let Some(ledger_entry) = self.record_map.get_mut(&response.ledger_num) else {
            return false;
        };
        let Some(surveyor_entry) = ledger_entry.get_mut(&response.surveyor_peer_id) else {
            return false;
        };
        let Some(seen) = surveyor_entry.get_mut(&response.surveyed_peer_id) else {
            return false;
        };

        if *seen {
            return false;
        }

        if !on_success_validation() {
            return false;
        }

        *seen = true;
        true
    }

    pub(crate) fn validate_start_collecting<F: FnOnce() -> bool>(
        &self,
        start: &TimeSlicedSurveyStartCollectingMessage,
        survey_active: bool,
        on_success_validation: F,
    ) -> bool {
        if !self.survey_ledger_num_valid(start.ledger_num) {
            return false;
        }
        if survey_active {
            return false;
        }
        on_success_validation()
    }

    pub(crate) fn validate_stop_collecting<F: FnOnce() -> bool>(
        &self,
        stop: &TimeSlicedSurveyStopCollectingMessage,
        on_success_validation: F,
    ) -> bool {
        if !self.survey_ledger_num_valid(stop.ledger_num) {
            return false;
        }
        on_success_validation()
    }

    pub(crate) fn clear_old_ledgers(&mut self, last_closed_ledger: u32) {
        let threshold = last_closed_ledger.saturating_sub(self.num_ledgers_before_ignore);
        while let Some((&ledger, _)) = self.record_map.iter().next() {
            if ledger < threshold {
                self.record_map.pop_first();
            } else {
                break;
            }
        }
    }

    /// Check if a ledger number is within the acceptable range for survey messages.
    /// Reads the current ledger exactly once via `self.ledger_source`.
    fn survey_ledger_num_valid(&self, ledger_num: u32) -> bool {
        let current_ledger = self.ledger_source.current_ledger();
        let max_offset = self.num_ledgers_before_ignore.max(1);
        let upper = current_ledger.saturating_add(max_offset);
        let lower = current_ledger.saturating_sub(self.num_ledgers_before_ignore);
        ledger_num >= lower && ledger_num <= upper
    }
}

#[derive(Debug)]
struct LatencyHistogram {
    samples: VecDeque<u64>,
    max_samples: usize,
}

impl LatencyHistogram {
    fn new(max_samples: usize) -> Self {
        Self {
            samples: VecDeque::with_capacity(max_samples),
            max_samples,
        }
    }

    fn update(&mut self, value_ms: u64) {
        self.samples.push_back(value_ms);
        if self.samples.len() > self.max_samples {
            self.samples.pop_front();
        }
    }

    fn percentile(&self, percentile: u32) -> u32 {
        if self.samples.is_empty() {
            return 0;
        }
        let mut sorted: Vec<u64> = self.samples.iter().copied().collect();
        sorted.sort_unstable();
        let idx = ((sorted.len() - 1) * percentile as usize) / 100;
        sorted[idx] as u32
    }

    fn median(&self) -> u32 {
        self.percentile(50)
    }

    fn p75(&self) -> u32 {
        self.percentile(75)
    }
}

#[derive(Debug)]
struct CollectingNodeData {
    initial_lost_sync_count: u64,
    initially_out_of_sync: bool,
    initial_added_peers: u64,
    initial_dropped_peers: u64,
    scp_first_to_self_latency: LatencyHistogram,
    scp_self_to_other_latency: LatencyHistogram,
}

#[derive(Debug, Clone)]
struct InitialPeerStats {
    messages_read: u64,
    messages_written: u64,
    bytes_read: u64,
    bytes_written: u64,
    unique_flood_bytes_recv: u64,
    duplicate_flood_bytes_recv: u64,
    unique_fetch_bytes_recv: u64,
    duplicate_fetch_bytes_recv: u64,
    unique_flood_messages_recv: u64,
    duplicate_flood_messages_recv: u64,
    unique_fetch_messages_recv: u64,
    duplicate_fetch_messages_recv: u64,
}

impl From<&henyey_overlay::PeerStatsSnapshot> for InitialPeerStats {
    fn from(stats: &henyey_overlay::PeerStatsSnapshot) -> Self {
        Self {
            messages_read: stats.messages_received,
            messages_written: stats.messages_sent,
            bytes_read: stats.bytes_received,
            bytes_written: stats.bytes_sent,
            unique_flood_bytes_recv: stats.unique_flood_bytes_recv,
            duplicate_flood_bytes_recv: stats.duplicate_flood_bytes_recv,
            unique_fetch_bytes_recv: stats.unique_fetch_bytes_recv,
            duplicate_fetch_bytes_recv: stats.duplicate_fetch_bytes_recv,
            unique_flood_messages_recv: stats.unique_flood_messages_recv,
            duplicate_flood_messages_recv: stats.duplicate_flood_messages_recv,
            unique_fetch_messages_recv: stats.unique_fetch_messages_recv,
            duplicate_fetch_messages_recv: stats.duplicate_fetch_messages_recv,
        }
    }
}

#[derive(Debug)]
struct CollectingPeerData {
    initial_stats: InitialPeerStats,
    latency_ms: LatencyHistogram,
}

/// Initial node-level statistics captured at the start of a survey collection.
///
/// These baseline values are subtracted from final totals to compute deltas
/// over the survey window.
#[derive(Debug, Clone)]
pub struct NodeStatsSnapshot {
    /// Cumulative lost-sync count at survey start.
    pub lost_sync_count: u64,
    /// Whether the node was out of sync when the survey started.
    pub out_of_sync: bool,
    /// Cumulative added-peer count at survey start.
    pub added_peers: u64,
    /// Cumulative dropped-peer count at survey start.
    pub dropped_peers: u64,
}

/// Manager for collecting and reporting time-sliced survey data.
///
/// This struct handles the complete survey lifecycle for a single node:
///
/// 1. **Start collecting**: Initialize statistics tracking for all connected peers
/// 2. **During collection**: Record latency samples, message counts, byte counts
/// 3. **Stop collecting**: Finalize statistics and transition to reporting phase
/// 4. **Reporting**: Respond to topology requests with collected data
///
/// The data collected includes:
/// - Per-peer message and byte counts (delta from survey start)
/// - Latency measurements (median values)
/// - Node-level metrics (sync losses, peer churn)
/// - SCP latency samples (first-to-self, self-to-other)
#[derive(Debug)]
pub struct SurveyDataManager {
    /// Current survey phase.
    phase: SurveyPhase,
    /// When collection started.
    collect_start: Option<Instant>,
    /// When collection ended.
    collect_end: Option<Instant>,
    /// Survey nonce for correlation.
    nonce: Option<u32>,
    /// Public key of the surveyor node.
    surveyor_id: Option<NodeId>,
    /// Node-level statistics being collected.
    collecting_node: Option<CollectingNodeData>,
    /// Per-peer statistics for inbound connections.
    collecting_inbound: HashMap<PeerId, CollectingPeerData>,
    /// Per-peer statistics for outbound connections.
    collecting_outbound: HashMap<PeerId, CollectingPeerData>,
    /// Finalized node data for reporting.
    final_node: Option<TimeSlicedNodeData>,
    /// Finalized inbound peer data for reporting.
    final_inbound: Vec<TimeSlicedPeerData>,
    /// Finalized outbound peer data for reporting.
    final_outbound: Vec<TimeSlicedPeerData>,
    /// Whether this node is a validator.
    is_validator: bool,
    /// Maximum inbound peer connections.
    max_inbound: u32,
    /// Maximum outbound peer connections.
    max_outbound: u32,
}

impl SurveyDataManager {
    pub fn new(is_validator: bool, max_inbound: u32, max_outbound: u32) -> Self {
        Self {
            phase: SurveyPhase::Inactive,
            collect_start: None,
            collect_end: None,
            nonce: None,
            surveyor_id: None,
            collecting_node: None,
            collecting_inbound: HashMap::new(),
            collecting_outbound: HashMap::new(),
            final_node: None,
            final_inbound: Vec::new(),
            final_outbound: Vec::new(),
            is_validator,
            max_inbound,
            max_outbound,
        }
    }

    pub fn phase(&self) -> SurveyPhase {
        self.phase
    }

    pub fn nonce(&self) -> Option<u32> {
        self.nonce
    }

    pub fn nonce_is_reporting(&self, nonce: u32) -> bool {
        self.phase == SurveyPhase::Reporting && self.nonce == Some(nonce)
    }

    pub fn survey_is_active(&self) -> bool {
        self.phase != SurveyPhase::Inactive
    }

    pub fn start_collecting(
        &mut self,
        msg: &TimeSlicedSurveyStartCollectingMessage,
        inbound_peers: &[PeerSnapshot],
        outbound_peers: &[PeerSnapshot],
        node_stats: NodeStatsSnapshot,
    ) -> bool {
        if self.phase != SurveyPhase::Inactive {
            return false;
        }

        self.phase = SurveyPhase::Collecting;
        self.collect_start = Some(Instant::now());
        self.collect_end = None;
        self.nonce = Some(msg.nonce);
        self.surveyor_id = Some(msg.surveyor_id.clone());
        self.collecting_node = Some(CollectingNodeData {
            initial_lost_sync_count: node_stats.lost_sync_count,
            initially_out_of_sync: node_stats.out_of_sync,
            initial_added_peers: node_stats.added_peers,
            initial_dropped_peers: node_stats.dropped_peers,
            scp_first_to_self_latency: LatencyHistogram::new(DEFAULT_HISTOGRAM_SAMPLES),
            scp_self_to_other_latency: LatencyHistogram::new(DEFAULT_HISTOGRAM_SAMPLES),
        });

        self.collecting_inbound = Self::initialize_collecting_peers(inbound_peers);
        self.collecting_outbound = Self::initialize_collecting_peers(outbound_peers);

        true
    }

    pub fn stop_collecting(
        &mut self,
        msg: &TimeSlicedSurveyStopCollectingMessage,
        inbound_peers: &[PeerSnapshot],
        outbound_peers: &[PeerSnapshot],
        added_peers_total: u64,
        dropped_peers_total: u64,
        lost_sync_count_total: u64,
    ) -> bool {
        self.stop_collecting_by_identity(
            msg.nonce,
            &msg.surveyor_id,
            inbound_peers,
            outbound_peers,
            added_peers_total,
            dropped_peers_total,
            lost_sync_count_total,
        )
    }

    /// Stop collecting by nonce and surveyor identity directly, without
    /// requiring the full network message struct. Used by local cleanup paths
    /// where constructing a wire message is unnecessary.
    #[allow(clippy::too_many_arguments)]
    pub fn stop_collecting_by_identity(
        &mut self,
        nonce: u32,
        surveyor_id: &stellar_xdr::NodeId,
        inbound_peers: &[PeerSnapshot],
        outbound_peers: &[PeerSnapshot],
        added_peers_total: u64,
        dropped_peers_total: u64,
        lost_sync_count_total: u64,
    ) -> bool {
        if self.phase != SurveyPhase::Collecting {
            return false;
        }
        if self.nonce != Some(nonce) || self.surveyor_id.as_ref() != Some(surveyor_id) {
            return false;
        }

        self.start_reporting_phase(
            inbound_peers,
            outbound_peers,
            added_peers_total,
            dropped_peers_total,
            lost_sync_count_total,
        )
    }

    pub fn update_phase(
        &mut self,
        inbound_peers: &[PeerSnapshot],
        outbound_peers: &[PeerSnapshot],
        added_peers_total: u64,
        dropped_peers_total: u64,
        lost_sync_count_total: u64,
    ) {
        match self.phase {
            SurveyPhase::Collecting => {
                if let Some(start) = self.collect_start {
                    if start.elapsed() > COLLECTING_PHASE_MAX_DURATION {
                        self.start_reporting_phase(
                            inbound_peers,
                            outbound_peers,
                            added_peers_total,
                            dropped_peers_total,
                            lost_sync_count_total,
                        );
                    }
                }
            }
            SurveyPhase::Reporting => {
                if let Some(end) = self.collect_end {
                    if end.elapsed() > REPORTING_PHASE_MAX_DURATION {
                        self.reset();
                    }
                }
            }
            SurveyPhase::Inactive => {}
        }
    }

    pub fn record_peer_latency(&mut self, peer_id: &PeerId, latency_ms: u64) {
        if self.phase != SurveyPhase::Collecting {
            return;
        }

        if let Some(entry) = self.collecting_inbound.get_mut(peer_id) {
            entry.latency_ms.update(latency_ms);
            return;
        }

        if let Some(entry) = self.collecting_outbound.get_mut(peer_id) {
            entry.latency_ms.update(latency_ms);
        }
    }

    pub fn record_scp_first_to_self_latency(&mut self, latency_ms: u64) {
        if self.phase != SurveyPhase::Collecting {
            return;
        }

        if let Some(node) = self.collecting_node.as_mut() {
            node.scp_first_to_self_latency.update(latency_ms);
        }
    }

    pub fn record_scp_self_to_other_latency(&mut self, latency_ms: u64) {
        if self.phase != SurveyPhase::Collecting {
            return;
        }

        if let Some(node) = self.collecting_node.as_mut() {
            node.scp_self_to_other_latency.update(latency_ms);
        }
    }

    pub fn fill_survey_data(
        &self,
        request: &TimeSlicedSurveyRequestMessage,
    ) -> Option<TopologyResponseBodyV2> {
        if self.phase != SurveyPhase::Reporting {
            return None;
        }
        if self.nonce != Some(request.nonce) {
            return None;
        }
        if self
            .surveyor_id
            .as_ref()
            .map(|id| id == &request.request.surveyor_peer_id)
            != Some(true)
        {
            return None;
        }

        let node_data = self.final_node.as_ref()?.clone();

        let inbound_peers = Self::slice_peer_data(&self.final_inbound, request.inbound_peers_index);
        let outbound_peers =
            Self::slice_peer_data(&self.final_outbound, request.outbound_peers_index);

        Some(TopologyResponseBodyV2 {
            inbound_peers,
            outbound_peers,
            node_data,
        })
    }

    pub fn final_node_data(&self) -> Option<TimeSlicedNodeData> {
        self.final_node.clone()
    }

    pub fn final_inbound_peers(&self) -> &[TimeSlicedPeerData] {
        &self.final_inbound
    }

    pub fn final_outbound_peers(&self) -> &[TimeSlicedPeerData] {
        &self.final_outbound
    }

    fn initialize_collecting_peers(peers: &[PeerSnapshot]) -> HashMap<PeerId, CollectingPeerData> {
        let mut result = HashMap::new();
        for snapshot in peers {
            result.insert(
                snapshot.info.peer_id.clone(),
                CollectingPeerData {
                    initial_stats: InitialPeerStats::from(&snapshot.stats),
                    latency_ms: LatencyHistogram::new(DEFAULT_HISTOGRAM_SAMPLES),
                },
            );
        }
        result
    }

    fn slice_peer_data(peers: &[TimeSlicedPeerData], index: u32) -> TimeSlicedPeerDataList {
        let idx = index as usize;
        if idx >= peers.len() {
            return TimeSlicedPeerDataList(Vec::new().try_into().unwrap_or_default());
        }

        let end = usize::min(peers.len(), idx + TIME_SLICED_PEERS_MAX);
        let slice = peers[idx..end].to_vec();
        TimeSlicedPeerDataList(slice.try_into().unwrap_or_default())
    }

    fn start_reporting_phase(
        &mut self,
        inbound_peers: &[PeerSnapshot],
        outbound_peers: &[PeerSnapshot],
        added_peers_total: u64,
        dropped_peers_total: u64,
        lost_sync_count_total: u64,
    ) -> bool {
        if self.phase != SurveyPhase::Collecting {
            return false;
        }

        self.phase = SurveyPhase::Reporting;
        self.collect_end = Some(Instant::now());

        self.final_inbound = self.finalize_peer_data(inbound_peers, &self.collecting_inbound);
        self.final_outbound = self.finalize_peer_data(outbound_peers, &self.collecting_outbound);

        self.final_node = self.finalize_node_data(
            added_peers_total,
            dropped_peers_total,
            lost_sync_count_total,
        );

        self.collecting_inbound.clear();
        self.collecting_outbound.clear();
        self.collecting_node.take();

        true
    }

    fn finalize_peer_data(
        &self,
        peers: &[PeerSnapshot],
        collecting: &HashMap<PeerId, CollectingPeerData>,
    ) -> Vec<TimeSlicedPeerData> {
        let mut ordered: Vec<&PeerSnapshot> = peers.iter().collect();
        ordered.sort_by(|a, b| a.info.peer_id.as_bytes().cmp(b.info.peer_id.as_bytes()));

        let mut result = Vec::new();
        for snapshot in ordered {
            let Some(initial) = collecting.get(&snapshot.info.peer_id) else {
                continue;
            };

            let stats = &snapshot.stats;
            let peer_stats = PeerStats {
                id: NodeId(snapshot.info.peer_id.0.clone()),
                version_str: snapshot
                    .info
                    .version_string
                    .clone()
                    .try_into()
                    .unwrap_or_default(),
                messages_read: stats
                    .messages_received
                    .saturating_sub(initial.initial_stats.messages_read),
                messages_written: stats
                    .messages_sent
                    .saturating_sub(initial.initial_stats.messages_written),
                bytes_read: stats
                    .bytes_received
                    .saturating_sub(initial.initial_stats.bytes_read),
                bytes_written: stats
                    .bytes_sent
                    .saturating_sub(initial.initial_stats.bytes_written),
                seconds_connected: snapshot.info.connected_at.elapsed().as_secs(),
                unique_flood_bytes_recv: stats
                    .unique_flood_bytes_recv
                    .saturating_sub(initial.initial_stats.unique_flood_bytes_recv),
                duplicate_flood_bytes_recv: stats
                    .duplicate_flood_bytes_recv
                    .saturating_sub(initial.initial_stats.duplicate_flood_bytes_recv),
                unique_fetch_bytes_recv: stats
                    .unique_fetch_bytes_recv
                    .saturating_sub(initial.initial_stats.unique_fetch_bytes_recv),
                duplicate_fetch_bytes_recv: stats
                    .duplicate_fetch_bytes_recv
                    .saturating_sub(initial.initial_stats.duplicate_fetch_bytes_recv),
                unique_flood_message_recv: stats
                    .unique_flood_messages_recv
                    .saturating_sub(initial.initial_stats.unique_flood_messages_recv),
                duplicate_flood_message_recv: stats
                    .duplicate_flood_messages_recv
                    .saturating_sub(initial.initial_stats.duplicate_flood_messages_recv),
                unique_fetch_message_recv: stats
                    .unique_fetch_messages_recv
                    .saturating_sub(initial.initial_stats.unique_fetch_messages_recv),
                duplicate_fetch_message_recv: stats
                    .duplicate_fetch_messages_recv
                    .saturating_sub(initial.initial_stats.duplicate_fetch_messages_recv),
            };

            let latency_ms = initial.latency_ms.median();

            result.push(TimeSlicedPeerData {
                peer_stats,
                average_latency_ms: latency_ms,
            });
        }
        result
    }

    fn finalize_node_data(
        &self,
        added_peers_total: u64,
        dropped_peers_total: u64,
        lost_sync_count_total: u64,
    ) -> Option<TimeSlicedNodeData> {
        let node = self.collecting_node.as_ref()?;
        let mut lost_sync_count =
            lost_sync_count_total.saturating_sub(node.initial_lost_sync_count);
        if node.initially_out_of_sync {
            lost_sync_count = lost_sync_count.saturating_add(1);
        }

        Some(TimeSlicedNodeData {
            added_authenticated_peers: added_peers_total.saturating_sub(node.initial_added_peers)
                as u32,
            dropped_authenticated_peers: dropped_peers_total
                .saturating_sub(node.initial_dropped_peers)
                as u32,
            total_inbound_peer_count: self.final_inbound.len() as u32,
            total_outbound_peer_count: self.final_outbound.len() as u32,
            p75_scp_first_to_self_latency_ms: node.scp_first_to_self_latency.p75(),
            p75_scp_self_to_other_latency_ms: node.scp_self_to_other_latency.p75(),
            lost_sync_count: lost_sync_count as u32,
            is_validator: self.is_validator,
            max_inbound_peer_count: self.max_inbound,
            max_outbound_peer_count: self.max_outbound,
        })
    }

    fn reset(&mut self) {
        self.phase = SurveyPhase::Inactive;
        self.collect_start = None;
        self.collect_end = None;
        self.nonce = None;
        self.surveyor_id = None;
        self.collecting_node = None;
        self.collecting_inbound.clear();
        self.collecting_outbound.clear();
        self.final_node = None;
        self.final_inbound.clear();
        self.final_outbound.clear();
    }
}

/// Combined survey state — data manager and message limiter under one lock.
///
/// # Invariant
///
/// No `.await` may be performed while holding a guard on the enclosing
/// `RwLock<SurveyState>`. All lock acquisitions must be scoped to synchronous
/// blocks. The `RwLockReadGuard`/`RwLockWriteGuard` from tokio is `!Send` when
/// the lock is not held across await points, providing a compile-time guarantee.
///
/// # Rationale
///
/// In stellar-core, `SurveyManager` owns both `mSurveyDataManager` and
/// `mMessageLimiter` as member fields accessed on a single thread. This struct
/// provides the same serialization guarantee for the data+limiter pair in an
/// async context via a single `RwLock`.
pub struct SurveyState {
    data: SurveyDataManager,
    limiter: SurveyMessageLimiter,
}

impl std::fmt::Debug for SurveyState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SurveyState")
            .field("data", &self.data)
            .field("limiter", &self.limiter)
            .finish()
    }
}

impl SurveyState {
    pub(crate) fn new(data: SurveyDataManager, limiter: SurveyMessageLimiter) -> Self {
        Self { data, limiter }
    }

    /// Access the survey data manager (read-only).
    pub fn data(&self) -> &SurveyDataManager {
        &self.data
    }

    /// Access the survey data manager (mutable).
    pub fn data_mut(&mut self) -> &mut SurveyDataManager {
        &mut self.data
    }

    /// Validate a start-collecting message atomically.
    ///
    /// Reads `survey_is_active()` and delegates to the limiter within the same
    /// lock acquisition, eliminating the TOCTOU race. The limiter reads the
    /// current ledger internally via its `LedgerSource`.
    pub fn validate_start_collecting<F: FnOnce() -> bool>(
        &self,
        start: &TimeSlicedSurveyStartCollectingMessage,
        on_success_validation: F,
    ) -> bool {
        let survey_active = self.data.survey_is_active();
        self.limiter
            .validate_start_collecting(start, survey_active, on_success_validation)
    }

    /// Validate a stop-collecting message (delegates to limiter).
    pub fn validate_stop_collecting<F: FnOnce() -> bool>(
        &self,
        stop: &TimeSlicedSurveyStopCollectingMessage,
        on_success_validation: F,
    ) -> bool {
        self.limiter
            .validate_stop_collecting(stop, on_success_validation)
    }

    /// Validate a survey request atomically.
    ///
    /// Reads `nonce_is_reporting()` and delegates to the limiter within the same
    /// lock acquisition, eliminating the TOCTOU race. The limiter reads the
    /// current ledger internally via its `LedgerSource`.
    pub fn add_and_validate_request<F: FnOnce() -> bool>(
        &mut self,
        request: &SurveyRequestMessage,
        local_node_id: &NodeId,
        nonce: u32,
        on_success_validation: F,
    ) -> bool {
        let nonce_is_reporting = self.data.nonce_is_reporting(nonce);
        self.limiter
            .add_and_validate_request(request, local_node_id, || {
                nonce_is_reporting && on_success_validation()
            })
    }

    /// Validate a survey response atomically.
    ///
    /// Reads `nonce_is_reporting()` and delegates to the limiter within the same
    /// lock acquisition, eliminating the TOCTOU race. The limiter reads the
    /// current ledger internally via its `LedgerSource`.
    pub fn record_and_validate_response<F: FnOnce() -> bool>(
        &mut self,
        response: &SurveyResponseMessage,
        nonce: u32,
        on_success_validation: F,
    ) -> bool {
        let nonce_is_reporting = self.data.nonce_is_reporting(nonce);
        self.limiter.record_and_validate_response(response, || {
            nonce_is_reporting && on_success_validation()
        })
    }

    /// Clear old ledger entries from the limiter.
    pub fn clear_old_ledgers(&mut self, last_closed_ledger: u32) {
        self.limiter.clear_old_ledgers(last_closed_ledger);
    }

    /// Update the survey phase and clear old limiter entries atomically.
    pub fn update_phase_and_clear(
        &mut self,
        inbound: &[PeerSnapshot],
        outbound: &[PeerSnapshot],
        added: u64,
        dropped: u64,
        lost_sync: u64,
        last_closed_ledger: u32,
    ) {
        self.data
            .update_phase(inbound, outbound, added, dropped, lost_sync);
        self.limiter.clear_old_ledgers(last_closed_ledger);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    use stellar_xdr::{NodeId, PublicKey, Uint256};

    /// Mock ledger source for tests — returns a controllable ledger number.
    #[derive(Debug)]
    struct MockLedgerSource(Arc<AtomicU32>);

    impl LedgerSource for MockLedgerSource {
        fn current_ledger(&self) -> u32 {
            self.0.load(Ordering::Relaxed)
        }
    }

    fn mock_ledger(value: u32) -> Box<dyn LedgerSource> {
        Box::new(MockLedgerSource(Arc::new(AtomicU32::new(value))))
    }

    fn mock_ledger_shared(counter: &Arc<AtomicU32>) -> Box<dyn LedgerSource> {
        Box::new(MockLedgerSource(counter.clone()))
    }

    fn test_node_id() -> NodeId {
        NodeId(PublicKey::PublicKeyTypeEd25519(Uint256([1u8; 32])))
    }

    fn test_start_msg(nonce: u32) -> TimeSlicedSurveyStartCollectingMessage {
        TimeSlicedSurveyStartCollectingMessage {
            surveyor_id: test_node_id(),
            nonce,
            ledger_num: 100,
        }
    }

    #[test]
    fn test_stop_collecting_by_identity_transitions_phase() {
        let mut mgr = SurveyDataManager::new(false, 10, 10);
        let nonce = 42u32;
        let surveyor_id = test_node_id();
        let node_stats = NodeStatsSnapshot {
            lost_sync_count: 0,
            out_of_sync: false,
            added_peers: 0,
            dropped_peers: 0,
        };

        // Start collecting first.
        let start_msg = test_start_msg(nonce);
        let started = mgr.start_collecting(&start_msg, &[], &[], node_stats.clone());
        assert!(started);
        assert_eq!(mgr.phase(), SurveyPhase::Collecting);

        // Stop collecting by identity.
        let stopped = mgr.stop_collecting_by_identity(nonce, &surveyor_id, &[], &[], 0, 0, 0);
        assert!(stopped);
        assert_eq!(mgr.phase(), SurveyPhase::Reporting);
    }

    #[test]
    fn test_stop_collecting_by_identity_rejects_wrong_nonce() {
        let mut mgr = SurveyDataManager::new(false, 10, 10);
        let nonce = 42u32;
        let surveyor_id = test_node_id();
        let node_stats = NodeStatsSnapshot {
            lost_sync_count: 0,
            out_of_sync: false,
            added_peers: 0,
            dropped_peers: 0,
        };

        let start_msg = test_start_msg(nonce);
        mgr.start_collecting(&start_msg, &[], &[], node_stats);

        // Wrong nonce — should not transition.
        let stopped = mgr.stop_collecting_by_identity(99, &surveyor_id, &[], &[], 0, 0, 0);
        assert!(!stopped);
        assert_eq!(mgr.phase(), SurveyPhase::Collecting);
    }

    #[test]
    fn test_stop_collecting_by_identity_rejects_wrong_surveyor() {
        let mut mgr = SurveyDataManager::new(false, 10, 10);
        let nonce = 42u32;
        let node_stats = NodeStatsSnapshot {
            lost_sync_count: 0,
            out_of_sync: false,
            added_peers: 0,
            dropped_peers: 0,
        };

        let start_msg = test_start_msg(nonce);
        mgr.start_collecting(&start_msg, &[], &[], node_stats);

        let wrong_id = NodeId(PublicKey::PublicKeyTypeEd25519(Uint256([9u8; 32])));
        let stopped = mgr.stop_collecting_by_identity(nonce, &wrong_id, &[], &[], 0, 0, 0);
        assert!(!stopped);
        assert_eq!(mgr.phase(), SurveyPhase::Collecting);
    }

    #[test]
    fn test_stop_collecting_by_identity_rejects_inactive_phase() {
        let mut mgr = SurveyDataManager::new(false, 10, 10);
        let surveyor_id = test_node_id();

        // Not in collecting phase — should fail.
        let stopped = mgr.stop_collecting_by_identity(42, &surveyor_id, &[], &[], 0, 0, 0);
        assert!(!stopped);
        assert_eq!(mgr.phase(), SurveyPhase::Inactive);
    }

    // --- SurveyState tests ---

    fn test_survey_state() -> SurveyState {
        let data = SurveyDataManager::new(false, 10, 10);
        let limiter = SurveyMessageLimiter::new(6, 10, mock_ledger(100));
        SurveyState::new(data, limiter)
    }

    fn test_request_msg(_nonce: u32) -> SurveyRequestMessage {
        SurveyRequestMessage {
            surveyor_peer_id: test_node_id(),
            surveyed_peer_id: NodeId(PublicKey::PublicKeyTypeEd25519(Uint256([2u8; 32]))),
            ledger_num: 100,
            encryption_key: stellar_xdr::Curve25519Public { key: [0u8; 32] },
            command_type: SurveyMessageCommandType::TimeSlicedSurveyTopology,
        }
    }

    #[test]
    fn test_survey_state_validate_start_collecting_rejects_active() {
        let mut state = test_survey_state();

        // Start a survey to make it active.
        let start_msg = test_start_msg(42);
        let node_stats = NodeStatsSnapshot {
            lost_sync_count: 0,
            out_of_sync: false,
            added_peers: 0,
            dropped_peers: 0,
        };
        assert!(state
            .data_mut()
            .start_collecting(&start_msg, &[], &[], node_stats));
        assert!(state.data().survey_is_active());

        // validate_start_collecting should reject because survey is active.
        let new_start = test_start_msg(99);
        let is_valid = state.validate_start_collecting(&new_start, || true);
        assert!(!is_valid);
    }

    #[test]
    fn test_survey_state_validate_start_collecting_accepts_inactive() {
        let state = test_survey_state();

        // Survey is inactive — should accept.
        let start_msg = test_start_msg(42);
        let is_valid = state.validate_start_collecting(&start_msg, || true);
        assert!(is_valid);
    }

    #[test]
    fn test_survey_state_add_and_validate_request_atomic() {
        let mut state = test_survey_state();

        // Put state into Reporting phase so nonce_is_reporting returns true.
        let start_msg = test_start_msg(42);
        let node_stats = NodeStatsSnapshot {
            lost_sync_count: 0,
            out_of_sync: false,
            added_peers: 0,
            dropped_peers: 0,
        };
        state
            .data_mut()
            .start_collecting(&start_msg, &[], &[], node_stats);
        // Transition to Reporting.
        let stop_msg = TimeSlicedSurveyStopCollectingMessage {
            surveyor_id: test_node_id(),
            nonce: 42,
            ledger_num: 100,
        };
        state
            .data_mut()
            .stop_collecting(&stop_msg, &[], &[], 0, 0, 0);
        assert_eq!(state.data().phase(), SurveyPhase::Reporting);
        assert!(state.data().nonce_is_reporting(42));

        // Should succeed with matching nonce.
        let request = test_request_msg(42);
        let local_node = test_node_id();
        let is_valid = state.add_and_validate_request(&request, &local_node, 42, || true);
        assert!(is_valid);

        // With wrong nonce, closure short-circuits (nonce_is_reporting is false).
        let request2 = SurveyRequestMessage {
            surveyed_peer_id: NodeId(PublicKey::PublicKeyTypeEd25519(Uint256([3u8; 32]))),
            ..request.clone()
        };
        let is_valid = state.add_and_validate_request(&request2, &local_node, 99, || true);
        assert!(!is_valid);
    }

    #[test]
    fn test_survey_state_record_and_validate_response_atomic() {
        let mut state = test_survey_state();

        // Put state into Reporting phase.
        let start_msg = test_start_msg(42);
        let node_stats = NodeStatsSnapshot {
            lost_sync_count: 0,
            out_of_sync: false,
            added_peers: 0,
            dropped_peers: 0,
        };
        state
            .data_mut()
            .start_collecting(&start_msg, &[], &[], node_stats);
        let stop_msg = TimeSlicedSurveyStopCollectingMessage {
            surveyor_id: test_node_id(),
            nonce: 42,
            ledger_num: 100,
        };
        state
            .data_mut()
            .stop_collecting(&stop_msg, &[], &[], 0, 0, 0);

        // First, register a request so the limiter knows about this surveyor/surveyed pair.
        let request = test_request_msg(42);
        let local_node = test_node_id();
        state.add_and_validate_request(&request, &local_node, 42, || true);

        // Now validate a response.
        let response = SurveyResponseMessage {
            surveyor_peer_id: test_node_id(),
            surveyed_peer_id: NodeId(PublicKey::PublicKeyTypeEd25519(Uint256([2u8; 32]))),
            ledger_num: 100,
            command_type: SurveyMessageCommandType::TimeSlicedSurveyTopology,
            encrypted_body: stellar_xdr::EncryptedBody(vec![].try_into().unwrap()),
        };
        let is_valid = state.record_and_validate_response(&response, 42, || true);
        assert!(is_valid);

        // With wrong nonce, should fail.
        let is_valid = state.record_and_validate_response(&response, 99, || true);
        assert!(!is_valid);
    }

    #[test]
    fn test_survey_state_stale_validate_then_mutate_regression() {
        let mut state = test_survey_state();

        // validate_start_collecting accepts (survey inactive).
        let start_msg = test_start_msg(42);
        let is_valid = state.validate_start_collecting(&start_msg, || true);
        assert!(is_valid);

        // Simulate race: another task starts a survey between validation and mutation.
        let node_stats = NodeStatsSnapshot {
            lost_sync_count: 0,
            out_of_sync: false,
            added_peers: 0,
            dropped_peers: 0,
        };
        state
            .data_mut()
            .start_collecting(&start_msg, &[], &[], node_stats);
        assert!(state.data().survey_is_active());

        // Now the original task tries to mutate — start_collecting should reject
        // because survey is already active (idempotency guard).
        let node_stats2 = NodeStatsSnapshot {
            lost_sync_count: 0,
            out_of_sync: false,
            added_peers: 0,
            dropped_peers: 0,
        };
        let result = state
            .data_mut()
            .start_collecting(&start_msg, &[], &[], node_stats2);
        assert!(
            !result,
            "start_collecting should reject when survey is already active"
        );
    }

    // --- Limiter boundary tests ---

    #[test]
    fn test_limiter_ledger_boundary_ignore_6() {
        // With num_ledgers_before_ignore=6, window is [current-6, current+6].
        let mut limiter = SurveyMessageLimiter::new(6, 10, mock_ledger(100));
        let local_node = test_node_id();

        // Just inside lower bound (ledger 94 = 100-6).
        let request_at_94 = SurveyRequestMessage {
            surveyor_peer_id: test_node_id(),
            surveyed_peer_id: NodeId(PublicKey::PublicKeyTypeEd25519(Uint256([2u8; 32]))),
            ledger_num: 94,
            encryption_key: stellar_xdr::Curve25519Public { key: [0u8; 32] },
            command_type: SurveyMessageCommandType::TimeSlicedSurveyTopology,
        };
        assert!(limiter.add_and_validate_request(&request_at_94, &local_node, || true));

        // Just outside lower bound (ledger 93 = 100-7).
        let request_at_93 = SurveyRequestMessage {
            surveyed_peer_id: NodeId(PublicKey::PublicKeyTypeEd25519(Uint256([3u8; 32]))),
            ledger_num: 93,
            ..request_at_94.clone()
        };
        assert!(!limiter.add_and_validate_request(&request_at_93, &local_node, || true));

        // Just inside upper bound (ledger 106 = 100+6).
        let request_at_106 = SurveyRequestMessage {
            surveyed_peer_id: NodeId(PublicKey::PublicKeyTypeEd25519(Uint256([4u8; 32]))),
            ledger_num: 106,
            ..request_at_94.clone()
        };
        assert!(limiter.add_and_validate_request(&request_at_106, &local_node, || true));

        // Just outside upper bound (ledger 107 = 100+7).
        let request_at_107 = SurveyRequestMessage {
            surveyed_peer_id: NodeId(PublicKey::PublicKeyTypeEd25519(Uint256([5u8; 32]))),
            ledger_num: 107,
            ..request_at_94.clone()
        };
        assert!(!limiter.add_and_validate_request(&request_at_107, &local_node, || true));
    }

    #[test]
    fn test_limiter_ledger_boundary_ignore_0() {
        // With num_ledgers_before_ignore=0, window is [current, current+1] (max(0,1)=1).
        let mut limiter = SurveyMessageLimiter::new(0, 10, mock_ledger(50));
        let local_node = test_node_id();

        let make_request = |ledger_num: u32, id: u8| SurveyRequestMessage {
            surveyor_peer_id: test_node_id(),
            surveyed_peer_id: NodeId(PublicKey::PublicKeyTypeEd25519(Uint256([id; 32]))),
            ledger_num,
            encryption_key: stellar_xdr::Curve25519Public { key: [0u8; 32] },
            command_type: SurveyMessageCommandType::TimeSlicedSurveyTopology,
        };

        // Exact current (50) — valid.
        assert!(limiter.add_and_validate_request(&make_request(50, 2), &local_node, || true));
        // current+1 (51) — valid (upper = 50 + max(0,1) = 51).
        assert!(limiter.add_and_validate_request(&make_request(51, 3), &local_node, || true));
        // current-1 (49) — invalid (lower = 50 - 0 = 50).
        assert!(!limiter.add_and_validate_request(&make_request(49, 4), &local_node, || true));
        // current+2 (52) — invalid.
        assert!(!limiter.add_and_validate_request(&make_request(52, 5), &local_node, || true));
    }

    #[test]
    fn test_limiter_ledger_zero_boot_state() {
        // When current_ledger() returns 0 (boot state), lower saturates to 0
        // and upper = 0 + max(ignore, 1) = max(6, 1) = 6.
        let mut limiter = SurveyMessageLimiter::new(6, 10, mock_ledger(0));
        let local_node = test_node_id();

        let make_request = |ledger_num: u32, id: u8| SurveyRequestMessage {
            surveyor_peer_id: test_node_id(),
            surveyed_peer_id: NodeId(PublicKey::PublicKeyTypeEd25519(Uint256([id; 32]))),
            ledger_num,
            encryption_key: stellar_xdr::Curve25519Public { key: [0u8; 32] },
            command_type: SurveyMessageCommandType::TimeSlicedSurveyTopology,
        };

        // Ledger 0 — valid (within [0, 6]).
        assert!(limiter.add_and_validate_request(&make_request(0, 1), &local_node, || true));
        // Ledger 6 — valid (upper bound = 0 + max(6,1) = 6).
        assert!(limiter.add_and_validate_request(&make_request(6, 2), &local_node, || true));
        // Ledger 7 — invalid (above upper).
        assert!(!limiter.add_and_validate_request(&make_request(7, 3), &local_node, || true));
    }

    #[test]
    fn test_limiter_dynamic_ledger_read() {
        // Verify that the limiter reads the ledger at validation time.
        let counter = Arc::new(AtomicU32::new(100));
        let mut limiter = SurveyMessageLimiter::new(6, 10, mock_ledger_shared(&counter));
        let local_node = test_node_id();

        // At ledger 100, request at ledger 94 is valid (100-6=94).
        let request_94 = SurveyRequestMessage {
            surveyor_peer_id: test_node_id(),
            surveyed_peer_id: NodeId(PublicKey::PublicKeyTypeEd25519(Uint256([2u8; 32]))),
            ledger_num: 94,
            encryption_key: stellar_xdr::Curve25519Public { key: [0u8; 32] },
            command_type: SurveyMessageCommandType::TimeSlicedSurveyTopology,
        };
        assert!(limiter.add_and_validate_request(&request_94, &local_node, || true));

        // Advance ledger to 105. Now ledger 94 is stale (105-6=99 > 94).
        counter.store(105, Ordering::Relaxed);
        let request_94b = SurveyRequestMessage {
            surveyed_peer_id: NodeId(PublicKey::PublicKeyTypeEd25519(Uint256([3u8; 32]))),
            ledger_num: 94,
            ..request_94.clone()
        };
        assert!(!limiter.add_and_validate_request(&request_94b, &local_node, || true));

        // But ledger 99 (105-6) is now valid.
        let request_99 = SurveyRequestMessage {
            surveyed_peer_id: NodeId(PublicKey::PublicKeyTypeEd25519(Uint256([4u8; 32]))),
            ledger_num: 99,
            ..request_94.clone()
        };
        assert!(limiter.add_and_validate_request(&request_99, &local_node, || true));
    }

    #[test]
    fn test_limiter_duplicate_request_rejected() {
        let mut limiter = SurveyMessageLimiter::new(6, 10, mock_ledger(100));
        let local_node = test_node_id();

        let request = SurveyRequestMessage {
            surveyor_peer_id: test_node_id(),
            surveyed_peer_id: NodeId(PublicKey::PublicKeyTypeEd25519(Uint256([2u8; 32]))),
            ledger_num: 100,
            encryption_key: stellar_xdr::Curve25519Public { key: [0u8; 32] },
            command_type: SurveyMessageCommandType::TimeSlicedSurveyTopology,
        };

        // First request succeeds.
        assert!(limiter.add_and_validate_request(&request, &local_node, || true));
        // Duplicate request is rejected.
        assert!(!limiter.add_and_validate_request(&request, &local_node, || true));
    }

    #[test]
    fn test_limiter_max_requests_per_surveyor() {
        // Set max to 2 requests per surveyor.
        let mut limiter = SurveyMessageLimiter::new(6, 2, mock_ledger(100));
        let other_node = NodeId(PublicKey::PublicKeyTypeEd25519(Uint256([9u8; 32])));

        let make_request = |id: u8| SurveyRequestMessage {
            surveyor_peer_id: test_node_id(),
            surveyed_peer_id: NodeId(PublicKey::PublicKeyTypeEd25519(Uint256([id; 32]))),
            ledger_num: 100,
            encryption_key: stellar_xdr::Curve25519Public { key: [0u8; 32] },
            command_type: SurveyMessageCommandType::TimeSlicedSurveyTopology,
        };

        // Two requests succeed (at limit).
        assert!(limiter.add_and_validate_request(&make_request(2), &other_node, || true));
        assert!(limiter.add_and_validate_request(&make_request(3), &other_node, || true));
        // Third is rejected (over limit).
        assert!(!limiter.add_and_validate_request(&make_request(4), &other_node, || true));
    }

    #[test]
    fn test_limiter_self_bypasses_limit() {
        // Self (local_node_id matches surveyor) bypasses the per-surveyor limit.
        let mut limiter = SurveyMessageLimiter::new(6, 2, mock_ledger(100));
        let local_node = test_node_id(); // Same as surveyor_peer_id in requests

        let make_request = |id: u8| SurveyRequestMessage {
            surveyor_peer_id: test_node_id(),
            surveyed_peer_id: NodeId(PublicKey::PublicKeyTypeEd25519(Uint256([id; 32]))),
            ledger_num: 100,
            encryption_key: stellar_xdr::Curve25519Public { key: [0u8; 32] },
            command_type: SurveyMessageCommandType::TimeSlicedSurveyTopology,
        };

        // Three requests all succeed because surveyor == self.
        assert!(limiter.add_and_validate_request(&make_request(2), &local_node, || true));
        assert!(limiter.add_and_validate_request(&make_request(3), &local_node, || true));
        assert!(limiter.add_and_validate_request(&make_request(4), &local_node, || true));
    }

    #[test]
    fn test_limiter_response_without_request_rejected() {
        let mut limiter = SurveyMessageLimiter::new(6, 10, mock_ledger(100));

        let response = SurveyResponseMessage {
            surveyor_peer_id: test_node_id(),
            surveyed_peer_id: NodeId(PublicKey::PublicKeyTypeEd25519(Uint256([2u8; 32]))),
            ledger_num: 100,
            command_type: SurveyMessageCommandType::TimeSlicedSurveyTopology,
            encrypted_body: stellar_xdr::EncryptedBody(vec![].try_into().unwrap()),
        };

        // No request was registered, so response is rejected.
        assert!(!limiter.record_and_validate_response(&response, || true));
    }
}
