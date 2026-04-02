//! Network topology survey management.
//!
//! This module implements the SurveyManager from stellar-core, which orchestrates
//! network topology surveys to collect data about the overlay network structure.
//!
//! # Overview
//!
//! Network surveys allow operators to gather information about:
//! - Network topology (which nodes are connected to which)
//! - Peer statistics (messages, bytes, latencies)
//! - Node health (sync status, state)
//!
//! # Survey Lifecycle
//!
//! Surveys have three phases:
//! 1. **Collecting** - Active data collection from peers
//! 2. **Reporting** - Collecting complete, data available for queries
//! 3. **Inactive** - No survey in progress
//!
//! # Message Types
//!
//! The survey system uses these XDR message types:
//! - `TimeSlicedSurveyStartCollectingMessage` - Start a survey
//! - `TimeSlicedSurveyStopCollectingMessage` - Stop collecting, enter reporting
//! - `TimeSlicedSurveyRequestMessage` - Request survey data from a node
//! - `SignedTimeSlicedSurveyResponseMessage` - Response with survey data
//!
//! # Security
//!
//! - Survey requests must be signed by authorized surveyors
//! - Responses are encrypted with Curve25519
//! - Rate limiting prevents survey flooding

use crate::PeerId;
use parking_lot::RwLock;
use std::collections::{HashMap, HashSet, VecDeque};
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};

/// Maximum phase duration for collecting (default: 30 minutes).
const MAX_COLLECTING_PHASE_DURATION: Duration = Duration::from_secs(30 * 60);

/// Maximum phase duration for reporting (default: 3 hours).
/// Spec: OVERLAY_SPEC §8.3 — MAX_REPORTING_PHASE_DURATION = 3 hours.
const MAX_REPORTING_PHASE_DURATION: Duration = Duration::from_secs(3 * 60 * 60);

/// Number of ledgers before survey messages are ignored.
/// Spec: OVERLAY_SPEC §8.3 — NUM_LEDGERS_BEFORE_IGNORE = 6.
const NUM_LEDGERS_BEFORE_IGNORE: u32 = 6;

/// Maximum requests per ledger from a single surveyor.
/// Spec: OVERLAY_SPEC §8.3 — MAX_REQUEST_LIMIT_PER_LEDGER = 10.
const MAX_REQUEST_LIMIT_PER_LEDGER: u32 = 10;

/// Throttle timeout multiplier for survey requests.
pub const SURVEY_THROTTLE_TIMEOUT_MULT: u32 = 3;

/// Survey throttle timeout in milliseconds.
const SURVEY_THROTTLE_TIMEOUT_MS: u64 = 200;

/// Current phase of a survey.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SurveyPhase {
    /// Survey is actively collecting data from peers.
    Collecting,
    /// Collecting is complete, data is available for reporting.
    Reporting,
    /// No active survey in progress.
    #[default]
    Inactive,
}

/// Data collected about this node during a survey.
#[derive(Debug, Clone, Default)]
pub struct CollectingNodeData {
    /// Number of authenticated peers added during survey.
    pub added_authenticated_peers: u32,
    /// Number of authenticated peers dropped during survey.
    pub dropped_authenticated_peers: u32,
    /// Initial count of times node lost sync.
    pub initial_lost_sync_count: u64,
    /// SCP first-to-self latencies in milliseconds.
    pub scp_first_to_self_latencies_ms: Vec<u64>,
    /// SCP self-to-other latencies in milliseconds.
    pub scp_self_to_other_latencies_ms: Vec<u64>,
}

impl CollectingNodeData {
    /// Create new node data with initial lost sync count.
    pub fn new(initial_lost_sync_count: u64) -> Self {
        Self {
            added_authenticated_peers: 0,
            dropped_authenticated_peers: 0,
            initial_lost_sync_count,
            scp_first_to_self_latencies_ms: Vec::new(),
            scp_self_to_other_latencies_ms: Vec::new(),
        }
    }

    /// Record a peer being added.
    pub fn record_peer_added(&mut self) {
        self.added_authenticated_peers += 1;
    }

    /// Record a peer being dropped.
    pub fn record_peer_dropped(&mut self) {
        self.dropped_authenticated_peers += 1;
    }

    /// Record an SCP latency measurement.
    pub fn record_scp_latency(&mut self, first_to_self_ms: u64, self_to_other_ms: u64) {
        self.scp_first_to_self_latencies_ms.push(first_to_self_ms);
        self.scp_self_to_other_latencies_ms.push(self_to_other_ms);
    }
}

