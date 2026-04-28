//! Transaction flooding: advertising, pulling, and broadcasting transactions across peers.

use super::*;
use henyey_common::protocol::soroban_supported;

const TX_ADVERT_VECTOR_MAX_SIZE: usize = 1000;
const TX_DEMAND_VECTOR_MAX_SIZE: usize = 1000;
const MAX_FLOOD_RESOURCE: usize = u32::MAX as usize;

/// Truncate `rate * limit` to i64, matching stellar-core's `getOpsFloodLedger`.
fn ops_to_flood_per_ledger(rate: f64, limit: usize) -> i64 {
    let product = rate * limit as f64;
    assert!(
        product.is_finite() && product >= 0.0 && product < i64::MAX as f64,
        "flood rate product must be representable as int64"
    );
    product as i64
}

fn dex_ops_to_flood_per_ledger(rate: f64, ops_limit: u32) -> u32 {
    let product = rate * ops_limit as f64;
    assert!(
        product.is_finite() && product >= 0.0 && product < (u32::MAX as f64 + 1.0),
        "DEX flood rate product must truncate to uint32"
    );
    product as u32
}

/// Extract the millisecond count from a `Duration`, asserting it is a
/// positive whole-millisecond value. Panics with `name` in the message
/// if the duration is zero, sub-millisecond, or has sub-ms remainder.
fn exact_millis(d: Duration, name: &str) -> u128 {
    assert!(
        d.subsec_nanos() % 1_000_000 == 0,
        "{name} must be a whole-millisecond duration"
    );
    let ms = d.as_millis();
    assert!(ms > 0, "{name} must be positive");
    ms
}

fn rounded_up_flood_budget(per_ledger: u128, period: Duration, ledger_close: Duration) -> usize {
    let period_ms = exact_millis(period, "flood period");
    let close_ms = exact_millis(ledger_close, "ledger close time");

    let numerator = per_ledger
        .checked_mul(period_ms)
        .expect("flood budget numerator overflowed");
    let quotient = numerator.div_ceil(close_ms);
    assert!(
        quotient <= i64::MAX as u128,
        "flood budget must fit stellar-core int64 result"
    );
    usize::try_from(quotient).expect("flood budget does not fit usize")
}

#[cfg(test)]
fn classic_flood_budget(
    rate: f64,
    ops_limit: usize,
    period: Duration,
    ledger_close: Duration,
) -> usize {
    let per_ledger = ops_to_flood_per_ledger(rate, ops_limit);
    rounded_up_flood_budget(per_ledger as u128, period, ledger_close)
}

/// Compute the combined classic+Soroban flood budget.
///
/// Matches stellar-core: sum `getOpsFloodLedger` for classic and Soroban
/// as i64 first (one per-ledger total), then a single `bigDivideOrThrow`
/// with ROUND_UP for the period fraction, then clamp externally.
fn combined_flood_budget(
    classic_rate: f64,
    classic_limit: usize,
    soroban_rate: f64,
    soroban_limit: usize,
    period: Duration,
    ledger_close: Duration,
) -> usize {
    let classic = ops_to_flood_per_ledger(classic_rate, classic_limit);
    let soroban = ops_to_flood_per_ledger(soroban_rate, soroban_limit);
    let total = classic
        .checked_add(soroban)
        .expect("combined flood per-ledger limit must fit stellar-core int64");
    rounded_up_flood_budget(total as u128, period, ledger_close)
}

fn dex_flood_budget(rate: f64, ops_limit: u32, period: Duration, ledger_close: Duration) -> usize {
    let per_ledger = dex_ops_to_flood_per_ledger(rate, ops_limit);
    rounded_up_flood_budget(per_ledger as u128, period, ledger_close)
}

