//! Overlay network metrics collection.
//!
//! This module implements the OverlayMetrics from stellar-core, providing
//! comprehensive metrics for monitoring overlay network operations.
//!
//! # Overview
//!
//! Metrics are organized into categories:
//!
//! - **Message metrics**: Counts of messages read, written, dropped
//! - **Byte metrics**: Bytes read and written
//! - **Error metrics**: Read and write errors
//! - **Timeout metrics**: Idle and straggler timeouts
//! - **Connection metrics**: Pending and authenticated peer counts
//! - **Send counters**: Counts per message type sent
//! - **Queue metrics**: Outbound queue drops
//! - **Flood metrics**: Transaction flooding statistics
//! - **Fetch metrics**: Item fetcher statistics
//! - **Pull metrics**: Demand timeouts and pulled transaction counts
//!
//! # Thread Safety
//!
//! All metrics use atomic operations and are safe to access from multiple threads.

use std::sync::atomic::{AtomicU64, Ordering};

use stellar_xdr::StellarMessage;

/// Atomic counter for simple metrics.
#[derive(Debug, Default)]
pub struct Counter {
    value: AtomicU64,
}

impl Counter {
    /// Create a new counter starting at 0.
    pub fn new() -> Self {
        Self {
            value: AtomicU64::new(0),
        }
    }

    /// Increment the counter by 1.
    pub fn inc(&self) {
        self.value.fetch_add(1, Ordering::Relaxed);
    }

    /// Increment the counter by n.
    pub fn add(&self, n: u64) {
        self.value.fetch_add(n, Ordering::Relaxed);
    }

    /// Set the counter to a specific value.
    pub fn set(&self, n: u64) {
        self.value.store(n, Ordering::Relaxed);
    }

    /// Get the current value.
    pub fn get(&self) -> u64 {
        self.value.load(Ordering::Relaxed)
    }

    /// Reset to 0 and return the previous value.
    pub fn reset(&self) -> u64 {
        self.value.swap(0, Ordering::Relaxed)
    }
}