/// Data collected about a peer during a survey.
#[derive(Debug, Clone)]
pub struct CollectingPeerData {
    /// Messages read at survey start.
    pub initial_messages_read: u64,
    /// Messages written at survey start.
    pub initial_messages_written: u64,
    /// Bytes read at survey start.
    pub initial_bytes_read: u64,
    /// Bytes written at survey start.
    pub initial_bytes_written: u64,
    /// Unique flood bytes received at survey start.
    pub initial_unique_flood_bytes_recv: u64,
    /// Duplicate flood bytes received at survey start.
    pub initial_duplicate_flood_bytes_recv: u64,
    /// Unique fetch bytes received at survey start.
    pub initial_unique_fetch_bytes_recv: u64,
    /// Duplicate fetch bytes received at survey start.
    pub initial_duplicate_fetch_bytes_recv: u64,
    /// Latency measurements in milliseconds.
    pub latencies_ms: Vec<u64>,
}

impl CollectingPeerData {
    /// Create new peer data with initial metrics.
    pub fn new(
        messages_read: u64,
        messages_written: u64,
        bytes_read: u64,
        bytes_written: u64,
    ) -> Self {
        Self {
            initial_messages_read: messages_read,
            initial_messages_written: messages_written,
            initial_bytes_read: bytes_read,
            initial_bytes_written: bytes_written,
            initial_unique_flood_bytes_recv: 0,
            initial_duplicate_flood_bytes_recv: 0,
            initial_unique_fetch_bytes_recv: 0,
            initial_duplicate_fetch_bytes_recv: 0,
            latencies_ms: Vec::new(),
        }
    }

    /// Record a latency measurement.
    pub fn record_latency(&mut self, latency_ms: u64) {
        self.latencies_ms.push(latency_ms);
    }

    /// Get average latency in milliseconds, or 0 if no measurements.
    pub fn avg_latency_ms(&self) -> u64 {
        if self.latencies_ms.is_empty() {
            0
        } else {
            self.latencies_ms.iter().sum::<u64>() / self.latencies_ms.len() as u64
        }
    }
}

/// Finalized peer data for reporting.
#[derive(Debug, Clone)]
pub struct TimeSlicedPeerData {
    /// Peer ID.
    pub peer_id: PeerId,
    /// Messages read delta.
    pub messages_read: u64,
    /// Messages written delta.
    pub messages_written: u64,
    /// Bytes read delta.
    pub bytes_read: u64,
    /// Bytes written delta.
    pub bytes_written: u64,
    /// Average latency in milliseconds.
    pub avg_latency_ms: u64,
}

/// Finalized node data for reporting.
#[derive(Debug, Clone)]
pub struct TimeSlicedNodeData {
    /// Added peers during survey.
    pub added_peers: u32,
    /// Dropped peers during survey.
    pub dropped_peers: u32,
    /// Total inbound peers at end.
    pub total_inbound_peers: u32,
    /// Total outbound peers at end.
    pub total_outbound_peers: u32,
    /// Times lost sync during survey.
    pub lost_sync_count: u64,
    /// Average SCP first-to-self latency.
    pub avg_scp_first_to_self_latency_ms: u64,
    /// Average SCP self-to-other latency.
    pub avg_scp_self_to_other_latency_ms: u64,
}

/// Configuration for survey operations.
#[derive(Debug, Clone)]
pub struct SurveyConfig {
    /// Maximum duration for collecting phase.
    pub max_collecting_duration: Duration,
    /// Maximum duration for reporting phase.
    pub max_reporting_duration: Duration,
    /// Number of ledgers before ignoring old messages.
    pub num_ledgers_before_ignore: u32,
    /// Maximum requests per ledger per surveyor.
    pub max_request_limit_per_ledger: u32,
    /// Throttle timeout for survey requests.
    pub throttle_timeout: Duration,
    /// Node IDs allowed to survey this node (empty = any).
    pub surveyor_allowlist: HashSet<PeerId>,
}

impl Default for SurveyConfig {
    fn default() -> Self {
        Self {
            max_collecting_duration: MAX_COLLECTING_PHASE_DURATION,
            max_reporting_duration: MAX_REPORTING_PHASE_DURATION,
            num_ledgers_before_ignore: NUM_LEDGERS_BEFORE_IGNORE,
            max_request_limit_per_ledger: MAX_REQUEST_LIMIT_PER_LEDGER,
            throttle_timeout: Duration::from_millis(SURVEY_THROTTLE_TIMEOUT_MS),
            surveyor_allowlist: HashSet::new(),
        }
    }
}

