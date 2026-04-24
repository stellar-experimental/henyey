//! Peer management: topology queries, peer info aggregation, and overlay statistics.

use super::*;

/// Error returned when disconnecting a peer fails.
#[derive(Debug, thiserror::Error)]
pub enum DisconnectError {
    #[error("overlay not available")]
    OverlayUnavailable,
    #[error("peer not found")]
    PeerNotFound,
}

impl App {
    pub async fn peer_snapshots(&self) -> Vec<PeerSnapshot> {
        match self.overlay().await {
            Some(overlay) => overlay.peer_snapshots(),
            None => Vec::new(),
        }
    }

    /// Get peer counts: `(pending_count, authenticated_count)`.
    pub async fn peer_counts(&self) -> (usize, usize) {
        match self.overlay().await {
            Some(overlay) => overlay.peer_counts(),
            None => (0, 0),
        }
    }

    pub async fn connect_peer(&self, addr: PeerAddress) -> anyhow::Result<PeerId> {
        let overlay = self
            .overlay()
            .await
            .ok_or_else(|| anyhow::anyhow!("Overlay manager not available"))?;
        overlay.connect(&addr).await.map_err(|e| anyhow::anyhow!(e))
    }

    pub async fn disconnect_peer(&self, peer_id: &PeerId) -> Result<(), DisconnectError> {
        let overlay = self
            .overlay()
            .await
            .ok_or(DisconnectError::OverlayUnavailable)?;
        if overlay.disconnect(peer_id).await {
            Ok(())
        } else {
            Err(DisconnectError::PeerNotFound)
        }
    }

    pub async fn ban_peer(&self, peer_id: PeerId) -> anyhow::Result<()> {
        let Some(strkey) = Self::peer_id_to_strkey(&peer_id) else {
            anyhow::bail!("Invalid peer id");
        };
        self.db_blocking("ban-peer", move |db| {
            db.ban_node(&strkey)?;
            Ok(())
        })
        .await?;
        let overlay = self
            .overlay()
            .await
            .ok_or_else(|| anyhow::anyhow!("Overlay manager not available"))?;
        overlay.ban_peer(peer_id).await;
        Ok(())
    }

    pub async fn unban_peer(&self, peer_id: &PeerId) -> anyhow::Result<bool> {
        let Some(strkey) = Self::peer_id_to_strkey(peer_id) else {
            anyhow::bail!("Invalid peer id");
        };
        self.db_blocking("unban-peer", move |db| {
            db.unban_node(&strkey)?;
            Ok(())
        })
        .await?;
        let overlay = self
            .overlay()
            .await
            .ok_or_else(|| anyhow::anyhow!("Overlay manager not available"))?;
        Ok(overlay.unban_peer(peer_id))
    }

    pub async fn banned_peers(&self) -> anyhow::Result<Vec<PeerId>> {
        let bans = self
            .db_blocking("load-bans", |db| db.load_bans().map_err(Into::into))
            .await?;
        let mut peers = Vec::new();
        for ban in bans {
            if let Some(peer_id) = Self::strkey_to_peer_id(&ban) {
                peers.push(peer_id);
            } else {
                tracing::warn!(node = %ban, "Ignoring invalid ban entry");
            }
        }
        Ok(peers)
    }