fn reset_counters(counters: &[&Counter]) {
    for counter in counters {
        counter.reset();
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// OverlayMessageKind — canonical message classifier for metrics and logging
// ═══════════════════════════════════════════════════════════════════════════

/// Classifies `StellarMessage` variants for per-type send metrics.
///
/// Each variant corresponds to exactly one XDR `StellarMessage` discriminant.
/// Intentionally richer than stellar-core's 19 grouped meters (which merge
/// `TX_SET`/`GENERALIZED_TX_SET` and `SEND_MORE`/`SEND_MORE_EXTENDED`). 21
/// labels are trivially aggregable in PromQL.
///
/// # Counting semantics
///
/// Counters increment **after** successful wire send. On connection failure,
/// no increment occurs. This differs from stellar-core which counts pre-send
/// (`Peer.cpp:830`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum OverlayMessageKind {
    ErrorMsg = 0,
    Hello = 1,
    Auth = 2,
    DontHave = 3,
    Peers = 4,
    GetTxSet = 5,
    TxSet = 6,
    GeneralizedTxSet = 7,
    Transaction = 8,
    GetScpQuorumset = 9,
    ScpQuorumset = 10,
    ScpMessage = 11,
    GetScpState = 12,
    SendMore = 13,
    SendMoreExtended = 14,
    FloodAdvert = 15,
    FloodDemand = 16,
    TimeSlicedSurveyRequest = 17,
    TimeSlicedSurveyResponse = 18,
    TimeSlicedSurveyStartCollecting = 19,
    TimeSlicedSurveyStopCollecting = 20,
}

impl OverlayMessageKind {
    /// All variants in discriminant order. Single source of truth for
    /// iteration, counter allocation, and Prometheus label generation.
    pub const ALL: [Self; 21] = [
        Self::ErrorMsg,
        Self::Hello,
        Self::Auth,
        Self::DontHave,
        Self::Peers,
        Self::GetTxSet,
        Self::TxSet,
        Self::GeneralizedTxSet,
        Self::Transaction,
        Self::GetScpQuorumset,
        Self::ScpQuorumset,
        Self::ScpMessage,
        Self::GetScpState,
        Self::SendMore,
        Self::SendMoreExtended,
        Self::FloodAdvert,
        Self::FloodDemand,
        Self::TimeSlicedSurveyRequest,
        Self::TimeSlicedSurveyResponse,
        Self::TimeSlicedSurveyStartCollecting,
        Self::TimeSlicedSurveyStopCollecting,
    ];

    /// Number of variants (derived from `ALL`).
    pub const COUNT: usize = Self::ALL.len();

    /// Prometheus metric label (lowercase snake_case).
    pub const fn label(&self) -> &'static str {
        match self {
            Self::ErrorMsg => "error",
            Self::Hello => "hello",
            Self::Auth => "auth",
            Self::DontHave => "dont_have",
            Self::Peers => "peers",
            Self::GetTxSet => "get_tx_set",
            Self::TxSet => "tx_set",
            Self::GeneralizedTxSet => "generalized_tx_set",
            Self::Transaction => "transaction",
            Self::GetScpQuorumset => "get_scp_qset",
            Self::ScpQuorumset => "scp_qset",
            Self::ScpMessage => "scp_message",
            Self::GetScpState => "get_scp_state",
            Self::SendMore => "send_more",
            Self::SendMoreExtended => "send_more_extended",
            Self::FloodAdvert => "flood_advert",
            Self::FloodDemand => "flood_demand",
            Self::TimeSlicedSurveyRequest => "time_sliced_survey_request",
            Self::TimeSlicedSurveyResponse => "time_sliced_survey_response",
            Self::TimeSlicedSurveyStartCollecting => "time_sliced_survey_start_collecting",
            Self::TimeSlicedSurveyStopCollecting => "time_sliced_survey_stop_collecting",
        }
    }

    /// Uppercase wire name for logging (matches existing `message_type_name` output).
    pub const fn wire_name(&self) -> &'static str {
        match self {
            Self::ErrorMsg => "ERROR",
            Self::Hello => "HELLO",
            Self::Auth => "AUTH",
            Self::DontHave => "DONT_HAVE",
            Self::Peers => "PEERS",
            Self::GetTxSet => "GET_TX_SET",
            Self::TxSet => "TX_SET",
            Self::GeneralizedTxSet => "GENERALIZED_TX_SET",
            Self::Transaction => "TRANSACTION",
            Self::GetScpQuorumset => "GET_SCP_QUORUMSET",
            Self::ScpQuorumset => "SCP_QUORUMSET",
            Self::ScpMessage => "SCP_MESSAGE",
            Self::GetScpState => "GET_SCP_STATE",
            Self::SendMore => "SEND_MORE",
            Self::SendMoreExtended => "SEND_MORE_EXTENDED",
            Self::FloodAdvert => "FLOOD_ADVERT",
            Self::FloodDemand => "FLOOD_DEMAND",
            Self::TimeSlicedSurveyRequest => "TIME_SLICED_SURVEY_REQUEST",
            Self::TimeSlicedSurveyResponse => "TIME_SLICED_SURVEY_RESPONSE",
            Self::TimeSlicedSurveyStartCollecting => "TIME_SLICED_SURVEY_START_COLLECTING",
            Self::TimeSlicedSurveyStopCollecting => "TIME_SLICED_SURVEY_STOP_COLLECTING",
        }
    }

    /// Map a `StellarMessage` to its kind. Exhaustive match ensures compile-time
    /// coverage — adding a new XDR variant without updating this function is a
    /// compile error.
    pub fn from_stellar_message(msg: &StellarMessage) -> Self {
        match msg {
            StellarMessage::ErrorMsg(_) => Self::ErrorMsg,
            StellarMessage::Hello(_) => Self::Hello,
            StellarMessage::Auth(_) => Self::Auth,
            StellarMessage::DontHave(_) => Self::DontHave,
            StellarMessage::Peers(_) => Self::Peers,
            StellarMessage::GetTxSet(_) => Self::GetTxSet,
            StellarMessage::TxSet(_) => Self::TxSet,
            StellarMessage::GeneralizedTxSet(_) => Self::GeneralizedTxSet,
            StellarMessage::Transaction(_) => Self::Transaction,
            StellarMessage::GetScpQuorumset(_) => Self::GetScpQuorumset,
            StellarMessage::ScpQuorumset(_) => Self::ScpQuorumset,
            StellarMessage::ScpMessage(_) => Self::ScpMessage,
            StellarMessage::GetScpState(_) => Self::GetScpState,
            StellarMessage::SendMore(_) => Self::SendMore,
            StellarMessage::SendMoreExtended(_) => Self::SendMoreExtended,
            StellarMessage::FloodAdvert(_) => Self::FloodAdvert,
            StellarMessage::FloodDemand(_) => Self::FloodDemand,
            StellarMessage::TimeSlicedSurveyRequest(_) => Self::TimeSlicedSurveyRequest,
            StellarMessage::TimeSlicedSurveyResponse(_) => Self::TimeSlicedSurveyResponse,
            StellarMessage::TimeSlicedSurveyStartCollecting(_) => {
                Self::TimeSlicedSurveyStartCollecting
            }
            StellarMessage::TimeSlicedSurveyStopCollecting(_) => {
                Self::TimeSlicedSurveyStopCollecting
            }
        }
    }
}

// Compile-time: ALL is complete, ordered, and covers every discriminant.
const _: () = {
    let mut i = 0;
    while i < OverlayMessageKind::ALL.len() {
        assert!(OverlayMessageKind::ALL[i] as usize == i);
        i += 1;
    }
    assert!(
        OverlayMessageKind::ALL.len()
            == OverlayMessageKind::TimeSlicedSurveyStopCollecting as usize + 1
    );
};

