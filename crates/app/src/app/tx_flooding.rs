//! Transaction flooding: advertising, pulling, and broadcasting transactions across peers.

use super::*;

impl App {
    pub(super) fn tx_set_start_index(
        hash: &Hash256,
        peers_len: usize,
        peer_offset: usize,
    ) -> usize {
        if peers_len == 0 {
            return 0;
        }
        let start = u64::from_le_bytes(hash.0[0..8].try_into().unwrap_or([0; 8]));
        let base = (start as usize) % peers_len;
        (base + (peer_offset % peers_len)) % peers_len
    }

    pub(super) async fn enqueue_tx_advert(&self, tx_env: &stellar_xdr::curr::TransactionEnvelope) {
        let Some(hash) = Hash256::hash_xdr(tx_env).ok() else {
            tracing::debug!("Failed to hash transaction for advert");
            return;
        };

        self.tx_advert_queue.write().await.insert(hash);
    }

    pub(super) async fn flush_tx_adverts(&self) {
        let hashes = {
            let mut queue = self.tx_advert_queue.write().await;
            if queue.is_empty() {
                return;
            }
            queue.drain()
        };

        tracing::debug!(count = hashes.len(), "Flushing tx adverts");

        let Some(overlay) = self.overlay().await else {
            return;
        };

        let max_advert_size = self.max_advert_size();
        let snapshots = overlay.peer_snapshots();
        if snapshots.is_empty() {
            return;
        }

        let peer_ids = snapshots
            .iter()
            .map(|snapshot| snapshot.info.peer_id.clone())
            .collect::<Vec<_>>();
        let peer_set: HashSet<_> = peer_ids.iter().cloned().collect();

        let per_peer = {
            let mut adverts_by_peer = self.tx_adverts_by_peer.write().await;
            adverts_by_peer.retain(|peer, _| peer_set.contains(peer));

            let mut per_peer = Vec::new();
            for peer_id in peer_ids {
                let adverts = adverts_by_peer
                    .entry(peer_id.clone())
                    .or_insert_with(PeerTxAdverts::new);
                let mut outgoing = Vec::new();
                for hash in &hashes {
                    if adverts.seen_advert(hash) {
                        continue;
                    }
                    outgoing.push(*hash);
                }
                if !outgoing.is_empty() {
                    per_peer.push((peer_id, outgoing));
                }
            }
            per_peer
        };

        for (peer_id, hashes) in per_peer {
            for chunk in hashes.chunks(max_advert_size) {
                let tx_hashes = match TxAdvertVector::try_from(
                    chunk
                        .iter()
                        .map(|hash| Hash::from(*hash))
                        .collect::<Vec<_>>(),
                ) {
                    Ok(vec) => vec,
                    Err(_) => {
                        tracing::debug!(peer = %peer_id, "Failed to build tx advert vector");
                        continue;
                    }
                };
                let advert = FloodAdvert { tx_hashes };
                if let Err(e) = overlay.try_send_to(&peer_id, StellarMessage::FloodAdvert(advert)) {
                    tracing::debug!(peer = %peer_id, error = %e, "Failed to send tx advert batch");
                }
            }
        }
    }

    pub(super) fn flood_advert_period(&self) -> Duration {
        Duration::from_millis(self.config.overlay.flood_advert_period_ms.max(1))
    }

    pub(super) fn flood_demand_period(&self) -> Duration {
        Duration::from_millis(self.config.overlay.flood_demand_period_ms.max(1))
    }

    fn flood_demand_backoff_delay(&self) -> Duration {
        Duration::from_millis(self.config.overlay.flood_demand_backoff_delay_ms.max(1))
    }

    fn max_advert_queue_size(&self) -> usize {
        self.herder.max_tx_set_size().max(1)
    }

    fn max_advert_size(&self) -> usize {
        const TX_ADVERT_VECTOR_MAX_SIZE: usize = 1000;
        let ledger_close_ms = (self.herder.ledger_close_time() as u64).saturating_mul(1000);
        let ledger_close_ms = ledger_close_ms.max(1) as f64;
        let ops_to_flood =
            self.config.overlay.flood_op_rate_per_ledger * self.herder.max_tx_set_size() as f64;
        let per_period = (ops_to_flood * self.config.overlay.flood_advert_period_ms as f64
            / ledger_close_ms)
            .ceil()
            .max(1.0);
        per_period.min(TX_ADVERT_VECTOR_MAX_SIZE as f64) as usize
    }