    /// Maintain peer connections - reconnect if peer count drops too low.
    ///
    /// IMPORTANT: This function must NOT hold the overlay lock during connection
    /// attempts, because each connect can take 30-90 seconds. Holding the lock
    /// would block the entire main event loop.
    pub(super) async fn maintain_peers(&self) {
        let max_failures = self.config.overlay.peer_max_failures;
        let _ = self
            .db_blocking("remove-failed-peers", move |db| {
                db.remove_peers_with_failures(max_failures)?;
                Ok(())
            })
            .await;

        // Phase 1: Acquire lock briefly to check peer count and collect candidates.
        let (_peer_count, _target_outbound, candidates) = {
            let Some(overlay) = self.overlay().await else {
                return;
            };

            let peer_count = overlay.peer_count();
            let min_peers = 3;

            if peer_count >= min_peers {
                return;
            }

            tracing::info!(
                peer_count,
                min_peers,
                "Peer count below threshold, reconnecting to known peers"
            );

            let candidates = self.refresh_known_peers(&overlay).await;
            let target = self.config.overlay.target_outbound_peers;
            (peer_count, target, candidates)
        };

        // Phase 2: Connect to candidates concurrently WITHOUT holding the overlay lock.
        // Each connect acquires the lock briefly and independently.
        // Use an overall timeout to keep the main loop responsive.
        let overlay_for_connects = {
            let Some(overlay) = self.overlay().await else {
                return;
            };
            Arc::clone(&overlay)
        };

        let connect_futures: Vec<_> = candidates
            .into_iter()
            .map(|addr| {
                let overlay = Arc::clone(&overlay_for_connects);
                async move {
                    match tokio::time::timeout(
                        Duration::from_secs(15),
                        overlay.connect(&addr),
                    )
                    .await
                    {
                        Ok(Ok(_)) => {
                            tracing::debug!(addr = %addr, "Reconnected to peer");
                            true
                        }
                        Ok(Err(e)) => {
                            tracing::debug!(addr = %addr, error = %e, "Failed to reconnect to peer");
                            false
                        }
                        Err(_) => {
                            tracing::debug!(addr = %addr, "Peer connection timed out (15s)");
                            false
                        }
                    }
                }
            })
            .collect();

        // Overall timeout: 20s for all connects combined
        let reconnected = match tokio::time::timeout(
            Duration::from_secs(20),
            futures::future::join_all(connect_futures),
        )
        .await
        {
            Ok(results) => results.into_iter().any(|ok| ok),
            Err(_) => {
                tracing::debug!("Overall maintain_peers connect timeout (20s)");
                false
            }
        };

        if reconnected {
            // Give peers time to complete handshake
            self.clock.sleep(Duration::from_millis(200)).await;
            self.request_scp_state_and_record().await;
        }
    }

    fn next_ping_hash(&self) -> Hash256 {
        let counter = self.ping_counter.fetch_add(1, Ordering::Relaxed);
        Hash256::hash(&counter.to_be_bytes())
    }

    pub(super) async fn send_peer_pings(&self) {
        const PING_TIMEOUT: Duration = Duration::from_secs(60);

        // Phase 1: Collect snapshots (no long-lived lock needed).
        let snapshots = {
            let Some(overlay) = self.overlay().await else {
                return;
            };
            overlay.peer_snapshots()
        };

        if snapshots.is_empty() {
            return;
        }

        // Phase 2: Build the to_ping list (no overlay lock needed).
        let now = self.clock.now();
        let to_ping = {
            let mut pings = self.ping_state.lock().await;
            pings.expire_timeouts(now, PING_TIMEOUT);

            let mut to_ping = Vec::new();
            for snapshot in snapshots {
                let hash = self.next_ping_hash();
                if pings.try_mark_sent(snapshot.info.peer_id.clone(), hash, self.clock.now()) {
                    to_ping.push((snapshot.info.peer_id, hash));
                }
            }
            to_ping
        };

        // Phase 3: Send pings concurrently.
        let Some(overlay) = self.overlay().await else {
            return;
        };

        for (peer, hash) in to_ping {
            let msg = StellarMessage::GetScpQuorumset(stellar_xdr::curr::Uint256(hash.0));
            if overlay.try_send_to(&peer, msg).is_err() {
                tracing::debug!(peer = %peer, "Failed to send ping");
                self.ping_state
                    .lock()
                    .await
                    .cleanup_failed_send(&peer, &hash);
            }
        }
    }

    pub(super) async fn process_ping_response(
        &self,
        peer_id: &henyey_overlay::PeerId,
        hash: [u8; 32],
    ) {
        let hash = Hash256::from_bytes(hash);
        let info = self.ping_state.lock().await.remove_response(&hash);

        let Some(info) = info else {
            return;
        };

        if &info.peer_id != peer_id {
            return;
        }

        let latency_ms = info.sent_at.elapsed().as_millis() as u64;
        let mut survey_data = self.survey_data.write().await;
        survey_data.record_peer_latency(peer_id, latency_ms);
    }

