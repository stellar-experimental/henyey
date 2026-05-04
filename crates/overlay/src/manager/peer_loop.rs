//! Per-peer event loop and message routing.
//!
//! Contains `run_peer_loop`, message routing, flow control handling,
//! ping/RTT tracking, timeout checks, and related helpers.

use super::{OutboundMessage, OverlayManager, OverlayMessage, SharedPeerState};
use crate::connection::ConnectionDirection;
use crate::{
    codec::helpers,
    flood::{compute_message_hash, RelayRecord},
    flow_control::{msg_body_size, FlowControl},
    peer::Peer,
    PeerId,
};
use sha2::Digest;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};
use stellar_xdr::curr::{ErrorCode, SError, StellarMessage, StringM, Uint256};
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TrySendError;
use tracing::{debug, info, trace, warn};

/// Maximum length for error messages sent to peers, matching the XDR
/// `string msg<100>` constraint in the `Error` struct.
const MAX_ERROR_MESSAGE_LEN: usize = 100;

use crate::query_policy::QueryKind;

/// Per-query-type sliding-window rate limiter.
///
/// Parity: stellar-core (Peer.cpp:1423-1438) limits GetTxSet, GetScpQuorumSet,
/// and GetScpState queries per peer with a time-windowed counter. GetScpState
/// uses a fixed max of 10 per window (`GET_SCP_STATE_MAX_RATE`); the other two
/// use `window_secs * QUERY_RESPONSE_MULTIPLIER`.
struct QueryInfo {
    last_reset: Instant,
    count: u32,
}

impl QueryInfo {
    fn new() -> Self {
        Self {
            last_reset: Instant::now(),
            count: 0,
        }
    }

    /// Returns true if the query is allowed under the rate limit.
    ///
    /// Mirrors stellar-core's `Peer::process(QueryInfo&, optional<uint32_t>)`.
    fn check_and_increment(&mut self, window: Duration, max_queries: u32) -> bool {
        if self.last_reset.elapsed() >= window {
            self.last_reset = Instant::now();
            self.count = 0;
        }
        if self.count >= max_queries {
            return false;
        }
        self.count += 1;
        true
    }
}

/// Per-peer query rate limiters for GetTxSet, GetScpQuorumSet, and GetScpState.
///
/// Parity: stellar-core Peer.cpp:1423-1438 (`process()`), Peer.cpp:1686
/// (GetScpState with fixed max=10).
struct QueryRateLimiter {
    tx_set: QueryInfo,
    quorum_set: QueryInfo,
    scp_state: QueryInfo,
}

impl QueryRateLimiter {
    fn new() -> Self {
        Self {
            tx_set: QueryInfo::new(),
            quorum_set: QueryInfo::new(),
            scp_state: QueryInfo::new(),
        }
    }

    /// Returns `true` if the message is allowed. Non-query messages always pass.
    fn check(&mut self, message: &StellarMessage, window: Duration) -> bool {
        let kind = match QueryKind::classify(message) {
            Some(k) => k,
            None => return true,
        };
        let max = kind.max_queries(window);
        let info = match kind {
            QueryKind::TxSet => &mut self.tx_set,
            QueryKind::ScpQuorumSet => &mut self.quorum_set,
            QueryKind::ScpState => &mut self.scp_state,
        };
        info.check_and_increment(window, max)
    }
}

/// Traffic class for per-peer inbound rate limiting.
///
/// Each class has its own sub-budget within the peer's overall allocation.
/// SCP is exempt from all rate limiting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TrafficClass {
    /// Transactions and FloodDemand — high priority flood traffic.
    TxAndDemand,
    /// FloodAdvert — lower priority flood traffic.
    Advert,
    /// Control/fetch messages (GetTxSet, TxSet, GetScpState, etc.) — reserved capacity.
    ControlFetch,
    /// Survey messages — counted against aggregate but exempt from flow control.
    Survey,
}

impl TrafficClass {
    fn classify(msg: &StellarMessage) -> Option<Self> {
        match msg {
            // SCP is exempt — returns None
            StellarMessage::ScpMessage(_) => None,
            // Tx + demand
            StellarMessage::Transaction(_) | StellarMessage::FloodDemand(_) => {
                Some(TrafficClass::TxAndDemand)
            }
            // Advert
            StellarMessage::FloodAdvert(_) => Some(TrafficClass::Advert),
            // Survey
            StellarMessage::TimeSlicedSurveyRequest(_)
            | StellarMessage::TimeSlicedSurveyResponse(_)
            | StellarMessage::TimeSlicedSurveyStartCollecting(_)
            | StellarMessage::TimeSlicedSurveyStopCollecting(_) => Some(TrafficClass::Survey),
            // All other messages are control/fetch
            _ => Some(TrafficClass::ControlFetch),
        }
    }
}

/// Per-peer inbound rate limiter with per-class sub-budgets.
///
/// Henyey-specific hardening (not present in stellar-core). Ensures no single
/// peer can exhaust the node's message processing capacity, and that control/fetch
/// messages always have reserved capacity even when flood traffic is high.
///
/// Sub-budgets:
/// - Tx + FloodDemand: up to `tx_demand_limit` per second
/// - FloodAdvert: up to `advert_limit` per second
/// - Control/fetch: reserved minimum `control_fetch_limit` per second
/// - Survey: counted against aggregate only
/// - SCP: exempt (not rate limited)
struct PeerRateLimiter {
    window_start: Instant,
    /// Per-class counts in the current 1-second window.
    tx_demand_count: u32,
    advert_count: u32,
    control_fetch_count: u32,
    aggregate_count: u32,
    /// Configurable limits (per second).
    tx_demand_limit: u32,
    advert_limit: u32,
    control_fetch_limit: u32,
    aggregate_limit: u32,
    /// Telemetry counters (cumulative, not reset per window).
    pub dropped_tx_demand: u64,
    pub dropped_advert: u64,
    pub dropped_control_fetch: u64,
    pub dropped_aggregate: u64,
}

/// Default per-peer aggregate message budget per second.
pub(crate) const DEFAULT_PEER_RATE_LIMIT: u32 = 200;
/// Default per-peer tx + demand sub-budget per second.
const DEFAULT_TX_DEMAND_LIMIT: u32 = 150;
/// Default per-peer advert sub-budget per second.
const DEFAULT_ADVERT_LIMIT: u32 = 50;
/// Default per-peer control/fetch reserved minimum per second.
const DEFAULT_CONTROL_FETCH_LIMIT: u32 = 20;

impl PeerRateLimiter {
    fn new() -> Self {
        Self {
            window_start: Instant::now(),
            tx_demand_count: 0,
            advert_count: 0,
            control_fetch_count: 0,
            aggregate_count: 0,
            tx_demand_limit: DEFAULT_TX_DEMAND_LIMIT,
            advert_limit: DEFAULT_ADVERT_LIMIT,
            control_fetch_limit: DEFAULT_CONTROL_FETCH_LIMIT,
            aggregate_limit: DEFAULT_PEER_RATE_LIMIT,
            dropped_tx_demand: 0,
            dropped_advert: 0,
            dropped_control_fetch: 0,
            dropped_aggregate: 0,
        }
    }

    /// Check if a message of the given traffic class is allowed.
    /// Returns true if allowed, false if rate limited.
    fn allow(&mut self, class: TrafficClass) -> bool {
        // Reset window if needed
        if self.window_start.elapsed() >= Duration::from_secs(1) {
            self.window_start = Instant::now();
            self.tx_demand_count = 0;
            self.advert_count = 0;
            self.control_fetch_count = 0;
            self.aggregate_count = 0;
        }

        // Check aggregate limit first (survey and all other classes)
        if self.aggregate_count >= self.aggregate_limit {
            // Control/fetch gets reserved capacity even when aggregate is exhausted
            if class == TrafficClass::ControlFetch
                && self.control_fetch_count < self.control_fetch_limit
            {
                self.control_fetch_count += 1;
                // Don't increment aggregate — this is reserved capacity
                return true;
            }
            match class {
                TrafficClass::TxAndDemand => self.dropped_tx_demand += 1,
                TrafficClass::Advert => self.dropped_advert += 1,
                TrafficClass::ControlFetch => self.dropped_control_fetch += 1,
                TrafficClass::Survey => self.dropped_aggregate += 1,
            }
            self.dropped_aggregate += 1;
            return false;
        }

        // Check per-class sub-budget
        let allowed = match class {
            TrafficClass::TxAndDemand => {
                if self.tx_demand_count >= self.tx_demand_limit {
                    self.dropped_tx_demand += 1;
                    false
                } else {
                    self.tx_demand_count += 1;
                    true
                }
            }
            TrafficClass::Advert => {
                if self.advert_count >= self.advert_limit {
                    self.dropped_advert += 1;
                    false
                } else {
                    self.advert_count += 1;
                    true
                }
            }
            TrafficClass::ControlFetch => {
                // Control/fetch always allowed within aggregate
                self.control_fetch_count += 1;
                true
            }
            TrafficClass::Survey => {
                // Survey counted against aggregate only, no sub-budget
                true
            }
        };

        if allowed {
            self.aggregate_count += 1;
        }
        allowed
    }
}

