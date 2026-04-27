//! Tick loop and periodic maintenance for the overlay manager.
//!
//! Contains the main tick loop (`start_tick_loop`), DNS resolution, preferred
//! peer connection, outbound slot filling, random peer dropping, and peer
//! advertisement.

use super::{OverlayManager, PeerHandle, PreferredPeerSet, SharedPeerState, TickConnectCtx};
use crate::{connection::ConnectionPool, peer::PeerInfo, PeerAddress, PeerId};
use dashmap::DashMap;
use parking_lot::RwLock;
use rand::seq::SliceRandom;
use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};
use stellar_xdr::curr::ErrorCode;
use tokio::task::JoinHandle;
use tracing::{debug, error, info, warn};

/// Interval between overlay maintenance ticks (3 seconds).
///
/// Matches stellar-core `PEER_AUTHENTICATION_TIMEOUT + 1` (2s + 1s = 3s).
/// Each tick: DNS check, connect preferred peers, maybe drop random peer,
/// fill outbound slots.
pub(super) const TICK_INTERVAL: Duration = Duration::from_secs(3);

/// Delay between successful DNS re-resolution cycles (600 seconds / 10 minutes).
///
/// Matches stellar-core `PEER_IP_RESOLVE_DELAY`.
pub(super) const PEER_IP_RESOLVE_DELAY: Duration = Duration::from_secs(600);

/// Base delay for DNS retry backoff (10 seconds).
///
/// On each consecutive failure, the delay increases linearly:
/// retry_count * PEER_IP_RESOLVE_RETRY_DELAY, until it exceeds
/// PEER_IP_RESOLVE_DELAY, at which point retries stop.
///
/// Matches stellar-core `PEER_IP_RESOLVE_RETRY_DELAY`.
pub(super) const PEER_IP_RESOLVE_RETRY_DELAY: Duration = Duration::from_secs(10);

/// Delay before retrying a failed outbound connection attempt.
const OUTBOUND_CONNECT_RETRY_DELAY: Duration = Duration::from_secs(10);

/// Result of a background DNS resolution of configured peers.
struct ResolvedPeers {
    /// Successfully resolved known peers (hostname → IP:port).
    known: Vec<PeerAddress>,
    /// Successfully resolved preferred peers (hostname → IP:port).
    preferred: Vec<PeerAddress>,
    /// True if any peer in either list failed to resolve.
    errors: bool,
}