/// State of an active survey being collected/reported.
struct SurveyState {
    /// Survey nonce (unique identifier).
    nonce: u32,
    /// Current phase.
    phase: SurveyPhase,
    /// When collecting started.
    collect_start_time: Instant,
    /// When collecting ended (if in reporting phase).
    collect_end_time: Option<Instant>,
    /// Node data being collected.
    node_data: CollectingNodeData,
    /// Inbound peer data being collected.
    inbound_peer_data: HashMap<PeerId, CollectingPeerData>,
    /// Outbound peer data being collected.
    outbound_peer_data: HashMap<PeerId, CollectingPeerData>,
    /// Finalized node data (reporting phase).
    final_node_data: Option<TimeSlicedNodeData>,
    /// Finalized inbound peer data (reporting phase).
    final_inbound_peer_data: Vec<TimeSlicedPeerData>,
    /// Finalized outbound peer data (reporting phase).
    final_outbound_peer_data: Vec<TimeSlicedPeerData>,
}

/// Rate limiter for survey messages.
struct SurveyMessageLimiter {
    /// Records of (ledger, surveyor, surveyed) -> response seen.
    records: HashMap<u32, HashMap<PeerId, HashMap<PeerId, bool>>>,
    /// Configuration.
    config: SurveyConfig,
}

impl SurveyMessageLimiter {
    fn new(config: SurveyConfig) -> Self {
        Self {
            records: HashMap::new(),
            config,
        }
    }

    /// Check if a ledger number is valid (within acceptable range).
    fn ledger_num_valid(&self, ledger_num: u32, current_ledger: u32) -> bool {
        if ledger_num > current_ledger {
            return false;
        }
        current_ledger - ledger_num <= self.config.num_ledgers_before_ignore
    }

    /// Add and validate a survey request.
    fn add_request(
        &mut self,
        ledger_num: u32,
        surveyor: &PeerId,
        surveyed: &PeerId,
        current_ledger: u32,
    ) -> bool {
        if !self.ledger_num_valid(ledger_num, current_ledger) {
            return false;
        }

        let ledger_map = self.records.entry(ledger_num).or_default();
        let surveyor_map = ledger_map.entry(surveyor.clone()).or_default();

        // Check request limit
        if surveyor_map.len() as u32 >= self.config.max_request_limit_per_ledger {
            return false;
        }

        // Check if already requested
        if surveyor_map.contains_key(surveyed) {
            return false;
        }

        surveyor_map.insert(surveyed.clone(), false);
        true
    }

    /// Record a survey response.
    fn record_response(
        &mut self,
        ledger_num: u32,
        surveyor: &PeerId,
        surveyed: &PeerId,
        current_ledger: u32,
    ) -> bool {
        if !self.ledger_num_valid(ledger_num, current_ledger) {
            return false;
        }

        if let Some(ledger_map) = self.records.get_mut(&ledger_num) {
            if let Some(surveyor_map) = ledger_map.get_mut(surveyor) {
                if let Some(response_seen) = surveyor_map.get_mut(surveyed) {
                    if *response_seen {
                        // Already seen response
                        return false;
                    }
                    *response_seen = true;
                    return true;
                }
            }
        }
        false
    }

    /// Clear old ledger records.
    fn clear_old_ledgers(&mut self, last_closed_ledger: u32) {
        let min_valid = last_closed_ledger.saturating_sub(self.config.num_ledgers_before_ignore);
        self.records.retain(|&ledger, _| ledger >= min_valid);
    }
}

/// Manager for network topology surveys.
///
/// Orchestrates survey collection, processes messages, and provides results.
pub struct SurveyManager {
    /// Configuration.
    config: SurveyConfig,
    /// Current survey state (if any).
    state: RwLock<Option<SurveyState>>,
    /// Message rate limiter.
    limiter: RwLock<SurveyMessageLimiter>,
    /// Peers queued to survey (reporting phase).
    peers_to_survey: RwLock<VecDeque<PeerId>>,
    /// Peers already surveyed.
    surveyed_peers: RwLock<HashSet<PeerId>>,
    /// Nodes that gave bad responses.
    bad_response_nodes: RwLock<HashSet<PeerId>>,
}

impl SurveyManager {
    /// Create a new survey manager with default configuration.
    pub fn new() -> Self {
        Self::with_config(SurveyConfig::default())
    }

    /// Create a new survey manager with custom configuration.
    pub fn with_config(config: SurveyConfig) -> Self {
        Self {
            limiter: RwLock::new(SurveyMessageLimiter::new(config.clone())),
            config,
            state: RwLock::new(None),
            peers_to_survey: RwLock::new(VecDeque::new()),
            surveyed_peers: RwLock::new(HashSet::new()),
            bad_response_nodes: RwLock::new(HashSet::new()),
        }
    }

    /// Get the current survey phase.
    pub fn phase(&self) -> SurveyPhase {
        let state = self.state.read();
        state
            .as_ref()
            .map(|s| s.phase)
            .unwrap_or(SurveyPhase::Inactive)
    }