    fn max_demand_size(&self) -> usize {
        const TX_DEMAND_VECTOR_MAX_SIZE: usize = 1000;
        let ledger_close_ms = (self.herder.ledger_close_time() as u64).saturating_mul(1000);
        let ledger_close_ms = ledger_close_ms.max(1) as f64;
        let ops_to_flood =
            self.config.overlay.flood_op_rate_per_ledger * self.herder.max_queue_size_ops() as f64;
        let per_period = (ops_to_flood * self.config.overlay.flood_demand_period_ms as f64
            / ledger_close_ms)
            .ceil()
            .max(1.0);
        per_period.min(TX_DEMAND_VECTOR_MAX_SIZE as f64) as usize
    }

    fn retry_delay_demand(&self, attempts: usize) -> Duration {
        let delay_ms = self
            .flood_demand_backoff_delay()
            .as_millis()
            .saturating_mul(attempts as u128);
        Duration::from_millis(delay_ms.min(2000) as u64)
    }

    pub(super) async fn clear_tx_advert_history(&self, ledger_seq: LedgerSeq) {
        let mut adverts_by_peer = self.tx_adverts_by_peer.write().await;
        for adverts in adverts_by_peer.values_mut() {
            adverts.clear_below(ledger_seq);
        }

        // Clean up old tx demand history entries (older than 5 minutes)
        const MAX_TX_DEMAND_AGE_SECS: u64 = 300;
        let cutoff = self.clock.now() - std::time::Duration::from_secs(MAX_TX_DEMAND_AGE_SECS);
        let mut history = self.tx_demand_history.write().await;
        history.retain(|_, entry| entry.last_demanded > cutoff);

        // Clean up old tx set dont have entries (older than 2 minutes)
        const MAX_TX_SET_DONT_HAVE_AGE_SECS: u64 = 120;
        let cutoff_short =
            self.clock.now() - std::time::Duration::from_secs(MAX_TX_SET_DONT_HAVE_AGE_SECS);
        let mut dont_have = self.tx_set_dont_have.write().await;
        // Note: tx_set_dont_have doesn't have timestamps, so we clear it periodically
        // to prevent unbounded growth. Clear entries for any old tx set hashes.
        // In practice this map should stay small since tx set requests are resolved quickly.
        if dont_have.len() > 100 {
            dont_have.clear();
        }

        // Clean up old tx set last request entries (older than 2 minutes)
        let mut last_request = self.tx_set_last_request.write().await;
        last_request.retain(|_, state| state.last_request > cutoff_short);
    }

    pub(super) async fn record_tx_pull_latency(
        &self,
        hash: Hash256,
        peer: &henyey_overlay::PeerId,
    ) {
        let now = self.clock.now();
        let mut history = self.tx_demand_history.write().await;
        let Some(entry) = history.get_mut(&hash) else {
            return;
        };

        if !entry.latency_recorded {
            entry.latency_recorded = true;
            let delta = now.duration_since(entry.first_demanded);
            tracing::debug!(
                hash = %hash.to_hex(),
                latency_ms = delta.as_millis(),
                peers = entry.peers.len(),
                "Pulled transaction after demand"
            );
        }

        if let Some(peer_demanded) = entry.peers.get(peer) {
            let delta = now.duration_since(*peer_demanded);
            tracing::debug!(
                hash = %hash.to_hex(),
                peer = %peer,
                latency_ms = delta.as_millis(),
                "Pulled transaction from peer"
            );
        }
    }

    fn demand_status(
        &self,
        hash: Hash256,
        peer: &henyey_overlay::PeerId,
        now: Instant,
        history: &HashMap<Hash256, TxDemandHistory>,
    ) -> DemandStatus {
        const MAX_RETRY_COUNT: usize = 15;

        if self.herder.tx_queue().contains(&hash) {
            return DemandStatus::Discard;
        }

        let Some(entry) = history.get(&hash) else {
            return DemandStatus::Demand;
        };

        if entry.peers.contains_key(peer) {
            return DemandStatus::Discard;
        }

        let num_demanded = entry.peers.len();
        if num_demanded < MAX_RETRY_COUNT {
            let retry_delay = self.retry_delay_demand(num_demanded);
            if now.duration_since(entry.last_demanded) >= retry_delay {
                DemandStatus::Demand
            } else {
                DemandStatus::RetryLater
            }
        } else {
            DemandStatus::Discard
        }
    }