    /// Process a peer list received from the network.
    pub(super) async fn process_peer_list(
        &self,
        peer_list: stellar_xdr::curr::VecM<stellar_xdr::curr::PeerAddress, 100>,
    ) {
        let Some(overlay) = self.overlay().await else {
            return;
        };

        // Convert XDR peer addresses to our PeerAddress format
        let addrs: Vec<PeerAddress> = peer_list
            .iter()
            .filter_map(|xdr_addr| {
                // Extract IP address from the XDR type
                let ip = match &xdr_addr.ip {
                    stellar_xdr::curr::PeerAddressIp::IPv4(bytes) => {
                        format!("{}.{}.{}.{}", bytes[0], bytes[1], bytes[2], bytes[3])
                    }
                    stellar_xdr::curr::PeerAddressIp::IPv6(_) => {
                        return None;
                    }
                };

                let port = xdr_addr.port;

                // Skip obviously invalid addresses
                if port == 0 || port > u16::MAX as u32 {
                    return None;
                }

                Some(PeerAddress::new(ip, port as u16))
            })
            .collect();

        let addrs = self.filter_discovered_peers(addrs).await;

        if !addrs.is_empty() {
            self.persist_peers(&addrs).await;
            let count = overlay.add_peers(addrs).await;
            if count > 0 {
                tracing::info!(added = count, "Added peers from discovery");
            }
        }

        let _ = self.refresh_known_peers(&overlay).await;
    }

    pub(super) fn parse_peer_address(value: &str) -> Option<PeerAddress> {
        let parts: Vec<&str> = value.split(':').collect();
        match parts.len() {
            1 => Some(PeerAddress::new(parts[0], 11625)),
            2 => parts[1]
                .parse()
                .ok()
                .map(|port| PeerAddress::new(parts[0], port)),
            _ => None,
        }
    }

    fn peer_id_to_strkey(peer_id: &PeerId) -> Option<String> {
        henyey_crypto::PublicKey::from_bytes(peer_id.as_bytes())
            .ok()
            .map(|pk| pk.to_strkey())
    }

    pub(super) fn strkey_to_peer_id(value: &str) -> Option<PeerId> {
        henyey_crypto::PublicKey::from_strkey(value)
            .ok()
            .map(|pk| PeerId::from_bytes(*pk.as_bytes()))
    }

    pub(super) async fn load_persisted_peers(&self) -> anyhow::Result<Vec<PeerAddress>> {
        let max_failures = self.config.overlay.peer_max_failures;
        self.db_blocking("load-persisted-peers", move |db| {
            let now = current_epoch_seconds();
            let filter = henyey_db::queries::PeerFilter {
                max_failures,
                before_time: Some(now),
                type_filter: Some(henyey_db::queries::PeerTypeFilter::Equals(
                    StoredPeerType::Outbound,
                )),
            };
            let peers = db.query_random_peers(1000, &filter)?;
            let mut addrs = Vec::new();
            for (host, port, _) in peers {
                addrs.push(PeerAddress::new(host, port));
            }
            Ok(addrs)
        })
        .await
    }

    pub(super) async fn store_config_peers(&self) {
        let known_peers = self.config.overlay.known_peers.clone();
        let preferred_peers = self.config.overlay.preferred_peers.clone();
        let _ = self
            .db_blocking("store-config-peers", move |db| {
                let now = current_epoch_seconds();
                for addr in &known_peers {
                    if let Some(peer) = App::parse_peer_address(addr) {
                        let record =
                            henyey_db::queries::PeerRecord::new(now, 0, StoredPeerType::Outbound);
                        let _ = db.store_peer(&peer.host, peer.port, record);
                    }
                }
                for addr in &preferred_peers {
                    if let Some(peer) = App::parse_peer_address(addr) {
                        let record =
                            henyey_db::queries::PeerRecord::new(now, 0, StoredPeerType::Preferred);
                        let _ = db.store_peer(&peer.host, peer.port, record);
                    }
                }
                Ok(())
            })
            .await;
    }

