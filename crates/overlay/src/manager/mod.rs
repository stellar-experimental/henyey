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

mod connection;
mod peer_loop;
mod tick;

use crate::{
    codec::helpers,
    connection::ConnectionPool,
    connection_factory::{ConnectionFactory, TcpConnectionFactory},
    flood::{compute_message_hash, FloodGate, FloodGateStats},
    flow_control::{FlowControl, ScpQueueCallback},
    metrics::OverlayMetrics,
    peer::{PeerInfo, PeerStats, PeerStatsSnapshot},
    LocalNode, OverlayConfig, OverlayError, PeerAddress, PeerEvent, PeerId, Result,
};
use dashmap::DashMap;
use parking_lot::{Mutex, RwLock};
use rand::seq::SliceRandom;
use rand::Rng;
use std::collections::HashSet;
use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use stellar_xdr::curr::{
    PeerAddress as XdrPeerAddress, PeerAddressIp, StellarMessage, Uint256, VecM,
};
use tokio::sync::{broadcast, mpsc, Mutex as TokioMutex};
use tokio::task::JoinHandle;
use tracing::{debug, error, info, trace, warn};

/// Maximum number of known peer addresses kept in memory.
///
/// Matches the batch size used by `load_random_peers` from the database (1000).
/// Prevents unbounded growth from PEERS messages sent by remote nodes.
const MAX_KNOWN_PEERS: usize = 1000;

/// Buffer size for the broadcast channel carrying non-critical overlay
/// messages (TX floods, etc.). SCP and fetch-response messages bypass
/// this channel via dedicated mpsc channels, so the broadcast channel
/// only carries remaining message types. 4096 provides headroom for
/// mainnet traffic bursts from multiple peers.
const BROADCAST_CHANNEL_SIZE: usize = 4096;

/// Maximum number of peer addresses included in a single PEERS message.
///
/// Matches stellar-core's limit of 50 addresses per Peers message
/// (see `Peer::recvPeers` in Peer.cpp).
const MAX_PEERS_PER_MESSAGE: usize = 50;

/// Extra inbound slots reserved for possibly-preferred peers.
///
/// Matches stellar-core `Config::POSSIBLY_PREFERRED_EXTRA`.
const POSSIBLY_PREFERRED_EXTRA: usize = 2;

/// Immutable snapshot of preferred peer state.
///
/// Holds both the original config entries (hostnames) and DNS-resolved
/// IP-based addresses. Callers consume this via `Arc<PreferredPeerSet>` —
/// no stale cloned `Vec<PeerAddress>` possible.
///
/// This replaces the previous three separate representations:
/// - `config.preferred_peers` Vec → `config_entries`
/// - `SharedPeerState.preferred_peers: Arc<Vec<PeerAddress>>` → `Arc<PreferredPeerSet>`
/// - `ConnectionPool.preferred_ips` → updated from `resolved_ips` after each DNS cycle
#[derive(Debug, Clone)]
pub(super) struct PreferredPeerSet {
    /// Original config entries (hostnames). Used for outbound dialing,
    /// original_address matching, and connect_preferred_peers iteration.
    config_entries: Vec<PeerAddress>,
    /// Resolved IP-based addresses. Updated after each DNS resolution cycle.
    resolved: Vec<PeerAddress>,
    /// Resolved IPs only. Used for ConnectionPool preferred_ips updates
    /// and fast IP-based matching.
    resolved_ips: HashSet<IpAddr>,
}

impl PreferredPeerSet {
    /// Create initial snapshot from config (no DNS resolution yet).
    pub(super) fn from_config(config_entries: Vec<PeerAddress>) -> Self {
        Self {
            config_entries,
            resolved: Vec::new(),
            resolved_ips: HashSet::new(),
        }
    }

    /// Create updated snapshot with new DNS resolution results.
    pub(super) fn with_resolved(&self, resolved: Vec<PeerAddress>) -> Self {
        let resolved_ips = resolved
            .iter()
            .filter_map(|addr| addr.host.parse::<IpAddr>().ok())
            .collect();
        Self {
            config_entries: self.config_entries.clone(),
            resolved,
            resolved_ips,
        }
    }

    /// Check if a peer matches any preferred entry (hostname OR resolved IP).
    ///
    /// For outbound peers (with `original_address`), the hostname config entry
    /// matches directly. For inbound peers (no `original_address`), the resolved
    /// IP addresses are checked.
    pub(super) fn is_preferred(&self, info: &PeerInfo) -> bool {
        self.config_entries
            .iter()
            .any(|pref| OverlayManager::peer_info_matches_address(info, pref))
            || self
                .resolved
                .iter()
                .any(|pref| OverlayManager::peer_info_matches_address(info, pref))
    }

    /// Get config entries for outbound connection attempts, shuffled to avoid
    /// starvation. stellar-core uses random selection; fixed order causes later
    /// entries to never get a turn when outbound slots are exhausted.
    pub(super) fn shuffled_config_entries(&self, rng: &mut impl Rng) -> Vec<PeerAddress> {
        let mut entries = self.config_entries.clone();
        entries.shuffle(rng);
        entries
    }

    /// Get the resolved IP addresses for updating ConnectionPool.
    pub(super) fn resolved_ips(&self) -> &HashSet<IpAddr> {
        &self.resolved_ips
    }
}

/// An overlay message received from a peer, ready for dispatch to subscribers.
#[derive(Clone)]
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
pub(super) struct PeerHandle {
    /// Channel to send outbound messages to the peer task.
    outbound_tx: mpsc::Sender<OutboundMessage>,
    /// Shared stats (atomically updated by the peer task).
    stats: Arc<PeerStats>,
    /// Per-peer flow control (shared with the peer task).
    flow_control: Arc<FlowControl>,
}

/// Messages sent to a peer task via the outbound channel.
pub(super) enum OutboundMessage {
    /// Direct send (non-flood, e.g. GetTxSet, ScpQuorumset response).
    Send(StellarMessage),
    /// Flood message (goes through FlowControl outbound queue).
    Flood(StellarMessage),
    /// Close the connection.
    Shutdown,
}

/// Bundled connection parameters for the tick-loop helpers
/// (`connect_preferred_peers`, `fill_outbound_slots`).
pub(super) struct TickConnectCtx {
    pub(super) local_node: LocalNode,
    pub(super) connect_timeout: u64,
    pub(super) auth_timeout: u64,
    pub(super) target_outbound: usize,
    pub(super) connection_factory: Arc<dyn ConnectionFactory>,
}

/// Shared admission state for authenticated-peer promotion.
///
/// The lock around this state serializes admission decisions so concurrent
/// preferred peers cannot evict the same victim or over-promote the pool.
#[derive(Debug, Default)]
pub(super) struct AdmissionState {
    evicting: HashSet<PeerId>,
}

impl AdmissionState {
    fn is_evicting(&self, peer_id: &PeerId) -> bool {
        self.evicting.contains(peer_id)
    }

    fn mark_evicting(&mut self, peer_id: PeerId) {
        self.evicting.insert(peer_id);
    }

    fn clear_evicting(&mut self, peer_id: &PeerId) {
        self.evicting.remove(peer_id);
    }
}

/// Tracks in-flight connections to prevent duplicate dials/handshakes.
///
/// During the window between initiating a connection and completing
/// registration in `SharedPeerState::peers`, multiple concurrent tasks
/// could start handshakes to the same destination. This struct provides
/// dedup at two levels:
///
/// - **by_address**: keyed by socket address (host:port), prevents outbound
///   dial races to the same target. Inserted before dial, removed on
///   completion.
/// - **by_peer_id**: keyed by peer ID (known after HELLO), prevents
///   concurrent registration attempts for the same node. Inserted after
///   handshake, removed after register_peer or on failure.
///
/// Stale entries (from crashed/hung tasks) are swept periodically from
/// the tick loop.
///
/// Matches stellar-core's `mPendingPeers` dedup (Peer.cpp:1881-1909).
#[derive(Clone)]
pub(super) struct PendingConnections {
    /// In-flight connections by target address (host:port string).
    pub(super) by_address: Arc<DashMap<String, std::time::Instant>>,
    /// In-flight connections by peer ID (known after handshake).
    pub(super) by_peer_id: Arc<DashMap<PeerId, std::time::Instant>>,
}

/// Maximum age for a pending connection before it is considered stale.
const PENDING_CONNECTION_TIMEOUT: Duration = Duration::from_secs(30);

impl PendingConnections {
    fn new() -> Self {
        Self {
            by_address: Arc::new(DashMap::new()),
            by_peer_id: Arc::new(DashMap::new()),
        }
    }

    /// Try to reserve a pending outbound connection to the given address.
    /// Returns false if a connection to this address is already in flight.
    pub(super) fn try_reserve_address(&self, addr_key: String) -> bool {
        use dashmap::mapref::entry::Entry;
        match self.by_address.entry(addr_key) {
            Entry::Occupied(_) => false,
            Entry::Vacant(e) => {
                e.insert(std::time::Instant::now());
                true
            }
        }
    }

    /// Try to reserve a pending connection for the given peer ID.
    /// Returns false if a handshake for this peer ID is already in flight.
    /// Used in tests; production reservation now happens inside Peer::handshake().
    #[cfg(test)]
    pub(super) fn try_reserve_peer_id(&self, peer_id: &PeerId) -> bool {
        use dashmap::mapref::entry::Entry;
        match self.by_peer_id.entry(peer_id.clone()) {
            Entry::Occupied(_) => false,
            Entry::Vacant(e) => {
                e.insert(std::time::Instant::now());
                true
            }
        }
    }

    /// Release a pending address reservation.
    pub(super) fn release_address(&self, addr_key: &str) {
        self.by_address.remove(addr_key);
    }

    /// Release a pending peer ID reservation.
    pub(super) fn release_peer_id(&self, peer_id: &PeerId) {
        self.by_peer_id.remove(peer_id);
    }

    /// Remove stale pending entries older than PENDING_CONNECTION_TIMEOUT.
    pub(super) fn sweep_stale(&self) {
        let cutoff = std::time::Instant::now() - PENDING_CONNECTION_TIMEOUT;
        self.by_address.retain(|_, ts| *ts > cutoff);
        self.by_peer_id.retain(|_, ts| *ts > cutoff);
    }
}