    fn prune_tx_demands(
        &self,
        now: Instant,
        pending: &mut VecDeque<Hash256>,
        history: &mut HashMap<Hash256, TxDemandHistory>,
    ) {
        const MAX_RETRY_COUNT: u32 = 15;
        let max_retention = Duration::from_secs(2) * MAX_RETRY_COUNT * 2;

        while let Some(hash) = pending.front().copied() {
            let Some(entry) = history.get(&hash) else {
                pending.pop_front();
                continue;
            };
            if now.duration_since(entry.first_demanded) >= max_retention {
                if !entry.latency_recorded {
                    tracing::debug!(hash = %hash.to_hex(), "Abandoned tx demand");
                }
                pending.pop_front();
                history.remove(&hash);
            } else {
                break;
            }
        }
    }

    pub(super) async fn run_tx_demands(&self) {
        let Some(overlay) = self.overlay().await else {
            return;
        };

        let mut peers = overlay.peer_snapshots();
        if peers.is_empty() {
            return;
        }

        peers.shuffle(&mut rand::thread_rng());
        let peer_ids = peers
            .iter()
            .map(|snapshot| snapshot.info.peer_id.clone())
            .collect::<Vec<_>>();
        let peer_set: HashSet<_> = peer_ids.iter().cloned().collect();

        let max_demand_size = self.max_demand_size();
        let max_queue_size = self.max_advert_queue_size();
        let now = self.clock.now();
        let mut to_send: Vec<(henyey_overlay::PeerId, Vec<Hash256>)> = Vec::new();

        {
            let mut adverts_by_peer = self.tx_adverts_by_peer.write().await;
            adverts_by_peer.retain(|peer, _| peer_set.contains(peer));
            for peer_id in &peer_ids {
                adverts_by_peer
                    .entry(peer_id.clone())
                    .or_insert_with(PeerTxAdverts::new);
            }

            let mut history = self.tx_demand_history.write().await;
            let mut pending = self.tx_pending_demands.write().await;
            self.prune_tx_demands(now, &mut pending, &mut history);

            let mut demand_map: HashMap<henyey_overlay::PeerId, (Vec<Hash256>, Vec<Hash256>)> =
                peer_ids
                    .iter()
                    .map(|peer| (peer.clone(), (Vec::new(), Vec::new())))
                    .collect();

            let mut any_new_demand = true;
            while any_new_demand {
                any_new_demand = false;
                for peer_id in &peer_ids {
                    let Some(adverts) = adverts_by_peer.get_mut(peer_id) else {
                        continue;
                    };
                    let Some((demand, retry)) = demand_map.get_mut(peer_id) else {
                        continue;
                    };

                    let mut added_new = false;
                    while demand.len() < max_demand_size && adverts.has_advert() && !added_new {
                        let Some(hash) = adverts.pop_advert() else {
                            break;
                        };
                        match self.demand_status(hash, peer_id, now, &history) {
                            DemandStatus::Demand => {
                                demand.push(hash);
                                let entry = history.entry(hash).or_insert_with(|| {
                                    pending.push_back(hash);
                                    TxDemandHistory {
                                        first_demanded: now,
                                        last_demanded: now,
                                        peers: HashMap::new(),
                                        latency_recorded: false,
                                    }
                                });
                                entry.peers.insert(peer_id.clone(), now);
                                entry.last_demanded = now;
                                added_new = true;
                                any_new_demand = true;
                            }
                            DemandStatus::RetryLater => {
                                retry.push(hash);
                            }
                            DemandStatus::Discard => {}
                        }
                    }
                }
            }

            for peer_id in &peer_ids {
                let Some(adverts) = adverts_by_peer.get_mut(peer_id) else {
                    continue;
                };
                let Some((demand, retry)) = demand_map.remove(peer_id) else {
                    continue;
                };
                adverts.retry_incoming(retry, max_queue_size);
                if !demand.is_empty() {
                    to_send.push((peer_id.clone(), demand));
                }
            }
        }

        for (peer_id, hashes) in to_send {
            let tx_hashes = match TxDemandVector::try_from(
                hashes.into_iter().map(Hash::from).collect::<Vec<_>>(),
            ) {
                Ok(vec) => vec,
                Err(_) => {
                    tracing::debug!(peer = %peer_id, "Failed to build tx demand vector");
                    continue;
                }
            };
            let demand = FloodDemand { tx_hashes };
            if let Err(e) = overlay.try_send_to(&peer_id, StellarMessage::FloodDemand(demand)) {
                tracing::warn!(peer = %peer_id, error = %e, "Failed to send flood demand");
            }
        }
    }