/// Overlay network metrics.
///
/// Provides comprehensive metrics for monitoring overlay operations.
/// All fields use atomic operations for thread safety.
#[derive(Debug, Default)]
pub struct OverlayMetrics {
    // ===== Message Metrics =====
    /// Messages read from peers.
    pub messages_read: Counter,
    /// Messages written to peers.
    pub messages_written: Counter,
    /// Messages dropped (queue full, etc).
    pub messages_dropped: Counter,
    /// Messages broadcast to all peers.
    pub messages_broadcast: Counter,
    /// Fetch-response/-request messages dropped because the bounded fetch
    /// intake channel ([`crate::FETCH_CHANNEL_CAPACITY`]) was full while the
    /// event loop was wedged (#3661). Recoverable: re-requested by the
    /// periodic `request_pending_tx_sets()` tick + ItemFetcher retry.
    pub fetch_messages_dropped: Counter,
    /// Catchup-cache fan-out messages dropped because the bounded catchup
    /// channel ([`crate::CATCHUP_CHANNEL_CAPACITY`]) was full while the cache
    /// task was not draining (#3661). Recoverable: the cache is pre-warm only;
    /// re-fetched/re-flooded after the catchup→Tracking handoff.
    pub catchup_messages_dropped: Counter,
    /// Per-message-type outbound broadcast fan-out drops, indexed by
    /// [`OverlayMessageKind`]. Incremented in `OverlayManager::broadcast` when a
    /// target peer's `outbound_tx` channel is `Full` (#3792). Dedicated series
    /// so the broadcast fan-out drops — dominated by our own `SCP_MESSAGE`
    /// envelopes — are not conflated with the 5 other sites that feed the
    /// aggregate `messages_dropped` (which is fed alongside this, for continuity,
    /// but is never bridged to `/metrics`).
    pub broadcast_fanout_drop_by_type: [Counter; OverlayMessageKind::COUNT],
    /// Broadcast "blackouts": calls where at least one peer was targeted but
    /// every one rejected via `Full` (`dropped > 0 && sent == 0`), so the
    /// message reached ZERO peers (#3792). Qualitatively worse than losing one
    /// peer out of many — this is the series worth alerting on. Incremented once
    /// per such call.
    pub broadcast_blackout: Counter,

    // ===== Byte Metrics =====
    /// Bytes read from peers (wire-level: `AuthenticatedMessage` XDR body, excluding the
    /// 4-byte length header). Matches stellar-core `mByteRead`.
    pub bytes_read: Counter,
    /// Bytes written to peers (wire-level: `AuthenticatedMessage` XDR body, excluding
    /// the 4-byte length header). Matches stellar-core `mByteWrite`.
    pub bytes_written: Counter,

    // ===== Async I/O Metrics =====
    /// Successful recv I/O operations (each `Connection::recv*` returning a frame).
    /// Matches stellar-core `mAsyncRead`.
    pub async_read: Counter,
    /// Successful send I/O operations (each `Connection::send` returning Ok).
    /// Matches stellar-core `mAsyncWrite`.
    pub async_write: Counter,

    // ===== Connection Lifecycle Metrics =====
    /// Inbound connection accepts: `listener.accept()` returning `Ok`.
    /// Counts every TCP-accepted inbound connection, including those later rejected.
    pub inbound_attempt: Counter,
    /// Inbound connections that completed handshake and were fully registered as peers.
    /// Incremented exactly once per inbound peer that reaches `register_peer` Ok.
    pub inbound_establish: Counter,
    /// Inbound peer disconnections: incremented after `run_peer_loop` returns for an
    /// inbound peer (mirrors `inbound_establish`). This is the unconditional total;
    /// it always equals `inbound_drop_remote + inbound_drop_local` by construction
    /// (both siblings are incremented at the same single drop site in
    /// `manager/connection.rs`).
    pub inbound_drop: Counter,
    /// Inbound drops where the *remote peer* closed/reset the socket: `recv`
    /// returning `Ok(None)` (peer FIN) or a recv error (RST / errno 104). #3419
    /// diagnostic: distinguishes "peers churn out" (high remote ratio, normal for
    /// transient leaf peers) from "henyey drops peers". Observability-only.
    pub inbound_drop_remote: Counter,
    /// Inbound drops where *henyey* broke the peer loop: idle/straggler timeout,
    /// send / flood-send error, protocol-violation or received-ERROR_MSG teardown,
    /// shutdown, or channel close. The `send_error` case is attributed local by the
    /// "who broke the loop" convention even though the underlying cause may be a
    /// remote RST surfaced on the next write. #3419 diagnostic; observability-only.
    pub inbound_drop_local: Counter,
    /// Inbound connections rejected at any point after accept but before establish
    /// (handshake failure, banned, duplicate, slots full, register race).
    pub inbound_reject: Counter,
    /// Monotonic count of inbound peers that transitioned to authenticated
    /// (handshake completed + registered). Diagnostic for #3419: lets an
    /// operator distinguish "never authenticated" (stays 0) from "authenticated
    /// then churned" (climbs while the instantaneous
    /// `stellar_overlay_inbound_authenticated` gauge stays near 0). This `_total`
    /// is the cumulative *trend* line; the `stellar_overlay_inbound_authenticated`
    /// gauge is the instantaneous net (currently-authenticated) count — a single
    /// snapshot of the gauge reading `== drop total` is not a regression, which
    /// misled three #3419 investigations. Mirrors the increment point of
    /// `inbound_establish`; observability-only.
    pub inbound_authenticated_total: Counter,
    /// Outbound connection attempts: a dial was actually initiated (after the
    /// address reservation succeeded inside `connect_to_discovered_peer` /
    /// `connect_to_explicit_peer`). Does NOT include caller-side skips (e.g.,
    /// `add_peer` returning early because the pool is full before dialing) or
    /// in-flight-duplicate skips (where the address is already reserved by
    /// another in-progress dial) — those are not "attempts" in the wire sense.
    pub outbound_attempt: Counter,
    /// Outbound connections that completed handshake and were fully registered.
    pub outbound_establish: Counter,
    /// Outbound peer disconnections: incremented after `run_peer_loop` returns for an
    /// outbound peer (mirrors `outbound_establish`).
    pub outbound_drop: Counter,
    /// Outbound connections rejected at any point after attempt but before establish
    /// (TCP connect fail, handshake fail, banned, duplicate, slots full, register race).
    pub outbound_reject: Counter,

