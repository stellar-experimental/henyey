//! Per-peer event loop and message routing.
//!
//! Contains `run_peer_loop`, message routing, flow control handling,
//! ping/RTT tracking, timeout checks, and related helpers.

use super::{OutboundMessage, OverlayManager, OverlayMessage, SharedPeerState};
use crate::connection::ConnectionDirection;
use crate::{
    codec::helpers,
    flood::compute_message_hash,
    flow_control::{msg_body_size, FlowControl},
    metrics::OverlayMessageKind,
    peer::Peer,
    PeerId,
};
use sha2::Digest;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use stellar_xdr::{ErrorCode, SError, StellarMessage, StringM, Uint256};
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TrySendError;
use tracing::{debug, info, trace, warn};

/// Maximum length for error messages sent to peers, matching the XDR
/// `string msg<100>` constraint in the `Error` struct.
const MAX_ERROR_MESSAGE_LEN: usize = 100;

use crate::query_policy::QueryKind;

/// Who initiated an inbound peer drop, used to segment the
/// `stellar_overlay_inbound_drop_total` counter into remote- vs
/// henyey-initiated siblings (#3422). This is a passive classification read out
/// of `run_peer_loop`'s exit path — it adds NO drop/`break` decision and changes
/// no protocol, handshake, auth, codec, or peer-lifecycle behavior.
///
/// Classification convention (matches the #3419 operator question "did peers
/// churn out, or did henyey stop talking?"):
/// - `Remote`: the peer closed/reset the socket — `recv` returned `Ok(None)`
///   (peer FIN) or a recv error (RST / errno 104).
/// - `Local`: henyey broke the loop — idle/straggler timeout, send / flood-send
///   error, protocol-violation or received-ERROR_MSG teardown, shutdown, or
///   channel close. `send_error` is attributed `Local` by the "who broke the
///   loop" convention even though the underlying cause may be a remote RST
///   surfaced on the next write: the operator signal is "henyey's send path
///   gave up."
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(super) enum DropInitiator {
    /// The remote peer closed or reset the connection.
    Remote,
    /// Henyey broke the peer loop. Default: any unattributed exit is henyey-side.
    #[default]
    Local,
}

impl DropInitiator {
    /// Map a `log_inbound_drop_diag` `reason` string to its initiator. Only the
    /// two remote-origin reasons (`remote_closed`, `recv_error`) are `Remote`;
    /// every other reason — and any unknown string — is `Local`. Kept as a pure
    /// function so the attribution is unit-testable without a live socket.
    pub(super) fn from_reason(reason: &str) -> Self {
        match reason {
            "remote_closed" | "recv_error" => DropInitiator::Remote,
            _ => DropInitiator::Local,
        }
    }
}

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

// maxtps (#flood-coverage-gap): these per-peer limits are a COARSE Sybil/DoS
// backstop ONLY — the real per-peer flood backpressure is flow control
// (SEND_MORE / `peer_flood_reading_capacity`), exactly as in stellar-core (which
// has NO per-peer message-count limiter). The previous values (200/150/50)
// hard-dropped legitimate flood under load: at >~250 classic tx/s the fulfill +
// demand traffic concentrates on the peers holding demanded txns and exceeds
// 150 TxAndDemand/s, so the limiter silently dropped fulfills/demands → those
// txns never reached the peer → coverage gap → the tx aged out of the queue at
// pending_depth=4 (never nominated) → loadgen accounts stranded → run failed.
// That capped the measured MaxTPSClassic ceiling at ~219 even though apply/flood
// throughput is ~400+/s. The limits are raised far above any legitimate
// per-peer flood rate (flow control + the global flood-gate still protect) so
// the limiter never drops legitimate traffic; it only catches a peer flooding
// orders of magnitude beyond the network's tx capacity.
/// Default per-peer aggregate message budget per second (coarse Sybil backstop).
pub(crate) const DEFAULT_PEER_RATE_LIMIT: u32 = 12000;
/// Default per-peer tx + demand sub-budget per second (coarse Sybil backstop;
/// flow control is the real flood bound).
const DEFAULT_TX_DEMAND_LIMIT: u32 = 10000;
/// Default per-peer advert sub-budget per second (coarse Sybil backstop).
const DEFAULT_ADVERT_LIMIT: u32 = 4000;
/// Default per-peer control/fetch reserved minimum per second.
const DEFAULT_CONTROL_FETCH_LIMIT: u32 = 500;

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
                // Survey has no per-class drop counter (aggregate-only by
                // design); the unconditional increment below records the
                // single aggregate-cap drop, matching the other classes.
                TrafficClass::Survey => {}
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

/// Delay before closing the socket on the error-drop path, after the final
/// `ERROR_MSG` has been flushed.
///
/// §12.3 / TCPPeer.cpp:849: `expires_from_now(std::chrono::seconds(5))`.
const ERROR_DROP_DRAIN_DELAY: Duration = Duration::from_secs(5);

/// Poll interval used to make the error-drop drain delay interruptible by
/// node shutdown without depending on a per-peer shutdown channel.
const ERROR_DROP_DRAIN_POLL: Duration = Duration::from_millis(100);

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