    pub(super) async fn handle_flood_advert(
        &self,
        peer_id: &henyey_overlay::PeerId,
        advert: FloodAdvert,
    ) {
        tracing::debug!(
            peer = %peer_id,
            count = advert.tx_hashes.0.len(),
            "Received FloodAdvert"
        );
        let ledger_seq = self.herder.tracking_slot().min(u32::MAX as u64) as u32;
        let max_ops = self.max_advert_queue_size();
        let mut adverts_by_peer = self.tx_adverts_by_peer.write().await;
        let entry = adverts_by_peer
            .entry(peer_id.clone())
            .or_insert_with(PeerTxAdverts::new);
        entry.queue_incoming(&advert.tx_hashes.0, ledger_seq.into(), max_ops);
    }

    pub(super) async fn handle_flood_demand(
        &self,
        peer_id: &henyey_overlay::PeerId,
        demand: FloodDemand,
    ) {
        tracing::debug!(
            peer = %peer_id,
            count = demand.tx_hashes.0.len(),
            "Received FloodDemand"
        );
        let Some(overlay) = self.overlay().await else {
            return;
        };

        // Use non-blocking try_send_to to avoid stalling the event loop
        // when the peer's outbound channel is full.  The peer will re-request
        // any transactions it still needs.
        let mut sent = 0u32;
        let mut dropped = 0u32;
        for hash in demand.tx_hashes.0.iter() {
            let hash256 = Hash256::from(hash.clone());
            if let Some(tx) = self.herder.tx_queue().get(&hash256) {
                match overlay.try_send_to(peer_id, StellarMessage::Transaction(tx.envelope)) {
                    Ok(()) => sent += 1,
                    Err(_) => {
                        dropped += 1;
                        break; // Channel full — stop sending to this peer
                    }
                }
            } else {
                let dont_have = DontHave {
                    type_: MessageType::Transaction,
                    req_hash: stellar_xdr::curr::Uint256(hash.0),
                };
                if overlay
                    .try_send_to(peer_id, StellarMessage::DontHave(dont_have))
                    .is_err()
                {
                    dropped += 1;
                    break;
                }
            }
        }
        if dropped > 0 {
            tracing::debug!(
                peer = %peer_id,
                sent,
                dropped,
                total = demand.tx_hashes.0.len(),
                "Flood demand partially served (peer outbound channel full)"
            );
        }
    }

    /// Handle a TxSet message from the network.
    pub(super) async fn handle_tx_set(&self, tx_set: stellar_xdr::curr::TransactionSet) {
        use henyey_herder::TransactionSet;

        // For legacy TransactionSet, hash is SHA-256 of previous_ledger_hash + tx XDR blobs
        let transactions: Vec<_> = tx_set.txs.to_vec();
        let prev_hash = henyey_common::Hash256::from_bytes(tx_set.previous_ledger_hash.0);
        let hash = match TransactionSet::compute_non_generalized_hash(prev_hash, &transactions) {
            Some(hash) => hash,
            None => {
                tracing::error!("Failed to compute legacy TxSet hash");
                return;
            }
        };

        // Create our internal TransactionSet with correct hash
        let internal_tx_set = TransactionSet::with_hash(prev_hash, hash, transactions);
        {
            let mut map = self.tx_set_dont_have.write().await;
            map.remove(&internal_tx_set.hash);
        }
        {
            let mut map = self.tx_set_last_request.write().await;
            map.remove(&internal_tx_set.hash);
        }

        tracing::info!(
            hash = %internal_tx_set.hash,
            tx_count = internal_tx_set.transactions.len(),
            "Processing TxSet"
        );

        if !self.herder.needs_tx_set(&internal_tx_set.hash) {
            tracing::info!(hash = %internal_tx_set.hash, "TxSet not pending");
        }

        let received_slot = self.herder.receive_tx_set(internal_tx_set.clone());
        if let Some(slot) = received_slot {
            tracing::info!(slot, "Received pending TxSet, attempting ledger close");
            self.process_externalized_slots().await;
        } else if self.attach_tx_set_by_hash(&internal_tx_set).await
            || self.buffer_externalized_tx_set(&internal_tx_set).await
        {
            self.try_apply_buffered_ledgers().await;
        }
    }

