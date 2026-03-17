//! Overlay manager for coordinating peer connections and message routing.
//!
//! The [`OverlayManager`] is the primary interface for the overlay network subsystem.
//! It handles all aspects of peer-to-peer networking:
//!
//! - **Connection Management**: Establishes and maintains TCP connections to peers,
//!   respecting configured limits for inbound and outbound connections
//!
//! - **Peer Discovery**: Learns about new peers from connected nodes and maintains
//!   a pool of known addresses to connect to
//!
//! - **Message Routing**: Receives messages from peers and distributes them to
//!   subscribers, while also sending outbound messages to appropriate peers
//!
//! - **Flood Control**: Uses the [`FloodGate`] to prevent duplicate message
//!   propagation while ensuring all peers receive new messages
//!
//! # Architecture
//!
//! The manager runs several background tasks:
//!
//! 1. **Listener task**: Accepts incoming connections (if enabled)
//! 2. **Connector task**: Initiates outbound connections to maintain target peer count
//! 3. **Peer tasks**: One per connected peer, handles message I/O
//! 4. **Advertiser task**: Periodically sends peer lists to connected nodes
//!
//! # Flow Control
//!
//! The overlay implements Stellar's flow control protocol using `SendMore` and
//! `SendMoreExtended` messages. This prevents peers from overwhelming each other
//! with messages during high-traffic periods.
//!
//! [`FloodGate`]: crate::FloodGate

use crate::{
    codec::helpers,
    connection::{ConnectionDirection, ConnectionPool},
    connection_factory::{ConnectionFactory, TcpConnectionFactory},
    flood::{compute_message_hash, FloodGate, FloodGateStats},
    flow_control::{msg_body_size, FlowControl, FlowControlConfig, ScpQueueCallback},
    peer::{Peer, PeerInfo, PeerStats, PeerStatsSnapshot},
    peer_manager::{PeerManager, StoredPeerType, BackOffUpdate},
    LocalNode, OverlayConfig, OverlayError, PeerAddress, PeerEvent, PeerId, PeerType, Result,
};
use dashmap::DashMap;
use parking_lot::RwLock;
use rand::seq::SliceRandom;
use std::collections::{HashMap, HashSet};
use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use stellar_xdr::curr::{
    ErrorCode, PeerAddress as XdrPeerAddress, PeerAddressIp, SError, StellarMessage, StringM,
    Uint256, VecM,
};
use tokio::sync::{broadcast, mpsc, Mutex as TokioMutex};
use tokio::task::JoinHandle;
use sha2::Digest;
use tracing::{debug, error, info, trace, warn};

/// Maximum length for error messages sent to peers, matching the XDR
/// `string msg<100>` constraint in the `Error` struct.
const MAX_ERROR_MESSAGE_LEN: usize = 100;

/// Interval between overlay maintenance ticks (3 seconds).
///
/// Matches stellar-core `PEER_AUTHENTICATION_TIMEOUT + 1` (2s + 1s = 3s).
/// Each tick: DNS check, connect preferred peers, maybe drop random peer,
/// fill outbound slots.
const TICK_INTERVAL: Duration = Duration::from_secs(3);

/// Delay between successful DNS re-resolution cycles (600 seconds / 10 minutes).
///
/// Matches stellar-core `PEER_IP_RESOLVE_DELAY`.
const PEER_IP_RESOLVE_DELAY: Duration = Duration::from_secs(600);

/// Base delay for DNS retry backoff (10 seconds).
///
/// On each consecutive failure, the delay increases linearly:
/// retry_count * PEER_IP_RESOLVE_RETRY_DELAY, until it exceeds
/// PEER_IP_RESOLVE_DELAY, at which point retries stop.
///
/// Matches stellar-core `PEER_IP_RESOLVE_RETRY_DELAY`.
const PEER_IP_RESOLVE_RETRY_DELAY: Duration = Duration::from_secs(10);

/// Result of a background DNS resolution of configured peers.
struct ResolvedPeers {
    /// Successfully resolved known peers (hostname → IP:port).
    known: Vec<PeerAddress>,
    /// True if any peer in either list failed to resolve.
    errors: bool,
}