/// Sanitize a received ERROR_MSG body for logging (OVERLAY §7.1.3-1).
///
/// Mirrors stellar-core `Peer::recvError` (Peer.cpp:1698-1733): the message is
/// built with `std::transform` over the raw `msg.error().msg` bytes using
/// `(isAsciiAlphaNumeric(c) || c == ' ') ? c : '*'`. Every byte that is not
/// ASCII-alphanumeric and not a space — including control chars, ANSI escape
/// sequences, DEL (0x7f), and high bytes (> 0x7f) — is replaced with `*`.
///
/// IMPORTANT: this operates on the raw `StringM` bytes (`&err.msg[..]`), NOT on
/// `StringM::to_string()`, which already escapes non-printable bytes via
/// `escape_bytes` (e.g. `0x1b` renders as the 4 chars `\x1b`). Sanitizing the
/// escaped string would not match stellar-core's per-byte transform.
pub(crate) fn sanitize_error_msg(raw: &[u8]) -> String {
    raw.iter()
        .map(|&b| {
            if b.is_ascii_alphanumeric() || b == b' ' {
                b as char
            } else {
                '*'
            }
        })
        .collect()
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
    // §12.3 / TCPPeer.cpp:835-862: use the deferred-shutdown variant so the
    // loop flushes the queued ERROR_MSG and then waits 5 s before closing the
    // socket, letting the peer actually receive the error. The `Send(err)` is
    // dequeued and flushed before `ShutdownAfterError` (same channel, FIFO),
    // so the drain delay is guaranteed to run post-flush.
    let shutdown_queued = match outbound_tx.try_send(OutboundMessage::ShutdownAfterError) {
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
fn compute_ping_hash(nanos: u128) -> stellar_xdr::Uint256 {
    let mut hasher = sha2::Sha256::new();
    hasher.update(nanos.to_le_bytes());
    let result = hasher.finalize();
    stellar_xdr::Uint256(result.into())
}

/// Check if a received hash matches an outstanding ping hash.
///
/// Returns true if both `ping_sent_time` and `ping_hash` are `Some` and the
/// received `hash_bytes` matches the stored ping hash.
///
/// Extracted from `run_peer_loop` ping response matching for testability (G4).
fn is_ping_response(ping_hash: Option<&stellar_xdr::Uint256>, hash: &stellar_xdr::Uint256) -> bool {
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
    hash: Option<stellar_xdr::Uint256>,
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
    fn record_sent(&mut self, hash: stellar_xdr::Uint256) {
        self.sent_time = Some(Instant::now());
        self.hash = Some(hash);
    }

    /// Check whether `response_hash` matches the outstanding ping.
    /// If so, record the RTT and clear the outstanding ping. Returns
    /// the RTT if this was a match.
    fn check_response(
        &mut self,
        response_hash: &stellar_xdr::Uint256,
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
        // Issue #2621 B4: Record ping RTT histogram at event site.
        metrics::histogram!("stellar_overlay_connection_latency_seconds").record(rtt.as_secs_f64());
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
    /// #3643 straggler parity: loop-local enqueue time of the newest message in
    /// the last batch actually written to this peer (`mEnqueueTimeOfLastWrite`,
    /// `TCPPeer.cpp:329`). Advanced by the SEND_MORE_EXTENDED drain path so the
    /// straggler timeout keys on throughput deficit, not completed-write time.
    enqueue_time_of_last_write: &'a mut Instant,
    /// #3570 observability: loop-local count of SCP envelopes henyey has
    /// written to this peer (accumulated from SEND_MORE-triggered drains).
    scp_written: &'a mut u64,
    /// Clone of this peer's outbound sender. Attached to the drain-gated
    /// flow-control release token (#3625) so the app SCP consumer can enqueue
    /// the `SEND_MORE_EXTENDED` grant back to this peer at the batch boundary.
    outbound_tx: &'a mpsc::Sender<OutboundMessage>,
}

/// Read-only timing and message counters for timeout checks.
struct PeerTimingInfo {
    last_read: Instant,
    last_write: Instant,
    /// #3643 straggler parity: the enqueue time of the *newest* message in the
    /// most recent batch actually written to this peer. Mirrors stellar-core's
    /// `mEnqueueTimeOfLastWrite` (`TCPPeer.cpp:329`, last-wins over the written
    /// FIFO prefix; init `now()` at `Peer.cpp:144`). The straggler timeout keys
    /// on this — NOT on `last_write` (completed-write time) — so a peer that
    /// keeps writing but cannot keep up (even its newest batched message is
    /// already >120s old) is flagged, while a peer with a deep-but-draining
    /// queue is not. Idle (`last_read`/`last_write`) and no-outbound-capacity
    /// (60s) stay distinct signals.
    enqueue_time_of_last_write: Instant,
    total_messages: u64,
    scp_messages: u64,
}

/// #3570 observability: which timeout fired when a peer is dropped.
///
/// Carried out of [`OverlayManager::check_peer_timeouts`] (which previously
/// returned a bare `bool`) so the inbound-drop diagnostic can record the
/// specific reason. The drop *conditions* and metric increments are unchanged
/// — this only labels which one tripped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum IdleReason {
    /// No read AND no write for >= PEER_TIMEOUT (30s). Matches Peer.cpp:450-451.
    Idle,
    /// No write for >= PEER_STRAGGLER_TIMEOUT (120s).
    Straggler,
    /// No outbound capacity (no SEND_MORE_EXTENDED) for >= 60s. OVERLAY_SPEC §5.6.
    SendModeIdle,
}

impl IdleReason {
    /// Short, low-cardinality label for the `overlay_inbound_diag` warn field.
    fn as_str(self) -> &'static str {
        match self {
            IdleReason::Idle => "idle",
            IdleReason::Straggler => "straggler",
            IdleReason::SendModeIdle => "send_mode_idle",
        }
    }
}

/// #3570 observability: per-inbound-peer flow-control snapshot gathered at the
/// idle-timeout drop, so the operator can pin WHY each inbound peer idles out.
///
/// All fields are reads of already-tracked state — building this struct has no
/// effect on any drop condition, SEND_MORE grant, or flood behavior. The
/// leading-hypothesis signature is: `idle_reason ∈ {idle, send_mode_idle}` +
/// `send_more_received == 0` + `peer_message_capacity == 0` + `scp_written == 0`
/// + `scp_queue_depth > 0` (we had SCP to flood but were blocked on capacity).
#[derive(Debug, Clone)]
pub(super) struct InboundDropDiag {
    /// SEND_MORE_EXTENDED grants received from the peer (0 = never granted).
    pub send_more_received: u64,
    /// SCP envelopes henyey actually wrote to this peer over the connection.
    pub scp_written: u64,
    /// SCP messages still queued (had SCP to send but blocked on capacity).
    pub scp_queue_depth: usize,
    /// Peer's currently-granted outbound message capacity (0 = exhausted/none).
    pub peer_message_capacity: u64,
    /// Peer's currently-granted outbound byte capacity.
    pub peer_bytes_capacity: u64,
    /// Seconds since henyey last wrote to this peer.
    pub last_write_age_secs: u64,
    /// Seconds since henyey last read from this peer.
    pub last_read_age_secs: u64,
    /// Which timeout fired.
    pub idle_reason: IdleReason,
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
    /// Returns `Some(IdleReason)` identifying which timeout fired (and thus that
    /// the peer should be dropped), or `None` if the peer is still live. The
    /// drop conditions and metric increments are unchanged from the prior
    /// `bool`-returning form; the reason is carried out solely so the #3570
    /// inbound-drop diagnostic at the call site can label the cause.
    fn check_peer_timeouts(
        peer_id: &PeerId,
        timing: &PeerTimingInfo,
        flow_control: &FlowControl,
        metrics: &crate::metrics::OverlayMetrics,
    ) -> Option<IdleReason> {
        const PEER_TIMEOUT: Duration = Duration::from_secs(30);
        const PEER_STRAGGLER_TIMEOUT: Duration = Duration::from_secs(120);
        // OVERLAY_SPEC §5.6 — drop peer if no SEND_MORE_EXTENDED for this long.
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
            return Some(IdleReason::Idle);
        }
        // #3643 straggler parity: key on the enqueue time of the newest message
        // in the last batch actually written — NOT `last_write` (completed-write
        // time, which resets to `now()` on every write). Mirrors stellar-core
        // `(now - mEnqueueTimeOfLastWrite) >= PEER_STRAGGLER_TIMEOUT`
        // (`Peer.cpp:462`). A peer that keeps writing but cannot keep up (even
        // its newest batched message is already >120s old) is a straggler; a
        // peer with a deep-but-draining queue is not. The idle (last_read/
        // last_write) and no-outbound-capacity (60s) signals stay distinct.
        if now.duration_since(timing.enqueue_time_of_last_write) >= PEER_STRAGGLER_TIMEOUT {
            warn!(
                "Dropping peer {} due to straggler timeout (total_msgs={}, scp_msgs={})",
                peer_id, timing.total_messages, timing.scp_messages
            );
            metrics.timeouts_straggler.inc();
            return Some(IdleReason::Straggler);
        }
        if flow_control.no_outbound_capacity_timeout(PEER_SEND_MODE_IDLE_TIMEOUT_SECS) {
            warn!(
                "Dropping peer {} due to PEER_SEND_MODE_IDLE_TIMEOUT (no SEND_MORE_EXTENDED for {}s)",
                peer_id, PEER_SEND_MODE_IDLE_TIMEOUT_SECS
            );
            metrics.timeouts_idle.inc();
            return Some(IdleReason::SendModeIdle);
        }
        None
    }

    /// #3570 observability: build the [`InboundDropDiag`] snapshot for a peer
    /// being dropped on a timeout. Pure read of already-tracked state — this
    /// has no effect on any drop condition, SEND_MORE grant, or flood behavior.
    ///
    /// `last_read`/`last_write` are the loop-local `Instant`s; `scp_written` is
    /// the loop-local count of SCP envelopes henyey wrote to this peer.
    fn build_inbound_drop_diag(
        flow_control: &FlowControl,
        last_read: Instant,
        last_write: Instant,
        scp_written: u64,
        idle_reason: IdleReason,
    ) -> InboundDropDiag {
        let stats = flow_control.get_stats();
        InboundDropDiag {
            send_more_received: stats.send_more_received_count,
            scp_written,
            scp_queue_depth: stats.scp_queue_size,
            peer_message_capacity: stats.peer_message_capacity,
            peer_bytes_capacity: stats.peer_bytes_capacity,
            last_write_age_secs: last_write.elapsed().as_secs(),
            last_read_age_secs: last_read.elapsed().as_secs(),
            idle_reason,
        }
    }

    /// #3570 observability: emit the per-inbound-peer idle-drop diagnostic.
    /// Inbound-gated, mirroring [`Self::log_inbound_drop_diag`]. Reads only
    /// already-tracked state; does not alter control flow.
    fn log_inbound_idle_drop_diag(peer: &Peer, peer_id: &PeerId, diag: &InboundDropDiag) {
        if peer.direction() != ConnectionDirection::Inbound {
            return;
        }
        warn!(
            target: "overlay_inbound_diag",
            peer_id = %peer_id,
            addr = %peer.remote_addr(),
            direction = "inbound",
            authenticated = peer.is_ready(),
            idle_reason = diag.idle_reason.as_str(),
            send_more_received = diag.send_more_received,
            scp_written = diag.scp_written,
            scp_queue_depth = diag.scp_queue_depth,
            peer_message_capacity = diag.peer_message_capacity,
            peer_bytes_capacity = diag.peer_bytes_capacity,
            last_write_age_secs = diag.last_write_age_secs,
            last_read_age_secs = diag.last_read_age_secs,
            "inbound peer dropped on idle timeout; per-peer flow-control signals recorded"
        );
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
    /// should be dropped), or `Ok(BatchSendOutcome)` describing the drain
    /// (whether anything was written and how many SCP envelopes).
    async fn handle_send_more_extended(
        peer: &mut Peer,
        peer_id: &PeerId,
        message: &StellarMessage,
        flow_control: &FlowControl,
        metrics: &crate::metrics::OverlayMetrics,
    ) -> std::result::Result<BatchSendOutcome, ()> {
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
                Ok(outcome) => Ok(outcome),
                Err(e) => {
                    debug!("Failed to drain queue to {}: {}", peer_id, e);
                    Err(())
                }
            }
        } else {
            Ok(BatchSendOutcome {
                sent: false,
                scp_written: 0,
                newest_emplaced: None,
            })
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
        // #3625: drain-gated release token for the SCP path. `Some` only when
        // `message` is an SCP envelope. If routing drops the message early
        // (rate-limit, watcher-shed, not-synced, etc.) the token is simply
        // dropped here, which releases the held capacity immediately — matching
        // stellar-core, where a dropped message releases its capacity right
        // away. On the SCP routing path the token is moved onto the
        // `OverlayMessage` so the release defers to consumer-drain time.
        mut flow_release: Option<super::FlowControlRelease>,
    ) -> Option<bool> {
        // Parity: shouldAbort (Peer.cpp:1157-1160) — skip message processing
        // if the overlay is shutting down.
        if !state.running.load(Ordering::Relaxed) {
            return Some(false);
        }

        let msg_type = helpers::message_type_name(message);

        // Set inside the flood-gate-tracked block for SCP envelopes; threaded
        // onto the routed `OverlayMessage` so the app consumer reuses the
        // already-computed hash and the already-claimed in-flight token
        // (maxtps iter 7).
        let mut scp_msg_hash: Option<henyey_common::Hash256> = None;
        let mut scp_inflight_token: Option<std::sync::Arc<()>> = None;

        // Parity: Peer.cpp:1164-1171 — drop flood messages when not synced.
        // During catchup, tx are rejected by herder and flood-pull responses
        // reference messages the node can't use. Early shedding avoids
        // flood-gate, rate-limiter, clone, and channel work.
        if !state.is_synced.load(Ordering::Relaxed) && helpers::is_flood_shed_on_unsync(message) {
            state.metrics.flood_shed_unsynced.inc();
            trace!("Dropping {} from {} (not synced)", msg_type, peer_id);
            return Some(false);
        }

        // OVERLAY_SPEC §9.4: PEERS message validation.
        match validate_incoming_peers(ctx.peer.direction(), *ctx.received_peers, message) {
            PeersValidation::NotPeers => {}
            PeersValidation::AcceptFirst => {
                *ctx.received_peers = true;
            }
            PeersValidation::RejectWrongDirection => {
                warn!(
                    "Peer {} sent PEERS but we are the responder — dropping (OVERLAY_SPEC §9.4)",
                    peer_id
                );
                return None; // signal break
            }
            PeersValidation::RejectDuplicate => {
                warn!(
                    "Peer {} sent duplicate PEERS — dropping (OVERLAY_SPEC §9.4)",
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
                // maxtps diagnostic: the henyey-specific per-peer rate limiter
                // dropped a flood message. Dropped adverts/demands → coverage gaps
                // → txns never reach this peer → age-out (candidate root cause).
                tracing::info!(
                    target: "maxtps_diag",
                    class = ?traffic_class,
                    "maxtps_ratelimit_drop"
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
                state.flood_gate.record_inbound_relay(
                    hash,
                    peer_id.clone(),
                    lcl,
                    || {
                        ctx.peer.record_flood_stats(true, message_size);
                        state.metrics.flood_unique_recv.inc();
                        if matches!(message, StellarMessage::ScpMessage(_)) {
                            state.metrics.scp_flood_unique_recv.inc();
                        }
                    },
                    || {
                        ctx.peer.record_flood_stats(false, message_size);
                        state.metrics.flood_duplicate_recv.inc();
                        if matches!(message, StellarMessage::ScpMessage(_)) {
                            state.metrics.scp_flood_duplicate_recv.inc();
                        }
                    },
                );
                // FloodGate-tracked messages are NEVER dropped based on the
                // FloodGate record (issues #2317, #2327 — see the self-echo
                // regression): relay accounting above runs for every copy so
                // the gate knows which peers already have the message.
                //
                // In-flight dedup for SCP envelopes DOES happen here, in the
                // peer task, via the app-provided scheduled-cache filter
                // (maxtps iter 7; parity: stellar-core drops already-scheduled
                // messages in the peer thread via `checkScheduledAndCache`,
                // Peer.cpp:1113-1117 → OverlayManagerImpl.cpp:1190-1212).
                // The filter is the SAME `ScpScheduledCache` the app's
                // `pump_scp_intake` used to consult on the main event loop —
                // token-lifetime semantics are unchanged (a duplicate is only
                // dropped while its first copy is still queued/processing;
                // after the token drops, a re-delivery is forwarded again, so
                // the #2317 self-echo and post-processing retries survive).
                // Moving the check upstream keeps the ~22-peer duplicate storm
                // (~3000 channel messages per slot per node at the max-TPS
                // ceiling) out of the dedicated SCP channel, whose slot-start
                // burst previously took the event loop 80-110 ms (median) to
                // drain — the dominant component of the slow-nomination tail.
                if matches!(message, StellarMessage::ScpMessage(_)) {
                    scp_msg_hash = Some(hash);
                    if let Some(filter) = state.scp_inbound_filter.read().as_ref() {
                        match filter(&hash) {
                            Some(token) => scp_inflight_token = Some(token),
                            None => {
                                // In-flight duplicate: drop in the peer task.
                                // `flow_release` (if any) drops here, releasing
                                // the held capacity immediately — same as the
                                // other early-drop paths.
                                return Some(true);
                            }
                        }
                    }
                }
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

        // Forward to subscribers. For SCP, the drain-gated release token
        // (#3625) rides on the moved copy of the message into the dedicated SCP
        // channel; `route_to_subscribers` releases it immediately if the
        // bounded channel is full (drop-on-full), so capacity never leaks.
        let overlay_msg = OverlayMessage {
            from_peer: peer_id.clone(),
            message: message.clone(),
            received_at: Instant::now(),
            flow_release: flow_release.take(),
            scp_inflight_token,
            message_hash: scp_msg_hash,
        };
        let is_scp = state.route_to_subscribers(overlay_msg);
        Some(is_scp)
    }

    /// Run the peer message loop.
    ///
    /// The peer is owned by this task (no mutex). Outbound messages arrive
    /// via `outbound_rx`. The `tokio::select!` multiplexes between network
    /// recv, outbound channel, and periodic timers without blocking.
    /// Sleep for `ERROR_DROP_DRAIN_DELAY`, returning early if the overlay is
    /// shutting down (`running` flips to false).
    ///
    /// §12.3 / TCPPeer.cpp:835-862: the per-peer 5 s deferred-close timer. The
    /// poll loop keeps node-wide shutdown responsive (≤100 ms) instead of
    /// blocking each peer task for a full 5 s.
    async fn wait_error_drop_drain(running: &AtomicBool) {
        // Use `tokio::time::Instant` (not `std::time::Instant`) for both the
        // deadline and the elapsed check so the delay is driven by Tokio's
        // clock. Under paused/advanced time in tests this stays deterministic
        // and fast; mixing std `Instant` with `tokio::time::sleep` would tie
        // the loop to wall-clock and busy-loop for ~5 real seconds.
        let deadline = tokio::time::Instant::now() + ERROR_DROP_DRAIN_DELAY;
        while tokio::time::Instant::now() < deadline {
            if !running.load(Ordering::Relaxed) {
                return;
            }
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            tokio::time::sleep(remaining.min(ERROR_DROP_DRAIN_POLL)).await;
        }
    }

    /// #3419 diagnostic (observability-only): emit a `warn!` describing how an
    /// inbound peer connection dropped/reset, capturing the last message henyey
    /// sent on that connection before the drop. This does NOT alter any control
    /// flow, handshake, auth, codec, or dispatch behavior — it only reads
    /// already-tracked per-peer state and logs it.
    ///
    /// Gated to inbound connections only: the mainnet symptom in #3419 is
    /// `stellar_overlay_inbound_authenticated` sustained near 0 while inbound
    /// peers establish then get reset by the remote post-auth. Outbound drops
    /// are not the subject of this investigation, so we don't spam them here.
    ///
    /// `reason` is a short, low-cardinality classification of the drop site
    /// (e.g. "recv_error", "remote_closed", "send_error"); `detail` carries the
    /// errno / error string when available (empty otherwise).
    fn log_inbound_drop_diag(peer: &Peer, peer_id: &PeerId, reason: &str, detail: &str) {
        if peer.direction() != ConnectionDirection::Inbound {
            return;
        }
        let stats = peer.stats();
        warn!(
            target: "overlay_inbound_diag",
            peer_id = %peer_id,
            addr = %peer.remote_addr(),
            direction = "inbound",
            authenticated = peer.is_ready(),
            last_sent_msg_type = peer.last_sent_msg_type(),
            messages_sent = stats.messages_sent.load(Ordering::Relaxed),
            messages_received = stats.messages_received.load(Ordering::Relaxed),
            drop_reason = reason,
            drop_detail = detail,
            "inbound peer connection dropped; last message henyey sent before reset recorded"
        );
    }

    pub(super) async fn run_peer_loop(
        peer_id: PeerId,
        mut peer: Peer,
        mut outbound_rx: mpsc::Receiver<OutboundMessage>,
        outbound_tx: mpsc::Sender<OutboundMessage>,
        flow_control: Arc<FlowControl>,
        state: SharedPeerState,
    ) -> DropInitiator {
        let running = &state.running;

        // #3422: who initiated the drop. Default Local (henyey-side); set Remote
        // only at the two remote-origin exits below. The caller in
        // connection.rs uses this to increment the matching sibling counter
        // alongside the unconditional inbound_drop total.
        let mut drop_initiator = DropInitiator::Local;
        let is_validator = state.is_validator;

        // NOTE: The initial SEND_MORE_EXTENDED grant is sent in Peer::handshake()
        // after authentication, matching stellar-core's Peer::recvAuth() → sendSendMore().
        // Do NOT send a second grant here.

        // Idle/straggler timeout tracking (matches stellar-core Peer::recurrentTimerExpired).
        let mut last_read = Instant::now();
        let mut last_write = Instant::now();
        // #3643 straggler parity: enqueue time of the newest message in the last
        // batch actually written to this peer. Mirrors stellar-core's
        // `mEnqueueTimeOfLastWrite`, initialized to `now()` at the peer ctor
        // (`Peer.cpp:144`) and advanced inside the write loop (`TCPPeer.cpp:329`,
        // last-wins over the written FIFO prefix). The straggler timeout
        // (`check_peer_timeouts`) keys on THIS, not on `last_write`.
        let mut enqueue_time_of_last_write = Instant::now();

        // Track message counts for periodic diagnostics
        let mut total_messages: u64 = 0;
        let mut scp_messages: u64 = 0;
        // #3570 observability: SCP envelopes henyey has written to this peer
        // (accumulated from flow-controlled batch sends). Read only by the
        // inbound idle-drop diagnostic; affects no control flow.
        let mut scp_written: u64 = 0;
        let mut last_stats_log = Instant::now();

        // Single periodic timer for ping, SendMore, and timeout checks.
        // Fires every second (covers 1s SendMore interval and 5s ping interval).
        let mut periodic_interval = tokio::time::interval(Duration::from_secs(1));
        let mut ticks_since_ping: u32 = 0;

        // #3642 Phase 2 — read-side socket throttle resume wake. When inbound
        // total reading capacity is exhausted the `peer.recv()` select arm below
        // is disabled (its `if flow_control.can_read()` precondition is false),
        // back-pressuring the sender at the TCP window exactly as stellar-core's
        // "don't reschedule the read" does (`TCPPeer::maybeThrottleRead`). The
        // throttle is lifted by the SCP consumer draining a full reading-capacity
        // batch: each SCP `FlowControlRelease` carries a clone of this `Notify`
        // and, on the batch-completing release, fires it (after `stop_throttling`)
        // — the async equivalent of core's `scheduleRead`. The 1s
        // `periodic_interval` tick re-evaluates the loop as a backstop so a lost
        // wake can never wedge a peer for more than ~1s.
        let read_resume = Arc::new(tokio::sync::Notify::new());

        // OVERLAY_SPEC §9.4: Track whether we've received a PEERS message
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
                                Self::log_inbound_drop_diag(&peer, &peer_id, "send_error", &e.to_string());
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
                                Ok(outcome) => {
                                    if outcome.sent {
                                        last_write = Instant::now();
                                    }
                                    // #3643 straggler parity: advance the
                                    // enqueue stamp to the newest message in the
                                    // batch actually written (TCPPeer.cpp:329).
                                    if let Some(t) = outcome.newest_emplaced {
                                        enqueue_time_of_last_write = t;
                                    }
                                    // #3570 observability: accumulate SCP writes.
                                    scp_written += outcome.scp_written;
                                }
                                Err(e) => {
                                    debug!("Failed to send batch to {}: {}", peer_id, e);
                                    Self::log_inbound_drop_diag(&peer, &peer_id, "flood_send_error", &e.to_string());
                                    break;
                                }
                            }
                        }
                        Some(OutboundMessage::Shutdown) => {
                            info!("Peer {} loop exiting: shutdown requested", peer_id);
                            break;
                        }
                        Some(OutboundMessage::ShutdownAfterError) => {
                            // §12.3 / TCPPeer.cpp:835-862: the preceding
                            // `Send(err)` has already been flushed to the socket
                            // (same channel, FIFO). Defer the close by 5 s so the
                            // ERROR_MSG drains rather than being RST'd. The sleep
                            // is per-peer-task and interruptible by node shutdown
                            // (`state.running` going false), so it never delays
                            // overlay teardown by 5 s per peer.
                            info!(
                                "Peer {} loop exiting: error drop, draining for {:?}",
                                peer_id, ERROR_DROP_DRAIN_DELAY
                            );
                            Self::wait_error_drop_drain(running).await;
                            break;
                        }
                        None => {
                            // Channel closed (PeerHandle dropped)
                            info!("Peer {} loop exiting: outbound channel closed", peer_id);
                            break;
                        }
                    }
                }

                // Receive from network (no mutex — peer is owned).
                //
                // #3642 Phase 2: gate this arm on `flow_control.can_read()`
                // (= inbound total reading capacity > 0). tokio does not poll a
                // select branch whose `if`-precondition is false, so a throttled
                // peer simply stops pulling bytes — NO busy-spin — until the SCP
                // consumer drains a full batch and the resume `Notify` re-enables
                // the loop. The outbound-send and periodic arms stay enabled, so
                // the task remains live (can still send, ping, time out). Mirrors
                // stellar-core `TCPPeer::maybeThrottleRead` declining to schedule
                // the next read while `!canRead()` (`TCPPeer.cpp:620/770`).
                result = peer.recv(), if flow_control.can_read() => {
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

                            // Per-message-type recv timing.
                            // Parity: stellar-core mRecv*Timer (OverlayMetrics.h:45-76).
                            let recv_start = Instant::now();
                            let msg_kind = OverlayMessageKind::from_stellar_message(&message);

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
                                    enqueue_time_of_last_write: &mut enqueue_time_of_last_write,
                                    scp_written: &mut scp_written,
                                    outbound_tx: &outbound_tx,
                                },
                                &flow_control,
                                &state,
                                is_validator,
                                &read_resume,
                            ).await;

                            metrics::histogram!(
                                "stellar_overlay_recv_message_seconds",
                                "message_type" => msg_kind.label()
                            )
                            .record(recv_start.elapsed().as_secs_f64());
                            if matches!(action, RecvAction::Break) {
                                break;
                            }

                            // #3642 Phase 2: arm the read throttle if this message
                            // exhausted inbound total reading capacity (records
                            // `last_throttle` only when `!can_read()`). Mirrors
                            // stellar-core's post-`recvMessage` `maybeThrottleRead`
                            // (`TCPPeer.cpp:620/770`). On the next loop iteration
                            // the gated `peer.recv()` arm is disabled until the
                            // consumer-drain resume fires.
                            flow_control.maybe_throttle_read();
                        }
                        Ok(None) => {
                            info!("Peer {} loop exiting: connection closed by remote (total_msgs={}, scp_msgs={})", peer_id, total_messages, scp_messages);
                            Self::log_inbound_drop_diag(&peer, &peer_id, "remote_closed", "");
                            drop_initiator = DropInitiator::from_reason("remote_closed");
                            break;
                        }
                        Err(e) => {
                            state.metrics.errors_read.inc();
                            info!("Peer {} loop exiting: recv error: {} (total_msgs={}, scp_msgs={})", peer_id, e, total_messages, scp_messages);
                            Self::log_inbound_drop_diag(&peer, &peer_id, "recv_error", &e.to_string());
                            drop_initiator = DropInitiator::from_reason("recv_error");
                            break;
                        }
                    }
                }

                // Periodic tasks: ping, timeout checks
                _ = periodic_interval.tick() => {
                    if let Some(idle_reason) = Self::check_peer_timeouts(&peer_id, &PeerTimingInfo {
                        last_read,
                        last_write,
                        enqueue_time_of_last_write,
                        total_messages,
                        scp_messages,
                    }, &flow_control, &state.metrics) {
                        // #3570 observability: emit the per-inbound-peer
                        // flow-control snapshot so the operator can pin WHY this
                        // peer idled out. Gathered BEFORE the break to avoid
                        // borrow conflicts; reads only already-tracked state and
                        // does not alter the drop (the `break` is unconditional
                        // on a timeout, exactly as before).
                        let diag = Self::build_inbound_drop_diag(
                            &flow_control,
                            last_read,
                            last_write,
                            scp_written,
                            idle_reason,
                        );
                        Self::log_inbound_idle_drop_diag(&peer, &peer_id, &diag);
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

                // #3642 Phase 2: read-resume wake. Fired by an SCP
                // `FlowControlRelease` (carrying a clone of `read_resume`) on the
                // drain that completes a full reading-capacity batch, after
                // `stop_throttling()` has cleared the throttle. The arm body does
                // nothing: re-entering the `select!` re-evaluates the
                // `if flow_control.can_read()` precondition on the `peer.recv()`
                // arm, which is now true, so reading resumes. Async equivalent of
                // stellar-core `Peer::scheduleRead` (`Peer.cpp:313-333`).
                _ = read_resume.notified() => {}
            }
        }

        // Close peer (owned, no mutex needed)
        peer.close().await;
        debug!("Peer {} loop exited and disconnected", peer_id);

        drop_initiator
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
        // #3642 Phase 2: the peer's read-resume wake, threaded into each SCP
        // `FlowControlRelease` so the consumer-drain release can lift the read
        // throttle. Overlay-internal — the app consumer never sees it.
        read_resume: &Arc<tokio::sync::Notify>,
    ) -> RecvAction {
        let msg_type = helpers::message_type_name(&message);
        trace!("Processing message_type={} from {}", msg_type, peer_id);

        // Log ERROR messages (Load rejections are expected, log at debug).
        // OVERLAY §7.1.3-1: sanitize the raw message bytes before logging (do
        // NOT use `to_string()`, which escapes rather than collapses bytes).
        if let StellarMessage::ErrorMsg(ref err) = message {
            if err.code == ErrorCode::Load {
                debug!(
                    "Peer sent_error peer={} code={:?} msg={}",
                    peer_id,
                    err.code,
                    sanitize_error_msg(&err.msg[..])
                );
            } else {
                // #3773: dump the per-peer outbound diagnostic ring alongside
                // the warning, so a peer-reported `ERR_DATA "received corrupt
                // XDR"` can be tied to the frame(s) henyey sent just before.
                warn!(
                    recent_sends = %ctx.peer.recent_sends_summary(),
                    "Peer sent_error peer={} code={:?} msg={}",
                    peer_id,
                    err.code,
                    sanitize_error_msg(&err.msg[..])
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
                    Ok(outcome) => {
                        if outcome.sent {
                            *ctx.last_write = Instant::now();
                        }
                        // #3643 straggler parity: advance the enqueue stamp to
                        // the newest message in the batch actually written
                        // (TCPPeer.cpp:329, last-wins over the written prefix).
                        if let Some(t) = outcome.newest_emplaced {
                            *ctx.enqueue_time_of_last_write = t;
                        }
                        // #3570 observability: accumulate SCP envelopes written.
                        *ctx.scp_written += outcome.scp_written;
                    }
                    Err(()) => return RecvAction::Break,
                }
            }
            _ => {}
        }

        // #3625 — drain-gated SEND_MORE. For SCP envelopes, transfer the
        // capacity-release obligation from the inline `finish()` to a
        // `FlowControlRelease` token that rides on the routed `OverlayMessage`
        // and fires `end_message_processing` only when the app event-loop
        // consumer actually drains the envelope. This keeps a stalled consumer
        // from granting SEND_MORE at channel-enqueue time (the bug: a wedged
        // event loop kept crediting senders until #3626's drop-on-full fired).
        //
        // Non-SCP messages are processed synchronously on this peer task (as in
        // stellar-core), so their capacity is released inline via `finish()`
        // immediately after routing — unchanged behavior.
        let is_scp_message = matches!(message, StellarMessage::ScpMessage(_));
        // For SCP, disarm the guard into a release token (deferred release).
        // For non-SCP, keep the guard to `finish()` inline below.
        let mut capacity_guard = Some(capacity_guard);
        let scp_release = if is_scp_message {
            let (fc, msg) = capacity_guard.take().unwrap().disarm();
            // #3642 Phase 2: thread the read-resume `Notify` into the token so
            // the consumer-drain release can lift the read throttle and wake the
            // peer loop's gated `peer.recv()` arm.
            Some(
                super::FlowControlRelease::new(
                    fc,
                    msg,
                    ctx.outbound_tx.clone(),
                    Arc::clone(&state.metrics),
                )
                .with_resume_notify(Arc::clone(read_resume)),
            )
        } else {
            None
        };

        // Route message through filtering and dispatch.
        // `None` signals the peer should be dropped. The SCP release token is
        // moved in; routing attaches it to the `OverlayMessage` (deferred
        // release) or, on an early drop, lets it drop here (immediate release).
        match Self::route_received_message(&message, peer_id, ctx, state, is_validator, scp_release)
        {
            None => return RecvAction::Break,
            Some(is_scp) => {
                if is_scp {
                    *ctx.scp_messages += 1;
                }
            }
        }

        // Non-SCP flow control: release capacity inline and send SEND_MORE now
        // (synchronous processing on the peer task). For SCP, capacity_guard
        // was disarmed above and released via the token at consumer drain.
        if !is_scp_message {
            let send_more_cap = capacity_guard
                .take()
                .expect("non-SCP guard present")
                .finish();
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
        }

        RecvAction::Continue
    }

    /// Send queued outbound messages that have flow control capacity.
    ///
    /// Retrieves the next batch from FlowControl's priority queues,
    /// sends each message, then cleans up sent entries. Returns a
    /// [`BatchSendOutcome`] reporting whether any message was sent and how many
    /// SCP envelopes were written (the latter for the #3570 inbound-drop diag).
    pub(super) async fn send_flow_controlled_batch(
        peer: &mut Peer,
        flow_control: &FlowControl,
        metrics: &crate::metrics::OverlayMetrics,
    ) -> crate::Result<BatchSendOutcome> {
        use crate::flow_control::MessagePriority;

        let batch = flow_control.get_next_batch_to_send();
        if batch.is_empty() {
            return Ok(BatchSendOutcome {
                sent: false,
                scp_written: 0,
                newest_emplaced: None,
            });
        }

        // #3643 straggler parity: track the MAX `time_emplaced` over messages
        // ACTUALLY written. maxtps (T2): the batch is now written as a SINGLE
        // coalesced `send_batch` (one syscall/segment for the whole batch)
        // instead of one write per message, so the send is all-or-nothing:
        // either every message goes out (advance to the batch max) or none does
        // (error → peer dropped by caller, stamp moot).
        let messages: Vec<StellarMessage> = batch.iter().map(|q| q.message.clone()).collect();
        let newest_emplaced: Option<Instant> = batch
            .iter()
            .fold(None, |acc, q| fold_newest_emplaced(acc, q.time_emplaced));

        if let Err(e) = peer.send_batch(&messages).await {
            // Nothing was sent (single write_all); drop with no partial stamp.
            metrics.errors_write.inc();
            return Err(e);
        }

        // Group the sent messages by priority for process_sent_messages.
        let mut sent_by_priority: Vec<Vec<StellarMessage>> =
            vec![Vec::new(); MessagePriority::COUNT];
        for msg in messages {
            metrics.messages_written.inc();
            if let Some(p) = MessagePriority::from_message(&msg) {
                sent_by_priority[p as usize].push(msg);
            }
        }

        // #3570 observability: SCP envelopes actually written this batch. The
        // ping path sends GetScpQuorumset (not an SCP envelope, and not routed
        // through this batch), so it is correctly excluded.
        let scp_written = sent_by_priority[MessagePriority::Scp as usize].len() as u64;

        flow_control.process_sent_messages(&sent_by_priority);
        Ok(BatchSendOutcome {
            sent: true,
            scp_written,
            newest_emplaced,
        })
    }
}