    // ===== Error Metrics =====
    /// Read errors encountered.
    pub errors_read: Counter,
    /// Write errors encountered.
    pub errors_write: Counter,

    // ===== Timeout Metrics =====
    /// Idle timeouts (peer not sending data).
    pub timeouts_idle: Counter,
    /// Straggler timeouts (peer too slow).
    pub timeouts_straggler: Counter,

    // ===== Send Counters =====
    /// Per-message-type send counters, indexed by [`OverlayMessageKind`].
    /// Incremented on successful wire send only.
    pub send_by_type: [Counter; OverlayMessageKind::COUNT],

    // ===== Queue Metrics =====
    /// SCP messages dropped from queue.
    pub queue_drop_scp: Counter,
    /// Transaction messages dropped from queue.
    pub queue_drop_tx: Counter,
    /// Advert messages dropped from queue.
    pub queue_drop_advert: Counter,
    /// Demand messages dropped from queue.
    pub queue_drop_demand: Counter,

    // ===== Flood Metrics =====
    /// Messages demanded via FloodDemand.
    pub flood_demanded: Counter,
    /// Demands fulfilled (tx sent back).
    pub flood_fulfilled: Counter,
    /// Demands unfulfilled due to banned tx.
    pub flood_unfulfilled_banned: Counter,
    /// Demands unfulfilled due to unknown tx.
    pub flood_unfulfilled_unknown: Counter,
    /// Unique flood bytes received.
    pub flood_unique_bytes_recv: Counter,
    /// Duplicate flood bytes received.
    pub flood_duplicate_bytes_recv: Counter,
    /// Per-recipient flood deliveries (is_flood messages only).
    pub flood_broadcast: Counter,
    /// Unique flood messages received (inbound, record_inbound_relay → on_new).
    pub flood_unique_recv: Counter,
    /// Duplicate flood messages received (inbound, record_inbound_relay → on_repeated).
    pub flood_duplicate_recv: Counter,
    /// SCP-only unique flood messages received (#2648 diagnostic).
    pub scp_flood_unique_recv: Counter,
    /// SCP-only duplicate flood messages received (#2648 diagnostic).
    pub scp_flood_duplicate_recv: Counter,
    /// Flood messages dropped because the node is not synced.
    /// Parity: Peer.cpp:1164-1171 — counts individual messages (Transaction,
    /// FloodAdvert, FloodDemand) shed before flood-gate accounting.
    pub flood_shed_unsynced: Counter,

    // ===== Fetch Metrics =====
    /// Unique fetch bytes received.
    pub fetch_unique_bytes_recv: Counter,
    /// Duplicate fetch bytes received.
    pub fetch_duplicate_bytes_recv: Counter,
    /// ItemFetcher next-peer selections (AskPeer results only).
    pub item_fetcher_next_peer: Counter,
    /// ItemFetcher tracker cap rejections (new hashes rejected due to cap).
    pub item_fetcher_tracker_cap_reached: Counter,
    /// Unique/solicited fetch responses (TxSet/QSet tracked by ItemFetcher).
    pub fetch_unique_recv: Counter,
    /// Duplicate/unsolicited fetch responses (TxSet/QSet not tracked).
    pub fetch_duplicate_recv: Counter,

    // ===== Pull Metrics =====
    /// Demand timeouts (retry needed).
    pub demand_timeouts: Counter,
    /// Pulled transactions that were relevant.
    pub pulled_relevant_txs: Counter,
    /// Pulled transactions that were irrelevant.
    pub pulled_irrelevant_txs: Counter,
    /// Abandoned demands (never received tx).
    pub abandoned_demands: Counter,
}

impl OverlayMetrics {
    /// Create a new metrics instance with all counters at 0.
    pub fn new() -> Self {
        Self::default()
    }