    /// Check if a survey is currently active.
    pub fn is_active(&self) -> bool {
        self.phase() != SurveyPhase::Inactive
    }

    /// Get the nonce of the current survey (if any).
    pub fn nonce(&self) -> Option<u32> {
        let state = self.state.read();
        state.as_ref().map(|s| s.nonce)
    }

    /// Check if a surveyor is permitted to survey this node.
    pub fn surveyor_permitted(&self, surveyor: &PeerId) -> bool {
        if self.config.surveyor_allowlist.is_empty() {
            return true;
        }
        self.config.surveyor_allowlist.contains(surveyor)
    }

    /// Start the collecting phase of a survey.
    ///
    /// Returns `false` if a survey is already active.
    pub fn start_collecting(
        &self,
        nonce: u32,
        initial_lost_sync_count: u64,
        inbound_peers: &[PeerId],
        outbound_peers: &[PeerId],
    ) -> bool {
        let mut state = self.state.write();

        if state.is_some() {
            debug!("Cannot start survey: survey already active");
            return false;
        }

        info!("Starting survey collecting phase, nonce={}", nonce);

        // Initialize peer data
        let mut inbound_peer_data = HashMap::new();
        for peer in inbound_peers {
            inbound_peer_data.insert(peer.clone(), CollectingPeerData::new(0, 0, 0, 0));
        }

        let mut outbound_peer_data = HashMap::new();
        for peer in outbound_peers {
            outbound_peer_data.insert(peer.clone(), CollectingPeerData::new(0, 0, 0, 0));
        }

        *state = Some(SurveyState {
            nonce,
            phase: SurveyPhase::Collecting,
            collect_start_time: Instant::now(),
            collect_end_time: None,
            node_data: CollectingNodeData::new(initial_lost_sync_count),
            inbound_peer_data,
            outbound_peer_data,
            final_node_data: None,
            final_inbound_peer_data: Vec::new(),
            final_outbound_peer_data: Vec::new(),
        });

        true
    }

    /// Stop the collecting phase and enter the reporting phase.
    ///
    /// Returns `false` if no survey is active or nonce doesn't match.
    pub fn stop_collecting(
        &self,
        nonce: u32,
        current_lost_sync_count: u64,
        inbound_peers: &[(PeerId, u64, u64, u64, u64)], // (id, msg_read, msg_write, bytes_read, bytes_write)
        outbound_peers: &[(PeerId, u64, u64, u64, u64)],
    ) -> bool {
        let mut state = self.state.write();

        let survey = match state.as_mut() {
            Some(s) if s.nonce == nonce && s.phase == SurveyPhase::Collecting => s,
            _ => {
                debug!("Cannot stop survey: no matching active survey");
                return false;
            }
        };

        info!("Stopping survey collecting phase, nonce={}", nonce);

        let now = Instant::now();
        survey.collect_end_time = Some(now);
        survey.phase = SurveyPhase::Reporting;

        // Finalize peer data
        survey.final_inbound_peer_data =
            finalize_peer_data(&survey.inbound_peer_data, inbound_peers);

        survey.final_outbound_peer_data =
            finalize_peer_data(&survey.outbound_peer_data, outbound_peers);

        // Finalize node data
        let node_data = &survey.node_data;
        let lost_sync_delta =
            current_lost_sync_count.saturating_sub(node_data.initial_lost_sync_count);

        survey.final_node_data = Some(TimeSlicedNodeData {
            added_peers: node_data.added_authenticated_peers,
            dropped_peers: node_data.dropped_authenticated_peers,
            total_inbound_peers: survey.final_inbound_peer_data.len() as u32,
            total_outbound_peers: survey.final_outbound_peer_data.len() as u32,
            lost_sync_count: lost_sync_delta,
            avg_scp_first_to_self_latency_ms: avg_or_zero(
                &node_data.scp_first_to_self_latencies_ms,
            ),
            avg_scp_self_to_other_latency_ms: avg_or_zero(
                &node_data.scp_self_to_other_latencies_ms,
            ),
        });

        true
    }

    /// Reset the survey (transition to inactive).
    pub fn reset(&self) {
        let mut state = self.state.write();
        *state = None;

        let mut peers = self.peers_to_survey.write();
        peers.clear();

        let mut surveyed = self.surveyed_peers.write();
        surveyed.clear();

        let mut bad = self.bad_response_nodes.write();
        bad.clear();

        info!("Survey reset to inactive");
    }