/// #3643 straggler parity: fold one written message's `time_emplaced` into the
/// running newest-emplaced accumulator (a max). Extracted so the advance-point
/// (advance-to-NEWEST, never oldest) is a single, unit-testable selection —
/// mirroring stellar-core's last-wins reassignment of `mEnqueueTimeOfLastWrite`
/// over the written FIFO prefix (`TCPPeer.cpp:329`). Folding each message's
/// emplace time with `max` is equivalent to taking the newest over the prefix.
fn fold_newest_emplaced(acc: Option<Instant>, emplaced: Instant) -> Option<Instant> {
    Some(match acc {
        Some(prev) if prev >= emplaced => prev,
        _ => emplaced,
    })
}

/// #3570 observability: outcome of [`OverlayManager::send_flow_controlled_batch`].
pub(super) struct BatchSendOutcome {
    /// Whether any message was written this batch (preserves the prior `bool`).
    pub sent: bool,
    /// Number of SCP envelopes written this batch.
    pub scp_written: u64,
    /// #3643 straggler parity: the MAX `time_emplaced` over the messages
    /// actually written this batch (the *newest* message in the written set),
    /// or `None` if nothing was written. The caller advances
    /// `enqueue_time_of_last_write` to this value — mirroring stellar-core's
    /// `mEnqueueTimeOfLastWrite = tsm.mEnqueuedTime` (`TCPPeer.cpp:329`,
    /// last-wins over the written FIFO prefix). henyey's batch is priority-
    /// interleaved (not a contiguous FIFO slice), so the MAX emplace time is
    /// the faithful analogue of core's last-in-prefix.
    pub newest_emplaced: Option<Instant>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::flow_control::FlowControlConfig;
    use stellar_xdr::ErrorCode;