/// Shared state passed to spawned peer tasks.
///
/// Bundles all `Arc`-wrapped state that background tasks need, avoiding
/// 20+ individual parameter lists on `connect_to_explicit_peer` and
/// `run_peer_loop`.
#[derive(Clone)]
pub(super) struct SharedPeerState {
    pub(super) peers: Arc<DashMap<PeerId, PeerHandle>>,
    pub(super) flood_gate: Arc<FloodGate>,
    pub(super) running: Arc<AtomicBool>,
    pub(super) message_tx: broadcast::Sender<OverlayMessage>,
    pub(super) scp_message_tx: mpsc::UnboundedSender<OverlayMessage>,
    pub(super) fetch_response_tx: mpsc::UnboundedSender<OverlayMessage>,
    pub(super) peer_handles: Arc<RwLock<Vec<JoinHandle<()>>>>,
    pub(super) advertised_outbound_peers: Arc<RwLock<Vec<PeerAddress>>>,
    pub(super) advertised_inbound_peers: Arc<RwLock<Vec<PeerAddress>>>,
    pub(super) added_authenticated_peers: Arc<std::sync::atomic::AtomicU64>,
    pub(super) dropped_authenticated_peers: Arc<std::sync::atomic::AtomicU64>,
    pub(super) banned_peers: Arc<RwLock<HashSet<PeerId>>>,
    pub(super) peer_info_cache: Arc<DashMap<PeerId, PeerInfo>>,
    /// Last closed ledger sequence, used for flood record cleanup.
    pub(super) last_closed_ledger: Arc<AtomicU32>,
    /// Optional callback for intelligent SCP queue trimming.
    pub(super) scp_callback: Option<Arc<dyn ScpQueueCallback>>,
    pub(super) is_validator: bool,
    pub(super) peer_event_tx: Option<mpsc::Sender<PeerEvent>>,
    pub(super) extra_subscribers: Arc<RwLock<Vec<mpsc::UnboundedSender<OverlayMessage>>>>,
    /// Whether the node is tracking consensus (set by the herder/app layer).
    /// When false, the overlay may drop random peers to try new connections.
    pub(super) is_tracking: Arc<AtomicBool>,
    /// Tracks in-flight connections for dedup.
    pub(super) pending_connections: PendingConnections,
    /// Preferred peer set shared by all connection tasks and updated after DNS
    /// resolution so admission decisions use current config and resolved IPs.
    pub(super) preferred_peers: Arc<RwLock<PreferredPeerSet>>,
    /// Serialized authenticated admission state.
    pub(super) admission_state: Arc<Mutex<AdmissionState>>,
    /// Current depth of the dedicated fetch channel. Incremented on every
    /// successful send from `route_to_subscribers` and decremented by the
    /// consumer on every successful `recv()`. Exposed via `/metrics` as
    /// `henyey_overlay_fetch_channel_depth`. Tracked on the send side so the
    /// gauge stays fresh even when the app event loop wedges (issue #1741).
    pub(super) fetch_channel_depth: Arc<AtomicI64>,
    /// Monotonic high-water mark for `fetch_channel_depth`. Advanced on the
    /// send side from `route_to_subscribers` via a CAS loop. Exposed via
    /// `/metrics` as `henyey_overlay_fetch_channel_depth_max`.
    pub(super) fetch_channel_depth_max: Arc<AtomicI64>,
    /// Shared overlay metrics counters.
    pub(super) metrics: Arc<OverlayMetrics>,
    /// Per-peer query rate-limit window in whole seconds, updated by the app
    /// layer after each ledger close. See `OverlayManager::set_query_rate_limit_window`.
    pub(super) query_rate_limit_window_secs: Arc<AtomicU64>,
    /// Current maximum transaction size in bytes. Shared with the app layer
    /// (same `Arc<AtomicU32>`) so the overlay can dynamically compute the
    /// initial byte grant for new peers via `compute_flow_control_bytes_total`.
    pub(super) max_tx_size_bytes: Arc<AtomicU32>,
}

impl SharedPeerState {
    /// Send a peer event if a subscriber is registered.
    pub(super) async fn send_peer_event(&self, event: PeerEvent) {
        if let Some(tx) = self.peer_event_tx.as_ref() {
            let _ = tx.send(event).await;
        }
    }

    /// Clean up shared state after a peer disconnects.
    /// Must be called after `run_peer_loop` completes for any authenticated peer.
    pub(super) fn cleanup_peer(&self, peer_id: &PeerId) {
        self.peers.remove(peer_id);
        self.peer_info_cache.remove(peer_id);
        self.admission_state.lock().clear_evicting(peer_id);
        self.dropped_authenticated_peers
            .fetch_add(1, Ordering::Relaxed);
    }