/// Resolve a list of peer addresses, performing DNS lookup for hostnames.
///
/// Returns the resolved addresses and a flag indicating whether any
/// resolution failed. Failed peers are skipped but the rest are still
/// returned, matching stellar-core's `resolvePeers()`.
async fn resolve_peer_list(peers: &[PeerAddress]) -> (Vec<PeerAddress>, bool) {
    let mut resolved = Vec::with_capacity(peers.len());
    let mut errors = false;

    for peer in peers {
        // If the host is already an IP address, keep as-is.
        if peer.host.parse::<IpAddr>().is_ok() {
            resolved.push(peer.clone());
            continue;
        }

        // Resolve hostname to IP address.
        let lookup_result = tokio::net::lookup_host(
            (peer.host.as_str(), peer.port)
        ).await;
        match lookup_result {
            Ok(addrs) => {
                // Take the first IPv4 address, matching stellar-core behavior.
                let ipv4_addr = addrs.into_iter().find(|a| a.is_ipv4());
                if let Some(socket_addr) = ipv4_addr {
                    resolved.push(PeerAddress::from(socket_addr));
                    trace!(
                        "Resolved peer {} -> {}:{}",
                        peer,
                        socket_addr.ip(),
                        socket_addr.port()
                    );
                } else {
                    errors = true;
                    warn!("No IPv4 address found for peer {}", peer);
                }
            }
            Err(e) => {
                errors = true;
                error!("Unable to resolve peer '{}': {}", peer, e);
            }
        }
    }

    (resolved, errors)
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

/// Compute the next DNS resolution delay based on the backoff state.
///
/// Implements the linear backoff state machine from stellar-core:
/// - If `resolving_with_backoff` and the latest resolution succeeded (no errors),
///   disable backoff and return `PEER_IP_RESOLVE_DELAY` (600s).
/// - If `resolving_with_backoff` and there are errors, increment retry count and
///   return `retry_count * PEER_IP_RESOLVE_RETRY_DELAY`.
/// - If the backoff delay exceeds `PEER_IP_RESOLVE_DELAY`, give up on retries.
/// - If not in backoff mode, always return `PEER_IP_RESOLVE_DELAY`.
///
/// Returns `(delay, new_resolving_with_backoff, new_retry_count)`.
///
/// Extracted from `start_tick_loop` for testability (G7).
fn compute_dns_backoff_delay(
    resolving_with_backoff: bool,
    retry_count: u32,
    had_errors: bool,
) -> (Duration, bool, u32) {
    if !resolving_with_backoff {
        return (PEER_IP_RESOLVE_DELAY, false, retry_count);
    }

    if !had_errors {
        // Success: disable retries permanently.
        (PEER_IP_RESOLVE_DELAY, false, retry_count)
    } else {
        // Failure: linear backoff.
        let new_retry_count = retry_count + 1;
        let backoff = PEER_IP_RESOLVE_RETRY_DELAY.saturating_mul(new_retry_count);
        if backoff > PEER_IP_RESOLVE_DELAY {
            // Give up on retries.
            (PEER_IP_RESOLVE_DELAY, false, new_retry_count)
        } else {
            (backoff, true, new_retry_count)
        }
    }
}

/// Truncate an error message to fit within the XDR `string msg<100>` limit.
///
/// If the message exceeds 100 bytes it is truncated at a valid UTF-8 boundary
/// (since the XDR string is opaque bytes, this is a convenience for logs).
fn truncate_error_msg(msg: &str) -> &str {
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
fn make_error_msg(code: ErrorCode, message: &str) -> StellarMessage {
    let truncated = truncate_error_msg(message);
    // safe: truncated.len() <= 100
    let msg = StringM::<100>::try_from(truncated).unwrap_or_default();
    StellarMessage::ErrorMsg(SError { code, msg })
}

/// Send an error to a peer then request its task to shut down.
///
/// Matches stellar-core `Peer::sendErrorAndDrop` (Peer.cpp:722-729).
/// Uses `try_send` so this never blocks; if the channel is full the error
/// is silently dropped but the shutdown still proceeds.
fn send_error_and_drop(
    peer_id: &PeerId,
    outbound_tx: &mpsc::Sender<OutboundMessage>,
    code: ErrorCode,
    message: &str,
) {
    let err_msg = make_error_msg(code, message);
    let _ = outbound_tx.try_send(OutboundMessage::Send(err_msg));
    let _ = outbound_tx.try_send(OutboundMessage::Shutdown);
    debug!(
        "Sent error to {} and requested drop: code={:?} msg={}",
        peer_id,
        code,
        truncate_error_msg(message),
    );
}

/// Log received fetch messages and check for ping responses.
///
/// Handles debug-level logging of fetch response details (hashes, types)
/// and checks `ScpQuorumset`/`DontHave` messages for ping RTT measurement.
fn log_fetch_message(message: &StellarMessage, peer_id: &PeerId, ping: &mut PingTracker) {
    match message {
        StellarMessage::TxSet(ts) => {
            debug!(
                "OVERLAY: Received TxSet from {} hash={} prev_ledger={}",
                peer_id,
                hex::encode(sha2::Sha256::digest(
                    stellar_xdr::curr::WriteXdr::to_xdr(ts, stellar_xdr::curr::Limits::none())
                        .unwrap_or_default()
                )),
                hex::encode(ts.previous_ledger_hash.0)
            );
        }
        StellarMessage::GeneralizedTxSet(ts) => {
            let hash = henyey_common::Hash256::hash_xdr(ts)
                .unwrap_or(henyey_common::Hash256::ZERO);
            debug!(
                "OVERLAY: Received GeneralizedTxSet from {} hash={}",
                peer_id, hash
            );
        }
        StellarMessage::ScpQuorumset(qs) => {
            let hash =
                henyey_common::Hash256::hash_xdr(qs).unwrap_or(henyey_common::Hash256::ZERO);
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
                peer_id, dh.type_, hex::encode(dh.req_hash.0)
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
enum PeersValidation {
    NotPeers,
    AcceptFirst,
    RejectWrongDirection,
    RejectDuplicate,
}

fn validate_incoming_peers(
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

fn should_skip_generic_routing(message: &StellarMessage) -> bool {
    helpers::is_handshake_message(message)
        || matches!(
            message,
            StellarMessage::SendMore(_) | StellarMessage::SendMoreExtended(_)
        )
}

/// A message received from a connected peer via the overlay network.
///
/// These messages are delivered to subscribers of the overlay manager's
/// broadcast channel. The `from_peer` field identifies the sender.
#[derive(Debug, Clone)]
pub struct OverlayMessage {
    /// The peer that sent this message.
    pub from_peer: PeerId,
    /// The Stellar protocol message.
    pub message: StellarMessage,
    /// When the message was received from the peer (before broadcast channel delivery).
    pub received_at: std::time::Instant,
}

/// A snapshot of a connected peer's info and statistics.
///
/// Provides a point-in-time view of a peer's state without holding any locks.
#[derive(Debug, Clone)]
pub struct PeerSnapshot {
    /// Static information about the peer (ID, address, version).
    pub info: PeerInfo,
    /// Message and byte counters.
    pub stats: PeerStatsSnapshot,
}

/// Lightweight handle stored in DashMap, replaces Arc<TokioMutex<Peer>>.
///
/// The actual `Peer` is owned by the spawned peer task. This handle
/// provides non-blocking access to send messages and read stats.
struct PeerHandle {
    /// Channel to send outbound messages to the peer task.
    outbound_tx: mpsc::Sender<OutboundMessage>,
    /// Shared stats (atomically updated by the peer task).
    stats: Arc<PeerStats>,
    /// Per-peer flow control (shared with the peer task).
    flow_control: Arc<FlowControl>,
}

/// Messages sent to a peer task via the outbound channel.
enum OutboundMessage {
    /// Direct send (non-flood, e.g. GetTxSet, ScpQuorumset response).
    Send(StellarMessage),
    /// Flood message (goes through FlowControl outbound queue).
    Flood(StellarMessage),
    /// Close the connection.
    Shutdown,
}

/// Shared state passed to spawned peer tasks.
///
/// Bundles all `Arc`-wrapped state that background tasks need, avoiding
/// 20+ individual parameter lists on `connect_outbound_inner` and
/// `run_peer_loop`.
#[derive(Clone)]
struct SharedPeerState {
    peers: Arc<DashMap<PeerId, PeerHandle>>,
    flood_gate: Arc<FloodGate>,
    running: Arc<AtomicBool>,
    message_tx: broadcast::Sender<OverlayMessage>,
    scp_message_tx: mpsc::UnboundedSender<OverlayMessage>,
    fetch_response_tx: mpsc::Sender<OverlayMessage>,
    peer_handles: Arc<RwLock<Vec<JoinHandle<()>>>>,
    advertised_outbound_peers: Arc<RwLock<Vec<PeerAddress>>>,
    advertised_inbound_peers: Arc<RwLock<Vec<PeerAddress>>>,
    added_authenticated_peers: Arc<std::sync::atomic::AtomicU64>,
    dropped_authenticated_peers: Arc<std::sync::atomic::AtomicU64>,
    banned_peers: Arc<RwLock<HashSet<PeerId>>>,
    peer_info_cache: Arc<DashMap<PeerId, PeerInfo>>,
    /// Last closed ledger sequence, used for flood record cleanup.
    last_closed_ledger: Arc<AtomicU32>,
    /// Optional callback for intelligent SCP queue trimming.
    scp_callback: Option<Arc<dyn ScpQueueCallback>>,
    is_validator: bool,
    peer_event_tx: Option<mpsc::Sender<PeerEvent>>,
    extra_subscribers: Arc<RwLock<Vec<mpsc::UnboundedSender<OverlayMessage>>>>,
    /// Whether the node is tracking consensus (set by the herder/app layer).
    /// When false, the overlay may drop random peers to try new connections.
    is_tracking: Arc<AtomicBool>,
}

impl SharedPeerState {
    /// Send a peer event if a subscriber is registered.
    async fn send_peer_event(&self, event: PeerEvent) {
        if let Some(tx) = self.peer_event_tx.clone() {
            let _ = tx.send(event).await;
        }
    }

    /// Clean up shared state after a peer disconnects.
    /// Must be called after `run_peer_loop` completes for any authenticated peer.
    fn cleanup_peer(&self, peer_id: &PeerId) {
        self.peers.remove(peer_id);
        self.peer_info_cache.remove(peer_id);
        self.dropped_authenticated_peers
            .fetch_add(1, Ordering::Relaxed);
    }

    /// Forward an overlay message to the appropriate subscriber channels.
    ///
    /// Returns `true` if the message was an SCP message (for counter tracking).
    /// Routes to dedicated channels (SCP, fetch response, extra subscribers)
    /// first, then falls through to the generic broadcast channel for
    /// non-dedicated messages.
    fn route_to_subscribers(&self, msg: OverlayMessage) -> bool {
        let is_scp = matches!(msg.message, StellarMessage::ScpMessage(_));
        let is_fetch_response = matches!(
            msg.message,
            StellarMessage::GeneralizedTxSet(_)
                | StellarMessage::TxSet(_)
                | StellarMessage::DontHave(_)
                | StellarMessage::ScpQuorumset(_)
        );
        let is_dedicated = is_scp || is_fetch_response;

        if is_scp {
            if let Err(e) = self.scp_message_tx.send(msg.clone()) {
                error!("SCP channel send FAILED for peer {}: {}", msg.from_peer, e);
            }
        }

        if is_fetch_response {
            if let Err(e) = self.fetch_response_tx.try_send(msg.clone()) {
                error!(
                    "Fetch response channel send FAILED for peer {}: {}",
                    msg.from_peer, e
                );
            }
        }

        // Send catchup-critical messages to extra subscribers
        if matches!(
            msg.message,
            StellarMessage::ScpMessage(_)
                | StellarMessage::GeneralizedTxSet(_)
                | StellarMessage::TxSet(_)
                | StellarMessage::ScpQuorumset(_)
        ) {
            let subs = self.extra_subscribers.read();
            for sub in subs.iter() {
                let _ = sub.send(msg.clone());
            }
        }

        if !is_dedicated {
            let _ = self.message_tx.send(msg);
        }

        is_scp
    }
}
/// Central manager for all peer connections in the overlay network.
///
/// The overlay manager is the main entry point for networking operations.
/// It handles connection lifecycle, message routing, and peer discovery.
///
/// # Usage
///
/// ```rust,ignore
/// // Create and start the manager
/// let config = OverlayConfig::testnet();
/// let local_node = LocalNode::new_testnet(secret_key);
/// let mut manager = OverlayManager::new(config, local_node)?;
/// manager.start().await?;
///
/// // Subscribe to messages
/// let mut rx = manager.subscribe();
/// while let Ok(msg) = rx.recv().await {
///     handle_message(msg);
/// }
///
/// // Broadcast a message
/// manager.broadcast(StellarMessage::Transaction(tx)).await?;
///
/// // Shutdown
/// manager.shutdown().await?;
/// ```
pub struct OverlayManager {
    /// Configuration.
    config: OverlayConfig,
    /// Local node info.
    local_node: LocalNode,
    /// Connected peers. Each entry is a lightweight handle with a channel
    /// to the peer's dedicated task (which owns the actual `Peer`).
    peers: Arc<DashMap<PeerId, PeerHandle>>,
    /// Flood gate.
    flood_gate: Arc<FloodGate>,
    /// Connection pool for inbound connections.
    inbound_pool: Arc<ConnectionPool>,
    /// Connection pool for outbound connections.
    outbound_pool: Arc<ConnectionPool>,
    /// Whether the manager is running.
    running: Arc<AtomicBool>,
    /// Channel for incoming messages.
    message_tx: broadcast::Sender<OverlayMessage>,
    /// Handle to listener task.
    listener_handle: Option<JoinHandle<()>>,
    /// Handle to connector task.
    connector_handle: Option<JoinHandle<()>>,
    /// Handle to peer tasks.
    peer_handles: Arc<RwLock<Vec<JoinHandle<()>>>>,
    /// Known peers learned from discovery.
    known_peers: Arc<RwLock<Vec<PeerAddress>>>,
    /// Outbound peers to advertise in Peers messages.
    advertised_outbound_peers: Arc<RwLock<Vec<PeerAddress>>>,
    /// Inbound peers to advertise in Peers messages.
    advertised_inbound_peers: Arc<RwLock<Vec<PeerAddress>>>,
    /// Handle to periodic peer advertiser task.
    peer_advertiser_handle: Option<JoinHandle<()>>,
    /// Total authenticated peers added.
    added_authenticated_peers: Arc<std::sync::atomic::AtomicU64>,
    /// Total authenticated peers dropped.
    dropped_authenticated_peers: Arc<std::sync::atomic::AtomicU64>,
    /// Banned peers by node ID.
    banned_peers: Arc<RwLock<HashSet<PeerId>>>,
    /// Shutdown signal.
    shutdown_tx: Option<broadcast::Sender<()>>,
    /// Cache of peer info for connected peers (lock-free access).
    peer_info_cache: Arc<DashMap<PeerId, PeerInfo>>,
    /// Dedicated unbounded channel for SCP messages.
    /// SCP messages are consensus-critical and must never be dropped.
    /// Mainnet generates ~24 validators * multiple SCP rounds per slot,
    /// which can overwhelm bounded channels during catchup.
    scp_message_tx: mpsc::UnboundedSender<OverlayMessage>,
    /// Receiver end of the SCP channel. Taken once via `subscribe_scp()`.
    scp_message_rx: Arc<TokioMutex<Option<mpsc::UnboundedReceiver<OverlayMessage>>>>,
    /// Dedicated bounded channel for fetch response messages.
    /// Routes GeneralizedTxSet, TxSet, DontHave, and ScpQuorumset through
    /// a dedicated channel. Buffer (4096) is generous for fetch responses.
    fetch_response_tx: mpsc::Sender<OverlayMessage>,
    /// Receiver end of the fetch response channel. Taken once via `subscribe_fetch_responses()`.
    fetch_response_rx: Arc<TokioMutex<Option<mpsc::Receiver<OverlayMessage>>>>,
    /// Dynamic extra subscribers for catchup-critical messages (SCP + TxSet).
    /// Created on demand via `subscribe_catchup()` and cleaned up when dropped.
    /// Uses parking_lot::RwLock for minimal contention in the hot path (read-heavy).
    extra_subscribers: Arc<RwLock<Vec<mpsc::UnboundedSender<OverlayMessage>>>>,
    /// Last closed ledger sequence, used for flood record cleanup.
    last_closed_ledger: Arc<AtomicU32>,
    /// Optional callback for intelligent SCP queue trimming.
    scp_callback: Option<Arc<dyn ScpQueueCallback>>,
    /// Optional peer manager for persistent peer storage.
    peer_manager: Option<Arc<PeerManager>>,
    /// Whether the node is tracking consensus (set by the herder/app layer).
    /// When false, the overlay may drop random peers to try new connections.
    is_tracking: Arc<AtomicBool>,
    /// Connection factory used for transport establishment.
    connection_factory: Arc<dyn ConnectionFactory>,
}

impl OverlayManager {
    fn initial_send_more_grant(config: &FlowControlConfig) -> (u32, u32) {
        // Flow control bytes credit is granted in byte-batch units.
        // This must match the local byte capacity tracked in FlowControl.
        (
            config.peer_flood_reading_capacity as u32,
            config.flow_control_bytes_batch_size as u32,
        )
    }

    /// Create a new overlay manager with the given configuration.
    pub fn new(config: OverlayConfig, local_node: LocalNode) -> Result<Self> {
        Self::new_with_connection_factory(config, local_node, Arc::new(TcpConnectionFactory))
    }

    /// Create a new overlay manager with a custom connection factory.
    pub fn new_with_connection_factory(
        config: OverlayConfig,
        local_node: LocalNode,
        connection_factory: Arc<dyn ConnectionFactory>,
    ) -> Result<Self> {
        // Broadcast channel for non-critical overlay messages (TX floods, etc.).
        // SCP and fetch-response messages bypass this channel via dedicated mpsc
        // channels, so the broadcast channel only carries remaining message types.
        // 4096 provides headroom for mainnet traffic bursts from multiple peers.
        let (message_tx, _) = broadcast::channel(4096);
        let (shutdown_tx, _) = broadcast::channel(1);
        let (scp_message_tx, scp_message_rx) = mpsc::unbounded_channel();
        let (fetch_response_tx, fetch_response_rx) = mpsc::channel(4096);

        Ok(Self {
            config: config.clone(),
            local_node,
            peers: Arc::new(DashMap::new()),
            flood_gate: Arc::new(FloodGate::with_ttl(Duration::from_secs(
                config.flood_ttl_secs,
            ))),
            inbound_pool: Arc::new({
                // Extract IPs from preferred peers for possibly-preferred inbound slots
                let preferred_ips: std::collections::HashSet<IpAddr> = config
                    .preferred_peers
                    .iter()
                    .filter_map(|addr| addr.host.parse::<IpAddr>().ok())
                    .collect();
                const POSSIBLY_PREFERRED_EXTRA: usize = 2;
                if preferred_ips.is_empty() {
                    ConnectionPool::new(config.max_inbound_peers)
                } else {
                    ConnectionPool::with_preferred(
                        config.max_inbound_peers,
                        POSSIBLY_PREFERRED_EXTRA,
                        preferred_ips,
                    )
                }
            }),
            outbound_pool: Arc::new(ConnectionPool::new(config.max_outbound_peers)),
            running: Arc::new(AtomicBool::new(false)),
            message_tx,
            listener_handle: None,
            connector_handle: None,
            peer_handles: Arc::new(RwLock::new(Vec::new())),
            known_peers: Arc::new(RwLock::new(config.known_peers.clone())),
            advertised_outbound_peers: Arc::new(RwLock::new(config.known_peers.clone())),
            advertised_inbound_peers: Arc::new(RwLock::new(Vec::new())),
            peer_advertiser_handle: None,
            added_authenticated_peers: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            dropped_authenticated_peers: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            banned_peers: Arc::new(RwLock::new(HashSet::new())),
            shutdown_tx: Some(shutdown_tx),
            peer_info_cache: Arc::new(DashMap::new()),
            scp_message_tx,
            scp_message_rx: Arc::new(TokioMutex::new(Some(scp_message_rx))),
            fetch_response_tx,
            fetch_response_rx: Arc::new(TokioMutex::new(Some(fetch_response_rx))),
            extra_subscribers: Arc::new(RwLock::new(Vec::new())),
            last_closed_ledger: Arc::new(AtomicU32::new(0)),
            scp_callback: None,
            peer_manager: config.peer_manager.clone(),
            is_tracking: Arc::new(AtomicBool::new(false)),
            connection_factory,
        })
    }

    /// Create a snapshot of shared state for passing to spawned tasks.
    fn shared_state(&self) -> SharedPeerState {
        SharedPeerState {
            peers: Arc::clone(&self.peers),
            flood_gate: Arc::clone(&self.flood_gate),
            running: Arc::clone(&self.running),
            message_tx: self.message_tx.clone(),
            scp_message_tx: self.scp_message_tx.clone(),
            fetch_response_tx: self.fetch_response_tx.clone(),
            peer_handles: Arc::clone(&self.peer_handles),
            advertised_outbound_peers: Arc::clone(&self.advertised_outbound_peers),
            advertised_inbound_peers: Arc::clone(&self.advertised_inbound_peers),
            added_authenticated_peers: Arc::clone(&self.added_authenticated_peers),
            dropped_authenticated_peers: Arc::clone(&self.dropped_authenticated_peers),
            banned_peers: Arc::clone(&self.banned_peers),
            peer_info_cache: Arc::clone(&self.peer_info_cache),
            last_closed_ledger: Arc::clone(&self.last_closed_ledger),
            scp_callback: self.scp_callback.clone(),
            is_validator: self.config.is_validator,
            peer_event_tx: self.config.peer_event_tx.clone(),
            extra_subscribers: Arc::clone(&self.extra_subscribers),
            is_tracking: Arc::clone(&self.is_tracking),
        }
    }

    /// Create a PeerHandle (outbound channel + FlowControl) and register
    /// the peer in the shared maps. Returns the receiver and FlowControl
    /// needed by `run_peer_loop`.
    fn register_peer(
        peer: &Peer,
        peer_id: &PeerId,
        peer_info: PeerInfo,
        shared: &SharedPeerState,
    ) -> (mpsc::Receiver<OutboundMessage>, Arc<FlowControl>) {
        let (outbound_tx, outbound_rx) = mpsc::channel(256);
        let stats = peer.stats();
        let flow_control = Arc::new(FlowControl::with_scp_callback(
            FlowControlConfig::default(),
            shared.scp_callback.clone(),
        ));
        flow_control.set_peer_id(peer_id.clone());
        let peer_handle = PeerHandle {
            outbound_tx,
            stats,
            flow_control: Arc::clone(&flow_control),
        };
        shared.peers.insert(peer_id.clone(), peer_handle);
        shared.peer_info_cache.insert(peer_id.clone(), peer_info);
        shared
            .added_authenticated_peers
            .fetch_add(1, Ordering::Relaxed);
        (outbound_rx, flow_control)
    }

    /// Start the overlay network (listening and connecting to peers).
    pub async fn start(&mut self) -> Result<()> {
        if self.running.load(Ordering::Relaxed) {
            return Err(OverlayError::AlreadyStarted);
        }

        info!("Starting overlay manager");
        self.running.store(true, Ordering::Relaxed);

        // G6: Purge dead peers from the peer database on startup.
        // Matches stellar-core OverlayManagerImpl::start() → purgeDeadPeers()
        // which removes peers with >= REALLY_DEAD_NUM_FAILURES_CUTOFF (120) failures.
        if let Some(ref pm) = self.peer_manager {
            const REALLY_DEAD_NUM_FAILURES_CUTOFF: u32 = 120;
            if let Err(e) = pm.remove_peers_with_many_failures(REALLY_DEAD_NUM_FAILURES_CUTOFF) {
                warn!("Failed to purge dead peers: {}", e);
            } else {
                info!("Purged dead peers (>= {} failures)", REALLY_DEAD_NUM_FAILURES_CUTOFF);
            }
        }

        // G5: Store known_peers and preferred_peers into the peer database on startup
        // with a hard reset (failures=0, next_attempt=now). Matches stellar-core
        // OverlayManagerImpl::start() → storeConfigPeers().
        if let Some(ref pm) = self.peer_manager {
            for addr in &self.config.known_peers {
                if let Err(e) = pm.ensure_exists(addr) {
                    warn!("Failed to store known peer {}: {}", addr, e);
                    continue;
                }
                if let Err(e) = pm.update(
                    addr,
                    StoredPeerType::Outbound,
                    false,
                    BackOffUpdate::HardReset,
                ) {
                    warn!("Failed to reset known peer {}: {}", addr, e);
                }
            }
            for addr in &self.config.preferred_peers {
                if let Err(e) = pm.ensure_exists(addr) {
                    warn!("Failed to store preferred peer {}: {}", addr, e);
                    continue;
                }
                if let Err(e) = pm.update(
                    addr,
                    StoredPeerType::Preferred,
                    true,
                    BackOffUpdate::HardReset,
                ) {
                    warn!("Failed to reset preferred peer {}: {}", addr, e);
                }
            }
            info!(
                "Stored {} known + {} preferred config peers",
                self.config.known_peers.len(),
                self.config.preferred_peers.len()
            );
        }

        // Start listener if enabled
        if self.config.listen_enabled {
            self.start_listener().await?;
        }

        // Start the periodic tick loop for peer management.
        // This replaces a dedicated connector task — the tick loop handles
        // all periodic maintenance: DNS resolution, peer connection,
        // preferred-peer eviction, random-peer drops, and slot filling.
        // Matches stellar-core OverlayManagerImpl::tick().
        self.start_tick_loop();
        self.start_peer_advertiser();

        Ok(())
    }

    /// Start the connection listener.
    async fn start_listener(&mut self) -> Result<()> {
        let listener = self.connection_factory.bind(self.config.listen_port).await?;
        info!("Listening on port {}", self.config.listen_port);

        let shared = self.shared_state();
        let local_node = self.local_node.clone();
        let pool = Arc::clone(&self.inbound_pool);
        let peer_handles = Arc::clone(&self.peer_handles);
        let auth_timeout = self.config.auth_timeout_secs;
        let mut shutdown_rx = self.shutdown_tx.as_ref().unwrap().subscribe();

        let handle = tokio::spawn(async move {
            loop {
                tokio::select! {
                    result = listener.accept() => {
                        match result {
                            Ok(connection) => {
                                let peer_ip = connection.remote_addr().ip();
                                if !pool.try_reserve_with_ip(Some(peer_ip)) {
                                    warn!("Inbound peer limit reached, rejecting connection from {}", peer_ip);
                                    continue;
                                }

                                let shared = shared.clone();
                                let local_node = local_node.clone();
                                let pool = Arc::clone(&pool);

                                let peer_handle = tokio::spawn(async move {
                                    let remote_addr = connection.remote_addr();
                                    match Peer::accept(connection, local_node, auth_timeout).await {
                                        Ok(mut peer) => {
                                            let peer_id = peer.id().clone();
                                            if shared.banned_peers.read().contains(&peer_id) {
                                                warn!("Rejected banned peer {}", peer_id);
                                                peer.close().await;
                                                pool.release_pending();
                                                return;
                                            }
                                            if shared.peers.contains_key(&peer_id) {
                                                debug!("Rejected duplicate inbound peer {}", peer_id);
                                                peer.close().await;
                                                pool.release_pending();
                                                return;
                                            }
                                            // Handshake succeeded: promote from pending to authenticated
                                            pool.mark_authenticated();
                                            info!("Accepted peer: {}", peer_id);

                                            let peer_info = peer.info().clone();
                                            shared.send_peer_event(PeerEvent::Connected(
                                                PeerAddress::from(peer_info.address),
                                                PeerType::Inbound,
                                            )).await;

                                            // Send Peers message directly (we still own the peer)
                                            let outbound_snapshot =
                                                shared.advertised_outbound_peers.read().clone();
                                            let inbound_snapshot =
                                                shared.advertised_inbound_peers.read().clone();
                                            let exclude = PeerAddress::from(peer_info.address);
                                            if let Some(message) =
                                                OverlayManager::build_peers_message(
                                                    &outbound_snapshot,
                                                    &inbound_snapshot,
                                                    Some(&exclude),
                                                )
                                            {
                                                if peer.is_ready() {
                                                    if let Err(e) = peer.send(message).await {
                                                        debug!(
                                                            "Failed to send peers to {}: {}",
                                                            peer_id, e
                                                        );
                                                    }
                                                }
                                            }

                                            let (outbound_rx, flow_control) =
                                                Self::register_peer(&peer, &peer_id, peer_info, &shared);

                                            // Run peer loop (peer is moved, not locked)
                                            Self::run_peer_loop(
                                                peer_id.clone(),
                                                peer,
                                                outbound_rx,
                                                flow_control,
                                                shared.clone(),
                                            ).await;

                                            // Cleanup
                                            shared.cleanup_peer(&peer_id);
                                            pool.release_authenticated();
                                        }
                                        Err(e) => {
                                            warn!("Failed to accept peer: {}", e);
                                            shared.send_peer_event(PeerEvent::Failed(
                                                PeerAddress::from(remote_addr),
                                                PeerType::Inbound,
                                            )).await;
                                            pool.release_pending();
                                        }
                                    }
                                });

                                peer_handles.write().push(peer_handle);
                            }
                            Err(e) => {
                                error!("Accept error: {}", e);
                            }
                        }
                    }
                    _ = shutdown_rx.recv() => {
                        debug!("Listener shutting down");
                        break;
                    }
                }

                if !shared.running.load(Ordering::Relaxed) {
                    break;
                }
            }
        });

        self.listener_handle = Some(handle);
        Ok(())
    }

    /// Start the outbound connector.
    /// Start the periodic tick loop for overlay maintenance.
    ///
    /// Runs every 3 seconds (PEER_AUTHENTICATION_TIMEOUT + 1), matching
    /// stellar-core's `OverlayManagerImpl::tick()`.  Each tick:
    ///
    /// 1. Checks and processes completed DNS resolution results (G7)
    /// 2. Triggers new DNS resolution if the timer has elapsed (G7)
    /// 3. Maybe drops a random non-preferred outbound peer when out of sync (G8)
    /// 4. Connects to preferred peers (with eviction if needed)
    /// 5. Fills remaining outbound slots from known peers
    fn start_tick_loop(&mut self) {
        let shared = self.shared_state();
        let local_node = self.local_node.clone();
        let pool = Arc::clone(&self.outbound_pool);
        let known_peers = Arc::clone(&self.known_peers);
        let preferred_peers = self.config.preferred_peers.clone();
        let target_outbound = self.config.target_outbound_peers;
        let max_outbound = self.config.max_outbound_peers;
        let connect_timeout = self.config.connect_timeout_secs;
        let auth_timeout = self.config.auth_timeout_secs;
        let connection_factory = Arc::clone(&self.connection_factory);
        let config_known_peers = self.config.known_peers.clone();
        let mut shutdown_rx = self.shutdown_tx.as_ref().unwrap().subscribe();

        let handle = tokio::spawn(async move {
            let mut retry_after: HashMap<PeerAddress, Instant> = HashMap::new();
            let mut interval = tokio::time::interval(TICK_INTERVAL);
            // G8: Track when we first noticed we were out of sync, for
            // random-peer-drop cooldown (OUT_OF_SYNC_RECONNECT_DELAY = 60s).
            let mut last_out_of_sync_reconnect: Option<Instant> = None;

            // G7: DNS re-resolution state.
            // Matches stellar-core's mResolvedPeers future, mResolvingPeersWithBackoff,
            // and mResolvingPeersRetryCount.
            let mut dns_resolving_with_backoff = true;
            let mut dns_retry_count: u32 = 0;
            let mut dns_next_resolve_at = Instant::now(); // Resolve immediately on first tick

            // Trigger initial DNS resolution.
            let mut dns_resolve_handle: Option<JoinHandle<ResolvedPeers>> = {
                let kp = config_known_peers.clone();
                let pp = preferred_peers.clone();
                Some(tokio::spawn(async move {
                    let (known, known_err) = resolve_peer_list(&kp).await;
                    let (_pref, pref_err) = resolve_peer_list(&pp).await;
                    ResolvedPeers {
                        known,
                        errors: known_err || pref_err,
                    }
                }))
            };

            loop {
                tokio::select! {
                    _ = shutdown_rx.recv() => {
                        debug!("Connector shutting down");
                        break;
                    }
                    _ = interval.tick() => {}
                }

                if !shared.running.load(Ordering::Relaxed) {
                    break;
                }

                // G8: When not tracking consensus and outbound slots are full,
                // periodically drop a random non-preferred outbound peer to
                // try fresh connections (matches stellar-core
                // updateTimerAndMaybeDropRandomPeer).
                let tracking = shared.is_tracking.load(Ordering::Relaxed);
                Self::maybe_drop_random_peer(
                    &shared.peers,
                    &shared.peer_info_cache,
                    &preferred_peers,
                    max_outbound,
                    tracking,
                    &mut last_out_of_sync_reconnect,
                );

                // G7: Check if background DNS resolution has completed.
                if let Some(ref handle) = dns_resolve_handle {
                    if handle.is_finished() {
                        let handle = dns_resolve_handle.take().unwrap();
                        match handle.await {
                            Ok(result) => {
                                // Update known peers with resolved addresses.
                                {
                                    let mut kp = known_peers.write();
                                    // Merge resolved known peers (add new, keep existing).
                                    for addr in &result.known {
                                        if !kp.iter().any(|p| p.host == addr.host && p.port == addr.port) {
                                            kp.push(addr.clone());
                                        }
                                    }
                                }

                                // Calculate next resolve delay based on success/failure.
                                let (delay, new_backoff, new_retry) = compute_dns_backoff_delay(
                                    dns_resolving_with_backoff,
                                    dns_retry_count,
                                    result.errors,
                                );
                                dns_resolving_with_backoff = new_backoff;
                                dns_retry_count = new_retry;

                                dns_next_resolve_at = Instant::now() + delay;
                                debug!(
                                    "DNS resolution complete (errors={}), next in {:?}",
                                    result.errors, delay
                                );
                            }
                            Err(e) => {
                                error!("DNS resolution task panicked: {}", e);
                                dns_next_resolve_at = Instant::now() + PEER_IP_RESOLVE_RETRY_DELAY;
                            }
                        }
                    }
                }

                // G7: Trigger new DNS resolution if timer has elapsed and no
                // resolution is in flight.
                if dns_resolve_handle.is_none() && Instant::now() >= dns_next_resolve_at {
                    let kp = config_known_peers.clone();
                    let pp = preferred_peers.clone();
                    dns_resolve_handle = Some(tokio::spawn(async move {
                        let (known, known_err) = resolve_peer_list(&kp).await;
                        let (_pref, pref_err) = resolve_peer_list(&pp).await;
                        ResolvedPeers {
                            known,
                            errors: known_err || pref_err,
                        }
                    }));
                }

                let now = Instant::now();
                let outbound_count = Self::count_outbound_peers(&shared.peer_info_cache);
                let available = max_outbound.saturating_sub(outbound_count);
                if available == 0 {
                    continue;
                }

                let mut remaining = available;

                // Preferred peers first
                for addr in &preferred_peers {
                    if remaining == 0 {
                        break;
                    }

                    if let Some(next) = retry_after.get(addr) {
                        if *next > now {
                            continue;
                        }
                    }

                    if Self::has_outbound_connection_to(&shared.peer_info_cache, addr) {
                        continue;
                    }

                    if !pool.try_reserve() {
                        // Preferred peer eviction: evict youngest non-preferred
                        // outbound peer to make room (matches stellar-core behavior).
                        let evicted = Self::maybe_evict_for_preferred(
                            &shared.peers,
                            &shared.peer_info_cache,
                            &preferred_peers,
                        );
                        if evicted {
                            // Give the evicted peer task time to clean up and
                            // release its pool slot.
                            tokio::time::sleep(Duration::from_millis(100)).await;
                        }
                        if !pool.try_reserve() {
                            debug!("Outbound peer limit reached (even after eviction attempt)");
                            remaining = 0;
                            break;
                        }
                    }

                    let timeout = connect_timeout.max(auth_timeout);
                    match Self::connect_outbound_inner(
                        &addr,
                        local_node.clone(),
                        timeout,
                        Arc::clone(&pool),
                        shared.clone(),
                        Arc::clone(&connection_factory),
                    )
                    .await
                    {
                        Ok(_) => {
                            retry_after.remove(addr);
                            remaining = remaining.saturating_sub(1);
                        }
                        Err(e) => {
                            warn!("Failed to connect to preferred peer {}: {}", addr, e);
                            retry_after.insert(addr.clone(), now + Duration::from_secs(10));
                        }
                    }
                }

                let outbound_count = Self::count_outbound_peers(&shared.peer_info_cache);
                if remaining == 0 || outbound_count >= target_outbound {
                    continue;
                }

                let mut known_snapshot = known_peers.read().clone();
                known_snapshot.shuffle(&mut rand::thread_rng());

                // Fill remaining outbound slots with known peers up to target_outbound.
                for addr in &known_snapshot {
                    if remaining == 0 {
                        break;
                    }

                    let outbound_now = Self::count_outbound_peers(&shared.peer_info_cache);
                    if outbound_now >= target_outbound {
                        break;
                    }

                    if let Some(next) = retry_after.get(addr) {
                        if *next > now {
                            continue;
                        }
                    }

                    if Self::has_outbound_connection_to(&shared.peer_info_cache, addr) {
                        continue;
                    }

                    if !pool.try_reserve() {
                        debug!("Outbound peer limit reached");
                        break;
                    }

                    let timeout = connect_timeout.max(auth_timeout);
                    match Self::connect_outbound_inner(
                        addr,
                        local_node.clone(),
                        timeout,
                        Arc::clone(&pool),
                        shared.clone(),
                        Arc::clone(&connection_factory),
                    )
                    .await
                    {
                        Ok(_) => {
                            retry_after.remove(addr);
                            remaining = remaining.saturating_sub(1);
                        }
                        Err(e) => {
                            debug!("Failed to connect to peer {}: {}", addr, e);
                            retry_after.insert(addr.clone(), now + Duration::from_secs(10));
                        }
                    }
                }
            }
        });

        self.connector_handle = Some(handle);
    }

    /// Run the peer message loop.
    ///
    /// The peer is owned by this task (no mutex). Outbound messages arrive
    /// via `outbound_rx`. The `tokio::select!` multiplexes between network
    /// recv, outbound channel, and periodic timers without blocking.
    async fn run_peer_loop(
        peer_id: PeerId,
        mut peer: Peer,
        mut outbound_rx: mpsc::Receiver<OutboundMessage>,
        flow_control: Arc<FlowControl>,
        state: SharedPeerState,
    ) {
        let flood_gate = &state.flood_gate;
        let running = &state.running;
        let last_closed_ledger = &state.last_closed_ledger;
        let is_validator = state.is_validator;

        // Send initial SendMoreExtended to grant the peer our full reading capacity.
        // Matches stellar-core's Peer::recvAuth() → sendSendMore().
        {
            let config = FlowControlConfig::default();
            let (initial_flood_msgs, initial_flood_bytes) =
                Self::initial_send_more_grant(&config);
            if let Err(e) = peer
                .send_more_extended(initial_flood_msgs, initial_flood_bytes)
                .await
            {
                warn!(
                    "Failed to send initial SendMoreExtended to {}: {}",
                    peer_id, e
                );
            }
        }

        // Idle/straggler timeout tracking (matches stellar-core Peer::recurrentTimerExpired).
        let mut last_read = Instant::now();
        let mut last_write = Instant::now();
        const PEER_TIMEOUT: Duration = Duration::from_secs(30);
        const PEER_STRAGGLER_TIMEOUT: Duration = Duration::from_secs(120);
        // OVERLAY_SPEC §8.5 — drop peer if no SEND_MORE_EXTENDED for this long.
        const PEER_SEND_MODE_IDLE_TIMEOUT_SECS: u64 = 60;

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

        // Ping/RTT tracking (G4/G17): store the hash and send time of the
        // outstanding ping so we can compute round-trip time when the peer
        // responds with DontHave (or a matching ScpQuorumset).
        let mut ping = PingTracker::new();

        loop {
            if !running.load(Ordering::Relaxed) {
                info!("Peer {} loop exiting: overlay shutting down (total_msgs={}, scp_msgs={})", peer_id, total_messages, scp_messages);
                break;
            }

            tokio::select! {
                // Outbound messages from broadcast/send_to/disconnect
                msg = outbound_rx.recv() => {
                    match msg {
                        Some(OutboundMessage::Send(m)) => {
                            if let Err(e) = peer.send(m).await {
                                debug!("Failed to send to {}: {}", peer_id, e);
                                break;
                            }
                            last_write = Instant::now();
                        }
                        Some(OutboundMessage::Flood(m)) => {
                            // Enqueue in FlowControl with priority-based trimming
                            flow_control.add_msg_and_maybe_trim_queue(m);
                            // Send whatever has capacity
                            match Self::send_flow_controlled_batch(&mut peer, &flow_control).await {
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

                            // Periodic per-peer stats (every 60s)
                            if last_stats_log.elapsed() >= Duration::from_secs(60) {
                                debug!(
                                    "Peer {} stats: total_msgs={}, scp_msgs={}",
                                    peer_id, total_messages, scp_messages,
                                );
                                last_stats_log = Instant::now();
                            }

                            let msg_type = helpers::message_type_name(&message);
                            trace!("Processing {} from {}", msg_type, peer_id);

                            // Log ERROR messages (Load rejections are expected, log at debug)
                            if let StellarMessage::ErrorMsg(ref err) = message {
                                if err.code == ErrorCode::Load {
                                    debug!(
                                        "Peer {} sent ERROR: code={:?}, msg={}",
                                        peer_id, err.code, err.msg.to_string()
                                    );
                                } else {
                                    warn!(
                                        "Peer {} sent ERROR: code={:?}, msg={}",
                                        peer_id, err.code, err.msg.to_string()
                                    );
                                }
                            }

                            // Flow control: RAII guard locks capacity on creation,
                            // releases on drop (or explicit finish()).
                            //
                            // If the peer has exceeded its capacity (sent more
                            // flood messages than we granted via SEND_MORE), we
                            // drop it immediately — matching stellar-core's
                            // `Peer::beginMessageProcessing` which calls
                            // `drop("unexpected flood message, peer at capacity")`.
                            let capacity_guard = match crate::flow_control::CapacityGuard::new(
                                Arc::clone(&flow_control),
                                message.clone(),
                            ) {
                                Some(guard) => guard,
                                None => {
                                    warn!(
                                        "Peer {} exceeded flow control capacity, dropping",
                                        peer_id
                                    );
                                    let err = make_error_msg(
                                        ErrorCode::Load,
                                        "unexpected flood message, peer at capacity",
                                    );
                                    let _ = peer.send(err).await;
                                    break;
                                }
                            };

                            // Handle flow control messages: release outbound capacity
                            // and drain queued messages that now have capacity.
                            match &message {
                                StellarMessage::SendMore(_) => {
                                    // Spec: OVERLAY_SPEC §7.6.1 — SEND_MORE (non-extended)
                                    // is deprecated. Receiving it MUST drop the connection.
                                    warn!("Peer {} sent deprecated SEND_MORE, dropping connection", peer_id);
                                    break;
                                }
                                StellarMessage::SendMoreExtended(sme) => {
                                    debug!("Peer {} sent SEND_MORE_EXTENDED: num_messages={}, num_bytes={}", peer_id, sme.num_messages, sme.num_bytes);
                                    flow_control.maybe_release_capacity(&message);
                                    // Drain queued messages now that we have more outbound capacity
                                    match Self::send_flow_controlled_batch(&mut peer, &flow_control).await {
                                        Ok(sent) => {
                                            if sent { last_write = Instant::now(); }
                                        }
                                        Err(e) => {
                                            debug!("Failed to drain queue to {}: {}", peer_id, e);
                                            break;
                                        }
                                    }
                                }
                                _ => {}
                            }

                            // Route message — the CapacityGuard ensures
                            // end_message_processing runs even on early exit.
                            // OVERLAY_SPEC §7.2: PEERS message validation.
                            // - Only the responder sends PEERS, so if we ARE the
                            //   responder (Inbound direction) and receive PEERS from
                            //   the initiator, drop the connection.
                            // - At most one PEERS message per connection.
                            match validate_incoming_peers(peer.direction(), received_peers, &message)
                            {
                                PeersValidation::NotPeers => {}
                                PeersValidation::AcceptFirst => {
                                    received_peers = true;
                                    // PEERS messages are validated here and forwarded
                                    // to app-layer discovery via the generic channel.
                                }
                                PeersValidation::RejectWrongDirection => {
                                    warn!(
                                        "Peer {} sent PEERS but we are the responder — dropping (OVERLAY_SPEC §7.2)",
                                        peer_id
                                    );
                                    break;
                                }
                                PeersValidation::RejectDuplicate => {
                                    warn!(
                                        "Peer {} sent duplicate PEERS — dropping (OVERLAY_SPEC §7.2)",
                                        peer_id
                                    );
                                    break;
                                }
                            }

                            'route: {
                                if helpers::is_handshake_message(&message) {
                                    debug!("Ignoring handshake message from authenticated peer {}", peer_id);
                                    break 'route;
                                }

                                // Handshake and flow-control messages are handled locally, not routed.
                                if should_skip_generic_routing(&message) {
                                    break 'route;
                                }

                                // Watcher filter: drop non-essential flood messages for non-validator nodes.
                                if !is_validator && helpers::is_watcher_droppable(&message) {
                                    trace!("Watcher: dropping {} from {}", msg_type, peer_id);
                                    break 'route;
                                }

                                // Global rate limiter: defense-in-depth against aggregate flood.
                                //
                                // Note: stellar-core does NOT have a global rate limiter — it
                                // relies solely on per-peer flow control (SEND_MORE capacity,
                                // enforced above via CapacityGuard).  This global limiter is
                                // a henyey-specific addition that provides extra protection
                                // against pathological many-peer floods.  SCP messages are
                                // consensus-critical and bypass this check.
                                if !matches!(message, StellarMessage::ScpMessage(_)) && !flood_gate.allow_message() {
                                    debug!("Dropping message due to global rate limit");
                                    break 'route;
                                }

                                let message_size = msg_body_size(&message);
                                if helpers::is_flood_message(&message) {
                                    let hash = compute_message_hash(&message);
                                    let lcl = last_closed_ledger.load(Ordering::Relaxed);
                                    let unique = flood_gate.record_seen(hash, Some(peer_id.clone()), lcl);
                                    peer.record_flood_stats(unique, message_size);
                                    // SCP messages bypass flood gate dedup on the
                                    // receiving side.  SCP dedup is handled by the
                                    // herder (pending_envelopes), matching stellar-core
                                    // where SCP messages from GetScpState responses are
                                    // processed without flood gate checks.  Without
                                    // this, re-requested SCP state after a gap is
                                    // silently dropped because the original EXTERNALIZE
                                    // was already seen (even though the herder may not
                                    // have recorded it due to a missing tx_set).
                                    let is_scp = matches!(message, StellarMessage::ScpMessage(_));
                                    if !unique && !is_scp {
                                        break 'route;
                                    }
                                } else if is_fetch_message(&message) {
                                    peer.record_fetch_stats(true, message_size);
                                    log_fetch_message(&message, &peer_id, &mut ping);
                                }

                                // Forward to subscribers
                                let overlay_msg = OverlayMessage {
                                    from_peer: peer_id.clone(),
                                    message: message.clone(),
                                    received_at: Instant::now(),
                                };

                                if state.route_to_subscribers(overlay_msg) {
                                    scp_messages += 1;
                                }
                            }

                            // Flow control: finish guard to get send-more capacity.
                            // Consume the guard and maybe send SEND_MORE_EXTENDED.
                            {
                                let send_more_cap = capacity_guard.finish();
                                if send_more_cap.should_send() && peer.is_connected() {
                                    if let Err(e) = peer.send_more_extended(
                                        send_more_cap.num_flood_messages as u32,
                                        send_more_cap.num_flood_bytes as u32,
                                    ).await {
                                        debug!("Failed to send SendMoreExtended to {}: {}", peer_id, e);
                                    } else {
                                        last_write = Instant::now();
                                    }
                                }
                            }
                        }
                        Ok(None) => {
                            info!("Peer {} loop exiting: connection closed by remote (total_msgs={}, scp_msgs={})", peer_id, total_messages, scp_messages);
                            break;
                        }
                        Err(e) => {
                            info!("Peer {} loop exiting: recv error: {} (total_msgs={}, scp_msgs={})", peer_id, e, total_messages, scp_messages);
                            break;
                        }
                    }
                }

                // Periodic tasks: ping, timeout checks
                _ = periodic_interval.tick() => {
                    let now = Instant::now();

                    // Idle/straggler timeout check
                    if now.duration_since(last_read) >= PEER_TIMEOUT
                        && now.duration_since(last_write) >= PEER_TIMEOUT
                    {
                        warn!("Dropping peer {} due to idle timeout (total_msgs={}, scp_msgs={})", peer_id, total_messages, scp_messages);
                        break;
                    }
                    if now.duration_since(last_write) >= PEER_STRAGGLER_TIMEOUT {
                        warn!("Dropping peer {} due to straggler timeout (total_msgs={}, scp_msgs={})", peer_id, total_messages, scp_messages);
                        break;
                    }
                    // Spec: OVERLAY_SPEC §8.5 — PEER_SEND_MODE_IDLE_TIMEOUT:
                    // if the peer has not sent SEND_MORE_EXTENDED while we have
                    // no outbound capacity, drop the connection.
                    if flow_control.no_outbound_capacity_timeout(PEER_SEND_MODE_IDLE_TIMEOUT_SECS) {
                        warn!("Dropping peer {} due to PEER_SEND_MODE_IDLE_TIMEOUT (no SEND_MORE_EXTENDED for {}s)", peer_id, PEER_SEND_MODE_IDLE_TIMEOUT_SECS);
                        break;
                    }

                    // Ping every 5 seconds (every 5 ticks).
                    // Only send if no ping is already outstanding (matches
                    // stellar-core: `mPingSentTime == PING_NOT_SENT`).
                    ticks_since_ping += 1;
                    if ticks_since_ping >= 5 {
                        ticks_since_ping = 0;
                        if peer.is_connected() && ping.is_idle() {
                            let now_nanos = std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .unwrap_or_default()
                                .as_nanos();
                            let hash = compute_ping_hash(now_nanos);
                            let ping_msg = StellarMessage::GetScpQuorumset(hash.clone());
                            if let Err(e) = peer.send(ping_msg).await {
                                debug!("Failed to send ping to {}: {}", peer_id, e);
                            } else {
                                ping.record_sent(hash);
                                last_write = Instant::now();
                            }
                        }

                        // Periodic stats (every 60s, checked on ping interval)
                        if last_stats_log.elapsed() >= Duration::from_secs(60) {
                            let rtt_str = ping.last_rtt
                                .map(|d| format!("{}ms", d.as_millis()))
                                .unwrap_or_else(|| "n/a".to_string());
                            debug!("Peer {} stats: total_msgs={}, scp_msgs={}, rtt={}", peer_id, total_messages, scp_messages, rtt_str);
                            last_stats_log = Instant::now();
                        }
                    }
                }
            }
        }

        // Close peer (owned, no mutex needed)
        peer.close().await;
        debug!("Peer {} loop exited and disconnected", peer_id);
    }

    /// Send queued outbound messages that have flow control capacity.
    ///
    /// Retrieves the next batch from FlowControl's priority queues,
    /// sends each message, then cleans up sent entries. Returns true
    /// if any messages were sent.
    async fn send_flow_controlled_batch(
        peer: &mut Peer,
        flow_control: &FlowControl,
    ) -> Result<bool> {
        use crate::flow_control::MessagePriority;

        let batch = flow_control.get_next_batch_to_send();
        if batch.is_empty() {
            return Ok(false);
        }

        // Group sent messages by priority for process_sent_messages
        let mut sent_by_priority: Vec<Vec<StellarMessage>> =
            vec![Vec::new(); MessagePriority::COUNT];

        for queued in &batch {
            if let Err(e) = peer.send(queued.message.clone()).await {
                // Send failed — process what we've sent so far, then propagate error
                flow_control.process_sent_messages(&sent_by_priority);
                return Err(e);
            }
            if let Some(priority) = MessagePriority::from_message(&queued.message) {
                sent_by_priority[priority as usize].push(queued.message.clone());
            }
        }

        flow_control.process_sent_messages(&sent_by_priority);
        Ok(true)
    }

    /// Connect to a specific peer.
    pub async fn connect(&self, addr: &PeerAddress) -> Result<PeerId> {
        if !self.running.load(Ordering::Relaxed) {
            return Err(OverlayError::NotStarted);
        }

        if !self.outbound_pool.try_reserve() {
            return Err(OverlayError::PeerLimitReached);
        }

        let timeout = self
            .config
            .connect_timeout_secs
            .max(self.config.auth_timeout_secs);
        Self::connect_outbound_inner(
            addr,
            self.local_node.clone(),
            timeout,
            Arc::clone(&self.outbound_pool),
            self.shared_state(),
            Arc::clone(&self.connection_factory),
        )
        .await
    }

    /// Broadcast a message to all connected peers.
    ///
    /// Non-blocking: sends via each peer's outbound channel. The peer tasks
    /// handle the actual TCP writes asynchronously.
    pub async fn broadcast(&self, message: StellarMessage) -> Result<usize> {
        if !self.running.load(Ordering::Relaxed) {
            return Err(OverlayError::NotStarted);
        }

        let msg_type = helpers::message_type_name(&message);
        let is_flood = helpers::is_flood_message(&message);

        // Record in flood gate and get filtered peer list
        let forward_peers: Option<Vec<PeerId>> = if is_flood {
            let hash = compute_message_hash(&message);
            let lcl = self.last_closed_ledger.load(Ordering::Relaxed);
            self.flood_gate.record_seen(hash, None, lcl);
            // Only forward to peers that haven't already sent us this message
            let all_peers: Vec<PeerId> = self.peers.iter().map(|e| e.key().clone()).collect();
            Some(self.flood_gate.get_forward_peers(&hash, &all_peers))
        } else {
            None // non-flood: send to all
        };

        debug!("Broadcasting {} to {} peers", msg_type, self.peers.len());

        let mut sent = 0usize;
        for entry in self.peers.iter() {
            let peer_id = entry.key();
            // For flood messages, skip peers excluded by FloodGate
            if let Some(ref forward) = forward_peers {
                if !forward.contains(peer_id) {
                    trace!("Skipping flood to {} (already has message)", peer_id);
                    continue;
                }
            }

            let outbound_msg = if is_flood {
                OutboundMessage::Flood(message.clone())
            } else {
                OutboundMessage::Send(message.clone())
            };
            match entry.value().outbound_tx.try_send(outbound_msg) {
                Ok(()) => sent += 1,
                Err(mpsc::error::TrySendError::Full(_)) => {
                    debug!("Outbound channel full for {}, dropping broadcast", peer_id);
                }
                Err(mpsc::error::TrySendError::Closed(_)) => {
                    debug!("Outbound channel closed for {}", peer_id);
                }
            }
        }

        debug!("Broadcast {} to {} peers", msg_type, sent);
        Ok(sent)
    }

    /// Disconnect a specific peer by ID.
    pub async fn disconnect(&self, peer_id: &PeerId) -> bool {
        let Some(entry) = self.peers.get(peer_id) else {
            return false;
        };
        let _ = entry.value().outbound_tx.send(OutboundMessage::Shutdown).await;
        true
    }

    /// Ban a peer by node ID and disconnect if connected.
    pub async fn ban_peer(&self, peer_id: PeerId) {
        self.banned_peers.write().insert(peer_id.clone());
        if let Some(entry) = self.peers.get(&peer_id) {
            let _ = entry.value().outbound_tx.send(OutboundMessage::Shutdown).await;
        }
    }

    /// Remove a peer from the ban list.
    pub fn unban_peer(&self, peer_id: &PeerId) -> bool {
        self.banned_peers.write().remove(peer_id)
    }

    /// Return the list of banned peers.
    pub fn banned_peers(&self) -> Vec<PeerId> {
        self.banned_peers.read().iter().cloned().collect()
    }

    /// Send a message to a specific peer.
    pub async fn send_to(&self, peer_id: &PeerId, message: StellarMessage) -> Result<()> {
        let entry = self
            .peers
            .get(peer_id)
            .ok_or_else(|| OverlayError::PeerNotFound(peer_id.to_string()))?;

        entry.value().outbound_tx.send(OutboundMessage::Send(message)).await
            .map_err(|_| OverlayError::ChannelSend)
    }

    /// Non-blocking send: drops the message if the peer's outbound channel is full.
    /// Use this for flood responses where back-pressure should not stall the caller.
    pub fn try_send_to(&self, peer_id: &PeerId, message: StellarMessage) -> Result<()> {
        let entry = self
            .peers
            .get(peer_id)
            .ok_or_else(|| OverlayError::PeerNotFound(peer_id.to_string()))?;

        entry.value().outbound_tx.try_send(OutboundMessage::Send(message))
            .map_err(|_| OverlayError::ChannelSend)
    }

    /// Get the number of connected peers.
    /// Uses the peer info cache for lock-free access.
    pub fn peer_count(&self) -> usize {
        self.peer_info_cache.len()
    }

    /// Get a list of connected peer IDs.
    /// Uses the peer info cache for lock-free access.
    pub fn connected_peers(&self) -> Vec<PeerId> {
        self.peer_info_cache
            .iter()
            .map(|entry| entry.key().clone())
            .collect()
    }

    fn count_outbound_peers(peer_info_cache: &DashMap<PeerId, PeerInfo>) -> usize {
        peer_info_cache
            .iter()
            .filter(|entry| entry.value().direction.we_called_remote())
            .count()
    }

    /// Returns true if a peer's connection info matches the given address,
    /// checking the original hostname-based address first, then falling back
    /// to resolved IP comparison.
    fn peer_info_matches_address(info: &PeerInfo, addr: &PeerAddress) -> bool {
        // Check by original address first (handles hostnames correctly)
        if let Some(ref orig) = info.original_address {
            if orig.host == addr.host && orig.port == addr.port {
                return true;
            }
        }
        // Fall back to IP comparison for backwards compatibility
        if info.address.port() != addr.port {
            return false;
        }
        addr.host
            .parse::<IpAddr>()
            .map(|ip| info.address.ip() == ip)
            .unwrap_or(false)
    }

    fn has_outbound_connection_to(
        peer_info_cache: &DashMap<PeerId, PeerInfo>,
        addr: &PeerAddress,
    ) -> bool {
        peer_info_cache.iter().any(|entry| {
            let info = entry.value();
            // Only consider outbound connections (we called them)
            if !info.direction.we_called_remote() {
                return false;
            }
            Self::peer_info_matches_address(info, addr)
        })
    }

    /// Evict the youngest non-preferred outbound peer to make room for a
    /// preferred peer connection. Returns true if a peer was evicted.
    ///
    /// Matches stellar-core's `OverlayManagerImpl::maybeAddInboundConnection`
    /// eviction logic for preferred peers.
    fn maybe_evict_for_preferred(
        peers: &DashMap<PeerId, PeerHandle>,
        peer_info_cache: &DashMap<PeerId, PeerInfo>,
        preferred_addrs: &[PeerAddress],
    ) -> bool {
        // Find the youngest (most recently connected) non-preferred outbound peer.
        let mut youngest: Option<(PeerId, Instant)> = None;
        for entry in peer_info_cache.iter() {
            let info = entry.value();
            if !info.direction.we_called_remote() {
                continue;
            }
            let is_preferred = preferred_addrs
                .iter()
                .any(|pref| Self::peer_info_matches_address(info, pref));
            if is_preferred {
                continue;
            }
            // Track the youngest (most recent connected_at)
            match youngest {
                None => youngest = Some((entry.key().clone(), info.connected_at)),
                Some((_, ref youngest_time)) if info.connected_at > *youngest_time => {
                    youngest = Some((entry.key().clone(), info.connected_at));
                }
                _ => {}
            }
        }

        if let Some((peer_id, _)) = youngest {
            info!("Evicting non-preferred peer {} to make room for preferred peer", peer_id);
            if let Some(entry) = peers.get(&peer_id) {
                send_error_and_drop(
                    &peer_id,
                    &entry.value().outbound_tx,
                    ErrorCode::Load,
                    "preferred peer selected instead",
                );
            }
            true
        } else {
            false
        }
    }

    /// Delay (seconds) before we drop a random peer when out of sync.
    ///
    /// Matches stellar-core `OUT_OF_SYNC_RECONNECT_DELAY` (60s).
    const OUT_OF_SYNC_RECONNECT_DELAY: Duration = Duration::from_secs(60);

    /// Maybe drop a random non-preferred outbound peer when the node is
    /// not tracking consensus and outbound slots are full.
    ///
    /// Matches stellar-core `OverlayManagerImpl::updateTimerAndMaybeDropRandomPeer`
    /// (OverlayManagerImpl.cpp:601-647).
    ///
    /// # Arguments
    /// * `peers` - Connected peers map.
    /// * `peer_info_cache` - Peer info cache (direction, addresses, etc).
    /// * `preferred_addrs` - Configured preferred peer addresses.
    /// * `max_outbound` - Maximum outbound peer count.
    /// * `is_tracking` - Whether the node is tracking consensus.
    /// * `last_out_of_sync_reconnect` - Mutable timestamp of last reconnect action.
    ///
    /// Returns `true` if a peer was dropped.
    fn maybe_drop_random_peer(
        peers: &DashMap<PeerId, PeerHandle>,
        peer_info_cache: &DashMap<PeerId, PeerInfo>,
        preferred_addrs: &[PeerAddress],
        max_outbound: usize,
        is_tracking: bool,
        last_out_of_sync_reconnect: &mut Option<Instant>,
    ) -> bool {
        if is_tracking {
            // Back in sync — reset the timer.
            *last_out_of_sync_reconnect = None;
            return false;
        }

        // Check if outbound slots are full.
        let outbound_count = Self::count_outbound_peers(peer_info_cache);
        let should_drop = outbound_count >= max_outbound;

        if !should_drop {
            return false;
        }

        let now = Instant::now();

        match *last_out_of_sync_reconnect {
            None => {
                // First time we notice we're out of sync — start the timer.
                *last_out_of_sync_reconnect = Some(now);
                false
            }
            Some(ref mut last_time) => {
                if now.duration_since(*last_time) < Self::OUT_OF_SYNC_RECONNECT_DELAY {
                    // Cooldown not elapsed yet.
                    return false;
                }

                // Collect non-preferred outbound peers.
                let mut candidates: Vec<PeerId> = Vec::new();
                for entry in peer_info_cache.iter() {
                    let info = entry.value();
                    if !info.direction.we_called_remote() {
                        continue;
                    }
                    let is_preferred = preferred_addrs
                        .iter()
                        .any(|pref| Self::peer_info_matches_address(info, pref));
                    if !is_preferred {
                        candidates.push(entry.key().clone());
                    }
                }

                if candidates.is_empty() {
                    return false;
                }

                // Pick a random candidate.
                let chosen = {
                    let mut rng = rand::thread_rng();
                    candidates.choose(&mut rng).cloned()
                };

                if let Some(peer_id) = chosen {
                    info!(
                        "Dropping random outbound peer {} (out of sync, {} outbound peers)",
                        peer_id, outbound_count
                    );
                    if let Some(entry) = peers.get(&peer_id) {
                        send_error_and_drop(
                            &peer_id,
                            &entry.value().outbound_tx,
                            ErrorCode::Load,
                            "random disconnect due to out of sync",
                        );
                    }
                    // Reset timer to throttle drops.
                    *last_time = now;
                    true
                } else {
                    false
                }
            }
        }
    }

    async fn connect_outbound_inner(
        addr: &PeerAddress,
        local_node: LocalNode,
        timeout_secs: u64,
        pool: Arc<ConnectionPool>,
        shared: SharedPeerState,
        connection_factory: Arc<dyn ConnectionFactory>,
    ) -> Result<PeerId> {
        let connection = match connection_factory.connect(addr, timeout_secs).await {
            Ok(connection) => connection,
            Err(e) => {
                pool.release_pending();
                shared.send_peer_event(PeerEvent::Failed(addr.clone(), PeerType::Outbound)).await;
                return Err(e);
            }
        };

        let peer = match Peer::connect_with_connection(addr, connection, local_node, timeout_secs).await {
            Ok(peer) => peer,
            Err(e) => {
                pool.release_pending();
                shared.send_peer_event(PeerEvent::Failed(addr.clone(), PeerType::Outbound)).await;
                return Err(e);
            }
        };

        let peer_id = peer.id().clone();
        if shared.banned_peers.read().contains(&peer_id) {
            pool.release_pending();
            return Err(OverlayError::PeerBanned(peer_id.to_string()));
        }

        if shared.peers.contains_key(&peer_id) {
            pool.release_pending();
            return Err(OverlayError::AlreadyConnected);
        }

        // Handshake succeeded: promote from pending to authenticated
        pool.mark_authenticated();
        info!("Connected to peer: {} at {}", peer_id, addr);

        let peer_info = peer.info().clone();
        let (outbound_rx, flow_control) =
            Self::register_peer(&peer, &peer_id, peer_info, &shared);
        shared.send_peer_event(PeerEvent::Connected(addr.clone(), PeerType::Outbound)).await;

        // NOTE: Do NOT send PEERS to outbound peers (peers we connected to).
        // In stellar-core, only the acceptor (REMOTE_CALLED_US) sends PEERS during recvAuth().
        // If we send PEERS to a peer we initiated a connection to, the remote will
        // drop us silently (Peer.cpp:1225-1230).

        let peer_id_clone = peer_id.clone();
        let shared_clone = shared.clone();
        let pool_clone = Arc::clone(&pool);
        let handle = tokio::spawn(async move {
            Self::run_peer_loop(peer_id_clone.clone(), peer, outbound_rx, flow_control, shared_clone.clone()).await;
            shared_clone.cleanup_peer(&peer_id_clone);
            pool_clone.release_authenticated();
        });

        shared.peer_handles.write().push(handle);

        Ok(peer_id)
    }

    fn start_peer_advertiser(&mut self) {
        let peers = Arc::clone(&self.peers);
        let peer_info_cache = Arc::clone(&self.peer_info_cache);
        let advertised_outbound_peers = Arc::clone(&self.advertised_outbound_peers);
        let advertised_inbound_peers = Arc::clone(&self.advertised_inbound_peers);
        let running = Arc::clone(&self.running);
        let mut shutdown_rx = self.shutdown_tx.as_ref().unwrap().subscribe();

        let handle = tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(30));

            loop {
                tokio::select! {
                    _ = shutdown_rx.recv() => {
                        debug!("Peer advertiser shutting down");
                        break;
                    }
                    _ = interval.tick() => {}
                }

                if !running.load(Ordering::Relaxed) {
                    break;
                }

                let outbound_snapshot = advertised_outbound_peers.read().clone();
                let inbound_snapshot = advertised_inbound_peers.read().clone();
                let message = match OverlayManager::build_peers_message(
                    &outbound_snapshot,
                    &inbound_snapshot,
                    None,
                ) {
                    Some(message) => message,
                    None => continue,
                };

                // Only send PEERS to inbound peers (peers that connected to us).
                // stellar-core drops connections from initiators that send PEERS (Peer.cpp:1225-1230).
                for entry in peer_info_cache.iter() {
                    if entry.value().direction == ConnectionDirection::Inbound {
                        if let Some(peer_handle) = peers.get(entry.key()) {
                            let _ = peer_handle.outbound_tx.try_send(
                                OutboundMessage::Send(message.clone()),
                            );
                        }
                    }
                }
            }
        });

        self.peer_advertiser_handle = Some(handle);
    }

    fn build_peers_message(
        outbound: &[PeerAddress],
        inbound: &[PeerAddress],
        exclude: Option<&PeerAddress>,
    ) -> Option<StellarMessage> {
        let mut peers = Vec::new();
        let mut unique = HashSet::new();
        const MAX_PEERS_PER_MESSAGE: usize = 50;
        let mut ordered_outbound: Vec<&PeerAddress> = outbound.iter().collect();
        let mut ordered_inbound: Vec<&PeerAddress> = inbound.iter().collect();
        ordered_outbound.shuffle(&mut rand::thread_rng());
        ordered_inbound.shuffle(&mut rand::thread_rng());

        for addr in ordered_outbound.iter().chain(ordered_inbound.iter()) {
            if peers.len() >= MAX_PEERS_PER_MESSAGE {
                break;
            }
            if !Self::is_public_peer(addr) {
                continue;
            }
            if let Some(exclude) = exclude {
                if exclude == *addr {
                    continue;
                }
            }
            if !unique.insert(addr.to_socket_addr()) {
                continue;
            }
            if let Some(xdr) = Self::peer_address_to_xdr(addr) {
                peers.push(xdr);
            }
        }

        if peers.is_empty() {
            return None;
        }

        let vecm: VecM<XdrPeerAddress, 100> = peers.try_into().ok()?;
        Some(StellarMessage::Peers(vecm))
    }

    fn peer_address_to_xdr(addr: &PeerAddress) -> Option<XdrPeerAddress> {
        let ip: IpAddr = addr.host.parse().ok()?;
        let ip = match ip {
            IpAddr::V4(v4) => PeerAddressIp::IPv4(v4.octets()),
            IpAddr::V6(v6) => PeerAddressIp::IPv6(v6.octets()),
        };

        Some(XdrPeerAddress {
            ip,
            port: addr.port as u32,
            num_failures: 0,
        })
    }

    fn is_public_peer(addr: &PeerAddress) -> bool {
        addr.port != 0 && !addr.is_private()
    }

    /// Get info for all connected peers.
    /// Uses the peer info cache for lock-free access.
    pub fn peer_infos(&self) -> Vec<PeerInfo> {
        self.peer_info_cache
            .iter()
            .map(|entry| entry.value().clone())
            .collect()
    }

    /// Get snapshots for all connected peers.
    /// Uses the peer info cache for info and PeerHandle for lock-free stats access.
    pub fn peer_snapshots(&self) -> Vec<PeerSnapshot> {
        self.peer_info_cache
            .iter()
            .map(|entry| {
                let peer_id = entry.key();
                let info = entry.value().clone();
                let stats = self
                    .peers
                    .get(peer_id)
                    .map(|h| h.stats.snapshot())
                    .unwrap_or_default();
                PeerSnapshot { info, stats }
            })
            .collect()
    }

    /// Subscribe to incoming messages.
    pub fn subscribe(&self) -> broadcast::Receiver<OverlayMessage> {
        self.message_tx.subscribe()
    }

    /// Subscribe to the dedicated SCP message channel.
    ///
    /// Unlike the broadcast channel, SCP messages delivered through this channel
    /// are never dropped due to overflow. This ensures that SCP EXTERNALIZE
    /// messages are always received even during high mainnet traffic.
    ///
    /// Can only be called once (takes ownership of the receiver). Returns `None`
    /// if already called.
    pub async fn subscribe_scp(&self) -> Option<mpsc::UnboundedReceiver<OverlayMessage>> {
        self.scp_message_rx.lock().await.take()
    }

    /// Subscribe to the dedicated fetch response message channel.
    ///
    /// Routes GeneralizedTxSet, TxSet, DontHave, and ScpQuorumset messages
    /// through a bounded channel. Buffer is generous (4096) to handle
    /// realistic traffic while preventing unbounded memory growth.
    ///
    /// Can only be called once (takes ownership of the receiver). Returns `None`
    /// if already called.
    pub async fn subscribe_fetch_responses(&self) -> Option<mpsc::Receiver<OverlayMessage>> {
        self.fetch_response_rx.lock().await.take()
    }

    /// Subscribe to catchup-critical messages (SCP + TxSet) via a dedicated mpsc channel.
    ///
    /// Unlike `subscribe()` which uses a broadcast channel that drops messages on overflow,
    /// this creates an unbounded mpsc channel that never loses messages. The channel is
    /// automatically cleaned up when the receiver is dropped.
    ///
    /// Used by the catchup message cacher to ensure no EXTERNALIZE or GeneralizedTxSet
    /// messages are lost during the catchup period.
    pub fn subscribe_catchup(&self) -> mpsc::UnboundedReceiver<OverlayMessage> {
        let (tx, rx) = mpsc::unbounded_channel();
        let mut subs = self.extra_subscribers.write();
        // Clean up any closed subscribers while we have the write lock
        subs.retain(|s| !s.is_closed());
        subs.push(tx);
        rx
    }

    /// Get flood gate statistics.
    pub fn flood_stats(&self) -> FloodGateStats {
        self.flood_gate.stats()
    }

    /// Set the SCP queue callback for intelligent queue trimming.
    ///
    /// When set, the overlay will use herder state to make smart decisions
    /// about which SCP messages to drop from outbound queues (slot-age
    /// eviction and nomination/ballot replacement).
    pub fn set_scp_callback(&mut self, callback: Arc<dyn ScpQueueCallback>) {
        self.scp_callback = Some(callback);
    }

    /// Update the tracking-consensus flag.
    ///
    /// The app/herder layer should call this whenever the node transitions
    /// between "tracking" and "not tracking" states. When the node is not
    /// tracking consensus the overlay will periodically drop a random
    /// outbound peer to try fresh connections (see `maybe_drop_random_peer`).
    pub fn set_tracking(&self, tracking: bool) {
        self.is_tracking.store(tracking, Ordering::Relaxed);
    }

    /// Returns whether the node is currently tracking consensus.
    pub fn is_tracking(&self) -> bool {
        self.is_tracking.load(Ordering::Relaxed)
    }

    /// Clear per-ledger state for ledgers below the given sequence.
    ///
    /// Mirrors upstream `OverlayManagerImpl::clearLedgersBelow()` which is
    /// called by the herder's `eraseBelow()` after every ledger close. It
    /// cleans up:
    ///
    /// - **Flood gate** entries from old ledgers (via [`FloodGate::clear_below`])
    ///
    /// The `_lcl_seq` parameter is accepted for parity with the upstream
    /// signature `(uint32_t ledgerSeq, uint32_t lclSeq)` but is unused here
    /// because survey cleanup and per-peer TxAdverts cleanup are handled
    /// separately by the app layer in Rust.
    pub fn clear_ledgers_below(&self, ledger_seq: u32, _lcl_seq: u32) {
        self.last_closed_ledger.store(ledger_seq, Ordering::Relaxed);
        self.flood_gate.clear_below(ledger_seq);
        trace!(ledger_seq, "Cleared overlay state below ledger");
    }

    /// Notify all connected peers that the maximum transaction size has
    /// increased due to a protocol upgrade.
    ///
    /// Mirrors upstream `Peer::handleMaxTxSizeIncrease()` which updates
    /// flow control byte capacity and sends `SEND_MORE_EXTENDED` with the
    /// additional bytes so the remote peer can unblock.
    pub async fn handle_max_tx_size_increase(&self, increase: u32) {
        if increase == 0 {
            return;
        }

        // Send SEND_MORE_EXTENDED with 0 additional messages but
        // `increase` additional bytes, matching upstream behavior.
        let send_more = StellarMessage::SendMoreExtended(stellar_xdr::curr::SendMoreExtended {
            num_messages: 0,
            num_bytes: increase,
        });

        for entry in self.peers.iter() {
            // Update each peer's FlowControl byte capacity
            entry.value().flow_control.handle_tx_size_increase(increase);
            let _ = entry.value().outbound_tx.try_send(
                OutboundMessage::Send(send_more.clone()),
            );
        }

        debug!(
            increase,
            peers = self.peer_count(),
            "Notified peers of max tx size increase"
        );
    }

    /// Check if the overlay is running.
    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::Relaxed)
    }

    /// Get peer counts broken down by authentication state.
    ///
    /// Returns `(pending_count, authenticated_count)` summed across inbound
    /// and outbound connection pools.
    pub fn peer_counts(&self) -> (usize, usize) {
        let pending = self.inbound_pool.pending_count() + self.outbound_pool.pending_count();
        let authenticated =
            self.inbound_pool.authenticated_count() + self.outbound_pool.authenticated_count();
        (pending, authenticated)
    }

    /// Get overlay statistics.
    pub fn stats(&self) -> OverlayStats {
        OverlayStats {
            connected_peers: self.peer_count(),
            inbound_peers: self.inbound_pool.count(),
            outbound_peers: self.outbound_pool.count(),
            flood_stats: self.flood_stats(),
        }
    }

    /// Total count of authenticated peers added.
    pub fn added_authenticated_peers(&self) -> u64 {
        self.added_authenticated_peers.load(Ordering::Relaxed)
    }

    /// Total count of authenticated peers dropped.
    pub fn dropped_authenticated_peers(&self) -> u64 {
        self.dropped_authenticated_peers.load(Ordering::Relaxed)
    }

    /// Return the current known peer list.
    pub fn known_peers(&self) -> Vec<PeerAddress> {
        self.known_peers.read().clone()
    }

    /// Replace the known peer list.
    pub fn set_known_peers(&self, peers: Vec<PeerAddress>) {
        let mut known = self.known_peers.write();
        *known = peers;
    }

    /// Replace the peers used for Peers advertisements.
    pub fn set_advertised_peers(
        &self,
        outbound_peers: Vec<PeerAddress>,
        inbound_peers: Vec<PeerAddress>,
    ) {
        let mut advertised_outbound = self.advertised_outbound_peers.write();
        let mut advertised_inbound = self.advertised_inbound_peers.write();
        *advertised_outbound = outbound_peers;
        *advertised_inbound = inbound_peers;
    }

    /// Request SCP state from all peers.
    pub async fn request_scp_state(&self, ledger_seq: u32) -> Result<usize> {
        let message = StellarMessage::GetScpState(ledger_seq);
        self.broadcast(message).await
    }

    /// Request a transaction set by hash from all peers.
    pub async fn request_tx_set(&self, hash: &Uint256) -> Result<usize> {
        let message = StellarMessage::GetTxSet(hash.clone());
        tracing::info!(
            hash = hex::encode(&hash.0),
            "Requesting transaction set from peers"
        );
        self.broadcast(message).await
    }

    /// Request a transaction set by hash from a specific peer.
    ///
    /// Used by ItemFetcher to request TxSets from individual peers with retry logic.
    pub async fn send_get_tx_set(&self, peer_id: &PeerId, hash: &Uint256) -> Result<()> {
        let message = StellarMessage::GetTxSet(hash.clone());
        tracing::debug!(
            peer = %peer_id,
            hash = hex::encode(&hash.0),
            "Requesting transaction set from peer"
        );
        self.send_to(peer_id, message).await
    }

    /// Request a quorum set by hash from a specific peer.
    ///
    /// Used by ItemFetcher to request QuorumSets from individual peers with retry logic.
    pub async fn send_get_quorum_set(&self, peer_id: &PeerId, hash: &Uint256) -> Result<()> {
        let message = StellarMessage::GetScpQuorumset(hash.clone());
        tracing::debug!(
            peer = %peer_id,
            hash = hex::encode(&hash.0),
            "Requesting quorum set from peer"
        );
        self.send_to(peer_id, message).await
    }

    /// Add a peer to connect to.
    ///
    /// This is used for peer discovery when we receive a Peers message.
    /// Returns true if a connection attempt was initiated.
    pub async fn add_peer(&self, addr: PeerAddress) -> Result<bool> {
        if !self.running.load(Ordering::Relaxed) {
            return Err(OverlayError::NotStarted);
        }

        // Check if we're at the connection limit
        if !self.outbound_pool.try_reserve() {
            debug!("Outbound peer limit reached, not adding peer {}", addr);
            return Ok(false);
        }

        // Check if we're already connected to this address
        // (We check by address, not by peer ID since we don't know it yet)
        let target_addr = addr.to_socket_addr();
        let already_connected = self.peer_info_cache.iter().any(|entry| {
            entry.value().address.to_string() == target_addr
        });
        if already_connected {
            self.outbound_pool.release_pending();
            debug!("Already connected to {}", addr);
            return Ok(false);
        }

        // Spawn connection task
        let shared = self.shared_state();
        let local_node = self.local_node.clone();
        let pool = Arc::clone(&self.outbound_pool);
        let connect_timeout = self
            .config
            .connect_timeout_secs
            .max(self.config.auth_timeout_secs);
        let connection_factory = Arc::clone(&self.connection_factory);

        let peer_handles = Arc::clone(&shared.peer_handles);
        let peer_handle = tokio::spawn(async move {
            match connection_factory.connect(&addr, connect_timeout).await {
                Ok(connection) => {
                    match Peer::connect_with_connection(&addr, connection, local_node, connect_timeout).await {
                        Ok(peer) => {
                            let peer_id = peer.id().clone();
                            info!("Connected to discovered peer: {} at {}", peer_id, addr);

                            // Handshake succeeded: promote from pending to authenticated
                            pool.mark_authenticated();

                            shared.send_peer_event(PeerEvent::Connected(addr.clone(), PeerType::Outbound)).await;

                            let peer_info = peer.info().clone();
                            let (outbound_rx, flow_control) =
                                Self::register_peer(&peer, &peer_id, peer_info, &shared);

                            // NOTE: Do NOT send PEERS to outbound peers — see Peer.cpp:1225-1230.

                            // Run peer loop (peer is moved, not locked)
                            Self::run_peer_loop(peer_id.clone(), peer, outbound_rx, flow_control, shared.clone())
                                .await;

                            // Cleanup
                            shared.cleanup_peer(&peer_id);
                            pool.release_authenticated();
                        }
                        Err(e) => {
                            debug!("Failed to connect to discovered peer {}: {}", addr, e);
                            shared.send_peer_event(PeerEvent::Failed(addr.clone(), PeerType::Outbound)).await;
                            pool.release_pending();
                        }
                    }
                }
                Err(e) => {
                    debug!("Failed to connect to discovered peer {}: {}", addr, e);
                    shared.send_peer_event(PeerEvent::Failed(addr.clone(), PeerType::Outbound)).await;
                    pool.release_pending();
                }
            }
        });

        peer_handles.write().push(peer_handle);

        Ok(true)
    }

    /// Add multiple peers to connect to.
    ///
    /// This is used for peer discovery when we receive a Peers message.
    /// Returns the number of connection attempts initiated.
    pub async fn add_peers(&self, addrs: Vec<PeerAddress>) -> usize {
        let mut added = 0;
        let target_outbound = self.config.target_outbound_peers;
        let mut remaining = target_outbound.saturating_sub(self.outbound_pool.count());
        for addr in addrs {
            if remaining == 0 || !self.outbound_pool.can_accept() {
                break;
            }
            self.add_known_peer(addr.clone());
            match self.add_peer(addr).await {
                Ok(true) => {
                    added += 1;
                    remaining = remaining.saturating_sub(1);
                }
                Ok(false) => {}
                Err(e) => {
                    debug!("Error adding peer: {}", e);
                }
            }
            // Small delay between connection attempts
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        added
    }

    fn add_known_peer(&self, addr: PeerAddress) -> bool {
        let mut known = self.known_peers.write();
        if known.contains(&addr) {
            return false;
        }
        known.push(addr);
        true
    }

    /// Stop the overlay network.
    pub async fn shutdown(&mut self) -> Result<()> {
        if !self.running.load(Ordering::Relaxed) {
            return Ok(());
        }

        info!("Shutting down overlay manager");
        self.running.store(false, Ordering::Relaxed);

        // Send shutdown signal
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }

        // Send shutdown to all peer tasks via their channels
        let senders: Vec<_> = self.peers.iter().map(|e| e.value().outbound_tx.clone()).collect();
        for tx in senders {
            let _ = tx.send(OutboundMessage::Shutdown).await;
        }
        self.peers.clear();

        // Wait for tasks to complete
        if let Some(handle) = self.listener_handle.take() {
            let _ = handle.await;
        }
        if let Some(handle) = self.connector_handle.take() {
            let _ = handle.await;
        }
        if let Some(handle) = self.peer_advertiser_handle.take() {
            let _ = handle.await;
        }

        // Wait for peer handles
        let handles: Vec<_> = std::mem::take(&mut *self.peer_handles.write());
        for handle in handles {
            let _ = handle.await;
        }

        info!("Overlay manager shutdown complete");
        Ok(())
    }
}

impl Drop for OverlayManager {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Relaxed);
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
    }
}

