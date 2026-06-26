//! Connection management: establishing, accepting, and registering peer connections.
//!
//! Contains `start_listener`, `handle_accepted_inbound_peer`,
//! `connect_outbound_inner`, `run_discovered_peer_connection`, `add_peer`,
//! and related helpers.

use super::peer_loop::{make_error_msg, DropInitiator};
use super::{OutboundMessage, OverlayManager, SharedPeerState};
use crate::{
    connection::{ConnectionDirection, ConnectionPool, Listener},
    connection_factory::ConnectionFactory,
    flow_control::{FlowControl, FlowControlConfig},
    peer::{Peer, PeerInfo},
    LocalNode, OverlayError, PeerAddress, PeerEvent, PeerId, PeerType, ResolvedPeerAddr, Result,
};
use rand::Rng;
use std::net::SocketAddr;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;
use stellar_xdr::ErrorCode;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use super::PeerHandle;
use parking_lot::RwLock;
use tokio::task::JoinHandle;

/// Result of a successful `try_register_peer` call.
pub(super) struct RegisterResult {
    pub outbound_rx: mpsc::Receiver<OutboundMessage>,
    /// Clone of this peer's outbound sender, handed to `run_peer_loop` so the
    /// drain-gated flow-control release token (#3625) can enqueue
    /// `SEND_MORE_EXTENDED` grants back to the peer once the SCP consumer
    /// drains the envelope.
    pub outbound_tx: mpsc::Sender<OutboundMessage>,
    pub flow_control: Arc<FlowControl>,
    pub generation: u64,
    pub replaced: Option<ReplacedPeer>,
}

/// A peer handle that was displaced by the mutual-dial tiebreaker.
/// Dropping the handle closes its outbound channel, signalling the old
/// peer_loop to exit.
pub(super) struct ReplacedPeer {
    pub handle: PeerHandle,
    pub direction: ConnectionDirection,
}

/// Delay in milliseconds between successive connection attempts when
/// filling outbound slots, to avoid overwhelming the network stack.
const CONNECTION_ATTEMPT_DELAY_MS: u64 = 50;

/// Pre-resolved peer address for batch dedup and outbound connection.
///
/// Carries the original address (for known-peer tracking and PeerInfo),
/// the resolved IPv4 socket address (for TCP connect), and the canonical
/// dedup key.
struct ResolvedPeer {
    original: PeerAddress,
    resolved: SocketAddr,
    key: ResolvedPeerAddr,
}

/// Resolve a PeerAddress to a canonical IPv4 `SocketAddr` for transport.
///
/// Mirrors stellar-core's `PeerBareAddress::resolve()` (PeerBareAddress.cpp:46-115):
/// - IPv4 literal → parsed into `SocketAddr` (fast path, no syscall)
/// - Hostname → DNS lookup, first IPv4 result used
/// - IPv6 literal → error (stellar-core is IPv4-only for peer addresses)
/// - DNS failure or no IPv4 results → error
///
/// Returns `(socket_addr, resolved_key)` where `socket_addr` is the resolved
/// IPv4 address for TCP connect and `resolved_key` is the typed
/// `ResolvedPeerAddr` for `PendingConnections` dedup.
async fn resolve_peer_address(addr: &PeerAddress) -> Result<(SocketAddr, ResolvedPeerAddr)> {
    use std::net::{IpAddr, SocketAddrV4};
    match addr.host.parse::<IpAddr>() {
        Ok(IpAddr::V4(ipv4)) => {
            let sa = SocketAddr::new(IpAddr::V4(ipv4), addr.port);
            let key = ResolvedPeerAddr::from_socket_addr_v4(SocketAddrV4::new(ipv4, addr.port));
            Ok((sa, key))
        }
        Ok(IpAddr::V6(_)) => Err(OverlayError::ConnectionFailed(format!(
            "IPv6 address not supported: {}",
            addr
        ))),
        Err(_) => {
            // Hostname — resolve via DNS, pick first IPv4
            let mut addrs = tokio::net::lookup_host(format!("{}:{}", addr.host, addr.port))
                .await
                .map_err(|e| {
                    OverlayError::ConnectionFailed(format!(
                        "DNS resolution failed for {}: {}",
                        addr, e
                    ))
                })?;
            let sa = addrs.find(|a| a.is_ipv4()).ok_or_else(|| {
                OverlayError::ConnectionFailed(format!("no IPv4 address found for {}", addr))
            })?;
            let key = match sa {
                SocketAddr::V4(v4) => ResolvedPeerAddr::from_socket_addr_v4(v4),
                _ => unreachable!("filtered to IPv4 above"),
            };
            Ok((sa, key))
        }
    }
}

