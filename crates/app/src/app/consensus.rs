use super::*;

impl App {
    /// Try to trigger consensus for the next ledger (validators only).
    ///
    /// Matches stellar-core's triggerNextLedger() gate: only propose when
    /// the node is tracking the network AND the ledger manager is synced
    /// (LCL == tracking slot). Without this, a node that is behind would
    /// propose stale transaction sets for slots it hasn't closed yet.
    pub(super) async fn try_trigger_consensus(&self) {
        if self.config.node.manual_close {
            return;
        }

        let tracking_slot = self.herder.tracking_slot();

        // Check if we should start a new round
        if self.herder.is_tracking() {
            let current_ledger = *self.current_ledger.read().await;

            // Don't propose if our LCL is not synced with the tracking slot.
            // stellar-core's isSynced() checks: lastClosedLedger + 1 == trackingConsensusLedgerIndex.
            // We are synced when our LCL is exactly one behind the tracking slot (the next slot
            // to reach consensus on). If we're more than one behind, skip.
            if (current_ledger as u64) + 1 < tracking_slot {
                tracing::debug!(
                    current_ledger,
                    tracking_slot,
                    "Skipping consensus trigger: not synced (LCL behind tracking slot)"
                );
                return;
            }

            let next_slot = current_ledger + 1;
            tracing::debug!(next_slot, "Checking if we should trigger consensus");

            // Record local close time for drift tracking before triggering consensus.
            // This captures when we started the consensus round.
            let local_time = self.clock.system_now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            if let Ok(mut tracker) = self.drift_tracker.lock() {
                tracker.record_local_close_time(next_slot, local_time);
            }

            // In a full implementation, we would:
            // 1. Check if enough time has passed since last close
            // 2. Build a transaction set from queued transactions
            // 3. Create a StellarValue with the tx set hash and close time
            // 4. Start SCP nomination with that value

            // For now, trigger the herder
            self.consensus_trigger_attempts.fetch_add(1, Ordering::Relaxed);
            if let Err(e) = self.herder.trigger_next_ledger(next_slot).await {
                self.consensus_trigger_failures.fetch_add(1, Ordering::Relaxed);
                tracing::error!(error = %e, slot = next_slot, "Failed to trigger ledger");
            } else {
                self.consensus_trigger_successes.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// Perform out-of-sync recovery matching stellar-core's outOfSyncRecovery().
    ///
    /// This broadcasts recent SCP messages to peers and requests SCP state,
    /// giving the network a chance to provide the missing data before we
    /// fall back to catchup.
    ///
    /// Tracks consecutive recovery attempts without ledger progress. After
    /// `RECOVERY_ESCALATION_SCP_REQUEST` attempts (~6s at 1s interval) we
    /// actively request SCP state from peers even with a small gap. After
    /// `RECOVERY_ESCALATION_CATCHUP` attempts (~6s at 1s interval) we trigger
    /// a full catchup.
    pub(super) async fn out_of_sync_recovery(&self, current_ledger: u32) {
        let latest_externalized = self.herder.latest_externalized_slot().unwrap_or(0);
        let last_processed = *self.last_processed_slot.read().await;
        let pending_tx_sets = self.herder.get_pending_tx_sets();
        let buffer_count = self.syncing_ledgers.read().await.len();
        let gap = latest_externalized.saturating_sub(current_ledger as u64);

        // Track consecutive recovery attempts without progress.
        let baseline = self.recovery_baseline_ledger.load(Ordering::SeqCst);
        if current_ledger as u64 > baseline {
            // Progress!  Reset the counter.
            self.recovery_baseline_ledger
                .store(current_ledger as u64, Ordering::SeqCst);
            self.recovery_attempts_without_progress
                .store(0, Ordering::SeqCst);
        }
        let attempts = self
            .recovery_attempts_without_progress
            .fetch_add(1, Ordering::SeqCst);

        tracing::info!(
            current_ledger,
            latest_externalized,
            last_processed,
            pending_tx_sets = pending_tx_sets.len(),
            buffer_count,
            gap,
            attempts,
            "Performing out-of-sync recovery"
        );

        // Clean up stale pending tx_set requests for slots we've already closed.
        // After rapid close, stale EXTERNALIZE messages from previous SCP state
        // requests create pending tx_set entries for old slots whose tx_sets are
        // evicted from peers' caches. These requests can never be fulfilled and
        // cause infinite timeout → DontHave → recovery loops.
        let stale_cleared = self
            .herder
            .cleanup_old_pending_tx_sets(current_ledger as u64 + 1);
        if stale_cleared > 0 {
            tracing::debug!(
                stale_cleared,
                current_ledger,
                "Cleared stale pending tx_set requests for already-closed slots"
            );
            // Also clear the local tx_set tracking state for these stale requests
            self.tx_set_dont_have.write().await.clear();
            self.tx_set_last_request.write().await.clear();
            self.tx_set_exhausted_warned.write().await.clear();
            self.tx_set_all_peers_exhausted
                .store(false, Ordering::SeqCst);
        }

        // --- Escalation: after many failed attempts, force catchup ---
        if attempts >= RECOVERY_ESCALATION_CATCHUP {
            // Fatal-failure guard (spec §13.3): block further catchup after a
            // verification/integrity failure.
            if self.catchup_fatal_failure.load(Ordering::SeqCst) {
                tracing::warn!(
                    "Recovery escalation blocked: previous fatal catchup failure — \
                     manual intervention required"
                );
                return;
            }

            tracing::warn!(
                current_ledger,
                latest_externalized,
                gap,
                attempts,
                "Recovery stalled for too long — forcing catchup"
            );
            // Clear all stale state
            {
                let mut buffer = self.syncing_ledgers.write().await;
                buffer.clear();
            }
            self.herder.clear_pending_tx_sets();
            self.tx_set_dont_have.write().await.clear();
            self.tx_set_last_request.write().await.clear();
            self.tx_set_exhausted_warned.write().await.clear();
            self.tx_set_all_peers_exhausted
                .store(false, Ordering::SeqCst);
            // Reset the counter so we don't immediately re-escalate after catchup
            self.recovery_attempts_without_progress
                .store(0, Ordering::SeqCst);

            // Guard against concurrent catchup
            if !self.catchup_in_progress.swap(true, Ordering::SeqCst) {
                self.set_state(AppState::CatchingUp).await;
                self.herder
                    .set_state(henyey_herder::HerderState::Syncing);

                let catchup_message_handle =
                    self.start_catchup_message_caching_from_self().await;

                self.set_phase(14); // 14 = catchup_running
                let catchup_result = self.catchup(CatchupTarget::Current).await;
                self.set_phase(5); // 5 = back in consensus_tick

                if let Some(handle) = catchup_message_handle {
                    handle.abort();
                }
                self.catchup_in_progress.store(false, Ordering::SeqCst);

                self.handle_catchup_result(catchup_result, true, "RecoveryEscalation")
                    .await;
            }
            return;
        }

        // When the node is essentially caught up (small or zero gap), the
        // recovery strategy depends on whether we have the EXTERNALIZE for the
        // very next slot (current_ledger + 1).
        //
        // Case 1: Next slot's EXTERNALIZE IS available — we're just waiting for
        //   its tx_set. Don't request SCP state because peers would respond with
        //   EXTERNALIZE for older slots whose tx_sets are already evicted from
        //   their caches (~60-72s window), creating unfulfillable requests.
        //
        // Case 2: Next slot's EXTERNALIZE is MISSING — we need to ask peers for
        //   it immediately. Every 5 seconds the gap grows by 1 slot, and peers
        //   only cache ~12 slots (~60s). Waiting 60s to escalate guarantees the
        //   EXTERNALIZE is evicted from peers by the time we ask.
        if gap <= TX_SET_REQUEST_WINDOW {
            // Check if the next slot's EXTERNALIZE is missing
            let next_slot = current_ledger as u64 + 1;
            let next_slot_missing = latest_externalized > next_slot
                && self.herder.get_externalized(next_slot).is_none();

            // Also clear syncing_ledgers entries with no tx_set — these are
            // unfulfillable entries from stale EXTERNALIZE and will block
            // try_start_ledger_close().
            {
                let mut buffer = self.syncing_ledgers.write().await;
                let pre_count = buffer.len();
                buffer.retain(|seq, info| {
                    *seq > current_ledger && info.tx_set.is_some()
                });
                let removed = pre_count - buffer.len();
                if removed > 0 {
                    tracing::info!(
                        removed,
                        remaining = buffer.len(),
                        current_ledger,
                        "Cleared unfulfillable syncing_ledgers entries (essentially caught up)"
                    );
                }
            }
            // Clear any remaining pending tx_sets from the herder
            self.herder.clear_pending_tx_sets();

            if next_slot_missing {
                // The next slot's EXTERNALIZE was missed (network blip, peer
                // disconnection, etc.). Request SCP state immediately — peers
                // should still have it cached if we act quickly.
                tracing::warn!(
                    current_ledger,
                    latest_externalized,
                    gap,
                    attempts,
                    "Next slot EXTERNALIZE missing — requesting SCP state immediately"
                );
                // Fall through to the SCP state request below
            } else if attempts < RECOVERY_ESCALATION_SCP_REQUEST {
                tracing::info!(
                    current_ledger,
                    latest_externalized,
                    gap,
                    attempts,
                    "Essentially caught up — waiting for fresh EXTERNALIZE"
                );
                return;
            } else {
                // Escalation: request SCP state despite small gap
                tracing::warn!(
                    current_ledger,
                    latest_externalized,
                    gap,
                    attempts,
                    "Essentially caught up but no progress — requesting SCP state from peers"
                );
                // Fall through to the SCP state request below
            }
        }

        // Detect gaps in externalized slots to help diagnose sync issues.
        // If the very next slot (current_ledger+1) is missing, peers will never
        // have it (they only cache ~12 recent slots / ~60-72s). The only recovery
        // path is catchup — requesting SCP state from peers is futile.
        let next_slot = current_ledger as u64 + 1;
        if latest_externalized > next_slot {
            let missing_slots = self.herder.find_missing_slots_in_range(next_slot, latest_externalized);
            if !missing_slots.is_empty() {
                let missing_count = missing_slots.len();
                let first_missing = missing_slots.first().copied().unwrap_or(0);
                let last_missing = missing_slots.last().copied().unwrap_or(0);
                tracing::warn!(
                    current_ledger,
                    latest_externalized,
                    missing_count,
                    first_missing,
                    last_missing,
                    missing_slots = ?if missing_count <= 10 { missing_slots.clone() } else { vec![] },
                    "Detected gap in externalized slots - missing EXTERNALIZE messages"
                );

                // If the very next slot is missing, we can NEVER close it via the
                // normal path (try_start_ledger_close requires syncing_ledgers[N+1]).
                // Peers have evicted this slot's data from their caches.  Trigger
                // catchup immediately to skip past the gap instead of spinning in
                // recovery forever.
                if missing_slots.contains(&next_slot) {
                    // Check if the target checkpoint hasn't been published yet.
                    // maybe_start_externalized_catchup targets latest_ext - 12;
                    // if the checkpoint containing that target hasn't been published,
                    // archive-based catchup would block on 404s.  Fall through to
                    // the SCP state request so peers can provide the missing data.
                    let catchup_target = latest_externalized
                        .saturating_sub(TX_SET_REQUEST_WINDOW) as u32;
                    let target_checkpoint =
                        henyey_history::checkpoint::checkpoint_containing(catchup_target);
                    if target_checkpoint as u64 > latest_externalized {
                        tracing::info!(
                            current_ledger,
                            next_slot,
                            catchup_target,
                            target_checkpoint,
                            latest_externalized,
                            "Next slot missing but target checkpoint not yet published — \
                             requesting SCP state from peers instead of archive catchup"
                        );
                        // Fall through to the SCP state request below instead of
                        // triggering archive catchup that would fail.
                    } else {
                        tracing::warn!(
                            current_ledger,
                            next_slot,
                            latest_externalized,
                            gap,
                            "Next slot permanently missing — triggering catchup to skip gap"
                        );
                        // Clear stale syncing_ledgers entries that will never be closeable
                        {
                            let mut buffer = self.syncing_ledgers.write().await;
                            buffer.retain(|seq, info| {
                                *seq > current_ledger && info.tx_set.is_some()
                            });
                        }
                        self.maybe_start_externalized_catchup(latest_externalized)
                            .await;
                        return;
                    }
                }
            } else {
                // No gaps in externalized, but we can't apply - missing tx_sets.
                // Peers won't have tx_sets for old slots (they cache ~60-72s).
                // Check if any syncing_ledgers entries actually have tx_sets;
                // if none do, catchup is the only recovery path.
                let buffer = self.syncing_ledgers.read().await;
                let total = buffer.range((current_ledger + 1)..).count();
                let with_tx_set = buffer
                    .range((current_ledger + 1)..)
                    .filter(|(_, info)| info.tx_set.is_some())
                    .count();
                drop(buffer);

                tracing::warn!(
                    current_ledger,
                    latest_externalized,
                    total_buffered = total,
                    with_tx_set,
                    without_tx_set = total - with_tx_set,
                    "All slots externalized but cannot apply - missing tx_sets"
                );

                // If no buffered entries have tx_sets, requesting SCP state is
                // futile — force immediate catchup escalation.
                if with_tx_set == 0 && total > 0 {
                    tracing::warn!(
                        current_ledger,
                        latest_externalized,
                        "No tx_sets available for any buffered slot — forcing catchup"
                    );
                    self.recovery_attempts_without_progress
                        .store(RECOVERY_ESCALATION_CATCHUP, Ordering::SeqCst);
                    // Recurse with the escalated counter — this will trigger catchup
                    // on the next recovery tick (within 5s).
                    return;
                }
            }
        }

        // Get recent SCP envelopes to broadcast
        let from_slot = current_ledger.saturating_sub(5) as u64;
        tracing::debug!(from_slot, "Getting SCP state for recovery");
        let (envelopes, _quorum_set) = self.herder.get_scp_state(from_slot);
        tracing::debug!(
            envelope_count = envelopes.len(),
            "Got SCP state for recovery"
        );

        tracing::debug!("Acquiring overlay for recovery");
        let Some(overlay) = self.overlay().await else {
            tracing::debug!("No overlay available for out-of-sync recovery");
            return;
        };
        tracing::debug!("Acquired overlay for recovery");

        let peer_count = overlay.peer_count();
        if peer_count == 0 {
            tracing::debug!("No peers connected for out-of-sync recovery");
            return;
        }

        // Broadcast recent SCP envelopes + request SCP state from peers.
        // Spawn as a background task so the main event loop is not blocked.
        // The overlay and envelopes are cheaply clonable (Arc / Vec).
        let overlay_clone = Arc::clone(&overlay);
        let envelope_count = envelopes.len();
        tokio::spawn(async move {
            // Broadcast all envelopes concurrently
            let broadcast_futures: Vec<_> = envelopes
                .into_iter()
                .map(|envelope| {
                    let overlay = Arc::clone(&overlay_clone);
                    async move {
                        let msg = StellarMessage::ScpMessage(envelope);
                        overlay.broadcast(msg).await.is_ok()
                    }
                })
                .collect();

            let results = futures::future::join_all(broadcast_futures).await;
            let broadcast_count = results.into_iter().filter(|ok| *ok).count();

            if broadcast_count > 0 {
                tracing::info!(
                    broadcast_count,
                    "Broadcast SCP envelopes during out-of-sync recovery"
                );
            }

            // Request SCP state from peers
            let ledger_seq = current_ledger;
            tracing::info!(ledger_seq, "Requesting SCP state from peers (recovery task)");
            match overlay_clone.request_scp_state(ledger_seq).await {
                Ok(count) => {
                    tracing::info!(
                        ledger_seq,
                        peers_requested = count,
                        "Requested SCP state during out-of-sync recovery"
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        ledger_seq,
                        error = %e,
                        "Failed to request SCP state during out-of-sync recovery"
                    );
                }
            }
        });

        tracing::debug!(
            envelope_count,
            "Spawned background task for recovery broadcast"
        );
    }

    /// Send SCP state to a peer in response to GetScpState.
    pub(super) async fn send_scp_state(&self, peer_id: &henyey_overlay::PeerId, from_ledger: u32) {
        let from_slot = from_ledger as u64;
        let (envelopes, quorum_set) = self.herder.get_scp_state(from_slot);

        let Some(overlay) = self.overlay().await else {
            return;
        };

        // Send our quorum set first if we have one configured
        if let Some(qs) = quorum_set {
            let msg = StellarMessage::ScpQuorumset(qs);
            if let Err(e) = overlay.try_send_to(peer_id, msg) {
                tracing::debug!(peer = %peer_id, error = %e, "Failed to send quorum set");
            }
        }

        // Send SCP envelopes for recent slots
        for envelope in envelopes {
            let msg = StellarMessage::ScpMessage(envelope);
            if let Err(e) = overlay.try_send_to(peer_id, msg) {
                tracing::debug!(peer = %peer_id, error = %e, "Failed to send SCP envelope");
                break; // Stop if we can't send (channel full)
            }
        }

        tracing::debug!(peer = %peer_id, from_ledger, "Sent SCP state response");
    }

    /// Respond to a GetScpQuorumset message.
    pub(super) async fn send_quorum_set(
        &self,
        peer_id: &henyey_overlay::PeerId,
        requested_hash: stellar_xdr::curr::Uint256,
    ) {
        let Some(overlay) = self.overlay().await else {
            return;
        };

        let req = requested_hash.0;
        if let Some(qs) = self.herder.get_quorum_set_by_hash(&req) {
            if let Err(e) = overlay.try_send_to(peer_id, StellarMessage::ScpQuorumset(qs)) {
                tracing::debug!(peer = %peer_id, error = %e, "Failed to send quorum set");
            }
        } else {
            let msg = StellarMessage::DontHave(stellar_xdr::curr::DontHave {
                type_: stellar_xdr::curr::MessageType::ScpQuorumset,
                req_hash: requested_hash,
            });
            if let Err(e) = overlay.try_send_to(peer_id, msg) {
                tracing::debug!(peer = %peer_id, error = %e, "Failed to send DontHave for quorum set");
            }
        }
    }

    /// Store a quorum set received from a peer.
    pub(super) async fn handle_quorum_set(
        &self,
        _peer_id: &henyey_overlay::PeerId,
        quorum_set: stellar_xdr::curr::ScpQuorumSet,
    ) {
        let hash = henyey_scp::hash_quorum_set(&quorum_set);

        // Get the node_ids that were waiting for this quorum set
        let node_ids = self.herder.get_pending_quorum_set_node_ids(&hash);

        if let Err(err) = self.db.store_scp_quorum_set(
            &hash,
            self.ledger_manager.current_ledger_seq(),
            &quorum_set,
        ) {
            tracing::warn!(error = %err, "Failed to store quorum set");
        }

        // Store for all node_ids that use this quorum set
        if node_ids.is_empty() {
            tracing::debug!(%hash, "Received quorum set with no pending requests");
        } else {
            for node_id in &node_ids {
                tracing::debug!(%hash, node_id = ?node_id, "Storing quorum set for node");
                self.herder.store_quorum_set(node_id, quorum_set.clone());
            }
        }

        self.herder.clear_quorum_set_request(&hash);
    }

    pub(super) fn scp_quorum_set_hash(statement: &stellar_xdr::curr::ScpStatement) -> Option<Hash> {
        match &statement.pledges {
            stellar_xdr::curr::ScpStatementPledges::Nominate(nom) => {
                Some(nom.quorum_set_hash.clone())
            }
            stellar_xdr::curr::ScpStatementPledges::Prepare(prep) => {
                Some(prep.quorum_set_hash.clone())
            }
            stellar_xdr::curr::ScpStatementPledges::Confirm(conf) => {
                Some(conf.quorum_set_hash.clone())
            }
            stellar_xdr::curr::ScpStatementPledges::Externalize(ext) => {
                Some(ext.commit_quorum_set_hash.clone())
            }
        }
    }

    pub(super) fn tx_hash(&self, tx_env: &stellar_xdr::curr::TransactionEnvelope) -> Option<Hash256> {
        Hash256::hash_xdr(tx_env).ok()
    }

    pub(super) fn build_scp_history_entry(&self, ledger_seq: u32) -> Option<ScpHistoryEntry> {
        let envelopes = self.herder.get_scp_envelopes(ledger_seq as u64);
        if envelopes.is_empty() {
            return None;
        }

        let mut qset_hashes = HashSet::new();
        for envelope in &envelopes {
            if let Some(hash) = Self::scp_quorum_set_hash(&envelope.statement) {
                qset_hashes.insert(Hash256::from_bytes(hash.0));
            }
        }

        let mut hashes = qset_hashes.into_iter().collect::<Vec<_>>();
        hashes.sort_by_key(|a| a.to_hex());

        let mut qsets = Vec::new();
        for hash in hashes {
            match self.herder.get_quorum_set_by_hash(hash.as_bytes()) {
                Some(qset) => qsets.push(qset),
                None => {
                    tracing::debug!(hash = %hash.to_hex(), "Missing quorum set for SCP history entry");
                    return None;
                }
            }
        }

        let quorum_sets = match qsets.try_into() {
            Ok(qsets) => qsets,
            Err(_) => {
                tracing::warn!(ledger_seq, "Too many quorum sets for SCP history entry");
                return None;
            }
        };
        let messages = match envelopes.try_into() {
            Ok(messages) => messages,
            Err(_) => {
                tracing::warn!(ledger_seq, "Too many SCP envelopes for SCP history entry");
                return None;
            }
        };

        Some(ScpHistoryEntry::V0(ScpHistoryEntryV0 {
            quorum_sets,
            ledger_messages: LedgerScpMessages {
                ledger_seq,
                messages,
            },
        }))
    }

    pub(super) async fn check_scp_timeouts(&self) {
        if !self.is_validator {
            return;
        }
        if !self.herder.state().can_receive_scp() {
            return;
        }
        let current_ledger = *self.current_ledger.read().await as u64;
        let slot = self.herder.tracking_slot().max(current_ledger + 1);
        let now = self.clock.now();
        let mut timeouts = self.scp_timeouts.write().await;
        if timeouts.slot != slot {
            timeouts.slot = slot;
            timeouts.next_nomination = None;
            timeouts.next_ballot = None;
        }

        if let Some(next) = timeouts.next_nomination {
            if now >= next {
                self.nomination_timeout_fires.fetch_add(1, Ordering::Relaxed);
                self.herder.handle_nomination_timeout(slot);
                timeouts.next_nomination = None;
            }
        }
        if timeouts.next_nomination.is_none() {
            if let Some(timeout) = self.herder.get_nomination_timeout(slot) {
                timeouts.next_nomination = Some(now + timeout);
            }
        }

        if let Some(next) = timeouts.next_ballot {
            if now >= next {
                self.ballot_timeout_fires.fetch_add(1, Ordering::Relaxed);
                self.herder.handle_ballot_timeout(slot);
                timeouts.next_ballot = None;
            }
        }
        if timeouts.next_ballot.is_none() {
            if let Some(timeout) = self.herder.get_ballot_timeout(slot) {
                timeouts.next_ballot = Some(now + timeout);
            }
        }
    }
}