    #[test]
    fn test_drop_initiator_classification() {
        // #3422: classify each inbound-drop `reason` (the low-cardinality
        // taxonomy fed to log_inbound_drop_diag) by who broke the loop.
        // Remote = peer closed/reset the socket; Local = henyey broke the loop.
        assert_eq!(
            DropInitiator::from_reason("remote_closed"),
            DropInitiator::Remote
        );
        assert_eq!(
            DropInitiator::from_reason("recv_error"),
            DropInitiator::Remote
        );
        assert_eq!(
            DropInitiator::from_reason("send_error"),
            DropInitiator::Local
        );
        assert_eq!(
            DropInitiator::from_reason("flood_send_error"),
            DropInitiator::Local
        );
        assert_eq!(DropInitiator::from_reason("timeout"), DropInitiator::Local);
        assert_eq!(
            DropInitiator::from_reason("protocol_break"),
            DropInitiator::Local
        );
        assert_eq!(DropInitiator::from_reason("shutdown"), DropInitiator::Local);
        // Default for any unknown / unattributed exit is Local (henyey-side).
        assert_eq!(
            DropInitiator::from_reason("anything_else"),
            DropInitiator::Local
        );
        assert_eq!(DropInitiator::default(), DropInitiator::Local);
    }

    /// #3775: a peer-sent `ErrorMsg` terminates the session at the peer's
    /// explicit request, so its drop reason (`peer_error`) must classify as
    /// `Remote` — not fall through to the `Local` default the way it did before
    /// this fix. Regression guard for `from_reason`'s taxonomy.
    #[test]
    fn test_from_reason_classifies_peer_error_as_remote() {
        assert_eq!(
            DropInitiator::from_reason("peer_error"),
            DropInitiator::Remote,
            "a peer-sent ErrorMsg (reason=\"peer_error\") is remote-initiated"
        );
    }

    #[test]
    fn test_check_peer_timeouts_returns_reason() {
        // #3570: check_peer_timeouts now returns Option<IdleReason> instead of
        // bool, so the idle-drop call site can carry WHICH timeout fired into
        // the diagnostic. The drop conditions themselves are unchanged.
        use crate::flow_control::FlowControl;
        let metrics = crate::metrics::OverlayMetrics::new();
        let peer_id = PeerId::from_bytes([7u8; 32]);
        let stale = Instant::now() - Duration::from_secs(40);
        let fresh = Instant::now();

        // Both clocks stale (>30s) → Idle.
        let fc_with_capacity = FlowControl::default();
        // Grant capacity so the send-mode-idle path does NOT fire, isolating Idle.
        fc_with_capacity.maybe_release_capacity(&StellarMessage::SendMoreExtended(
            stellar_xdr::SendMoreExtended {
                num_messages: 10,
                num_bytes: 10_000,
            },
        ));
        assert_eq!(
            OverlayManager::check_peer_timeouts(
                &peer_id,
                &PeerTimingInfo {
                    last_read: stale,
                    last_write: stale,
                    enqueue_time_of_last_write: fresh,
                    total_messages: 2,
                    scp_messages: 0,
                },
                &fc_with_capacity,
                &metrics,
            ),
            Some(IdleReason::Idle),
            "both clocks stale with capacity → Idle"
        );

        // Fresh clocks, but no outbound capacity for >=60s → SendModeIdle.
        // A default FlowControl has no_outbound_capacity set at construction;
        // backdate it past the 60s send-mode-idle threshold.
        let fc_no_capacity = FlowControl::default();
        fc_no_capacity.force_no_outbound_capacity_age(70);
        assert_eq!(
            OverlayManager::check_peer_timeouts(
                &peer_id,
                &PeerTimingInfo {
                    last_read: fresh,
                    last_write: fresh,
                    enqueue_time_of_last_write: fresh,
                    total_messages: 2,
                    scp_messages: 0,
                },
                &fc_no_capacity,
                &metrics,
            ),
            Some(IdleReason::SendModeIdle),
            "fresh clocks but no capacity for >=60s → SendModeIdle"
        );

        // Everything fresh and capacity granted → None.
        assert_eq!(
            OverlayManager::check_peer_timeouts(
                &peer_id,
                &PeerTimingInfo {
                    last_read: fresh,
                    last_write: fresh,
                    enqueue_time_of_last_write: fresh,
                    total_messages: 10,
                    scp_messages: 5,
                },
                &fc_with_capacity,
                &metrics,
            ),
            None,
            "fresh and capacitated → no timeout"
        );
    }

