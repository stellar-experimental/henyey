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

impl OverlayManager {
    /// Create a PeerHandle (outbound channel + FlowControl) and register
    /// the peer in the shared maps. Returns the receiver and FlowControl
    /// needed by `run_peer_loop`.
    pub(super) fn register_peer(
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
                if let Err(e) = peer.send(message).await {
                    debug!("Failed to send peers to {}: {}", peer_id, e);
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
            pool.release_pending();
            return;
        }

        let peer_info = peer.info().clone();

        // Always send PEERS before checking slot limits.
        // Matches stellar-core: sendPeers() is called before acceptAuthenticatedPeer().
        // This ensures crawlers and other peers get topology data even when rejected.
        Self::send_peers_to_inbound_peer(&mut peer, &peer_id, &peer_info, &shared).await;

        // Check if we have room for this authenticated peer.
        // Matches stellar-core acceptAuthenticatedPeer (OverlayManagerImpl.cpp:206).
        if !pool.try_promote_to_authenticated() {
            info!(
                "Non-preferred inbound authenticated peer {} rejected because all \
                 available slots are taken",
                peer_id
            );
            let err_msg = make_error_msg(ErrorCode::Load, "peer rejected");
            let _ = peer.send(err_msg).await;
            peer.close().await;
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

        let (outbound_rx, flow_control) = Self::register_peer(&peer, &peer_id, peer_info, &shared);

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
        let mut shutdown_rx = self.shutdown_tx.as_ref().unwrap().subscribe();

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
                                    match Peer::accept(connection, local_node, auth_timeout).await {
                                        Ok(peer) => {
                                            Self::handle_accepted_inbound_peer(peer, shared, pool).await;
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
    async fn run_discovered_peer_connection(
        addr: PeerAddress,
        local_node: LocalNode,
        connect_timeout: u64,
        pool: Arc<ConnectionPool>,
        shared: SharedPeerState,
        connection_factory: Arc<dyn ConnectionFactory>,
    ) {
        let connection = match connection_factory.connect(&addr, connect_timeout).await {
            Ok(c) => c,
            Err(e) => {
                debug!("Failed to connect to discovered peer {}: {}", addr, e);
                shared
                    .send_peer_event(PeerEvent::Failed(addr.clone(), PeerType::Outbound))
                    .await;
                pool.release_pending();
                return;
            }
        };

        let peer =
            match Peer::connect_with_connection(&addr, connection, local_node, connect_timeout)
                .await
            {
                Ok(p) => p,
                Err(e) => {
                    debug!("Failed to connect to discovered peer {}: {}", addr, e);
                    shared
                        .send_peer_event(PeerEvent::Failed(addr.clone(), PeerType::Outbound))
                        .await;
                    pool.release_pending();
                    return;
                }
            };

        let peer_id = peer.id().clone();
        info!("Connected to discovered peer: {} at {}", peer_id, addr);

        pool.mark_authenticated();
        shared
            .send_peer_event(PeerEvent::Connected(addr.clone(), PeerType::Outbound))
            .await;

        let peer_info = peer.info().clone();
        let (outbound_rx, flow_control) = Self::register_peer(&peer, &peer_id, peer_info, &shared);

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
            Self::run_discovered_peer_connection(
                addr,
                local_node,
                connect_timeout,
                pool,
                shared,
                connection_factory,
            )
            .await;
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
}

/// Connect to a peer and spawn its peer loop.
///
/// This is a module-level function (not a method) so it can be called from
/// `tick.rs` without requiring `&self`.
pub(super) async fn connect_outbound_inner(
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
            shared
                .send_peer_event(PeerEvent::Failed(addr.clone(), PeerType::Outbound))
                .await;
            return Err(e);
        }
    };

    let peer = match Peer::connect_with_connection(addr, connection, local_node, timeout_secs).await
    {
        Ok(peer) => peer,
        Err(e) => {
            pool.release_pending();
            shared
                .send_peer_event(PeerEvent::Failed(addr.clone(), PeerType::Outbound))
                .await;
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
        OverlayManager::register_peer(&peer, &peer_id, peer_info, &shared);
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

    shared.peer_handles.write().push(handle);

    Ok(peer_id)
}
