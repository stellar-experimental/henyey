//! Connection management: establishing, accepting, and registering peer connections.
//!
//! Contains `start_listener`, `handle_accepted_inbound_peer`,
//! `connect_outbound_inner`, `run_discovered_peer_connection`, `add_peer`,
//! and related helpers.

use super::peer_loop::make_error_msg;
use super::{OutboundMessage, OverlayManager, SharedPeerState};
use crate::{
    connection::ConnectionPool,
    connection_factory::ConnectionFactory,
    flow_control::{FlowControl, FlowControlConfig},
    peer::{Peer, PeerInfo},
    LocalNode, OverlayError, PeerAddress, PeerEvent, PeerId, PeerType, Result,
};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;
use stellar_xdr::curr::ErrorCode;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use super::PeerHandle;
use parking_lot::RwLock;
use tokio::task::JoinHandle;

/// Buffer size for the per-peer outbound message channel.
///
/// This channel queues messages waiting to be sent to a specific peer.
/// 256 provides enough headroom for bursts without excessive memory use.
const OUTBOUND_MESSAGE_CHANNEL_SIZE: usize = 256;

/// Delay in milliseconds between successive connection attempts when
/// filling outbound slots, to avoid overwhelming the network stack.
const CONNECTION_ATTEMPT_DELAY_MS: u64 = 50;

/// Prune completed JoinHandles from the vec, then push the new one.
/// This prevents unbounded growth of the peer_handles vector.
fn push_peer_handle(handles: &RwLock<Vec<JoinHandle<()>>>, handle: JoinHandle<()>) {
    let mut guard = handles.write();
    guard.retain(|h| !h.is_finished());
    guard.push(handle);
}

impl OverlayManager {
    /// Promote a handshaken peer to authenticated, evicting one non-preferred
    /// peer in the same direction for preferred peers when authenticated slots
    /// are full.
    ///
    /// This is deliberately synchronous: admission state and pool counters are
    /// updated under one short critical section, and no async I/O happens while
    /// holding the admission lock.
    pub(super) fn try_accept_authenticated_peer(
        peer_info: &PeerInfo,
        shared: &SharedPeerState,
        pool: &ConnectionPool,
    ) -> bool {
        let mut admission = shared.admission_state.lock();

        if pool.try_promote_to_authenticated() {
            return true;
        }

        let preferred = shared.preferred_peers.read();
        if !preferred.is_preferred(peer_info) {
            return false;
        }

        let direction = peer_info.direction;
        let mut candidates: Vec<PeerId> = shared
            .peer_info_cache
            .iter()
            .filter_map(|entry| {
                let candidate_id = entry.key();
                let candidate_info = entry.value();
                if candidate_info.direction != direction {
                    return None;
                }
                if preferred.is_preferred(candidate_info) {
                    return None;
                }
                if admission.is_evicting(candidate_id) {
                    return None;
                }
                if !shared.peers.contains_key(candidate_id) {
                    return None;
                }
                Some(candidate_id.clone())
            })
            .collect();
        candidates.sort_by(|left, right| left.as_bytes().cmp(right.as_bytes()));

        for victim_id in candidates {
            if let Some(entry) = shared.peers.get(&victim_id) {
                info!(
                    "Evicting non-preferred {:?} peer {} for preferred peer {}",
                    direction, victim_id, peer_info.peer_id
                );
                admission.mark_evicting(victim_id.clone());
                super::peer_loop::send_error_and_drop(
                    &victim_id,
                    &entry.value().outbound_tx,
                    ErrorCode::Load,
                    "preferred peer selected instead",
                );
                pool.force_promote_authenticated();
                return true;
            }
        }

        false
    }

    async fn reject_authenticated_load(peer: &mut Peer, shared: &SharedPeerState) {
        let err_msg = make_error_msg(ErrorCode::Load, "peer rejected");
        match peer.send(err_msg).await {
            Ok(()) => shared.metrics.messages_written.inc(),
            Err(_) => shared.metrics.errors_write.inc(),
        }
        peer.close().await;
    }