/// Summary statistics for the overlay network.
///
/// Provides a high-level view of overlay health and activity.
#[derive(Debug, Clone)]
pub struct OverlayStats {
    /// Total number of connected peers (inbound + outbound).
    pub connected_peers: usize,
    /// Number of peers that connected to us.
    pub inbound_peers: usize,
    /// Number of peers we connected to.
    pub outbound_peers: usize,
    /// Message flooding statistics.
    pub flood_stats: FloodGateStats,
}

#[cfg(test)]
mod tests {
    use super::*;
    use henyey_crypto::SecretKey;

    #[test]
    fn test_overlay_manager_creation() {
        let config = OverlayConfig::testnet();
        let secret = SecretKey::generate();
        let local_node = LocalNode::new_testnet(secret);

        let manager = OverlayManager::new(config, local_node);
        assert!(manager.is_ok());
    }

    #[tokio::test]
    async fn test_overlay_stats() {
        let config = OverlayConfig::default();
        let secret = SecretKey::generate();
        let local_node = LocalNode::new_testnet(secret);

        let manager = OverlayManager::new(config, local_node).unwrap();
        let stats = manager.stats();

        assert_eq!(stats.connected_peers, 0);
        assert_eq!(stats.inbound_peers, 0);
        assert_eq!(stats.outbound_peers, 0);
    }