    /// Update the survey phase based on timeouts.
    ///
    /// Call periodically to handle phase transitions and timeouts.
    pub fn update_phase(&self) {
        let mut state = self.state.write();

        let survey = match state.as_mut() {
            Some(s) => s,
            None => return,
        };

        let now = Instant::now();

        match survey.phase {
            SurveyPhase::Collecting => {
                let elapsed = now.duration_since(survey.collect_start_time);
                if elapsed >= self.config.max_collecting_duration {
                    warn!("Survey collecting phase timed out after {:?}", elapsed);
                    // Transition to inactive (could also auto-transition to reporting)
                    *state = None;
                }
            }
            SurveyPhase::Reporting => {
                if let Some(end_time) = survey.collect_end_time {
                    let elapsed = now.duration_since(end_time);
                    if elapsed >= self.config.max_reporting_duration {
                        info!("Survey reporting phase expired after {:?}", elapsed);
                        *state = None;
                    }
                }
            }
            SurveyPhase::Inactive => {}
        }
    }

    /// Modify node data during collecting phase.
    pub fn modify_node_data<F>(&self, f: F)
    where
        F: FnOnce(&mut CollectingNodeData),
    {
        let mut state = self.state.write();
        if let Some(survey) = state.as_mut() {
            if survey.phase == SurveyPhase::Collecting {
                f(&mut survey.node_data);
            }
        }
    }

    /// Modify peer data during collecting phase.
    pub fn modify_peer_data<F>(&self, peer_id: &PeerId, is_inbound: bool, f: F)
    where
        F: FnOnce(&mut CollectingPeerData),
    {
        let mut state = self.state.write();
        if let Some(survey) = state.as_mut() {
            if survey.phase == SurveyPhase::Collecting {
                let peer_data = if is_inbound {
                    &mut survey.inbound_peer_data
                } else {
                    &mut survey.outbound_peer_data
                };
                if let Some(data) = peer_data.get_mut(peer_id) {
                    f(data);
                }
            }
        }
    }

    /// Record that a peer was dropped during collecting phase.
    pub fn record_dropped_peer(&self, peer_id: &PeerId) {
        let mut state = self.state.write();
        if let Some(survey) = state.as_mut() {
            if survey.phase == SurveyPhase::Collecting {
                survey.node_data.record_peer_dropped();
                survey.inbound_peer_data.remove(peer_id);
                survey.outbound_peer_data.remove(peer_id);
            }
        }
    }

    /// Record that a peer was added during collecting phase.
    pub fn record_added_peer(
        &self,
        peer_id: &PeerId,
        is_inbound: bool,
        initial_metrics: CollectingPeerData,
    ) {
        let mut state = self.state.write();
        if let Some(survey) = state.as_mut() {
            if survey.phase == SurveyPhase::Collecting {
                survey.node_data.record_peer_added();
                let peer_data = if is_inbound {
                    &mut survey.inbound_peer_data
                } else {
                    &mut survey.outbound_peer_data
                };
                peer_data.insert(peer_id.clone(), initial_metrics);
            }
        }
    }

    /// Get the finalized node data (reporting phase only).
    pub fn get_node_data(&self) -> Option<TimeSlicedNodeData> {
        let state = self.state.read();
        state.as_ref().and_then(|s| {
            if s.phase == SurveyPhase::Reporting {
                s.final_node_data.clone()
            } else {
                None
            }
        })
    }

    /// Get the finalized inbound peer data (reporting phase only).
    pub fn get_inbound_peer_data(&self) -> Vec<TimeSlicedPeerData> {
        let state = self.state.read();
        state
            .as_ref()
            .filter(|s| s.phase == SurveyPhase::Reporting)
            .map(|s| s.final_inbound_peer_data.clone())
            .unwrap_or_default()
    }

    /// Get the finalized outbound peer data (reporting phase only).
    pub fn get_outbound_peer_data(&self) -> Vec<TimeSlicedPeerData> {
        let state = self.state.read();
        state
            .as_ref()
            .filter(|s| s.phase == SurveyPhase::Reporting)
            .map(|s| s.final_outbound_peer_data.clone())
            .unwrap_or_default()
    }

    /// Add a peer to the backlog of peers to survey.
    pub fn add_peer_to_backlog(&self, peer_id: PeerId) -> bool {
        let surveyed = self.surveyed_peers.write();
        if surveyed.contains(&peer_id) {
            return false;
        }

        let mut peers = self.peers_to_survey.write();
        peers.push_back(peer_id);
        true
    }

    /// Pop the next peer to survey from the backlog.
    pub fn pop_peer_to_survey(&self) -> Option<PeerId> {
        let mut peers = self.peers_to_survey.write();
        let peer = peers.pop_front()?;

        let mut surveyed = self.surveyed_peers.write();
        surveyed.insert(peer.clone());

        Some(peer)
    }

    /// Check if there are peers waiting to be surveyed.
    pub fn has_peers_to_survey(&self) -> bool {
        let peers = self.peers_to_survey.read();
        !peers.is_empty()
    }