    #[test]
    fn test_straggler_timeout_fires_on_stale_enqueue_time_with_recent_last_write() {
        // #3643 centerpiece — straggler parity with stellar-core.
        //
        // Core keys the straggler on `(now - mEnqueueTimeOfLastWrite) >=
        // PEER_STRAGGLER_TIMEOUT(120s)` (`Peer.cpp:462`), where
        // `mEnqueueTimeOfLastWrite` is the enqueue time of the newest message in
        // the last batch actually written (`TCPPeer.cpp:329`). henyey previously
        // keyed it on `last_write` (the *completed-write* time), which resets to
        // `now()` on every write — so a peer that writes steadily but never
        // catches up (its newest batched message is already >120s old) was never
        // flagged.
        //
        // FAIL-BEFORE: with the straggler keyed on `last_write` (main), a recent
        // `last_write` masks a stale `enqueue_time_of_last_write` → returns None.
        // PASS-AFTER: keyed on `enqueue_time_of_last_write` → Straggler.
        use crate::flow_control::FlowControl;
        let metrics = crate::metrics::OverlayMetrics::new();
        let peer_id = PeerId::from_bytes([9u8; 32]);

        // Capacity granted so the SendModeIdle path does not fire — isolate the
        // straggler branch.
        let fc = FlowControl::default();
        fc.maybe_release_capacity(&StellarMessage::SendMoreExtended(
            stellar_xdr::SendMoreExtended {
                num_messages: 10,
                num_bytes: 10_000,
            },
        ));

        // last_read/last_write are RECENT (peer is actively writing and reading,
        // so neither idle nor a completed-write timeout would fire), but the
        // newest message we managed to batch-write was enqueued >120s ago.
        let recent = Instant::now();
        let stale_enqueue = Instant::now() - Duration::from_secs(125);

        assert_eq!(
            OverlayManager::check_peer_timeouts(
                &peer_id,
                &PeerTimingInfo {
                    last_read: recent,
                    last_write: recent,
                    enqueue_time_of_last_write: stale_enqueue,
                    total_messages: 100,
                    scp_messages: 50,
                },
                &fc,
                &metrics,
            ),
            Some(IdleReason::Straggler),
            "stale enqueue-time with recent last_write must fire the straggler"
        );
    }

    #[test]
    fn test_straggler_does_not_fire_when_writes_keep_up() {
        // #3643: a peer whose newest batched message is recent (writes keep up)
        // is NOT straggler-dropped, even under high message volume.
        use crate::flow_control::FlowControl;
        let metrics = crate::metrics::OverlayMetrics::new();
        let peer_id = PeerId::from_bytes([10u8; 32]);
        let fc = FlowControl::default();
        fc.maybe_release_capacity(&StellarMessage::SendMoreExtended(
            stellar_xdr::SendMoreExtended {
                num_messages: 10,
                num_bytes: 10_000,
            },
        ));
        let recent = Instant::now();
        assert_eq!(
            OverlayManager::check_peer_timeouts(
                &peer_id,
                &PeerTimingInfo {
                    last_read: recent,
                    last_write: recent,
                    enqueue_time_of_last_write: recent,
                    total_messages: 100_000,
                    scp_messages: 50_000,
                },
                &fc,
                &metrics,
            ),
            None,
            "recent enqueue-time → no straggler even at high volume"
        );
    }

    #[test]
    fn test_no_outbound_capacity_not_attributed_to_straggler() {
        // #3643 no-conflation invariant: a peer whose outbound queue is backed
        // up *purely because the remote granted no SEND_MORE* must be attributed
        // to SendModeIdle (60s, `no_outbound_capacity_timeout`), NOT to the
        // straggler (120s). Core keeps these as separate drops on separate
        // signals; the straggler must not be derived from the merely-queued set.
        //
        // Here: last_read/last_write recent, enqueue-time recent (we never got
        // to write anything stale), but no_outbound_capacity aged past 60s.
        use crate::flow_control::FlowControl;
        let metrics = crate::metrics::OverlayMetrics::new();
        let peer_id = PeerId::from_bytes([11u8; 32]);
        let fc = FlowControl::default();
        // No SEND_MORE ever granted → no_outbound_capacity is set at ctor;
        // backdate it past the 60s send-mode-idle threshold but BELOW 120s.
        fc.force_no_outbound_capacity_age(70);
        let recent = Instant::now();
        assert_eq!(
            OverlayManager::check_peer_timeouts(
                &peer_id,
                &PeerTimingInfo {
                    last_read: recent,
                    last_write: recent,
                    enqueue_time_of_last_write: recent,
                    total_messages: 5,
                    scp_messages: 0,
                },
                &fc,
                &metrics,
            ),
            Some(IdleReason::SendModeIdle),
            "no-outbound-capacity backlog must be SendModeIdle, not Straggler"
        );
    }

    #[test]
    fn test_idle_peer_with_no_writes_not_flagged_straggler() {
        // #3643: a peer that never had outbound traffic keeps its initialized
        // enqueue-time (= ctor `now()`). With fresh clocks it is not flagged;
        // the idle check governs once both read/write clocks go stale (as core's
        // ctor-init to `now()` ensures a no-traffic peer is never straggler-
        // false-flagged on the enqueue signal).
        use crate::flow_control::FlowControl;
        let metrics = crate::metrics::OverlayMetrics::new();
        let peer_id = PeerId::from_bytes([12u8; 32]);
        let fc = FlowControl::default();
        fc.maybe_release_capacity(&StellarMessage::SendMoreExtended(
            stellar_xdr::SendMoreExtended {
                num_messages: 10,
                num_bytes: 10_000,
            },
        ));
        let now = Instant::now();
        // Fresh enqueue-time (ctor init), fresh clocks → no drop at all.
        assert_eq!(
            OverlayManager::check_peer_timeouts(
                &peer_id,
                &PeerTimingInfo {
                    last_read: now,
                    last_write: now,
                    enqueue_time_of_last_write: now,
                    total_messages: 0,
                    scp_messages: 0,
                },
                &fc,
                &metrics,
            ),
            None,
            "a peer with no outbound traffic and fresh clocks is not flagged"
        );
        // When both read/write clocks go stale, the *idle* check fires first —
        // not the straggler — even though the enqueue-time is also old here.
        let stale = now - Duration::from_secs(40);
        assert_eq!(
            OverlayManager::check_peer_timeouts(
                &peer_id,
                &PeerTimingInfo {
                    last_read: stale,
                    last_write: stale,
                    enqueue_time_of_last_write: now,
                    total_messages: 0,
                    scp_messages: 0,
                },
                &fc,
                &metrics,
            ),
            Some(IdleReason::Idle),
            "stale read/write clocks → Idle governs (fresh enqueue-time)"
        );
    }

    #[tokio::test]
    async fn test_enqueue_time_advances_to_newest_sent_not_oldest() {
        // #3643 advance-point guard — mirrors core `TCPPeer.cpp:329` last-wins.
        //
        // When a non-empty batch is actually written, the stamp advances to the
        // MAX `time_emplaced` over the sent batch (the newest message), NOT the
        // oldest. Keying on the oldest would false-positive-drop a peer with a
        // deep-but-draining queue. henyey's batch is priority-interleaved (not a
        // contiguous FIFO slice), so max-emplaced-time is the faithful analogue
        // of core's last-in-written-prefix.
        //
        // We assert on `BatchSendOutcome.newest_emplaced` directly (the value
        // the loop uses to advance `enqueue_time_of_last_write`), driving a real
        // FlowControl with two messages emplaced at distinct, known times.
        use crate::flow_control::{FlowControl, FlowControlConfig};

        let fc = FlowControl::new(FlowControlConfig::default());
        // Grant ample outbound capacity so both messages are in the batch.
        fc.maybe_release_capacity(&StellarMessage::SendMoreExtended(
            stellar_xdr::SendMoreExtended {
                num_messages: 1000,
                num_bytes: 10_000_000,
            },
        ));

        // Emplace an OLD message, then (a beat later) a NEW message. Both go to
        // the same priority queue (TX_DEMAND/SCP etc. — use SCP so they share a
        // queue and FIFO order is old→new).
        let mk_scp = |seed: u8| {
            StellarMessage::ScpMessage(stellar_xdr::ScpEnvelope {
                statement: stellar_xdr::ScpStatement {
                    node_id: stellar_xdr::NodeId(stellar_xdr::PublicKey::PublicKeyTypeEd25519(
                        stellar_xdr::Uint256([seed; 32]),
                    )),
                    slot_index: seed as u64,
                    pledges: stellar_xdr::ScpStatementPledges::Externalize(
                        stellar_xdr::ScpStatementExternalize {
                            commit: stellar_xdr::ScpBallot {
                                counter: 1,
                                value: vec![seed].try_into().unwrap(),
                            },
                            n_h: 1,
                            commit_quorum_set_hash: stellar_xdr::Hash([0u8; 32]),
                        },
                    ),
                },
                signature: stellar_xdr::Signature(Vec::new().try_into().unwrap()),
            })
        };

        fc.add_msg_and_maybe_trim_queue(mk_scp(1));
        // Snapshot the batch the loop would send and confirm both messages are
        // present with distinct emplace times, with the newest > oldest.
        fc.add_msg_and_maybe_trim_queue(mk_scp(2));

        let batch = fc.get_next_batch_to_send();
        assert_eq!(batch.len(), 2, "both messages should be in the batch");
        let oldest = batch.iter().map(|q| q.time_emplaced).min().unwrap();
        let newest = batch.iter().map(|q| q.time_emplaced).max().unwrap();
        assert!(
            newest > oldest,
            "fixture sanity: the two messages have distinct emplace times"
        );

        // Drive the SAME selection the production send path uses
        // (`fold_newest_emplaced`, the running max in `send_flow_controlled_batch`)
        // over the actually-batched messages, in queue order (old→new).
        let mut acc: Option<Instant> = None;
        for q in &batch {
            acc = fold_newest_emplaced(acc, q.time_emplaced);
        }
        assert_eq!(
            acc,
            Some(newest),
            "advance-point must be the MAX time_emplaced (newest message in the \
             written batch), mirroring core's last-wins mEnqueueTimeOfLastWrite"
        );
        assert_ne!(
            acc,
            Some(oldest),
            "advance-point must NOT be the oldest time_emplaced — keying on the \
             oldest would false-positive-drop a peer with a deep-but-draining queue"
        );

        // Also confirm order-independence: folding new→old yields the same max
        // (a min-based bug would be order-sensitive and is excluded here).
        let mut acc_rev: Option<Instant> = None;
        for q in batch.iter().rev() {
            acc_rev = fold_newest_emplaced(acc_rev, q.time_emplaced);
        }
        assert_eq!(acc_rev, Some(newest), "fold is a max, order-independent");
    }