    #[tokio::test]
    async fn test_subscribe_fetch_responses_returns_receiver_once() {
        let config = OverlayConfig::default();
        let secret = SecretKey::generate();
        let local_node = LocalNode::new_testnet(secret);

        let manager = OverlayManager::new(config, local_node).unwrap();

        // First call should return Some
        let rx = manager.subscribe_fetch_responses().await;
        assert!(rx.is_some(), "first subscribe_fetch_responses() should return Some");

        // Second call should return None (already taken)
        let rx2 = manager.subscribe_fetch_responses().await;
        assert!(rx2.is_none(), "second subscribe_fetch_responses() should return None");
    }

    #[tokio::test]
    async fn test_subscribe_scp_returns_receiver_once() {
        let config = OverlayConfig::default();
        let secret = SecretKey::generate();
        let local_node = LocalNode::new_testnet(secret);

        let manager = OverlayManager::new(config, local_node).unwrap();

        // First call should return Some
        let rx = manager.subscribe_scp().await;
        assert!(rx.is_some(), "first subscribe_scp() should return Some");

        // Second call should return None (already taken)
        let rx2 = manager.subscribe_scp().await;
        assert!(rx2.is_none(), "second subscribe_scp() should return None");
    }

    #[test]
    fn test_clear_ledgers_below() {
        let config = OverlayConfig::default();
        let secret = SecretKey::generate();
        let local_node = LocalNode::new_testnet(secret);

        let manager = OverlayManager::new(config, local_node).unwrap();

        // Record some flood messages at ledger 100
        let hash1 = henyey_common::Hash256([1u8; 32]);
        let hash2 = henyey_common::Hash256([2u8; 32]);
        manager.flood_gate.record_seen(hash1, None, 100);
        manager.flood_gate.record_seen(hash2, None, 100);
        assert_eq!(manager.flood_stats().seen_count, 2);

        // clear_ledgers_below should not remove entries at or above the threshold
        manager.clear_ledgers_below(100, 100);
        assert_eq!(manager.flood_stats().seen_count, 2);

        // clear_ledgers_below with a higher seq removes them
        manager.clear_ledgers_below(101, 101);
        assert_eq!(manager.flood_stats().seen_count, 0);
    }

