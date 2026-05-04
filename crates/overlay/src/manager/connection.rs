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
    ///
    /// Matches stellar-core's `acceptAuthenticatedPeer()` order:
    /// 1. Preferred peers: try promote, or evict a non-preferred same-direction peer.
    /// 2. Non-preferred + `preferred_peers_only`: reject immediately.
    /// 3. Non-preferred + !`preferred_peers_only`: try promote if capacity exists.
    pub(super) fn try_accept_authenticated_peer(
        peer_info: &PeerInfo,
        shared: &SharedPeerState,
        pool: &ConnectionPool,
    ) -> bool {
        let mut admission = shared.admission_state.lock();
        let preferred = shared.preferred_peers.read();
        let is_preferred = preferred.is_preferred(peer_info);

        if is_preferred {
            // Preferred peer: try normal promotion first.
            if pool.try_promote_to_authenticated() {
                return true;
            }

            // No capacity — try evicting a non-preferred peer of same direction.
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
                    if !super::peer_loop::send_error_and_drop(
                        &victim_id,
                        &entry.value().outbound_tx,
                        ErrorCode::Load,
                        "preferred peer selected instead",
                    ) {
                        debug!(
                            "Could not queue shutdown for non-preferred peer {}; trying next victim",
                            victim_id
                        );
                        continue;
                    }
                    admission.mark_evicting(victim_id.clone());
                    pool.force_promote_authenticated();
                    return true;
                }
            }

            false
        } else if shared.preferred_peers_only {
            // Non-preferred peer under strict mode: reject immediately.
            // Matches stellar-core: `if (!PREFERRED_PEERS_ONLY && capacity)`
            info!(
                "Non-preferred peer {} rejected: PREFERRED_PEERS_ONLY is set",
                peer_info.peer_id
            );
            false
        } else {
            // Non-preferred peer, normal mode: admit if capacity exists.
            pool.try_promote_to_authenticated()
        }
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
        initial_byte_grant: u32,
    ) -> std::result::Result<(mpsc::Receiver<OutboundMessage>, Arc<FlowControl>), OverlayError>
    {
        let (outbound_tx, outbound_rx) = mpsc::channel(shared.outbound_channel_capacity);
        let stats = peer.stats();
        let flow_control = Arc::new(FlowControl::with_scp_callback(
            FlowControlConfig {
                flow_control_bytes_batch_size: shared.flow_control_bytes_config.bytes_batch()
                    as u64,
                ..FlowControlConfig::default()
            },
            initial_byte_grant,
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
        initial_byte_grant: u32,
    ) {
        let peer_id = peer.id().clone();
        if shared.banned_peers.read().contains(&peer_id) {
            warn!("Rejected banned peer {}", peer_id);
            shared.metrics.inbound_reject.inc();
            peer.close().await;
            pool.release_pending();
            return;
        }
        if shared.peers.contains_key(&peer_id) {
            debug!("Rejected duplicate inbound peer {}", peer_id);
            shared.metrics.inbound_reject.inc();
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
            shared.metrics.inbound_reject.inc();
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
            match Self::register_peer(&peer, &peer_id, peer_info, &shared, initial_byte_grant) {
                Ok(result) => result,
                Err(_) => {
                    debug!("Rejected duplicate inbound peer {} (race)", peer_id);
                    shared.metrics.inbound_reject.inc();
                    peer.close().await;
                    shared.pending_connections.release_peer_id(&peer_id);
                    pool.release_authenticated();
                    return;
                }
            };

        // Successfully registered — release the pending reservation.
        shared.pending_connections.release_peer_id(&peer_id);
        shared.metrics.inbound_establish.inc();

        Self::run_peer_loop(
            peer_id.clone(),
            peer,
            outbound_rx,
            flow_control,
            shared.clone(),
        )
        .await;

        shared.metrics.inbound_drop.inc();
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
                                // Count every TCP-accepted inbound connection. Even
                                // connections we immediately reject (pool full, handshake
                                // failure) are real "attempts" from a wire perspective.
                                shared.metrics.inbound_attempt.inc();
                                let peer_ip = connection.remote_addr().ip();
                                // Reserve a pending slot. We allow the handshake to proceed
                                // even when authenticated slots are full — the actual
                                // authenticated limit is checked in
                                // handle_accepted_inbound_peer after PEERS is sent.
                                // This matches stellar-core, which always completes the
                                // handshake and sends PEERS before rejecting with ERR_LOAD.
                                if !pool.try_reserve_with_ip(Some(peer_ip)) {
                                    warn!("Inbound peer limit reached, rejecting connection from {}", peer_ip);
                                    shared.metrics.inbound_reject.inc();
                                    continue;
                                }

                                let shared = shared.clone();
                                let local_node = local_node.clone();
                                let pool = Arc::clone(&pool);

                                let peer_handle = tokio::spawn(async move {
                                    let remote_addr = connection.remote_addr();
                                    let pending_peer_ids = Arc::clone(&shared.pending_connections.by_peer_id);
                                    // Single snapshot: compute the initial byte grant once
                                    // and reuse for both SEND_MORE_EXTENDED and FlowControl.
                                    let initial_byte_grant = shared.flow_control_bytes_config.bytes_total(
                                        shared.max_tx_size_bytes.load(Ordering::Relaxed),
                                    );
                                    match Peer::accept(connection, local_node, auth_timeout, Arc::clone(&shared.banned_peers), pending_peer_ids, initial_byte_grant, Arc::clone(&shared.metrics)).await {
                                        Ok(peer) => {
                                            Self::handle_accepted_inbound_peer(peer, shared, pool, initial_byte_grant).await;
                                        }
                                        Err(e) => {
                                            debug!("Failed to accept peer: {}", e);
                                            shared.metrics.inbound_reject.inc();
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
        timeouts: crate::OutboundTimeouts,
        pool: Arc<ConnectionPool>,
        shared: SharedPeerState,
        connection_factory: Arc<dyn ConnectionFactory>,
    ) {
        // Reserve address slot to prevent duplicate outbound dials. If the
        // address is already in flight, no actual dial happens, so this is a
        // no-op skip — neither `outbound_attempt` nor `outbound_reject` fire
        // (those track real on-the-wire lifecycle events).
        let addr_key = format!("{}:{}", addr.host, addr.port);
        if !shared
            .pending_connections
            .try_reserve_address(addr_key.clone())
        {
            debug!(
                "Skipped discovered peer {} — connection already in flight",
                addr
            );
            pool.release_pending();
            return;
        }

        // Reservation succeeded — a TCP connect is about to happen. Count
        // the dial as an outbound attempt now (after reservation, before
        // the actual dial).
        shared.metrics.outbound_attempt.inc();

        let connection = match connection_factory
            .connect(&addr, timeouts.connect_secs)
            .await
        {
            Ok(c) => c,
            Err(e) => {
                debug!("Failed to connect to discovered peer {}: {}", addr, e);
                shared.metrics.outbound_reject.inc();
                shared
                    .send_peer_event(PeerEvent::Failed(addr.clone(), PeerType::Outbound))
                    .await;
                shared.pending_connections.release_address(&addr_key);
                pool.release_pending();
                return;
            }
        };

        // Single snapshot: compute the initial byte grant once and reuse for
        // both SEND_MORE_EXTENDED and FlowControl.
        let initial_byte_grant = shared
            .flow_control_bytes_config
            .bytes_total(shared.max_tx_size_bytes.load(Ordering::Relaxed));

        let pending_peer_ids = Some(Arc::clone(&shared.pending_connections.by_peer_id));
        let mut peer = match Peer::connect_with_connection(
            &addr,
            connection,
            local_node,
            timeouts.auth_secs,
            pending_peer_ids,
            initial_byte_grant,
            Arc::clone(&shared.metrics),
        )
        .await
        {
            Ok(p) => p,
            Err(e) => {
                debug!("Failed to connect to discovered peer {}: {}", addr, e);
                shared.metrics.outbound_reject.inc();
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
            shared.metrics.outbound_reject.inc();
            peer.close().await;
            shared.pending_connections.release_peer_id(&peer_id);
            pool.release_pending();
            return;
        }

        // Reject peers we're already connected to (mirrors connect_outbound_inner).
        if shared.peers.contains_key(&peer_id) {
            debug!("Rejected duplicate discovered peer {} at {}", peer_id, addr);
            shared.metrics.outbound_reject.inc();
            peer.close().await;
            shared.pending_connections.release_peer_id(&peer_id);
            pool.release_pending();
            return;
        }

        let peer_info = peer.info().clone();
        if !Self::try_accept_authenticated_peer(&peer_info, &shared, &pool) {
            let reason = if shared.preferred_peers_only {
                "PREFERRED_PEERS_ONLY is set"
            } else {
                "all available slots are taken"
            };
            info!(
                "Outbound discovered peer {} rejected because {}",
                peer_id, reason
            );
            shared.metrics.outbound_reject.inc();
            Self::reject_authenticated_load(&mut peer, &shared).await;
            shared.pending_connections.release_peer_id(&peer_id);
            pool.release_pending();
            return;
        }

        info!("Connected to discovered peer: {} at {}", peer_id, addr);
        let (outbound_rx, flow_control) =
            match Self::register_peer(&peer, &peer_id, peer_info, &shared, initial_byte_grant) {
                Ok(result) => result,
                Err(_) => {
                    debug!("Rejected duplicate discovered peer {} (race)", peer_id);
                    shared.metrics.outbound_reject.inc();
                    peer.close().await;
                    shared.pending_connections.release_peer_id(&peer_id);
                    pool.release_authenticated();
                    return;
                }
            };

        shared.pending_connections.release_peer_id(&peer_id);
        shared.metrics.outbound_establish.inc();
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

        shared.metrics.outbound_drop.inc();
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
        let timeouts = crate::OutboundTimeouts::from_config(&self.config);
        let connection_factory = Arc::clone(&self.connection_factory);

        let peer_handles = Arc::clone(&shared.peer_handles);
        let peer_handle = tokio::spawn(async move {
            Self::connect_to_discovered_peer(
                addr,
                local_node,
                timeouts,
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
    timeouts: crate::OutboundTimeouts,
    pool: Arc<ConnectionPool>,
    shared: SharedPeerState,
    connection_factory: Arc<dyn ConnectionFactory>,
) -> Result<PeerId> {
    // Reserve address slot to prevent duplicate outbound dials. If the
    // address is already in flight, no actual dial happens — this is a
    // no-op skip (neither `outbound_attempt` nor `outbound_reject` fire).
    // See `connect_to_discovered_peer` for the matching contract.
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

    // Reservation succeeded — a TCP connect is about to happen. Count the
    // dial as an outbound attempt.
    shared.metrics.outbound_attempt.inc();

    let connection = match connection_factory
        .connect(addr, timeouts.connect_secs)
        .await
    {
        Ok(connection) => connection,
        Err(e) => {
            shared.metrics.outbound_reject.inc();
            shared.pending_connections.release_address(&addr_key);
            pool.release_pending();
            shared
                .send_peer_event(PeerEvent::Failed(addr.clone(), PeerType::Outbound))
                .await;
            return Err(e);
        }
    };

    // Single snapshot: compute the initial byte grant once and reuse for
    // both SEND_MORE_EXTENDED and FlowControl.
    let initial_byte_grant = shared
        .flow_control_bytes_config
        .bytes_total(shared.max_tx_size_bytes.load(Ordering::Relaxed));

    let pending_peer_ids = Some(Arc::clone(&shared.pending_connections.by_peer_id));
    let mut peer = match Peer::connect_with_connection(
        addr,
        connection,
        local_node,
        timeouts.auth_secs,
        pending_peer_ids,
        initial_byte_grant,
        Arc::clone(&shared.metrics),
    )
    .await
    {
        Ok(peer) => peer,
        Err(e) => {
            shared.metrics.outbound_reject.inc();
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
        shared.metrics.outbound_reject.inc();
        peer.close().await;
        pool.release_pending();
        return Err(OverlayError::PeerBanned(peer_id.to_string()));
    }

    if shared.peers.contains_key(&peer_id) {
        shared.metrics.outbound_reject.inc();
        peer.close().await;
        shared.pending_connections.release_peer_id(&peer_id);
        pool.release_pending();
        return Err(OverlayError::AlreadyConnected);
    }

    let peer_info = peer.info().clone();
    if !OverlayManager::try_accept_authenticated_peer(&peer_info, &shared, &pool) {
        let reason = if shared.preferred_peers_only {
            "PREFERRED_PEERS_ONLY is set"
        } else {
            "all available slots are taken"
        };
        info!("Outbound peer {} rejected because {}", peer_id, reason);
        shared.metrics.outbound_reject.inc();
        OverlayManager::reject_authenticated_load(&mut peer, &shared).await;
        shared.pending_connections.release_peer_id(&peer_id);
        pool.release_pending();
        return Err(OverlayError::Internal(
            "peer rejected due to load".to_string(),
        ));
    }

    info!("Connected to peer: {} at {}", peer_id, addr);
    let (outbound_rx, flow_control) = match OverlayManager::register_peer(
        &peer,
        &peer_id,
        peer_info,
        &shared,
        initial_byte_grant,
    ) {
        Ok(result) => result,
        Err(e) => {
            debug!("Rejected duplicate peer {} (race)", peer_id);
            shared.metrics.outbound_reject.inc();
            peer.close().await;
            shared.pending_connections.release_peer_id(&peer_id);
            pool.release_authenticated();
            return Err(e);
        }
    };

    shared.pending_connections.release_peer_id(&peer_id);
    shared.metrics.outbound_establish.inc();
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
        shared_clone.metrics.outbound_drop.inc();
        shared_clone.cleanup_peer(&peer_id_clone);
        pool_clone.release_authenticated();
    });

    push_peer_handle(&shared.peer_handles, handle);

    Ok(peer_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connection::{Connection, ConnectionDirection, Listener};
    use crate::connection_factory::ConnectionFactory;
    use crate::{LocalNode, OverlayConfig, OverlayError, PeerAddress, PeerEvent, PeerType, Result};
    use async_trait::async_trait;
    use henyey_crypto::SecretKey;
    use std::sync::Arc;
    use tokio::time::Instant;

    // ---- Mock factories ----

    /// A connection factory that returns an in-memory connection immediately,
    /// but the remote end never sends any data (simulating a stalled HELLO).
    /// The server half of the duplex is kept alive to avoid EOF.
    struct StalledHelloFactory {
        _server_half: tokio::sync::Mutex<Option<tokio::io::DuplexStream>>,
    }

    impl StalledHelloFactory {
        fn new() -> Self {
            Self {
                _server_half: tokio::sync::Mutex::new(None),
            }
        }
    }

    #[async_trait]
    impl ConnectionFactory for StalledHelloFactory {
        async fn connect(&self, _addr: &PeerAddress, _timeout_secs: u64) -> Result<Connection> {
            let (client, server) = tokio::io::duplex(8192);
            *self._server_half.lock().await = Some(server);
            Connection::from_io(
                client,
                "127.0.0.1:11625".parse().unwrap(),
                ConnectionDirection::Outbound,
            )
        }

        async fn bind(&self, _port: u16) -> Result<Listener> {
            Err(OverlayError::ConnectionFailed("not used in test".into()))
        }
    }

    /// A connection factory whose `connect()` sleeps for the given timeout
    /// then returns `ConnectionTimeout`, simulating a stalled TCP SYN.
    struct StalledConnectFactory;

    #[async_trait]
    impl ConnectionFactory for StalledConnectFactory {
        async fn connect(&self, addr: &PeerAddress, timeout_secs: u64) -> Result<Connection> {
            tokio::time::sleep(Duration::from_secs(timeout_secs)).await;
            Err(OverlayError::ConnectionTimeout(addr.to_string()))
        }

        async fn bind(&self, _port: u16) -> Result<Listener> {
            Err(OverlayError::ConnectionFailed("not used in test".into()))
        }
    }

    /// Create a minimal OverlayManager with a custom factory and a peer_event
    /// receiver for assertions.
    fn setup_manager_with_factory(
        factory: Arc<dyn ConnectionFactory>,
    ) -> (
        super::super::OverlayManager,
        tokio::sync::mpsc::Receiver<PeerEvent>,
    ) {
        let mut config = OverlayConfig::default();
        let (tx, rx) = tokio::sync::mpsc::channel(16);
        config.peer_event_tx = Some(tx);
        let local_node = LocalNode::new_testnet(SecretKey::generate());
        let manager =
            super::super::OverlayManager::new_with_connection_factory(config, local_node, factory)
                .unwrap();
        (manager, rx)
    }

    // ---- Tests ----

    /// Verify that when TCP connects instantly but the remote never sends
    /// HELLO, the connection fails at auth_timeout_secs (2s), not
    /// connect_timeout_secs (10s).
    #[tokio::test(start_paused = true)]
    async fn test_stalled_hello_explicit_peer_uses_auth_timeout() {
        let factory = Arc::new(StalledHelloFactory::new());
        let (manager, mut peer_event_rx) = setup_manager_with_factory(factory);
        let shared = manager.shared_state();

        let addr = PeerAddress::new("10.0.0.1", 11625);
        let timeouts = crate::OutboundTimeouts {
            connect_secs: 10,
            auth_secs: 2,
        };

        // Reserve a pending slot (connect_to_explicit_peer releases it on failure).
        assert!(manager.outbound_pool.try_reserve());

        let start = Instant::now();
        let result = connect_to_explicit_peer(
            &addr,
            manager.local_node.clone(),
            timeouts,
            Arc::clone(&manager.outbound_pool),
            shared.clone(),
            manager.connection_factory.clone(),
        )
        .await;

        let elapsed = start.elapsed();

        // Must fail with a timeout error.
        let err = result.unwrap_err();
        assert!(
            matches!(err, OverlayError::ConnectionTimeout(_)),
            "expected ConnectionTimeout, got: {err}"
        );

        // Elapsed should be ~2s (auth timeout), not 10s (connect timeout).
        assert!(
            elapsed >= Duration::from_secs(2),
            "elapsed {elapsed:?} < 2s"
        );
        assert!(
            elapsed < Duration::from_secs(5),
            "elapsed {elapsed:?} >= 5s — likely used connect_timeout_secs instead of auth_timeout_secs"
        );

        // Verify cleanup: pending slot released.
        assert_eq!(manager.outbound_pool.pending_count(), 0);

        // Verify cleanup: pending address reservation cleared.
        let addr_key = format!("{}:{}", addr.host, addr.port);
        assert!(
            !shared
                .pending_connections
                .by_address
                .contains_key(&addr_key),
            "pending address reservation not cleared"
        );

        // Stage F.1: outbound_attempt was counted at function entry, and
        // outbound_reject was counted on the handshake failure. No establish
        // or drop because the peer never registered.
        assert_eq!(
            shared.metrics.outbound_attempt.get(),
            1,
            "outbound_attempt should fire once per connect_to_explicit_peer call"
        );
        assert_eq!(
            shared.metrics.outbound_reject.get(),
            1,
            "outbound_reject should fire on handshake timeout"
        );
        assert_eq!(shared.metrics.outbound_establish.get(), 0);
        assert_eq!(shared.metrics.outbound_drop.get(), 0);

        // Verify PeerEvent::Failed was emitted.
        let event = peer_event_rx
            .try_recv()
            .expect("expected PeerEvent::Failed");
        assert!(
            matches!(event, PeerEvent::Failed(_, PeerType::Outbound)),
            "expected Failed(_, Outbound), got: {event:?}"
        );
    }

    /// Verify that when TCP connect stalls, the connection fails at
    /// connect_timeout_secs (1s), not auth_timeout_secs (10s).
    #[tokio::test(start_paused = true)]
    async fn test_stalled_tcp_connect_explicit_peer_uses_connect_timeout() {
        let factory = Arc::new(StalledConnectFactory);
        let (manager, mut peer_event_rx) = setup_manager_with_factory(factory);
        let shared = manager.shared_state();

        let addr = PeerAddress::new("10.0.0.1", 11625);
        let timeouts = crate::OutboundTimeouts {
            connect_secs: 1,
            auth_secs: 10,
        };

        assert!(manager.outbound_pool.try_reserve());

        let start = Instant::now();
        let result = connect_to_explicit_peer(
            &addr,
            manager.local_node.clone(),
            timeouts,
            Arc::clone(&manager.outbound_pool),
            shared.clone(),
            manager.connection_factory.clone(),
        )
        .await;

        let elapsed = start.elapsed();

        let err = result.unwrap_err();
        assert!(
            matches!(err, OverlayError::ConnectionTimeout(_)),
            "expected ConnectionTimeout, got: {err}"
        );

        // Elapsed should be ~1s (connect timeout), not 10s (auth timeout).
        assert!(
            elapsed >= Duration::from_secs(1),
            "elapsed {elapsed:?} < 1s"
        );
        assert!(
            elapsed < Duration::from_secs(5),
            "elapsed {elapsed:?} >= 5s — likely used auth_timeout_secs instead of connect_timeout_secs"
        );

        assert_eq!(manager.outbound_pool.pending_count(), 0);

        let addr_key = format!("{}:{}", addr.host, addr.port);
        assert!(
            !shared
                .pending_connections
                .by_address
                .contains_key(&addr_key),
            "pending address reservation not cleared"
        );

        // Stage F.1: TCP connect failure path also goes through
        // outbound_attempt + outbound_reject.
        assert_eq!(shared.metrics.outbound_attempt.get(), 1);
        assert_eq!(shared.metrics.outbound_reject.get(), 1);
        assert_eq!(shared.metrics.outbound_establish.get(), 0);
        assert_eq!(shared.metrics.outbound_drop.get(), 0);

        let event = peer_event_rx
            .try_recv()
            .expect("expected PeerEvent::Failed");
        assert!(
            matches!(event, PeerEvent::Failed(_, PeerType::Outbound)),
            "expected Failed(_, Outbound), got: {event:?}"
        );
    }

    /// Verify that `connect_to_discovered_peer` also uses auth_timeout_secs
    /// for the handshake phase (not connect_timeout_secs).
    #[tokio::test(start_paused = true)]
    async fn test_stalled_hello_discovered_peer_uses_auth_timeout() {
        let factory = Arc::new(StalledHelloFactory::new());
        let (manager, mut peer_event_rx) = setup_manager_with_factory(factory);
        let shared = manager.shared_state();

        let addr = PeerAddress::new("10.0.0.2", 11625);
        let timeouts = crate::OutboundTimeouts {
            connect_secs: 10,
            auth_secs: 2,
        };

        assert!(manager.outbound_pool.try_reserve());

        let start = Instant::now();
        OverlayManager::connect_to_discovered_peer(
            addr.clone(),
            manager.local_node.clone(),
            timeouts,
            Arc::clone(&manager.outbound_pool),
            shared.clone(),
            manager.connection_factory.clone(),
        )
        .await;

        let elapsed = start.elapsed();

        // Elapsed should be ~2s (auth timeout), not 10s (connect timeout).
        assert!(
            elapsed >= Duration::from_secs(2),
            "elapsed {elapsed:?} < 2s"
        );
        assert!(
            elapsed < Duration::from_secs(5),
            "elapsed {elapsed:?} >= 5s — likely used connect_timeout_secs instead of auth_timeout_secs"
        );

        // Verify cleanup: pending slot released.
        assert_eq!(manager.outbound_pool.pending_count(), 0);

        // Verify cleanup: pending address reservation cleared.
        let addr_key = format!("{}:{}", addr.host, addr.port);
        assert!(
            !shared
                .pending_connections
                .by_address
                .contains_key(&addr_key),
            "pending address reservation not cleared"
        );

        // Stage F.1: discovered-peer path also bumps outbound_attempt +
        // outbound_reject on handshake failure.
        assert_eq!(shared.metrics.outbound_attempt.get(), 1);
        assert_eq!(shared.metrics.outbound_reject.get(), 1);
        assert_eq!(shared.metrics.outbound_establish.get(), 0);
        assert_eq!(shared.metrics.outbound_drop.get(), 0);

        // Verify PeerEvent::Failed was emitted.
        let event = peer_event_rx
            .try_recv()
            .expect("expected PeerEvent::Failed");
        assert!(
            matches!(event, PeerEvent::Failed(_, PeerType::Outbound)),
            "expected Failed(_, Outbound), got: {event:?}"
        );
    }

    /// Verify that a successful end-to-end connection between two managers
    /// produces matching `outbound_establish` (initiator) and
    /// `inbound_establish` (acceptor) increments, plus matching `*_drop`
    /// counts after teardown.
    ///
    /// This is the success-path counterpart to the timeout/reject tests above.
    #[tokio::test]
    async fn test_connection_lifecycle_counters_on_handshake_success() {
        use crate::loopback::LoopbackConnectionFactory;
        use crate::OverlayConfig;

        let factory = Arc::new(LoopbackConnectionFactory::default());

        // Two managers wired up via the loopback connection factory.
        let mut config_a = OverlayConfig::testnet();
        config_a.listen_port = 11625;
        config_a.listen_enabled = true;
        config_a.known_peers.clear();
        config_a.connect_timeout_secs = 1;

        let mut config_b = OverlayConfig::testnet();
        config_b.listen_port = 11626;
        config_b.listen_enabled = true;
        config_b.known_peers.clear();
        config_b.connect_timeout_secs = 1;

        let local_a = LocalNode::new_testnet(SecretKey::generate());
        let local_b = LocalNode::new_testnet(SecretKey::generate());

        let mut manager_a = super::super::OverlayManager::new_with_connection_factory(
            config_a,
            local_a,
            Arc::clone(&factory) as Arc<dyn ConnectionFactory>,
        )
        .unwrap();
        let mut manager_b = super::super::OverlayManager::new_with_connection_factory(
            config_b,
            local_b,
            factory as Arc<dyn ConnectionFactory>,
        )
        .unwrap();

        manager_a.start().await.expect("start a");
        manager_b.start().await.expect("start b");

        let metrics_a = Arc::clone(&manager_a.shared_state().metrics);
        let metrics_b = Arc::clone(&manager_b.shared_state().metrics);

        // A dials B.
        let addr_b = PeerAddress::new("127.0.0.1", 11626);
        manager_a.connect(&addr_b).await.expect("connect");

        // Wait for the handshake on both sides. The loopback duplex completes
        // both within ~50ms; poll up to ~2s.
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            if metrics_a.outbound_establish.get() >= 1 && metrics_b.inbound_establish.get() >= 1 {
                break;
            }
            if std::time::Instant::now() > deadline {
                panic!(
                    "lifecycle counters did not reach establish: \
                     A: outbound_attempt={}, outbound_establish={}, outbound_reject={}; \
                     B: inbound_attempt={}, inbound_establish={}, inbound_reject={}",
                    metrics_a.outbound_attempt.get(),
                    metrics_a.outbound_establish.get(),
                    metrics_a.outbound_reject.get(),
                    metrics_b.inbound_attempt.get(),
                    metrics_b.inbound_establish.get(),
                    metrics_b.inbound_reject.get(),
                );
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        // Initiator side: exactly one outbound attempt + one establish, no reject.
        assert_eq!(metrics_a.outbound_attempt.get(), 1, "outbound_attempt");
        assert_eq!(metrics_a.outbound_establish.get(), 1, "outbound_establish");
        assert_eq!(metrics_a.outbound_reject.get(), 0, "outbound_reject");

        // Acceptor side: exactly one inbound attempt + one establish, no reject.
        assert_eq!(metrics_b.inbound_attempt.get(), 1, "inbound_attempt");
        assert_eq!(metrics_b.inbound_establish.get(), 1, "inbound_establish");
        assert_eq!(metrics_b.inbound_reject.get(), 0, "inbound_reject");

        // Tear down and verify drop counters match establish counters.
        manager_a.shutdown().await.expect("shutdown a");
        manager_b.shutdown().await.expect("shutdown b");

        let drop_deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            if metrics_a.outbound_drop.get() >= 1 && metrics_b.inbound_drop.get() >= 1 {
                break;
            }
            if std::time::Instant::now() > drop_deadline {
                panic!(
                    "drop counters did not reach 1: \
                     outbound_drop={}, inbound_drop={}",
                    metrics_a.outbound_drop.get(),
                    metrics_b.inbound_drop.get()
                );
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(metrics_a.outbound_drop.get(), 1);
        assert_eq!(metrics_b.inbound_drop.get(), 1);
    }
}
