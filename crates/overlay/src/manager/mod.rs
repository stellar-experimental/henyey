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
    peer::{PeerInfo, PeerStats, PeerStatsSnapshot},
    LocalNode, OverlayConfig, OverlayError, PeerAddress, PeerEvent, PeerId, Result,
};
use dashmap::DashMap;
use henyey_common::LedgerSeq;
use parking_lot::RwLock;
use rand::seq::SliceRandom;
use std::collections::HashSet;
use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;
use stellar_xdr::curr::{
    PeerAddress as XdrPeerAddress, PeerAddressIp, StellarMessage, Uint256, VecM,
};
use tokio::sync::{broadcast, mpsc, Mutex as TokioMutex};
use tokio::task::JoinHandle;
use tracing::{debug, error, info, trace};

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

/// Buffer size for the dedicated fetch-response mpsc channel
/// (GetTxSet / GetScpQuorumSet replies).
const FETCH_RESPONSE_CHANNEL_SIZE: usize = 4096;

/// Maximum number of peer addresses included in a single PEERS message.
///
/// Matches stellar-core's limit of 50 addresses per Peers message
/// (see `Peer::recvPeers` in Peer.cpp).
const MAX_PEERS_PER_MESSAGE: usize = 50;

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
    pub(super) fetch_response_tx: mpsc::Sender<OverlayMessage>,
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
    /// Preferred peer addresses from config, used for inbound admission priority.
    /// Matches stellar-core's `mConfigurationPreferredPeers` in `isPreferred`.
    pub(super) preferred_peers: Arc<Vec<PeerAddress>>,
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
    /// Shutdown signal.
    pub(super) shutdown_tx: Option<broadcast::Sender<()>>,
    /// Cache of peer info for connected peers (lock-free access).
    pub(super) peer_info_cache: Arc<DashMap<PeerId, PeerInfo>>,
    /// Dedicated unbounded channel for SCP messages.
    /// SCP messages are consensus-critical and must never be dropped.
    /// Mainnet generates ~24 validators * multiple SCP rounds per slot,
    /// which can overwhelm bounded channels during catchup.
    pub(super) scp_message_tx: mpsc::UnboundedSender<OverlayMessage>,
    /// Receiver end of the SCP channel. Taken once via `subscribe_scp()`.
    scp_message_rx: Arc<TokioMutex<Option<mpsc::UnboundedReceiver<OverlayMessage>>>>,
    /// Dedicated bounded channel for fetch response messages.
    /// Routes GeneralizedTxSet, TxSet, DontHave, and ScpQuorumset through
    /// a dedicated channel. Buffer (4096) is generous for fetch responses.
    pub(super) fetch_response_tx: mpsc::Sender<OverlayMessage>,
    /// Receiver end of the fetch response channel. Taken once via `subscribe_fetch_responses()`.
    fetch_response_rx: Arc<TokioMutex<Option<mpsc::Receiver<OverlayMessage>>>>,
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
}

