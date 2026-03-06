use super::*;

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

    pub async fn disconnect_peer(&self, peer_id: &PeerId) -> bool {
        let Some(overlay) = self.overlay().await else {
            return false;
        };
        overlay.disconnect(peer_id).await
    }

    pub async fn ban_peer(&self, peer_id: PeerId) -> anyhow::Result<()> {
        let Some(strkey) = Self::peer_id_to_strkey(&peer_id) else {
            anyhow::bail!("Invalid peer id");
        };
        self.db.ban_node(&strkey)?;
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
        self.db.unban_node(&strkey)?;
        let overlay = self
            .overlay()
            .await
            .ok_or_else(|| anyhow::anyhow!("Overlay manager not available"))?;
        Ok(overlay.unban_peer(peer_id))
    }

    pub async fn banned_peers(&self) -> anyhow::Result<Vec<PeerId>> {
        let bans = self.db.load_bans()?;
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
        let _ = self
            .db
            .remove_peers_with_failures(self.config.overlay.peer_max_failures);

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

            let candidates = self.refresh_known_peers(&overlay);
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
            tokio::time::sleep(Duration::from_millis(200)).await;
            self.request_scp_state_from_peers().await;
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
        let now = Instant::now();
        let mut inflight = self.ping_inflight.write().await;
        let mut peer_inflight = self.peer_ping_inflight.write().await;
        inflight.retain(|hash, info| {
            if now.duration_since(info.sent_at) > PING_TIMEOUT {
                if let Some(existing) = peer_inflight.get(&info.peer_id) {
                    if existing == hash {
                        peer_inflight.remove(&info.peer_id);
                    }
                }
                return false;
            }
            true
        });

        let mut to_ping = Vec::new();
        for snapshot in snapshots {
            if peer_inflight.contains_key(&snapshot.info.peer_id) {
                continue;
            }
            let hash = self.next_ping_hash();
            peer_inflight.insert(snapshot.info.peer_id.clone(), hash);
            inflight.insert(
                hash,
                PingInfo {
                    peer_id: snapshot.info.peer_id.clone(),
                    sent_at: Instant::now(),
                },
            );
            to_ping.push((snapshot.info.peer_id, hash));
        }
        drop(inflight);
        drop(peer_inflight);

        // Phase 3: Send pings concurrently.
        let Some(overlay) = self.overlay().await else {
            return;
        };

        for (peer, hash) in to_ping {
            let msg = StellarMessage::GetScpQuorumset(stellar_xdr::curr::Uint256(hash.0));
            if overlay.try_send_to(&peer, msg).is_err() {
                tracing::debug!(peer = %peer, "Failed to send ping");
                let mut inflight = self.ping_inflight.write().await;
                inflight.remove(&hash);
                let mut peer_inflight = self.peer_ping_inflight.write().await;
                if let Some(existing) = peer_inflight.get(&peer) {
                    if *existing == hash {
                        peer_inflight.remove(&peer);
                    }
                }
            }
        }
    }

    pub(super) async fn process_ping_response(&self, peer_id: &henyey_overlay::PeerId, hash: [u8; 32]) {
        let hash = Hash256::from_bytes(hash);
        let info = {
            let mut inflight = self.ping_inflight.write().await;
            inflight.remove(&hash)
        };

        let Some(info) = info else {
            return;
        };

        {
            let mut peer_inflight = self.peer_ping_inflight.write().await;
            if let Some(existing) = peer_inflight.get(&info.peer_id) {
                if *existing == hash {
                    peer_inflight.remove(&info.peer_id);
                }
            }
        }

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
                if port == 0 {
                    return None;
                }

                Some(PeerAddress::new(ip, port as u16))
            })
            .collect();

        let addrs = self.filter_discovered_peers(addrs);

        if !addrs.is_empty() {
            self.persist_peers(&addrs);
            let count = overlay.add_peers(addrs).await;
            if count > 0 {
                tracing::info!(added = count, "Added peers from discovery");
            }
        }

        let _ = self.refresh_known_peers(&overlay);
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

    pub(super) fn load_persisted_peers(&self) -> anyhow::Result<Vec<PeerAddress>> {
        let now = current_epoch_seconds();
        let peers = self.db.load_random_peers(
            1000,
            self.config.overlay.peer_max_failures,
            now,
            Some(PEER_TYPE_OUTBOUND),
        )?;
        let mut addrs = Vec::new();
        for (host, port, _) in peers {
            addrs.push(PeerAddress::new(host, port));
        }
        Ok(addrs)
    }

    pub(super) fn store_config_peers(&self) {
        let now = current_epoch_seconds();
        for addr in &self.config.overlay.known_peers {
            if let Some(peer) = Self::parse_peer_address(addr) {
                let record = henyey_db::queries::PeerRecord::new(now, 0, PEER_TYPE_OUTBOUND);
                let _ = self.db.store_peer(&peer.host, peer.port, record);
            }
        }
        for addr in &self.config.overlay.preferred_peers {
            if let Some(peer) = Self::parse_peer_address(addr) {
                let record = henyey_db::queries::PeerRecord::new(now, 0, PEER_TYPE_PREFERRED);
                let _ = self.db.store_peer(&peer.host, peer.port, record);
            }
        }
    }

    fn load_advertised_outbound_peers(&self) -> anyhow::Result<Vec<PeerAddress>> {
        let peers = self.db.load_random_peers_any_outbound_max_failures(
            1000,
            PEER_MAX_FAILURES_TO_SEND,
            PEER_TYPE_INBOUND,
        )?;
        let mut addrs = Vec::new();
        for (host, port, _) in peers {
            addrs.push(PeerAddress::new(host, port));
        }
        Ok(addrs)
    }

    fn load_advertised_inbound_peers(&self) -> anyhow::Result<Vec<PeerAddress>> {
        let peers = self.db.load_random_peers_by_type_max_failures(
            1000,
            PEER_MAX_FAILURES_TO_SEND,
            PEER_TYPE_INBOUND,
        )?;
        let mut addrs = Vec::new();
        for (host, port, _) in peers {
            addrs.push(PeerAddress::new(host, port));
        }
        Ok(addrs)
    }

    fn persist_peers(&self, peers: &[PeerAddress]) {
        let now = current_epoch_seconds();
        for peer in peers {
            let existing = self.db.load_peer(&peer.host, peer.port).ok().flatten();
            if existing.is_some() {
                continue;
            }
            let record = henyey_db::queries::PeerRecord::new(now, 0, PEER_TYPE_OUTBOUND);
            if let Err(err) = self.db.store_peer(&peer.host, peer.port, record) {
                tracing::debug!(peer = %peer, error = %err, "Failed to persist peer");
            }
        }
    }

    fn filter_discovered_peers(&self, peers: Vec<PeerAddress>) -> Vec<PeerAddress> {
        let now = current_epoch_seconds();
        let mut filtered = Vec::new();
        for peer in peers {
            if !Self::is_public_peer(&peer) {
                continue;
            }
            let record = self.db.load_peer(&peer.host, peer.port).ok().flatten();
            if let Some(record) = record {
                if record.num_failures >= self.config.overlay.peer_max_failures {
                    continue;
                }
                if record.next_attempt > now {
                    continue;
                }
            }
            filtered.push(peer);
        }
        filtered
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

    pub(super) fn refresh_known_peers(&self, overlay: &OverlayManager) -> Vec<PeerAddress> {
        let mut peers = Vec::new();
        for addr in &self.config.overlay.known_peers {
            if let Some(peer) = Self::parse_peer_address(addr) {
                peers.push(peer);
            }
        }
        for addr in &self.config.overlay.preferred_peers {
            if let Some(peer) = Self::parse_peer_address(addr) {
                self.upsert_peer_type(&peer, PEER_TYPE_PREFERRED);
                peers.push(peer);
            }
        }
        if let Ok(persisted) = self.load_persisted_peers() {
            peers.extend(persisted);
        }
        let peers = self.filter_discovered_peers(peers);
        let peers = self.dedupe_peers(peers);
        overlay.set_known_peers(peers.clone());

        let mut advertised_outbound = Vec::new();
        for addr in &self.config.overlay.known_peers {
            if let Some(peer) = Self::parse_peer_address(addr) {
                advertised_outbound.push(peer);
            }
        }
        for addr in &self.config.overlay.preferred_peers {
            if let Some(peer) = Self::parse_peer_address(addr) {
                advertised_outbound.push(peer);
            }
        }
        if let Ok(persisted) = self.load_advertised_outbound_peers() {
            advertised_outbound.extend(persisted);
        }
        let advertised_outbound = self.filter_advertised_peers(advertised_outbound);
        let advertised_outbound = self.dedupe_peers(advertised_outbound);

        let mut advertised_inbound = Vec::new();
        if let Ok(persisted) = self.load_advertised_inbound_peers() {
            advertised_inbound.extend(persisted);
        }
        let advertised_inbound = self.filter_advertised_peers(advertised_inbound);
        let advertised_inbound = self.dedupe_peers(advertised_inbound);
        overlay.set_advertised_peers(advertised_outbound, advertised_inbound);

        peers
    }

    fn upsert_peer_type(&self, peer: &PeerAddress, peer_type: i32) {
        let now = current_epoch_seconds();
        let existing = self.db.load_peer(&peer.host, peer.port).ok().flatten();
        let record = match existing {
            Some(existing) => henyey_db::queries::PeerRecord::new(
                existing.next_attempt,
                existing.num_failures,
                peer_type,
            ),
            None => henyey_db::queries::PeerRecord::new(now, 0, peer_type),
        };
        let _ = self.db.store_peer(&peer.host, peer.port, record);
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