    /// Create a PeerHandle (outbound channel + FlowControl) and atomically
    /// register the peer in the shared maps. Returns the receiver and
    /// FlowControl needed by `run_peer_loop`, or `Err` if a peer with the
    /// same ID is already registered (TOCTOU-safe via `DashMap::entry`).
    pub(super) fn register_peer(
        peer: &Peer,
        peer_id: &PeerId,
        peer_info: PeerInfo,
        shared: &SharedPeerState,
    ) -> std::result::Result<(mpsc::Receiver<OutboundMessage>, Arc<FlowControl>), OverlayError>
    {
        let (outbound_tx, outbound_rx) = mpsc::channel(OUTBOUND_MESSAGE_CHANNEL_SIZE);
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

        // Atomic check-and-insert: prevents TOCTOU race where two concurrent
        // tasks both pass the earlier `contains_key` check and both try to
        // register the same PeerId.
        use dashmap::mapref::entry::Entry;
        match shared.peers.entry(peer_id.clone()) {
            Entry::Occupied(_) => {
                return Err(OverlayError::AlreadyConnected);
            }
            Entry::Vacant(e) => {
                e.insert(peer_handle);
            }
        }
        shared.peer_info_cache.insert(peer_id.clone(), peer_info);
        shared
            .added_authenticated_peers
            .fetch_add(1, Ordering::Relaxed);
        Ok((outbound_rx, flow_control))
    }

    /// Send a PEERS advertisement to a newly accepted inbound peer.
    async fn send_peers_to_inbound_peer(
        peer: &mut Peer,
        peer_id: &PeerId,
        peer_info: &PeerInfo,
        shared: &SharedPeerState,
    ) {
        let outbound_snapshot = shared.advertised_outbound_peers.read().clone();
        let inbound_snapshot = shared.advertised_inbound_peers.read().clone();
        let exclude = PeerAddress::from(peer_info.address);
        if let Some(message) = OverlayManager::build_peers_message(
            &outbound_snapshot,
            &inbound_snapshot,
            Some(&exclude),
        ) {
            if peer.is_ready() {
                match peer.send(message).await {
                    Ok(()) => shared.metrics.messages_written.inc(),
                    Err(e) => {
                        debug!("Failed to send peers to {}: {}", peer_id, e);
                        shared.metrics.errors_write.inc();
                    }
                }
            }
        }
    }

    /// Handle a successfully accepted inbound connection through its full
    /// lifecycle: validation, PEERS message, peer-loop, and cleanup.
    ///
    /// Matches stellar-core `Peer::recvAuth()` (Peer.cpp:1913-1970):
    /// 1. sendAuth + sendPeers (always, even if we will reject)
    /// 2. acceptAuthenticatedPeer — reject with ERR_LOAD if slots are full
    /// 3. Start flow control and peer loop
    pub(super) async fn handle_accepted_inbound_peer(
        mut peer: Peer,
        shared: SharedPeerState,
        pool: Arc<ConnectionPool>,
    ) {
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
            shared.pending_connections.release_peer_id(&peer_id);
            pool.release_pending();
            return;
        }

        let peer_info = peer.info().clone();

        // Always send PEERS before checking slot limits.
        // Matches stellar-core: sendPeers() is called before acceptAuthenticatedPeer().
        // This ensures crawlers and other peers get topology data even when rejected.
        Self::send_peers_to_inbound_peer(&mut peer, &peer_id, &peer_info, &shared).await;

        let is_preferred = shared.preferred_peers.read().is_preferred(&peer_info);
        let promoted = Self::try_accept_authenticated_peer(&peer_info, &shared, &pool);

        if !promoted {
            let reason = if is_preferred {
                "all inbound slots occupied by preferred peers"
            } else {
                "all available slots are taken"
            };
            info!(
                "Inbound authenticated peer {} rejected because {}",
                peer_id, reason
            );
            Self::reject_authenticated_load(&mut peer, &shared).await;
            shared.pending_connections.release_peer_id(&peer_id);
            pool.release_pending();
            return;
        }

        info!("Accepted peer: {}", peer_id);

        shared
            .send_peer_event(PeerEvent::Connected(
                PeerAddress::from(peer_info.address),
                PeerType::Inbound,
            ))
            .await;

        let (outbound_rx, flow_control) =
            match Self::register_peer(&peer, &peer_id, peer_info, &shared) {
                Ok(result) => result,
                Err(_) => {
                    debug!("Rejected duplicate inbound peer {} (race)", peer_id);
                    peer.close().await;
                    shared.pending_connections.release_peer_id(&peer_id);
                    pool.release_authenticated();
                    return;
                }
            };