    async fn persist_peers(&self, peers: &[PeerAddress]) {
        let peers = peers.to_vec();
        let _ = self
            .db_blocking("persist-peers", move |db| {
                let now = current_epoch_seconds();
                for peer in &peers {
                    let existing = db.load_peer(&peer.host, peer.port).ok().flatten();
                    if existing.is_some() {
                        continue;
                    }
                    let record =
                        henyey_db::queries::PeerRecord::new(now, 0, StoredPeerType::Outbound);
                    if let Err(err) = db.store_peer(&peer.host, peer.port, record) {
                        tracing::debug!(peer = %peer, error = %err, "Failed to persist peer");
                    }
                }
                Ok(())
            })
            .await;
    }

    async fn filter_discovered_peers(&self, peers: Vec<PeerAddress>) -> Vec<PeerAddress> {
        let max_failures = self.config.overlay.peer_max_failures;
        // Pre-filter non-public peers before DB call (no DB needed)
        let public_peers: Vec<PeerAddress> =
            peers.into_iter().filter(Self::is_public_peer).collect();
        if public_peers.is_empty() {
            return Vec::new();
        }
        self.db_blocking("filter-discovered-peers", move |db| {
            let now = current_epoch_seconds();
            let mut filtered = Vec::new();
            for peer in public_peers {
                let record = db.load_peer(&peer.host, peer.port).ok().flatten();
                if let Some(record) = record {
                    if record.num_failures >= max_failures {
                        continue;
                    }
                    if record.next_attempt > now {
                        continue;
                    }
                }
                filtered.push(peer);
            }
            Ok(filtered)
        })
        .await
        .unwrap_or_default()
    }

    fn filter_advertised_peers(&self, peers: Vec<PeerAddress>) -> Vec<PeerAddress> {
        peers.into_iter().filter(Self::is_public_peer).collect()
    }

    fn is_public_peer(peer: &PeerAddress) -> bool {
        if peer.port == 0 {
            return false;
        }
        let Ok(ip) = peer.host.parse::<std::net::IpAddr>() else {
            return true;
        };
        match ip {
            std::net::IpAddr::V4(v4) => {
                !(v4.is_private()
                    || v4.is_loopback()
                    || v4.is_link_local()
                    || v4.is_multicast()
                    || v4.is_unspecified())
            }
            std::net::IpAddr::V6(_) => false,
        }
    }