    /// Record a bad response from a node.
    pub fn record_bad_response(&self, peer_id: PeerId) {
        let mut bad = self.bad_response_nodes.write();
        bad.insert(peer_id);
    }

    /// Check if a node has given bad responses.
    pub fn is_bad_response_node(&self, peer_id: &PeerId) -> bool {
        let bad = self.bad_response_nodes.read();
        bad.contains(peer_id)
    }

    /// Add and validate a survey request message.
    pub fn add_request(
        &self,
        ledger_num: u32,
        surveyor: &PeerId,
        surveyed: &PeerId,
        current_ledger: u32,
    ) -> bool {
        let mut limiter = self.limiter.write();
        limiter.add_request(ledger_num, surveyor, surveyed, current_ledger)
    }

    /// Record and validate a survey response message.
    pub fn record_response(
        &self,
        ledger_num: u32,
        surveyor: &PeerId,
        surveyed: &PeerId,
        current_ledger: u32,
    ) -> bool {
        let mut limiter = self.limiter.write();
        limiter.record_response(ledger_num, surveyor, surveyed, current_ledger)
    }

    /// Clear old ledger records from the rate limiter.
    pub fn clear_old_ledgers(&self, last_closed_ledger: u32) {
        let mut limiter = self.limiter.write();
        limiter.clear_old_ledgers(last_closed_ledger);
    }

    /// Get statistics about the survey manager.
    pub fn stats(&self) -> SurveyManagerStats {
        let state = self.state.read();
        let peers = self.peers_to_survey.read();
        let surveyed = self.surveyed_peers.read();
        let bad = self.bad_response_nodes.read();

        SurveyManagerStats {
            phase: state
                .as_ref()
                .map(|s| s.phase)
                .unwrap_or(SurveyPhase::Inactive),
            nonce: state.as_ref().map(|s| s.nonce),
            peers_to_survey: peers.len(),
            peers_surveyed: surveyed.len(),
            bad_response_nodes: bad.len(),
            collecting_inbound_peers: state
                .as_ref()
                .map(|s| s.inbound_peer_data.len())
                .unwrap_or(0),
            collecting_outbound_peers: state
                .as_ref()
                .map(|s| s.outbound_peer_data.len())
                .unwrap_or(0),
        }
    }
}

impl Default for SurveyManager {
    fn default() -> Self {
        Self::new()
    }
}

/// Statistics about the survey manager.
#[derive(Debug, Clone)]
pub struct SurveyManagerStats {
    /// Current survey phase.
    pub phase: SurveyPhase,
    /// Current survey nonce (if any).
    pub nonce: Option<u32>,
    /// Number of peers waiting to be surveyed.
    pub peers_to_survey: usize,
    /// Number of peers already surveyed.
    pub peers_surveyed: usize,
    /// Number of nodes that gave bad responses.
    pub bad_response_nodes: usize,
    /// Number of inbound peers being collected.
    pub collecting_inbound_peers: usize,
    /// Number of outbound peers being collected.
    pub collecting_outbound_peers: usize,
}

/// Helper to calculate average or return 0.
fn avg_or_zero(values: &[u64]) -> u64 {
    if values.is_empty() {
        0
    } else {
        values.iter().sum::<u64>() / values.len() as u64
    }
}