/// Outcome of an `add_peer` call.
///
/// These are success-path outcomes only — error conditions (e.g.
/// `OverlayError::NotStarted`) are returned via the `Err` variant.
/// Note: `OverlayError` also has `AlreadyConnected` and `PeerLimitReached`
/// variants, but those are used for inbound-connection rejection and
/// handshake-level errors.  In the outbound-dial path, "already connected"
/// and "pool full" are expected flow-control outcomes, not errors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AddPeerOutcome {
    /// New outbound connection task was spawned.
    Initiated,
    /// Already have an authenticated connection to this address.
    AlreadyConnected,
    /// Outbound connection pool is at capacity.
    PoolFull,
}

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

    /// Atomically register a peer, applying a mutual-dial tiebreaker when
    /// a cross-direction collision is detected.
    ///
    /// Returns `RegisterResult` on success, which may include a replaced
    /// `PeerHandle` from a mutual-dial resolution. The caller MUST drop
    /// the replaced handle promptly — dropping closes its outbound channel,
    /// causing the old peer_loop to exit.
    ///
    /// Returns `Err(AlreadyConnected)` for same-direction duplicates or
    /// when the tiebreaker favors the existing connection.
    pub(super) fn try_register_peer(
        peer: &Peer,
        peer_id: &PeerId,
        peer_info: PeerInfo,
        shared: &SharedPeerState,
        initial_byte_grant: u32,
    ) -> std::result::Result<RegisterResult, OverlayError> {
        let (outbound_tx, outbound_rx) = mpsc::channel(shared.outbound_channel_capacity);
        // Clone the sender for run_peer_loop's drain-gated SEND_MORE release
        // (#3625); the original is moved into the PeerHandle below.
        let outbound_tx_for_loop = outbound_tx.clone();
        let stats = peer.stats();
        let flow_control = Arc::new(FlowControl::with_scp_callback(
            FlowControlConfig {
                flow_control_bytes_batch_size: shared.flow_control_bytes_config.bytes_batch()
                    as u64,
                peer_flood_reading_capacity: shared.peer_flood_reading_capacity as u64,
                ..FlowControlConfig::default()
            },
            initial_byte_grant,
            shared.scp_callback.clone(),
        ));
        flow_control.set_peer_id(peer_id.clone());
        let generation = shared.next_peer_generation.fetch_add(1, Ordering::Relaxed);
        let new_direction = peer_info.direction;
        let peer_handle = PeerHandle {
            outbound_tx,
            stats,
            flow_control: Arc::clone(&flow_control),
            direction: new_direction,
            generation,
        };

        use dashmap::mapref::entry::Entry;
        let replaced = match shared.peers.entry(peer_id.clone()) {
            Entry::Vacant(e) => {
                e.insert(peer_handle);
                None
            }
            Entry::Occupied(mut e) => {
                let existing_direction = e.get().direction;
                if existing_direction == new_direction {
                    // Same-direction duplicate — always reject.
                    return Err(OverlayError::AlreadyConnected(peer_id.to_string()));
                }
                // Cross-direction collision (mutual-dial).
                // Tiebreaker: higher peer ID keeps outbound.
                let local_bytes = shared.local_peer_id.as_bytes();
                let remote_bytes = peer_id.as_bytes();
                let local_is_higher = local_bytes > remote_bytes;

                // Determine if the NEW connection should win.
                let new_wins = match (new_direction, existing_direction) {
                    (ConnectionDirection::Inbound, ConnectionDirection::Outbound) => {
                        // We're accepting inbound. We should yield outbound
                        // (accept inbound) when local < remote.
                        !local_is_higher
                    }
                    (ConnectionDirection::Outbound, ConnectionDirection::Inbound) => {
                        // We're dialing outbound. We should keep outbound
                        // when local > remote.
                        local_is_higher
                    }
                    _ => unreachable!("same-direction case handled above"),
                };

                if new_wins {
                    let old = e.insert(peer_handle);
                    let old_direction = old.direction;
                    info!(
                        "Mutual-dial tiebreaker: replaced {:?} with {:?} for peer {}",
                        old_direction, new_direction, peer_id
                    );
                    Some(ReplacedPeer {
                        handle: old,
                        direction: old_direction,
                    })
                } else {
                    debug!(
                        "Mutual-dial tiebreaker: keeping existing {:?} for peer {}, rejecting {:?}",
                        existing_direction, peer_id, new_direction
                    );
                    return Err(OverlayError::AlreadyConnected(peer_id.to_string()));
                }
            }
        };
        shared.peer_info_cache.insert(peer_id.clone(), peer_info);
        // Only count a NEWLY-occupied authenticated slot. When this registration
        // REPLACED an existing authenticated handle (the mutual-dial "new wins"
        // tiebreaker above), the slot was already counted at the displaced peer's
        // original registration, so re-counting would double-bump (+2 add / 0 drop
        // for a single surviving peer). stellar-core v27.0.0 resolves the same
        // collision pre-auth at HELLO (Peer::recvHello ERR_CONF "already-connected"),
        // dropping the loser while still pending — never ++mAddedAuthenticatedPeers,
        // never recordDroppedPeer. Suppressing the duplicate add (NOT bumping a drop)
        // matches core's absolute counters: +1 add / 0 drop. (#3500)
        if replaced.is_none() {
            shared
                .added_authenticated_peers
                .fetch_add(1, Ordering::Relaxed);
        }
        Ok(RegisterResult {
            outbound_rx,
            outbound_tx: outbound_tx_for_loop,
            flow_control,
            generation,
            replaced,
        })
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
            if peer.holds_pending_peer_id() {
                shared.pending_connections.release_peer_id(&peer_id);
            }
            pool.release_pending();
            return;
        }
        if let Some(existing) = shared.peers.get(&peer_id) {
            if existing.direction == ConnectionDirection::Inbound {
                debug!("Rejected duplicate inbound peer {}", peer_id);
                drop(existing);
                shared.metrics.inbound_reject.inc();
                let _ = peer
                    .send(make_error_msg(ErrorCode::Conf, "already connected"))
                    .await;
                peer.close().await;
                if peer.holds_pending_peer_id() {
                    shared.pending_connections.release_peer_id(&peer_id);
                }
                pool.release_pending();
                return;
            }
            // Cross-direction (outbound exists, inbound arriving): fall through
            // to tiebreaker in try_register_peer.
            drop(existing);
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
            if peer.holds_pending_peer_id() {
                shared.pending_connections.release_peer_id(&peer_id);
            }
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

        let register_result = match Self::try_register_peer(
            &peer,
            &peer_id,
            peer_info,
            &shared,
            initial_byte_grant,
        ) {
            Ok(result) => result,
            Err(_) => {
                debug!(
                    "Rejected duplicate inbound peer {} (race or tiebreaker)",
                    peer_id
                );
                shared.metrics.inbound_reject.inc();
                peer.close().await;
                if peer.holds_pending_peer_id() {
                    shared.pending_connections.release_peer_id(&peer_id);
                }
                pool.release_authenticated();
                return;
            }
        };

        // If a mutual-dial replacement occurred, drop the old handle to
        // signal the old peer_loop to exit.
        if let Some(replaced) = register_result.replaced {
            info!(
                "Dropping replaced {:?} handle for peer {} after mutual-dial resolution",
                replaced.direction, peer_id
            );
            drop(replaced.handle);
        }

        // Successfully registered — release the pending reservation if we own it.
        if peer.holds_pending_peer_id() {
            shared.pending_connections.release_peer_id(&peer_id);
        }
        shared.metrics.inbound_establish.inc();
        // #3419 diagnostic (observability-only): an inbound peer transitioned to
        // authenticated. The monotonic counter lets an operator distinguish
        // "never authenticated" (stays 0) from "authenticated then churned"
        // (climbs while the instantaneous gauge stays near 0). The warn event
        // gives a timestamped, per-direction authenticated-transition signal.
        shared.metrics.inbound_authenticated_total.inc();
        warn!(
            target: "overlay_inbound_diag",
            peer_id = %peer_id,
            direction = "inbound",
            event = "authenticated",
            "inbound peer transitioned to authenticated"
        );

        let generation = register_result.generation;
        let drop_initiator = Self::run_peer_loop(
            peer_id.clone(),
            peer,
            register_result.outbound_rx,
            register_result.outbound_tx,
            register_result.flow_control,
            shared.clone(),
        )
        .await;

        // #3422: the total is unconditional; the initiator-segmented siblings sum
        // to it by construction (every inbound drop increments exactly one of
        // remote/local here).
        shared.metrics.inbound_drop.inc();
        match drop_initiator {
            DropInitiator::Remote => shared.metrics.inbound_drop_remote.inc(),
            DropInitiator::Local => shared.metrics.inbound_drop_local.inc(),
        }
        shared.cleanup_peer(&peer_id, generation);
        pool.release_authenticated();
    }

    /// Start the connection listener.
    pub(super) async fn start_listener(&mut self, pre_bound: Option<Listener>) -> Result<()> {
        let listener = match pre_bound {
            Some(l) => l,
            None => {
                self.connection_factory
                    .bind(self.config.listen_port)
                    .await?
            }
        };
        let bound_addr = listener.local_addr();
        self.listen_addr = Some(bound_addr);
        info!("Listening on {}", bound_addr);

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
                                    // Single snapshot: compute initial grants once
                                    // and reuse for both SEND_MORE_EXTENDED and FlowControl.
                                    let initial_byte_grant = shared.flow_control_bytes_config.bytes_total(
                                        shared.max_tx_size_bytes.load(Ordering::Relaxed),
                                    );
                                    let initial_message_grant = shared.peer_flood_reading_capacity;
                                    match Peer::accept(connection, local_node, auth_timeout, Arc::clone(&shared.banned_peers), pending_peer_ids, initial_byte_grant, initial_message_grant, Arc::clone(&shared.metrics)).await {
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
        peer: ResolvedPeer,
        local_node: LocalNode,
        timeouts: crate::OutboundTimeouts,
        pool: Arc<ConnectionPool>,
        shared: SharedPeerState,
        connection_factory: Arc<dyn ConnectionFactory>,
    ) {
        let ResolvedPeer {
            original: addr,
            resolved: resolved_addr,
            key: addr_key,
        } = peer;

        // Reserve address slot to prevent duplicate outbound dials. If the
        // address is already in flight, no actual dial happens, so this is a
        // no-op skip — neither `outbound_attempt` nor `outbound_reject` fire
        // (those track real on-the-wire lifecycle events).
        if !shared.pending_connections.try_reserve_address(addr_key) {
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
            .connect(resolved_addr, timeouts.connect_secs)
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

        // Single snapshot: compute initial grants once and reuse for
        // both SEND_MORE_EXTENDED and FlowControl.
        let initial_byte_grant = shared
            .flow_control_bytes_config
            .bytes_total(shared.max_tx_size_bytes.load(Ordering::Relaxed));
        let initial_message_grant = shared.peer_flood_reading_capacity;

        let pending_peer_ids = Some(Arc::clone(&shared.pending_connections.by_peer_id));
        let mut peer = match Peer::connect_with_connection(
            &addr,
            connection,
            local_node,
            timeouts.auth_secs,
            pending_peer_ids,
            initial_byte_grant,
            initial_message_grant,
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

        // Reject peers we're already connected to in the same direction.
        // Cross-direction collisions (mutual-dial) fall through to the
        // tiebreaker in try_register_peer.
        if let Some(existing) = shared.peers.get(&peer_id) {
            if existing.direction == ConnectionDirection::Outbound {
                debug!("Rejected duplicate discovered peer {} at {}", peer_id, addr);
                drop(existing);
                shared.metrics.outbound_reject.inc();
                peer.close().await;
                shared.pending_connections.release_peer_id(&peer_id);
                pool.release_pending();
                return;
            }
            drop(existing);
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
        let register_result = match Self::try_register_peer(
            &peer,
            &peer_id,
            peer_info,
            &shared,
            initial_byte_grant,
        ) {
            Ok(result) => result,
            Err(_) => {
                debug!(
                    "Rejected duplicate discovered peer {} (race or tiebreaker)",
                    peer_id
                );
                shared.metrics.outbound_reject.inc();
                peer.close().await;
                shared.pending_connections.release_peer_id(&peer_id);
                pool.release_authenticated();
                return;
            }
        };

        if let Some(replaced) = register_result.replaced {
            info!(
                "Dropping replaced {:?} handle for peer {} after mutual-dial resolution",
                replaced.direction, peer_id
            );
            drop(replaced.handle);
        }

        shared.pending_connections.release_peer_id(&peer_id);
        shared.metrics.outbound_establish.inc();
        shared
            .send_peer_event(PeerEvent::Connected(addr.clone(), PeerType::Outbound))
            .await;

        // NOTE: Do NOT send PEERS to outbound peers — see Peer.cpp:1225-1230.

        let generation = register_result.generation;
        // Outbound out of scope for #3422 — discard the DropInitiator.
        let _ = Self::run_peer_loop(
            peer_id.clone(),
            peer,
            register_result.outbound_rx,
            register_result.outbound_tx,
            register_result.flow_control,
            shared.clone(),
        )
        .await;

        shared.metrics.outbound_drop.inc();
        shared.cleanup_peer(&peer_id, generation);
        // Insert reconnection cooldown with random jitter to break
        // mutual-dial oscillation. Both sides insert independent random
        // delays, ensuring one side's cooldown expires first and dials
        // alone (no simultaneous mutual-dial).
        let jitter = Duration::from_millis(rand::thread_rng().gen_range(1000..3000));
        shared
            .dial_cooldowns
            .insert(addr_key, std::time::Instant::now() + jitter);
        pool.release_authenticated();
    }

    /// Add a peer to connect to.
    ///
    /// This is used for peer discovery when we receive a Peers message.
    /// Returns `AddPeerOutcome` describing what happened.
    // SECURITY: dial dedup and queue bounded by max_peer_count config + peer universe
    pub async fn add_peer(&self, addr: PeerAddress) -> Result<AddPeerOutcome> {
        if !self.running.load(Ordering::Relaxed) {
            return Err(OverlayError::NotStarted);
        }

        // Check already-connected before DNS resolution to avoid unnecessary
        // lookups for peers we're already talking to.
        let already_connected = self
            .peer_info_cache
            .iter()
            .any(|entry| Self::peer_info_matches_address(entry.value(), &addr));
        if already_connected {
            debug!("Already connected to {}", addr);
            return Ok(AddPeerOutcome::AlreadyConnected);
        }

        // Resolve hostname → IPv4 once up front. The resolved IP is used for
        // dedup and TCP connect; the original addr is kept for PeerInfo.
        let (resolved_addr, addr_key) = resolve_peer_address(&addr).await?;
        let resolved = ResolvedPeer {
            original: addr,
            resolved: resolved_addr,
            key: addr_key,
        };

        self.add_peer_resolved(resolved).await
    }

    /// Add a pre-resolved peer to connect to.
    ///
    /// Internal helper used by both `add_peer` (after resolving) and
    /// `add_peers` (which resolves in batch for dedup).
    async fn add_peer_resolved(&self, resolved: ResolvedPeer) -> Result<AddPeerOutcome> {
        // Check reconnection cooldown — prevents immediate re-dial after a
        // connection drops, breaking mutual-dial oscillation.
        if let Some(expiry) = self.shared_state().dial_cooldowns.get(&resolved.key) {
            if std::time::Instant::now() < *expiry.value() {
                return Ok(AddPeerOutcome::AlreadyConnected);
            }
        }

        // Check if we're already connected to this address (any direction).
        // Check both the original address (hostname match) and the resolved
        // address (IP match) to catch aliases — e.g., a hostname that resolves
        // to an IP we're already connected to via a different path.
        // Checked before pool capacity to match stellar-core's connectToImpl
        // ordering and ensure AlreadyConnected is returned rather than PoolFull.
        let already_connected = self.peer_info_cache.iter().any(|entry| {
            let info = entry.value();
            Self::peer_info_matches_address(info, &resolved.original)
                || Self::peer_info_matches_socket_addr(info, resolved.resolved)
        });
        if already_connected {
            debug!("Already connected to {}", resolved.original);
            return Ok(AddPeerOutcome::AlreadyConnected);
        }

        // Check if we're at the connection limit
        if !self.outbound_pool.try_reserve() {
            debug!(
                "Outbound peer limit reached, not adding peer {}",
                resolved.original
            );
            return Ok(AddPeerOutcome::PoolFull);
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
                resolved,
                local_node,
                timeouts,
                pool,
                shared,
                connection_factory,
            )
            .await;
        });

        push_peer_handle(&peer_handles, peer_handle);

        Ok(AddPeerOutcome::Initiated)
    }

    /// Add multiple peers to connect to.
    ///
    /// This is used for peer discovery when we receive a Peers message.
    /// Returns the number of connection attempts initiated.
    ///
    /// Each address is resolved via DNS before dedup so that the canonical
    /// `ip:port` key is used — matching the key used downstream by
    /// `PendingConnections`. This prevents hostname/IP aliasing from
    /// causing wasted dials and pool-slot churn.
    // SECURITY: dial dedup and queue bounded by max_peer_count config + peer universe
    pub async fn add_peers(&self, addrs: Vec<PeerAddress>) -> usize {
        let mut seen_keys = std::collections::HashSet::new();
        let mut added = 0;
        let target_outbound = self.config.target_outbound_peers;
        let mut remaining = target_outbound.saturating_sub(self.outbound_pool.count());

        for addr in addrs {
            // Always record the peer for future connection attempts, even if
            // we can't resolve or dial it right now. The address was already
            // persisted to the peer DB by the caller.
            self.add_known_peer(addr.clone());

            // Resolve to canonical ip:port key for dedup.
            let (resolved_addr, addr_key) = match resolve_peer_address(&addr).await {
                Ok(pair) => pair,
                Err(e) => {
                    debug!("Skipped peer {} in batch: {}", addr, e);
                    continue;
                }
            };

            // Dedup on resolved key — duplicates don't consume remaining budget.
            if !seen_keys.insert(addr_key) {
                continue;
            }

            if remaining == 0 || !self.outbound_pool.can_accept() {
                break;
            }

            let resolved = ResolvedPeer {
                original: addr,
                resolved: resolved_addr,
                key: addr_key,
            };
            match self.add_peer_resolved(resolved).await {
                Ok(AddPeerOutcome::Initiated) => {
                    added += 1;
                    remaining = remaining.saturating_sub(1);
                }
                Ok(AddPeerOutcome::AlreadyConnected | AddPeerOutcome::PoolFull) => {}
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
    // Resolve hostname → IPv4 once. The resolved IP is used for both the
    // pending-connection dedup key and the TCP connect. The original `addr`
    // is kept for PeerInfo::original_address (hostname matching).
    let (resolved_addr, addr_key) = match resolve_peer_address(addr).await {
        Ok(pair) => pair,
        Err(e) => {
            pool.release_pending();
            return Err(e);
        }
    };

    // Reserve address slot to prevent duplicate outbound dials. If the
    // address is already in flight, no actual dial happens — this is a
    // no-op skip (neither `outbound_attempt` nor `outbound_reject` fire).
    // See `connect_to_discovered_peer` for the matching contract.
    if !shared.pending_connections.try_reserve_address(addr_key) {
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
        .connect(resolved_addr, timeouts.connect_secs)
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
    let initial_message_grant = shared.peer_flood_reading_capacity;

    let pending_peer_ids = Some(Arc::clone(&shared.pending_connections.by_peer_id));
    let mut peer = match Peer::connect_with_connection(
        addr,
        connection,
        local_node,
        timeouts.auth_secs,
        pending_peer_ids,
        initial_byte_grant,
        initial_message_grant,
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
        if peer.holds_pending_peer_id() {
            shared.pending_connections.release_peer_id(&peer_id);
        }
        pool.release_pending();
        return Err(OverlayError::PeerBanned(peer_id.to_string()));
    }

    // Reject same-direction (outbound) duplicates early.
    // Cross-direction collisions fall through to tiebreaker.
    if let Some(existing) = shared.peers.get(&peer_id) {
        if existing.direction == ConnectionDirection::Outbound {
            drop(existing);
            shared.metrics.outbound_reject.inc();
            peer.close().await;
            shared.pending_connections.release_peer_id(&peer_id);
            pool.release_pending();
            return Err(OverlayError::AlreadyConnected(peer_id.to_string()));
        }
        drop(existing);
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
    let register_result = match OverlayManager::try_register_peer(
        &peer,
        &peer_id,
        peer_info,
        &shared,
        initial_byte_grant,
    ) {
        Ok(result) => result,
        Err(e) => {
            debug!("Rejected duplicate peer {} (race or tiebreaker)", peer_id);
            shared.metrics.outbound_reject.inc();
            peer.close().await;
            shared.pending_connections.release_peer_id(&peer_id);
            pool.release_authenticated();
            return Err(e);
        }
    };

    if let Some(replaced) = register_result.replaced {
        info!(
            "Dropping replaced {:?} handle for peer {} after mutual-dial resolution",
            replaced.direction, peer_id
        );
        drop(replaced.handle);
    }

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
    let generation = register_result.generation;
    let handle = tokio::spawn(async move {
        // Clone again for run_peer_loop (takes ownership); originals used for cleanup.
        // Outbound out of scope for #3422 — discard the DropInitiator.
        let _ = OverlayManager::run_peer_loop(
            peer_id_clone.clone(),
            peer,
            register_result.outbound_rx,
            register_result.outbound_tx,
            register_result.flow_control,
            shared_clone.clone(),
        )
        .await;
        shared_clone.metrics.outbound_drop.inc();
        shared_clone.cleanup_peer(&peer_id_clone, generation);
        // Insert reconnection cooldown with random jitter (see connect_to_discovered_peer).
        let jitter = Duration::from_millis(rand::thread_rng().gen_range(1000..3000));
        shared_clone
            .dial_cooldowns
            .insert(addr_key, std::time::Instant::now() + jitter);
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
    use crate::manager::PendingPeerEntry;
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
        async fn connect(&self, _addr: SocketAddr, _timeout_secs: u64) -> Result<Connection> {
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

    /// A connection factory whose `connect()` immediately returns an error,
    /// simulating a refused TCP connection.
    struct FailingConnectFactory;

    #[async_trait]
    impl ConnectionFactory for FailingConnectFactory {
        async fn connect(&self, addr: SocketAddr, _timeout_secs: u64) -> Result<Connection> {
            Err(OverlayError::ConnectionFailed(format!(
                "test: connection refused to {}",
                addr
            )))
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
        async fn connect(&self, addr: SocketAddr, timeout_secs: u64) -> Result<Connection> {
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
        let addr_key = ResolvedPeerAddr::from_socket_addr_v4(std::net::SocketAddrV4::new(
            std::net::Ipv4Addr::new(10, 0, 0, 1),
            11625,
        ));
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

        let addr_key = ResolvedPeerAddr::from_socket_addr_v4(std::net::SocketAddrV4::new(
            std::net::Ipv4Addr::new(10, 0, 0, 2),
            11625,
        ));
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

        let addr_key = ResolvedPeerAddr::from_socket_addr_v4(std::net::SocketAddrV4::new(
            std::net::Ipv4Addr::new(10, 0, 0, 2),
            11625,
        ));
        let resolved = ResolvedPeer {
            original: addr.clone(),
            resolved: format!("{}:{}", addr.host, addr.port).parse().unwrap(),
            key: addr_key,
        };

        let start = Instant::now();
        OverlayManager::connect_to_discovered_peer(
            resolved,
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

        manager_a.start(None).await.expect("start a");
        manager_b.start(None).await.expect("start b");

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

        // #3422: the initiator-segmented siblings must sum to the inbound_drop
        // total by construction. Exactly one of remote/local is incremented per
        // inbound drop, so remote + local == total == 1. (Which side fires is
        // non-deterministic here — both managers shut down concurrently — so we
        // assert the invariant, not a specific attribution.)
        assert_eq!(
            metrics_b.inbound_drop_remote.get() + metrics_b.inbound_drop_local.get(),
            metrics_b.inbound_drop.get(),
            "inbound_drop_remote + inbound_drop_local must equal inbound_drop total"
        );
        assert_eq!(
            metrics_b.inbound_drop_remote.get() + metrics_b.inbound_drop_local.get(),
            1
        );
    }

    // ---- add_peer dedup tests ----

    /// Helper to set up a manager with running=true and a pre-populated peer_info_cache.
    fn setup_manager_for_add_peer_dedup() -> super::super::OverlayManager {
        let factory = Arc::new(StalledConnectFactory);
        let (manager, _rx) = setup_manager_with_factory(factory);
        manager.running.store(true, Ordering::Relaxed);
        manager
    }

    #[tokio::test(start_paused = true)]
    async fn test_add_peer_already_connected_by_resolved_ip() {
        let manager = setup_manager_for_add_peer_dedup();
        let shared = manager.shared_state();

        // Simulate an authenticated peer cached at 1.2.3.4:11625
        let peer_id = PeerId::from_bytes([1u8; 32]);
        let peer_info = crate::peer::PeerInfo {
            peer_id: peer_id.clone(),
            address: "1.2.3.4:11625".parse().unwrap(),
            direction: ConnectionDirection::Inbound,
            version_string: String::new(),
            overlay_version: 0,
            ledger_version: 0,
            connected_at: std::time::Instant::now(),
            original_address: None,
        };
        shared.peer_info_cache.insert(peer_id, peer_info);

        // add_peer with same IP:port should return AlreadyConnected
        let result = manager
            .add_peer(PeerAddress::new("1.2.3.4", 11625))
            .await
            .unwrap();
        assert_eq!(result, AddPeerOutcome::AlreadyConnected);
    }

    #[tokio::test(start_paused = true)]
    async fn test_add_peer_already_connected_by_hostname() {
        let manager = setup_manager_for_add_peer_dedup();
        let shared = manager.shared_state();

        // Simulate an outbound peer with original_address preserving hostname
        let peer_id = PeerId::from_bytes([2u8; 32]);
        let peer_info = crate::peer::PeerInfo {
            peer_id: peer_id.clone(),
            address: "10.0.0.42:11625".parse().unwrap(),
            direction: ConnectionDirection::Outbound,
            version_string: String::new(),
            overlay_version: 0,
            ledger_version: 0,
            connected_at: std::time::Instant::now(),
            original_address: Some(PeerAddress::new("core.stellar.org", 11625)),
        };
        shared.peer_info_cache.insert(peer_id, peer_info);

        // add_peer with hostname should match via original_address
        let result = manager
            .add_peer(PeerAddress::new("core.stellar.org", 11625))
            .await
            .unwrap();
        assert_eq!(result, AddPeerOutcome::AlreadyConnected);
    }

    #[tokio::test(start_paused = true)]
    async fn test_add_peer_different_port_not_matched() {
        let manager = setup_manager_for_add_peer_dedup();
        let shared = manager.shared_state();

        // Peer at port 11625
        let peer_id = PeerId::from_bytes([3u8; 32]);
        let peer_info = crate::peer::PeerInfo {
            peer_id: peer_id.clone(),
            address: "1.2.3.4:11625".parse().unwrap(),
            direction: ConnectionDirection::Outbound,
            version_string: String::new(),
            overlay_version: 0,
            ledger_version: 0,
            connected_at: std::time::Instant::now(),
            original_address: None,
        };
        shared.peer_info_cache.insert(peer_id, peer_info);

        // Different port should NOT match
        let result = manager
            .add_peer(PeerAddress::new("1.2.3.4", 11626))
            .await
            .unwrap();
        assert_eq!(result, AddPeerOutcome::Initiated);
    }

    #[tokio::test(start_paused = true)]
    async fn test_add_peer_different_ip_not_matched() {
        let manager = setup_manager_for_add_peer_dedup();
        let shared = manager.shared_state();

        // Peer at 1.2.3.4
        let peer_id = PeerId::from_bytes([4u8; 32]);
        let peer_info = crate::peer::PeerInfo {
            peer_id: peer_id.clone(),
            address: "1.2.3.4:11625".parse().unwrap(),
            direction: ConnectionDirection::Inbound,
            version_string: String::new(),
            overlay_version: 0,
            ledger_version: 0,
            connected_at: std::time::Instant::now(),
            original_address: None,
        };
        shared.peer_info_cache.insert(peer_id, peer_info);

        // Different IP should NOT match
        let result = manager
            .add_peer(PeerAddress::new("5.6.7.8", 11625))
            .await
            .unwrap();
        assert_eq!(result, AddPeerOutcome::Initiated);
    }

    #[tokio::test(start_paused = true)]
    async fn test_add_peer_already_connected_returns_before_pool_full() {
        let manager = setup_manager_for_add_peer_dedup();
        let shared = manager.shared_state();

        // Exhaust pool capacity so try_reserve would fail
        while manager.outbound_pool.try_reserve() {}
        // Confirm pool is full
        assert!(!manager.outbound_pool.try_reserve());

        // Insert an authenticated peer
        let peer_id = PeerId::from_bytes([5u8; 32]);
        let peer_info = crate::peer::PeerInfo {
            peer_id: peer_id.clone(),
            address: "1.2.3.4:11625".parse().unwrap(),
            direction: ConnectionDirection::Inbound,
            version_string: String::new(),
            overlay_version: 0,
            ledger_version: 0,
            connected_at: std::time::Instant::now(),
            original_address: None,
        };
        shared.peer_info_cache.insert(peer_id, peer_info);

        // Should return AlreadyConnected, NOT PoolFull (duplicate check first)
        let result = manager
            .add_peer(PeerAddress::new("1.2.3.4", 11625))
            .await
            .unwrap();
        assert_eq!(result, AddPeerOutcome::AlreadyConnected);
    }

    #[tokio::test(start_paused = true)]
    async fn test_add_peer_respects_dial_cooldown() {
        use std::net::{Ipv4Addr, SocketAddrV4};
        let manager = setup_manager_for_add_peer_dedup();
        let shared = manager.shared_state();

        let addr_key = ResolvedPeerAddr::from_socket_addr_v4(SocketAddrV4::new(
            Ipv4Addr::new(1, 2, 3, 4),
            11625,
        ));

        // Insert a cooldown expiring 2 seconds from now.
        shared
            .dial_cooldowns
            .insert(addr_key, std::time::Instant::now() + Duration::from_secs(2));

        // Should be blocked by cooldown (returns AlreadyConnected).
        let result = manager
            .add_peer(PeerAddress::new("1.2.3.4", 11625))
            .await
            .unwrap();
        assert_eq!(result, AddPeerOutcome::AlreadyConnected);

        // Advance past cooldown. Note: start_paused doesn't affect
        // std::time::Instant, so we simulate expiry by removing and
        // re-inserting with an already-past timestamp.
        shared
            .dial_cooldowns
            .insert(addr_key, std::time::Instant::now() - Duration::from_secs(1));

        // Now the dial should proceed (Initiated, since no existing connection).
        let result = manager
            .add_peer(PeerAddress::new("1.2.3.4", 11625))
            .await
            .unwrap();
        assert_eq!(result, AddPeerOutcome::Initiated);
    }

    // ---- resolve_peer_address tests ----

    #[tokio::test]
    async fn test_resolve_peer_address_ipv4_passthrough() {
        use std::net::{Ipv4Addr, SocketAddrV4};
        let addr = PeerAddress::new("10.0.0.1", 11625);
        let (resolved, key) = resolve_peer_address(&addr).await.unwrap();
        assert_eq!(resolved.ip().to_string(), "10.0.0.1");
        assert_eq!(resolved.port(), 11625);
        assert_eq!(
            key,
            ResolvedPeerAddr::from_socket_addr_v4(SocketAddrV4::new(
                Ipv4Addr::new(10, 0, 0, 1),
                11625
            ))
        );
    }

    #[tokio::test]
    async fn test_resolve_peer_address_localhost() {
        use std::net::{Ipv4Addr, SocketAddrV4};
        let addr = PeerAddress::new("localhost", 11625);
        let (resolved, key) = resolve_peer_address(&addr).await.unwrap();
        assert_eq!(resolved.ip().to_string(), "127.0.0.1");
        assert_eq!(resolved.port(), 11625);
        assert_eq!(
            key,
            ResolvedPeerAddr::from_socket_addr_v4(SocketAddrV4::new(
                Ipv4Addr::new(127, 0, 0, 1),
                11625
            ))
        );
    }

    #[tokio::test]
    async fn test_resolve_peer_address_failure_returns_error() {
        let addr = PeerAddress::new("this.hostname.does.not.exist.invalid", 11625);
        let result = resolve_peer_address(&addr).await;
        assert!(result.is_err(), "unresolvable hostname should return error");
    }

    #[tokio::test]
    async fn test_resolve_peer_address_ipv6_rejected() {
        let addr = PeerAddress::new("::1", 11625);
        let result = resolve_peer_address(&addr).await;
        assert!(result.is_err(), "IPv6 addresses should be rejected");
    }

    #[tokio::test]
    async fn test_pending_dedup_with_resolved_keys() {
        use super::super::PendingConnections;
        let pending = PendingConnections::new();

        // Simulate what callers now do: resolve first, then reserve.
        // Two addresses that resolve to the same IP should dedup.
        let addr_ip = PeerAddress::new("127.0.0.1", 11625);
        let (_, key1) = resolve_peer_address(&addr_ip).await.unwrap();
        assert!(
            pending.try_reserve_address(key1),
            "first reservation should succeed"
        );

        // Same resolved IP via hostname should produce same key
        let addr_hostname = PeerAddress::new("localhost", 11625);
        let (_, key2) = resolve_peer_address(&addr_hostname).await.unwrap();
        assert_eq!(key1, key2, "hostname and IP should produce same key");
        assert!(
            !pending.try_reserve_address(key2),
            "duplicate reservation (hostname alias) should fail"
        );

        // Different IP should succeed
        let addr_other = PeerAddress::new("10.0.0.1", 11625);
        let (_, key3) = resolve_peer_address(&addr_other).await.unwrap();
        assert!(
            pending.try_reserve_address(key3),
            "different IP should succeed"
        );
    }

    // ---- Cleanup edge-case regression tests (issue #2527) ----

    /// Verify that `connect_to_explicit_peer` releases the outbound pool
    /// slot when address resolution fails due to an IPv6 literal.
    #[tokio::test]
    async fn test_connect_to_explicit_peer_releases_pool_on_ipv6() {
        let factory = Arc::new(StalledConnectFactory);
        let (manager, mut peer_event_rx) = setup_manager_with_factory(factory);
        let shared = manager.shared_state();

        let addr = PeerAddress::new("::1", 11625);
        let timeouts = crate::OutboundTimeouts {
            connect_secs: 10,
            auth_secs: 2,
        };

        // Reserve a pending slot (connect_to_explicit_peer must release it).
        assert!(manager.outbound_pool.try_reserve());

        let result = connect_to_explicit_peer(
            &addr,
            manager.local_node.clone(),
            timeouts,
            Arc::clone(&manager.outbound_pool),
            shared.clone(),
            manager.connection_factory.clone(),
        )
        .await;

        assert!(result.is_err(), "IPv6 should fail at resolve");

        // Pool slot must be released.
        assert_eq!(
            manager.outbound_pool.pending_count(),
            0,
            "pool slot not released after IPv6 resolve failure"
        );

        // No dial happened, so no metrics or events should fire.
        assert_eq!(shared.metrics.outbound_attempt.get(), 0);
        assert_eq!(shared.metrics.outbound_reject.get(), 0);
        assert_eq!(shared.metrics.outbound_establish.get(), 0);
        assert!(
            peer_event_rx.try_recv().is_err(),
            "no PeerEvent should be emitted for pre-dial failure"
        );
    }

    /// Verify that `connect_to_explicit_peer` releases the outbound pool
    /// slot when DNS resolution fails for an unresolvable hostname.
    #[tokio::test]
    async fn test_connect_to_explicit_peer_releases_pool_on_dns_failure() {
        let factory = Arc::new(StalledConnectFactory);
        let (manager, mut peer_event_rx) = setup_manager_with_factory(factory);
        let shared = manager.shared_state();

        let addr = PeerAddress::new("this.host.does.not.exist.invalid", 11625);
        let timeouts = crate::OutboundTimeouts {
            connect_secs: 10,
            auth_secs: 2,
        };

        assert!(manager.outbound_pool.try_reserve());

        let result = connect_to_explicit_peer(
            &addr,
            manager.local_node.clone(),
            timeouts,
            Arc::clone(&manager.outbound_pool),
            shared.clone(),
            manager.connection_factory.clone(),
        )
        .await;

        assert!(result.is_err(), "unresolvable hostname should fail");

        assert_eq!(
            manager.outbound_pool.pending_count(),
            0,
            "pool slot not released after DNS failure"
        );

        assert_eq!(shared.metrics.outbound_attempt.get(), 0);
        assert_eq!(shared.metrics.outbound_reject.get(), 0);
        assert_eq!(shared.metrics.outbound_establish.get(), 0);
        assert!(
            peer_event_rx.try_recv().is_err(),
            "no PeerEvent should be emitted for pre-dial failure"
        );
    }

    /// Verify that `connect_to_explicit_peer` correctly cleans up when
    /// the remote peer is banned — releases pool slot, peer-ID reservation,
    /// and increments reject metrics.
    #[tokio::test]
    async fn test_connect_to_explicit_peer_rejects_banned_peer() {
        use crate::loopback::LoopbackConnectionFactory;

        let factory = Arc::new(LoopbackConnectionFactory::default());

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
        let banned_peer_id = local_b.peer_id();

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

        // Ban B's peer-ID on A's side BEFORE connecting.
        manager_a
            .banned_peers
            .write()
            .insert(banned_peer_id.clone());

        manager_a.start(None).await.expect("start a");
        manager_b.start(None).await.expect("start b");

        let metrics_a = Arc::clone(&manager_a.shared_state().metrics);
        let shared_a = manager_a.shared_state();

        // A tries to connect to B.
        let addr_b = PeerAddress::new("127.0.0.1", 11626);
        let result = manager_a.connect(&addr_b).await;

        // The connect call should fail with PeerBanned.
        assert!(result.is_err(), "connecting to banned peer should fail");
        let err = result.unwrap_err();
        assert!(
            matches!(err, OverlayError::PeerBanned(_)),
            "expected PeerBanned, got: {err}"
        );

        // Bounded wait for metrics to settle.
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while metrics_a.outbound_reject.get() < 1 {
            if std::time::Instant::now() > deadline {
                panic!("outbound_reject did not reach 1 within 2s");
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        assert_eq!(metrics_a.outbound_attempt.get(), 1, "outbound_attempt");
        assert_eq!(metrics_a.outbound_reject.get(), 1, "outbound_reject");
        assert_eq!(metrics_a.outbound_establish.get(), 0, "outbound_establish");

        // Pool slot must be released.
        assert_eq!(
            manager_a.outbound_pool.pending_count(),
            0,
            "pool slot not released after banned peer rejection"
        );

        // Peer-ID reservation must be released.
        assert!(
            !shared_a
                .pending_connections
                .by_peer_id
                .contains_key(&banned_peer_id),
            "peer-ID reservation not released after banned peer rejection"
        );

        manager_a.shutdown().await.expect("shutdown a");
        manager_b.shutdown().await.expect("shutdown b");
    }

    /// Verify that `handle_accepted_inbound_peer` releases the pool slot
    /// AND the peer-ID reservation when the peer is banned and holds the
    /// reservation (`holds_pending_peer_id == true`).
    #[tokio::test]
    async fn test_handle_accepted_inbound_peer_rejects_banned_peer_with_reservation() {
        use crate::peer::Peer;

        let factory = Arc::new(StalledConnectFactory);
        let (manager, _rx) = setup_manager_with_factory(factory);
        let shared = manager.shared_state();

        let banned_id = PeerId::from_bytes([42u8; 32]);

        // Pre-ban the peer.
        shared.banned_peers.write().insert(banned_id.clone());

        // Pre-insert a pending peer-ID reservation (simulates successful
        // handshake that reserved the peer-ID).
        shared.pending_connections.by_peer_id.insert(
            banned_id.clone(),
            PendingPeerEntry {
                reserved_at: std::time::Instant::now(),
                direction: ConnectionDirection::Inbound,
            },
        );

        // Reserve a pool slot.
        assert!(manager.inbound_pool.try_reserve());

        let peer = Peer::new_test_inbound(banned_id.clone(), true, Arc::clone(&shared.metrics));

        super::super::OverlayManager::handle_accepted_inbound_peer(
            peer,
            shared.clone(),
            Arc::clone(&manager.inbound_pool),
            300_000,
        )
        .await;

        // Banned peer should be rejected.
        assert_eq!(
            shared.metrics.inbound_reject.get(),
            1,
            "inbound_reject should be 1"
        );
        assert_eq!(
            shared.metrics.inbound_establish.get(),
            0,
            "inbound_establish should be 0"
        );

        // Pool slot must be released.
        assert_eq!(
            manager.inbound_pool.pending_count(),
            0,
            "pool slot not released after banned inbound peer"
        );

        // Peer-ID reservation must be released (holds_pending_peer_id was true).
        assert!(
            !shared
                .pending_connections
                .by_peer_id
                .contains_key(&banned_id),
            "peer-ID reservation should be released when holds_pending_peer_id is true"
        );
    }

    /// Verify that `handle_accepted_inbound_peer` releases the pool slot
    /// but does NOT release the peer-ID reservation when `holds_pending_peer_id`
    /// is false (simulates a mutual-dial inbound peer that doesn't own the
    /// reservation).
    #[tokio::test]
    async fn test_handle_accepted_inbound_peer_rejects_banned_peer_without_reservation() {
        use crate::peer::Peer;

        let factory = Arc::new(StalledConnectFactory);
        let (manager, _rx) = setup_manager_with_factory(factory);
        let shared = manager.shared_state();

        let banned_id = PeerId::from_bytes([43u8; 32]);

        // Pre-ban the peer.
        shared.banned_peers.write().insert(banned_id.clone());

        // Pre-insert a pending peer-ID reservation (owned by the outbound
        // side in a mutual-dial scenario — this inbound peer should NOT
        // release it).
        shared.pending_connections.by_peer_id.insert(
            banned_id.clone(),
            PendingPeerEntry {
                reserved_at: std::time::Instant::now(),
                direction: ConnectionDirection::Outbound,
            },
        );

        // Reserve a pool slot.
        assert!(manager.inbound_pool.try_reserve());

        let peer = Peer::new_test_inbound(banned_id.clone(), false, Arc::clone(&shared.metrics));

        super::super::OverlayManager::handle_accepted_inbound_peer(
            peer,
            shared.clone(),
            Arc::clone(&manager.inbound_pool),
            300_000,
        )
        .await;

        // Banned peer should be rejected.
        assert_eq!(
            shared.metrics.inbound_reject.get(),
            1,
            "inbound_reject should be 1"
        );
        assert_eq!(
            shared.metrics.inbound_establish.get(),
            0,
            "inbound_establish should be 0"
        );

        // Pool slot must be released.
        assert_eq!(
            manager.inbound_pool.pending_count(),
            0,
            "pool slot not released after banned inbound peer"
        );

        // Peer-ID reservation must NOT be released (holds_pending_peer_id was false).
        assert!(
            shared
                .pending_connections
                .by_peer_id
                .contains_key(&banned_id),
            "peer-ID reservation should NOT be released when holds_pending_peer_id is false"
        );
    }

    // ---- add_peers batch dedup tests ----

    #[tokio::test(start_paused = true)]
    async fn test_add_peers_dedup_same_ip_port() {
        let manager = setup_manager_for_add_peer_dedup();

        // Two identical entries should produce only one connection attempt.
        let addrs = vec![
            PeerAddress::new("127.0.0.1", 11625),
            PeerAddress::new("127.0.0.1", 11625),
        ];
        let added = manager.add_peers(addrs).await;
        assert_eq!(added, 1, "duplicate ip:port should be deduped to one dial");
    }

    #[tokio::test(start_paused = true)]
    async fn test_add_peers_no_dedup_different_ports() {
        let manager = setup_manager_for_add_peer_dedup();

        // Different ports are distinct peers — both should be dialed.
        let addrs = vec![
            PeerAddress::new("127.0.0.1", 11625),
            PeerAddress::new("127.0.0.1", 11626),
        ];
        let added = manager.add_peers(addrs).await;
        assert_eq!(added, 2, "different ports should not dedup");
    }

    #[tokio::test(start_paused = true)]
    async fn test_add_peers_duplicate_does_not_starve_unique() {
        let manager = setup_manager_for_add_peer_dedup();

        // Batch [A, A, B] with enough remaining budget — duplicate A
        // should be skipped without consuming budget, so B gets dialed.
        let addrs = vec![
            PeerAddress::new("127.0.0.1", 11625),
            PeerAddress::new("127.0.0.1", 11625),
            PeerAddress::new("10.0.0.1", 11625),
        ];
        let added = manager.add_peers(addrs).await;
        assert_eq!(
            added, 2,
            "duplicate should be skipped, unique peer should still be dialed"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn test_add_peers_dedup_hostname_ip_alias() {
        let manager = setup_manager_for_add_peer_dedup();

        // "localhost" resolves to 127.0.0.1 — these should dedup to one dial.
        let addrs = vec![
            PeerAddress::new("localhost", 11625),
            PeerAddress::new("127.0.0.1", 11625),
        ];
        let added = manager.add_peers(addrs).await;
        assert_eq!(
            added, 1,
            "hostname/IP alias should dedup to one dial via resolved key"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn test_add_peers_alias_does_not_starve_unique() {
        let manager = setup_manager_for_add_peer_dedup();

        // Batch [localhost:11625, 127.0.0.1:11625, 10.0.0.1:11625]
        // The alias pair should dedup, but the unique peer should still be dialed.
        let addrs = vec![
            PeerAddress::new("localhost", 11625),
            PeerAddress::new("127.0.0.1", 11625),
            PeerAddress::new("10.0.0.1", 11625),
        ];
        let added = manager.add_peers(addrs).await;
        assert_eq!(added, 2, "alias dedup should not starve unique peer");
    }

    // ---- Discovered-peer cleanup regression tests (issue #2530) ----

    /// Verify that `connect_to_discovered_peer` releases the pool slot and
    /// address reservation when TCP connect fails immediately.
    #[tokio::test]
    async fn test_connect_to_discovered_peer_releases_pool_on_connect_failure() {
        let factory = Arc::new(FailingConnectFactory);
        let (manager, mut peer_event_rx) = setup_manager_with_factory(factory);
        let shared = manager.shared_state();

        let addr = PeerAddress::new("10.0.0.1", 11625);
        let timeouts = crate::OutboundTimeouts {
            connect_secs: 10,
            auth_secs: 2,
        };

        // Reserve a pending pool slot (connect_to_discovered_peer must release it).
        assert!(manager.outbound_pool.try_reserve());

        let addr_key = ResolvedPeerAddr::from_socket_addr_v4(std::net::SocketAddrV4::new(
            std::net::Ipv4Addr::new(1, 2, 3, 4),
            11625,
        ));
        let resolved = ResolvedPeer {
            original: addr.clone(),
            resolved: format!("{}:{}", addr.host, addr.port).parse().unwrap(),
            key: addr_key,
        };

        OverlayManager::connect_to_discovered_peer(
            resolved,
            manager.local_node.clone(),
            timeouts,
            Arc::clone(&manager.outbound_pool),
            shared.clone(),
            manager.connection_factory.clone(),
        )
        .await;

        // Pool slot must be released.
        assert_eq!(
            manager.outbound_pool.pending_count(),
            0,
            "pool slot not released after connect failure"
        );

        // Address reservation must be released.
        assert!(
            !shared
                .pending_connections
                .by_address
                .contains_key(&addr_key),
            "address reservation not released after connect failure"
        );

        // Metrics: one attempt, one reject, no establish.
        assert_eq!(shared.metrics.outbound_attempt.get(), 1, "outbound_attempt");
        assert_eq!(shared.metrics.outbound_reject.get(), 1, "outbound_reject");
        assert_eq!(
            shared.metrics.outbound_establish.get(),
            0,
            "outbound_establish"
        );

        // PeerEvent::Failed should have been emitted.
        let event = peer_event_rx
            .try_recv()
            .expect("expected PeerEvent::Failed");
        assert!(
            matches!(event, PeerEvent::Failed(_, PeerType::Outbound)),
            "expected Failed(_, Outbound), got: {event:?}"
        );
    }

    /// Verify that `connect_to_discovered_peer` releases the pool slot when
    /// the address is already in flight (another connection is pending to
    /// the same address).
    #[tokio::test]
    async fn test_connect_to_discovered_peer_releases_pool_on_address_in_flight() {
        let factory = Arc::new(StalledConnectFactory);
        let (manager, mut peer_event_rx) = setup_manager_with_factory(factory);
        let shared = manager.shared_state();

        let addr = PeerAddress::new("10.0.0.1", 11625);
        let timeouts = crate::OutboundTimeouts {
            connect_secs: 10,
            auth_secs: 2,
        };

        // Pre-insert an address reservation to simulate a prior in-flight connection.
        let addr_key = ResolvedPeerAddr::from_socket_addr_v4(std::net::SocketAddrV4::new(
            std::net::Ipv4Addr::new(10, 0, 0, 1),
            11625,
        ));
        assert!(
            shared.pending_connections.try_reserve_address(addr_key),
            "pre-reservation should succeed"
        );

        // Reserve a pending pool slot (connect_to_discovered_peer must release it).
        assert!(manager.outbound_pool.try_reserve());

        let resolved = ResolvedPeer {
            original: addr.clone(),
            resolved: format!("{}:{}", addr.host, addr.port).parse().unwrap(),
            key: addr_key,
        };

        OverlayManager::connect_to_discovered_peer(
            resolved,
            manager.local_node.clone(),
            timeouts,
            Arc::clone(&manager.outbound_pool),
            shared.clone(),
            manager.connection_factory.clone(),
        )
        .await;

        // Pool slot must be released.
        assert_eq!(
            manager.outbound_pool.pending_count(),
            0,
            "pool slot not released after address-in-flight skip"
        );

        // The pre-existing address reservation must still be present (not our
        // reservation to clear — it belongs to the other in-flight connection).
        assert!(
            shared
                .pending_connections
                .by_address
                .contains_key(&addr_key),
            "pre-existing address reservation should not be removed"
        );

        // No metrics should fire — this is a no-op skip, not a real dial.
        assert_eq!(shared.metrics.outbound_attempt.get(), 0, "outbound_attempt");
        assert_eq!(shared.metrics.outbound_reject.get(), 0, "outbound_reject");
        assert_eq!(
            shared.metrics.outbound_establish.get(),
            0,
            "outbound_establish"
        );

        // No PeerEvent should be emitted.
        assert!(
            peer_event_rx.try_recv().is_err(),
            "no PeerEvent should be emitted for address-in-flight skip"
        );
    }

    /// Verify that `connect_to_discovered_peer` (via `add_peer`) releases
    /// the pool slot and peer-ID reservation when the remote peer is banned.
    #[tokio::test]
    async fn test_connect_to_discovered_peer_rejects_banned_peer() {
        use crate::loopback::LoopbackConnectionFactory;

        let factory = Arc::new(LoopbackConnectionFactory::default());

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
        let banned_peer_id = local_b.peer_id();

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

        // Ban B's peer-ID on A's side BEFORE connecting.
        manager_a
            .banned_peers
            .write()
            .insert(banned_peer_id.clone());

        manager_a.start(None).await.expect("start a");
        manager_b.start(None).await.expect("start b");

        let metrics_a = Arc::clone(&manager_a.shared_state().metrics);
        let shared_a = manager_a.shared_state();

        // A tries to connect to B via add_peer (discovered path).
        let addr_b = PeerAddress::new("127.0.0.1", 11626);
        let outcome = manager_a.add_peer(addr_b).await;

        // add_peer() spawns the connection task and returns Initiated.
        assert!(
            matches!(outcome, Ok(super::super::AddPeerOutcome::Initiated)),
            "expected Initiated, got: {outcome:?}"
        );

        // Bounded wait for the async task to detect the ban.
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while metrics_a.outbound_reject.get() < 1 {
            if std::time::Instant::now() > deadline {
                panic!("outbound_reject did not reach 1 within 2s");
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        assert_eq!(metrics_a.outbound_attempt.get(), 1, "outbound_attempt");
        assert_eq!(metrics_a.outbound_reject.get(), 1, "outbound_reject");
        assert_eq!(metrics_a.outbound_establish.get(), 0, "outbound_establish");

        // Pool slot must be released.
        assert_eq!(
            manager_a.outbound_pool.pending_count(),
            0,
            "pool slot not released after banned peer rejection"
        );

        // Peer-ID reservation must be released.
        assert!(
            !shared_a
                .pending_connections
                .by_peer_id
                .contains_key(&banned_peer_id),
            "peer-ID reservation not released after banned peer rejection"
        );

        // The banned-peer branch does NOT emit a PeerEvent (unlike connect
        // failure which sends PeerEvent::Failed). Verify no peer was registered.
        assert_eq!(
            shared_a.peers.len(),
            0,
            "banned peer should not be registered"
        );

        manager_a.shutdown().await.expect("shutdown a");
        manager_b.shutdown().await.expect("shutdown b");
    }

    // ---- Mutual-dial tiebreaker tests ----

    mod tiebreaker {
        use super::*;
        use crate::connection::ConnectionDirection;
        use crate::flow_control::{FlowControl, FlowControlConfig};
        use crate::peer::PeerStats;

        /// Build a minimal SharedPeerState with a specific local peer ID.
        fn make_shared(local_id: PeerId) -> SharedPeerState {
            use crate::flow_control::FlowControlBytesConfig;
            use crate::manager::{AdmissionState, PendingConnections, PreferredPeerSet};
            use dashmap::DashMap;
            use parking_lot::Mutex;
            use std::collections::HashSet;
            use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64};

            let (message_tx, _) = tokio::sync::broadcast::channel(1);
            let (scp_message_tx, _scp_rx) =
                tokio::sync::mpsc::channel(crate::manager::SCP_CHANNEL_CAPACITY);
            let (fetch_response_tx, _) = tokio::sync::mpsc::unbounded_channel();
            SharedPeerState {
                peers: Arc::new(DashMap::new()),
                flood_gate: Arc::new(crate::flood::FloodGate::new()),
                running: Arc::new(AtomicBool::new(true)),
                message_tx,
                scp_message_tx,
                fetch_response_tx,
                peer_handles: Arc::new(RwLock::new(Vec::new())),
                advertised_outbound_peers: Arc::new(RwLock::new(Vec::new())),
                advertised_inbound_peers: Arc::new(RwLock::new(Vec::new())),
                added_authenticated_peers: Arc::new(AtomicU64::new(0)),
                dropped_authenticated_peers: Arc::new(AtomicU64::new(0)),
                banned_peers: Arc::new(RwLock::new(HashSet::new())),
                peer_info_cache: Arc::new(DashMap::new()),
                peer_latest_externalized: Arc::new(DashMap::new()),
                last_closed_ledger: Arc::new(AtomicU32::new(0)),
                scp_callback: None,
                is_validator: false,
                peer_event_tx: None,
                extra_subscribers: Arc::new(RwLock::new(Vec::new())),
                is_tracking: Arc::new(AtomicBool::new(true)),
                is_synced: Arc::new(AtomicBool::new(true)),
                pending_connections: PendingConnections::new(),
                preferred_peers: Arc::new(RwLock::new(PreferredPeerSet::from_config(
                    vec![],
                    HashSet::new(),
                ))),
                preferred_peers_only: false,
                admission_state: Arc::new(Mutex::new(AdmissionState::default())),
                fetch_channel_depth: Arc::new(AtomicI64::new(0)),
                fetch_channel_depth_max: Arc::new(AtomicI64::new(0)),
                metrics: Arc::new(crate::OverlayMetrics::new()),
                query_rate_limit_window_secs: Arc::new(AtomicU64::new(60)),
                max_tx_size_bytes: Arc::new(AtomicU32::new(
                    crate::flow_control::DEFAULT_MAX_TX_SIZE_BYTES,
                )),
                flow_control_bytes_config: FlowControlBytesConfig::default(),
                peer_flood_reading_capacity: 200,
                outbound_channel_capacity: 256,
                dial_cooldowns: Arc::new(DashMap::new()),
                local_peer_id: local_id,
                next_peer_generation: Arc::new(AtomicU64::new(0)),
            }
        }

        /// Insert a fake peer handle into the shared state with a given direction.
        fn insert_fake(shared: &SharedPeerState, peer_id: &PeerId, direction: ConnectionDirection) {
            let (tx, _rx) = tokio::sync::mpsc::channel(1);
            let handle = PeerHandle {
                outbound_tx: tx,
                stats: Arc::new(PeerStats::default()),
                flow_control: Arc::new(FlowControl::new(FlowControlConfig::default())),
                direction,
                generation: shared
                    .next_peer_generation
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            };
            shared.peers.insert(peer_id.clone(), handle);
        }

        // Deterministic IDs: low < high lexicographically
        fn low_id() -> PeerId {
            PeerId::from_bytes([0x11; 32])
        }
        fn high_id() -> PeerId {
            PeerId::from_bytes([0xFF; 32])
        }

        #[tokio::test]
        async fn same_direction_inbound_rejected() {
            let local = low_id();
            let remote = high_id();
            let shared = make_shared(local);
            insert_fake(&shared, &remote, ConnectionDirection::Inbound);

            let peer = crate::peer::Peer::new_test_inbound(
                remote.clone(),
                false,
                Arc::clone(&shared.metrics),
            );
            let result = OverlayManager::try_register_peer(
                &peer,
                &remote,
                peer.info().clone(),
                &shared,
                1024,
            );
            assert!(result.is_err());
        }

        #[tokio::test]
        async fn same_direction_outbound_rejected() {
            let local = low_id();
            let remote = high_id();
            let shared = make_shared(local);
            insert_fake(&shared, &remote, ConnectionDirection::Outbound);

            let peer = crate::peer::Peer::new_test_inbound(
                remote.clone(),
                false,
                Arc::clone(&shared.metrics),
            );
            let mut info = peer.info().clone();
            info.direction = ConnectionDirection::Outbound;
            let result = OverlayManager::try_register_peer(&peer, &remote, info, &shared, 1024);
            assert!(result.is_err());
        }

        #[tokio::test]
        async fn cross_direction_local_low_inbound_replaces_outbound() {
            // local < remote → lower ID yields outbound, accepts inbound.
            // Existing = outbound, new = inbound → new wins.
            let local = low_id();
            let remote = high_id();
            let shared = make_shared(local);
            insert_fake(&shared, &remote, ConnectionDirection::Outbound);

            let peer = crate::peer::Peer::new_test_inbound(
                remote.clone(),
                false,
                Arc::clone(&shared.metrics),
            );
            let result = OverlayManager::try_register_peer(
                &peer,
                &remote,
                peer.info().clone(),
                &shared,
                1024,
            );
            let reg = result.expect("should replace outbound with inbound");
            assert!(reg.replaced.is_some());
            assert_eq!(
                reg.replaced.as_ref().unwrap().direction,
                ConnectionDirection::Outbound
            );
            assert_eq!(
                shared.peers.get(&remote).unwrap().direction,
                ConnectionDirection::Inbound
            );
        }

        #[tokio::test]
        async fn cross_direction_local_high_outbound_replaces_inbound() {
            // local > remote → higher ID keeps outbound.
            // Existing = inbound, new = outbound → new wins.
            let local = high_id();
            let remote = low_id();
            let shared = make_shared(local);
            insert_fake(&shared, &remote, ConnectionDirection::Inbound);

            let peer = crate::peer::Peer::new_test_inbound(
                remote.clone(),
                false,
                Arc::clone(&shared.metrics),
            );
            let mut info = peer.info().clone();
            info.direction = ConnectionDirection::Outbound;
            let result = OverlayManager::try_register_peer(&peer, &remote, info, &shared, 1024);
            let reg = result.expect("should replace inbound with outbound");
            assert!(reg.replaced.is_some());
            assert_eq!(
                reg.replaced.as_ref().unwrap().direction,
                ConnectionDirection::Inbound
            );
            assert_eq!(
                shared.peers.get(&remote).unwrap().direction,
                ConnectionDirection::Outbound
            );
        }

        #[tokio::test]
        async fn cross_direction_local_high_inbound_loses_to_outbound() {
            // local > remote → higher ID keeps outbound.
            // Existing = outbound, new = inbound → existing wins → reject.
            let local = high_id();
            let remote = low_id();
            let shared = make_shared(local);
            insert_fake(&shared, &remote, ConnectionDirection::Outbound);

            let peer = crate::peer::Peer::new_test_inbound(
                remote.clone(),
                false,
                Arc::clone(&shared.metrics),
            );
            let result = OverlayManager::try_register_peer(
                &peer,
                &remote,
                peer.info().clone(),
                &shared,
                1024,
            );
            assert!(result.is_err());
            assert_eq!(
                shared.peers.get(&remote).unwrap().direction,
                ConnectionDirection::Outbound
            );
        }

        #[tokio::test]
        async fn cross_direction_local_low_outbound_loses_to_inbound() {
            // local < remote → lower ID yields outbound, keeps inbound.
            // Existing = inbound, new = outbound → existing wins → reject.
            let local = low_id();
            let remote = high_id();
            let shared = make_shared(local);
            insert_fake(&shared, &remote, ConnectionDirection::Inbound);

            let peer = crate::peer::Peer::new_test_inbound(
                remote.clone(),
                false,
                Arc::clone(&shared.metrics),
            );
            let mut info = peer.info().clone();
            info.direction = ConnectionDirection::Outbound;
            let result = OverlayManager::try_register_peer(&peer, &remote, info, &shared, 1024);
            assert!(result.is_err());
            assert_eq!(
                shared.peers.get(&remote).unwrap().direction,
                ConnectionDirection::Inbound
            );
        }

        #[tokio::test]
        async fn vacant_entry_always_succeeds() {
            let local = low_id();
            let remote = high_id();
            let shared = make_shared(local);

            let peer = crate::peer::Peer::new_test_inbound(
                remote.clone(),
                false,
                Arc::clone(&shared.metrics),
            );
            let result = OverlayManager::try_register_peer(
                &peer,
                &remote,
                peer.info().clone(),
                &shared,
                1024,
            );
            let reg = result.expect("vacant entry should succeed");
            assert!(reg.replaced.is_none());
            assert!(shared.peers.contains_key(&remote));
        }

        #[tokio::test]
        async fn cleanup_peer_generation_guard() {
            let local = low_id();
            let remote = high_id();
            let shared = make_shared(local);
            insert_fake(&shared, &remote, ConnectionDirection::Outbound);

            // The fake was inserted with generation 0. Replace via tiebreaker.
            let peer = crate::peer::Peer::new_test_inbound(
                remote.clone(),
                false,
                Arc::clone(&shared.metrics),
            );
            let reg = OverlayManager::try_register_peer(
                &peer,
                &remote,
                peer.info().clone(),
                &shared,
                1024,
            )
            .expect("replacement should succeed");
            let new_gen = reg.generation;
            assert!(new_gen > 0);

            // Old peer_loop cleanup with stale generation = no-op.
            shared.cleanup_peer(&remote, 0);
            assert!(shared.peers.contains_key(&remote));

            // Correct generation cleanup removes it.
            shared.cleanup_peer(&remote, new_gen);
            assert!(!shared.peers.contains_key(&remote));
        }

        #[tokio::test]
        async fn test_mutual_dial_replacement_does_not_double_count_added_peers() {
            // Regression for #3500: the mutual-dial "new wins" replacement branch
            // reuses an already-counted authenticated slot, so it must NOT bump the
            // `added_authenticated_peers` atomic again. stellar-core v27.0.0 resolves
            // the same collision pre-auth at HELLO (Peer::recvHello ERR_CONF
            // "already-connected") so the loser is dropped while still pending —
            // never counted as added, never recordDroppedPeer'd. Net core: +1/0.
            // henyey must match: a winning replacement adds 0 (the slot was counted
            // at the displaced peer's original registration) and the stale-generation
            // cleanup drops 0.
            let local = low_id();
            let remote = high_id();
            let shared = make_shared(local);
            // Seed an existing authenticated outbound peer at generation 0. NOTE:
            // insert_fake does NOT bump the add atomic, so we sample the baseline
            // immediately before the replacing call and assert the *delta* across it.
            insert_fake(&shared, &remote, ConnectionDirection::Outbound);

            let added_before = shared.added_authenticated_peers.load(Ordering::Relaxed);
            let dropped_before = shared.dropped_authenticated_peers.load(Ordering::Relaxed);

            // Winning cross-direction inbound registration (local < remote ⇒ accept
            // inbound ⇒ new wins ⇒ replaced.is_some()).
            let peer = crate::peer::Peer::new_test_inbound(
                remote.clone(),
                false,
                Arc::clone(&shared.metrics),
            );
            let reg = OverlayManager::try_register_peer(
                &peer,
                &remote,
                peer.info().clone(),
                &shared,
                1024,
            )
            .expect("replacement should succeed");
            assert!(
                reg.replaced.is_some(),
                "this case must be a replacement to exercise the double-count path"
            );

            // The replacement reused an already-counted slot: the add atomic must
            // not have moved. On origin/main this is +1 (the bug) — the test fails.
            assert_eq!(
                shared.added_authenticated_peers.load(Ordering::Relaxed),
                added_before,
                "mutual-dial replacement must not bump added_authenticated_peers (double count)"
            );

            // Stale-generation cleanup of the displaced peer (the caller drops the
            // old handle; its peer_loop calls cleanup_peer with the old generation):
            // the new generation is installed, so this is a no-op and must not bump
            // the drop atomic.
            shared.cleanup_peer(&remote, 0);
            assert_eq!(
                shared.dropped_authenticated_peers.load(Ordering::Relaxed),
                dropped_before,
                "stale-generation cleanup must not bump dropped_authenticated_peers"
            );
            assert!(shared.peers.contains_key(&remote));
        }

        #[tokio::test]
        async fn test_normal_registration_still_counts_added_peer() {
            // Guard against over-correcting the fix: a non-replacing registration
            // (vacant entry ⇒ replaced.is_none()) must still bump the add atomic by
            // exactly 1.
            let local = low_id();
            let remote = high_id();
            let shared = make_shared(local);

            let added_before = shared.added_authenticated_peers.load(Ordering::Relaxed);
            let peer = crate::peer::Peer::new_test_inbound(
                remote.clone(),
                false,
                Arc::clone(&shared.metrics),
            );
            let reg = OverlayManager::try_register_peer(
                &peer,
                &remote,
                peer.info().clone(),
                &shared,
                1024,
            )
            .expect("vacant entry should succeed");
            assert!(reg.replaced.is_none());
            assert_eq!(
                shared.added_authenticated_peers.load(Ordering::Relaxed),
                added_before + 1,
                "a normal (non-replacement) registration must still count the added peer"
            );
        }

        #[tokio::test]
        async fn test_mutual_dial_loser_does_not_count() {
            // The mutual-dial "existing wins" branch returns Err(AlreadyConnected)
            // BEFORE the add bump, so a rejected duplicate must leave the add atomic
            // unchanged. local > remote ⇒ keep existing outbound ⇒ reject new inbound.
            let local = high_id();
            let remote = low_id();
            let shared = make_shared(local);
            insert_fake(&shared, &remote, ConnectionDirection::Outbound);

            let added_before = shared.added_authenticated_peers.load(Ordering::Relaxed);
            let peer = crate::peer::Peer::new_test_inbound(
                remote.clone(),
                false,
                Arc::clone(&shared.metrics),
            );
            let result = OverlayManager::try_register_peer(
                &peer,
                &remote,
                peer.info().clone(),
                &shared,
                1024,
            );
            assert!(result.is_err());
            assert_eq!(
                shared.added_authenticated_peers.load(Ordering::Relaxed),
                added_before,
                "a rejected mutual-dial loser must not bump added_authenticated_peers"
            );
        }
    }
}