    /// Handle a GeneralizedTxSet message from the network.
    pub(super) async fn handle_generalized_tx_set(
        &self,
        gen_tx_set: stellar_xdr::curr::GeneralizedTransactionSet,
    ) {
        use henyey_herder::TransactionSet;
        use stellar_xdr::curr::{
            GeneralizedTransactionSet, TransactionPhase, TxSetComponent, WriteXdr,
        };

        // Compute hash as SHA-256 of XDR-encoded GeneralizedTransactionSet
        // This matches how stellar-core computes it: xdrSha256(xdrTxSet)
        let xdr_bytes = match gen_tx_set.to_xdr(stellar_xdr::curr::Limits::none()) {
            Ok(bytes) => bytes,
            Err(e) => {
                tracing::error!(error = %e, "Failed to encode GeneralizedTxSet to XDR");
                return;
            }
        };
        let hash = henyey_common::Hash256::hash(&xdr_bytes);

        // Validate phase count before extracting
        let GeneralizedTransactionSet::V1(v1) = &gen_tx_set;
        if v1.phases.len() != 2 {
            tracing::warn!(
                hash = %hash,
                phases = v1.phases.len(),
                "Invalid GeneralizedTxSet phase count"
            );
            return;
        }

        // Extract transactions from GeneralizedTransactionSet
        let (prev_hash, transactions) = super::extract_txs_from_generalized(&gen_tx_set);

        tracing::debug!(
            hash = %hash,
            tx_count = transactions.len(),
            "Processing GeneralizedTxSet"
        );

        if !self.herder.needs_tx_set(&hash) {
            tracing::debug!(hash = %hash, "GeneralizedTxSet not pending");
        }

        let phase_check = match &gen_tx_set {
            GeneralizedTransactionSet::V1(v1) => {
                let classic_ok = matches!(v1.phases[0], TransactionPhase::V0(_));
                let soroban_ok = matches!(
                    v1.phases[1],
                    TransactionPhase::V1(_) | TransactionPhase::V0(_)
                );
                if !classic_ok || !soroban_ok {
                    tracing::warn!(hash = %hash, "Invalid GeneralizedTxSet phase types");
                }
                classic_ok && soroban_ok
            }
        };
        if !phase_check {
            return;
        }

        let base_fee_limit = self.ledger_manager.current_header().base_fee as i64;
        let base_fee_ok = match &gen_tx_set {
            GeneralizedTransactionSet::V1(v1) => {
                let classic_ok = match &v1.phases[0] {
                    TransactionPhase::V0(components) => components.iter().all(|component| {
                        let TxSetComponent::TxsetCompTxsMaybeDiscountedFee(comp) = component;
                        comp.base_fee.map_or(true, |fee| fee >= base_fee_limit)
                    }),
                    _ => false,
                };
                let soroban_ok = match &v1.phases[1] {
                    TransactionPhase::V1(parallel) => {
                        parallel.base_fee.map_or(true, |fee| fee >= base_fee_limit)
                    }
                    TransactionPhase::V0(components) => components.iter().all(|component| {
                        let TxSetComponent::TxsetCompTxsMaybeDiscountedFee(comp) = component;
                        comp.base_fee.map_or(true, |fee| fee >= base_fee_limit)
                    }),
                };
                classic_ok && soroban_ok
            }
        };
        if !base_fee_ok {
            tracing::warn!(hash = %hash, base_fee = base_fee_limit, "GeneralizedTxSet base fee below ledger base fee");
            return;
        }

        let network_id = NetworkId(self.network_id());
        let mut classic_count = 0usize;
        let mut soroban_count = 0usize;
        for env in &transactions {
            let frame =
                henyey_tx::TransactionFrame::from_owned_with_network(env.clone(), network_id);
            if frame.is_soroban() {
                soroban_count += 1;
            } else {
                classic_count += 1;
            }
        }
        let phase_sizes = match &gen_tx_set {
            GeneralizedTransactionSet::V1(v1) => {
                let classic_phase_count: usize = match &v1.phases[0] {
                    TransactionPhase::V0(components) => components
                        .iter()
                        .map(|component| match component {
                            TxSetComponent::TxsetCompTxsMaybeDiscountedFee(comp) => comp.txs.len(),
                        })
                        .sum(),
                    _ => 0,
                };
                let soroban_phase_count: usize = match &v1.phases[1] {
                    TransactionPhase::V1(parallel) => parallel
                        .execution_stages
                        .iter()
                        .map(|stage| stage.0.iter().map(|cluster| cluster.0.len()).sum::<usize>())
                        .sum(),
                    TransactionPhase::V0(components) => components
                        .iter()
                        .map(|component| match component {
                            TxSetComponent::TxsetCompTxsMaybeDiscountedFee(comp) => comp.txs.len(),
                        })
                        .sum(),
                };
                (classic_phase_count, soroban_phase_count)
            }
        };
        if classic_count != phase_sizes.0 || soroban_count != phase_sizes.1 {
            tracing::warn!(
                hash = %hash,
                classic = classic_count,
                soroban = soroban_count,
                classic_phase = phase_sizes.0,
                soroban_phase = phase_sizes.1,
                "GeneralizedTxSet phase tx type mismatch"
            );
            return;
        }

        // Create internal tx set with the correct hash and retain generalized set
        let internal_tx_set =
            TransactionSet::with_generalized(prev_hash, hash, transactions, gen_tx_set);
        {
            let mut map = self.tx_set_dont_have.write().await;
            map.remove(&internal_tx_set.hash);
        }
        {
            let mut map = self.tx_set_last_request.write().await;
            map.remove(&internal_tx_set.hash);
        }

        let received_slot = self.herder.receive_tx_set(internal_tx_set.clone());
        if let Some(slot) = received_slot {
            tracing::debug!(
                slot,
                hash = %hash,
                "Received pending GeneralizedTxSet, attempting ledger close"
            );
            self.try_close_slot_directly(slot).await;
        } else if self.attach_tx_set_by_hash(&internal_tx_set).await
            || self.buffer_externalized_tx_set(&internal_tx_set).await
        {
            tracing::debug!(hash = %hash, "TxSet matched buffered/externalized slot");
            self.try_apply_buffered_ledgers().await;
        } else {
            tracing::debug!(hash = %hash, "TxSet not matched to any slot or buffer entry");
        }
    }