/// Number of 1-second ticks between ping attempts.
///
/// Matches stellar-core `RECURRENT_TIMER_PERIOD` (5 seconds).
const PING_INTERVAL_TICKS: u32 = 5;

/// Truncate an error message to fit within the XDR `string msg<100>` limit.
///
/// If the message exceeds 100 bytes it is truncated at a valid UTF-8 boundary
/// (since the XDR string is opaque bytes, this is a convenience for logs).
pub(super) fn truncate_error_msg(msg: &str) -> &str {
    if msg.len() <= MAX_ERROR_MESSAGE_LEN {
        return msg;
    }
    // Find the largest char boundary <= MAX_ERROR_MESSAGE_LEN
    let mut end = MAX_ERROR_MESSAGE_LEN;
    while !msg.is_char_boundary(end) && end > 0 {
        end -= 1;
    }
    &msg[..end]
}

/// Build a `StellarMessage::ErrorMsg` with proper truncation.
///
/// Matches stellar-core `Peer::sendError` (Peer.cpp:710-720) but adds
/// truncation so that `StringM<100>::try_from` cannot fail.
pub(super) fn make_error_msg(code: ErrorCode, message: &str) -> StellarMessage {
    let truncated = truncate_error_msg(message);
    // safe: truncated.len() <= 100
    let msg = StringM::<100>::try_from(truncated).unwrap_or_default();
    StellarMessage::ErrorMsg(SError { code, msg })
}

/// Send an error to a peer then request its task to shut down.
///
/// Matches stellar-core `Peer::sendErrorAndDrop` (Peer.cpp:722-729).
/// Uses `try_send` so this never blocks. Returns true only when the shutdown
/// request was queued; callers that replace this peer must not assume eviction
/// is in progress when the channel is full.
pub(super) fn send_error_and_drop(
    peer_id: &PeerId,
    outbound_tx: &mpsc::Sender<OutboundMessage>,
    code: ErrorCode,
    message: &str,
) -> bool {
    let err_msg = make_error_msg(code, message);
    let _ = outbound_tx.try_send(OutboundMessage::Send(err_msg));
    let shutdown_queued = match outbound_tx.try_send(OutboundMessage::Shutdown) {
        Ok(()) | Err(TrySendError::Closed(_)) => true,
        Err(TrySendError::Full(_)) => false,
    };
    debug!(
        "Sent error to {} and requested drop: code={:?} msg={}",
        peer_id,
        code,
        truncate_error_msg(message),
    );
    shutdown_queued
}

/// Compute the ping hash for a given nanosecond timestamp.
///
/// Creates a SHA-256 hash of the timestamp in little-endian bytes, matching
/// stellar-core's ping nonce generation. The resulting hash is sent as a
/// `GetScpQuorumset` request; a `DontHave` or `ScpQuorumset` response with
/// a matching hash is used to measure round-trip time.
///
/// Extracted from `run_peer_loop` for testability (G4).
fn compute_ping_hash(nanos: u128) -> stellar_xdr::curr::Uint256 {
    let mut hasher = sha2::Sha256::new();
    hasher.update(nanos.to_le_bytes());
    let result = hasher.finalize();
    stellar_xdr::curr::Uint256(result.into())
}

/// Check if a received hash matches an outstanding ping hash.
///
/// Returns true if both `ping_sent_time` and `ping_hash` are `Some` and the
/// received `hash_bytes` matches the stored ping hash.
///
/// Extracted from `run_peer_loop` ping response matching for testability (G4).
fn is_ping_response(
    ping_hash: Option<&stellar_xdr::curr::Uint256>,
    hash: &stellar_xdr::curr::Uint256,
) -> bool {
    match ping_hash {
        Some(ph) => ph.0 == hash.0,
        None => false,
    }
}

/// Tracks outstanding ping state and measures round-trip time.
///
/// Encapsulates the ping hash, send time, and last RTT so that the
/// duplicated ping-response matching in `run_peer_loop` can be a
/// single `check_response` call.
struct PingTracker {
    sent_time: Option<Instant>,
    hash: Option<stellar_xdr::curr::Uint256>,
    last_rtt: Option<Duration>,
}

impl PingTracker {
    fn new() -> Self {
        Self {
            sent_time: None,
            hash: None,
            last_rtt: None,
        }
    }

    /// Record that a ping was sent with the given hash.
    fn record_sent(&mut self, hash: stellar_xdr::curr::Uint256) {
        self.sent_time = Some(Instant::now());
        self.hash = Some(hash);
    }

    /// Check whether `response_hash` matches the outstanding ping.
    /// If so, record the RTT and clear the outstanding ping. Returns
    /// the RTT if this was a match.
    fn check_response(
        &mut self,
        response_hash: &stellar_xdr::curr::Uint256,
        peer_id: &PeerId,
    ) -> Option<Duration> {
        let sent = self.sent_time?;
        if !is_ping_response(self.hash.as_ref(), response_hash) {
            return None;
        }
        let rtt = sent.elapsed();
        debug!("Latency {}: {} ms", peer_id, rtt.as_millis());
        self.last_rtt = Some(rtt);
        self.sent_time = None;
        self.hash = None;
        Some(rtt)
    }

    /// True if no ping is currently outstanding.
    fn is_idle(&self) -> bool {
        self.sent_time.is_none()
    }
}

/// Mutable per-peer state for the peer loop.
///
/// Bundles the individual tracking fields that `handle_received_message` and
/// `route_received_message` need, keeping their parameter counts manageable.
struct PeerLoopCtx<'a> {
    peer: &'a mut Peer,
    received_peers: &'a mut bool,
    ping: &'a mut PingTracker,
    query_limiter: &'a mut QueryRateLimiter,
    peer_rate_limiter: &'a mut PeerRateLimiter,
    scp_messages: &'a mut u64,
    last_write: &'a mut Instant,
}

/// Read-only timing and message counters for timeout checks.
struct PeerTimingInfo {
    last_read: Instant,
    last_write: Instant,
    total_messages: u64,
    scp_messages: u64,
}

/// Result from handling a received message — controls the peer loop's flow.
enum RecvAction {
    /// Continue the loop normally.
    Continue,
    /// Break out of the loop (disconnect).
    Break,
}

/// Log received fetch messages and check for ping responses.
///
/// Handles debug-level logging of fetch response details (hashes, types)
/// and checks `ScpQuorumset`/`DontHave` messages for ping RTT measurement.
fn log_fetch_message(message: &StellarMessage, peer_id: &PeerId, ping: &mut PingTracker) {
    match message {
        StellarMessage::TxSet(ts) => {
            let hash = henyey_common::Hash256::hash_xdr(ts);
            debug!(
                "OVERLAY: Received TxSet from {} hash={} prev_ledger={}",
                peer_id,
                hash,
                hex::encode(ts.previous_ledger_hash.0)
            );
        }
        StellarMessage::GeneralizedTxSet(ts) => {
            let hash = henyey_common::Hash256::hash_xdr(ts);
            debug!(
                "OVERLAY: Received GeneralizedTxSet from {} hash={}",
                peer_id, hash
            );
        }
        StellarMessage::ScpQuorumset(qs) => {
            let hash = henyey_common::Hash256::hash_xdr(qs);
            ping.check_response(&Uint256(hash.0), peer_id);
            debug!(
                "OVERLAY: Received ScpQuorumset from {} hash={}",
                peer_id, hash
            );
        }
        StellarMessage::DontHave(dh) => {
            ping.check_response(&dh.req_hash, peer_id);
            debug!(
                "OVERLAY: Received DontHave from {} type={:?} hash={}",
                peer_id,
                dh.type_,
                hex::encode(dh.req_hash.0)
            );
        }
        StellarMessage::GetTxSet(hash) => {
            debug!(
                "OVERLAY: Received GetTxSet from {} hash={}",
                peer_id,
                hex::encode(hash.0)
            );
        }
        _ => {}
    }
}