    /// Get a snapshot of all metrics.
    pub fn snapshot(&self) -> OverlayMetricsSnapshot {
        OverlayMetricsSnapshot {
            // Message metrics
            messages_read: self.messages_read.get(),
            messages_written: self.messages_written.get(),
            messages_dropped: self.messages_dropped.get(),
            messages_broadcast: self.messages_broadcast.get(),
            fetch_messages_dropped: self.fetch_messages_dropped.get(),
            catchup_messages_dropped: self.catchup_messages_dropped.get(),
            broadcast_fanout_drop_by_type: std::array::from_fn(|i| {
                self.broadcast_fanout_drop_by_type[i].get()
            }),
            broadcast_blackout: self.broadcast_blackout.get(),

            // Byte metrics
            bytes_read: self.bytes_read.get(),
            bytes_written: self.bytes_written.get(),

            // Async I/O metrics
            async_read: self.async_read.get(),
            async_write: self.async_write.get(),

            // Connection lifecycle metrics
            inbound_attempt: self.inbound_attempt.get(),
            inbound_establish: self.inbound_establish.get(),
            inbound_drop: self.inbound_drop.get(),
            inbound_drop_remote: self.inbound_drop_remote.get(),
            inbound_drop_local: self.inbound_drop_local.get(),
            inbound_reject: self.inbound_reject.get(),
            inbound_authenticated_total: self.inbound_authenticated_total.get(),
            outbound_attempt: self.outbound_attempt.get(),
            outbound_establish: self.outbound_establish.get(),
            outbound_drop: self.outbound_drop.get(),
            outbound_reject: self.outbound_reject.get(),

            // Error metrics
            errors_read: self.errors_read.get(),
            errors_write: self.errors_write.get(),

            // Timeout metrics
            timeouts_idle: self.timeouts_idle.get(),
            timeouts_straggler: self.timeouts_straggler.get(),

            // Send counters
            send_by_type: std::array::from_fn(|i| self.send_by_type[i].get()),

            // Queue metrics
            queue_drop_scp: self.queue_drop_scp.get(),
            queue_drop_tx: self.queue_drop_tx.get(),
            queue_drop_advert: self.queue_drop_advert.get(),
            queue_drop_demand: self.queue_drop_demand.get(),

            // Flood metrics
            flood_demanded: self.flood_demanded.get(),
            flood_fulfilled: self.flood_fulfilled.get(),
            flood_unfulfilled_banned: self.flood_unfulfilled_banned.get(),
            flood_unfulfilled_unknown: self.flood_unfulfilled_unknown.get(),
            flood_unique_bytes_recv: self.flood_unique_bytes_recv.get(),
            flood_duplicate_bytes_recv: self.flood_duplicate_bytes_recv.get(),
            flood_broadcast: self.flood_broadcast.get(),
            flood_unique_recv: self.flood_unique_recv.get(),
            flood_duplicate_recv: self.flood_duplicate_recv.get(),
            scp_flood_unique_recv: self.scp_flood_unique_recv.get(),
            scp_flood_duplicate_recv: self.scp_flood_duplicate_recv.get(),
            flood_shed_unsynced: self.flood_shed_unsynced.get(),

            // Fetch metrics
            fetch_unique_bytes_recv: self.fetch_unique_bytes_recv.get(),
            fetch_duplicate_bytes_recv: self.fetch_duplicate_bytes_recv.get(),
            item_fetcher_next_peer: self.item_fetcher_next_peer.get(),
            item_fetcher_tracker_cap_reached: self.item_fetcher_tracker_cap_reached.get(),
            fetch_unique_recv: self.fetch_unique_recv.get(),
            fetch_duplicate_recv: self.fetch_duplicate_recv.get(),

            // Populated externally by the app layer from FloodGate::stats().
            flood_known_count: 0,

            // Pull metrics
            demand_timeouts: self.demand_timeouts.get(),
            pulled_relevant_txs: self.pulled_relevant_txs.get(),
            pulled_irrelevant_txs: self.pulled_irrelevant_txs.get(),
            abandoned_demands: self.abandoned_demands.get(),
        }
    }

    /// Record a successful message send of the given kind.
    pub fn record_send(&self, kind: OverlayMessageKind) {
        self.send_by_type[kind as usize].inc();
    }

    /// Reset all metrics to initial state.
    pub fn reset(&self) {
        reset_counters(&[
            &self.messages_read,
            &self.messages_written,
            &self.messages_dropped,
            &self.messages_broadcast,
            &self.fetch_messages_dropped,
            &self.catchup_messages_dropped,
            &self.broadcast_blackout,
            &self.bytes_read,
            &self.bytes_written,
            &self.async_read,
            &self.async_write,
            &self.inbound_attempt,
            &self.inbound_establish,
            &self.inbound_drop,
            &self.inbound_drop_remote,
            &self.inbound_drop_local,
            &self.inbound_reject,
            &self.inbound_authenticated_total,
            &self.outbound_attempt,
            &self.outbound_establish,
            &self.outbound_drop,
            &self.outbound_reject,
            &self.errors_read,
            &self.errors_write,
            &self.timeouts_idle,
            &self.timeouts_straggler,
            &self.queue_drop_scp,
            &self.queue_drop_tx,
            &self.queue_drop_advert,
            &self.queue_drop_demand,
            &self.flood_demanded,
            &self.flood_fulfilled,
            &self.flood_unfulfilled_banned,
            &self.flood_unfulfilled_unknown,
            &self.flood_unique_bytes_recv,
            &self.flood_duplicate_bytes_recv,
            &self.flood_broadcast,
            &self.flood_unique_recv,
            &self.flood_duplicate_recv,
            &self.scp_flood_unique_recv,
            &self.scp_flood_duplicate_recv,
            &self.flood_shed_unsynced,
            &self.fetch_unique_bytes_recv,
            &self.fetch_duplicate_bytes_recv,
            &self.item_fetcher_next_peer,
            &self.item_fetcher_tracker_cap_reached,
            &self.fetch_unique_recv,
            &self.fetch_duplicate_recv,
            &self.demand_timeouts,
            &self.pulled_relevant_txs,
            &self.pulled_irrelevant_txs,
            &self.abandoned_demands,
        ]);

        for counter in &self.send_by_type {
            counter.reset();
        }

        for counter in &self.broadcast_fanout_drop_by_type {
            counter.reset();
        }
    }
}