    /// Send a TxSet to a peer in response to GetTxSet.
    pub(super) async fn send_tx_set(
        &self,
        peer_id: &henyey_overlay::PeerId,
        hash: &henyey_common::Hash256,
    ) {
        // Get the tx set from cache
        let tx_set = match self.herder.get_tx_set(hash) {
            Some(ts) => ts,
            None => {
                tracing::debug!(hash = hex::encode(hash.0), peer = %peer_id, "TxSet not found in cache");
                if let Some(overlay) = self.overlay().await {
                    let ledger_version = self.ledger_manager.current_header().ledger_version;
                    let message_type =
                        if protocol_version_starts_from(ledger_version, ProtocolVersion::V20) {
                            stellar_xdr::curr::MessageType::GeneralizedTxSet
                        } else {
                            stellar_xdr::curr::MessageType::TxSet
                        };
                    let msg = StellarMessage::DontHave(stellar_xdr::curr::DontHave {
                        type_: message_type,
                        req_hash: stellar_xdr::curr::Uint256(hash.0),
                    });
                    if let Err(e) = overlay.try_send_to(peer_id, msg) {
                        tracing::debug!(hash = hex::encode(hash.0), peer = %peer_id, error = %e, "Failed to send DontHave for TxSet");
                    }
                }
                return;
            }
        };

        // Try to send as GeneralizedTxSet first if the cached tx_set has one.
        // The requesting node asked for a specific hash — if the cached entry was
        // stored with a GeneralizedTxSet hash (protocol >= 20 consensus), we must
        // send the GeneralizedTxSet back so the hash matches. Using ledger_version
        // from the current header would be wrong for nodes that haven't yet closed
        // the ledger at the newer protocol version.
        if let Some(gen_tx_set) = tx_set
            .generalized_tx_set
            .clone()
            .or_else(|| build_generalized_tx_set(&tx_set))
        {
            let gen_hash = match gen_tx_set.to_xdr(stellar_xdr::curr::Limits::none()) {
                Ok(bytes) => henyey_common::Hash256::hash(&bytes),
                Err(e) => {
                    tracing::warn!(hash = %hash, error = %e, "Failed to encode GeneralizedTxSet");
                    henyey_common::Hash256::ZERO
                }
            };
            if gen_hash == *hash {
                let message = StellarMessage::GeneralizedTxSet(gen_tx_set);
                if let Some(overlay) = self.overlay().await {
                    if let Err(e) = overlay.try_send_to(peer_id, message) {
                        tracing::warn!(hash = %hash, peer = %peer_id, error = %e, "Failed to send GeneralizedTxSet");
                    } else {
                        tracing::debug!(hash = %hash, peer = %peer_id, "Sent GeneralizedTxSet");
                    }
                }
                return;
            }
            tracing::warn!(hash = %hash, computed = %gen_hash, "GeneralizedTxSet hash mismatch; falling back");
        }

        // Convert to legacy XDR TransactionSet
        let prev_hash = tx_set.previous_ledger_hash;
        let xdr_tx_set = stellar_xdr::curr::TransactionSet {
            previous_ledger_hash: Hash::from(prev_hash),
            txs: tx_set.transactions.try_into().unwrap_or_default(),
        };

        let message = StellarMessage::TxSet(xdr_tx_set);

        if let Some(overlay) = self.overlay().await {
            if let Err(e) = overlay.try_send_to(peer_id, message) {
                tracing::warn!(hash = hex::encode(hash), peer = %peer_id, error = %e, "Failed to send TxSet");
            } else {
                tracing::debug!(hash = hex::encode(hash), peer = %peer_id, "Sent TxSet");
            }
        }
    }