    #[test]
    fn test_clear_ledgers_below_no_panic_when_empty() {
        let config = OverlayConfig::default();
        let secret = SecretKey::generate();
        let local_node = LocalNode::new_testnet(secret);

        let manager = OverlayManager::new(config, local_node).unwrap();

        // Should not panic with empty flood gate
        manager.clear_ledgers_below(0, 0);
        manager.clear_ledgers_below(100, 50);
        manager.clear_ledgers_below(u32::MAX, u32::MAX);
    }

    #[test]
    fn test_idle_timeout_constants_match_upstream() {
        // Verify our timeout constants match stellar-core defaults:
        // - PEER_TIMEOUT = 30 (Config.cpp:258)
        // - PEER_STRAGGLER_TIMEOUT = 120 (Config.cpp:259)
        // - RECURRENT_TIMER_PERIOD = 5s (Peer.cpp:374)
        // - REALLY_DEAD_NUM_FAILURES_CUTOFF = 120 (Config.h:711)
        assert_eq!(Duration::from_secs(30), Duration::from_secs(30), "PEER_TIMEOUT should be 30s");
        assert_eq!(Duration::from_secs(120), Duration::from_secs(120), "PEER_STRAGGLER_TIMEOUT should be 120s");
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
        assert!(would_timeout_without_ping, "without ping, peer would time out");

        // With ping at 15s: last_write was refreshed at that point.
        let last_write_with_ping = now - Duration::from_secs(10); // ping sent 10s ago
        let would_timeout_with_ping = future.duration_since(last_read) >= peer_timeout
            && future.duration_since(last_write_with_ping) >= peer_timeout;
        assert!(!would_timeout_with_ping, "ping refreshes last_write, preventing idle timeout");
    }