/// Snapshot of overlay metrics at a point in time.
#[derive(Debug, Clone)]
pub struct OverlayMetricsSnapshot {
    // Message metrics
    pub messages_read: u64,
    pub messages_written: u64,
    pub messages_dropped: u64,
    pub messages_broadcast: u64,
    /// Fetch intake channel drops (#3661).
    pub fetch_messages_dropped: u64,
    /// Catchup-cache channel drops (#3661).
    pub catchup_messages_dropped: u64,
    /// Per-message-type outbound broadcast fan-out drops (#3792), indexed by
    /// [`OverlayMessageKind`].
    pub broadcast_fanout_drop_by_type: [u64; OverlayMessageKind::COUNT],
    /// Broadcast calls that reached ZERO peers (`dropped > 0 && sent == 0`) (#3792).
    pub broadcast_blackout: u64,

    // Byte metrics
    pub bytes_read: u64,
    pub bytes_written: u64,

    // Async I/O metrics
    pub async_read: u64,
    pub async_write: u64,

    // Connection lifecycle metrics
    pub inbound_attempt: u64,
    pub inbound_establish: u64,
    pub inbound_drop: u64,
    /// Inbound drops initiated by the remote peer (FIN / RST / recv-error) (#3422).
    pub inbound_drop_remote: u64,
    /// Inbound drops initiated by henyey (timeout / send-error / protocol / shutdown) (#3422).
    pub inbound_drop_local: u64,
    pub inbound_reject: u64,
    /// Monotonic count of inbound peers that reached authenticated state (#3419).
    pub inbound_authenticated_total: u64,
    pub outbound_attempt: u64,
    pub outbound_establish: u64,
    pub outbound_drop: u64,
    pub outbound_reject: u64,

    // Error metrics
    pub errors_read: u64,
    pub errors_write: u64,

    // Timeout metrics
    pub timeouts_idle: u64,
    pub timeouts_straggler: u64,

    // Send counts (indexed by OverlayMessageKind)
    pub send_by_type: [u64; OverlayMessageKind::COUNT],

    // Queue drops
    pub queue_drop_scp: u64,
    pub queue_drop_tx: u64,
    pub queue_drop_advert: u64,
    pub queue_drop_demand: u64,

    // Flood metrics
    pub flood_demanded: u64,
    pub flood_fulfilled: u64,
    pub flood_unfulfilled_banned: u64,
    pub flood_unfulfilled_unknown: u64,
    pub flood_unique_bytes_recv: u64,
    pub flood_duplicate_bytes_recv: u64,
    pub flood_broadcast: u64,
    pub flood_unique_recv: u64,
    pub flood_duplicate_recv: u64,
    pub scp_flood_unique_recv: u64,
    pub scp_flood_duplicate_recv: u64,
    /// Flood messages shed because node was not synced.
    pub flood_shed_unsynced: u64,

    // Fetch metrics
    pub fetch_unique_bytes_recv: u64,
    pub fetch_duplicate_bytes_recv: u64,
    pub item_fetcher_next_peer: u64,
    pub item_fetcher_tracker_cap_reached: u64,
    pub fetch_unique_recv: u64,
    pub fetch_duplicate_recv: u64,

    /// FloodGate known entries (populated by the app layer, not by OverlayMetrics).
    pub flood_known_count: u64,

    // Pull metrics
    pub demand_timeouts: u64,
    pub pulled_relevant_txs: u64,
    pub pulled_irrelevant_txs: u64,
    pub abandoned_demands: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    #[test]
    fn test_counter_basic() {
        let counter = Counter::new();
        assert_eq!(counter.get(), 0);

        counter.inc();
        assert_eq!(counter.get(), 1);

        counter.add(5);
        assert_eq!(counter.get(), 6);

        counter.set(100);
        assert_eq!(counter.get(), 100);

        let prev = counter.reset();
        assert_eq!(prev, 100);
        assert_eq!(counter.get(), 0);
    }

    #[test]
    fn test_counter_concurrent() {
        let counter = Counter::new();
        let counter_ref = &counter;

        thread::scope(|s| {
            for _ in 0..10 {
                s.spawn(|| {
                    for _ in 0..100 {
                        counter_ref.inc();
                    }
                });
            }
        });

        assert_eq!(counter.get(), 1000);
    }