        // Successfully registered — release the pending reservation.
        shared.pending_connections.release_peer_id(&peer_id);

        Self::run_peer_loop(
            peer_id.clone(),
            peer,
            outbound_rx,
            flow_control,
            shared.clone(),
        )
        .await;

        shared.cleanup_peer(&peer_id);
        pool.release_authenticated();
    }

    /// Start the connection listener.
    pub(super) async fn start_listener(&mut self) -> Result<()> {
        let listener = self
            .connection_factory
            .bind(self.config.listen_port)
            .await?;
        info!("Listening on port {}", self.config.listen_port);

        let shared = self.shared_state();
        let local_node = self.local_node.clone();
        let pool = Arc::clone(&self.inbound_pool);
        let peer_handles = Arc::clone(&self.peer_handles);
        let auth_timeout = self.config.auth_timeout_secs;
        let mut shutdown_rx = self.shutdown_tx.lock().as_ref().unwrap().subscribe();

        let handle = tokio::spawn(async move {
            loop {
                tokio::select! {
                    result = listener.accept() => {
                        match result {
                            Ok(connection) => {
                                let peer_ip = connection.remote_addr().ip();
                                // Reserve a pending slot. We allow the handshake to proceed
                                // even when authenticated slots are full — the actual
                                // authenticated limit is checked in
                                // handle_accepted_inbound_peer after PEERS is sent.
                                // This matches stellar-core, which always completes the
                                // handshake and sends PEERS before rejecting with ERR_LOAD.
                                if !pool.try_reserve_with_ip(Some(peer_ip)) {
                                    warn!("Inbound peer limit reached, rejecting connection from {}", peer_ip);
                                    continue;
                                }

                                let shared = shared.clone();
                                let local_node = local_node.clone();
                                let pool = Arc::clone(&pool);

                                let peer_handle = tokio::spawn(async move {
                                    let remote_addr = connection.remote_addr();
                                    let pending_peer_ids = Arc::clone(&shared.pending_connections.by_peer_id);
                                    match Peer::accept(connection, local_node, auth_timeout, Arc::clone(&shared.banned_peers), pending_peer_ids).await {
                                        Ok(peer) => {
                                            Self::handle_accepted_inbound_peer(peer, shared, pool).await;
                                        }
                                        Err(e) => {
                                            debug!("Failed to accept peer: {}", e);
                                            shared.send_peer_event(PeerEvent::Failed(
                                                PeerAddress::from(remote_addr),
                                                PeerType::Inbound,
                                            )).await;
                                            pool.release_pending();
                                        }
                                    }
                                });

                                push_peer_handle(&peer_handles, peer_handle);
                            }
                            Err(e) => {
                                tracing::error!("Accept error: {}", e);
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

    /// Connect to a discovered peer, run its peer loop, then clean up.
    ///
    /// Used by `add_peer` for background connections discovered via Peers messages.
    async fn connect_to_discovered_peer(
        addr: PeerAddress,
        local_node: LocalNode,
        connect_timeout: u64,
        pool: Arc<ConnectionPool>,
        shared: SharedPeerState,
        connection_factory: Arc<dyn ConnectionFactory>,
    ) {
        // Reserve address slot to prevent duplicate outbound dials.
        let addr_key = format!("{}:{}", addr.host, addr.port);
        if !shared
            .pending_connections
            .try_reserve_address(addr_key.clone())
        {
            debug!(
                "Rejected discovered peer {} — connection already in flight",
                addr
            );
            pool.release_pending();
            return;
        }

        let connection = match connection_factory.connect(&addr, connect_timeout).await {
            Ok(c) => c,
            Err(e) => {
                debug!("Failed to connect to discovered peer {}: {}", addr, e);
                shared
                    .send_peer_event(PeerEvent::Failed(addr.clone(), PeerType::Outbound))
                    .await;
                shared.pending_connections.release_address(&addr_key);
                pool.release_pending();
                return;
            }
        };

        let pending_peer_ids = Some(Arc::clone(&shared.pending_connections.by_peer_id));
        let mut peer = match Peer::connect_with_connection(
            &addr,
            connection,
            local_node,
            connect_timeout,
            pending_peer_ids,
        )
        .await
        {
            Ok(p) => p,
            Err(e) => {
                debug!("Failed to connect to discovered peer {}: {}", addr, e);
                shared
                    .send_peer_event(PeerEvent::Failed(addr.clone(), PeerType::Outbound))
                    .await;
                shared.pending_connections.release_address(&addr_key);
                pool.release_pending();
                return;
            }
        };

        let peer_id = peer.id().clone();
        // Address reservation no longer needed — we have the peer_id now.
        shared.pending_connections.release_address(&addr_key);

        // Reject banned peers (mirrors connect_outbound_inner).
        if shared.banned_peers.read().contains(&peer_id) {
            debug!("Rejected banned discovered peer {} at {}", peer_id, addr);
            peer.close().await;
            shared.pending_connections.release_peer_id(&peer_id);
            pool.release_pending();
            return;
        }

        // Reject peers we're already connected to (mirrors connect_outbound_inner).
        if shared.peers.contains_key(&peer_id) {
            debug!("Rejected duplicate discovered peer {} at {}", peer_id, addr);
            peer.close().await;
            shared.pending_connections.release_peer_id(&peer_id);
            pool.release_pending();
            return;
        }

        let peer_info = peer.info().clone();
        if !Self::try_accept_authenticated_peer(&peer_info, &shared, &pool) {
            info!(
                "Outbound discovered peer {} rejected because all available slots are taken",
                peer_id
            );
            Self::reject_authenticated_load(&mut peer, &shared).await;
            shared.pending_connections.release_peer_id(&peer_id);
            pool.release_pending();
            return;
        }

        info!("Connected to discovered peer: {} at {}", peer_id, addr);
        let (outbound_rx, flow_control) =
            match Self::register_peer(&peer, &peer_id, peer_info, &shared) {
                Ok(result) => result,
                Err(_) => {
                    debug!("Rejected duplicate discovered peer {} (race)", peer_id);
                    peer.close().await;
                    shared.pending_connections.release_peer_id(&peer_id);
                    pool.release_authenticated();
                    return;
                }
            };

        shared.pending_connections.release_peer_id(&peer_id);
        shared
            .send_peer_event(PeerEvent::Connected(addr.clone(), PeerType::Outbound))
            .await;

        // NOTE: Do NOT send PEERS to outbound peers — see Peer.cpp:1225-1230.

        Self::run_peer_loop(
            peer_id.clone(),
            peer,
            outbound_rx,
            flow_control,
            shared.clone(),
        )
        .await;

        shared.cleanup_peer(&peer_id);
        pool.release_authenticated();
    }

    /// Add a peer to connect to.
    ///
    /// This is used for peer discovery when we receive a Peers message.
    /// Returns true if a connection attempt was initiated.
    // SECURITY: dial dedup and queue bounded by max_peer_count config + peer universe
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
        let already_connected = self
            .peer_info_cache
            .iter()
            .any(|entry| entry.value().address.to_string() == target_addr);
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
            Self::connect_to_discovered_peer(
                addr,
                local_node,
                connect_timeout,
                pool,
                shared,
                connection_factory,
            )
            .await;
        });

        push_peer_handle(&peer_handles, peer_handle);

        Ok(true)
    }

    /// Add multiple peers to connect to.
    ///
    /// This is used for peer discovery when we receive a Peers message.
    /// Returns the number of connection attempts initiated.
    // SECURITY: dial dedup and queue bounded by max_peer_count config + peer universe
    pub async fn add_peers(&self, addrs: Vec<PeerAddress>) -> usize {
        // Deduplicate addresses within the batch to prevent repeated dials
        // to the same endpoint. Matches stellar-core's connectToImpl which
        // checks pending connections by address before dialing.
        let mut seen_addrs = std::collections::HashSet::new();
        let unique_addrs: Vec<PeerAddress> = addrs
            .into_iter()
            .filter(|addr| seen_addrs.insert(format!("{}:{}", addr.host, addr.port)))
            .collect();

        let mut added = 0;
        let target_outbound = self.config.target_outbound_peers;
        let mut remaining = target_outbound.saturating_sub(self.outbound_pool.count());
        for addr in unique_addrs {
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
            tokio::time::sleep(Duration::from_millis(CONNECTION_ATTEMPT_DELAY_MS)).await;
        }
        added
    }
}

/// Connect to a peer and spawn its peer loop.
///
/// This is a module-level function (not a method) so it can be called from
/// `tick.rs` without requiring `&self`.
pub(super) async fn connect_to_explicit_peer(
    addr: &PeerAddress,
    local_node: LocalNode,
    timeout_secs: u64,
    pool: Arc<ConnectionPool>,
    shared: SharedPeerState,
    connection_factory: Arc<dyn ConnectionFactory>,
) -> Result<PeerId> {
    // Reserve address slot to prevent duplicate outbound dials.
    let addr_key = format!("{}:{}", addr.host, addr.port);
    if !shared
        .pending_connections
        .try_reserve_address(addr_key.clone())
    {
        pool.release_pending();
        return Err(OverlayError::Internal(format!(
            "connection to {} already in flight",
            addr
        )));
    }

    let connection = match connection_factory.connect(addr, timeout_secs).await {
        Ok(connection) => connection,
        Err(e) => {
            shared.pending_connections.release_address(&addr_key);
            pool.release_pending();
            shared
                .send_peer_event(PeerEvent::Failed(addr.clone(), PeerType::Outbound))
                .await;
            return Err(e);
        }
    };

    let pending_peer_ids = Some(Arc::clone(&shared.pending_connections.by_peer_id));
    let mut peer = match Peer::connect_with_connection(
        addr,
        connection,
        local_node,
        timeout_secs,
        pending_peer_ids,
    )
    .await
    {
        Ok(peer) => peer,
        Err(e) => {
            shared.pending_connections.release_address(&addr_key);
            pool.release_pending();
            shared
                .send_peer_event(PeerEvent::Failed(addr.clone(), PeerType::Outbound))
                .await;
            return Err(e);
        }
    };

    let peer_id = peer.id().clone();
    // Address reservation no longer needed — we have the peer_id now.
    shared.pending_connections.release_address(&addr_key);

    if shared.banned_peers.read().contains(&peer_id) {
        peer.close().await;
        pool.release_pending();
        return Err(OverlayError::PeerBanned(peer_id.to_string()));
    }

    if shared.peers.contains_key(&peer_id) {
        peer.close().await;
        shared.pending_connections.release_peer_id(&peer_id);
        pool.release_pending();
        return Err(OverlayError::AlreadyConnected);
    }

    let peer_info = peer.info().clone();
    if !OverlayManager::try_accept_authenticated_peer(&peer_info, &shared, &pool) {
        info!(
            "Outbound peer {} rejected because all available slots are taken",
            peer_id
        );
        OverlayManager::reject_authenticated_load(&mut peer, &shared).await;
        shared.pending_connections.release_peer_id(&peer_id);
        pool.release_pending();
        return Err(OverlayError::Internal(
            "peer rejected due to load".to_string(),
        ));
    }

    info!("Connected to peer: {} at {}", peer_id, addr);
    let (outbound_rx, flow_control) =
        match OverlayManager::register_peer(&peer, &peer_id, peer_info, &shared) {
            Ok(result) => result,
            Err(e) => {
                debug!("Rejected duplicate peer {} (race)", peer_id);
                peer.close().await;
                shared.pending_connections.release_peer_id(&peer_id);
                pool.release_authenticated();
                return Err(e);
            }
        };

    shared.pending_connections.release_peer_id(&peer_id);
    shared
        .send_peer_event(PeerEvent::Connected(addr.clone(), PeerType::Outbound))
        .await;

    // NOTE: Do NOT send PEERS to outbound peers (peers we connected to).
    // In stellar-core, only the acceptor (REMOTE_CALLED_US) sends PEERS during recvAuth().
    // If we send PEERS to a peer we initiated a connection to, the remote will
    // drop us silently (Peer.cpp:1225-1230).

    let peer_id_clone = peer_id.clone();
    let shared_clone = shared.clone();
    let pool_clone = Arc::clone(&pool);
    let handle = tokio::spawn(async move {
        // Clone again for run_peer_loop (takes ownership); originals used for cleanup.
        OverlayManager::run_peer_loop(
            peer_id_clone.clone(),
            peer,
            outbound_rx,
            flow_control,
            shared_clone.clone(),
        )
        .await;
        shared_clone.cleanup_peer(&peer_id_clone);
        pool_clone.release_authenticated();
    });

    push_peer_handle(&shared.peer_handles, handle);

    Ok(peer_id)
}