    #[test]
    fn test_tx_release_is_inline_not_deferred() {
        // PARITY LOCK (#3643, #3625 Phase 3): stellar-core processes
        // `recvTransaction`/`recvFloodAdvert`/`recvFloodDemand` SYNCHRONOUSLY on
        // the main thread (`Peer.cpp:1144-1533`); `~CapacityTrackedMessage`
        // fires the release inline. In henyey the SCP path defers release to a
        // `FlowControlRelease` token (drain-gated, #3625) while NON-SCP messages
        // keep the `CapacityGuard` and release inline via `finish()` on the peer
        // task (`peer_loop.rs` ~1500-1560) — the faithful async analogue.
        // Deferring tx/flood onto the lossy broadcast consumer would be a parity
        // REGRESSION (couples a parity credit to a lossy channel and diverges
        // from core's timing). This test pins the inline-release decision.
        use crate::flow_control::{CapacityGuard, FlowControl};
        use stellar_xdr::{TransactionEnvelope, TransactionV0, TransactionV0Envelope};

        // 1) The production routing discriminant: only SCP disarms into a
        //    deferred release token; a TRANSACTION is NOT SCP, so it takes the
        //    inline-`finish()` path.
        let tx = StellarMessage::Transaction(TransactionEnvelope::TxV0(TransactionV0Envelope {
            tx: TransactionV0 {
                source_account_ed25519: stellar_xdr::Uint256([0u8; 32]),
                fee: 100,
                seq_num: stellar_xdr::SequenceNumber(1),
                time_bounds: None,
                memo: stellar_xdr::Memo::None,
                operations: vec![].try_into().unwrap(),
                ext: stellar_xdr::TransactionV0Ext::V0,
            },
            signatures: vec![].try_into().unwrap(),
        }));
        let is_scp_message = matches!(tx, StellarMessage::ScpMessage(_));
        assert!(
            !is_scp_message,
            "a TRANSACTION must NOT be treated as SCP — it releases inline, not \
             via a deferred FlowControlRelease token"
        );

        // 2) End-to-end: acquiring then `finish()`-ing a guard for a TX releases
        //    its flow-control capacity INLINE (the flood-processed counter
        //    advances on this task), with no token minted/deferred.
        let fc = Arc::new(FlowControl::default());
        assert_eq!(fc.test_flood_data_processed(), 0);
        let guard = CapacityGuard::new(Arc::clone(&fc), tx).expect("capacity available for TX");
        // No deferral: `finish()` (not `disarm()`) is the non-SCP path.
        let _send_more = guard.finish();
        assert_eq!(
            fc.test_flood_data_processed(),
            1,
            "TX capacity must be released inline via finish() (flood-processed \
             counter advanced on this task), matching core's synchronous \
             recvTransaction — not deferred to a drain-gated token"
        );
    }

    #[test]
    fn test_inbound_idle_drop_emits_diag_with_flow_signals() {
        // #3570: at the idle-timeout drop, gather an InboundDropDiag from the
        // already-tracked per-peer state so the operator can pin WHY each
        // inbound peer idles out. The leading-hypothesis signature is:
        // idle_reason ∈ {Idle, SendModeIdle}, send_more_received=0 (peer never
        // granted us capacity), scp_written=0 (we wrote no SCP envelopes),
        // scp_queue_depth>0 (we HAD SCP to send but were blocked on capacity).
        //
        // We assert on the RETURNED STRUCT, not on scraped log output.
        use crate::flow_control::FlowControl;
        use stellar_xdr::{ScpEnvelope, ScpStatement, ScpStatementPledges};

        // Inbound peer that authenticated but never sent SEND_MORE_EXTENDED back:
        // no_outbound_capacity is set, send_more_received_count stays 0.
        let fc = FlowControl::default();
        assert_eq!(fc.get_stats().send_more_received_count, 0);

        // Enqueue an SCP flood message so the SCP queue is non-empty (we have
        // SCP to broadcast to this peer, but no capacity to send it).
        let scp_env = ScpEnvelope {
            statement: ScpStatement {
                node_id: stellar_xdr::NodeId(stellar_xdr::PublicKey::PublicKeyTypeEd25519(
                    stellar_xdr::Uint256([0u8; 32]),
                )),
                slot_index: 1,
                pledges: ScpStatementPledges::Externalize(stellar_xdr::ScpStatementExternalize {
                    commit: stellar_xdr::ScpBallot {
                        counter: 1,
                        value: vec![1u8].try_into().unwrap(),
                    },
                    n_h: 1,
                    commit_quorum_set_hash: stellar_xdr::Hash([0u8; 32]),
                }),
            },
            signature: stellar_xdr::Signature(Vec::new().try_into().unwrap()),
        };
        fc.add_msg_and_maybe_trim_queue(StellarMessage::ScpMessage(scp_env));
        assert!(
            fc.get_stats().scp_queue_size > 0,
            "SCP flood message should be queued"
        );

        // Idle clock advanced past PEER_TIMEOUT for both read and write.
        let stale = Instant::now() - Duration::from_secs(40);

        // No SCP envelopes were written on this connection (capacity-blocked).
        let scp_written: u64 = 0;

        let diag = OverlayManager::build_inbound_drop_diag(
            &fc,
            stale, // last_read
            stale, // last_write
            scp_written,
            IdleReason::Idle,
        );

        assert_eq!(diag.send_more_received, 0, "peer never granted capacity");
        assert_eq!(diag.scp_written, 0, "no SCP envelopes written");
        assert!(diag.scp_queue_depth > 0, "had SCP queued but blocked");
        assert_eq!(diag.peer_message_capacity, 0, "no capacity granted");
        assert!(
            diag.last_write_age_secs >= 40,
            "last_write age reflects the stale clock"
        );
        assert!(
            diag.last_read_age_secs >= 40,
            "last_read age reflects the stale clock"
        );
        assert!(
            matches!(
                diag.idle_reason,
                IdleReason::Idle | IdleReason::SendModeIdle
            ),
            "idle_reason in the leading-hypothesis set"
        );
    }

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

    // OVERLAY §7.1.3-1: ERROR_MSG sanitization on receipt. Mirrors
    // stellar-core `Peer::recvError` (Peer.cpp:1698-1733): every byte that is
    // not ASCII-alphanumeric and not a space is replaced with `*`.

    #[test]
    fn test_sanitize_error_msg_replaces_control_and_ansi() {
        // `error` kept, ESC(0x1b)->*, `[`->*, `31` kept, `m` kept, `red` kept,
        // NUL(0x00)->*.
        assert_eq!(
            sanitize_error_msg(b"error\x1b[31mred\x00"),
            "error**31mred*"
        );
    }

    #[test]
    fn test_sanitize_error_msg_high_bytes() {
        // Bytes > 0x7f are non-ASCII-alphanumeric -> `*`.
        assert_eq!(sanitize_error_msg(b"a\xff\x80z"), "a**z");
    }

    #[test]
    fn test_sanitize_error_msg_preserves_alnum_space() {
        assert_eq!(sanitize_error_msg(b"valid error 123"), "valid error 123");
    }