    #[test]
    fn test_overlay_metrics_creation() {
        let metrics = OverlayMetrics::new();

        assert_eq!(metrics.messages_read.get(), 0);
        assert_eq!(metrics.bytes_written.get(), 0);
    }

    #[test]
    fn test_overlay_metrics_increment() {
        let metrics = OverlayMetrics::new();

        metrics.messages_read.inc();
        metrics.messages_read.inc();
        metrics.bytes_read.add(1024);
        metrics.record_send(OverlayMessageKind::Hello);

        assert_eq!(metrics.messages_read.get(), 2);
        assert_eq!(metrics.bytes_read.get(), 1024);
        assert_eq!(
            metrics.send_by_type[OverlayMessageKind::Hello as usize].get(),
            1
        );
    }

    #[test]
    fn test_overlay_metrics_snapshot() {
        let metrics = OverlayMetrics::new();

        metrics.messages_read.add(100);
        metrics.messages_written.add(50);
        metrics.bytes_read.add(10000);
        metrics.errors_read.inc();

        let snapshot = metrics.snapshot();

        assert_eq!(snapshot.messages_read, 100);
        assert_eq!(snapshot.messages_written, 50);
        assert_eq!(snapshot.bytes_read, 10000);
        assert_eq!(snapshot.errors_read, 1);
    }

    #[test]
    fn test_overlay_metrics_reset() {
        let metrics = OverlayMetrics::new();

        metrics.messages_read.add(100);
        metrics.bytes_read.add(10000);

        metrics.reset();

        assert_eq!(metrics.messages_read.get(), 0);
        assert_eq!(metrics.bytes_read.get(), 0);
    }

    #[test]
    fn test_flood_metrics() {
        let metrics = OverlayMetrics::new();

        // Simulate flood operations
        metrics.flood_demanded.add(10);
        metrics.flood_fulfilled.add(8);
        metrics.flood_unfulfilled_unknown.add(2);
        metrics.flood_unique_bytes_recv.add(50000);
        metrics.flood_duplicate_bytes_recv.add(10000);

        let snapshot = metrics.snapshot();

        assert_eq!(snapshot.flood_demanded, 10);
        assert_eq!(snapshot.flood_fulfilled, 8);
        assert_eq!(snapshot.flood_unfulfilled_unknown, 2);
        assert_eq!(snapshot.flood_unique_bytes_recv, 50000);
        assert_eq!(snapshot.flood_duplicate_bytes_recv, 10000);
    }

    #[test]
    fn test_stage_f2_counters_in_snapshot() {
        let metrics = OverlayMetrics::new();

        metrics.flood_broadcast.add(5);
        metrics.flood_unique_recv.inc();
        metrics.flood_unique_recv.inc();
        metrics.flood_duplicate_recv.inc();
        metrics.fetch_unique_recv.add(3);
        metrics.fetch_duplicate_recv.add(7);
        metrics.item_fetcher_next_peer.add(4);
        metrics.item_fetcher_tracker_cap_reached.add(2);

        let snap = metrics.snapshot();
        assert_eq!(snap.flood_broadcast, 5);
        assert_eq!(snap.flood_unique_recv, 2);
        assert_eq!(snap.flood_duplicate_recv, 1);
        assert_eq!(snap.fetch_unique_recv, 3);
        assert_eq!(snap.fetch_duplicate_recv, 7);
        assert_eq!(snap.item_fetcher_next_peer, 4);
        assert_eq!(snap.item_fetcher_tracker_cap_reached, 2);
    }

    #[test]
    fn test_stage_f2_counters_reset() {
        let metrics = OverlayMetrics::new();

        metrics.flood_broadcast.add(10);
        metrics.flood_unique_recv.add(20);
        metrics.flood_duplicate_recv.add(30);
        metrics.fetch_unique_recv.add(40);
        metrics.fetch_duplicate_recv.add(50);
        metrics.item_fetcher_next_peer.add(60);
        metrics.item_fetcher_tracker_cap_reached.add(70);

        metrics.reset();

        let snap = metrics.snapshot();
        assert_eq!(snap.flood_broadcast, 0);
        assert_eq!(snap.flood_unique_recv, 0);
        assert_eq!(snap.flood_duplicate_recv, 0);
        assert_eq!(snap.fetch_unique_recv, 0);
        assert_eq!(snap.fetch_duplicate_recv, 0);
        assert_eq!(snap.item_fetcher_next_peer, 0);
        assert_eq!(snap.item_fetcher_tracker_cap_reached, 0);
    }

    #[test]
    fn test_overlay_message_kind_all_completeness() {
        // ALL must contain exactly COUNT variants, each at its discriminant index.
        assert_eq!(OverlayMessageKind::ALL.len(), OverlayMessageKind::COUNT);
        for (i, kind) in OverlayMessageKind::ALL.iter().enumerate() {
            assert_eq!(*kind as usize, i);
        }
    }