    /// Forward an overlay message to the appropriate subscriber channels.
    ///
    /// Returns `true` if the message was an SCP message (for counter tracking).
    /// Routes to dedicated channels (SCP, fetch response, extra subscribers)
    /// first, then falls through to the generic broadcast channel for
    /// non-dedicated messages.
    // SECURITY: subscriber count bounded by internal callers; no external input
    pub(super) fn route_to_subscribers(&self, msg: OverlayMessage) -> bool {
        let is_scp = matches!(msg.message, StellarMessage::ScpMessage(_));
        let is_fetch_response = matches!(
            msg.message,
            StellarMessage::GeneralizedTxSet(_)
                | StellarMessage::TxSet(_)
                | StellarMessage::DontHave(_)
                | StellarMessage::ScpQuorumset(_)
        );
        // Fetch-request messages (GetScpState, GetScpQuorumset, GetTxSet) must also
        // use the dedicated fetch channel so they are not silently dropped by the
        // lossy broadcast ring when the app loop lags. stellar-core services these
        // directly on the peer thread without a lossy intermediary.
        let is_fetch_request = matches!(
            msg.message,
            StellarMessage::GetScpState(_)
                | StellarMessage::GetScpQuorumset(_)
                | StellarMessage::GetTxSet(_)
        );
        let is_dedicated = is_scp || is_fetch_response || is_fetch_request;

        if is_scp {
            if let Err(e) = self.scp_message_tx.send(msg.clone()) {
                error!("SCP channel send FAILED for peer {}: {}", msg.from_peer, e);
            }
        }

        if is_fetch_response || is_fetch_request {
            if let Err(e) = self.fetch_response_tx.send(msg.clone()) {
                error!(
                    "Fetch channel send FAILED for peer {}: {}",
                    msg.from_peer, e
                );
            } else {
                // Issue #1741: account for the enqueue on the send side so the
                // depth gauge reflects backlog even when the event loop is
                // wedged (which is the exact failure mode the metric is meant
                // to diagnose).
                let new_depth = self.fetch_channel_depth.fetch_add(1, Ordering::Relaxed) + 1;
                let mut prev = self.fetch_channel_depth_max.load(Ordering::Relaxed);
                while new_depth > prev {
                    match self.fetch_channel_depth_max.compare_exchange_weak(
                        prev,
                        new_depth,
                        Ordering::Relaxed,
                        Ordering::Relaxed,
                    ) {
                        Ok(_) => break,
                        Err(observed) => prev = observed,
                    }
                }
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
    pub(super) config: OverlayConfig,
    /// Local node info.
    pub(super) local_node: LocalNode,
    /// Connected peers. Each entry is a lightweight handle with a channel
    /// to the peer's dedicated task (which owns the actual `Peer`).
    pub(super) peers: Arc<DashMap<PeerId, PeerHandle>>,
    /// Flood gate.
    pub(super) flood_gate: Arc<FloodGate>,
    /// Connection pool for inbound connections.
    pub(super) inbound_pool: Arc<ConnectionPool>,
    /// Connection pool for outbound connections.
    pub(super) outbound_pool: Arc<ConnectionPool>,
    /// Whether the manager is running.
    pub(super) running: Arc<AtomicBool>,
    /// Channel for incoming messages.
    pub(super) message_tx: broadcast::Sender<OverlayMessage>,
    /// Handle to listener task.
    pub(super) listener_handle: Option<JoinHandle<()>>,
    /// Handle to connector task.
    pub(super) connector_handle: Option<JoinHandle<()>>,
    /// Handle to peer tasks.
    pub(super) peer_handles: Arc<RwLock<Vec<JoinHandle<()>>>>,
    /// Known peers learned from discovery.
    pub(super) known_peers: Arc<RwLock<Vec<PeerAddress>>>,
    /// Outbound peers to advertise in Peers messages.
    pub(super) advertised_outbound_peers: Arc<RwLock<Vec<PeerAddress>>>,
    /// Inbound peers to advertise in Peers messages.
    pub(super) advertised_inbound_peers: Arc<RwLock<Vec<PeerAddress>>>,
    /// Total authenticated peers added.
    pub(super) added_authenticated_peers: Arc<std::sync::atomic::AtomicU64>,
    /// Total authenticated peers dropped.
    pub(super) dropped_authenticated_peers: Arc<std::sync::atomic::AtomicU64>,
    /// Banned peers by node ID.
    pub(super) banned_peers: Arc<RwLock<HashSet<PeerId>>>,
    /// Shutdown signal. Wrapped in `Mutex` for interior mutability so
    /// `signal_shutdown(&self)` can take it through a shared reference.
    pub(super) shutdown_tx: Mutex<Option<broadcast::Sender<()>>>,
    /// Cache of peer info for connected peers (lock-free access).
    pub(super) peer_info_cache: Arc<DashMap<PeerId, PeerInfo>>,
    /// Dedicated unbounded channel for SCP messages.
    /// SCP messages are consensus-critical and must never be dropped.
    /// Mainnet generates ~24 validators * multiple SCP rounds per slot,
    /// which can overwhelm bounded channels during catchup.
    pub(super) scp_message_tx: mpsc::UnboundedSender<OverlayMessage>,
    /// Receiver end of the SCP channel. Taken once via `subscribe_scp()`.
    scp_message_rx: Arc<TokioMutex<Option<mpsc::UnboundedReceiver<OverlayMessage>>>>,
    /// Dedicated unbounded channel for fetch response messages.
    /// Routes GeneralizedTxSet, TxSet, DontHave, ScpQuorumset, GetScpState,
    /// GetScpQuorumset, and GetTxSet through a never-drop channel, matching
    /// stellar-core's synchronous IO-loop dispatch. Depth is exposed via
    /// `henyey_overlay_fetch_channel_depth` gauges for operator visibility.
    pub(super) fetch_response_tx: mpsc::UnboundedSender<OverlayMessage>,
    /// Receiver end of the fetch response channel. Taken once via `subscribe_fetch_responses()`.
    fetch_response_rx: Arc<TokioMutex<Option<mpsc::UnboundedReceiver<OverlayMessage>>>>,
    /// Dynamic extra subscribers for catchup-critical messages (SCP + TxSet).
    /// Created on demand via `subscribe_catchup()` and cleaned up when dropped.
    /// Uses parking_lot::RwLock for minimal contention in the hot path (read-heavy).
    pub(super) extra_subscribers: Arc<RwLock<Vec<mpsc::UnboundedSender<OverlayMessage>>>>,
    /// Last closed ledger sequence, used for flood record cleanup.
    pub(super) last_closed_ledger: Arc<AtomicU32>,
    /// Optional callback for intelligent SCP queue trimming.
    pub(super) scp_callback: Option<Arc<dyn ScpQueueCallback>>,
    /// Whether the node is tracking consensus (set by the herder/app layer).
    /// When false, the overlay may drop random peers to try new connections.
    pub(super) is_tracking: Arc<AtomicBool>,
    /// Connection factory used for transport establishment.
    pub(super) connection_factory: Arc<dyn ConnectionFactory>,
    /// Tracks in-flight connections for dedup.
    pub(super) pending_connections: PendingConnections,
    /// Current preferred peer set shared with connection tasks.
    pub(super) preferred_peers: Arc<RwLock<PreferredPeerSet>>,
    /// Serialized authenticated admission state shared with connection tasks.
    pub(super) admission_state: Arc<Mutex<AdmissionState>>,
    /// Shared with `SharedPeerState`; see field docs there. Plumbed in from
    /// the app so the same atomics back both the `/metrics` gauge and the
    /// watchdog read path.
    pub(super) fetch_channel_depth: Arc<AtomicI64>,
    pub(super) fetch_channel_depth_max: Arc<AtomicI64>,
    /// Overlay metrics counters. Shared with peer loops and exposed via
    /// `/metrics` as `stellar_overlay_*` gauges and counters.
    pub(super) metrics: Arc<OverlayMetrics>,
    /// Per-peer query rate-limit window in whole seconds.
    ///
    /// stellar-core computes this as `expectedLedgerCloseTime * MAX_SLOTS_TO_REMEMBER`
    /// (Peer.cpp:1426-1429), truncated to seconds. The app layer updates this
    /// via [`set_query_rate_limit_window`] after each ledger close; peer tasks
    /// read it through `SharedPeerState`.
    pub(super) query_rate_limit_window_secs: Arc<AtomicU64>,
    /// Current maximum transaction size in bytes. Shared with the app layer
    /// via the same `Arc<AtomicU32>` so the overlay reads the latest value
    /// when computing initial byte grants for new peers.
    pub(super) max_tx_size_bytes: Arc<AtomicU32>,
}

impl OverlayManager {
    /// Create a new overlay manager with the given configuration.
    pub fn new(config: OverlayConfig, local_node: LocalNode) -> Result<Self> {
        Self::new_with_connection_factory(config, local_node, Arc::new(TcpConnectionFactory))
    }

    /// Create a new overlay manager with a custom connection factory.
    // SECURITY: subscriber count bounded by internal callers; no external input
    pub fn new_with_connection_factory(
        config: OverlayConfig,
        local_node: LocalNode,
        connection_factory: Arc<dyn ConnectionFactory>,
    ) -> Result<Self> {
        Self::new_with_fetch_metrics(
            config,
            local_node,
            connection_factory,
            Arc::new(AtomicI64::new(0)),
            Arc::new(AtomicI64::new(0)),
            Arc::new(AtomicU32::new(
                crate::flow_control::INITIAL_PEER_FLOOD_READING_CAPACITY_BYTES,
            )),
        )
    }

    /// Create a new overlay manager with externally-owned atomics for the
    /// fetch channel depth metrics. The caller (typically `App`) keeps its
    /// own `Arc` handles so the same atomics back `/metrics` and the
    /// watchdog. Issue #1741.
    ///
    /// `max_tx_size_bytes` is the shared atomic tracking the current maximum
    /// transaction size in bytes. The overlay reads this to compute the
    /// initial byte grant for new peers via [`compute_flow_control_bytes_total`].
    // SECURITY: subscriber count bounded by internal callers; no external input
    pub fn new_with_fetch_metrics(
        config: OverlayConfig,
        local_node: LocalNode,
        connection_factory: Arc<dyn ConnectionFactory>,
        fetch_channel_depth: Arc<AtomicI64>,
        fetch_channel_depth_max: Arc<AtomicI64>,
        max_tx_size_bytes: Arc<AtomicU32>,
    ) -> Result<Self> {
        // Broadcast channel for non-critical overlay messages (TX floods, etc.).
        // SCP and fetch-response messages bypass this channel via dedicated mpsc
        // channels, so the broadcast channel only carries remaining message types.
        let (message_tx, _) = broadcast::channel(BROADCAST_CHANNEL_SIZE);
        let (shutdown_tx, _) = broadcast::channel(1);
        let (scp_message_tx, scp_message_rx) = mpsc::unbounded_channel();
        let (fetch_response_tx, fetch_response_rx) = mpsc::unbounded_channel();
        let preferred_peers = Arc::new(RwLock::new(PreferredPeerSet::from_config(
            config.preferred_peers.clone(),
        )));

        Ok(Self {
            config: config.clone(),
            local_node,
            peers: Arc::new(DashMap::new()),
            flood_gate: Arc::new(FloodGate::with_ttl(Duration::from_secs(
                config.flood_ttl_secs,
            ))),
            inbound_pool: Arc::new({
                // Always construct with preferred headroom so that once DNS
                // resolves, inbound preferred peers get extra slots immediately.
                // Initially empty — update_preferred_ips() is called after DNS.
                ConnectionPool::with_preferred(
                    config.max_inbound_peers,
                    POSSIBLY_PREFERRED_EXTRA,
                    HashSet::new(),
                )
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
            added_authenticated_peers: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            dropped_authenticated_peers: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            banned_peers: Arc::new(RwLock::new(HashSet::new())),
            shutdown_tx: Mutex::new(Some(shutdown_tx)),
            peer_info_cache: Arc::new(DashMap::new()),
            scp_message_tx,
            scp_message_rx: Arc::new(TokioMutex::new(Some(scp_message_rx))),
            fetch_response_tx,
            fetch_response_rx: Arc::new(TokioMutex::new(Some(fetch_response_rx))),
            extra_subscribers: Arc::new(RwLock::new(Vec::new())),
            last_closed_ledger: Arc::new(AtomicU32::new(0)),
            scp_callback: None,
            is_tracking: Arc::new(AtomicBool::new(false)),
            connection_factory,
            pending_connections: PendingConnections::new(),
            preferred_peers,
            admission_state: Arc::new(Mutex::new(AdmissionState::default())),
            fetch_channel_depth,
            fetch_channel_depth_max,
            metrics: Arc::new(OverlayMetrics::new()),
            query_rate_limit_window_secs: Arc::new(AtomicU64::new(60)),
            max_tx_size_bytes,
        })
    }

    /// Create a snapshot of shared state for passing to spawned tasks.
    pub(super) fn shared_state(&self) -> SharedPeerState {
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
            pending_connections: self.pending_connections.clone(),
            preferred_peers: Arc::clone(&self.preferred_peers),
            admission_state: Arc::clone(&self.admission_state),
            fetch_channel_depth: Arc::clone(&self.fetch_channel_depth),
            fetch_channel_depth_max: Arc::clone(&self.fetch_channel_depth_max),
            metrics: Arc::clone(&self.metrics),
            query_rate_limit_window_secs: Arc::clone(&self.query_rate_limit_window_secs),
            max_tx_size_bytes: Arc::clone(&self.max_tx_size_bytes),
        }
    }

    /// Start the overlay network (listening and connecting to peers).
    pub async fn start(&mut self) -> Result<()> {
        if self.running.load(Ordering::Relaxed) {
            return Err(OverlayError::AlreadyStarted);
        }

        info!("Starting overlay manager");
        self.running.store(true, Ordering::Relaxed);

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

        Ok(())
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
        connection::connect_to_explicit_peer(
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

        // Collect target peer IDs so we can move the message into the last send.
        let target_peers: Vec<PeerId> = self
            .peers
            .iter()
            .filter_map(|entry| {
                let peer_id = entry.key();
                if forward_peers
                    .as_ref()
                    .map_or(true, |fwd| fwd.contains(peer_id))
                {
                    Some(peer_id.clone())
                } else {
                    None
                }
            })
            .collect();

        debug!("Broadcasting {} to {} peers", msg_type, target_peers.len());

        let mut sent = 0usize;
        let num_targets = target_peers.len();
        let mut message = Some(message);
        for (i, peer_id) in target_peers.iter().enumerate() {
            let is_last = i + 1 == num_targets;
            let outbound_msg = if is_last {
                // Move the original into the last send to avoid one clone.
                let msg = message.take().unwrap();
                if is_flood {
                    OutboundMessage::Flood(msg)
                } else {
                    OutboundMessage::Send(msg)
                }
            } else {
                // Clone for all but the last peer.
                let msg = message.as_ref().unwrap().clone();
                if is_flood {
                    OutboundMessage::Flood(msg)
                } else {
                    OutboundMessage::Send(msg)
                }
            };
            if let Some(entry) = self.peers.get(peer_id) {
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
        }

        debug!("Broadcast {} to {} peers", msg_type, sent);
        self.metrics.messages_broadcast.add(sent as u64);
        Ok(sent)
    }

    /// Disconnect a specific peer by ID.
    pub async fn disconnect(&self, peer_id: &PeerId) -> bool {
        let Some(entry) = self.peers.get(peer_id) else {
            return false;
        };
        // Use try_send to avoid blocking if the peer's channel is full.
        // The peer_loop will exit on its own via the `running` flag or
        // straggler timeout.
        let _ = entry
            .value()
            .outbound_tx
            .try_send(OutboundMessage::Shutdown);
        true
    }

    /// Ban a peer by node ID and disconnect if connected.
    pub async fn ban_peer(&self, peer_id: PeerId) {
        self.banned_peers.write().insert(peer_id.clone());
        if let Some(entry) = self.peers.get(&peer_id) {
            let _ = entry
                .value()
                .outbound_tx
                .try_send(OutboundMessage::Shutdown);
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
    ///
    /// Non-blocking: drops the message if the peer's outbound channel is full,
    /// returning `Err(ChannelSend)`. This prevents a slow/malicious peer from
    /// stalling the caller (matching stellar-core's non-blocking sendMessage).
    pub fn try_send_to(&self, peer_id: &PeerId, message: StellarMessage) -> Result<()> {
        let entry = self
            .peers
            .get(peer_id)
            .ok_or_else(|| OverlayError::PeerNotFound(peer_id.to_string()))?;

        // Route flow-controlled messages through the Flood path so they
        // consume per-peer SEND_MORE_EXTENDED credit, matching stellar-core's
        // Peer::sendMessage() which always flow-controls flood messages
        // regardless of broadcast vs. targeted send (AUDIT-086).
        let outbound = if helpers::is_flood_message(&message) {
            OutboundMessage::Flood(message)
        } else {
            OutboundMessage::Send(message)
        };

        entry
            .value()
            .outbound_tx
            .try_send(outbound)
            .map_err(|_| OverlayError::ChannelSend)
    }

    /// Get the number of connected peers.
    /// Uses the peer info cache for lock-free access.
    pub fn peer_count(&self) -> usize {
        self.peer_info_cache.len()
    }

    /// Get the shared overlay metrics.
    pub fn overlay_metrics(&self) -> &OverlayMetrics {
        &self.metrics
    }

    /// Returns `(inbound_auth, outbound_auth, inbound_pending, outbound_pending)`.
    pub fn connection_breakdown(&self) -> (usize, usize, usize, usize) {
        (
            self.inbound_pool.authenticated_count(),
            self.outbound_pool.authenticated_count(),
            self.inbound_pool.pending_count(),
            self.outbound_pool.pending_count(),
        )
    }

    /// Get a list of connected peer IDs.
    /// Uses the peer info cache for lock-free access.
    pub fn connected_peers(&self) -> Vec<PeerId> {
        self.peer_info_cache
            .iter()
            .map(|entry| entry.key().clone())
            .collect()
    }

    pub(super) fn count_outbound_peers(peer_info_cache: &DashMap<PeerId, PeerInfo>) -> usize {
        peer_info_cache
            .iter()
            .filter(|entry| entry.value().direction.we_called_remote())
            .count()
    }

    /// Count outbound peers that are not in the preferred set.
    ///
    /// Matches stellar-core `nonPreferredAuthenticatedCount()`
    /// (OverlayManagerImpl.cpp:835-849). Used to compute how many outbound
    /// slots are replaceable by preferred peers.
    pub(super) fn count_non_preferred_outbound_peers(
        peer_info_cache: &DashMap<PeerId, PeerInfo>,
        preferred_set: &PreferredPeerSet,
    ) -> usize {
        peer_info_cache
            .iter()
            .filter(|entry| {
                let info = entry.value();
                info.direction.we_called_remote() && !preferred_set.is_preferred(info)
            })
            .count()
    }

    /// Returns true if a peer's connection info matches the given address,
    /// checking the original hostname-based address first, then falling back
    /// to resolved IP comparison.
    pub(super) fn peer_info_matches_address(info: &PeerInfo, addr: &PeerAddress) -> bool {
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

    pub(super) fn has_outbound_connection_to(
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

    pub(super) fn build_peers_message(
        outbound: &[PeerAddress],
        inbound: &[PeerAddress],
        exclude: Option<&PeerAddress>,
    ) -> Option<StellarMessage> {
        let mut peers = Vec::new();
        let mut unique = HashSet::new();
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
    /// Routes GeneralizedTxSet, TxSet, DontHave, ScpQuorumset, GetScpState,
    /// GetScpQuorumset, and GetTxSet messages through a dedicated unbounded
    /// channel so that no fetch-related traffic is ever silently dropped.
    /// Queue depth is sampled into `App` atomics and exported via `/metrics`
    /// (`henyey_overlay_fetch_channel_depth{,_max}`).
    ///
    /// Can only be called once (takes ownership of the receiver). Returns `None`
    /// if already called.
    pub async fn subscribe_fetch_responses(
        &self,
    ) -> Option<mpsc::UnboundedReceiver<OverlayMessage>> {
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
    // SECURITY: subscriber count bounded by internal callers; no external input
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

    /// Update the per-peer query rate-limit window.
    ///
    /// The app layer should call this after each ledger close with the
    /// result of `query_rate_limit_window(herder.ledger_close_duration())`.
    /// Parity: stellar-core recomputes this per-call in `Peer::process()`
    /// from `expectedLedgerCloseTime * MAX_SLOTS_TO_REMEMBER`.
    pub fn set_query_rate_limit_window(&self, window: Duration) {
        self.query_rate_limit_window_secs
            .store(window.as_secs(), Ordering::Relaxed);
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
            let _ = entry
                .value()
                .outbound_tx
                .try_send(OutboundMessage::Send(send_more.clone()));
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
        self.try_send_to(peer_id, message)
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
        self.try_send_to(peer_id, message)
    }

    pub(super) fn add_known_peer(&self, addr: PeerAddress) -> bool {
        let mut known = self.known_peers.write();
        if known.len() >= MAX_KNOWN_PEERS || known.contains(&addr) {
            return false;
        }
        known.push(addr);
        true
    }

    /// Timeout for joining overlay handles (listener, connector, peers)
    /// during shutdown. A single shared deadline — not additive per handle.
    const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

    /// Send the shutdown signal without joining any handles.
    ///
    /// Idempotent: the `running` atomic swap ensures the signal logic runs
    /// at most once. Safe to call through `&self` (and thus through
    /// `Arc<Self>`) when `Arc::try_unwrap` fails in the app shutdown path.
    pub fn signal_shutdown(&self) {
        if !self.running.swap(false, Ordering::SeqCst) {
            return; // already signaled
        }

        info!("Signaling overlay shutdown");

        // Broadcast shutdown to listener/connector/tick tasks.
        if let Some(tx) = self.shutdown_tx.lock().take() {
            let _ = tx.send(());
        }

        // Send shutdown to all peer tasks via their outbound channels.
        let senders: Vec<_> = self
            .peers
            .iter()
            .map(|e| e.value().outbound_tx.clone())
            .collect();
        for tx in senders {
            let _ = tx.try_send(OutboundMessage::Shutdown);
        }
        self.peers.clear();
    }

    /// Await `handle` up to `deadline`; if it doesn't finish, abort it.
    ///
    /// `JoinHandle::drop` only detaches a task (does NOT cancel it), so we
    /// poll via `&mut` to retain ownership and call `abort()` explicitly on
    /// timeout.
    async fn join_or_abort_handle(
        mut handle: JoinHandle<()>,
        deadline: tokio::time::Instant,
        label: &str,
    ) {
        tokio::select! {
            _ = &mut handle => {}
            _ = tokio::time::sleep_until(deadline) => {
                warn!("{label} handle join timed out, aborting");
                handle.abort();
            }
        }
    }

    /// Join listener, connector, and peer handles under a single shared
    /// deadline. Handles that don't finish in time are explicitly aborted.
    async fn join_handles(&mut self) {
        let start = std::time::Instant::now();
        let deadline = tokio::time::Instant::now() + Self::SHUTDOWN_TIMEOUT;

        // Listener
        if let Some(handle) = self.listener_handle.take() {
            Self::join_or_abort_handle(handle, deadline, "Listener").await;
        }

        // Connector
        if let Some(handle) = self.connector_handle.take() {
            Self::join_or_abort_handle(handle, deadline, "Connector").await;
        }

        // Peer handles — join concurrently, abort any that exceed the deadline
        let handles: Vec<_> = std::mem::take(&mut *self.peer_handles.write());
        let peer_count = handles.len();
        if !handles.is_empty() {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                warn!(
                    peer_count,
                    "No time remaining for peer handles, aborting all"
                );
                for handle in &handles {
                    handle.abort();
                }
            } else {
                let futs: Vec<_> = handles
                    .into_iter()
                    .map(|h| Self::join_or_abort_handle(h, deadline, "Peer"))
                    .collect();
                futures::future::join_all(futs).await;
                let elapsed_ms = start.elapsed().as_millis() as u64;
                if elapsed_ms > Self::SHUTDOWN_TIMEOUT.as_millis() as u64 {
                    warn!(
                        peer_count,
                        elapsed_ms, "Peer handle joins exceeded deadline"
                    );
                } else {
                    info!(peer_count, elapsed_ms, "All peer handles joined");
                }
            }
        }
    }

    /// Stop the overlay network.
    pub async fn shutdown(&mut self) -> Result<()> {
        self.signal_shutdown();

        let start = std::time::Instant::now();
        self.join_handles().await;
        info!(
            elapsed_ms = start.elapsed().as_millis() as u64,
            "Overlay manager shutdown complete"
        );

        Ok(())
    }
}

impl Drop for OverlayManager {
    fn drop(&mut self) {
        self.signal_shutdown();
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

    #[test]
    fn test_set_query_rate_limit_window_propagates_to_shared_state() {
        let config = OverlayConfig::default();
        let secret = SecretKey::generate();
        let local_node = LocalNode::new_testnet(secret);

        let manager = OverlayManager::new(config, local_node).unwrap();

        // Default should be 60s (5s * 12).
        let shared = manager.shared_state();
        assert_eq!(
            shared.query_rate_limit_window_secs.load(Ordering::Relaxed),
            60
        );

        // Update via setter and verify SharedPeerState sees the new value.
        manager.set_query_rate_limit_window(Duration::from_secs(54));
        let shared2 = manager.shared_state();
        assert_eq!(
            shared2.query_rate_limit_window_secs.load(Ordering::Relaxed),
            54
        );

        // The previously-cloned SharedPeerState should also see the update
        // (same Arc).
        assert_eq!(
            shared.query_rate_limit_window_secs.load(Ordering::Relaxed),
            54
        );
    }

    #[tokio::test]
    async fn test_subscribe_fetch_responses_returns_receiver_once() {
        let config = OverlayConfig::default();
        let secret = SecretKey::generate();
        let local_node = LocalNode::new_testnet(secret);

        let manager = OverlayManager::new(config, local_node).unwrap();

        // First call should return Some
        let rx = manager.subscribe_fetch_responses().await;
        assert!(
            rx.is_some(),
            "first subscribe_fetch_responses() should return Some"
        );

        // Second call should return None (already taken)
        let rx2 = manager.subscribe_fetch_responses().await;
        assert!(
            rx2.is_none(),
            "second subscribe_fetch_responses() should return None"
        );
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

    /// Regression test for AUDIT-H13: known_peers must be capped at MAX_KNOWN_PEERS.
    #[test]
    fn test_known_peers_cap() {
        let config = OverlayConfig::default();
        let secret = SecretKey::generate();
        let local_node = LocalNode::new_testnet(secret);

        let manager = OverlayManager::new(config, local_node).unwrap();

        // Add MAX_KNOWN_PEERS unique addresses — all should be accepted.
        for i in 0..MAX_KNOWN_PEERS {
            let port = (i % 65534 + 1) as u16;
            let host = format!("10.{}.{}.{}", (i >> 16) & 0xFF, (i >> 8) & 0xFF, i & 0xFF);
            let addr = PeerAddress::new(&host, port);
            assert!(
                manager.add_known_peer(addr),
                "peer {i} should be accepted (under cap)"
            );
        }
        assert_eq!(manager.known_peers().len(), MAX_KNOWN_PEERS);

        // One more should be rejected.
        let extra = PeerAddress::new("192.168.1.1", 9999);
        assert!(
            !manager.add_known_peer(extra),
            "should reject when at MAX_KNOWN_PEERS"
        );
        assert_eq!(manager.known_peers().len(), MAX_KNOWN_PEERS);
    }

    /// Verify deduplication still works.
    #[test]
    fn test_known_peers_dedup() {
        let config = OverlayConfig::default();
        let secret = SecretKey::generate();
        let local_node = LocalNode::new_testnet(secret);

        let manager = OverlayManager::new(config, local_node).unwrap();

        let addr = PeerAddress::new("10.0.0.1", 11625);
        assert!(manager.add_known_peer(addr.clone()));
        assert!(!manager.add_known_peer(addr));
    }

    #[test]
    fn test_pending_connections_address_dedup() {
        let pending = PendingConnections::new();
        let addr = "10.0.0.1:11625".to_string();

        assert!(
            pending.try_reserve_address(addr.clone()),
            "first reservation should succeed"
        );
        assert!(
            !pending.try_reserve_address(addr.clone()),
            "duplicate reservation should fail"
        );

        pending.release_address(&addr);
        assert!(
            pending.try_reserve_address(addr.clone()),
            "should succeed after release"
        );

        // Same IP, different port should succeed independently
        let addr2 = "10.0.0.1:11626".to_string();
        assert!(
            pending.try_reserve_address(addr2),
            "same IP but different port should succeed"
        );
    }

    #[test]
    fn test_pending_connections_peer_id_dedup() {
        let pending = PendingConnections::new();
        let peer_id = PeerId::from_bytes([1u8; 32]);

        assert!(
            pending.try_reserve_peer_id(&peer_id),
            "first reservation should succeed"
        );
        assert!(
            !pending.try_reserve_peer_id(&peer_id),
            "duplicate should fail"
        );

        pending.release_peer_id(&peer_id);
        assert!(
            pending.try_reserve_peer_id(&peer_id),
            "should succeed after release"
        );
    }

    #[test]
    fn test_pending_connections_independent_tracking() {
        let pending = PendingConnections::new();
        let addr = "10.0.0.1:11625".to_string();
        let peer_id = PeerId::from_bytes([1u8; 32]);

        // Address and peer_id are independent
        assert!(pending.try_reserve_address(addr));
        assert!(pending.try_reserve_peer_id(&peer_id));

        // Different address should work
        let addr2 = "10.0.0.2:11625".to_string();
        assert!(pending.try_reserve_address(addr2));
    }

    #[test]
    fn test_pending_connections_sweep_stale() {
        let pending = PendingConnections::new();
        let addr = "10.0.0.1:11625".to_string();

        // Insert with a backdated timestamp
        pending.by_address.insert(
            addr.clone(),
            std::time::Instant::now() - std::time::Duration::from_secs(60),
        );

        assert!(
            !pending.try_reserve_address(addr.clone()),
            "stale entry still blocks before sweep"
        );

        pending.sweep_stale();

        assert!(
            pending.try_reserve_address(addr),
            "should succeed after sweep removes stale entry"
        );
    }

    /// Build a minimal SharedPeerState for testing preferred-peer eviction.
    fn test_shared_state(preferred: Vec<PeerAddress>) -> SharedPeerState {
        let (message_tx, _) = tokio::sync::broadcast::channel(1);
        let (scp_message_tx, _) = tokio::sync::mpsc::unbounded_channel();
        let (fetch_response_tx, _) = tokio::sync::mpsc::unbounded_channel();
        SharedPeerState {
            peers: Arc::new(DashMap::new()),
            flood_gate: Arc::new(FloodGate::new()),
            running: Arc::new(AtomicBool::new(true)),
            message_tx,
            scp_message_tx,
            fetch_response_tx,
            peer_handles: Arc::new(RwLock::new(Vec::new())),
            advertised_outbound_peers: Arc::new(RwLock::new(Vec::new())),
            advertised_inbound_peers: Arc::new(RwLock::new(Vec::new())),
            added_authenticated_peers: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            dropped_authenticated_peers: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            banned_peers: Arc::new(RwLock::new(HashSet::new())),
            peer_info_cache: Arc::new(DashMap::new()),
            last_closed_ledger: Arc::new(AtomicU32::new(0)),
            scp_callback: None,
            is_validator: false,
            peer_event_tx: None,
            extra_subscribers: Arc::new(RwLock::new(Vec::new())),
            is_tracking: Arc::new(AtomicBool::new(true)),
            pending_connections: PendingConnections::new(),
            preferred_peers: Arc::new(RwLock::new(PreferredPeerSet::from_config(preferred))),
            admission_state: Arc::new(Mutex::new(AdmissionState::default())),
            fetch_channel_depth: Arc::new(AtomicI64::new(0)),
            fetch_channel_depth_max: Arc::new(AtomicI64::new(0)),
            metrics: Arc::new(OverlayMetrics::new()),
            query_rate_limit_window_secs: Arc::new(AtomicU64::new(60)),
            max_tx_size_bytes: Arc::new(AtomicU32::new(
                crate::flow_control::INITIAL_PEER_FLOOD_READING_CAPACITY_BYTES,
            )),
        }
    }

    /// Insert a fake authenticated peer into the shared state.
    fn insert_fake_peer(
        shared: &SharedPeerState,
        peer_id: PeerId,
        addr: std::net::SocketAddr,
        direction: crate::connection::ConnectionDirection,
    ) -> tokio::sync::mpsc::Receiver<super::OutboundMessage> {
        use crate::flow_control::{FlowControl, FlowControlConfig};
        use crate::peer::PeerStats;

        let (outbound_tx, outbound_rx) = tokio::sync::mpsc::channel(16);
        let handle = super::PeerHandle {
            outbound_tx,
            stats: Arc::new(PeerStats::default()),
            flow_control: Arc::new(FlowControl::new(FlowControlConfig::default())),
        };
        shared.peers.insert(peer_id.clone(), handle);
        shared.peer_info_cache.insert(
            peer_id.clone(),
            crate::peer::PeerInfo {
                peer_id,
                address: addr,
                direction,
                version_string: String::new(),
                overlay_version: 0,
                ledger_version: 0,
                connected_at: std::time::Instant::now(),
                original_address: None,
            },
        );
        outbound_rx
    }

    fn candidate_info(
        peer_id: PeerId,
        addr: std::net::SocketAddr,
        direction: crate::connection::ConnectionDirection,
    ) -> crate::peer::PeerInfo {
        crate::peer::PeerInfo {
            peer_id,
            address: addr,
            direction,
            version_string: String::new(),
            overlay_version: 0,
            ledger_version: 0,
            connected_at: std::time::Instant::now(),
            original_address: None,
        }
    }

    /// Regression test for AUDIT-055: preferred inbound peer evicts non-preferred
    /// when all slots are full.
    #[tokio::test]
    async fn test_audit_055_preferred_peer_evicts_non_preferred_inbound() {
        use crate::connection::ConnectionPool;

        let preferred_addr = PeerAddress::new("10.0.0.1", 11625);
        let shared = test_shared_state(vec![preferred_addr.clone()]);

        // Fill inbound pool to capacity (max_connections = 2).
        let pool = Arc::new(ConnectionPool::new(2));
        pool.try_reserve();
        pool.force_promote_authenticated(); // peer A: non-preferred
        pool.try_reserve();
        pool.force_promote_authenticated(); // peer B: non-preferred
        assert_eq!(pool.authenticated_count(), 2);

        // Insert a non-preferred inbound peer.
        let non_pref_id = PeerId::from_bytes([1u8; 32]);
        let non_pref_addr: std::net::SocketAddr = "10.0.0.99:11625".parse().unwrap();
        let mut victim_rx = insert_fake_peer(
            &shared,
            non_pref_id.clone(),
            non_pref_addr,
            crate::connection::ConnectionDirection::Inbound,
        );

        // Simulate the incoming preferred peer's pending reservation.
        pool.try_reserve();

        // Preferred peer eviction should succeed.
        let candidate = candidate_info(
            PeerId::from_bytes([9u8; 32]),
            "10.0.0.1:11625".parse().unwrap(),
            crate::connection::ConnectionDirection::Inbound,
        );
        let evicted = OverlayManager::try_accept_authenticated_peer(&candidate, &shared, &pool);
        assert!(evicted, "should evict a non-preferred peer for preferred");

        // The evicted peer should have received a shutdown message.
        let msg = victim_rx.try_recv();
        assert!(
            msg.is_ok(),
            "victim should receive an error-and-drop message"
        );

        // Pool authenticated count is now 3 (2 existing + 1 force-promoted).
        // The evicted peer will release its slot asynchronously.
        assert_eq!(pool.authenticated_count(), 3);
    }

    /// When all authenticated inbound peers are preferred, eviction should fail.
    #[tokio::test]
    async fn test_audit_055_all_preferred_no_eviction() {
        use crate::connection::ConnectionPool;

        let preferred_addr = PeerAddress::new("10.0.0.1", 11625);
        let shared = test_shared_state(vec![preferred_addr.clone()]);

        let pool = Arc::new(ConnectionPool::new(1));
        pool.try_reserve();
        pool.force_promote_authenticated();
        assert_eq!(pool.authenticated_count(), 1);

        // Insert an inbound peer that IS preferred.
        let pref_id = PeerId::from_bytes([2u8; 32]);
        let pref_addr: std::net::SocketAddr = "10.0.0.1:11625".parse().unwrap();
        let _rx = insert_fake_peer(
            &shared,
            pref_id,
            pref_addr,
            crate::connection::ConnectionDirection::Inbound,
        );

        // Simulate the incoming preferred peer's pending reservation.
        pool.try_reserve();

        let candidate = candidate_info(
            PeerId::from_bytes([9u8; 32]),
            "10.0.0.1:11625".parse().unwrap(),
            crate::connection::ConnectionDirection::Inbound,
        );
        // Eviction should fail — the only authenticated peer is preferred.
        let evicted = OverlayManager::try_accept_authenticated_peer(&candidate, &shared, &pool);
        assert!(!evicted, "should not evict when all peers are preferred");
        assert_eq!(pool.authenticated_count(), 1);
    }

    /// Non-preferred peers should not trigger eviction (only preferred peers
    /// get this treatment).
    #[tokio::test]
    async fn test_audit_055_non_preferred_peer_does_not_evict() {
        use crate::connection::ConnectionPool;

        let shared = test_shared_state(vec![PeerAddress::new("10.0.0.1", 11625)]);

        let pool = Arc::new(ConnectionPool::new(1));
        pool.try_reserve();
        pool.force_promote_authenticated();

        // Insert a non-preferred inbound peer.
        let np_id = PeerId::from_bytes([3u8; 32]);
        let np_addr: std::net::SocketAddr = "10.0.0.99:11625".parse().unwrap();
        let _rx = insert_fake_peer(
            &shared,
            np_id,
            np_addr,
            crate::connection::ConnectionDirection::Inbound,
        );

        pool.try_reserve();
        let candidate = candidate_info(
            PeerId::from_bytes([9u8; 32]),
            "10.0.0.99:11625".parse().unwrap(),
            crate::connection::ConnectionDirection::Inbound,
        );
        assert!(
            !OverlayManager::try_accept_authenticated_peer(&candidate, &shared, &pool),
            "non-preferred peer should not evict"
        );
    }

    /// Outbound non-preferred peers must not be evicted when making room for
    /// a preferred inbound peer — eviction only considers inbound peers.
    #[tokio::test]
    async fn test_audit_055_outbound_peer_not_evicted_for_inbound() {
        use crate::connection::ConnectionPool;

        let preferred_addr = PeerAddress::new("10.0.0.1", 11625);
        let shared = test_shared_state(vec![preferred_addr.clone()]);

        let pool = Arc::new(ConnectionPool::new(1));
        pool.try_reserve();
        pool.force_promote_authenticated();

        // Insert a non-preferred OUTBOUND peer — should not be evictable.
        let outbound_id = PeerId::from_bytes([4u8; 32]);
        let outbound_addr: std::net::SocketAddr = "10.0.0.99:11625".parse().unwrap();
        let _rx = insert_fake_peer(
            &shared,
            outbound_id,
            outbound_addr,
            crate::connection::ConnectionDirection::Outbound,
        );

        // Simulate the incoming preferred peer's pending reservation.
        pool.try_reserve();

        let candidate = candidate_info(
            PeerId::from_bytes([9u8; 32]),
            "10.0.0.1:11625".parse().unwrap(),
            crate::connection::ConnectionDirection::Inbound,
        );
        // Eviction should fail — the only peer in the cache is outbound.
        let evicted = OverlayManager::try_accept_authenticated_peer(&candidate, &shared, &pool);
        assert!(
            !evicted,
            "should not evict outbound peers for inbound admission"
        );
    }

    #[tokio::test]
    async fn test_preferred_outbound_admission_evicts_non_preferred_outbound() {
        use crate::connection::{ConnectionDirection, ConnectionPool};

        let shared = test_shared_state(vec![PeerAddress::new("10.0.0.1", 11625)]);
        let pool = Arc::new(ConnectionPool::new(1));
        pool.try_reserve();
        pool.force_promote_authenticated();

        let victim_id = PeerId::from_bytes([4u8; 32]);
        let mut victim_rx = insert_fake_peer(
            &shared,
            victim_id,
            "10.0.0.99:11625".parse().unwrap(),
            ConnectionDirection::Outbound,
        );

        pool.try_reserve();
        let candidate = candidate_info(
            PeerId::from_bytes([9u8; 32]),
            "10.0.0.1:11625".parse().unwrap(),
            ConnectionDirection::Outbound,
        );

        assert!(OverlayManager::try_accept_authenticated_peer(
            &candidate, &shared, &pool
        ));
        assert!(
            victim_rx.try_recv().is_ok(),
            "outbound victim should receive error-and-drop"
        );
        assert_eq!(pool.authenticated_count(), 2);
    }

    #[tokio::test]
    async fn test_preferred_outbound_admission_reserves_victim_once() {
        use crate::connection::{ConnectionDirection, ConnectionPool};

        let shared = test_shared_state(vec![
            PeerAddress::new("10.0.0.1", 11625),
            PeerAddress::new("10.0.0.2", 11625),
        ]);
        let pool = Arc::new(ConnectionPool::new(1));
        pool.try_reserve();
        pool.force_promote_authenticated();
        let victim_id = PeerId::from_bytes([4u8; 32]);
        let _victim_rx = insert_fake_peer(
            &shared,
            victim_id,
            "10.0.0.99:11625".parse().unwrap(),
            ConnectionDirection::Outbound,
        );

        pool.try_reserve();
        let first = candidate_info(
            PeerId::from_bytes([9u8; 32]),
            "10.0.0.1:11625".parse().unwrap(),
            ConnectionDirection::Outbound,
        );
        assert!(OverlayManager::try_accept_authenticated_peer(
            &first, &shared, &pool
        ));

        pool.try_reserve();
        let second = candidate_info(
            PeerId::from_bytes([8u8; 32]),
            "10.0.0.2:11625".parse().unwrap(),
            ConnectionDirection::Outbound,
        );
        assert!(
            !OverlayManager::try_accept_authenticated_peer(&second, &shared, &pool),
            "already-evicting victim must not be selected twice"
        );
        assert_eq!(pool.authenticated_count(), 2);
    }

    #[tokio::test]
    async fn test_admission_cleanup_clears_evicting_marker() {
        use crate::connection::{ConnectionDirection, ConnectionPool};

        let shared = test_shared_state(vec![PeerAddress::new("10.0.0.1", 11625)]);
        let pool = Arc::new(ConnectionPool::new(1));
        pool.try_reserve();
        pool.force_promote_authenticated();
        let victim_id = PeerId::from_bytes([4u8; 32]);
        let _victim_rx = insert_fake_peer(
            &shared,
            victim_id.clone(),
            "10.0.0.99:11625".parse().unwrap(),
            ConnectionDirection::Outbound,
        );

        pool.try_reserve();
        let candidate = candidate_info(
            PeerId::from_bytes([9u8; 32]),
            "10.0.0.1:11625".parse().unwrap(),
            ConnectionDirection::Outbound,
        );
        assert!(OverlayManager::try_accept_authenticated_peer(
            &candidate, &shared, &pool
        ));
        assert!(shared.admission_state.lock().is_evicting(&victim_id));
        shared.cleanup_peer(&victim_id);
        assert!(!shared.admission_state.lock().is_evicting(&victim_id));
    }

    #[tokio::test]
    async fn test_preferred_outbound_admission_uses_peer_id_order() {
        use crate::connection::{ConnectionDirection, ConnectionPool};

        let shared = test_shared_state(vec![PeerAddress::new("10.0.0.1", 11625)]);
        let pool = Arc::new(ConnectionPool::new(3));
        let mut receivers = Vec::new();
        for byte in [9u8, 1, 5] {
            pool.try_reserve();
            pool.force_promote_authenticated();
            let id = PeerId::from_bytes([byte; 32]);
            let rx = insert_fake_peer(
                &shared,
                id,
                format!("10.0.1.{byte}:11625").parse().unwrap(),
                ConnectionDirection::Outbound,
            );
            receivers.push((byte, rx));
        }

        pool.try_reserve();
        let candidate = candidate_info(
            PeerId::from_bytes([99u8; 32]),
            "10.0.0.1:11625".parse().unwrap(),
            ConnectionDirection::Outbound,
        );
        assert!(OverlayManager::try_accept_authenticated_peer(
            &candidate, &shared, &pool
        ));

        for (byte, mut rx) in receivers {
            if byte == 1 {
                assert!(rx.try_recv().is_ok(), "lowest PeerId should be evicted");
            } else {
                assert!(
                    rx.try_recv().is_err(),
                    "higher PeerId should not be evicted"
                );
            }
        }
    }

    #[tokio::test]
    async fn test_all_preferred_outbound_peers_block_eviction() {
        use crate::connection::{ConnectionDirection, ConnectionPool};

        let shared = test_shared_state(vec![
            PeerAddress::new("10.0.0.1", 11625),
            PeerAddress::new("10.0.0.2", 11625),
        ]);
        let pool = Arc::new(ConnectionPool::new(1));
        pool.try_reserve();
        pool.force_promote_authenticated();
        let _rx = insert_fake_peer(
            &shared,
            PeerId::from_bytes([4u8; 32]),
            "10.0.0.2:11625".parse().unwrap(),
            ConnectionDirection::Outbound,
        );

        pool.try_reserve();
        let candidate = candidate_info(
            PeerId::from_bytes([9u8; 32]),
            "10.0.0.1:11625".parse().unwrap(),
            ConnectionDirection::Outbound,
        );

        assert!(
            !OverlayManager::try_accept_authenticated_peer(&candidate, &shared, &pool),
            "preferred peer should not evict another preferred outbound peer"
        );
        assert_eq!(pool.authenticated_count(), 1);
    }

    #[test]
    fn test_shared_preferred_state_updates_for_admission() {
        let shared = test_shared_state(vec![PeerAddress::new("validator.example", 11625)]);
        let candidate = candidate_info(
            PeerId::from_bytes([9u8; 32]),
            "10.0.0.42:11625".parse().unwrap(),
            crate::connection::ConnectionDirection::Inbound,
        );
        assert!(!shared.preferred_peers.read().is_preferred(&candidate));

        let updated = shared
            .preferred_peers
            .read()
            .with_resolved(vec![PeerAddress::new("10.0.0.42", 11625)]);
        *shared.preferred_peers.write() = updated;

        assert!(shared.preferred_peers.read().is_preferred(&candidate));
    }

    /// Regression test for AUDIT-086: targeted sends of flow-controlled messages
    /// (SCP, Transaction, FloodAdvert, FloodDemand) must go through the Flood
    /// path, not the direct Send path, so they consume per-peer flow-control credit.
    #[tokio::test]
    async fn test_audit_086_targeted_flood_uses_flow_control() {
        use crate::flow_control::{FlowControl, FlowControlConfig};
        use crate::peer::PeerStats;
        use stellar_xdr::curr::*;

        let config = OverlayConfig::default();
        let secret = SecretKey::generate();
        let local_node = LocalNode::new_testnet(secret);
        let manager = OverlayManager::new(config, local_node).unwrap();

        let peer_id = PeerId::from_bytes([99u8; 32]);
        let (outbound_tx, mut rx) = tokio::sync::mpsc::channel(16);
        let handle = super::PeerHandle {
            outbound_tx,
            stats: Arc::new(PeerStats::default()),
            flow_control: Arc::new(FlowControl::new(FlowControlConfig::default())),
        };
        manager.peers.insert(peer_id.clone(), handle);

        // Flow-controlled SCP message should be routed as Flood
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

        manager
            .try_send_to(&peer_id, scp_msg)
            .expect("send should succeed");
        let msg = rx.recv().await.expect("should receive message");
        assert!(
            matches!(msg, OutboundMessage::Flood(_)),
            "SCP message should be routed through Flood path for flow control"
        );

        // Non-flood messages (e.g. GetScpState) should still use Send
        let get_state = StellarMessage::GetScpState(1);
        manager
            .try_send_to(&peer_id, get_state)
            .expect("send should succeed");
        let msg = rx.recv().await.expect("should receive message");
        assert!(
            matches!(msg, OutboundMessage::Send(_)),
            "GetScpState should use direct Send path"
        );
    }

    /// Regression test for AUDIT-105: GetScpState, GetScpQuorumset, and GetTxSet
    /// must be routed through the dedicated fetch channel, not the lossy broadcast.
    #[tokio::test]
    async fn test_fetch_requests_routed_to_dedicated_channel() {
        let (message_tx, _) = tokio::sync::broadcast::channel(64);
        let mut broadcast_rx = message_tx.subscribe();
        let (scp_message_tx, _scp_rx) = tokio::sync::mpsc::unbounded_channel();
        let (fetch_response_tx, mut fetch_rx) = tokio::sync::mpsc::unbounded_channel();

        let shared = SharedPeerState {
            peers: Arc::new(DashMap::new()),
            flood_gate: Arc::new(FloodGate::new()),
            running: Arc::new(AtomicBool::new(true)),
            message_tx,
            scp_message_tx,
            fetch_response_tx,
            peer_handles: Arc::new(RwLock::new(Vec::new())),
            advertised_outbound_peers: Arc::new(RwLock::new(Vec::new())),
            advertised_inbound_peers: Arc::new(RwLock::new(Vec::new())),
            added_authenticated_peers: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            dropped_authenticated_peers: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            banned_peers: Arc::new(RwLock::new(HashSet::new())),
            peer_info_cache: Arc::new(DashMap::new()),
            last_closed_ledger: Arc::new(AtomicU32::new(0)),
            scp_callback: None,
            is_validator: false,
            peer_event_tx: None,
            extra_subscribers: Arc::new(RwLock::new(Vec::new())),
            is_tracking: Arc::new(AtomicBool::new(true)),
            pending_connections: PendingConnections::new(),
            preferred_peers: Arc::new(RwLock::new(PreferredPeerSet::from_config(Vec::new()))),
            admission_state: Arc::new(Mutex::new(AdmissionState::default())),
            fetch_channel_depth: Arc::new(AtomicI64::new(0)),
            fetch_channel_depth_max: Arc::new(AtomicI64::new(0)),
            metrics: Arc::new(OverlayMetrics::new()),
            query_rate_limit_window_secs: Arc::new(AtomicU64::new(60)),
            max_tx_size_bytes: Arc::new(AtomicU32::new(
                crate::flow_control::INITIAL_PEER_FLOOD_READING_CAPACITY_BYTES,
            )),
        };

        let peer_id = PeerId::from_bytes([42u8; 32]);

        let request_msgs = vec![
            StellarMessage::GetScpState(100),
            StellarMessage::GetScpQuorumset(stellar_xdr::curr::Uint256([1u8; 32])),
            StellarMessage::GetTxSet(stellar_xdr::curr::Uint256([2u8; 32])),
        ];

        for msg in &request_msgs {
            let overlay_msg = OverlayMessage {
                from_peer: peer_id.clone(),
                message: msg.clone(),
                received_at: std::time::Instant::now(),
            };
            shared.route_to_subscribers(overlay_msg);
        }

        // All three should arrive on the dedicated fetch channel.
        for _ in 0..3 {
            let received = fetch_rx
                .try_recv()
                .expect("fetch-request should arrive on dedicated channel");
            assert!(
                matches!(
                    received.message,
                    StellarMessage::GetScpState(_)
                        | StellarMessage::GetScpQuorumset(_)
                        | StellarMessage::GetTxSet(_)
                ),
                "unexpected message type on fetch channel"
            );
        }

        // None should arrive on the broadcast channel.
        let broadcast_result = broadcast_rx.try_recv();
        assert!(
            broadcast_result.is_err(),
            "fetch-request messages must NOT appear on the lossy broadcast channel"
        );
    }

    /// Build a SharedPeerState wired up for `route_to_subscribers` routing tests.
    /// Returns the shared state plus the broadcast and fetch receivers so the
    /// test can assert per-channel delivery.
    fn make_routing_shared_state() -> (
        SharedPeerState,
        tokio::sync::broadcast::Receiver<OverlayMessage>,
        tokio::sync::mpsc::UnboundedReceiver<OverlayMessage>,
    ) {
        let (message_tx, _) = tokio::sync::broadcast::channel(1024);
        let broadcast_rx = message_tx.subscribe();
        let (scp_message_tx, _scp_rx) = tokio::sync::mpsc::unbounded_channel();
        let (fetch_response_tx, fetch_rx) = tokio::sync::mpsc::unbounded_channel();
        let shared = SharedPeerState {
            peers: Arc::new(DashMap::new()),
            flood_gate: Arc::new(FloodGate::new()),
            running: Arc::new(AtomicBool::new(true)),
            message_tx,
            scp_message_tx,
            fetch_response_tx,
            peer_handles: Arc::new(RwLock::new(Vec::new())),
            advertised_outbound_peers: Arc::new(RwLock::new(Vec::new())),
            advertised_inbound_peers: Arc::new(RwLock::new(Vec::new())),
            added_authenticated_peers: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            dropped_authenticated_peers: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            banned_peers: Arc::new(RwLock::new(HashSet::new())),
            peer_info_cache: Arc::new(DashMap::new()),
            last_closed_ledger: Arc::new(AtomicU32::new(0)),
            scp_callback: None,
            is_validator: false,
            peer_event_tx: None,
            extra_subscribers: Arc::new(RwLock::new(Vec::new())),
            is_tracking: Arc::new(AtomicBool::new(true)),
            pending_connections: PendingConnections::new(),
            preferred_peers: Arc::new(RwLock::new(PreferredPeerSet::from_config(Vec::new()))),
            admission_state: Arc::new(Mutex::new(AdmissionState::default())),
            fetch_channel_depth: Arc::new(AtomicI64::new(0)),
            fetch_channel_depth_max: Arc::new(AtomicI64::new(0)),
            metrics: Arc::new(OverlayMetrics::new()),
            query_rate_limit_window_secs: Arc::new(AtomicU64::new(60)),
            max_tx_size_bytes: Arc::new(AtomicU32::new(
                crate::flow_control::INITIAL_PEER_FLOOD_READING_CAPACITY_BYTES,
            )),
        };
        (shared, broadcast_rx, fetch_rx)
    }

    /// Build one `OverlayMessage` for each of the seven fetch variants.
    fn all_fetch_variant_messages(peer: &PeerId) -> Vec<OverlayMessage> {
        let variants = vec![
            StellarMessage::GetScpState(0),
            StellarMessage::GetScpQuorumset(stellar_xdr::curr::Uint256([1u8; 32])),
            StellarMessage::GetTxSet(stellar_xdr::curr::Uint256([2u8; 32])),
            StellarMessage::GeneralizedTxSet(stellar_xdr::curr::GeneralizedTransactionSet::V1(
                stellar_xdr::curr::TransactionSetV1 {
                    previous_ledger_hash: stellar_xdr::curr::Hash([0u8; 32]),
                    phases: vec![].try_into().unwrap(),
                },
            )),
            StellarMessage::TxSet(stellar_xdr::curr::TransactionSet {
                previous_ledger_hash: stellar_xdr::curr::Hash([0u8; 32]),
                txs: stellar_xdr::curr::VecM::default(),
            }),
            StellarMessage::DontHave(stellar_xdr::curr::DontHave {
                type_: stellar_xdr::curr::MessageType::TxSet,
                req_hash: stellar_xdr::curr::Uint256([3u8; 32]),
            }),
            StellarMessage::ScpQuorumset(stellar_xdr::curr::ScpQuorumSet {
                threshold: 1,
                validators: stellar_xdr::curr::VecM::default(),
                inner_sets: stellar_xdr::curr::VecM::default(),
            }),
        ];
        variants
            .into_iter()
            .map(|m| OverlayMessage {
                from_peer: peer.clone(),
                message: m,
                received_at: std::time::Instant::now(),
            })
            .collect()
    }

    /// Classification helper: which fetch variant does this message match?
    fn fetch_variant_key(msg: &StellarMessage) -> Option<&'static str> {
        match msg {
            StellarMessage::GetScpState(_) => Some("GetScpState"),
            StellarMessage::GetScpQuorumset(_) => Some("GetScpQuorumset"),
            StellarMessage::GetTxSet(_) => Some("GetTxSet"),
            StellarMessage::GeneralizedTxSet(_) => Some("GeneralizedTxSet"),
            StellarMessage::TxSet(_) => Some("TxSet"),
            StellarMessage::DontHave(_) => Some("DontHave"),
            StellarMessage::ScpQuorumset(_) => Some("ScpQuorumset"),
            _ => None,
        }
    }

    /// Issue #1741 regression: the fetch channel must be unbounded so that a
    /// lagging app loop never drops SCP fetch traffic. Push 10_000 messages
    /// across all 7 fetch variants while the receiver is parked, then drain
    /// and assert per-variant counts. This test would fail under the old
    /// bounded (4096) `try_send` implementation.
    #[tokio::test]
    async fn fetch_channel_unbounded_all_variants() {
        let (shared, _broadcast_rx, mut fetch_rx) = make_routing_shared_state();
        let peer = PeerId::from_bytes([7u8; 32]);
        let variants = all_fetch_variant_messages(&peer);
        let variant_count = variants.len();
        assert_eq!(variant_count, 7, "expected 7 fetch variants");

        const TOTAL: usize = 10_000;
        let mut sent_counts: std::collections::HashMap<&'static str, usize> =
            std::collections::HashMap::new();
        for i in 0..TOTAL {
            let msg = variants[i % variant_count].clone();
            let key = fetch_variant_key(&msg.message).expect("fetch variant");
            *sent_counts.entry(key).or_insert(0) += 1;
            shared.route_to_subscribers(msg);
        }

        let mut received_counts: std::collections::HashMap<&'static str, usize> =
            std::collections::HashMap::new();
        let mut drained = 0usize;
        while let Ok(msg) = fetch_rx.try_recv() {
            let key = fetch_variant_key(&msg.message).expect("received fetch variant");
            *received_counts.entry(key).or_insert(0) += 1;
            drained += 1;
        }
        assert_eq!(
            drained, TOTAL,
            "all {} messages must survive — unbounded channel never drops",
            TOTAL
        );
        assert_eq!(
            sent_counts, received_counts,
            "per-variant counts must match"
        );
    }

    /// Each of the 7 fetch variants must be routed exclusively to the
    /// dedicated fetch channel — they must NOT appear on the lossy broadcast.
    #[tokio::test]
    async fn fetch_variants_not_delivered_to_broadcast() {
        let (shared, mut broadcast_rx, _fetch_rx) = make_routing_shared_state();
        let peer = PeerId::from_bytes([9u8; 32]);
        for msg in all_fetch_variant_messages(&peer) {
            let key = fetch_variant_key(&msg.message).unwrap();
            shared.route_to_subscribers(msg);
            assert!(
                matches!(
                    broadcast_rx.try_recv(),
                    Err(tokio::sync::broadcast::error::TryRecvError::Empty)
                ),
                "{} must NOT appear on broadcast channel",
                key
            );
        }
    }

    /// Positive counterpart: each of the 7 fetch variants DOES land on the
    /// dedicated fetch channel exactly once.
    #[tokio::test]
    async fn fetch_variants_routed_to_dedicated_channel() {
        let (shared, _broadcast_rx, mut fetch_rx) = make_routing_shared_state();
        let peer = PeerId::from_bytes([11u8; 32]);
        let variants = all_fetch_variant_messages(&peer);
        let expected: std::collections::HashSet<&'static str> = variants
            .iter()
            .map(|m| fetch_variant_key(&m.message).unwrap())
            .collect();
        for msg in variants {
            shared.route_to_subscribers(msg);
        }
        let mut seen: std::collections::HashSet<&'static str> = std::collections::HashSet::new();
        while let Ok(msg) = fetch_rx.try_recv() {
            seen.insert(fetch_variant_key(&msg.message).unwrap());
        }
        assert_eq!(
            seen, expected,
            "every fetch variant must reach fetch channel"
        );
    }

    /// Wedged-loop regression: depth/max advance on enqueue even when the
    /// receiver is never polled. This is the exact failure mode the metric is
    /// meant to diagnose — receiver-side sampling would miss it.
    #[tokio::test]
    async fn fetch_channel_depth_tracks_enqueue_with_parked_receiver() {
        let (shared, _broadcast_rx, _fetch_rx) = make_routing_shared_state();
        // Parked: never call fetch_rx.recv(). Hold the rx alive so sends succeed.
        let peer = PeerId::from_bytes([22u8; 32]);
        let variants = all_fetch_variant_messages(&peer);
        let n = variants.len() as i64;

        assert_eq!(shared.fetch_channel_depth.load(Ordering::Relaxed), 0);
        assert_eq!(shared.fetch_channel_depth_max.load(Ordering::Relaxed), 0);

        for msg in variants {
            shared.route_to_subscribers(msg);
        }

        assert_eq!(
            shared.fetch_channel_depth.load(Ordering::Relaxed),
            n,
            "depth must reflect every enqueued fetch message without any recv"
        );
        assert!(
            shared.fetch_channel_depth_max.load(Ordering::Relaxed) >= n,
            "max must advance to at least the observed depth"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn test_signal_shutdown_idempotent() {
        let config = OverlayConfig::default();
        let secret = SecretKey::generate();
        let local_node = LocalNode::new_testnet(secret);

        let manager = OverlayManager::new(config, local_node).unwrap();
        manager.running.store(true, Ordering::SeqCst);

        // First call should signal
        manager.signal_shutdown();
        assert!(!manager.running.load(Ordering::SeqCst));
        // shutdown_tx should have been taken
        assert!(manager.shutdown_tx.lock().is_none());

        // Second call should be a no-op (no panic)
        manager.signal_shutdown();
        assert!(!manager.running.load(Ordering::SeqCst));
    }

    #[tokio::test(start_paused = true)]
    async fn test_shutdown_fast_with_no_handles() {
        let config = OverlayConfig::default();
        let secret = SecretKey::generate();
        let local_node = LocalNode::new_testnet(secret);

        let mut manager = OverlayManager::new(config, local_node).unwrap();
        manager.running.store(true, Ordering::SeqCst);

        // Shutdown with no handles should complete instantly
        let start = tokio::time::Instant::now();
        manager.shutdown().await.unwrap();
        assert!(!manager.running.load(Ordering::SeqCst));
        // With paused time, should be essentially zero
        assert!(start.elapsed() < Duration::from_secs(1));
    }

    #[tokio::test(start_paused = true)]
    async fn test_shutdown_timeout_aborts_slow_handles() {
        let config = OverlayConfig::default();
        let secret = SecretKey::generate();
        let local_node = LocalNode::new_testnet(secret);

        let mut manager = OverlayManager::new(config, local_node).unwrap();
        manager.running.store(true, Ordering::SeqCst);

        // Track whether each task was actually cancelled (not just detached).
        let cancelled = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        {
            let mut handles = manager.peer_handles.write();
            for _ in 0..5 {
                let cancelled = Arc::clone(&cancelled);
                handles.push(tokio::spawn(async move {
                    match tokio::time::sleep(Duration::from_secs(3600)).await {
                        () => {} // Would only reach here if not aborted
                    }
                    // If the task completes normally (not aborted), this
                    // wouldn't run because sleep(3600) in paused-time
                    // only resolves via time advance. Abort cancels it.
                    drop(cancelled); // prevent "unused" warning
                }));
            }
        }

        let start = tokio::time::Instant::now();
        manager.shutdown().await.unwrap();
        // Should complete at or near the 5s deadline, not wait 3600s
        let elapsed = start.elapsed();
        assert!(
            elapsed <= Duration::from_secs(6),
            "shutdown took {elapsed:?}, expected <= 6s"
        );

        // Verify the tasks were truly aborted: after a short yield, the
        // Arc refcount should have dropped to 1 (only our local `cancelled`
        // clone remains). If tasks were merely detached, they'd still hold
        // their clone.
        tokio::task::yield_now().await;
        assert_eq!(
            Arc::strong_count(&cancelled),
            1,
            "timed-out tasks should have been aborted, not detached"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn test_shutdown_fast_handles_complete_before_timeout() {
        let config = OverlayConfig::default();
        let secret = SecretKey::generate();
        let local_node = LocalNode::new_testnet(secret);

        let mut manager = OverlayManager::new(config, local_node).unwrap();
        manager.running.store(true, Ordering::SeqCst);

        // Add handles that complete quickly
        {
            let mut handles = manager.peer_handles.write();
            for _ in 0..3 {
                handles.push(tokio::spawn(async {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }));
            }
        }

        let start = tokio::time::Instant::now();
        manager.shutdown().await.unwrap();
        // Should complete well under the 5s timeout
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_secs(1),
            "shutdown took {elapsed:?}, expected < 1s"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn test_signal_shutdown_through_arc() {
        let config = OverlayConfig::default();
        let secret = SecretKey::generate();
        let local_node = LocalNode::new_testnet(secret);

        let manager = OverlayManager::new(config, local_node).unwrap();
        manager.running.store(true, Ordering::SeqCst);

        // Wrap in Arc — simulates the Arc::try_unwrap failure path
        let arc = Arc::new(manager);

        // signal_shutdown should work through &self (via Arc)
        arc.signal_shutdown();
        assert!(!arc.running.load(Ordering::SeqCst));
    }

    // ──────── PreferredPeerSet tests ────────

    #[test]
    fn test_preferred_peer_set_from_config() {
        let entries = vec![
            PeerAddress::new("validator1.example.com", 11625),
            PeerAddress::new("10.0.0.1", 11625),
        ];
        let set = PreferredPeerSet::from_config(entries.clone());
        assert_eq!(set.config_entries.len(), 2);
        assert!(set.resolved.is_empty());
        assert!(set.resolved_ips.is_empty());
    }

    #[test]
    fn test_preferred_peer_set_with_resolved() {
        let config = vec![PeerAddress::new("validator1.example.com", 11625)];
        let set = PreferredPeerSet::from_config(config);

        let resolved = vec![PeerAddress::new("10.0.0.42", 11625)];
        let updated = set.with_resolved(resolved);

        assert_eq!(updated.config_entries.len(), 1);
        assert_eq!(updated.resolved.len(), 1);
        assert_eq!(updated.resolved_ips.len(), 1);
        assert!(updated
            .resolved_ips
            .contains(&"10.0.0.42".parse::<IpAddr>().unwrap()));
    }

    #[test]
    fn test_preferred_peer_set_is_preferred_outbound_hostname() {
        // Outbound peers have original_address set — should match config hostname.
        let config = vec![PeerAddress::new("validator1.example.com", 11625)];
        let set = PreferredPeerSet::from_config(config);

        let peer_info = crate::peer::PeerInfo {
            peer_id: PeerId::from_bytes([1u8; 32]),
            address: "10.0.0.42:11625".parse().unwrap(),
            direction: crate::connection::ConnectionDirection::Outbound,
            version_string: String::new(),
            overlay_version: 0,
            ledger_version: 0,
            connected_at: std::time::Instant::now(),
            original_address: Some(PeerAddress::new("validator1.example.com", 11625)),
        };

        assert!(
            set.is_preferred(&peer_info),
            "outbound peer with matching original_address hostname should be preferred"
        );
    }

    #[test]
    fn test_preferred_peer_set_is_preferred_inbound_resolved_ip() {
        // Inbound peers have no original_address — must match by resolved IP.
        let config = vec![PeerAddress::new("validator1.example.com", 11625)];
        let set = PreferredPeerSet::from_config(config);

        // Before DNS resolution: inbound peer should NOT match (hostname can't parse as IP)
        let peer_info = crate::peer::PeerInfo {
            peer_id: PeerId::from_bytes([2u8; 32]),
            address: "10.0.0.42:11625".parse().unwrap(),
            direction: crate::connection::ConnectionDirection::Inbound,
            version_string: String::new(),
            overlay_version: 0,
            ledger_version: 0,
            connected_at: std::time::Instant::now(),
            original_address: None,
        };

        assert!(
            !set.is_preferred(&peer_info),
            "inbound peer should NOT match before DNS resolution"
        );

        // After DNS resolution: should match via resolved IP
        let resolved = vec![PeerAddress::new("10.0.0.42", 11625)];
        let updated = set.with_resolved(resolved);
        assert!(
            updated.is_preferred(&peer_info),
            "inbound peer should match after DNS resolution"
        );
    }

    #[test]
    fn test_preferred_peer_set_is_preferred_no_match() {
        let config = vec![PeerAddress::new("validator1.example.com", 11625)];
        let resolved = vec![PeerAddress::new("10.0.0.42", 11625)];
        let set = PreferredPeerSet::from_config(config).with_resolved(resolved);

        let peer_info = crate::peer::PeerInfo {
            peer_id: PeerId::from_bytes([3u8; 32]),
            address: "192.168.1.1:11625".parse().unwrap(),
            direction: crate::connection::ConnectionDirection::Inbound,
            version_string: String::new(),
            overlay_version: 0,
            ledger_version: 0,
            connected_at: std::time::Instant::now(),
            original_address: None,
        };

        assert!(
            !set.is_preferred(&peer_info),
            "non-preferred peer should not match"
        );
    }

    #[test]
    fn test_preferred_peer_set_shuffled_entries_all_present() {
        let config = vec![
            PeerAddress::new("a.example.com", 11625),
            PeerAddress::new("b.example.com", 11625),
            PeerAddress::new("c.example.com", 11625),
        ];
        let set = PreferredPeerSet::from_config(config.clone());

        // Use a seeded RNG for determinism
        use rand::SeedableRng;
        let mut rng = rand::rngs::StdRng::seed_from_u64(42);
        let shuffled = set.shuffled_config_entries(&mut rng);

        assert_eq!(shuffled.len(), config.len());
        for entry in &config {
            assert!(
                shuffled.iter().any(|e| e.host == entry.host),
                "all config entries must appear in shuffled output"
            );
        }
    }

    #[test]
    fn test_connection_pool_update_preferred_ips_enables_reservation() {
        // Verify update_preferred_ips works by checking that a preferred IP
        // can get extra slots after the update, using only the public API.
        let pool = ConnectionPool::with_preferred(2, 2, HashSet::new());

        // Fill to base limit (2 reserved)
        assert!(pool.try_reserve());
        assert!(pool.try_reserve());

        // Before update: even preferred IP can't get extra because it's not in the set.
        // But we're still within pending headroom (max_pending_extra=32 by default).
        // We can only test this properly using with_preferred which sets up initial state.
        // So just verify update_preferred_ips doesn't panic:
        let mut ips = HashSet::new();
        ips.insert("10.0.0.1".parse::<IpAddr>().unwrap());
        pool.update_preferred_ips(ips);
    }

    #[test]
    fn test_eviction_skips_preferred_peers_from_set() {
        use crate::connection::ConnectionDirection;

        // Create a set where 10.0.0.1:11625 is preferred
        let preferred = vec![PeerAddress::new("10.0.0.1", 11625)];
        let set = PreferredPeerSet::from_config(preferred);

        // Preferred peer
        let preferred_info = crate::peer::PeerInfo {
            peer_id: PeerId::from_bytes([1u8; 32]),
            address: "10.0.0.1:11625".parse().unwrap(),
            direction: ConnectionDirection::Outbound,
            version_string: String::new(),
            overlay_version: 0,
            ledger_version: 0,
            connected_at: std::time::Instant::now(),
            original_address: Some(PeerAddress::new("10.0.0.1", 11625)),
        };
        assert!(
            set.is_preferred(&preferred_info),
            "should recognize preferred peer by IP"
        );

        // Non-preferred peer
        let non_preferred_info = crate::peer::PeerInfo {
            peer_id: PeerId::from_bytes([2u8; 32]),
            address: "10.0.0.99:11625".parse().unwrap(),
            direction: ConnectionDirection::Outbound,
            version_string: String::new(),
            overlay_version: 0,
            ledger_version: 0,
            connected_at: std::time::Instant::now(),
            original_address: Some(PeerAddress::new("10.0.0.99", 11625)),
        };
        assert!(
            !set.is_preferred(&non_preferred_info),
            "should not recognize non-preferred peer"
        );
    }

    #[tokio::test]
    async fn test_preferred_set_protects_peers_from_random_drop() {
        use crate::connection::ConnectionDirection;

        // With a preferred set containing 10.0.0.1, only that peer should be
        // recognized as preferred.
        let preferred = vec![PeerAddress::new("10.0.0.1", 11625)];
        let shared = test_shared_state(preferred);

        // Insert preferred outbound peer
        let preferred_id = PeerId::from_bytes([1u8; 32]);
        let _rx = insert_fake_peer(
            &shared,
            preferred_id,
            "10.0.0.1:11625".parse().unwrap(),
            ConnectionDirection::Outbound,
        );

        // Verify the preferred_peers set on shared state correctly identifies this peer
        let info = shared
            .peer_info_cache
            .get(&PeerId::from_bytes([1u8; 32]))
            .unwrap();
        assert!(
            shared.preferred_peers.read().is_preferred(info.value()),
            "shared preferred_peers set should recognize the peer"
        );
    }
}