/// Helper to finalize peer data.
fn finalize_peer_data(
    collecting: &HashMap<PeerId, CollectingPeerData>,
    current: &[(PeerId, u64, u64, u64, u64)],
) -> Vec<TimeSlicedPeerData> {
    let mut result = Vec::new();

    for (peer_id, msg_read, msg_write, bytes_read, bytes_write) in current {
        if let Some(initial) = collecting.get(peer_id) {
            result.push(TimeSlicedPeerData {
                peer_id: peer_id.clone(),
                messages_read: msg_read.saturating_sub(initial.initial_messages_read),
                messages_written: msg_write.saturating_sub(initial.initial_messages_written),
                bytes_read: bytes_read.saturating_sub(initial.initial_bytes_read),
                bytes_written: bytes_write.saturating_sub(initial.initial_bytes_written),
                avg_latency_ms: initial.avg_latency_ms(),
            });
        }
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_peer_id(id: u8) -> PeerId {
        PeerId::from_bytes([id; 32])
    }

    #[test]
    fn test_survey_manager_creation() {
        let manager = SurveyManager::new();
        assert_eq!(manager.phase(), SurveyPhase::Inactive);
        assert!(!manager.is_active());
        assert!(manager.nonce().is_none());
    }

    #[test]
    fn test_start_collecting() {
        let manager = SurveyManager::new();

        let result = manager.start_collecting(
            12345,
            0,
            &[make_peer_id(2), make_peer_id(3)],
            &[make_peer_id(4)],
        );

        assert!(result);
        assert_eq!(manager.phase(), SurveyPhase::Collecting);
        assert!(manager.is_active());
        assert_eq!(manager.nonce(), Some(12345));

        // Can't start another survey while one is active
        let result = manager.start_collecting(99999, 0, &[], &[]);
        assert!(!result);
    }

    #[test]
    fn test_stop_collecting() {
        let manager = SurveyManager::new();
        let peer2 = make_peer_id(2);
        let peer3 = make_peer_id(3);

        manager.start_collecting(
            12345,
            0,
            std::slice::from_ref(&peer2),
            std::slice::from_ref(&peer3),
        );

        // Wrong nonce should fail
        assert!(!manager.stop_collecting(99999, 0, &[], &[]));

        // Correct nonce should succeed
        let result = manager.stop_collecting(
            12345,
            0,
            &[(peer2.clone(), 100, 50, 1000, 500)],
            &[(peer3.clone(), 200, 100, 2000, 1000)],
        );

        assert!(result);
        assert_eq!(manager.phase(), SurveyPhase::Reporting);

        // Check finalized data
        let node_data = manager.get_node_data();
        assert!(node_data.is_some());

        let inbound = manager.get_inbound_peer_data();
        assert_eq!(inbound.len(), 1);

        let outbound = manager.get_outbound_peer_data();
        assert_eq!(outbound.len(), 1);
    }

    #[test]
    fn test_reset() {
        let manager = SurveyManager::new();

        manager.start_collecting(12345, 0, &[], &[]);
        assert!(manager.is_active());

        manager.reset();
        assert!(!manager.is_active());
        assert_eq!(manager.phase(), SurveyPhase::Inactive);
    }

    #[test]
    fn test_modify_node_data() {
        let manager = SurveyManager::new();

        manager.start_collecting(12345, 10, &[], &[]);

        manager.modify_node_data(|data| {
            data.record_peer_added();
            data.record_peer_added();
            data.record_peer_dropped();
        });

        // Verify by stopping and checking final data
        manager.stop_collecting(12345, 10, &[], &[]);

        let node_data = manager.get_node_data().unwrap();
        assert_eq!(node_data.added_peers, 2);
        assert_eq!(node_data.dropped_peers, 1);
    }

    #[test]
    fn test_peer_backlog() {
        let manager = SurveyManager::new();

        let peer1 = make_peer_id(1);
        let peer2 = make_peer_id(2);

        assert!(!manager.has_peers_to_survey());

        assert!(manager.add_peer_to_backlog(peer1.clone()));
        assert!(manager.add_peer_to_backlog(peer2.clone()));
        assert!(manager.has_peers_to_survey());

        // Can't add same peer twice (already in surveyed set after pop)
        let popped1 = manager.pop_peer_to_survey();
        assert_eq!(popped1, Some(peer1.clone()));

        // peer1 is now in surveyed set
        assert!(!manager.add_peer_to_backlog(peer1));

        let popped2 = manager.pop_peer_to_survey();
        assert_eq!(popped2, Some(peer2));

        assert!(!manager.has_peers_to_survey());
        assert!(manager.pop_peer_to_survey().is_none());
    }

    #[test]
    fn test_bad_response_tracking() {
        let manager = SurveyManager::new();
        let peer = make_peer_id(1);

        assert!(!manager.is_bad_response_node(&peer));

        manager.record_bad_response(peer.clone());

        assert!(manager.is_bad_response_node(&peer));
    }

    #[test]
    fn test_message_limiter_request() {
        let manager = SurveyManager::new();
        let surveyor = make_peer_id(1);
        let surveyed = make_peer_id(2);

        // First request should succeed
        assert!(manager.add_request(100, &surveyor, &surveyed, 100));

        // Duplicate request should fail
        assert!(!manager.add_request(100, &surveyor, &surveyed, 100));

        // Different surveyed should succeed
        let surveyed2 = make_peer_id(3);
        assert!(manager.add_request(100, &surveyor, &surveyed2, 100));
    }

    #[test]
    fn test_message_limiter_response() {
        let manager = SurveyManager::new();
        let surveyor = make_peer_id(1);
        let surveyed = make_peer_id(2);

        // Must have request first
        assert!(!manager.record_response(100, &surveyor, &surveyed, 100));

        // Add request
        manager.add_request(100, &surveyor, &surveyed, 100);

        // First response should succeed
        assert!(manager.record_response(100, &surveyor, &surveyed, 100));

        // Duplicate response should fail
        assert!(!manager.record_response(100, &surveyor, &surveyed, 100));
    }

    #[test]
    fn test_message_limiter_old_ledger() {
        let manager = SurveyManager::new();
        let surveyor = make_peer_id(1);
        let surveyed = make_peer_id(2);

        // Old ledger should be rejected
        assert!(!manager.add_request(100, &surveyor, &surveyed, 200));
    }

    #[test]
    fn test_clear_old_ledgers() {
        let manager = SurveyManager::new();
        let surveyor = make_peer_id(1);
        let surveyed1 = make_peer_id(2);
        let surveyed2 = make_peer_id(3);

        // Add requests at different ledgers
        manager.add_request(100, &surveyor, &surveyed1, 100);
        manager.add_request(110, &surveyor, &surveyed2, 110);

        // Clear old ledgers
        manager.clear_old_ledgers(120);

        // Old request should now be invalid, so adding again should work
        // (but the ledger is too old to be valid)
        // New request at recent ledger should work
        let surveyed3 = make_peer_id(4);
        assert!(manager.add_request(115, &surveyor, &surveyed3, 120));
    }

    #[test]
    fn test_surveyor_permitted() {
        // Empty allowlist = anyone can survey
        let manager = SurveyManager::new();
        assert!(manager.surveyor_permitted(&make_peer_id(1)));

        // With allowlist, only allowed surveyors permitted
        let allowed = make_peer_id(1);
        let config = SurveyConfig {
            surveyor_allowlist: [allowed.clone()].into_iter().collect(),
            ..Default::default()
        };
        let manager = SurveyManager::with_config(config);

        assert!(manager.surveyor_permitted(&allowed));
        assert!(!manager.surveyor_permitted(&make_peer_id(2)));
    }

    #[test]
    fn test_stats() {
        let manager = SurveyManager::new();

        let stats = manager.stats();
        assert_eq!(stats.phase, SurveyPhase::Inactive);
        assert!(stats.nonce.is_none());

        manager.start_collecting(
            12345,
            0,
            &[make_peer_id(2)],
            &[make_peer_id(3), make_peer_id(4)],
        );

        let stats = manager.stats();
        assert_eq!(stats.phase, SurveyPhase::Collecting);
        assert_eq!(stats.nonce, Some(12345));
        assert_eq!(stats.collecting_inbound_peers, 1);
        assert_eq!(stats.collecting_outbound_peers, 2);
    }

    #[test]
    fn test_collecting_peer_data() {
        let mut data = CollectingPeerData::new(100, 50, 1000, 500);

        assert_eq!(data.avg_latency_ms(), 0);

        data.record_latency(10);
        data.record_latency(20);
        data.record_latency(30);

        assert_eq!(data.avg_latency_ms(), 20);
    }

    #[test]
    fn test_collecting_node_data() {
        let mut data = CollectingNodeData::new(5);

        data.record_peer_added();
        data.record_peer_added();
        data.record_peer_dropped();

        assert_eq!(data.added_authenticated_peers, 2);
        assert_eq!(data.dropped_authenticated_peers, 1);
        assert_eq!(data.initial_lost_sync_count, 5);
    }

    // ── OVERLAY_SPEC §8.3: Survey constants ──────────────────────────

    #[test]
    fn test_max_reporting_phase_duration_is_3_hours() {
        let config = SurveyConfig::default();
        assert_eq!(
            config.max_reporting_duration,
            Duration::from_secs(3 * 60 * 60),
            "OVERLAY_SPEC §8.3: MAX_REPORTING_PHASE_DURATION must be 3 hours"
        );
    }

    #[test]
    fn test_num_ledgers_before_ignore_is_6() {
        let config = SurveyConfig::default();
        assert_eq!(
            config.num_ledgers_before_ignore, 6,
            "OVERLAY_SPEC §8.3: NUM_LEDGERS_BEFORE_IGNORE must be 6"
        );
    }

    #[test]
    fn test_max_request_limit_per_ledger_is_10() {
        let config = SurveyConfig::default();
        assert_eq!(
            config.max_request_limit_per_ledger, 10,
            "OVERLAY_SPEC §8.3: MAX_REQUEST_LIMIT_PER_LEDGER must be 10"
        );
    }

    #[test]
    fn test_max_collecting_phase_duration_is_30_minutes() {
        let config = SurveyConfig::default();
        assert_eq!(
            config.max_collecting_duration,
            Duration::from_secs(30 * 60),
            "OVERLAY_SPEC §8.3: MAX_COLLECTING_PHASE_DURATION must be 30 minutes"
        );
    }

    #[test]
    fn test_survey_throttle_timeout_mult_is_3() {
        assert_eq!(
            SURVEY_THROTTLE_TIMEOUT_MULT, 3,
            "OVERLAY_SPEC §8.3: SURVEY_THROTTLE_TIMEOUT_MULT must be 3"
        );
    }
}