    #[test]
    fn test_overlay_message_kind_from_stellar_message() {
        use stellar_xdr::*;

        // Test representative variants
        let hello = StellarMessage::Hello(Hello::default());
        assert_eq!(
            OverlayMessageKind::from_stellar_message(&hello),
            OverlayMessageKind::Hello
        );

        let peers = StellarMessage::Peers(VecM::default());
        assert_eq!(
            OverlayMessageKind::from_stellar_message(&peers),
            OverlayMessageKind::Peers
        );

        let get_scp_state = StellarMessage::GetScpState(42);
        assert_eq!(
            OverlayMessageKind::from_stellar_message(&get_scp_state),
            OverlayMessageKind::GetScpState
        );

        let send_more = StellarMessage::SendMore(SendMore { num_messages: 10 });
        assert_eq!(
            OverlayMessageKind::from_stellar_message(&send_more),
            OverlayMessageKind::SendMore
        );

        let send_more_ext = StellarMessage::SendMoreExtended(SendMoreExtended {
            num_messages: 10,
            num_bytes: 1000,
        });
        assert_eq!(
            OverlayMessageKind::from_stellar_message(&send_more_ext),
            OverlayMessageKind::SendMoreExtended
        );
    }

    #[test]
    fn test_overlay_message_kind_labels() {
        // Spot-check label format
        assert_eq!(OverlayMessageKind::Hello.label(), "hello");
        assert_eq!(OverlayMessageKind::GetTxSet.label(), "get_tx_set");
        assert_eq!(OverlayMessageKind::FloodAdvert.label(), "flood_advert");
        assert_eq!(
            OverlayMessageKind::TimeSlicedSurveyRequest.label(),
            "time_sliced_survey_request"
        );

        // Wire names (uppercase for logging)
        assert_eq!(OverlayMessageKind::Hello.wire_name(), "HELLO");
        assert_eq!(OverlayMessageKind::GetTxSet.wire_name(), "GET_TX_SET");
    }

    #[test]
    fn test_record_send() {
        let metrics = OverlayMetrics::new();

        metrics.record_send(OverlayMessageKind::Hello);
        metrics.record_send(OverlayMessageKind::Hello);
        metrics.record_send(OverlayMessageKind::Transaction);

        assert_eq!(
            metrics.send_by_type[OverlayMessageKind::Hello as usize].get(),
            2
        );
        assert_eq!(
            metrics.send_by_type[OverlayMessageKind::Transaction as usize].get(),
            1
        );
        assert_eq!(
            metrics.send_by_type[OverlayMessageKind::Auth as usize].get(),
            0
        );
    }

    #[test]
    fn test_send_by_type_in_snapshot() {
        let metrics = OverlayMetrics::new();

        metrics.record_send(OverlayMessageKind::ScpMessage);
        metrics.record_send(OverlayMessageKind::ScpMessage);
        metrics.record_send(OverlayMessageKind::FloodAdvert);

        let snap = metrics.snapshot();
        assert_eq!(
            snap.send_by_type[OverlayMessageKind::ScpMessage as usize],
            2
        );
        assert_eq!(
            snap.send_by_type[OverlayMessageKind::FloodAdvert as usize],
            1
        );
        assert_eq!(snap.send_by_type[OverlayMessageKind::Hello as usize], 0);
    }

    #[test]
    fn test_send_by_type_reset() {
        let metrics = OverlayMetrics::new();

        metrics.record_send(OverlayMessageKind::Hello);
        metrics.record_send(OverlayMessageKind::Transaction);

        metrics.reset();

        for kind in OverlayMessageKind::ALL {
            assert_eq!(metrics.send_by_type[kind as usize].get(), 0);
        }
    }

    #[test]
    fn test_flood_shed_unsynced_counter_in_snapshot_and_reset() {
        let metrics = OverlayMetrics::new();

        metrics.flood_shed_unsynced.add(42);

        let snap = metrics.snapshot();
        assert_eq!(snap.flood_shed_unsynced, 42);

        metrics.reset();
        assert_eq!(metrics.flood_shed_unsynced.get(), 0);
    }

    #[test]
    fn test_inbound_drop_remote_local_split_sums_to_total() {
        // #3422: the two initiator-segmented counters must round-trip through
        // snapshot and (by the connection.rs increment convention) sum to the
        // unconditional `inbound_drop` total. Here we model that convention by
        // incrementing the total alongside each split increment.
        let metrics = OverlayMetrics::new();

        // 3 remote-initiated drops, 2 local-initiated drops.
        for _ in 0..3 {
            metrics.inbound_drop_remote.inc();
            metrics.inbound_drop.inc();
        }
        for _ in 0..2 {
            metrics.inbound_drop_local.inc();
            metrics.inbound_drop.inc();
        }

        let snap = metrics.snapshot();
        assert_eq!(snap.inbound_drop_remote, 3);
        assert_eq!(snap.inbound_drop_local, 2);
        assert_eq!(snap.inbound_drop, 5);
        assert_eq!(
            snap.inbound_drop_remote + snap.inbound_drop_local,
            snap.inbound_drop,
            "remote + local must equal the inbound_drop total"
        );

        metrics.reset();
        let snap = metrics.snapshot();
        assert_eq!(snap.inbound_drop_remote, 0);
        assert_eq!(snap.inbound_drop_local, 0);
        assert_eq!(snap.inbound_drop, 0);
    }
}