    #[test]
    fn test_sanitize_error_msg_empty() {
        assert_eq!(sanitize_error_msg(b""), "");
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

        // Second message should be the deferred (error-drop) shutdown so the
        // loop drains the ERROR_MSG for 5 s before closing the socket.
        match rx.recv().await.unwrap() {
            OutboundMessage::ShutdownAfterError => {}
            other => panic!(
                "expected ShutdownAfterError, got {:?}",
                std::mem::discriminant(&other)
            ),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn test_error_drop_drains_before_close() {
        // §12.3: on the error-drop path the loop waits the full 5 s drain
        // before returning, when the node stays up.
        let running = AtomicBool::new(true);
        let start = tokio::time::Instant::now();
        OverlayManager::wait_error_drop_drain(&running).await;
        let elapsed = start.elapsed();
        assert!(
            elapsed >= ERROR_DROP_DRAIN_DELAY,
            "error-drop drain must wait the full {ERROR_DROP_DRAIN_DELAY:?}, waited {elapsed:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn test_error_drop_drain_interrupted_by_shutdown() {
        // Node shutdown (`running` → false) cuts the 5 s drain short so overlay
        // teardown is never blocked 5 s per peer.
        let running = AtomicBool::new(false);
        let start = tokio::time::Instant::now();
        OverlayManager::wait_error_drop_drain(&running).await;
        let elapsed = start.elapsed();
        assert!(
            elapsed < ERROR_DROP_DRAIN_DELAY,
            "shutdown must interrupt the drain well before {ERROR_DROP_DRAIN_DELAY:?}, waited {elapsed:?}"
        );
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
        let peers_msg = StellarMessage::Peers(stellar_xdr::VecM::default());
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
            stellar_xdr::Auth { flags: 200 }
        )));
        assert!(should_skip_generic_routing(&StellarMessage::SendMore(
            stellar_xdr::SendMore { num_messages: 1 }
        )));
        assert!(should_skip_generic_routing(
            &StellarMessage::SendMoreExtended(stellar_xdr::SendMoreExtended {
                num_messages: 1,
                num_bytes: 1,
            })
        ));
        assert!(!should_skip_generic_routing(&StellarMessage::Peers(
            stellar_xdr::VecM::default()
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
        use stellar_xdr::TransactionEnvelope;
        let config = FlowControlConfig::default();
        let fc = Arc::new(FlowControl::new(config.clone()));

        // Exhaust all flood capacity by locking messages until none remain.
        let mut guards = Vec::new();
        for _ in 0..config.peer_flood_reading_capacity {
            let msg = StellarMessage::Transaction(TransactionEnvelope::Tx(
                stellar_xdr::TransactionV1Envelope {
                    tx: stellar_xdr::Transaction {
                        source_account: stellar_xdr::MuxedAccount::Ed25519(stellar_xdr::Uint256(
                            [0; 32],
                        )),
                        fee: 100,
                        seq_num: stellar_xdr::SequenceNumber(1),
                        cond: stellar_xdr::Preconditions::None,
                        memo: stellar_xdr::Memo::None,
                        operations: stellar_xdr::VecM::default(),
                        ext: stellar_xdr::TransactionExt::V0,
                    },
                    signatures: stellar_xdr::VecM::default(),
                },
            ));
            match crate::flow_control::CapacityGuard::new(Arc::clone(&fc), msg) {
                Some(guard) => guards.push(guard),
                None => break,
            }
        }

        // Next message should fail — capacity exhausted.
        let overflow_msg = StellarMessage::Transaction(TransactionEnvelope::Tx(
            stellar_xdr::TransactionV1Envelope {
                tx: stellar_xdr::Transaction {
                    source_account: stellar_xdr::MuxedAccount::Ed25519(stellar_xdr::Uint256(
                        [1; 32],
                    )),
                    fee: 100,
                    seq_num: stellar_xdr::SequenceNumber(2),
                    cond: stellar_xdr::Preconditions::None,
                    memo: stellar_xdr::Memo::None,
                    operations: stellar_xdr::VecM::default(),
                    ext: stellar_xdr::TransactionExt::V0,
                },
                signatures: stellar_xdr::VecM::default(),
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
        use stellar_xdr::TransactionEnvelope;
        let config = FlowControlConfig::default();
        let fc = Arc::new(FlowControl::new(config.clone()));

        // Exhaust flood capacity.
        let mut guards = Vec::new();
        for _ in 0..config.peer_flood_reading_capacity {
            let msg = StellarMessage::Transaction(TransactionEnvelope::Tx(
                stellar_xdr::TransactionV1Envelope {
                    tx: stellar_xdr::Transaction {
                        source_account: stellar_xdr::MuxedAccount::Ed25519(stellar_xdr::Uint256(
                            [0; 32],
                        )),
                        fee: 100,
                        seq_num: stellar_xdr::SequenceNumber(1),
                        cond: stellar_xdr::Preconditions::None,
                        memo: stellar_xdr::Memo::None,
                        operations: stellar_xdr::VecM::default(),
                        ext: stellar_xdr::TransactionExt::V0,
                    },
                    signatures: stellar_xdr::VecM::default(),
                },
            ));
            match crate::flow_control::CapacityGuard::new(Arc::clone(&fc), msg) {
                Some(guard) => guards.push(guard),
                None => break,
            }
        }

        // Non-flow-controlled message (Peers) should still be accepted.
        let peers_msg = StellarMessage::Peers(stellar_xdr::VecM::default());
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
        use stellar_xdr::*;

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
    fn test_peer_rate_limiter_allows_high_tps_flood() {
        // Regression (#flood-coverage-gap): under high classic-tx load a single
        // peer's legitimate flood (fulfills + demands concentrating on the holder
        // of demanded txns) far exceeds the old 150/s TxAndDemand limit. Dropping
        // those messages caused coverage gaps → tx age-out at pending_depth=4 →
        // stranded loadgen accounts → MaxTPSClassic measured-ceiling pinned at
        // ~219 despite ~400+/s real apply/flood throughput. Flow control (not this
        // limiter) is the real per-peer flood backpressure, so a high legitimate
        // per-peer rate MUST NOT be hard-dropped here. (Would fail at the old
        // DEFAULT_TX_DEMAND_LIMIT=150.)
        let mut limiter = PeerRateLimiter::new();
        for i in 0..5000u32 {
            assert!(
                limiter.allow(TrafficClass::TxAndDemand),
                "legitimate high-TPS flood must not be rate-limited (dropped at {i})"
            );
        }
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

    /// Regression test for #3428: a Survey message dropped at the aggregate
    /// cap must increment `dropped_aggregate` by exactly 1, matching the other
    /// traffic classes. Previously the aggregate-exhausted match arm did
    /// `Survey => self.dropped_aggregate += 1` AND then the unconditional
    /// `self.dropped_aggregate += 1` ran, so each dropped Survey added +2.
    #[test]
    fn test_peer_rate_limiter_survey_drop_single_aggregate_count() {
        let mut limiter = PeerRateLimiter::new();

        // Fill the aggregate window with Survey messages (all allowed).
        for _ in 0..DEFAULT_PEER_RATE_LIMIT {
            assert!(limiter.allow(TrafficClass::Survey));
        }

        // Drop N more Survey messages past the aggregate cap (all rejected).
        const N: u64 = 5;
        for _ in 0..N {
            assert!(!limiter.allow(TrafficClass::Survey));
        }

        // Each aggregate-cap drop must count exactly once, not twice.
        assert_eq!(
            limiter.dropped_aggregate, N,
            "each dropped Survey must add +1 to dropped_aggregate (not +2)"
        );
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
            StellarMessage::TxSet(stellar_xdr::TransactionSet {
                previous_ledger_hash: stellar_xdr::Hash([0; 32]),
                txs: stellar_xdr::VecM::default(),
            }),
            StellarMessage::DontHave(stellar_xdr::DontHave {
                type_: stellar_xdr::MessageType::TxSet,
                req_hash: stellar_xdr::Uint256([0; 32]),
            }),
            StellarMessage::ScpQuorumset(stellar_xdr::ScpQuorumSet {
                threshold: 1,
                validators: stellar_xdr::VecM::default(),
                inner_sets: stellar_xdr::VecM::default(),
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

    /// Regression test for #2626: verify `PingTracker::check_response` emits
    /// the `stellar_overlay_connection_latency_seconds` histogram on match,
    /// and does NOT emit on non-match or consumed ping.
    #[test]
    fn test_ping_rtt_histogram_emission() {
        use metrics_exporter_prometheus::PrometheusBuilder;

        let recorder = PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();

        let matching_hash = stellar_xdr::Uint256([1u8; 32]);
        let non_matching_hash = stellar_xdr::Uint256([2u8; 32]);
        let peer_id = PeerId(stellar_xdr::PublicKey::PublicKeyTypeEd25519(
            stellar_xdr::Uint256([0u8; 32]),
        ));

        metrics::with_local_recorder(&recorder, || {
            let mut ping = PingTracker::new();
            ping.record_sent(matching_hash.clone());

            // Non-matching hash: no RTT, no histogram emission.
            let rtt = ping.check_response(&non_matching_hash, &peer_id);
            assert!(rtt.is_none(), "non-matching hash should return None");

            // Matching hash: RTT recorded, histogram emitted.
            let rtt = ping.check_response(&matching_hash, &peer_id);
            assert!(rtt.is_some(), "matching hash should return Some(rtt)");

            let output = handle.render();
            assert!(
                output.contains("stellar_overlay_connection_latency_seconds_count 1"),
                "histogram count should be 1 after one matching response.\nOutput:\n{}",
                output,
            );

            // Consumed ping: second check with same hash returns None, count stays 1.
            let rtt = ping.check_response(&matching_hash, &peer_id);
            assert!(rtt.is_none(), "consumed ping should return None");

            let output = handle.render();
            assert!(
                output.contains("stellar_overlay_connection_latency_seconds_count 1"),
                "histogram count should still be 1 after consumed ping.\nOutput:\n{}",
                output,
            );
        });
    }

    // --- #3625: drain-gated inbound flow-control (SEND_MORE release timing) ---

    /// Build a distinct SCP envelope `StellarMessage` keyed by `slot_index`.
    fn scp_flood_msg(slot_index: u64) -> StellarMessage {
        use stellar_xdr::*;
        StellarMessage::ScpMessage(ScpEnvelope {
            statement: ScpStatement {
                node_id: NodeId(PublicKey::PublicKeyTypeEd25519(Uint256([0; 32]))),
                slot_index,
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
        })
    }

    /// Count `SEND_MORE_EXTENDED` messages currently queued on a peer outbound
    /// receiver, draining it in the process.
    fn drain_send_more_extended(
        rx: &mut mpsc::Receiver<crate::manager::OutboundMessage>,
    ) -> Vec<stellar_xdr::SendMoreExtended> {
        let mut out = Vec::new();
        while let Ok(m) = rx.try_recv() {
            if let crate::manager::OutboundMessage::Send(StellarMessage::SendMoreExtended(sme)) = m
            {
                out.push(sme);
            }
        }
        out
    }

    /// The released SCP credit drives `end_message_processing` exactly once,
    /// even if the token is explicitly released and then dropped — no
    /// double-release. The per-peer flood counter returns to baseline.
    #[test]
    fn test_release_token_fires_end_message_processing_once() {
        let config = FlowControlConfig::default();
        let batch = config.flow_control_send_more_batch_size; // 40
        let fc = Arc::new(crate::flow_control::FlowControl::new(config));
        let (tx, mut rx) = mpsc::channel::<crate::manager::OutboundMessage>(64);
        let metrics = Arc::new(crate::metrics::OverlayMetrics::new());

        // Lock capacity for one SCP message (mirrors begin_message_processing on
        // the peer task), then hand the release obligation to the token.
        let msg = scp_flood_msg(1);
        assert!(fc.begin_message_processing(&msg));
        let mut token =
            crate::manager::FlowControlRelease::new(Arc::clone(&fc), msg, tx, Arc::clone(&metrics));

        // First release fires end_message_processing (flood_data_processed -> 1).
        token.release();
        let after_first = fc.test_flood_data_processed();
        assert_eq!(
            after_first, 1,
            "first release must process exactly one flood message"
        );

        // Second release (and the subsequent Drop) must be no-ops.
        token.release();
        drop(token);
        assert_eq!(
            fc.test_flood_data_processed(),
            after_first,
            "release must be idempotent — no double end_message_processing"
        );

        // A single message is far below the 40-batch boundary, so no SEND_MORE.
        assert!(
            drain_send_more_extended(&mut rx).is_empty(),
            "no SEND_MORE_EXTENDED below the {}-message batch boundary",
            batch
        );
    }

    /// Routing >= one full batch of SCP messages through `route_to_subscribers`
    /// with the SCP consumer NEVER draining must NOT grant any
    /// `SEND_MORE_EXTENDED` to the peer. On origin/main, credit was released at
    /// channel-enqueue time, so this FAILS (a batch's worth of grants appear).
    #[tokio::test]
    async fn test_send_more_withheld_while_scp_consumer_stalls() {
        let config = FlowControlConfig::default();
        let batch = config.flow_control_send_more_batch_size; // 40
        let fc = Arc::new(crate::flow_control::FlowControl::new(config));
        let (tx, mut rx) = mpsc::channel::<crate::manager::OutboundMessage>(256);
        let metrics = Arc::new(crate::metrics::OverlayMetrics::new());

        let (shared, _scp_rx) = crate::manager::tests::shared_state_with_scp_receiver();

        // Route a full batch of SCP messages, each carrying a release token, but
        // NEVER drain `_scp_rx` (the stalled-consumer condition). The tokens
        // therefore never release, so no SEND_MORE_EXTENDED is granted.
        for slot in 0..batch {
            let msg = scp_flood_msg(slot);
            assert!(fc.begin_message_processing(&msg));
            let token = crate::manager::FlowControlRelease::new(
                Arc::clone(&fc),
                msg.clone(),
                tx.clone(),
                Arc::clone(&metrics),
            );
            let mut overlay_msg = crate::manager::OverlayMessage::new(
                crate::PeerId::from_bytes([7u8; 32]),
                msg,
                Instant::now(),
            );
            overlay_msg.flow_release = Some(token);
            shared.route_to_subscribers(overlay_msg);
        }

        let grants = drain_send_more_extended(&mut rx);
        assert!(
            grants.is_empty(),
            "a stalled SCP consumer must not be granted SEND_MORE_EXTENDED \
             (got {} grants); credit must be released at drain, not enqueue",
            grants.len()
        );
    }

    /// After the consumer drains and releases the tokens, exactly one
    /// `SEND_MORE_EXTENDED` is granted per 40-message batch, carrying the batch
    /// message count.
    #[tokio::test]
    async fn test_send_more_granted_after_consumer_drains() {
        let config = FlowControlConfig::default();
        let batch = config.flow_control_send_more_batch_size; // 40
        let fc = Arc::new(crate::flow_control::FlowControl::new(config));
        let (tx, mut rx) = mpsc::channel::<crate::manager::OutboundMessage>(256);
        let metrics = Arc::new(crate::metrics::OverlayMetrics::new());

        let (shared, mut scp_rx) = crate::manager::tests::shared_state_with_scp_receiver();

        // Route exactly one batch of token-bearing SCP messages.
        for slot in 0..batch {
            let msg = scp_flood_msg(slot);
            assert!(fc.begin_message_processing(&msg));
            let token = crate::manager::FlowControlRelease::new(
                Arc::clone(&fc),
                msg.clone(),
                tx.clone(),
                Arc::clone(&metrics),
            );
            let mut overlay_msg = crate::manager::OverlayMessage::new(
                crate::PeerId::from_bytes([7u8; 32]),
                msg,
                Instant::now(),
            );
            overlay_msg.flow_release = Some(token);
            shared.route_to_subscribers(overlay_msg);
        }

        // No grant yet — nothing drained.
        assert!(drain_send_more_extended(&mut rx).is_empty());

        // The consumer drains the channel and releases each token (mirrors
        // pump_scp_intake taking + dropping the token).
        let mut drained = 0u64;
        while let Ok(mut overlay_msg) = scp_rx.try_recv() {
            let _release = overlay_msg.take_flow_release();
            // _release drops here, firing end_message_processing.
            drained += 1;
        }
        assert_eq!(drained, batch, "consumer should drain the whole batch");

        let grants = drain_send_more_extended(&mut rx);
        assert_eq!(
            grants.len(),
            1,
            "exactly one SEND_MORE_EXTENDED per {}-message batch",
            batch
        );
        assert_eq!(
            grants[0].num_messages as u64, batch,
            "the grant must request the full batch of flood messages"
        );
    }

    // ---------------------------------------------------------------------
    // #3642 — Phase 2: read-side socket throttle wired into the per-peer loop.
    //
    // These exercise the `can_read()` gate the `peer.recv()` select arm keys on
    // and the consumer-drain `Notify` resume path that lifts the throttle.
    // Mirror stellar-core `TCPPeer::maybeThrottleRead` (TCPPeer.cpp:620/770) and
    // `Peer.cpp:313-333` (`stopThrottling`+`scheduleRead` gated on
    // `isThrottled() && numTotalMessages > 0`).
    // ---------------------------------------------------------------------

    /// Build a `FlowControlConfig` with a small total reading capacity so a
    /// handful of un-drained SCP envelopes exhaust it (the 201-batch scaled down
    /// for the test). The flood batch is kept >= the reading capacity so the
    /// resume gate is driven purely by the total-message track, isolating the
    /// `num_total_messages` reschedule condition.
    fn small_capacity_config(reading_capacity: u64) -> FlowControlConfig {
        FlowControlConfig {
            peer_reading_capacity: reading_capacity,
            // Keep flood batch larger than reading capacity so the flood
            // SEND_MORE boundary never fires first and the only resume trigger
            // is the total-message batch (matches the 201>40 production ratio
            // not being relied on here — we want the total track to drive).
            flow_control_send_more_batch_size: reading_capacity + 10,
            peer_flood_reading_capacity: reading_capacity + 10,
            ..FlowControlConfig::default()
        }
    }

    /// #3642 test 1 — read throttles when total reading capacity is exhausted.
    ///
    /// Lock capacity for `reading_capacity` SCP messages via
    /// `begin_message_processing` WITHOUT releasing (consumer stalled). The total
    /// capacity track hits 0, so `can_read()` (the predicate the `peer.recv()`
    /// select arm is gated on) is false and `maybe_throttle_read()` arms the
    /// throttle. This is the gate the wiring keys on.
    #[test]
    fn test_read_throttles_when_total_capacity_exhausted() {
        let cap = 4u64;
        let fc = Arc::new(crate::flow_control::FlowControl::new(
            small_capacity_config(cap),
        ));

        // Plenty of capacity at the start.
        assert!(fc.can_read(), "fresh peer can read");
        assert!(
            !fc.maybe_throttle_read(),
            "no throttle while capacity remains"
        );

        // Lock capacity for a full reading batch without ever releasing.
        for slot in 0..cap {
            assert!(
                fc.begin_message_processing(&scp_flood_msg(slot)),
                "capacity should be available for message {slot}"
            );
        }

        // Total capacity is now 0 → the recv-arm gate must be closed.
        assert!(
            !fc.can_read(),
            "total reading capacity exhausted → can_read() false (recv arm disabled)"
        );
        // maybe_throttle_read records the throttle and reports it engaged.
        assert!(
            fc.maybe_throttle_read(),
            "maybe_throttle_read arms the throttle when can_read() is false"
        );
        assert!(fc.is_throttled(), "peer is now throttled");
    }

    /// #3642 test 2 — the no-deadlock recovery centerpiece. After the consumer
    /// drains a full reading-capacity batch, the throttle lifts (Notify fires,
    /// `is_throttled()` false, `can_read()` true) WITHOUT any further socket
    /// read. Proves recovery is driven by consumer drain alone.
    #[tokio::test]
    async fn test_throttle_lifts_and_read_resumes_after_consumer_drains() {
        let cap = 4u64;
        let fc = Arc::new(crate::flow_control::FlowControl::new(
            small_capacity_config(cap),
        ));
        let (tx, _rx) = mpsc::channel::<crate::manager::OutboundMessage>(64);
        let metrics = Arc::new(crate::metrics::OverlayMetrics::new());
        let notify = Arc::new(tokio::sync::Notify::new());

        // Exhaust capacity by locking a full batch into release tokens (the
        // consumer-stalled state) and arm the throttle.
        let mut tokens = Vec::new();
        for slot in 0..cap {
            let msg = scp_flood_msg(slot);
            assert!(fc.begin_message_processing(&msg));
            tokens.push(
                crate::manager::FlowControlRelease::new(
                    Arc::clone(&fc),
                    msg,
                    tx.clone(),
                    Arc::clone(&metrics),
                )
                .with_resume_notify(Arc::clone(&notify)),
            );
        }
        assert!(fc.maybe_throttle_read(), "throttle armed at exhaustion");
        assert!(!fc.can_read(), "throttled: recv arm disabled");
        assert!(fc.is_throttled());

        // The select-arm wake: a clone of the per-peer Notify. It must NOT be
        // already-notified before the drain.
        let notified = notify.notified();
        tokio::pin!(notified);
        assert!(
            futures::poll!(notified.as_mut()).is_pending(),
            "Notify must not fire before the consumer drains a full batch"
        );

        // Consumer drains the whole batch (drops every token) — the ONLY action,
        // no socket read occurs.
        tokens.clear();

        // Throttle lifted via the drain-release path.
        assert!(
            !fc.is_throttled(),
            "stop_throttling() ran on the batch refill"
        );
        assert!(fc.can_read(), "capacity restored → recv arm re-enabled");
        // The peer-loop wake fired.
        assert!(
            futures::poll!(notified.as_mut()).is_ready(),
            "consumer drain must fire the resume Notify (core scheduleRead equivalent)"
        );
    }

    /// #3642 test 3 — self-wedge invariant: the fetch backfill path is never
    /// flow-controlled, so it is never throttled and a throttled SCP socket
    /// cannot starve the fetch the consumer depends on.
    #[test]
    fn test_throttle_does_not_block_fetch_path() {
        use crate::flow_control::is_flow_controlled_message;

        // The fetch responses SCP backfill depends on. These ride the separate
        // unbounded fetch_response channel and are excluded from flow control.
        let txset = StellarMessage::TxSet(stellar_xdr::TransactionSet {
            previous_ledger_hash: stellar_xdr::Hash([0; 32]),
            txs: stellar_xdr::VecM::default(),
        });
        let gen_txset = StellarMessage::GeneralizedTxSet(
            stellar_xdr::GeneralizedTransactionSet::V1(stellar_xdr::TransactionSetV1 {
                previous_ledger_hash: stellar_xdr::Hash([0; 32]),
                phases: stellar_xdr::VecM::default(),
            }),
        );
        let qset = StellarMessage::ScpQuorumset(stellar_xdr::ScpQuorumSet {
            threshold: 1,
            validators: stellar_xdr::VecM::default(),
            inner_sets: stellar_xdr::VecM::default(),
        });
        let dont_have = StellarMessage::DontHave(stellar_xdr::DontHave {
            type_: stellar_xdr::MessageType::TxSet,
            req_hash: stellar_xdr::Uint256([0; 32]),
        });

        for msg in [&txset, &gen_txset, &qset, &dont_have] {
            assert!(
                !is_flow_controlled_message(msg),
                "fetch-path message must NOT be flow-controlled (it rides the \
                 unbounded fetch_response channel and is never throttled): {:?}",
                helpers::message_type_name(msg)
            );
        }

        // And the messages that ARE flow-controlled (and therefore throttleable)
        // are exactly the flood set — fetch responses are excluded.
        assert!(is_flow_controlled_message(&scp_flood_msg(0)));
    }

    /// #3642 test 4 — reschedule only on a full total batch (Peer.cpp:314 gate:
    /// `numTotalMessages > 0`, set only at the 201st drain). A partial drain that
    /// does not complete a reading batch must NOT lift the throttle or fire the
    /// Notify.
    #[tokio::test]
    async fn test_reschedule_only_on_full_total_batch() {
        let cap = 4u64;
        let fc = Arc::new(crate::flow_control::FlowControl::new(
            small_capacity_config(cap),
        ));
        let (tx, _rx) = mpsc::channel::<crate::manager::OutboundMessage>(64);
        let metrics = Arc::new(crate::metrics::OverlayMetrics::new());
        let notify = Arc::new(tokio::sync::Notify::new());

        let mut tokens = Vec::new();
        for slot in 0..cap {
            let msg = scp_flood_msg(slot);
            assert!(fc.begin_message_processing(&msg));
            tokens.push(
                crate::manager::FlowControlRelease::new(
                    Arc::clone(&fc),
                    msg,
                    tx.clone(),
                    Arc::clone(&metrics),
                )
                .with_resume_notify(Arc::clone(&notify)),
            );
        }
        assert!(fc.maybe_throttle_read());

        let notified = notify.notified();
        tokio::pin!(notified);

        // Drain all but one — the total batch is NOT complete, so no reschedule.
        for _ in 0..(cap - 1) {
            tokens.remove(0); // drop one token → one end_message_processing
            assert!(
                fc.is_throttled(),
                "partial drain must not lift the throttle (numTotalMessages == 0)"
            );
            assert!(
                futures::poll!(notified.as_mut()).is_pending(),
                "Notify must not fire on a partial-batch drain"
            );
        }

        // Drop the last token → completes the reading batch → reschedule fires.
        tokens.clear();
        assert!(
            !fc.is_throttled(),
            "the batch-completing drain lifts the throttle (numTotalMessages > 0)"
        );
        assert!(
            futures::poll!(notified.as_mut()).is_ready(),
            "the batch-completing drain fires the resume Notify exactly once"
        );
    }

    /// #3773: the non-Load `ErrorMsg` warn must carry a `recent_sends` field
    /// populated from the peer's outbound diagnostic ring, so an operator who
    /// sees a peer-reported `ERR_DATA "received corrupt XDR"` can identify the
    /// frames henyey sent just before the peer dropped us. Uses the shared
    /// `tracing_capture` harness (now enabled via the `test-support` dev
    /// feature) to inspect the emitted event's structured fields.
    #[test]
    fn test_error_msg_warn_includes_recent_sends() {
        use henyey_common::test_support::tracing_capture::capture_events;

        // A current-thread runtime we drive via `block_on`, so the async
        // `handle_received_message` runs on THIS thread — the one where
        // `capture_events` installs its thread-local subscriber.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        let metrics = Arc::new(crate::metrics::OverlayMetrics::new());
        let (mut peer, _peer_b) = crate::peer::Peer::new_test_authenticated_pair(
            Arc::clone(&metrics),
            Arc::new(crate::metrics::OverlayMetrics::new()),
        );
        // Populate the ring with a real outbound send so the summary is
        // non-empty when the warn reads it.
        rt.block_on(peer.send(StellarMessage::GetScpState(9)))
            .expect("send to populate ring");

        // PeerLoopCtx scaffolding. The non-Load ErrorMsg branch returns
        // `Break` before touching flow control / capacity, so these are only
        // needed to satisfy the signature.
        let mut received_peers = false;
        let mut ping = PingTracker::new();
        let mut query_limiter = QueryRateLimiter::new();
        let mut peer_rate_limiter = PeerRateLimiter::new();
        let mut scp_messages = 0u64;
        let mut last_write = Instant::now();
        let mut enqueue_time_of_last_write = Instant::now();
        let mut scp_written = 0u64;
        let (outbound_tx, _outbound_rx) = mpsc::channel::<OutboundMessage>(16);
        let mut ctx = PeerLoopCtx {
            peer: &mut peer,
            received_peers: &mut received_peers,
            ping: &mut ping,
            query_limiter: &mut query_limiter,
            peer_rate_limiter: &mut peer_rate_limiter,
            scp_messages: &mut scp_messages,
            last_write: &mut last_write,
            enqueue_time_of_last_write: &mut enqueue_time_of_last_write,
            scp_written: &mut scp_written,
            outbound_tx: &outbound_tx,
        };

        let peer_id = crate::PeerId::from_bytes([7u8; 32]);
        let (shared, _scp_rx) = crate::manager::tests::shared_state_with_scp_receiver();
        let fc = Arc::new(crate::flow_control::FlowControl::new(
            FlowControlConfig::default(),
        ));
        let read_resume = Arc::new(tokio::sync::Notify::new());

        // A non-Load ERROR — the exact class peers report in #3773.
        let err_msg = StellarMessage::ErrorMsg(stellar_xdr::SError {
            code: ErrorCode::Data,
            msg: stellar_xdr::StringM::try_from("received corrupt XDR".to_string()).unwrap(),
        });

        let events = capture_events(|| {
            let action = rt.block_on(OverlayManager::handle_received_message(
                err_msg,
                &peer_id,
                &mut ctx,
                &fc,
                &shared,
                false,
                &read_resume,
            ));
            assert!(
                matches!(action, RecvAction::Break),
                "a peer-sent ErrorMsg must terminate the loop"
            );
        });

        let warn = events
            .iter()
            .find(|e| e.message.contains("sent_error"))
            .expect("a `Peer sent_error` warn must be emitted for a non-Load ErrorMsg");
        let (_, value) = warn
            .fields
            .iter()
            .find(|(k, _)| k == "recent_sends")
            .expect("the sent_error warn must carry a `recent_sends` field");
        assert!(
            value.contains("GET_SCP_STATE"),
            "recent_sends must surface the frame(s) sent before the drop, got: {value}"
        );
    }
}