    /// Request pending transaction sets from peers.
    pub(super) async fn request_pending_tx_sets(&self) {
        let current_ledger = match self.get_current_ledger().await {
            Ok(seq) => seq,
            Err(_) => return,
        };
        let min_slot = current_ledger.saturating_add(1) as u64;
        let window_end = current_ledger as u64 + TX_SET_REQUEST_WINDOW;
        let mut pending = self.herder.get_pending_tx_sets();
        pending.sort_by_key(|(_, slot)| *slot);

        // Log all pending tx_sets for debugging
        if !pending.is_empty() {
            tracing::debug!(
                current_ledger,
                min_slot = current_ledger.saturating_add(1),
                window_end = current_ledger as u64 + TX_SET_REQUEST_WINDOW,
                pending_count = pending.len(),
                pending_slots = ?pending.iter().map(|(h, s)| (*s, format!("{}...", &hex::encode(h.0)[..8]))).collect::<Vec<_>>(),
                "Pending tx_sets before filtering"
            );
        }

        let pending_hashes: Vec<Hash256> = pending
            .into_iter()
            .filter(|(_, slot)| *slot >= min_slot && *slot <= window_end)
            .map(|(hash, _)| hash)
            .take(MAX_TX_SET_REQUESTS_PER_TICK)
            .collect();
        if pending_hashes.is_empty() {
            return;
        }

        tracing::debug!(
            current_ledger,
            pending_count = pending_hashes.len(),
            hashes = ?pending_hashes.iter().map(|h| format!("{}...", &hex::encode(h.0)[..8])).collect::<Vec<_>>(),
            "Will request tx_sets"
        );

        let Some(overlay) = self.overlay().await else {
            tracing::warn!("No overlay available to request tx sets");
            return;
        };

        let peer_infos = overlay.peer_infos();
        if peer_infos.is_empty() {
            tracing::warn!("No peers connected, cannot request tx sets");
            return;
        }
        let mut peers = Vec::new();
        let mut fallback = Vec::new();
        for info in peer_infos {
            fallback.push(info.peer_id.clone());
            let is_outbound = matches!(info.direction, ConnectionDirection::Outbound);
            let is_preferred = if is_outbound {
                true
            } else {
                let host = info.address.ip().to_string();
                let port = info.address.port();
                match self.db.load_peer(&host, port) {
                    Ok(Some(record)) => {
                        record.peer_type == StoredPeerType::Preferred
                            || record.peer_type == StoredPeerType::Outbound
                    }
                    _ => false,
                }
            };
            if is_outbound || is_preferred {
                peers.push(info.peer_id);
            }
        }
        if peers.is_empty() {
            peers = fallback;
        }
        peers.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));

        let now = self.clock.now();
        let (requests, newly_exhausted) = {
            let mut dont_have = self.tx_set_dont_have.write().await;
            let pending_set: HashSet<Hash256> = pending_hashes.iter().copied().collect();
            dont_have.retain(|hash, _| pending_set.contains(hash));
            let mut last_request = self.tx_set_last_request.write().await;
            last_request.retain(|hash, _| pending_set.contains(hash));
            let exhausted_warned = self.tx_set_exhausted_warned.read().await;

            let mut reqs = Vec::new();
            let mut exhausted = Vec::new();

            for hash in &pending_hashes {
                if !self.herder.needs_tx_set(hash) {
                    continue;
                }
                let throttle = std::time::Duration::from_millis(200);
                let mut request_state =
                    last_request
                        .get(hash)
                        .cloned()
                        .unwrap_or(TxSetRequestState {
                            last_request: now.checked_sub(throttle).unwrap_or(now),
                            first_requested: now,
                            next_peer_offset: 0,
                        });
                if now.duration_since(request_state.last_request) < throttle {
                    continue;
                }

                // Timeout detection: if we've been requesting this tx_set for
                // TX_SET_REQUEST_TIMEOUT_SECS with no response at all (no
                // GeneralizedTxSet, no DontHave), peers are silently dropping
                // our requests. Synthetically mark all peers as DontHave.
                let request_age = now.duration_since(request_state.first_requested);
                if request_age >= std::time::Duration::from_secs(TX_SET_REQUEST_TIMEOUT_SECS) {
                    let dont_have_set = dont_have.entry(*hash).or_insert_with(HashSet::new);
                    let already_exhausted = dont_have_set.len() >= peers.len();
                    if !already_exhausted {
                        tracing::debug!(
                            hash = %hash,
                            elapsed_secs = request_age.as_secs(),
                            peers_responded = dont_have_set.len(),
                            total_peers = peers.len(),
                            "Tx_set request timed out with no response — marking all peers as DontHave"
                        );
                        for peer in &peers {
                            dont_have_set.insert(peer.clone());
                        }
                        if !exhausted_warned.contains(hash) {
                            exhausted.push((*hash, dont_have_set.len(), peers.len()));
                        }
                        self.tx_set_all_peers_exhausted
                            .store(true, Ordering::SeqCst);
                    }
                    continue;
                }

                let start_idx =
                    Self::tx_set_start_index(hash, peers.len(), request_state.next_peer_offset);
                let eligible_peer = match dont_have.get_mut(hash) {
                    Some(set) => {
                        let found = peers
                            .iter()
                            .cycle()
                            .skip(start_idx)
                            .take(peers.len())
                            .find(|peer| !set.contains(peer));
                        if found.is_none() {
                            // All peers have said DontHave for this tx set.
                            // Track for warning (only if not already warned).
                            if !exhausted_warned.contains(hash) {
                                exhausted.push((*hash, set.len(), peers.len()));
                            }
                            self.tx_set_all_peers_exhausted
                                .store(true, Ordering::SeqCst);
                            // Don't clear the set or return a peer - stop requesting this tx set
                            // until catchup or tx_set tracking is reset.
                        }
                        found
                    }
                    None => peers.get(start_idx),
                };

                if let Some(peer_id) = eligible_peer.cloned() {
                    request_state.last_request = now;
                    request_state.next_peer_offset =
                        request_state.next_peer_offset.saturating_add(1);
                    last_request.insert(*hash, request_state);
                    reqs.push((*hash, peer_id));
                }
            }

            (reqs, exhausted)
        };

        // Log warnings for newly exhausted tx sets (only once per hash)
        if !newly_exhausted.is_empty() {
            let mut exhausted_warned = self.tx_set_exhausted_warned.write().await;
            for (hash, peers_asked, total_peers) in &newly_exhausted {
                if exhausted_warned.insert(*hash) {
                    tracing::info!(
                        hash = %hash,
                        peers_asked,
                        total_peers,
                        "All peers exhausted for tx set - triggering faster catchup"
                    );
                }
            }
        }

        for (hash, peer_id) in requests {
            tracing::debug!(hash = %hash, peer = %peer_id, "Requesting tx set");
            let request = StellarMessage::GetTxSet(stellar_xdr::curr::Uint256(hash.0));
            if let Err(e) = overlay.try_send_to(&peer_id, request) {
                tracing::warn!(hash = %hash, peer = %peer_id, error = %e, "Failed to request TxSet");
            }
        }
    }
}