/// Resolve a list of peer addresses, performing DNS lookup for hostnames.
///
/// Returns the resolved addresses and a flag indicating whether any
/// resolution failed. Failed peers are skipped but the rest are still
/// returned, matching stellar-core's `resolvePeers()`.
pub(super) async fn resolve_peer_list(peers: &[PeerAddress]) -> (Vec<PeerAddress>, bool) {
    let mut resolved = Vec::with_capacity(peers.len());
    let mut errors = false;

    for peer in peers {
        // If the host is already an IP address, keep as-is.
        if peer.host.parse::<IpAddr>().is_ok() {
            resolved.push(peer.clone());
            continue;
        }

        // Resolve hostname to IP address.
        let lookup_result = tokio::net::lookup_host((peer.host.as_str(), peer.port)).await;
        match lookup_result {
            Ok(addrs) => {
                // Take the first IPv4 address, matching stellar-core behavior.
                let ipv4_addr = addrs.into_iter().find(|a| a.is_ipv4());
                if let Some(socket_addr) = ipv4_addr {
                    resolved.push(PeerAddress::from(socket_addr));
                    tracing::trace!(
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

fn spawn_dns_resolution(
    known_peers: Vec<PeerAddress>,
    preferred_peers: Vec<PeerAddress>,
) -> JoinHandle<ResolvedPeers> {
    tokio::spawn(async move {
        let (known, known_err) = resolve_peer_list(&known_peers).await;
        let (preferred, pref_err) = resolve_peer_list(&preferred_peers).await;
        ResolvedPeers {
            known,
            preferred,
            errors: known_err || pref_err,
        }
    })
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
pub(super) fn compute_dns_backoff_delay(
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

impl OverlayManager {
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
    pub(super) fn start_tick_loop(&mut self) {
        let shared = self.shared_state();
        let pool = Arc::clone(&self.outbound_pool);
        let inbound_pool = Arc::clone(&self.inbound_pool);
        let known_peers = Arc::clone(&self.known_peers);
        let preferred_peers_config = self.config.preferred_peers.clone();
        let max_outbound = self.config.max_outbound_peers;
        let config_known_peers = self.config.known_peers.clone();
        let mut shutdown_rx = self.shutdown_tx.lock().as_ref().unwrap().subscribe();
        let ctx = TickConnectCtx {
            local_node: self.local_node.clone(),
            connect_timeout: self.config.connect_timeout_secs,
            auth_timeout: self.config.auth_timeout_secs,
            target_outbound: self.config.target_outbound_peers,
            connection_factory: Arc::clone(&self.connection_factory),
        };

        let handle = tokio::spawn(async move {
            let mut retry_after: HashMap<PeerAddress, Instant> = HashMap::new();
            let mut interval = tokio::time::interval(TICK_INTERVAL);
            // G8: Track when we first noticed we were out of sync, for
            // random-peer-drop cooldown (OUT_OF_SYNC_RECONNECT_DELAY = 60s).
            let mut last_out_of_sync_reconnect: Option<Instant> = None;

            // G7: DNS re-resolution state.
            let mut dns_resolving_with_backoff = true;
            let mut dns_retry_count: u32 = 0;
            let mut dns_next_resolve_at = Instant::now();

            // Current preferred peer set snapshot — starts from config, updated
            // after each DNS resolution cycle.
            let mut preferred_set = Arc::new(PreferredPeerSet::from_config(
                preferred_peers_config.clone(),
            ));

            // Trigger initial DNS resolution.
            let mut dns_resolve_handle: Option<JoinHandle<ResolvedPeers>> = Some(
                spawn_dns_resolution(config_known_peers.clone(), preferred_peers_config.clone()),
            );

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

                // G7: Collect completed DNS resolution and schedule next.
                // Also updates the preferred peer set and inbound pool IPs.
                Self::maybe_collect_dns_result(
                    &mut dns_resolve_handle,
                    &known_peers,
                    &mut preferred_set,
                    &inbound_pool,
                    &shared.preferred_peers,
                    &mut dns_resolving_with_backoff,
                    &mut dns_retry_count,
                    &mut dns_next_resolve_at,
                )
                .await;

                // G8: Maybe drop a random non-preferred outbound peer when out of sync.
                // Run this after DNS collection so preferred classification uses
                // the freshest resolved IPs available for this tick.
                let tracking = shared.is_tracking.load(Ordering::Relaxed);
                Self::maybe_drop_random_peer(
                    &shared.peers,
                    &shared.peer_info_cache,
                    &preferred_set,
                    max_outbound,
                    tracking,
                    &mut last_out_of_sync_reconnect,
                );

                // Sweep stale pending connection reservations.
                shared.pending_connections.sweep_stale();

                if dns_resolve_handle.is_none() && Instant::now() >= dns_next_resolve_at {
                    dns_resolve_handle = Some(spawn_dns_resolution(
                        config_known_peers.clone(),
                        preferred_peers_config.clone(),
                    ));
                }

                let now = Instant::now();
                let outbound_count = Self::count_outbound_peers(&shared.peer_info_cache);
                let available = max_outbound.saturating_sub(outbound_count);

                // Preferred peers can evict non-preferred outbound peers, so
                // their dial capacity includes replaceable (non-preferred) slots.
                // Matches stellar-core OverlayManagerImpl.cpp:748-749:
                //   preferredToConnect = availableAuthenticatedSlots
                //                      + nonPreferredAuthenticatedCount();
                let non_preferred_count = Self::count_non_preferred_outbound_peers(
                    &shared.peer_info_cache,
                    &preferred_set,
                );
                let preferred_capacity = available + non_preferred_count;

                // Connect to preferred peers first. Preferred peers may evict
                // non-preferred outbound peers later, at authenticated admission.
                if preferred_capacity > 0 {
                    Self::connect_preferred_peers(
                        &preferred_set,
                        &mut retry_after,
                        now,
                        preferred_capacity,
                        max_outbound,
                        &pool,
                        &shared,
                        &ctx,
                    )
                    .await;
                }

                // Recount after preferred connections may have changed things.
                let outbound_count = Self::count_outbound_peers(&shared.peer_info_cache);
                let remaining = max_outbound.saturating_sub(outbound_count);
                if remaining == 0 || outbound_count >= ctx.target_outbound {
                    continue;
                }

                // Fill remaining outbound slots from known peers.
                Self::fill_outbound_slots(
                    &known_peers,
                    &mut retry_after,
                    now,
                    remaining,
                    &pool,
                    &shared,
                    &ctx,
                )
                .await;
            }
        });

        self.connector_handle = Some(handle);
    }

    /// Check if a background DNS resolution has completed and apply results.
    ///
    /// Merges newly-resolved peers into `known_peers`, updates the preferred
    /// peer set with resolved IPs, and refreshes the inbound pool's preferred
    /// IPs. Computes the next resolve delay using backoff logic.
    #[allow(clippy::too_many_arguments)]
    async fn maybe_collect_dns_result(
        dns_resolve_handle: &mut Option<JoinHandle<ResolvedPeers>>,
        known_peers: &RwLock<Vec<PeerAddress>>,
        preferred_set: &mut Arc<PreferredPeerSet>,
        inbound_pool: &Arc<ConnectionPool>,
        shared_preferred_peers: &Arc<RwLock<PreferredPeerSet>>,
        dns_resolving_with_backoff: &mut bool,
        dns_retry_count: &mut u32,
        dns_next_resolve_at: &mut Instant,
    ) {
        let handle_ref = match dns_resolve_handle.as_ref() {
            Some(h) if h.is_finished() => dns_resolve_handle.take().unwrap(),
            _ => return,
        };
        match handle_ref.await {
            Ok(result) => {
                // Merge resolved known peers (add new, keep existing).
                {
                    let mut kp = known_peers.write();
                    for addr in &result.known {
                        if kp.len() >= super::MAX_KNOWN_PEERS {
                            break;
                        }
                        if !kp
                            .iter()
                            .any(|p| p.host == addr.host && p.port == addr.port)
                        {
                            kp.push(addr.clone());
                        }
                    }
                }

                // Update preferred peer set with DNS-resolved addresses.
                let new_set = preferred_set.with_resolved(result.preferred);
                let new_ips = new_set.resolved_ips().clone();
                *preferred_set = Arc::new(new_set);
                *shared_preferred_peers.write() = (**preferred_set).clone();

                // Update inbound pool so resolved preferred peers get extra slots.
                inbound_pool.update_preferred_ips(new_ips);

                let (delay, new_backoff, new_retry) = compute_dns_backoff_delay(
                    *dns_resolving_with_backoff,
                    *dns_retry_count,
                    result.errors,
                );
                *dns_resolving_with_backoff = new_backoff;
                *dns_retry_count = new_retry;
                *dns_next_resolve_at = Instant::now() + delay;
                debug!(
                    "DNS resolution complete (errors={}, preferred_ips={}), next in {:?}",
                    result.errors,
                    preferred_set.resolved_ips().len(),
                    delay
                );
            }
            Err(e) => {
                error!("DNS resolution task panicked: {}", e);
                *dns_next_resolve_at = Instant::now() + PEER_IP_RESOLVE_RETRY_DELAY;
            }
        }
    }

    /// Try connecting to each preferred peer that isn't already connected.
    ///
    /// Returns how many slots remain after connecting.
    ///
    /// Matches stellar-core's preferred connection logic in
    /// `OverlayManagerImpl.cpp:744-757`.
    #[allow(clippy::too_many_arguments)]
    async fn connect_preferred_peers(
        preferred_set: &PreferredPeerSet,
        retry_after: &mut HashMap<PeerAddress, Instant>,
        now: Instant,
        mut remaining: usize,
        _max_outbound: usize,
        pool: &Arc<ConnectionPool>,
        shared: &SharedPeerState,
        ctx: &TickConnectCtx,
    ) -> usize {
        // Shuffle to avoid starvation — stellar-core uses random selection.
        let entries = preferred_set.shuffled_config_entries(&mut rand::thread_rng());
        for addr in &entries {
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
                debug!("Outbound peer pending limit reached");
                return 0;
            }

            let timeout = ctx.connect_timeout.max(ctx.auth_timeout);
            match super::connection::connect_to_explicit_peer(
                addr,
                ctx.local_node.clone(),
                timeout,
                Arc::clone(pool),
                shared.clone(),
                Arc::clone(&ctx.connection_factory),
            )
            .await
            {
                Ok(_) => {
                    retry_after.remove(addr);
                    remaining = remaining.saturating_sub(1);
                }
                Err(e) => {
                    warn!("Failed to connect to preferred peer {}: {}", addr, e);
                    retry_after.insert(addr.clone(), now + OUTBOUND_CONNECT_RETRY_DELAY);
                }
            }
        }
        remaining
    }

    /// Fill remaining outbound slots from the shuffled known-peer list.
    async fn fill_outbound_slots(
        known_peers: &RwLock<Vec<PeerAddress>>,
        retry_after: &mut HashMap<PeerAddress, Instant>,
        now: Instant,
        mut remaining: usize,
        pool: &Arc<ConnectionPool>,
        shared: &SharedPeerState,
        ctx: &TickConnectCtx,
    ) {
        let mut known_snapshot = known_peers.read().clone();
        known_snapshot.shuffle(&mut rand::thread_rng());

        for addr in &known_snapshot {
            if remaining == 0 {
                break;
            }

            let outbound_now = Self::count_outbound_peers(&shared.peer_info_cache);
            if outbound_now >= ctx.target_outbound {
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

            let timeout = ctx.connect_timeout.max(ctx.auth_timeout);
            match super::connection::connect_to_explicit_peer(
                addr,
                ctx.local_node.clone(),
                timeout,
                Arc::clone(pool),
                shared.clone(),
                Arc::clone(&ctx.connection_factory),
            )
            .await
            {
                Ok(_) => {
                    retry_after.remove(addr);
                    remaining = remaining.saturating_sub(1);
                }
                Err(e) => {
                    debug!("Failed to connect to peer {}: {}", addr, e);
                    retry_after.insert(addr.clone(), now + OUTBOUND_CONNECT_RETRY_DELAY);
                }
            }
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
    pub(super) fn maybe_drop_random_peer(
        peers: &DashMap<PeerId, PeerHandle>,
        peer_info_cache: &DashMap<PeerId, PeerInfo>,
        preferred_set: &PreferredPeerSet,
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
                    if !preferred_set.is_preferred(info) {
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
                        if !super::peer_loop::send_error_and_drop(
                            &peer_id,
                            &entry.value().outbound_tx,
                            ErrorCode::Load,
                            "random disconnect due to out of sync",
                        ) {
                            return false;
                        }
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connection::{Connection, ConnectionDirection, ConnectionPool, Listener};
    use crate::connection_factory::ConnectionFactory;
    use crate::flow_control::{FlowControl, FlowControlConfig};
    use crate::peer::{PeerInfo, PeerStats};
    use crate::{LocalNode, OverlayConfig, OverlayError, PeerAddress, Result};
    use async_trait::async_trait;
    use henyey_crypto::SecretKey;
    use tokio::sync::mpsc;

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

    fn register_fake_peer_with_rx(
        peers: &DashMap<PeerId, PeerHandle>,
        info_cache: &DashMap<PeerId, PeerInfo>,
        info: PeerInfo,
    ) -> mpsc::Receiver<super::super::OutboundMessage> {
        let (tx, rx) = mpsc::channel(8);
        let peer_id = info.peer_id.clone();
        let handle = PeerHandle {
            outbound_tx: tx,
            stats: Arc::new(PeerStats::default()),
            flow_control: Arc::new(FlowControl::new(FlowControlConfig::default())),
        };
        peers.insert(peer_id.clone(), handle);
        info_cache.insert(peer_id, info);
        rx
    }

    #[derive(Debug)]
    struct FailingConnectionFactory;

    #[async_trait]
    impl ConnectionFactory for FailingConnectionFactory {
        async fn connect(&self, addr: &PeerAddress, _timeout_secs: u64) -> Result<Connection> {
            Err(OverlayError::ConnectionFailed(format!(
                "intentional failure for {addr}"
            )))
        }

        async fn bind(&self, _port: u16) -> Result<Listener> {
            Err(OverlayError::ConnectionFailed(
                "bind not used in test".to_string(),
            ))
        }
    }

    #[tokio::test]
    async fn test_preferred_connect_failure_does_not_pre_evict() {
        let preferred_addr = PeerAddress::new("10.0.0.1", 11625);
        let mut config = OverlayConfig::default();
        config.max_outbound_peers = 1;
        config.target_outbound_peers = 1;
        config.preferred_peers = vec![preferred_addr.clone()];
        let local_node = LocalNode::new_testnet(SecretKey::generate());
        let manager = OverlayManager::new_with_connection_factory(
            config,
            local_node.clone(),
            Arc::new(FailingConnectionFactory),
        )
        .unwrap();
        let shared = manager.shared_state();

        assert!(manager.outbound_pool.try_reserve());
        manager.outbound_pool.force_promote_authenticated();
        let mut victim_info = make_peer_info(ConnectionDirection::Outbound, 11626);
        victim_info.address = "10.0.0.99:11625".parse().unwrap();
        let mut victim_rx =
            register_fake_peer_with_rx(&shared.peers, &shared.peer_info_cache, victim_info);

        let preferred_set = PreferredPeerSet::from_config(vec![preferred_addr.clone()]);
        let mut retry_after = HashMap::new();
        let ctx = TickConnectCtx {
            local_node,
            connect_timeout: 1,
            auth_timeout: 1,
            target_outbound: 1,
            connection_factory: Arc::new(FailingConnectionFactory),
        };

        let remaining = OverlayManager::connect_preferred_peers(
            &preferred_set,
            &mut retry_after,
            Instant::now(),
            1,
            1,
            &manager.outbound_pool,
            &shared,
            &ctx,
        )
        .await;

        assert_eq!(
            remaining, 1,
            "failed preferred dial should not consume capacity"
        );
        assert_eq!(manager.outbound_pool.authenticated_count(), 1);
        assert!(
            victim_rx.try_recv().is_err(),
            "failed preferred dial must not evict before authentication"
        );
        assert!(
            retry_after.contains_key(&preferred_addr),
            "failed preferred dial should still enter retry backoff"
        );
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
            &PreferredPeerSet::from_config(vec![]),
            8,    // max_outbound = 8 (full)
            true, // tracking = true
            &mut last_reconnect,
        );
        assert!(!dropped, "should not drop when tracking");
        assert!(
            last_reconnect.is_none(),
            "timer should be cleared when tracking"
        );
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
            &PreferredPeerSet::from_config(vec![]),
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
            &PreferredPeerSet::from_config(vec![]),
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
            &PreferredPeerSet::from_config(vec![]),
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
            &PreferredPeerSet::from_config(vec![]),
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
            &PreferredPeerSet::from_config(preferred.clone()),
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
            &PreferredPeerSet::from_config(vec![]),
            8,
            false,
            &mut last_reconnect,
        );
        assert!(
            !dropped,
            "should not drop when outbound not full (inbound don't count)"
        );
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
            &PreferredPeerSet::from_config(vec![]),
            8,
            true, // tracking again
            &mut last_reconnect,
        );
        assert!(!dropped);
        assert!(
            last_reconnect.is_none(),
            "timer should be cleared when back in sync"
        );
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
        let peers = vec![PeerAddress::new(
            "this-does-not-exist-at-all.invalid",
            11625,
        )];
        let (resolved, errors) = resolve_peer_list(&peers).await;
        assert!(errors, "unresolvable hostname should set errors flag");
        assert!(
            resolved.is_empty(),
            "failed hostname should not produce a result"
        );
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
        assert_eq!(
            delay,
            Duration::from_secs(600),
            "should cap at PEER_IP_RESOLVE_DELAY"
        );
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
        let max_retries = PEER_IP_RESOLVE_DELAY.as_secs() / PEER_IP_RESOLVE_RETRY_DELAY.as_secs();
        assert_eq!(max_retries, 60);
    }

    // ---- AUDIT-182 tests: preferred-peer eviction when slots full ----

    #[test]
    fn test_count_non_preferred_outbound_peers_mixed() {
        let info_cache: DashMap<PeerId, PeerInfo> = DashMap::new();

        // 3 outbound non-preferred peers
        for i in 0..3 {
            let info = make_peer_info(ConnectionDirection::Outbound, 11000 + i);
            info_cache.insert(info.peer_id.clone(), info);
        }

        // 2 outbound preferred peers (127.0.0.1:11100, 127.0.0.1:11101)
        let preferred_addrs: Vec<PeerAddress> = (0..2)
            .map(|i| PeerAddress::new("127.0.0.1", 11100 + i))
            .collect();
        for i in 0..2u16 {
            let info = make_peer_info(ConnectionDirection::Outbound, 11100 + i);
            info_cache.insert(info.peer_id.clone(), info);
        }

        // 2 inbound peers (should be ignored)
        for i in 0..2 {
            let info = make_peer_info(ConnectionDirection::Inbound, 12000 + i);
            info_cache.insert(info.peer_id.clone(), info);
        }

        let preferred_set = PreferredPeerSet::from_config(preferred_addrs);
        let count = OverlayManager::count_non_preferred_outbound_peers(&info_cache, &preferred_set);
        assert_eq!(count, 3, "should only count non-preferred outbound peers");
    }

    #[test]
    fn test_count_non_preferred_outbound_all_preferred() {
        let info_cache: DashMap<PeerId, PeerInfo> = DashMap::new();

        let preferred_addrs: Vec<PeerAddress> = (0..3)
            .map(|i| PeerAddress::new("127.0.0.1", 11000 + i))
            .collect();
        for i in 0..3u16 {
            let info = make_peer_info(ConnectionDirection::Outbound, 11000 + i);
            info_cache.insert(info.peer_id.clone(), info);
        }

        let preferred_set = PreferredPeerSet::from_config(preferred_addrs);
        let count = OverlayManager::count_non_preferred_outbound_peers(&info_cache, &preferred_set);
        assert_eq!(count, 0, "all outbound are preferred");
    }

    #[test]
    fn test_preferred_capacity_includes_non_preferred_replaceable_slots() {
        // Regression test for AUDIT-182: when all outbound slots are full of
        // non-preferred peers, preferred_capacity should be > 0 so preferred
        // dials are attempted. Eviction now happens later, at admission.
        let info_cache: DashMap<PeerId, PeerInfo> = DashMap::new();
        let max_outbound: usize = 8;

        // Fill all 8 slots with non-preferred outbound peers
        for i in 0..8u16 {
            let info = make_peer_info(ConnectionDirection::Outbound, 11000 + i);
            info_cache.insert(info.peer_id.clone(), info);
        }

        let preferred_set = PreferredPeerSet::from_config(vec![]);
        let outbound_count = OverlayManager::count_outbound_peers(&info_cache);
        let available = max_outbound.saturating_sub(outbound_count);
        assert_eq!(available, 0, "no free slots");

        let non_preferred_count =
            OverlayManager::count_non_preferred_outbound_peers(&info_cache, &preferred_set);
        let preferred_capacity = available + non_preferred_count;
        assert_eq!(
            preferred_capacity, 8,
            "preferred_capacity should include replaceable non-preferred slots"
        );
    }

    #[test]
    fn test_preferred_capacity_zero_when_all_preferred() {
        // When all outbound slots are full of preferred peers,
        // preferred_capacity should be 0 (no replaceable slots).
        let info_cache: DashMap<PeerId, PeerInfo> = DashMap::new();
        let max_outbound: usize = 3;

        let preferred_addrs: Vec<PeerAddress> = (0..3)
            .map(|i| PeerAddress::new("127.0.0.1", 11000 + i))
            .collect();
        for i in 0..3u16 {
            let info = make_peer_info(ConnectionDirection::Outbound, 11000 + i);
            info_cache.insert(info.peer_id.clone(), info);
        }

        let preferred_set = PreferredPeerSet::from_config(preferred_addrs);
        let outbound_count = OverlayManager::count_outbound_peers(&info_cache);
        let available = max_outbound.saturating_sub(outbound_count);
        let non_preferred_count =
            OverlayManager::count_non_preferred_outbound_peers(&info_cache, &preferred_set);
        let preferred_capacity = available + non_preferred_count;
        assert_eq!(
            preferred_capacity, 0,
            "no replaceable slots when all are preferred"
        );
    }

    #[test]
    fn test_eviction_trigger_uses_authenticated_count_not_try_reserve() {
        // Regression test: eviction should fire based on authenticated_count
        // >= max_outbound, not on try_reserve() failure (which succeeds due
        // to pending headroom).
        let pool = Arc::new(ConnectionPool::new(2)); // max_connections = 2

        // Simulate 2 authenticated connections (pool is "full" for authenticated)
        assert!(pool.try_reserve()); // pending slot 1
        pool.force_promote_authenticated(); // promote to authenticated
        assert!(pool.try_reserve()); // pending slot 2
        pool.force_promote_authenticated(); // promote to authenticated

        assert_eq!(pool.authenticated_count(), 2);

        // try_reserve() still succeeds due to pending headroom (max_pending_extra = 32)
        assert!(
            pool.try_reserve(),
            "try_reserve succeeds even with all authenticated slots full"
        );
        pool.release_pending(); // clean up the test reservation

        // The eviction condition should trigger on authenticated_count >= max
        let needs_eviction = pool.authenticated_count() >= 2;
        assert!(
            needs_eviction,
            "eviction should trigger based on authenticated count"
        );
    }
}