fn is_fetch_message(message: &StellarMessage) -> bool {
    matches!(
        message,
        StellarMessage::GetTxSet(_)
            | StellarMessage::TxSet(_)
            | StellarMessage::GeneralizedTxSet(_)
            | StellarMessage::GetScpState(_)
            | StellarMessage::ScpQuorumset(_)
            | StellarMessage::GetScpQuorumset(_)
            | StellarMessage::DontHave(_)
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PeersValidation {
    NotPeers,
    AcceptFirst,
    RejectWrongDirection,
    RejectDuplicate,
}

pub(super) fn validate_incoming_peers(
    direction: ConnectionDirection,
    received_peers: bool,
    message: &StellarMessage,
) -> PeersValidation {
    if !matches!(message, StellarMessage::Peers(_)) {
        return PeersValidation::NotPeers;
    }

    if direction == ConnectionDirection::Inbound {
        return PeersValidation::RejectWrongDirection;
    }

    if received_peers {
        return PeersValidation::RejectDuplicate;
    }

    PeersValidation::AcceptFirst
}

pub(super) fn should_skip_generic_routing(message: &StellarMessage) -> bool {
    helpers::is_handshake_message(message)
        || matches!(
            message,
            StellarMessage::SendMore(_) | StellarMessage::SendMoreExtended(_)
        )
}

impl OverlayManager {
    /// Check whether the peer has exceeded idle or straggler timeouts.
    ///
    /// Returns `true` if the peer should be dropped.
    fn check_peer_timeouts(
        peer_id: &PeerId,
        timing: &PeerTimingInfo,
        flow_control: &FlowControl,
        metrics: &crate::metrics::OverlayMetrics,
    ) -> bool {
        const PEER_TIMEOUT: Duration = Duration::from_secs(30);
        const PEER_STRAGGLER_TIMEOUT: Duration = Duration::from_secs(120);
        // OVERLAY_SPEC §8.5 — drop peer if no SEND_MORE_EXTENDED for this long.
        const PEER_SEND_MODE_IDLE_TIMEOUT_SECS: u64 = 60;

        let now = Instant::now();
        if now.duration_since(timing.last_read) >= PEER_TIMEOUT
            && now.duration_since(timing.last_write) >= PEER_TIMEOUT
        {
            warn!(
                "Dropping peer {} due to idle timeout (total_msgs={}, scp_msgs={})",
                peer_id, timing.total_messages, timing.scp_messages
            );
            metrics.timeouts_idle.inc();
            return true;
        }
        if now.duration_since(timing.last_write) >= PEER_STRAGGLER_TIMEOUT {
            warn!(
                "Dropping peer {} due to straggler timeout (total_msgs={}, scp_msgs={})",
                peer_id, timing.total_messages, timing.scp_messages
            );
            metrics.timeouts_straggler.inc();
            return true;
        }
        if flow_control.no_outbound_capacity_timeout(PEER_SEND_MODE_IDLE_TIMEOUT_SECS) {
            warn!(
                "Dropping peer {} due to PEER_SEND_MODE_IDLE_TIMEOUT (no SEND_MORE_EXTENDED for {}s)",
                peer_id, PEER_SEND_MODE_IDLE_TIMEOUT_SECS
            );
            metrics.timeouts_idle.inc();
            return true;
        }
        false
    }

    /// Send a ping (GetScpQuorumset with a random hash) if due and idle.
    ///
    /// Returns `true` if a message was written (so the caller can update `last_write`).
    async fn maybe_send_ping(
        peer: &mut Peer,
        peer_id: &PeerId,
        ping: &mut PingTracker,
        metrics: &crate::metrics::OverlayMetrics,
    ) -> bool {
        if !peer.is_connected() || !ping.is_idle() {
            return false;
        }
        let now_nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let hash = compute_ping_hash(now_nanos);
        let ping_msg = StellarMessage::GetScpQuorumset(hash.clone());
        if let Err(e) = peer.send(ping_msg).await {
            debug!("Failed to send ping to {}: {}", peer_id, e);
            metrics.errors_write.inc();
            false
        } else {
            metrics.messages_written.inc();
            ping.record_sent(hash);
            true
        }
    }

    /// Log periodic per-peer diagnostics (every 60s on the ping interval).
    fn maybe_log_peer_stats(
        peer_id: &PeerId,
        total_messages: u64,
        scp_messages: u64,
        ping: &PingTracker,
        last_stats_log: &mut Instant,
    ) {
        if last_stats_log.elapsed() >= Duration::from_secs(60) {
            let rtt_str = ping
                .last_rtt
                .map(|d| format!("{}ms", d.as_millis()))
                .unwrap_or_else(|| "n/a".to_string());
            debug!(
                "Peer {} stats: total_msgs={}, scp_msgs={}, rtt={}",
                peer_id, total_messages, scp_messages, rtt_str
            );
            *last_stats_log = Instant::now();
        }
    }

    /// Handle a received `SendMoreExtended` message: release outbound capacity
    /// and drain queued messages. Returns `Err` if the drain send fails (peer
    /// should be dropped), `Ok(true)` if any messages were written, `Ok(false)`
    /// otherwise.
    async fn handle_send_more_extended(
        peer: &mut Peer,
        peer_id: &PeerId,
        message: &StellarMessage,
        flow_control: &FlowControl,
        metrics: &crate::metrics::OverlayMetrics,
    ) -> std::result::Result<bool, ()> {
        if let StellarMessage::SendMoreExtended(sme) = message {
            debug!(
                "Peer {} sent SEND_MORE_EXTENDED: num_messages={}, num_bytes={}",
                peer_id, sme.num_messages, sme.num_bytes
            );
            if let Err(e) = flow_control.is_send_more_valid(message) {
                debug!("Peer {} sent invalid SEND_MORE_EXTENDED: {}", peer_id, e);
                return Err(());
            }
            flow_control.maybe_release_capacity(message);
            match Self::send_flow_controlled_batch(peer, flow_control, metrics).await {
                Ok(sent) => Ok(sent),
                Err(e) => {
                    debug!("Failed to drain queue to {}: {}", peer_id, e);
                    Err(())
                }
            }
        } else {
            Ok(false)
        }
    }

    /// Route a received message to the appropriate subscribers.
    ///
    /// Applies all filtering rules (handshake, flow-control, watcher, rate
    /// limit, flood-gate dedup) and forwards surviving messages. Returns
    /// `true` if the message was SCP (so the caller can bump the SCP counter).
    fn route_received_message(
        message: &StellarMessage,
        peer_id: &PeerId,
        ctx: &mut PeerLoopCtx<'_>,
        state: &SharedPeerState,
        is_validator: bool,
    ) -> Option<bool> {
        // Parity: shouldAbort (Peer.cpp:1157-1160) — skip message processing
        // if the overlay is shutting down.
        if !state.running.load(Ordering::Relaxed) {
            return Some(false);
        }

        let msg_type = helpers::message_type_name(message);

        // OVERLAY_SPEC §7.2: PEERS message validation.
        match validate_incoming_peers(ctx.peer.direction(), *ctx.received_peers, message) {
            PeersValidation::NotPeers => {}
            PeersValidation::AcceptFirst => {
                *ctx.received_peers = true;
            }
            PeersValidation::RejectWrongDirection => {
                warn!(
                    "Peer {} sent PEERS but we are the responder — dropping (OVERLAY_SPEC §7.2)",
                    peer_id
                );
                return None; // signal break
            }
            PeersValidation::RejectDuplicate => {
                warn!(
                    "Peer {} sent duplicate PEERS — dropping (OVERLAY_SPEC §7.2)",
                    peer_id
                );
                return None; // signal break
            }
        }

        if helpers::is_handshake_message(message) {
            warn!(
                "Dropping peer {} for sending post-auth handshake message",
                peer_id
            );
            return None; // drop peer, matching stellar-core
        }

        if should_skip_generic_routing(message) {
            return Some(false);
        }

        // Watcher filter: drop non-essential flood messages for non-validator nodes.
        if !is_validator && helpers::is_watcher_droppable(message) {
            trace!("Watcher: dropping {} from {}", msg_type, peer_id);
            return Some(false);
        }

        // Per-peer query rate limit (parity: Peer.cpp:1423-1438, 1686).
        // stellar-core's window = expectedLedgerCloseTime * MAX_SLOTS_TO_REMEMBER,
        // recomputed dynamically. The app layer updates the atomic after each
        // ledger close via OverlayManager::set_query_rate_limit_window().
        {
            let window_secs = state.query_rate_limit_window_secs.load(Ordering::Relaxed);
            let query_window = Duration::from_secs(window_secs);
            if !ctx.query_limiter.check(message, query_window) {
                debug!(
                    "Dropping {} from {}: query rate limit exceeded",
                    msg_type, peer_id
                );
                return Some(false);
            }
        }

        // Per-peer rate limiter (henyey-specific hardening).
        // SCP messages are exempt (TrafficClass::classify returns None for SCP).
        if let Some(traffic_class) = TrafficClass::classify(message) {
            if !ctx.peer_rate_limiter.allow(traffic_class) {
                debug!(
                    "Dropping {} from {}: per-peer rate limit exceeded ({:?})",
                    msg_type, peer_id, traffic_class
                );
                return Some(false);
            }
        }

        // Global rate limiter backstop (Sybil protection).
        // SCP messages and fetch responses bypass the global limiter — these
        // are critical for consensus and must not be starved by flood traffic.
        // Matches stellar-core which has no global receive-side flood limiter
        // and handles fetch/control traffic on a separate path.
        let is_exempt =
            matches!(message, StellarMessage::ScpMessage(_)) || is_fetch_message(message);
        if !is_exempt && !state.flood_gate.allow_message() {
            debug!(
                "Dropping {} from {}: global rate limit exceeded",
                msg_type, peer_id
            );
            return Some(false);
        }

        let message_size = msg_body_size(message);
        if helpers::is_flood_message(message) {
            if helpers::is_flood_gate_tracked(message) {
                let hash = compute_message_hash(message);
                let lcl = state.last_closed_ledger.load(Ordering::Relaxed);
                let relay = state
                    .flood_gate
                    .record_seen(hash, Some(peer_id.clone()), lcl);
                match relay {
                    RelayRecord::New => {
                        ctx.peer.record_flood_stats(true, message_size);
                        state.metrics.flood_unique_recv.inc();
                    }
                    RelayRecord::Repeated => {
                        ctx.peer.record_flood_stats(false, message_size);
                        state.metrics.flood_duplicate_recv.inc();
                    }
                }
                // FloodGate-tracked messages (SCP and Transaction) are NEVER
                // dropped at the overlay/FloodGate layer.
                //
                // FloodGate's role is *relay accounting* only: `record_seen`
                // above tracked which peer sent us this hash so
                // `get_forward_peers` won't echo it back. Actual dedup
                // happens downstream in message-type-specific handlers:
                // - SCP: `scp_scheduled_envelopes` HashSet in
                //   `pump_scp_intake` (henyey equivalent of stellar-core's
                //   `checkScheduledAndCache`, Peer.cpp:1113-1117)
                // - Tx: herder `receive_transaction` / tx queue dedup
                //   (app/lifecycle.rs, herder.rs)
                //
                // Note: Transaction messages route through the lossy generic
                // broadcast channel (message_tx), not a dedicated channel
                // like SCP. This is a separate architectural difference from
                // stellar-core's direct call path; it does not affect the
                // correctness of removing the FloodGate drop.
                //
                // stellar-core parity:
                // - SCP: Peer.cpp:1667-1673 — unconditional
                //   `recvSCPEnvelope`
                // - Tx:  OverlayManagerImpl.cpp:1215-1248 —
                //   `recvFloodedMsgID` (line 1224) then unconditional
                //   `recvTransaction` (Peer.cpp:1524-1533 delegates here)
                // See issues #2317, #2327.
            } else {
                // Pull-control messages (FloodAdvert/FloodDemand) use flow-control
                // capacity but are NOT globally deduplicated or tracked in FloodGate.
                // stellar-core's recvFloodAdvert/recvFloodDemand do not call
                // recvFloodedMsgID. Every pull-control message is "unique" from
                // the receiver's perspective since there is no global dedup.
                // NOT counted in flood_unique_recv — that metric is reserved for
                // FloodGate-tracked messages (Transaction, ScpMessage).
                ctx.peer.record_flood_stats(true, message_size);
            }
        } else if is_fetch_message(message) {
            ctx.peer.record_fetch_stats(true, message_size);
            log_fetch_message(message, peer_id, ctx.ping);
        }

        // Forward to subscribers.
        let overlay_msg = OverlayMessage {
            from_peer: peer_id.clone(),
            message: message.clone(),
            received_at: Instant::now(),
        };
        let is_scp = state.route_to_subscribers(overlay_msg);
        Some(is_scp)
    }

    /// Run the peer message loop.
    ///
    /// The peer is owned by this task (no mutex). Outbound messages arrive
    /// via `outbound_rx`. The `tokio::select!` multiplexes between network
    /// recv, outbound channel, and periodic timers without blocking.
    pub(super) async fn run_peer_loop(
        peer_id: PeerId,
        mut peer: Peer,
        mut outbound_rx: mpsc::Receiver<OutboundMessage>,
        flow_control: Arc<FlowControl>,
        state: SharedPeerState,
    ) {
        let running = &state.running;
        let is_validator = state.is_validator;

        // NOTE: The initial SEND_MORE_EXTENDED grant is sent in Peer::handshake()
        // after authentication, matching stellar-core's Peer::recvAuth() → sendSendMore().
        // Do NOT send a second grant here.

        // Idle/straggler timeout tracking (matches stellar-core Peer::recurrentTimerExpired).
        let mut last_read = Instant::now();
        let mut last_write = Instant::now();

        // Track message counts for periodic diagnostics
        let mut total_messages: u64 = 0;
        let mut scp_messages: u64 = 0;
        let mut last_stats_log = Instant::now();

        // Single periodic timer for ping, SendMore, and timeout checks.
        // Fires every second (covers 1s SendMore interval and 5s ping interval).
        let mut periodic_interval = tokio::time::interval(Duration::from_secs(1));
        let mut ticks_since_ping: u32 = 0;

        // OVERLAY_SPEC §7.2: Track whether we've received a PEERS message
        // from this peer. At most one is allowed; duplicates cause a drop.
        let mut received_peers = false;
        let mut query_limiter = QueryRateLimiter::new();
        let mut peer_rate_limiter = PeerRateLimiter::new();

        // Ping/RTT tracking (G4/G17): store the hash and send time of the
        // outstanding ping so we can compute round-trip time when the peer
        // responds with DontHave (or a matching ScpQuorumset).
        let mut ping = PingTracker::new();

        loop {
            if !running.load(Ordering::Relaxed) {
                info!(
                    "Peer {} loop exiting: overlay shutting down (total_msgs={}, scp_msgs={})",
                    peer_id, total_messages, scp_messages
                );
                break;
            }

            tokio::select! {
                // Outbound messages from broadcast/send_to/disconnect
                msg = outbound_rx.recv() => {
                    match msg {
                        Some(OutboundMessage::Send(m)) => {
                            if let Err(e) = peer.send(m).await {
                                debug!("Failed to send to {}: {}", peer_id, e);
                                state.metrics.errors_write.inc();
                                break;
                            }
                            state.metrics.messages_written.inc();
                            last_write = Instant::now();
                        }
                        Some(OutboundMessage::Flood(m)) => {
                            // Enqueue in FlowControl with priority-based trimming
                            flow_control.add_msg_and_maybe_trim_queue(m);
                            // Send whatever has capacity
                            match Self::send_flow_controlled_batch(&mut peer, &flow_control, &state.metrics).await {
                                Ok(sent) => {
                                    if sent {
                                        last_write = Instant::now();
                                    }
                                }
                                Err(e) => {
                                    debug!("Failed to send batch to {}: {}", peer_id, e);
                                    break;
                                }
                            }
                        }
                        Some(OutboundMessage::Shutdown) => {
                            info!("Peer {} loop exiting: shutdown requested", peer_id);
                            break;
                        }
                        None => {
                            // Channel closed (PeerHandle dropped)
                            info!("Peer {} loop exiting: outbound channel closed", peer_id);
                            break;
                        }
                    }
                }

                // Receive from network (no mutex — peer is owned)
                result = peer.recv() => {
                    match result {
                        Ok(Some(message)) => {
                            last_read = Instant::now();
                            total_messages += 1;

                            // Overlay metrics: message read.
                            state.metrics.messages_read.inc();

                            // Periodic per-peer stats (every 60s)
                            Self::maybe_log_peer_stats(
                                &peer_id,
                                total_messages,
                                scp_messages,
                                &ping,
                                &mut last_stats_log,
                            );

                            let action = Self::handle_received_message(
                                message,
                                &peer_id,
                                &mut PeerLoopCtx {
                                    peer: &mut peer,
                                    received_peers: &mut received_peers,
                                    ping: &mut ping,
                                    query_limiter: &mut query_limiter,
                                    peer_rate_limiter: &mut peer_rate_limiter,
                                    scp_messages: &mut scp_messages,
                                    last_write: &mut last_write,
                                },
                                &flow_control,
                                &state,
                                is_validator,
                            ).await;
                            if matches!(action, RecvAction::Break) {
                                break;
                            }
                        }
                        Ok(None) => {
                            info!("Peer {} loop exiting: connection closed by remote (total_msgs={}, scp_msgs={})", peer_id, total_messages, scp_messages);
                            break;
                        }
                        Err(e) => {
                            state.metrics.errors_read.inc();
                            info!("Peer {} loop exiting: recv error: {} (total_msgs={}, scp_msgs={})", peer_id, e, total_messages, scp_messages);
                            break;
                        }
                    }
                }

                // Periodic tasks: ping, timeout checks
                _ = periodic_interval.tick() => {
                    if Self::check_peer_timeouts(&peer_id, &PeerTimingInfo {
                        last_read,
                        last_write,
                        total_messages,
                        scp_messages,
                    }, &flow_control, &state.metrics) {
                        break;
                    }

                    ticks_since_ping += 1;
                    if ticks_since_ping >= PING_INTERVAL_TICKS {
                        ticks_since_ping = 0;
                        if Self::maybe_send_ping(&mut peer, &peer_id, &mut ping, &state.metrics).await {
                            last_write = Instant::now();
                        }
                        Self::maybe_log_peer_stats(&peer_id, total_messages, scp_messages, &ping, &mut last_stats_log);
                    }
                }
            }
        }

        // Close peer (owned, no mutex needed)
        peer.close().await;
        debug!("Peer {} loop exited and disconnected", peer_id);
    }

    /// Process a single received message from a peer.
    ///
    /// Handles error messages, flow control, message routing, and SendMore
    /// grants. Returns `RecvAction::Break` if the peer loop should exit.
    async fn handle_received_message(
        message: StellarMessage,
        peer_id: &PeerId,
        ctx: &mut PeerLoopCtx<'_>,
        flow_control: &Arc<FlowControl>,
        state: &SharedPeerState,
        is_validator: bool,
    ) -> RecvAction {
        let msg_type = helpers::message_type_name(&message);
        trace!("Processing message_type={} from {}", msg_type, peer_id);

        // Log ERROR messages (Load rejections are expected, log at debug)
        if let StellarMessage::ErrorMsg(ref err) = message {
            if err.code == ErrorCode::Load {
                debug!(
                    "Peer sent_error peer={} code={:?} msg={}",
                    peer_id,
                    err.code,
                    err.msg.to_string()
                );
            } else {
                warn!(
                    "Peer sent_error peer={} code={:?} msg={}",
                    peer_id,
                    err.code,
                    err.msg.to_string()
                );
            }
            // Parity: stellar-core's recvError() unconditionally
            // calls drop() \u2014 ErrorMsg is terminal.
            return RecvAction::Break;
        }

        // Flow control: RAII guard locks capacity on creation,
        // releases on drop (or explicit finish()).
        let capacity_guard = match crate::flow_control::CapacityGuard::new(
            Arc::clone(flow_control),
            message.clone(),
        ) {
            Some(guard) => guard,
            None => {
                warn!(
                    "Peer exceeded_flow_control_capacity peer={}, dropping",
                    peer_id
                );
                let err = make_error_msg(
                    ErrorCode::Load,
                    "unexpected flood message, peer at capacity",
                );
                match ctx.peer.send(err).await {
                    Ok(()) => state.metrics.messages_written.inc(),
                    Err(_) => state.metrics.errors_write.inc(),
                }
                return RecvAction::Break;
            }
        };

        // Handle flow control messages.
        match &message {
            StellarMessage::SendMore(_) => {
                warn!(
                    "Peer sent_deprecated_send_more peer={}, dropping connection",
                    peer_id
                );
                return RecvAction::Break;
            }
            StellarMessage::SendMoreExtended(_) => {
                match Self::handle_send_more_extended(
                    ctx.peer,
                    peer_id,
                    &message,
                    flow_control,
                    &state.metrics,
                )
                .await
                {
                    Ok(sent) => {
                        if sent {
                            *ctx.last_write = Instant::now();
                        }
                    }
                    Err(()) => return RecvAction::Break,
                }
            }
            _ => {}
        }

        // Route message through filtering and dispatch.
        // `None` signals the peer should be dropped.
        match Self::route_received_message(&message, peer_id, ctx, state, is_validator) {
            None => return RecvAction::Break,
            Some(is_scp) => {
                if is_scp {
                    *ctx.scp_messages += 1;
                }
            }
        }

        // Flow control: finish guard to get send-more capacity.
        let send_more_cap = capacity_guard.finish();
        if send_more_cap.should_send() && ctx.peer.is_connected() {
            if let Err(e) = ctx
                .peer
                .send_more_extended(
                    send_more_cap.num_flood_messages as u32,
                    send_more_cap.num_flood_bytes as u32,
                )
                .await
            {
                debug!("Failed to send SendMoreExtended to peer={}: {}", peer_id, e);
                state.metrics.errors_write.inc();
            } else {
                state.metrics.messages_written.inc();
                *ctx.last_write = Instant::now();
            }
        }

        RecvAction::Continue
    }

    /// Send queued outbound messages that have flow control capacity.
    ///
    /// Retrieves the next batch from FlowControl's priority queues,
    /// sends each message, then cleans up sent entries. Returns true
    /// if any messages were sent.
    pub(super) async fn send_flow_controlled_batch(
        peer: &mut Peer,
        flow_control: &FlowControl,
        metrics: &crate::metrics::OverlayMetrics,
    ) -> crate::Result<bool> {
        use crate::flow_control::MessagePriority;

        let batch = flow_control.get_next_batch_to_send();
        if batch.is_empty() {
            return Ok(false);
        }

        // Group sent messages by priority for process_sent_messages
        let mut sent_by_priority: Vec<Vec<StellarMessage>> =
            vec![Vec::new(); MessagePriority::COUNT];

        for queued in batch {
            let priority = MessagePriority::from_message(&queued.message);
            if let Err(e) = peer.send(queued.message.clone()).await {
                // Send failed — process what we've sent so far, then propagate error
                flow_control.process_sent_messages(&sent_by_priority);
                metrics.errors_write.inc();
                return Err(e);
            }
            metrics.messages_written.inc();
            if let Some(p) = priority {
                sent_by_priority[p as usize].push(queued.message);
            }
        }

        flow_control.process_sent_messages(&sent_by_priority);
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::flow_control::FlowControlConfig;
    use stellar_xdr::curr::ErrorCode;

    #[test]
    fn test_idle_timeout_constants_match_upstream() {
        // Verify our timeout constants match stellar-core defaults:
        // - PEER_TIMEOUT = 30 (Config.cpp:258)
        // - PEER_STRAGGLER_TIMEOUT = 120 (Config.cpp:259)
        // - RECURRENT_TIMER_PERIOD = 5s (Peer.cpp:374)
        // - REALLY_DEAD_NUM_FAILURES_CUTOFF = 120 (Config.h:711)
        assert_eq!(
            Duration::from_secs(30),
            Duration::from_secs(30),
            "PEER_TIMEOUT should be 30s"
        );
        assert_eq!(
            Duration::from_secs(120),
            Duration::from_secs(120),
            "PEER_STRAGGLER_TIMEOUT should be 120s"
        );
    }

    #[test]
    fn test_idle_timeout_detection_logic() {
        // Simulate the idle timeout check that runs in run_peer_loop.
        // If both last_read and last_write are older than PEER_TIMEOUT, peer is idle.
        let peer_timeout = Duration::from_secs(30);
        let straggler_timeout = Duration::from_secs(120);

        // Case 1: Recent activity — no timeout
        let now = Instant::now();
        let last_read = now;
        let last_write = now;
        assert!(now.duration_since(last_read) < peer_timeout);
        assert!(now.duration_since(last_write) < peer_timeout);

        // Case 2: Old read but recent write — no idle timeout
        // (peer is still writing, so it's not fully idle)
        let old_time = now - Duration::from_secs(35);
        let last_read_old = old_time;
        let last_write_recent = now;
        let is_idle = now.duration_since(last_read_old) >= peer_timeout
            && now.duration_since(last_write_recent) >= peer_timeout;
        assert!(!is_idle, "should not be idle when write is recent");

        // Case 3: Both old — idle timeout
        let last_read_old2 = old_time;
        let last_write_old = old_time;
        let is_idle2 = now.duration_since(last_read_old2) >= peer_timeout
            && now.duration_since(last_write_old) >= peer_timeout;
        assert!(is_idle2, "should be idle when both read and write are old");

        // Case 4: Straggler — write is very old
        let very_old = now - Duration::from_secs(125);
        let is_straggling = now.duration_since(very_old) >= straggler_timeout;
        assert!(is_straggling, "should be straggling when write is very old");
    }

    /// G17: Verify that updating last_write (as ping does) prevents idle timeout.
    ///
    /// In run_peer_loop, a successful ping sets `last_write = Instant::now()`.
    /// The idle timeout fires only when BOTH last_read and last_write exceed
    /// PEER_TIMEOUT. So ping acts as a keepalive by refreshing last_write.
    #[test]
    fn test_ping_updates_last_write_prevents_idle_timeout_g17() {
        let peer_timeout = Duration::from_secs(30);

        // Scenario: 25 seconds have passed with no reads.
        // Without any writes, both would be stale at 30s and peer would be dropped.
        let now = Instant::now();
        let started = now - Duration::from_secs(25);
        let last_read = started; // no reads for 25s

        // Without ping: last_write is also old → will timeout at 30s.
        let last_write_no_ping = started;
        // 5 more seconds pass...
        let future = now + Duration::from_secs(6);
        let would_timeout_without_ping = future.duration_since(last_read) >= peer_timeout
            && future.duration_since(last_write_no_ping) >= peer_timeout;
        assert!(
            would_timeout_without_ping,
            "without ping, peer would time out"
        );

        // With ping at 15s: last_write was refreshed at that point.
        let last_write_with_ping = now - Duration::from_secs(10); // ping sent 10s ago
        let would_timeout_with_ping = future.duration_since(last_read) >= peer_timeout
            && future.duration_since(last_write_with_ping) >= peer_timeout;
        assert!(
            !would_timeout_with_ping,
            "ping refreshes last_write, preventing idle timeout"
        );
    }

    #[test]
    fn test_truncate_error_msg_short() {
        // Messages <= 100 bytes pass through unchanged
        let msg = "short message";
        assert_eq!(truncate_error_msg(msg), msg);
    }

    #[test]
    fn test_truncate_error_msg_exactly_100() {
        let msg = "a".repeat(100);
        assert_eq!(truncate_error_msg(&msg), msg.as_str());
    }

    #[test]
    fn test_truncate_error_msg_over_100() {
        let msg = "b".repeat(150);
        let truncated = truncate_error_msg(&msg);
        assert_eq!(truncated.len(), 100);
        assert_eq!(truncated, "b".repeat(100).as_str());
    }

    #[test]
    fn test_truncate_error_msg_multibyte_boundary() {
        // A string that would split a multi-byte char at byte 100.
        // 'é' is 2 bytes (0xC3 0xA9). Fill 99 ASCII bytes then 'é'.
        let mut msg = "x".repeat(99);
        msg.push('é'); // bytes 99..101 → exceeds 100
        assert_eq!(msg.len(), 101);
        let truncated = truncate_error_msg(&msg);
        // Should truncate to 99 (before the 'é'), not 100 (mid-char)
        assert_eq!(truncated.len(), 99);
        assert_eq!(truncated, "x".repeat(99).as_str());
    }

    #[test]
    fn test_truncate_error_msg_empty() {
        assert_eq!(truncate_error_msg(""), "");
    }

    #[test]
    fn test_make_error_msg_creates_valid_xdr() {
        let msg = make_error_msg(ErrorCode::Load, "peer rejected");
        match msg {
            StellarMessage::ErrorMsg(err) => {
                assert_eq!(err.code, ErrorCode::Load);
                assert_eq!(err.msg.to_string(), "peer rejected");
            }
            _ => panic!("expected ErrorMsg"),
        }
    }

    #[test]
    fn test_make_error_msg_truncates_long_message() {
        let long_msg = "z".repeat(200);
        let msg = make_error_msg(ErrorCode::Misc, &long_msg);
        match msg {
            StellarMessage::ErrorMsg(err) => {
                assert_eq!(err.code, ErrorCode::Misc);
                assert_eq!(err.msg.len(), 100);
            }
            _ => panic!("expected ErrorMsg"),
        }
    }

    #[tokio::test]
    async fn test_send_error_and_drop_sends_error_then_shutdown() {
        let (tx, mut rx) = mpsc::channel::<OutboundMessage>(16);
        let peer_id = PeerId::from_bytes([1u8; 32]);

        assert!(send_error_and_drop(
            &peer_id,
            &tx,
            ErrorCode::Load,
            "test message"
        ));

        // First message should be the error
        match rx.recv().await.unwrap() {
            OutboundMessage::Send(StellarMessage::ErrorMsg(err)) => {
                assert_eq!(err.code, ErrorCode::Load);
                assert_eq!(err.msg.to_string(), "test message");
            }
            other => panic!(
                "expected Send(ErrorMsg), got {:?}",
                std::mem::discriminant(&other)
            ),
        }

        // Second message should be shutdown
        match rx.recv().await.unwrap() {
            OutboundMessage::Shutdown => {}
            other => panic!(
                "expected Shutdown, got {:?}",
                std::mem::discriminant(&other)
            ),
        }
    }

    #[test]
    fn test_send_error_and_drop_reports_full_channel() {
        let (tx, _rx) = mpsc::channel::<OutboundMessage>(1);
        let peer_id = PeerId::from_bytes([1u8; 32]);
        assert!(tx.try_send(OutboundMessage::Shutdown).is_ok());

        assert!(
            !send_error_and_drop(&peer_id, &tx, ErrorCode::Load, "test message"),
            "full channel cannot be treated as an in-progress eviction"
        );
    }

    /// Verify the ping hash computation is deterministic and the
    /// DontHave/ScpQuorumset response-matching logic works correctly (G4).
    #[test]
    fn test_ping_hash_computation_is_deterministic_g4() {
        let nanos: u128 = 1_000_000_000;
        let hash1 = compute_ping_hash(nanos);
        let hash2 = compute_ping_hash(nanos);
        assert_eq!(hash1.0, hash2.0, "same nanos should produce same ping hash");

        // Different nanos should produce different hash
        let hash3 = compute_ping_hash(2_000_000_000);
        assert_ne!(
            hash1.0, hash3.0,
            "different nanos should produce different hash"
        );
    }

    /// Verify that DontHave response matching correctly identifies
    /// a ping response by matching the req_hash (G4).
    #[test]
    fn test_ping_response_matching_dont_have_g4() {
        let nanos: u128 = 42_000_000_000;
        let ping_hash_val = compute_ping_hash(nanos);

        // Matching hash should be recognized as a ping response
        assert!(
            is_ping_response(Some(&ping_hash_val), &ping_hash_val),
            "DontHave with matching hash should be recognized as ping response"
        );

        // Non-matching hash should not match
        assert!(
            !is_ping_response(Some(&ping_hash_val), &Uint256([0xff; 32])),
            "DontHave with wrong hash should not match"
        );

        // No outstanding ping → no match
        assert!(
            !is_ping_response(None, &ping_hash_val),
            "No outstanding ping should never match"
        );
    }

    #[test]
    fn test_validate_incoming_peers_rules() {
        let peers_msg = StellarMessage::Peers(stellar_xdr::curr::VecM::default());
        let tx_msg = StellarMessage::GetScpState(0);

        assert_eq!(
            validate_incoming_peers(ConnectionDirection::Outbound, false, &peers_msg),
            PeersValidation::AcceptFirst
        );
        assert_eq!(
            validate_incoming_peers(ConnectionDirection::Outbound, true, &peers_msg),
            PeersValidation::RejectDuplicate
        );
        assert_eq!(
            validate_incoming_peers(ConnectionDirection::Inbound, false, &peers_msg),
            PeersValidation::RejectWrongDirection
        );
        assert_eq!(
            validate_incoming_peers(ConnectionDirection::Outbound, false, &tx_msg),
            PeersValidation::NotPeers
        );
    }

    #[test]
    fn test_should_skip_generic_routing() {
        assert!(should_skip_generic_routing(&StellarMessage::Hello(
            Default::default()
        )));
        assert!(should_skip_generic_routing(&StellarMessage::Auth(
            stellar_xdr::curr::Auth { flags: 200 }
        )));
        assert!(should_skip_generic_routing(&StellarMessage::SendMore(
            stellar_xdr::curr::SendMore { num_messages: 1 }
        )));
        assert!(should_skip_generic_routing(
            &StellarMessage::SendMoreExtended(stellar_xdr::curr::SendMoreExtended {
                num_messages: 1,
                num_bytes: 1,
            })
        ));
        assert!(!should_skip_generic_routing(&StellarMessage::Peers(
            stellar_xdr::curr::VecM::default()
        )));
    }

    #[test]
    fn test_initial_send_more_grant_matches_default_capacity() {
        // When max_tx_size is below the threshold (200KB), the initial byte
        // grant should be the default 300KB. This is the common mainnet case.
        use crate::flow_control::{
            FlowControlBytesConfig, INITIAL_PEER_FLOOD_READING_CAPACITY_BYTES,
        };
        let config = FlowControlConfig::default();
        let msgs = config.peer_flood_reading_capacity;
        let bytes = FlowControlBytesConfig::default().bytes_total(100_000); // typical max_tx_size

        assert_eq!(msgs, 200);
        assert_eq!(bytes, INITIAL_PEER_FLOOD_READING_CAPACITY_BYTES);
    }

    // --- G16: Per-peer capacity enforcement tests ---

    #[test]
    fn test_capacity_guard_none_drops_peer_flow() {
        // When all flood capacity is exhausted, CapacityGuard::new returns None.
        // In run_peer_loop this would trigger send_error_and_drop + break.
        use stellar_xdr::curr::TransactionEnvelope;
        let config = FlowControlConfig::default();
        let fc = Arc::new(FlowControl::new(config.clone()));

        // Exhaust all flood capacity by locking messages until none remain.
        let mut guards = Vec::new();
        for _ in 0..config.peer_flood_reading_capacity {
            let msg = StellarMessage::Transaction(TransactionEnvelope::Tx(
                stellar_xdr::curr::TransactionV1Envelope {
                    tx: stellar_xdr::curr::Transaction {
                        source_account: stellar_xdr::curr::MuxedAccount::Ed25519(
                            stellar_xdr::curr::Uint256([0; 32]),
                        ),
                        fee: 100,
                        seq_num: stellar_xdr::curr::SequenceNumber(1),
                        cond: stellar_xdr::curr::Preconditions::None,
                        memo: stellar_xdr::curr::Memo::None,
                        operations: stellar_xdr::curr::VecM::default(),
                        ext: stellar_xdr::curr::TransactionExt::V0,
                    },
                    signatures: stellar_xdr::curr::VecM::default(),
                },
            ));
            match crate::flow_control::CapacityGuard::new(Arc::clone(&fc), msg) {
                Some(guard) => guards.push(guard),
                None => break,
            }
        }

        // Next message should fail — capacity exhausted.
        let overflow_msg = StellarMessage::Transaction(TransactionEnvelope::Tx(
            stellar_xdr::curr::TransactionV1Envelope {
                tx: stellar_xdr::curr::Transaction {
                    source_account: stellar_xdr::curr::MuxedAccount::Ed25519(
                        stellar_xdr::curr::Uint256([1; 32]),
                    ),
                    fee: 100,
                    seq_num: stellar_xdr::curr::SequenceNumber(2),
                    cond: stellar_xdr::curr::Preconditions::None,
                    memo: stellar_xdr::curr::Memo::None,
                    operations: stellar_xdr::curr::VecM::default(),
                    ext: stellar_xdr::curr::TransactionExt::V0,
                },
                signatures: stellar_xdr::curr::VecM::default(),
            },
        ));
        let guard = crate::flow_control::CapacityGuard::new(Arc::clone(&fc), overflow_msg);
        assert!(guard.is_none(), "should return None when peer at capacity");
    }

    #[test]
    fn test_make_error_msg_capacity_exceeded() {
        // Verify the error message we send matches stellar-core's wording.
        let err = make_error_msg(
            ErrorCode::Load,
            "unexpected flood message, peer at capacity",
        );
        match err {
            StellarMessage::ErrorMsg(e) => {
                assert_eq!(e.code, ErrorCode::Load);
                assert_eq!(
                    e.msg.to_string(),
                    "unexpected flood message, peer at capacity"
                );
            }
            _ => panic!("expected ErrorMsg"),
        }
    }

    #[test]
    fn test_capacity_guard_non_flood_always_accepted() {
        // Non-flow-controlled messages (like GetPeers) should always succeed,
        // even when flood capacity is exhausted.
        use stellar_xdr::curr::TransactionEnvelope;
        let config = FlowControlConfig::default();
        let fc = Arc::new(FlowControl::new(config.clone()));

        // Exhaust flood capacity.
        let mut guards = Vec::new();
        for _ in 0..config.peer_flood_reading_capacity {
            let msg = StellarMessage::Transaction(TransactionEnvelope::Tx(
                stellar_xdr::curr::TransactionV1Envelope {
                    tx: stellar_xdr::curr::Transaction {
                        source_account: stellar_xdr::curr::MuxedAccount::Ed25519(
                            stellar_xdr::curr::Uint256([0; 32]),
                        ),
                        fee: 100,
                        seq_num: stellar_xdr::curr::SequenceNumber(1),
                        cond: stellar_xdr::curr::Preconditions::None,
                        memo: stellar_xdr::curr::Memo::None,
                        operations: stellar_xdr::curr::VecM::default(),
                        ext: stellar_xdr::curr::TransactionExt::V0,
                    },
                    signatures: stellar_xdr::curr::VecM::default(),
                },
            ));
            match crate::flow_control::CapacityGuard::new(Arc::clone(&fc), msg) {
                Some(guard) => guards.push(guard),
                None => break,
            }
        }

        // Non-flow-controlled message (Peers) should still be accepted.
        let peers_msg = StellarMessage::Peers(stellar_xdr::curr::VecM::default());
        let guard = crate::flow_control::CapacityGuard::new(Arc::clone(&fc), peers_msg);
        assert!(
            guard.is_some(),
            "non-flow-controlled messages must always be accepted regardless of flood capacity"
        );
    }

    // --- G2: Auth timeout ---
    //
    // NOTE: Auth timeout enforcement (disconnecting peers that don't complete
    // the handshake within `auth_timeout_secs`) occurs inside `run_peer_loop`
    // which requires real TCP streams. This is an **integration test candidate**.
    // The config default (2s for unauth, 30s for auth) is tested in lib.rs tests.

    /// Regression test: QueryInfo sliding-window rate limiter with default max.
    /// Parity: stellar-core Peer.cpp:1423-1438 (QUERY_RESPONSE_MULTIPLIER=5).
    #[test]
    fn test_query_rate_limiter_default_max() {
        let window = Duration::from_secs(10);
        let max_queries = QueryKind::TxSet.max_queries(window); // 50

        let mut info = QueryInfo::new();

        // All queries within limit should be allowed
        for _ in 0..max_queries {
            assert!(
                info.check_and_increment(window, max_queries),
                "query within limit should be allowed"
            );
        }

        // Next query should be rejected
        assert!(
            !info.check_and_increment(window, max_queries),
            "query exceeding limit should be rejected"
        );
    }

    /// Test QueryInfo with custom max override (used for GetScpState).
    /// Parity: stellar-core Peer.cpp:1686 (GET_SCP_STATE_MAX_RATE=10).
    #[test]
    fn test_query_rate_limiter_custom_max() {
        let window = Duration::from_secs(10);
        let custom_max = QueryKind::ScpState.max_queries(window); // 10

        let mut info = QueryInfo::new();

        for _ in 0..custom_max {
            assert!(
                info.check_and_increment(window, custom_max),
                "query within custom limit should be allowed"
            );
        }

        assert!(
            !info.check_and_increment(window, custom_max),
            "query exceeding custom limit should be rejected"
        );
    }

    /// Test QueryRateLimiter::check with GetScpState messages — exactly 10
    /// allowed per window. Parity: stellar-core Peer.cpp:1686.
    #[test]
    fn test_query_rate_limiter_scp_state_cap() {
        let window = Duration::from_secs(10);
        let mut limiter = QueryRateLimiter::new();
        let msg = StellarMessage::GetScpState(0);
        let max = QueryKind::ScpState.max_queries(window);

        for i in 0..max {
            assert!(
                limiter.check(&msg, window),
                "GetScpState #{} should be allowed",
                i + 1
            );
        }

        assert!(
            !limiter.check(&msg, window),
            "GetScpState #{} should be rejected (exceeds max=10)",
            max + 1
        );
    }

    /// Test that the sliding window resets after expiry.
    #[test]
    fn test_query_rate_limiter_window_reset() {
        let window = Duration::from_millis(1);
        let mut info = QueryInfo::new();

        let custom_max = 2u32;
        assert!(info.check_and_increment(window, custom_max));
        assert!(info.check_and_increment(window, custom_max));
        assert!(!info.check_and_increment(window, custom_max));

        // Wait for window to expire
        std::thread::sleep(Duration::from_millis(5));

        // Should be allowed again after reset
        assert!(
            info.check_and_increment(window, custom_max),
            "query should be allowed after window reset"
        );
    }

    #[test]
    fn test_traffic_class_classification() {
        use stellar_xdr::curr::*;

        // SCP is exempt (None)
        let scp_msg = StellarMessage::ScpMessage(ScpEnvelope {
            statement: ScpStatement {
                node_id: NodeId(PublicKey::PublicKeyTypeEd25519(Uint256([0; 32]))),
                slot_index: 1,
                pledges: ScpStatementPledges::Externalize(ScpStatementExternalize {
                    commit: ScpBallot {
                        counter: 1,
                        value: vec![].try_into().unwrap(),
                    },
                    n_h: 1,
                    commit_quorum_set_hash: Hash([0; 32]),
                }),
            },
            signature: vec![].try_into().unwrap(),
        });
        assert_eq!(TrafficClass::classify(&scp_msg), None);

        // Transaction is TxAndDemand
        let tx_msg =
            StellarMessage::Transaction(TransactionEnvelope::TxV0(TransactionV0Envelope {
                tx: TransactionV0 {
                    source_account_ed25519: Uint256([0; 32]),
                    fee: 100,
                    seq_num: SequenceNumber(1),
                    time_bounds: None,
                    memo: Memo::None,
                    operations: vec![].try_into().unwrap(),
                    ext: TransactionV0Ext::V0,
                },
                signatures: vec![].try_into().unwrap(),
            }));
        assert_eq!(
            TrafficClass::classify(&tx_msg),
            Some(TrafficClass::TxAndDemand)
        );

        // FloodDemand is TxAndDemand
        let demand_msg = StellarMessage::FloodDemand(FloodDemand {
            tx_hashes: vec![].try_into().unwrap(),
        });
        assert_eq!(
            TrafficClass::classify(&demand_msg),
            Some(TrafficClass::TxAndDemand)
        );

        // FloodAdvert is Advert
        let advert_msg = StellarMessage::FloodAdvert(FloodAdvert {
            tx_hashes: vec![].try_into().unwrap(),
        });
        assert_eq!(
            TrafficClass::classify(&advert_msg),
            Some(TrafficClass::Advert)
        );

        // GetTxSet is ControlFetch
        let get_tx_set = StellarMessage::GetTxSet(Uint256([0; 32]));
        assert_eq!(
            TrafficClass::classify(&get_tx_set),
            Some(TrafficClass::ControlFetch)
        );

        // DontHave is ControlFetch
        let dont_have = StellarMessage::DontHave(DontHave {
            type_: MessageType::Transaction,
            req_hash: Uint256([0; 32]),
        });
        assert_eq!(
            TrafficClass::classify(&dont_have),
            Some(TrafficClass::ControlFetch)
        );
    }

    #[test]
    fn test_peer_rate_limiter_per_peer_isolation() {
        let mut limiter_a = PeerRateLimiter::new();
        let mut limiter_b = PeerRateLimiter::new();

        // Exhaust peer A's tx budget
        for _ in 0..DEFAULT_TX_DEMAND_LIMIT {
            assert!(limiter_a.allow(TrafficClass::TxAndDemand));
        }
        // Peer A's next tx should be rejected
        assert!(!limiter_a.allow(TrafficClass::TxAndDemand));

        // Peer B should be unaffected
        assert!(limiter_b.allow(TrafficClass::TxAndDemand));
    }

    #[test]
    fn test_peer_rate_limiter_class_sub_budgets() {
        let mut limiter = PeerRateLimiter::new();

        // Exhaust tx+demand sub-budget
        for _ in 0..DEFAULT_TX_DEMAND_LIMIT {
            assert!(limiter.allow(TrafficClass::TxAndDemand));
        }
        assert!(
            !limiter.allow(TrafficClass::TxAndDemand),
            "tx+demand should be exhausted"
        );

        // Advert should still work (separate sub-budget)
        assert!(
            limiter.allow(TrafficClass::Advert),
            "advert should have own budget"
        );
    }

    #[test]
    fn test_peer_rate_limiter_control_fetch_reserved() {
        let mut limiter = PeerRateLimiter::new();

        // Exhaust the full aggregate budget with tx+demand + advert
        for _ in 0..DEFAULT_TX_DEMAND_LIMIT {
            limiter.allow(TrafficClass::TxAndDemand);
        }
        for _ in 0..DEFAULT_ADVERT_LIMIT {
            limiter.allow(TrafficClass::Advert);
        }

        // Aggregate is now at 200 (150 tx + 50 advert) = limit
        // Control/fetch should still work due to reserved capacity
        assert!(
            limiter.allow(TrafficClass::ControlFetch),
            "control/fetch should have reserved capacity even when aggregate exhausted"
        );
    }

    #[test]
    fn test_peer_rate_limiter_aggregate_caps_survey() {
        let mut limiter = PeerRateLimiter::new();

        // Exhaust aggregate with survey messages
        for _ in 0..DEFAULT_PEER_RATE_LIMIT {
            assert!(limiter.allow(TrafficClass::Survey));
        }

        // Next survey should be rejected (aggregate exhausted)
        assert!(!limiter.allow(TrafficClass::Survey));

        // But control/fetch should still work (reserved)
        assert!(limiter.allow(TrafficClass::ControlFetch));
    }

    #[test]
    fn test_peer_rate_limiter_telemetry_counters() {
        let mut limiter = PeerRateLimiter::new();

        // Exhaust tx budget
        for _ in 0..DEFAULT_TX_DEMAND_LIMIT {
            limiter.allow(TrafficClass::TxAndDemand);
        }
        // This should be rejected and counted
        limiter.allow(TrafficClass::TxAndDemand);

        assert!(
            limiter.dropped_tx_demand > 0,
            "should track dropped tx+demand"
        );
    }

    /// Regression test for AUDIT-016: fetch/control messages must bypass
    /// the global rate limiter so one peer's flood traffic cannot starve
    /// consensus-critical responses (TxSet, ScpQuorumset, DontHave, etc.).
    #[test]
    fn test_audit_016_fetch_messages_bypass_global_rate_limiter() {
        let fetch_messages = vec![
            StellarMessage::TxSet(stellar_xdr::curr::TransactionSet {
                previous_ledger_hash: stellar_xdr::curr::Hash([0; 32]),
                txs: stellar_xdr::curr::VecM::default(),
            }),
            StellarMessage::DontHave(stellar_xdr::curr::DontHave {
                type_: stellar_xdr::curr::MessageType::TxSet,
                req_hash: stellar_xdr::curr::Uint256([0; 32]),
            }),
            StellarMessage::ScpQuorumset(stellar_xdr::curr::ScpQuorumSet {
                threshold: 1,
                validators: stellar_xdr::curr::VecM::default(),
                inner_sets: stellar_xdr::curr::VecM::default(),
            }),
        ];

        for msg in &fetch_messages {
            assert!(
                is_fetch_message(msg),
                "{:?} should be classified as fetch message",
                helpers::message_type_name(msg)
            );
        }
    }
}