    pub(super) async fn refresh_known_peers(&self, overlay: &OverlayManager) -> Vec<PeerAddress> {
        let known_peers_config = self.config.overlay.known_peers.clone();
        let preferred_peers_config = self.config.overlay.preferred_peers.clone();
        let max_failures = self.config.overlay.peer_max_failures;

        // Phase 1: All DB work on the blocking pool
        struct DbResult {
            peers: Vec<PeerAddress>,
            advertised_outbound: Vec<PeerAddress>,
            advertised_inbound: Vec<PeerAddress>,
        }

        let db_result = self
            .db_blocking("refresh-known-peers", move |db| {
                let now = current_epoch_seconds();

                // Build peer list from config
                let mut peers = Vec::new();
                for addr in &known_peers_config {
                    if let Some(peer) = App::parse_peer_address(addr) {
                        peers.push(peer);
                    }
                }
                for addr in &preferred_peers_config {
                    if let Some(peer) = App::parse_peer_address(addr) {
                        // upsert_peer_type inline
                        let existing = db.load_peer(&peer.host, peer.port).ok().flatten();
                        let record = match existing {
                            Some(existing) => henyey_db::queries::PeerRecord::new(
                                existing.next_attempt,
                                existing.num_failures,
                                StoredPeerType::Preferred,
                            ),
                            None => henyey_db::queries::PeerRecord::new(
                                now,
                                0,
                                StoredPeerType::Preferred,
                            ),
                        };
                        let _ = db.store_peer(&peer.host, peer.port, record);
                        peers.push(peer);
                    }
                }

                // Load persisted peers
                let outbound_filter = henyey_db::queries::PeerFilter {
                    max_failures,
                    before_time: Some(now),
                    type_filter: Some(henyey_db::queries::PeerTypeFilter::Equals(
                        StoredPeerType::Outbound,
                    )),
                };
                if let Ok(persisted) = db.query_random_peers(1000, &outbound_filter) {
                    for (host, port, _) in persisted {
                        peers.push(PeerAddress::new(host, port));
                    }
                }

                // Filter discovered peers (inline)
                let mut filtered_peers = Vec::new();
                for peer in peers {
                    if !App::is_public_peer(&peer) {
                        // Config peers may not be public — keep them
                        filtered_peers.push(peer);
                        continue;
                    }
                    let record = db.load_peer(&peer.host, peer.port).ok().flatten();
                    if let Some(ref record) = record {
                        if record.num_failures >= max_failures {
                            continue;
                        }
                        if record.next_attempt > now {
                            continue;
                        }
                    }
                    filtered_peers.push(peer);
                }

                // Build advertised outbound
                let mut advertised_outbound = Vec::new();
                for addr in &known_peers_config {
                    if let Some(peer) = App::parse_peer_address(addr) {
                        advertised_outbound.push(peer);
                    }
                }
                for addr in &preferred_peers_config {
                    if let Some(peer) = App::parse_peer_address(addr) {
                        advertised_outbound.push(peer);
                    }
                }
                let adv_outbound_filter = henyey_db::queries::PeerFilter {
                    max_failures: PEER_MAX_FAILURES_TO_SEND,
                    type_filter: Some(henyey_db::queries::PeerTypeFilter::NotEquals(
                        StoredPeerType::Inbound,
                    )),
                    ..Default::default()
                };
                if let Ok(persisted) = db.query_random_peers(1000, &adv_outbound_filter) {
                    for (host, port, _) in persisted {
                        advertised_outbound.push(PeerAddress::new(host, port));
                    }
                }

                // Build advertised inbound
                let mut advertised_inbound = Vec::new();
                let adv_inbound_filter = henyey_db::queries::PeerFilter {
                    max_failures: PEER_MAX_FAILURES_TO_SEND,
                    type_filter: Some(henyey_db::queries::PeerTypeFilter::Equals(
                        StoredPeerType::Inbound,
                    )),
                    ..Default::default()
                };
                if let Ok(persisted) = db.query_random_peers(1000, &adv_inbound_filter) {
                    for (host, port, _) in persisted {
                        advertised_inbound.push(PeerAddress::new(host, port));
                    }
                }

                Ok(DbResult {
                    peers: filtered_peers,
                    advertised_outbound,
                    advertised_inbound,
                })
            })
            .await;

        let db_result = match db_result {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(error = %e, "Failed to refresh known peers from DB");
                return Vec::new();
            }
        };

        // Phase 2: In-memory overlay operations (no DB)
        let peers = self.dedupe_peers(db_result.peers);
        overlay.set_known_peers(peers.clone());

        let advertised_outbound = self.filter_advertised_peers(db_result.advertised_outbound);
        let advertised_outbound = self.dedupe_peers(advertised_outbound);
        let advertised_inbound = self.filter_advertised_peers(db_result.advertised_inbound);
        let advertised_inbound = self.dedupe_peers(advertised_inbound);
        overlay.set_advertised_peers(advertised_outbound, advertised_inbound);

        peers
    }

    fn dedupe_peers(&self, peers: Vec<PeerAddress>) -> Vec<PeerAddress> {
        let mut seen = HashSet::new();
        let mut deduped = Vec::new();
        for peer in peers {
            if seen.insert(peer.to_socket_addr()) {
                deduped.push(peer);
            }
        }
        deduped
    }
}