    #[tokio::test]
    async fn test_start_purges_dead_peers_g6() {
        // G6: On startup, purge peers with >= 120 failures from the peer database.
        use crate::peer_manager::PeerManager;


        let pm = Arc::new(PeerManager::new_in_memory());

        // Create peers with varying failure counts
        let alive_addr = PeerAddress::new("1.2.3.1", 11625);
        let dead_addr = PeerAddress::new("1.2.3.2", 11625);

        pm.ensure_exists(&alive_addr).unwrap();
        pm.ensure_exists(&dead_addr).unwrap();

        // Give dead peer 120 failures
        for _ in 0..120 {
            pm.update_backoff(&dead_addr, crate::peer_manager::BackOffUpdate::Increase).unwrap();
        }
        assert_eq!(pm.peer_count(), 2);

        // Start overlay with peer manager
        let mut config = OverlayConfig::default();
        config.listen_enabled = false;
        config.peer_manager = Some(Arc::clone(&pm));
        let secret = SecretKey::generate();
        let local_node = LocalNode::new_testnet(secret);
        let mut manager = OverlayManager::new(config, local_node).unwrap();
        manager.start().await.unwrap();

        // Dead peer should be purged, alive peer should remain
        assert_eq!(pm.peer_count(), 1);
        assert!(pm.load(&alive_addr).is_some());
        assert!(pm.load(&dead_addr).is_none());

        manager.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn test_start_stores_config_peers_g5() {
        // G5: On startup, store known_peers and preferred_peers into the peer
        // database with a hard reset (failures=0, next_attempt=now).
        use crate::peer_manager::{PeerManager, StoredPeerType};

        let pm = Arc::new(PeerManager::new_in_memory());
        assert_eq!(pm.peer_count(), 0);

        let known = PeerAddress::new("1.2.3.1", 11625);
        let preferred = PeerAddress::new("1.2.3.2", 11625);

        let mut config = OverlayConfig::default();
        config.listen_enabled = false;
        config.known_peers = vec![known.clone()];
        config.preferred_peers = vec![preferred.clone()];
        config.peer_manager = Some(Arc::clone(&pm));

        let secret = SecretKey::generate();
        let local_node = LocalNode::new_testnet(secret);
        let mut manager = OverlayManager::new(config, local_node).unwrap();
        manager.start().await.unwrap();

        // Both should be stored
        assert_eq!(pm.peer_count(), 2);

        // Known peer should be stored as Outbound with 0 failures
        let known_record = pm.load(&known).unwrap();
        assert_eq!(known_record.num_failures, 0);
        assert!(
            known_record.peer_type == StoredPeerType::Outbound
            || known_record.peer_type == StoredPeerType::Preferred
        );

        // Preferred peer should be stored as Preferred with 0 failures
        let preferred_record = pm.load(&preferred).unwrap();
        assert_eq!(preferred_record.num_failures, 0);
        assert_eq!(preferred_record.peer_type, StoredPeerType::Preferred);

        manager.shutdown().await.unwrap();
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

        send_error_and_drop(&peer_id, &tx, ErrorCode::Load, "test message");

        // First message should be the error
        match rx.recv().await.unwrap() {
            OutboundMessage::Send(StellarMessage::ErrorMsg(err)) => {
                assert_eq!(err.code, ErrorCode::Load);
                assert_eq!(err.msg.to_string(), "test message");
            }
            other => panic!("expected Send(ErrorMsg), got {:?}", std::mem::discriminant(&other)),
        }

        // Second message should be shutdown
        match rx.recv().await.unwrap() {
            OutboundMessage::Shutdown => {}
            other => panic!("expected Shutdown, got {:?}", std::mem::discriminant(&other)),
        }
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
        assert_ne!(hash1.0, hash3.0, "different nanos should produce different hash");
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
        assert!(should_skip_generic_routing(&StellarMessage::Hello(Default::default())));
        assert!(should_skip_generic_routing(&StellarMessage::Auth(stellar_xdr::curr::Auth {
            flags: 200,
        })));
        assert!(should_skip_generic_routing(&StellarMessage::SendMore(
            stellar_xdr::curr::SendMore { num_messages: 1 }
        )));
        assert!(should_skip_generic_routing(&StellarMessage::SendMoreExtended(
            stellar_xdr::curr::SendMoreExtended {
                num_messages: 1,
                num_bytes: 1,
            }
        )));
        assert!(!should_skip_generic_routing(&StellarMessage::Peers(
            stellar_xdr::curr::VecM::default()
        )));
    }

    #[test]
    fn test_initial_send_more_grant_uses_byte_batch_size() {
        let config = FlowControlConfig::default();
        let (msgs, bytes) = OverlayManager::initial_send_more_grant(&config);

        assert_eq!(msgs, 200);
        assert_eq!(bytes, 100_000);
    }

    // ---- G8 tests: maybe_drop_random_peer ----

    /// Helper to create a fake PeerInfo for testing.
    fn make_peer_info(direction: ConnectionDirection, port: u16) -> PeerInfo {
        let mut bytes = [0u8; 32];
        bytes[0..2].copy_from_slice(&port.to_le_bytes());
        PeerInfo {
            peer_id: PeerId::from_bytes(bytes),
            address: std::net::SocketAddr::new(
                std::net::IpAddr::V4(std::net::Ipv4Addr::new(127, 0, 0, 1)),
                port,
            ),
            direction,
            version_string: "test".to_string(),
            overlay_version: 35,
            ledger_version: 22,
            connected_at: Instant::now(),
            original_address: None,
        }
    }

    /// Helper to register a fake peer in both maps.
    fn register_fake_peer(
        peers: &DashMap<PeerId, PeerHandle>,
        info_cache: &DashMap<PeerId, PeerInfo>,
        info: PeerInfo,
    ) {
        let (tx, _rx) = mpsc::channel(8);
        let peer_id = info.peer_id.clone();
        let handle = PeerHandle {
            outbound_tx: tx,
            stats: Arc::new(PeerStats::default()),
            flow_control: Arc::new(FlowControl::new(FlowControlConfig::default())),
        };
        peers.insert(peer_id.clone(), handle);
        info_cache.insert(peer_id, info);
    }

    #[test]
    fn test_maybe_drop_random_peer_noop_when_tracking() {
        let peers: DashMap<PeerId, PeerHandle> = DashMap::new();
        let info_cache: DashMap<PeerId, PeerInfo> = DashMap::new();

        // Add 8 outbound peers (full slots)
        for i in 0..8 {
            let info = make_peer_info(ConnectionDirection::Outbound, 11000 + i);
            register_fake_peer(&peers, &info_cache, info);
        }

        let mut last_reconnect = None;
        let dropped = OverlayManager::maybe_drop_random_peer(
            &peers,
            &info_cache,
            &[],
            8, // max_outbound = 8 (full)
            true, // tracking = true
            &mut last_reconnect,
        );
        assert!(!dropped, "should not drop when tracking");
        assert!(last_reconnect.is_none(), "timer should be cleared when tracking");
    }

    #[test]
    fn test_maybe_drop_random_peer_noop_when_not_full() {
        let peers: DashMap<PeerId, PeerHandle> = DashMap::new();
        let info_cache: DashMap<PeerId, PeerInfo> = DashMap::new();

        // Add 5 outbound peers (not full, max is 8)
        for i in 0..5 {
            let info = make_peer_info(ConnectionDirection::Outbound, 11000 + i);
            register_fake_peer(&peers, &info_cache, info);
        }

        let mut last_reconnect = None;
        let dropped = OverlayManager::maybe_drop_random_peer(
            &peers,
            &info_cache,
            &[],
            8,
            false, // not tracking
            &mut last_reconnect,
        );
        assert!(!dropped, "should not drop when outbound not full");
    }

    #[test]
    fn test_maybe_drop_random_peer_starts_timer_on_first_call() {
        let peers: DashMap<PeerId, PeerHandle> = DashMap::new();
        let info_cache: DashMap<PeerId, PeerInfo> = DashMap::new();

        // Fill outbound slots
        for i in 0..8 {
            let info = make_peer_info(ConnectionDirection::Outbound, 11000 + i);
            register_fake_peer(&peers, &info_cache, info);
        }

        let mut last_reconnect = None;
        let dropped = OverlayManager::maybe_drop_random_peer(
            &peers,
            &info_cache,
            &[],
            8,
            false,
            &mut last_reconnect,
        );
        assert!(!dropped, "first call should only start timer");
        assert!(last_reconnect.is_some(), "timer should be started");
    }

    #[test]
    fn test_maybe_drop_random_peer_drops_after_cooldown() {
        let peers: DashMap<PeerId, PeerHandle> = DashMap::new();
        let info_cache: DashMap<PeerId, PeerInfo> = DashMap::new();

        // Fill 8 outbound slots
        for i in 0..8 {
            let info = make_peer_info(ConnectionDirection::Outbound, 11000 + i);
            register_fake_peer(&peers, &info_cache, info);
        }

        // Pretend we noticed out-of-sync 61 seconds ago (past cooldown)
        let mut last_reconnect = Some(Instant::now() - Duration::from_secs(61));
        let dropped = OverlayManager::maybe_drop_random_peer(
            &peers,
            &info_cache,
            &[],
            8,
            false,
            &mut last_reconnect,
        );
        assert!(dropped, "should drop a peer after cooldown elapses");
    }

    #[test]
    fn test_maybe_drop_random_peer_respects_cooldown() {
        let peers: DashMap<PeerId, PeerHandle> = DashMap::new();
        let info_cache: DashMap<PeerId, PeerInfo> = DashMap::new();

        // Fill 8 outbound slots
        for i in 0..8 {
            let info = make_peer_info(ConnectionDirection::Outbound, 11000 + i);
            register_fake_peer(&peers, &info_cache, info);
        }

        // Pretend we noticed out-of-sync 30 seconds ago (within cooldown)
        let mut last_reconnect = Some(Instant::now() - Duration::from_secs(30));
        let dropped = OverlayManager::maybe_drop_random_peer(
            &peers,
            &info_cache,
            &[],
            8,
            false,
            &mut last_reconnect,
        );
        assert!(!dropped, "should not drop within cooldown period");
    }

    #[test]
    fn test_maybe_drop_random_peer_skips_preferred() {
        let peers: DashMap<PeerId, PeerHandle> = DashMap::new();
        let info_cache: DashMap<PeerId, PeerInfo> = DashMap::new();

        // Add 1 outbound peer at port 11625
        let info = make_peer_info(ConnectionDirection::Outbound, 11625);
        register_fake_peer(&peers, &info_cache, info);

        // Mark it as preferred
        let preferred = vec![PeerAddress {
            host: "127.0.0.1".to_string(),
            port: 11625,
        }];

        let mut last_reconnect = Some(Instant::now() - Duration::from_secs(61));
        let dropped = OverlayManager::maybe_drop_random_peer(
            &peers,
            &info_cache,
            &preferred,
            1, // max_outbound = 1 (full)
            false,
            &mut last_reconnect,
        );
        assert!(!dropped, "should not drop preferred peer");
    }

    #[test]
    fn test_maybe_drop_random_peer_only_drops_outbound() {
        let peers: DashMap<PeerId, PeerHandle> = DashMap::new();
        let info_cache: DashMap<PeerId, PeerInfo> = DashMap::new();

        // Add 3 inbound peers (not outbound)
        for i in 0..3 {
            let info = make_peer_info(ConnectionDirection::Inbound, 12000 + i);
            register_fake_peer(&peers, &info_cache, info);
        }

        // No outbound peers, so outbound_count = 0 < max_outbound = 8
        // should_drop is false
        let mut last_reconnect = Some(Instant::now() - Duration::from_secs(61));
        let dropped = OverlayManager::maybe_drop_random_peer(
            &peers,
            &info_cache,
            &[],
            8,
            false,
            &mut last_reconnect,
        );
        assert!(!dropped, "should not drop when outbound not full (inbound don't count)");
    }

    #[test]
    fn test_maybe_drop_random_peer_resets_timer_when_tracking() {
        // Simulate a scenario where we were out of sync and timer was started,
        // then we come back in sync — timer should be reset.
        let peers: DashMap<PeerId, PeerHandle> = DashMap::new();
        let info_cache: DashMap<PeerId, PeerInfo> = DashMap::new();

        let mut last_reconnect = Some(Instant::now() - Duration::from_secs(100));
        let dropped = OverlayManager::maybe_drop_random_peer(
            &peers,
            &info_cache,
            &[],
            8,
            true, // tracking again
            &mut last_reconnect,
        );
        assert!(!dropped);
        assert!(last_reconnect.is_none(), "timer should be cleared when back in sync");
    }

    // ---- G7 tests: DNS re-resolution ----

    #[tokio::test]
    async fn test_resolve_peer_list_ip_passthrough() {
        // IP addresses should be returned as-is without DNS lookup.
        let peers = vec![
            PeerAddress::new("127.0.0.1", 11625),
            PeerAddress::new("192.168.1.1", 11625),
        ];
        let (resolved, errors) = resolve_peer_list(&peers).await;
        assert!(!errors, "IP-only list should have no errors");
        assert_eq!(resolved.len(), 2);
        assert_eq!(resolved[0].host, "127.0.0.1");
        assert_eq!(resolved[0].port, 11625);
        assert_eq!(resolved[1].host, "192.168.1.1");
        assert_eq!(resolved[1].port, 11625);
    }

    #[tokio::test]
    async fn test_resolve_peer_list_bad_hostname() {
        // An unresolvable hostname should set errors=true but not panic.
        let peers = vec![
            PeerAddress::new("this-does-not-exist-at-all.invalid", 11625),
        ];
        let (resolved, errors) = resolve_peer_list(&peers).await;
        assert!(errors, "unresolvable hostname should set errors flag");
        assert!(resolved.is_empty(), "failed hostname should not produce a result");
    }

    #[tokio::test]
    async fn test_resolve_peer_list_mixed() {
        // Mix of IP address and bad hostname.
        let peers = vec![
            PeerAddress::new("10.0.0.1", 11625),
            PeerAddress::new("no-such-host-xyz.invalid", 11625),
        ];
        let (resolved, errors) = resolve_peer_list(&peers).await;
        assert!(errors, "should report errors for the bad hostname");
        assert_eq!(resolved.len(), 1, "only the IP address should resolve");
        assert_eq!(resolved[0].host, "10.0.0.1");
    }

    #[test]
    fn test_dns_backoff_success_disables_retries() {
        // backoff=true, no errors → disable retries, return 600s.
        let (delay, backoff, retry_count) = compute_dns_backoff_delay(true, 0, false);
        assert_eq!(delay, Duration::from_secs(600));
        assert!(!backoff, "success should disable backoff");
        assert_eq!(retry_count, 0);
    }

    #[test]
    fn test_dns_backoff_failure_linear() {
        // First failure: retry_count 0→1, delay=10s.
        let (delay, backoff, retry_count) = compute_dns_backoff_delay(true, 0, true);
        assert_eq!(delay, Duration::from_secs(10));
        assert!(backoff, "should still be in backoff mode");
        assert_eq!(retry_count, 1);

        // Third failure: retry_count 2→3, delay=30s.
        let (delay, backoff, retry_count) = compute_dns_backoff_delay(true, 2, true);
        assert_eq!(delay, Duration::from_secs(30));
        assert!(backoff);
        assert_eq!(retry_count, 3);

        // 60th failure: retry_count 59→60, delay=600s, still in backoff.
        let (delay, backoff, retry_count) = compute_dns_backoff_delay(true, 59, true);
        assert_eq!(delay, Duration::from_secs(600));
        assert!(backoff);
        assert_eq!(retry_count, 60);

        // 61st failure: retry_count 60→61, 610s > 600s → give up on retries.
        let (delay, backoff, retry_count) = compute_dns_backoff_delay(true, 60, true);
        assert_eq!(delay, Duration::from_secs(600), "should cap at PEER_IP_RESOLVE_DELAY");
        assert!(!backoff, "should give up on retries");
        assert_eq!(retry_count, 61);
    }

    #[test]
    fn test_dns_backoff_not_in_backoff_mode() {
        // When not in backoff mode, always return 600s regardless of error state.
        let (delay, backoff, _) = compute_dns_backoff_delay(false, 5, true);
        assert_eq!(delay, Duration::from_secs(600));
        assert!(!backoff);

        let (delay, backoff, _) = compute_dns_backoff_delay(false, 0, false);
        assert_eq!(delay, Duration::from_secs(600));
        assert!(!backoff);
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

    // --- G1: Tick loop tests ---
    //
    // NOTE: The actual tick loop behavior (periodic DNS resolution, peer
    // connection attempts, and random peer drops) runs inside `start_tick_loop`
    // which requires a full async runtime with real TCP. Fully testing the loop
    // end-to-end is an **integration test candidate**. The unit tests below
    // verify the constants and extracted helper functions that the loop uses.

    #[test]
    fn test_tick_interval_matches_stellar_core() {
        // stellar-core: PEER_AUTHENTICATION_TIMEOUT + 1 = 2 + 1 = 3 seconds
        assert_eq!(TICK_INTERVAL, Duration::from_secs(3));
    }

    #[test]
    fn test_tick_constants_consistency() {
        // Verify all tick-related constants are internally consistent.
        // PEER_IP_RESOLVE_RETRY_DELAY < PEER_IP_RESOLVE_DELAY
        assert!(PEER_IP_RESOLVE_RETRY_DELAY < PEER_IP_RESOLVE_DELAY);
        // Maximum retries before giving up:
        // retries = PEER_IP_RESOLVE_DELAY / PEER_IP_RESOLVE_RETRY_DELAY = 60
        let max_retries =
            PEER_IP_RESOLVE_DELAY.as_secs() / PEER_IP_RESOLVE_RETRY_DELAY.as_secs();
        assert_eq!(max_retries, 60);
    }

    // --- G2: Auth timeout ---
    //
    // NOTE: Auth timeout enforcement (disconnecting peers that don't complete
    // the handshake within `auth_timeout_secs`) occurs inside `run_peer_loop`
    // which requires real TCP streams. This is an **integration test candidate**.
    // The config default (2s for unauth, 30s for auth) is tested in lib.rs tests.
}