fn add_flood_carryover(base: usize, carryover: usize) -> usize {
    let total = base
        .checked_add(carryover)
        .expect("flood budget plus carry-over overflowed");
    assert!(
        total <= MAX_FLOOD_RESOURCE,
        "flood budget must fit stellar-core uint32 resource"
    );
    total
}

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

    /// Returns `ledger_max_tx_count` from Soroban network config as a scalar
    /// flood-budget limit, or 0 when Soroban is not supported on the current
    /// protocol version or when network info is not yet available.
    fn soroban_flood_tx_limit(&self) -> usize {
        if soroban_supported(
            self.ledger_manager()
                .header_snapshot()
                .header
                .ledger_version,
        ) {
            self.soroban_network_info()
                .map(|info| info.ledger_max_tx_count as usize)
                .unwrap_or(0)
        } else {
            0
        }
    }

    /// Compute the combined classic+Soroban ops flood budget for one period.
    ///
    /// Wraps [`combined_flood_budget`] with the overlay config's flood rates
    /// and the current ledger close duration. Callers supply capacity limits
    /// (ledger-max or queue-max) and the period for their specific use case.
    fn combined_ops_flood_budget(
        &self,
        classic_limit: usize,
        soroban_limit: usize,
        period: Duration,
    ) -> usize {
        combined_flood_budget(
            self.config.overlay.flood_op_rate_per_ledger,
            classic_limit,
            self.config.overlay.flood_soroban_rate_per_ledger,
            soroban_limit,
            period,
            self.herder.ledger_close_duration(),
        )
    }

    /// Compute the per-period ops budget for transaction flooding.
    ///
    /// Computes the combined classic + Soroban broadcast budget using
    /// `flood_tx_period_ms`. In stellar-core, classic and Soroban queues
    /// have separate broadcast budgets; henyey's unified queue requires
    /// a combined budget here.
    fn compute_flood_ops_budget(&self) -> usize {
        let base_budget = self.combined_ops_flood_budget(
            self.herder.max_tx_set_size(),
            self.soroban_flood_tx_limit(),
            self.flood_tx_period(),
        );
        let carryover = self.broadcast_op_carryover.load(Ordering::Relaxed);
        add_flood_carryover(base_budget, carryover)
    }

    /// Compute the per-period DEX ops budget for transaction flooding.
    ///
    /// Returns `None` when `MAX_DEX_TX_OPERATIONS_IN_TX_SET` is not configured,
    /// meaning DEX transactions are uncapped. When configured, mirrors
    /// stellar-core's DEX clamp, truncate, round-up, then carry-over sequence.
    fn compute_dex_flood_ops_budget(&self) -> Option<usize> {
        let max_dex_ops = self.config.surge_pricing.max_dex_tx_operations?;
        let max_ops = self.herder.max_tx_set_size() as u32;
        let effective_dex_ops = max_dex_ops.min(max_ops);
        let base = dex_flood_budget(
            self.config.overlay.flood_op_rate_per_ledger,
            effective_dex_ops,
            self.flood_tx_period(),
            self.herder.ledger_close_duration(),
        );
        let carryover = self.broadcast_dex_op_carryover.load(Ordering::Relaxed);
        Some(add_flood_carryover(base, carryover))
    }

    /// Maximum carry-over ops between flood periods.
    /// Matches stellar-core's cap at MAX_OPS_PER_TX + 1 to allow one worst-case
    /// fee-bump transaction in carry-over.
    const MAX_CARRYOVER_OPS: usize = 101; // MAX_OPS_PER_TX (100) + 1

    pub(super) async fn flush_tx_adverts(&self) {
        let ops_budget = self.compute_flood_ops_budget();
        let dex_ops_budget = self.compute_dex_flood_ops_budget();

        let Some(overlay) = self.overlay().await else {
            // No overlay — preserve carry-over (capped).
            self.store_carryover(ops_budget, dex_ops_budget);
            return;
        };

        let snapshots = overlay.peer_snapshots();
        if snapshots.is_empty() {
            // No peers — preserve carry-over (capped).
            self.store_carryover(ops_budget, dex_ops_budget);
            return;
        }

        let peer_ids: Vec<_> = snapshots
            .iter()
            .map(|snapshot| snapshot.info.peer_id.clone())
            .collect();
        let peer_set: HashSet<_> = peer_ids.iter().cloned().collect();
        let ledger_seq = self.herder.tracking_slot().saturating_sub(1) as u32;

        // XDR vector chunk size — cap at 1000 per the protocol limit.
        let max_chunk_size = self.max_advert_size().min(1000);

        // Phase 0: Prune stale peers and ensure entries exist for all active peers.
        {
            let mut adverts_by_peer = self.tx_adverts_by_peer.write().await;
            adverts_by_peer.retain(|peer, _| peer_set.contains(peer));
            for peer_id in &peer_ids {
                adverts_by_peer
                    .entry(peer_id.clone())
                    .or_insert_with(PeerTxAdverts::new);
            }
        }

        // Phase 1: Traverse candidates with the visitor API.
        // Budget-fit checks in broadcast_with_visitor ensure only fitting
        // candidates reach our closure. Skipped candidates are budget-neutral.
        let mut budget = BroadcastBudget {
            ops_remaining: ops_budget,
            dex_ops_remaining: dex_ops_budget,
        };
        let per_peer = {
            let adverts_by_peer = self.tx_adverts_by_peer.read().await;
            collect_adverts_for_peers(
                self.herder.tx_queue(),
                &mut budget,
                &peer_ids,
                &adverts_by_peer,
            )
        };

        let ops_used = ops_budget.saturating_sub(budget.ops_remaining);
        let dex_ops_used = dex_ops_budget
            .zip(budget.dex_ops_remaining)
            .map(|(orig, rem)| orig.saturating_sub(rem))
            .unwrap_or(0);

        tracing::debug!(
            new_adverts = per_peer.values().map(|v| v.len()).sum::<usize>(),
            ops_used,
            ops_budget,
            dex_ops_used,
            ?dex_ops_budget,
            "Flushing tx adverts (priority-ordered)"
        );

        // Update carry-over from remaining budget (capped).
        self.broadcast_op_carryover.store(
            budget.ops_remaining.min(Self::MAX_CARRYOVER_OPS),
            Ordering::Relaxed,
        );
        if let Some(dex_remaining) = budget.dex_ops_remaining {
            self.broadcast_dex_op_carryover.store(
                dex_remaining.min(Self::MAX_CARRYOVER_OPS),
                Ordering::Relaxed,
            );
        }

        if per_peer.is_empty() {
            return;
        }

        // Phase 2: Send adverts and mark successfully sent hashes.
        for (peer_id, hashes) in &per_peer {
            for chunk in hashes.chunks(max_chunk_size) {
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
                match overlay.try_send_to(peer_id, StellarMessage::FloodAdvert(advert)) {
                    Ok(()) => {
                        // Mark as sent only after successful send.
                        let mut adverts_by_peer = self.tx_adverts_by_peer.write().await;
                        if let Some(adverts) = adverts_by_peer.get_mut(peer_id) {
                            for hash in chunk {
                                adverts.remember(*hash, ledger_seq);
                            }
                        }
                    }
                    Err(e) => {
                        tracing::debug!(peer = %peer_id, error = %e, "Failed to send tx advert batch");
                        // Don't mark — will retry next period.
                    }
                }
            }
        }
    }

    /// Store carry-over budgets (capped) for the next flood period.
    fn store_carryover(&self, ops_budget: usize, dex_ops_budget: Option<usize>) {
        self.broadcast_op_carryover
            .store(ops_budget.min(Self::MAX_CARRYOVER_OPS), Ordering::Relaxed);
        if let Some(dex_budget) = dex_ops_budget {
            self.broadcast_dex_op_carryover
                .store(dex_budget.min(Self::MAX_CARRYOVER_OPS), Ordering::Relaxed);
        }
    }

    /// Period for the transaction broadcast cycle (budget + advert flush).
    ///
    /// Matches stellar-core's `FLOOD_TX_PERIOD_MS` (default 200 ms).
    pub(super) fn flood_tx_period(&self) -> Duration {
        Duration::from_millis(self.config.overlay.flood_tx_period_ms.max(1))
    }

    pub(super) fn flood_demand_period(&self) -> Duration {
        Duration::from_millis(self.config.overlay.flood_demand_period_ms.max(1))
    }

    fn flood_advert_period(&self) -> Duration {
        Duration::from_millis(self.config.overlay.flood_advert_period_ms.max(1))
    }

    fn flood_demand_backoff_delay(&self) -> Duration {
        Duration::from_millis(self.config.overlay.flood_demand_backoff_delay_ms.max(1))
    }

    fn max_advert_queue_size(&self) -> usize {
        self.herder.max_tx_set_size().max(1)
    }

    fn max_advert_size(&self) -> usize {
        // stellar-core: TxAdverts::getMaxAdvertSize()
        let per_period = self.combined_ops_flood_budget(
            self.herder.max_tx_set_size(),
            self.soroban_flood_tx_limit(),
            self.flood_advert_period(),
        );
        per_period.clamp(1, TX_ADVERT_VECTOR_MAX_SIZE)
    }

    fn max_demand_size(&self) -> usize {
        // stellar-core: TxDemandsManager::getMaxDemandSize()
        // Uses queue-capacity limits (not ledger-max) per stellar-core.
        let per_period = self.combined_ops_flood_budget(
            self.herder.max_queue_size_ops(),
            self.herder.max_queue_size_soroban_ops(),
            self.flood_demand_period(),
        );
        per_period.clamp(1, TX_DEMAND_VECTOR_MAX_SIZE)
    }

    fn retry_delay_demand(&self, attempts: usize) -> Duration {
        let delay_ms = self
            .flood_demand_backoff_delay()
            .as_millis()
            .saturating_mul(attempts as u128);
        Duration::from_millis(delay_ms.min(2000) as u64)
    }

    pub(super) async fn clear_tx_advert_history(&self, ledger_seq: u32) {
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
        entry.queue_incoming(&advert.tx_hashes.0, ledger_seq, max_ops);
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
                match overlay.try_send_to(
                    peer_id,
                    StellarMessage::Transaction(Arc::unwrap_or_clone(tx.envelope)),
                ) {
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
        let hash = TransactionSet::compute_non_generalized_hash(prev_hash, &transactions);

        // Create our internal TransactionSet with correct hash
        let internal_tx_set = TransactionSet::with_hash(prev_hash, hash, transactions);
        {
            let mut map = self.tx_set_dont_have.write().await;
            map.remove(internal_tx_set.hash());
        }
        {
            let mut map = self.tx_set_last_request.write().await;
            map.remove(internal_tx_set.hash());
        }

        tracing::info!(
            hash = %internal_tx_set.hash(),
            tx_count = internal_tx_set.len(),
            "Processing TxSet"
        );

        if !self.herder.needs_tx_set(internal_tx_set.hash()) {
            tracing::info!(hash = %internal_tx_set.hash(), "TxSet not pending");
        }

        // `receive_tx_set` is async as of #1773: the envelope-drain phase
        // runs on a blocking-pool thread so the event loop stays free
        // during the 300+ ms dispatch.
        let received_slot = self
            .herder
            .clone()
            .receive_tx_set(internal_tx_set.clone())
            .await;
        if let Some(slot) = received_slot {
            tracing::info!(slot, "Received pending TxSet, attempting ledger close");
            if let Some(pending) = self.process_externalized_slots().await {
                *self.deferred_catchup.lock().await = Some(pending);
            }
        } else if self.attach_tx_set_by_hash(&internal_tx_set).await
            || self.buffer_externalized_tx_set(&internal_tx_set).await
        {
            // Buffered — the event loop's pending_close chaining will pick
            // up the close via try_start_ledger_close on the next tick.
        }
    }

    /// Handle a GeneralizedTxSet message from the network.
    pub(super) async fn handle_generalized_tx_set(
        &self,
        gen_tx_set: stellar_xdr::curr::GeneralizedTransactionSet,
    ) {
        // Time the full body (#1759 diagnostics): this runs inline on
        // the event loop for every GeneralizedTxSet, including the
        // bulk-drain paths in the post-close and consensus-tick arms,
        // so sustained slow calls here can drive phase=2/6 freezes.
        let hgts_start = std::time::Instant::now();
        let result = self.handle_generalized_tx_set_inner(gen_tx_set).await;
        super::warn_if_slow(hgts_start.elapsed(), "handle_generalized_tx_set", 0);
        result
    }

    async fn handle_generalized_tx_set_inner(
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

        // Time-wrapped (#1759 diagnostics): acquires
        // `Herder::scp_driver.needs_tx_set`'s internal
        // parking_lot::RwLock on every inbound GeneralizedTxSet.
        if !tracked_lock::time_call("herder.needs_tx_set", || self.herder.needs_tx_set(&hash)) {
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
            map.remove(internal_tx_set.hash());
        }
        {
            let mut map = self.tx_set_last_request.write().await;
            map.remove(internal_tx_set.hash());
        }

        // `receive_tx_set` is async as of #1773: the envelope-drain phase
        // runs on a blocking-pool thread so the event loop stays free
        // during the 300+ ms dispatch. The inner `PhaseTimer` inside
        // `receive_tx_set` emits the per-phase WARN when slow; the outer
        // `time_call` here has been removed because the Phase-2 rework
        // makes the total wall time include a spawn_blocking await,
        // which is expected to be long while still being non-blocking
        // for the event loop. The inner `process_ready_spawn_blocking_ms`
        // phase name signals whether the fix is behaving as intended.
        let received_slot = self
            .herder
            .clone()
            .receive_tx_set(internal_tx_set.clone())
            .await;
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
            // Buffered — the event loop's pending_close chaining will pick
            // up the close via try_start_ledger_close on the next tick.
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
            .generalized_tx_set()
            .cloned()
            .or_else(|| tx_set.to_generalized_tx_set())
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
        let prev_hash = tx_set.previous_ledger_hash();
        let xdr_tx_set = stellar_xdr::curr::TransactionSet {
            previous_ledger_hash: Hash::from(prev_hash),
            txs: tx_set.transactions_owned().try_into().unwrap_or_default(),
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
        let peers = Self::tx_set_eligible_peers(&peer_infos);
        if peers.is_empty() {
            return;
        }

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
                        self.mark_tx_set_exhausted();
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
                            self.mark_tx_set_exhausted();
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

    /// Record the false→true transition of `tx_set_all_peers_exhausted` and
    /// stamp `tx_set_exhausted_since` on the first transition only.
    pub(super) fn mark_tx_set_exhausted(&self) {
        if self
            .tx_set_all_peers_exhausted
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            let elapsed = self.start_instant.elapsed().as_secs().max(1);
            self.tx_set_exhausted_since.store(elapsed, Ordering::SeqCst);
        }
    }

    /// Clear `tx_set_all_peers_exhausted` and its timestamp together.
    /// Use this instead of bare `.store(false)` to keep the flag and timestamp
    /// in sync at every transition point.
    pub(super) fn clear_tx_set_exhausted(&self) {
        self.tx_set_all_peers_exhausted
            .store(false, Ordering::SeqCst);
        self.tx_set_exhausted_since.store(0, Ordering::SeqCst);
    }

    /// Return the seconds-since-start at which `tx_set_all_peers_exhausted`
    /// first became true, or 0 if not exhausted.
    pub(crate) fn tx_set_exhausted_since_offset(&self) -> u64 {
        self.tx_set_exhausted_since.load(Ordering::SeqCst)
    }

    /// Build the eligible peer list for tx-set requests, preferring outbound
    /// peers. This is shared between `request_pending_tx_sets` and
    /// `retry_exhausted_tx_sets` so both use the same peer universe.
    pub(super) fn tx_set_eligible_peers(
        peer_infos: &[henyey_overlay::PeerInfo],
    ) -> Vec<henyey_overlay::PeerId> {
        let mut peers = Vec::new();
        let mut fallback = Vec::new();
        for info in peer_infos {
            fallback.push(info.peer_id.clone());
            if matches!(info.direction, ConnectionDirection::Outbound) {
                peers.push(info.peer_id.clone());
            }
        }
        if peers.is_empty() {
            peers = fallback;
        }
        peers.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
        peers
    }

    /// Retry fetching exhausted tx_sets with per-hash 30s backoff.
    ///
    /// When all peers have reported DontHave for every pending tx_set hash,
    /// the normal `request_pending_tx_sets` stops requesting them. If the
    /// archive hasn't published the next checkpoint yet, the node stalls for
    /// minutes. This method re-asks peers that may have re-acquired the
    /// tx_set (e.g., via a slow peer catching up).
    ///
    /// Called from the recovery fallback path in `trigger_recovery_catchup`.
    pub(super) async fn retry_exhausted_tx_sets(&self) {
        if !self.tx_set_all_peers_exhausted.load(Ordering::SeqCst) {
            return;
        }

        let current_ledger = match self.get_current_ledger().await {
            Ok(seq) => seq,
            Err(_) => return,
        };
        let min_slot = current_ledger.saturating_add(1) as u64;
        let window_end = current_ledger as u64 + TX_SET_REQUEST_WINDOW;

        let pending = self.herder.get_pending_tx_sets();
        let pending_hashes: Vec<Hash256> = pending
            .into_iter()
            .filter(|(_, slot)| *slot >= min_slot && *slot <= window_end)
            .map(|(hash, _)| hash)
            .collect();
        if pending_hashes.is_empty() {
            return;
        }

        let Some(overlay) = self.overlay().await else {
            return;
        };
        let peer_infos = overlay.peer_infos();
        let peers = Self::tx_set_eligible_peers(&peer_infos);
        if peers.is_empty() {
            return;
        }

        let now = self.clock.now();
        const RETRY_BACKOFF: Duration = Duration::from_secs(30);
        const MAX_RETRIES_PER_TICK: usize = 4;

        let retry_hashes = {
            let mut dont_have = self.tx_set_dont_have.write().await;
            let mut last_request = self.tx_set_last_request.write().await;
            let mut exhausted_warned = self.tx_set_exhausted_warned.write().await;
            let mut last_retry = self.tx_set_last_retry.write().await;

            let mut to_retry = Vec::new();

            for hash in &pending_hashes {
                if to_retry.len() >= MAX_RETRIES_PER_TICK {
                    break;
                }

                // Only retry hashes where ALL eligible peers are in dont_have.
                let is_exhausted = dont_have
                    .get(hash)
                    .map(|set| peers.iter().all(|p| set.contains(p)))
                    .unwrap_or(false);
                if !is_exhausted {
                    continue;
                }

                // 30s per-hash backoff.
                if let Some(&prev) = last_retry.get(hash) {
                    if now.duration_since(prev) < RETRY_BACKOFF {
                        continue;
                    }
                }

                // Clear per-hash state for retry.
                dont_have.remove(hash);
                exhausted_warned.remove(hash);
                // Reset request tracking so request_pending_tx_sets doesn't
                // immediately re-exhaust via the timeout path.
                last_request.remove(hash);
                last_retry.insert(*hash, now);

                to_retry.push(*hash);
            }

            // Recompute global flag: any remaining hash still exhausted?
            let any_still_exhausted = pending_hashes.iter().any(|hash| {
                dont_have
                    .get(hash)
                    .map(|set| peers.iter().all(|p| set.contains(p)))
                    .unwrap_or(false)
            });

            if !any_still_exhausted {
                self.clear_tx_set_exhausted();
            }

            to_retry
        };

        // Broadcast GetTxSet to all eligible peers for retried hashes.
        for hash in &retry_hashes {
            tracing::info!(
                hash = %hash,
                peer_count = peers.len(),
                "Retrying exhausted tx_set fetch — broadcasting to all eligible peers"
            );
            let msg = StellarMessage::GetTxSet(stellar_xdr::curr::Uint256(hash.0));
            for peer in &peers {
                let _ = overlay.try_send_to(peer, msg.clone());
            }
        }
    }
}

/// Traverse the queue's broadcast candidates and collect per-peer advert lists.
///
/// For each candidate, checks which peers haven't seen it yet. If at least one
/// peer needs it, returns [`BroadcastVisitResult::Processed`] (budget consumed);
/// if all peers already know it, returns [`BroadcastVisitResult::Skipped`]
/// (budget-neutral).
///
/// # Precondition
///
/// Every entry in `peer_ids` must have a corresponding entry in `adverts_by_peer`.
/// Phase 0 of [`App::flush_tx_adverts`] establishes this invariant before calling
/// this function.
fn collect_adverts_for_peers(
    queue: &henyey_herder::TransactionQueue,
    budget: &mut BroadcastBudget,
    peer_ids: &[henyey_overlay::PeerId],
    adverts_by_peer: &HashMap<henyey_overlay::PeerId, PeerTxAdverts>,
) -> HashMap<henyey_overlay::PeerId, Vec<Hash256>> {
    debug_assert!(
        peer_ids.iter().all(|pid| adverts_by_peer.contains_key(pid)),
        "collect_adverts_for_peers: every peer_id must have an entry in adverts_by_peer"
    );

    let mut per_peer: HashMap<henyey_overlay::PeerId, Vec<Hash256>> = HashMap::new();
    queue.broadcast_with_visitor(budget, |candidate| {
        let mut new_to_any_peer = false;
        for peer_id in peer_ids {
            if let Some(adverts) = adverts_by_peer.get(peer_id) {
                if !adverts.seen_advert(&candidate.hash) {
                    new_to_any_peer = true;
                    per_peer
                        .entry(peer_id.clone())
                        .or_default()
                        .push(candidate.hash);
                }
            }
        }
        if new_to_any_peer {
            BroadcastVisitResult::Processed
        } else {
            BroadcastVisitResult::Skipped
        }
    });
    per_peer
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::time::Duration;

    fn ms(v: u64) -> Duration {
        Duration::from_millis(v)
    }

    #[test]
    fn test_classic_flood_budget_truncates_before_division() {
        assert_eq!(classic_flood_budget(0.51, 10, ms(3), ms(10)), 2);
    }

    #[test]
    fn test_classic_flood_budget_can_truncate_to_zero() {
        let base = classic_flood_budget(0.009, 100, ms(200), ms(5000));

        assert_eq!(base, 0);
        assert_eq!(add_flood_carryover(base, 7), 7);
    }

    #[test]
    fn test_rounded_up_flood_budget_uses_integer_ceiling() {
        assert_eq!(rounded_up_flood_budget(7, ms(200), ms(5000)), 1);
        assert_eq!(rounded_up_flood_budget(125, ms(200), ms(5000)), 5);
    }

    #[test]
    fn test_vector_size_clamps_zero_to_one() {
        let per_period = classic_flood_budget(0.001, 10, ms(100), ms(5000));

        assert_eq!(per_period, 0);
        assert_eq!(per_period.clamp(1, TX_ADVERT_VECTOR_MAX_SIZE), 1);
        assert_eq!(per_period.clamp(1, TX_DEMAND_VECTOR_MAX_SIZE), 1);
    }

    #[test]
    #[should_panic(expected = "flood rate product must be representable as int64")]
    fn test_classic_flood_budget_rejects_non_finite_rate() {
        let _ = classic_flood_budget(f64::NAN, 100, ms(200), ms(5000));
    }

    #[test]
    #[should_panic(expected = "flood rate product must be representable as int64")]
    fn test_classic_flood_budget_rejects_too_large_product() {
        let _ = classic_flood_budget(i64::MAX as f64, 1, ms(200), ms(5000));
    }

    #[test]
    fn test_combined_flood_budget_adds_soroban_before_division() {
        // classic: 0.1 * 10 = 1, soroban: 0.1 * 10 = 1, total = 2
        // 2 * 1 / 2 = 1 (exact)
        assert_eq!(combined_flood_budget(0.1, 10, 0.1, 10, ms(1), ms(2)), 1);
    }

    #[test]
    fn test_combined_flood_budget_matches_classic_when_soroban_zero() {
        assert_eq!(
            combined_flood_budget(0.51, 10, 0.9, 0, ms(3), ms(10)),
            classic_flood_budget(0.51, 10, ms(3), ms(10))
        );
    }

    #[test]
    fn test_combined_flood_budget_soroban_increases_budget() {
        // With Soroban limit > 0, the combined budget must be strictly larger
        // than classic-only. This is the regression test for the bug where
        // compute_flood_ops_budget used classic_flood_budget only.
        let classic_only = classic_flood_budget(0.5, 100, ms(200), ms(5000));
        let combined = combined_flood_budget(0.5, 100, 0.9, 50, ms(200), ms(5000));
        assert!(
            combined > classic_only,
            "combined budget ({combined}) must exceed classic-only ({classic_only})"
        );
    }

    #[test]
    #[should_panic(expected = "combined flood per-ledger limit must fit stellar-core int64")]
    fn test_combined_flood_budget_rejects_overflow() {
        // Each term fits i64, but their sum overflows.
        let half = (i64::MAX / 2 + 1) as f64;
        let _ = combined_flood_budget(half, 1, half, 1, ms(200), ms(5000));
    }

    #[test]
    #[should_panic(expected = "flood period must be positive")]
    fn test_rounded_up_flood_budget_rejects_zero_period() {
        let _ = rounded_up_flood_budget(1, Duration::ZERO, ms(5000));
    }

    #[test]
    #[should_panic(expected = "ledger close time must be positive")]
    fn test_rounded_up_flood_budget_rejects_zero_ledger_close() {
        let _ = rounded_up_flood_budget(1, ms(200), Duration::ZERO);
    }

    #[test]
    #[should_panic(expected = "flood period must be a whole-millisecond duration")]
    fn test_rounded_up_flood_budget_rejects_sub_ms_period() {
        let _ = rounded_up_flood_budget(1, Duration::from_micros(1500), ms(5000));
    }

    #[test]
    #[should_panic(expected = "ledger close time must be a whole-millisecond duration")]
    fn test_rounded_up_flood_budget_rejects_sub_ms_ledger_close() {
        let _ = rounded_up_flood_budget(1, ms(200), Duration::from_micros(1500));
    }

    #[test]
    fn test_dex_flood_budget_clamps_before_truncation() {
        let max_ops = 100u32;
        let max_dex_ops = 1000u32;
        let effective_dex_ops = max_dex_ops.min(max_ops);

        assert_eq!(
            dex_flood_budget(0.51, effective_dex_ops, ms(200), ms(5000)),
            3
        );
    }

    #[test]
    fn test_dex_product_allows_fractional_u32_upper_bound() {
        assert_eq!(
            dex_ops_to_flood_per_ledger(u32::MAX as f64 + 0.5, 1),
            u32::MAX
        );
    }

    #[test]
    #[should_panic(expected = "DEX flood rate product must truncate to uint32")]
    fn test_dex_product_rejects_values_that_do_not_truncate_to_u32() {
        let _ = dex_ops_to_flood_per_ledger(u32::MAX as f64 + 1.0, 1);
    }

    #[test]
    #[should_panic(expected = "flood budget must fit stellar-core uint32 resource")]
    fn test_flood_carryover_rejects_final_resource_overflow() {
        let _ = add_flood_carryover(u32::MAX as usize, 1);
    }

    /// Verify that `classic_flood_budget` uses the tx period (200 ms by default),
    /// not the advert period (100 ms), producing a larger per-period budget.
    #[test]
    fn test_budget_uses_tx_period_not_advert_period() {
        let rate = 0.5;
        let ops_limit = 100;
        let ledger_close = ms(5000);

        let advert_period = ms(100); // flood_advert_period_ms default
        let tx_period = ms(200); // flood_tx_period_ms default

        let budget_advert = classic_flood_budget(rate, ops_limit, advert_period, ledger_close);
        let budget_tx = classic_flood_budget(rate, ops_limit, tx_period, ledger_close);

        // tx period (200ms) should yield double the per-period budget vs advert period (100ms)
        assert_eq!(budget_tx, budget_advert * 2);
        // Verify concrete values: 0.5 * 100 = 50 per ledger
        // 50 * 200 / 5000 = 2, 50 * 100 / 5000 = 1
        assert_eq!(budget_tx, 2);
        assert_eq!(budget_advert, 1);
    }

    /// Verify that `max_advert_size` computation uses advert_period_ms (100ms).
    /// The per-period budget for advert sizing should be half the broadcast budget.
    #[test]
    fn test_advert_size_uses_advert_period() {
        let rate = 0.5;
        let ops_limit = 100;
        let advert_period = ms(100);
        let ledger_close = ms(5000);

        let advert_budget = classic_flood_budget(rate, ops_limit, advert_period, ledger_close);
        // With advert period 100ms: 50 * 100 / 5000 = 1
        assert_eq!(advert_budget, 1);
    }

    /// Verify the default config values match stellar-core defaults.
    #[test]
    fn test_flood_period_defaults() {
        let config = crate::config::OverlayConfig::default();
        assert_eq!(config.flood_tx_period_ms, 200); // stellar-core FLOOD_TX_PERIOD_MS
        assert_eq!(config.flood_advert_period_ms, 100); // stellar-core FLOOD_ADVERT_PERIOD_MS
    }

    // ── Test helpers for collect_adverts_for_peers ────────────────────────

    use henyey_herder::{TransactionQueue, TxQueueConfig, TxQueueResult};
    use stellar_xdr::curr::{
        AccountId, AlphaNum4, Asset, AssetCode4, DecoratedSignature, ManageSellOfferOp, Memo,
        MuxedAccount, Operation, OperationBody, Preconditions, Price, PublicKey, SequenceNumber,
        Signature, SignatureHint, Transaction, TransactionEnvelope, TransactionExt,
        TransactionV1Envelope, Uint256,
    };

    fn test_queue_config() -> TxQueueConfig {
        TxQueueConfig {
            validate_signatures: false,
            validate_time_bounds: false,
            ..Default::default()
        }
    }

    fn make_envelope(fee: u32, ops: usize) -> TransactionEnvelope {
        let source = MuxedAccount::Ed25519(Uint256([0u8; 32]));
        let operations: Vec<Operation> = (0..ops)
            .map(|_| Operation {
                source_account: None,
                body: OperationBody::BumpSequence(stellar_xdr::curr::BumpSequenceOp {
                    bump_to: SequenceNumber(0),
                }),
            })
            .collect();
        let tx = Transaction {
            source_account: source,
            fee,
            seq_num: SequenceNumber(1),
            cond: Preconditions::None,
            memo: Memo::None,
            operations: operations.try_into().unwrap(),
            ext: TransactionExt::V0,
        };
        TransactionEnvelope::Tx(TransactionV1Envelope {
            tx,
            signatures: vec![DecoratedSignature {
                hint: SignatureHint([0u8; 4]),
                signature: Signature(vec![0u8; 64].try_into().unwrap()),
            }]
            .try_into()
            .unwrap(),
        })
    }

    fn set_source(envelope: &mut TransactionEnvelope, seed: u8) {
        match envelope {
            TransactionEnvelope::Tx(ref mut env) => {
                env.tx.source_account = MuxedAccount::Ed25519(Uint256([seed; 32]));
            }
            _ => panic!("Expected Tx variant"),
        }
    }

    fn make_dex_envelope(fee: u32, ops: usize) -> TransactionEnvelope {
        let source = MuxedAccount::Ed25519(Uint256([0u8; 32]));
        let operations: Vec<Operation> = (0..ops)
            .map(|_| Operation {
                source_account: None,
                body: OperationBody::ManageSellOffer(ManageSellOfferOp {
                    selling: Asset::Native,
                    buying: Asset::CreditAlphanum4(AlphaNum4 {
                        asset_code: AssetCode4(*b"USD\0"),
                        issuer: AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([1u8; 32]))),
                    }),
                    amount: 100,
                    price: Price { n: 1, d: 1 },
                    offer_id: 0,
                }),
            })
            .collect();
        let tx = Transaction {
            source_account: source,
            fee,
            seq_num: SequenceNumber(1),
            cond: Preconditions::None,
            memo: Memo::None,
            operations: operations.try_into().unwrap(),
            ext: TransactionExt::V0,
        };
        TransactionEnvelope::Tx(TransactionV1Envelope {
            tx,
            signatures: vec![DecoratedSignature {
                hint: SignatureHint([0u8; 4]),
                signature: Signature(vec![0u8; 64].try_into().unwrap()),
            }]
            .try_into()
            .unwrap(),
        })
    }

    fn make_peer_id(seed: u8) -> henyey_overlay::PeerId {
        henyey_overlay::PeerId(PublicKey::PublicKeyTypeEd25519(Uint256([seed; 32])))
    }

    // ── collect_adverts_for_peers tests ──────────────────────────────────

    #[test]
    fn test_flush_adverts_budget_neutral_when_all_peers_seen() {
        let queue = TransactionQueue::new(test_queue_config());
        let mut env = make_envelope(100, 1);
        set_source(&mut env, 1);
        let hash = Hash256::hash_xdr(&env);
        assert_eq!(queue.try_add(env), TxQueueResult::Added);

        let peer_a = make_peer_id(10);
        let peer_b = make_peer_id(20);
        let peer_ids = vec![peer_a.clone(), peer_b.clone()];

        let mut adverts: HashMap<henyey_overlay::PeerId, PeerTxAdverts> = HashMap::new();
        let mut pa = PeerTxAdverts::new();
        pa.remember(hash, 1);
        adverts.insert(peer_a, pa);
        let mut pb = PeerTxAdverts::new();
        pb.remember(hash, 1);
        adverts.insert(peer_b, pb);

        let mut budget = BroadcastBudget {
            ops_remaining: 10,
            dex_ops_remaining: None,
        };

        let per_peer = collect_adverts_for_peers(&queue, &mut budget, &peer_ids, &adverts);

        assert!(per_peer.is_empty(), "no peer should receive a known tx");
        assert_eq!(
            budget.ops_remaining, 10,
            "budget must be unchanged (skip is budget-neutral)"
        );
    }

    #[test]
    fn test_flush_adverts_routes_to_correct_peers() {
        let queue = TransactionQueue::new(test_queue_config());

        let mut tx_high = make_envelope(200, 1); // fee-per-op = 200
        set_source(&mut tx_high, 1);
        let hash_high = Hash256::hash_xdr(&tx_high);
        assert_eq!(queue.try_add(tx_high), TxQueueResult::Added);

        let mut tx_low = make_envelope(100, 1); // fee-per-op = 100
        set_source(&mut tx_low, 2);
        let hash_low = Hash256::hash_xdr(&tx_low);
        assert_eq!(queue.try_add(tx_low), TxQueueResult::Added);

        let peer_a = make_peer_id(10);
        let peer_b = make_peer_id(20);
        let peer_ids = vec![peer_a.clone(), peer_b.clone()];

        let mut adverts: HashMap<henyey_overlay::PeerId, PeerTxAdverts> = HashMap::new();
        // Peer A has already seen the high-fee tx.
        let mut pa = PeerTxAdverts::new();
        pa.remember(hash_high, 1);
        adverts.insert(peer_a.clone(), pa);
        // Peer B has not seen either tx.
        adverts.insert(peer_b.clone(), PeerTxAdverts::new());

        let mut budget = BroadcastBudget {
            ops_remaining: 10,
            dex_ops_remaining: None,
        };

        let per_peer = collect_adverts_for_peers(&queue, &mut budget, &peer_ids, &adverts);

        // Peer A should only get the low-fee tx (already seen the high-fee one).
        let a_hashes = per_peer.get(&peer_a).expect("peer_a should have adverts");
        assert_eq!(a_hashes, &[hash_low]);

        // Peer B should get both in fee-per-op descending order.
        let b_hashes = per_peer.get(&peer_b).expect("peer_b should have adverts");
        assert_eq!(b_hashes, &[hash_high, hash_low]);

        // Both txs were Processed (each is new to at least one peer).
        assert_eq!(budget.ops_remaining, 8, "2 ops consumed (1 per tx)");
    }

    #[test]
    fn test_flush_adverts_carry_over_reflects_only_processed() {
        let queue = TransactionQueue::new(test_queue_config());

        let mut tx_known = make_envelope(200, 1); // higher fee, visited first
        set_source(&mut tx_known, 1);
        let hash_known = Hash256::hash_xdr(&tx_known);
        assert_eq!(queue.try_add(tx_known), TxQueueResult::Added);

        let mut tx_new = make_envelope(100, 1); // lower fee, visited second
        set_source(&mut tx_new, 2);
        assert_eq!(queue.try_add(tx_new), TxQueueResult::Added);

        let peer = make_peer_id(10);
        let peer_ids = vec![peer.clone()];

        let mut adverts: HashMap<henyey_overlay::PeerId, PeerTxAdverts> = HashMap::new();
        let mut pa = PeerTxAdverts::new();
        pa.remember(hash_known, 1); // peer already knows tx_known
        adverts.insert(peer, pa);

        let mut budget = BroadcastBudget {
            ops_remaining: 10,
            dex_ops_remaining: None,
        };

        let _per_peer = collect_adverts_for_peers(&queue, &mut budget, &peer_ids, &adverts);

        // tx_known was Skipped (budget-neutral), tx_new was Processed (1 op consumed).
        assert_eq!(budget.ops_remaining, 9, "only tx_new should consume budget");
    }

    #[test]
    fn test_flush_adverts_dex_skip_is_budget_neutral() {
        let queue = TransactionQueue::new(test_queue_config());

        let mut dex_env = make_dex_envelope(100, 1);
        set_source(&mut dex_env, 1);
        let hash = Hash256::hash_xdr(&dex_env);
        assert_eq!(queue.try_add(dex_env), TxQueueResult::Added);

        let peer = make_peer_id(10);
        let peer_ids = vec![peer.clone()];

        let mut adverts: HashMap<henyey_overlay::PeerId, PeerTxAdverts> = HashMap::new();
        let mut pa = PeerTxAdverts::new();
        pa.remember(hash, 1);
        adverts.insert(peer, pa);

        let mut budget = BroadcastBudget {
            ops_remaining: 10,
            dex_ops_remaining: Some(5),
        };

        let per_peer = collect_adverts_for_peers(&queue, &mut budget, &peer_ids, &adverts);

        assert!(per_peer.is_empty(), "known DEX tx should not be advertised");
        assert_eq!(budget.ops_remaining, 10, "generic budget untouched");
        assert_eq!(budget.dex_ops_remaining, Some(5), "DEX budget untouched");
    }

    #[test]
    #[should_panic(expected = "every peer_id must have an entry in adverts_by_peer")]
    fn test_flush_adverts_panics_on_missing_peer() {
        let queue = TransactionQueue::new(test_queue_config());
        let peer = make_peer_id(10);
        let peer_ids = vec![peer];
        let adverts: HashMap<henyey_overlay::PeerId, PeerTxAdverts> = HashMap::new();
        let mut budget = BroadcastBudget {
            ops_remaining: 10,
            dex_ops_remaining: None,
        };
        let _ = collect_adverts_for_peers(&queue, &mut budget, &peer_ids, &adverts);
    }
}