impl OverlayManager {
    #[cfg(test)]
    pub(super) fn initial_send_more_grant(
        config: &crate::flow_control::FlowControlConfig,
    ) -> (u32, u32) {
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
    // SECURITY: subscriber count bounded by internal callers; no external input
    pub fn new_with_connection_factory(
        config: OverlayConfig,
        local_node: LocalNode,
        connection_factory: Arc<dyn ConnectionFactory>,
    ) -> Result<Self> {
        // Broadcast channel for non-critical overlay messages (TX floods, etc.).
        // SCP and fetch-response messages bypass this channel via dedicated mpsc
        // channels, so the broadcast channel only carries remaining message types.
        let (message_tx, _) = broadcast::channel(BROADCAST_CHANNEL_SIZE);
        let (shutdown_tx, _) = broadcast::channel(1);
        let (scp_message_tx, scp_message_rx) = mpsc::unbounded_channel();
        let (fetch_response_tx, fetch_response_rx) = mpsc::channel(FETCH_RESPONSE_CHANNEL_SIZE);

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
            is_tracking: Arc::new(AtomicBool::new(false)),
            connection_factory,
            pending_connections: PendingConnections::new(),
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
            preferred_peers: Arc::new(self.config.preferred_peers.clone()),
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
            let lcl = LedgerSeq::new(self.last_closed_ledger.load(Ordering::Relaxed));
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
                if let Some(ref forward) = forward_peers {
                    if !forward.contains(peer_id) {
                        return None;
                    }
                }
                Some(peer_id.clone())
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
    pub fn clear_ledgers_below(&self, ledger_seq: LedgerSeq, _lcl_seq: u32) {
        self.last_closed_ledger
            .store(ledger_seq.get(), Ordering::Relaxed);
        self.flood_gate.clear_below(ledger_seq);
        trace!(
            ledger_seq = ledger_seq.get(),
            "Cleared overlay state below ledger"
        );
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
    pub async fn request_scp_state(&self, ledger_seq: LedgerSeq) -> Result<usize> {
        let message = StellarMessage::GetScpState(ledger_seq.get());
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

        // Send shutdown to all peer tasks via their channels.
        // Use try_send to avoid blocking: the `running` flag (set to false
        // above) ensures peer_loops exit on their next iteration regardless.
        let senders: Vec<_> = self
            .peers
            .iter()
            .map(|e| e.value().outbound_tx.clone())
            .collect();
        for tx in senders {
            let _ = tx.try_send(OutboundMessage::Shutdown);
        }
        self.peers.clear();

        // Wait for tasks to complete
        if let Some(handle) = self.listener_handle.take() {
            let _ = handle.await;
        }
        if let Some(handle) = self.connector_handle.take() {
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
        manager.flood_gate.record_seen(hash1, None, 100.into());
        manager.flood_gate.record_seen(hash2, None, 100.into());
        assert_eq!(manager.flood_stats().seen_count, 2);

        // clear_ledgers_below should not remove entries at or above the threshold
        manager.clear_ledgers_below(100.into(), 100);
        assert_eq!(manager.flood_stats().seen_count, 2);

        // clear_ledgers_below with a higher seq removes them
        manager.clear_ledgers_below(101.into(), 101);
        assert_eq!(manager.flood_stats().seen_count, 0);
    }

    #[test]
    fn test_clear_ledgers_below_no_panic_when_empty() {
        let config = OverlayConfig::default();
        let secret = SecretKey::generate();
        let local_node = LocalNode::new_testnet(secret);

        let manager = OverlayManager::new(config, local_node).unwrap();

        // Should not panic with empty flood gate
        manager.clear_ledgers_below(0.into(), 0);
        manager.clear_ledgers_below(100.into(), 50);
        manager.clear_ledgers_below(u32::MAX.into(), u32::MAX);
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
        let (fetch_response_tx, _) = tokio::sync::mpsc::channel(1);
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
            preferred_peers: Arc::new(preferred),
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
        pool.mark_authenticated(); // peer A: non-preferred
        pool.try_reserve();
        pool.mark_authenticated(); // peer B: non-preferred
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

        // Pool is full — normal promotion should fail.
        assert!(!pool.try_promote_to_authenticated());

        // Preferred peer eviction should succeed.
        let evicted = OverlayManager::try_evict_non_preferred_for_inbound(&shared, &pool);
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
        pool.mark_authenticated();
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

        // Eviction should fail — the only authenticated peer is preferred.
        let evicted = OverlayManager::try_evict_non_preferred_for_inbound(&shared, &pool);
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
        pool.mark_authenticated();

        // Insert a non-preferred inbound peer.
        let np_id = PeerId::from_bytes([3u8; 32]);
        let np_addr: std::net::SocketAddr = "10.0.0.99:11625".parse().unwrap();
        let _rx = insert_fake_peer(
            &shared,
            np_id,
            np_addr,
            crate::connection::ConnectionDirection::Inbound,
        );

        // try_promote fails (pool full) and non-preferred peer should NOT evict.
        assert!(!pool.try_promote_to_authenticated());
        // (The caller in handle_accepted_inbound_peer only calls eviction for
        // preferred peers — this test verifies the pool correctly rejects.)
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
        pool.mark_authenticated();

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

        // Eviction should fail — the only peer in the cache is outbound.
        let evicted = OverlayManager::try_evict_non_preferred_for_inbound(&shared, &pool);
        assert!(
            !evicted,
            "should not evict outbound peers for inbound admission"
        );
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
}
