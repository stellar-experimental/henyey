//! SCP consensus: triggering rounds, out-of-sync recovery, and quorum set management.

use super::*;

/// Why the node cannot apply the next buffered slot even though the herder
/// has an EXTERNALIZE for every slot in `[current_ledger+1, latest_externalized]`.
///
/// Extracted from [`App::analyze_externalized_gaps`] so the classification
/// logic can be unit tested without spinning up a full [`App`] (see
/// issue #1759: the historical "missing tx_sets" warning was emitted even for
/// the sequence-gap case, hiding the real fault during diagnostics).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CannotApplyReason {
    /// At least one buffered slot in `[current_ledger+1, latest_externalized]`
    /// is missing its tx_set (`without_tx_set > 0`). Either the fetch is
    /// in-flight, or peers have evicted the tx_set from their caches.
    MissingTxSets,
    /// Every buffered slot has its tx_set (`without_tx_set == 0`), but the
    /// buffer does not start at `current_ledger + 1` — one or more ledgers
    /// between `current_ledger + 1` and `first_buffered - 1` are missing
    /// from the buffer entirely. This is the condition `catchup_impl`
    /// surfaces as "Buffered gap detected".
    BufferedSequenceGap,
}

/// Pure decision helper for which "cannot apply" diagnostic to emit.
///
/// * `without_tx_set` — count of buffered slots (>= `current_ledger + 1`)
///   whose `LedgerCloseInfo.tx_set` is `None`.
/// * `sequence_gap` — `first_buffered - (current_ledger + 1)`, saturating
///   to zero when the buffer is empty or contiguous with `current_ledger`.
///
/// Returns [`CannotApplyReason::BufferedSequenceGap`] only when tx_sets are
/// fully present *and* a sequence gap is observed. All other shapes
/// (tx_sets missing, empty buffer, etc.) fall back to the legacy
/// "missing tx_sets" classification.
#[inline]
pub(super) fn classify_cannot_apply_reason(
    without_tx_set: usize,
    sequence_gap: u32,
) -> CannotApplyReason {
    if without_tx_set == 0 && sequence_gap > 0 {
        CannotApplyReason::BufferedSequenceGap
    } else {
        CannotApplyReason::MissingTxSets
    }
}

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
            let current_ledger = self.current_ledger_seq();

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
            let local_time = self
                .clock
                .system_now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock before UNIX epoch")
                .as_secs();
            if let Ok(mut tracker) = self.drift_tracker.lock() {
                tracker.record_local_close_time(next_slot, local_time);
            }

            // In a full implementation, we would:
            // 1. Check if enough time has passed since last close
            // 2. Build a transaction set from queued transactions
            // 3. Create a StellarValue with the tx set hash and close time
            // 4. Start SCP nomination with that value

            // For now, trigger the herder
            self.consensus_trigger_attempts
                .fetch_add(1, Ordering::Relaxed);
            if let Err(e) = self.herder.trigger_next_ledger(next_slot).await {
                self.consensus_trigger_failures
                    .fetch_add(1, Ordering::Relaxed);
                tracing::error!(error = %e, slot = next_slot, "Failed to trigger ledger");
            } else {
                self.consensus_trigger_successes
                    .fetch_add(1, Ordering::Relaxed);
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
    pub(super) async fn out_of_sync_recovery(&self, current_ledger: u32) -> Option<PendingCatchup> {
        use super::phase::*;

        let latest_externalized = self.herder.latest_externalized_slot().unwrap_or(0);
        let last_processed = *self.last_processed_slot.read().await;
        let pending_tx_sets = self.herder.get_pending_tx_sets();
        self.set_phase_sub(PHASE_13_6_OUT_OF_SYNC_BUFFER_COUNT_READ);
        let buffer_count = tracked_lock::tracked_read("syncing_ledgers", &self.syncing_ledgers)
            .await
            .len();
        let gap = latest_externalized.saturating_sub(current_ledger as u64);

        // Track consecutive recovery attempts without progress.
        let baseline = self.recovery_baseline_ledger.load(Ordering::SeqCst);
        if current_ledger as u64 > baseline {
            // Progress!  Reset the counter.
            self.recovery_baseline_ledger
                .store(current_ledger as u64, Ordering::SeqCst);
            self.recovery_attempts_without_progress
                .store(0, Ordering::SeqCst);
            // Also clear any archive-behind backoff — the node is advancing
            // again, so the next stall (if any) should query fresh.
            let mut guard = self.archive_behind_until.write().await;
            *guard = None;
        }
        let attempts = self
            .recovery_attempts_without_progress
            .fetch_add(1, Ordering::SeqCst);

        tracing::debug!(
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
            self.reset_tx_set_tracking().await;
        }

        // --- Escalation: after many failed attempts, force catchup ---
        if attempts >= RECOVERY_ESCALATION_CATCHUP {
            self.set_phase_sub(PHASE_13_10_TRIGGER_RECOVERY_CATCHUP);
            return self
                .trigger_recovery_catchup(current_ledger, latest_externalized, gap, attempts)
                .await;
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

            // Clear syncing_ledgers entries for already-closed slots.
            // Do NOT clear entries without tx_sets — their tx_set fetches
            // may still be in-flight. Clearing pending_tx_sets here was the
            // root cause of the post-catchup convergence failure: the
            // EXTERNALIZE envelope would arrive and register a tx_set fetch,
            // but recovery would immediately clear the request, so the
            // tx_set was never fetched and the slot could never close.
            {
                self.set_phase_sub(PHASE_13_7_OUT_OF_SYNC_CLEAR_SYNCING_WRITE);
                let mut buffer =
                    tracked_lock::tracked_write("syncing_ledgers", &self.syncing_ledgers).await;
                let pre_count = buffer.len();
                buffer.retain(|seq, _| *seq > current_ledger);
                let removed = pre_count - buffer.len();
                if removed > 0 {
                    tracing::info!(
                        removed,
                        remaining = buffer.len(),
                        current_ledger,
                        "Cleared stale syncing_ledgers entries for closed slots"
                    );
                }
            }

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
                // Fast-track: If we're receiving SCP messages but none result in
                // externalization, the tx_sets for our slots are gone from peers'
                // caches (~60s window). Waiting the full 6 escalation cycles is
                // futile — skip straight to catchup after 1 recovery cycle.
                // This is critical for captive-core following a standalone
                // validator in quickstart/local mode where the validator closes
                // ledgers every second.
                let scp_total = self.scp_messages_received.load(Ordering::Relaxed);
                if attempts >= 1 && scp_total > 0 && gap == 0 {
                    tracing::warn!(
                        current_ledger,
                        latest_externalized,
                        gap,
                        attempts,
                        scp_total,
                        "Receiving SCP messages but no externalization — \
                         tx_sets evicted from peers, fast-tracking catchup"
                    );
                    // Jump directly to catchup instead of waiting 6 cycles
                    self.set_phase_sub(PHASE_13_10_TRIGGER_RECOVERY_CATCHUP);
                    return self
                        .trigger_recovery_catchup(
                            current_ledger,
                            latest_externalized,
                            gap,
                            attempts,
                        )
                        .await;
                }

                tracing::debug!(
                    current_ledger,
                    latest_externalized,
                    gap,
                    attempts,
                    "Essentially caught up — waiting for fresh EXTERNALIZE"
                );
                return None;
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

        // Detect gaps in externalized slots and potentially trigger catchup.
        self.set_phase_sub(PHASE_13_8_OUT_OF_SYNC_ANALYZE_GAPS);
        if let Some(result) = self
            .analyze_externalized_gaps(current_ledger, latest_externalized, gap, attempts)
            .await
        {
            return result;
        }

        // Broadcast recent SCP envelopes and request SCP state from peers.
        self.set_phase_sub(PHASE_13_9_BROADCAST_RECOVERY);
        self.broadcast_recovery_scp_state(current_ledger).await;
        None
    }

    /// Analyze gaps in externalized slots and potentially trigger catchup.
    ///
    /// Returns `Some(Some(PendingCatchup))` if catchup was triggered,
    /// `Some(None)` if the caller should wait (no broadcast needed),
    /// or `None` if the caller should fall through to SCP broadcast.
    async fn analyze_externalized_gaps(
        &self,
        current_ledger: u32,
        latest_externalized: u64,
        gap: u64,
        attempts: u64,
    ) -> Option<Option<PendingCatchup>> {
        let next_slot = current_ledger as u64 + 1;
        if latest_externalized <= next_slot {
            return None; // No gap to analyze — fall through to broadcast
        }

        let missing_slots = self
            .herder
            .find_missing_slots_in_range(next_slot, latest_externalized);

        if !missing_slots.is_empty() {
            let next_is_fetching = missing_slots.contains(&next_slot)
                && self.herder.has_fetching_envelopes_for_slot(next_slot);

            let missing_count = missing_slots.len();
            let first_missing = missing_slots.first().copied().unwrap_or(0);
            let last_missing = missing_slots.last().copied().unwrap_or(0);

            if next_is_fetching {
                tracing::info!(
                    current_ledger,
                    next_slot,
                    latest_externalized,
                    gap,
                    attempts,
                    "Next slot EXTERNALIZE in-flight (waiting for tx_set fetch) — waiting"
                );
                // Fall through to SCP state request for other missing slots.
            } else {
                tracing::warn!(
                    current_ledger,
                    latest_externalized,
                    missing_count,
                    first_missing,
                    last_missing,
                    missing_slots = ?if missing_count <= 10 { missing_slots.clone() } else { vec![] },
                    "Detected gap in externalized slots - missing EXTERNALIZE messages"
                );
            }

            // If the very next slot is truly missing (not in-flight),
            // we may need catchup to skip past the gap.
            if missing_slots.contains(&next_slot) && !next_is_fetching {
                let catchup_target =
                    latest_externalized.saturating_sub(TX_SET_REQUEST_WINDOW) as u32;
                let target_checkpoint =
                    henyey_history::checkpoint::checkpoint_containing(catchup_target);
                if target_checkpoint as u64 > latest_externalized {
                    if attempts <= 2 {
                        tracing::info!(
                            current_ledger,
                            next_slot,
                            catchup_target,
                            target_checkpoint,
                            latest_externalized,
                            attempts,
                            "Next slot missing, checkpoint not published — \
                             requesting SCP state from peers"
                        );
                        // Fall through to SCP broadcast.
                    } else {
                        tracing::info!(
                            current_ledger,
                            next_slot,
                            target_checkpoint,
                            latest_externalized,
                            attempts,
                            "Waiting for checkpoint {} to be published \
                             (next slot EXTERNALIZE not available from peers)",
                            target_checkpoint,
                        );
                        return Some(None); // Wait — no broadcast needed.
                    }
                } else if target_checkpoint as u64 <= current_ledger as u64 {
                    tracing::info!(
                        current_ledger,
                        next_slot,
                        target_checkpoint,
                        latest_externalized,
                        gap,
                        attempts,
                        "Already at target checkpoint — waiting for escalation"
                    );
                    // Fall through to SCP broadcast.
                } else {
                    tracing::warn!(
                        current_ledger,
                        next_slot,
                        latest_externalized,
                        gap,
                        "Next slot permanently missing — triggering catchup to skip gap"
                    );
                    {
                        let mut buffer =
                            tracked_lock::tracked_write("syncing_ledgers", &self.syncing_ledgers)
                                .await;
                        buffer.retain(|seq, _| *seq > current_ledger);
                    }
                    return Some(
                        self.trigger_recovery_catchup(
                            current_ledger,
                            latest_externalized,
                            gap,
                            attempts,
                        )
                        .await,
                    );
                }
            }
        } else {
            // No gaps in externalized (herder has EXTERNALIZE for every slot),
            // but we still can't apply. Two distinct causes are possible:
            //
            //   1. The buffer's first slot sits at `current_ledger + 1` (contiguous)
            //      and one or more slots lack a tx_set. This is the true
            //      "missing tx_sets" case.
            //
            //   2. The buffer starts at `current_ledger + 2` or later (a sequence
            //      gap). Every buffered slot may already have its tx_set
            //      (`without_tx_set == 0`), but the missing slot(s) between
            //      `current_ledger + 1` and `first_buffered - 1` block the apply.
            //      This path is driven by `catchup_impl::maybe_start_buffered_catchup`
            //      which logs "Buffered gap detected".
            //
            // Historically this branch printed "missing tx_sets" in both cases,
            // which made diagnostics misleading (see issue #1759). Emit a
            // distinct `buffered_sequence_gap` warning when the true cause is a
            // sequence gap, so operators can triage the right subsystem.
            let (total, with_tx_set, first_buffered, last_buffered) = {
                let buffer =
                    tracked_lock::tracked_read("syncing_ledgers", &self.syncing_ledgers).await;
                let total = buffer.range((current_ledger + 1)..).count();
                let with_tx_set = buffer
                    .range((current_ledger + 1)..)
                    .filter(|(_, info)| info.tx_set.is_some())
                    .count();
                let first = buffer
                    .range((current_ledger + 1)..)
                    .next()
                    .map(|(seq, _)| *seq);
                let last = buffer
                    .range((current_ledger + 1)..)
                    .next_back()
                    .map(|(seq, _)| *seq);
                (total, with_tx_set, first, last)
            };
            let without_tx_set = total - with_tx_set;
            let required_first = current_ledger + 1;
            let sequence_gap = first_buffered
                .map(|f| f.saturating_sub(required_first))
                .unwrap_or(0);

            match classify_cannot_apply_reason(without_tx_set, sequence_gap) {
                CannotApplyReason::BufferedSequenceGap => {
                    // tx_sets are all present — the real blocker is a gap
                    // in buffered ledger sequences.
                    tracing::warn!(
                        current_ledger,
                        latest_externalized,
                        total_buffered = total,
                        first_buffered,
                        last_buffered,
                        required_first,
                        sequence_gap,
                        "All slots externalized but cannot apply - buffered sequence gap \
                         (tx_sets present, but missing ledgers between current_ledger+1 \
                         and first_buffered)"
                    );
                }
                CannotApplyReason::MissingTxSets => {
                    // At least one buffered slot is missing its tx_set, so the
                    // original diagnostic is accurate.
                    tracing::warn!(
                        current_ledger,
                        latest_externalized,
                        total_buffered = total,
                        with_tx_set,
                        without_tx_set,
                        first_buffered,
                        last_buffered,
                        required_first,
                        sequence_gap,
                        "All slots externalized but cannot apply - missing tx_sets"
                    );
                }
            }

            if with_tx_set == 0 && total > 0 {
                tracing::warn!(
                    current_ledger,
                    latest_externalized,
                    "No tx_sets available for any buffered slot — forcing catchup"
                );
                self.recovery_attempts_without_progress
                    .store(RECOVERY_ESCALATION_CATCHUP, Ordering::SeqCst);
                return Some(None); // Wait for next tick to trigger escalation.
            }
        }

        None // Fall through to SCP broadcast.
    }

    /// Broadcast recent SCP envelopes and request SCP state from peers.
    ///
    /// Spawns a background task to avoid blocking the event loop.
    async fn broadcast_recovery_scp_state(&self, current_ledger: u32) {
        let from_slot = current_ledger.saturating_sub(5) as u64;
        tracing::debug!(from_slot, "Getting SCP state for recovery");
        let (envelopes, _quorum_set) = self.herder.get_scp_state(from_slot);
        tracing::debug!(
            envelope_count = envelopes.len(),
            "Got SCP state for recovery"
        );

        let Some(overlay) = self.overlay().await else {
            tracing::debug!("No overlay available for out-of-sync recovery");
            return;
        };

        let peer_count = overlay.peer_count();
        if peer_count == 0 {
            tracing::debug!("No peers connected for out-of-sync recovery");
            return;
        }

        let overlay_clone = Arc::clone(&overlay);
        let envelope_count = envelopes.len();
        tokio::spawn(async move {
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

            let ledger_seq = current_ledger;
            tracing::info!(
                ledger_seq,
                "Requesting SCP state from peers (recovery task)"
            );
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

        let req = henyey_common::Hash256::from_bytes(requested_hash.0);
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

        // Get the node_ids that were waiting for this quorum set.
        // Reject unsolicited quorum sets — matching stellar-core's
        // PendingEnvelopes::recvSCPQuorumSet which checks
        // getLastSeenSlotIndex(hash) != 0 before accepting.
        let node_ids = self.herder.get_pending_quorum_set_node_ids(&hash);
        if node_ids.is_empty() {
            tracing::debug!(%hash, "Ignoring unsolicited quorum set (no pending requests)");
            return;
        }

        // Validate sanity before storing — matching stellar-core's
        // PendingEnvelopes::recvSCPQuorumSet which calls
        // isQuorumSetSane(q, false, errString) before accepting.
        if let Err(reason) = henyey_scp::is_quorum_set_sane(&quorum_set, false) {
            tracing::warn!(%hash, %reason, "Rejecting insane quorum set");
            // Notify fetching_envelopes so blocked envelopes are untracked —
            // matching stellar-core's discardSCPEnvelopesWithQSet behavior.
            // recv_quorum_set handles the insane case internally (rejects but
            // cleans up the fetcher tracking for this hash).
            self.herder.recv_quorum_set(hash, quorum_set);
            self.herder.clear_quorum_set_request(&hash);
            return;
        }

        if let Err(err) = self.db.store_scp_quorum_set(
            &hash,
            self.ledger_manager.current_ledger_seq(),
            &quorum_set,
        ) {
            tracing::warn!(error = %err, "Failed to store quorum set");
        }

        for node_id in &node_ids {
            tracing::debug!(%hash, node_id = ?node_id, "Storing quorum set for node");
            self.herder.store_quorum_set(node_id, quorum_set.clone());
        }

        self.herder.clear_quorum_set_request(&hash);
    }

    pub(super) fn build_scp_history_entry(&self, ledger_seq: u32) -> Option<ScpHistoryEntry> {
        let envelopes = self.herder.get_scp_envelopes(ledger_seq as u64);
        if envelopes.is_empty() {
            return None;
        }

        let mut qset_hashes = HashSet::new();
        for envelope in &envelopes {
            let hash = henyey_common::scp_quorum_set_hash(&envelope.statement);
            qset_hashes.insert(Hash256::from_bytes(hash.0));
        }

        let mut hashes = qset_hashes.into_iter().collect::<Vec<_>>();
        hashes.sort_by_key(|a| a.to_hex());

        let mut qsets = Vec::new();
        for hash in hashes {
            match self.herder.get_quorum_set_by_hash(&hash) {
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
        let current_ledger = self.current_ledger_seq() as u64;
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
                self.nomination_timeout_fires
                    .fetch_add(1, Ordering::Relaxed);
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

    /// Trigger a recovery catchup: clear stale state and run catchup to current.
    ///
    /// Used by both the normal escalation path (after RECOVERY_ESCALATION_CATCHUP
    /// attempts) and the fast-track path (when SCP messages arrive but tx_sets
    /// are evicted from peers' caches).
    async fn trigger_recovery_catchup(
        &self,
        current_ledger: u32,
        latest_externalized: u64,
        gap: u64,
        attempts: u64,
    ) -> Option<PendingCatchup> {
        // Fatal-failure guard (spec §13.3): block further catchup after a
        // verification/integrity failure.
        if self.catchup_fatal_failure.load(Ordering::SeqCst) {
            tracing::warn!(
                "Recovery escalation blocked: previous fatal catchup failure — \
                 manual intervention required"
            );
            return None;
        }

        tracing::info!(
            current_ledger,
            latest_externalized,
            gap,
            attempts,
            "Recovery stalled for too long — forcing catchup"
        );

        let next_cp = henyey_history::checkpoint::checkpoint_containing(current_ledger + 1);

        // Archive-behind backoff: if a previous tick already learned the
        // archive is behind `next_cp`, skip the redundant query. The archive
        // publishes checkpoints every ~5 minutes, so re-asking every 10s is
        // guaranteed waste (and creates a "Querying history archives" log
        // storm during a stall). We still run the peer-SCP fallback below.
        self.set_phase_sub(super::phase::PHASE_13_5_BUFFERED_ARCHIVE_BEHIND_READ);
        let backoff_active = {
            let guard = self.archive_behind_until.read().await;
            match *guard {
                Some(deadline) => self.clock.now() < deadline,
                None => false,
            }
        };

        // NOTE (#1755 debug, 2026-04-18): Previously this branch called
        // `archive_checkpoint_cache.clear()` with the stated goal of
        // forcing a fresh archive read. In practice the clear was
        // catastrophic: the immediately-following non-blocking read then
        // returned `None` (cold cache), which logs "archive hasn't
        // published checkpoint yet" and skips the catchup. A background
        // refresh fires and populates the cache ~1s later, but the next
        // recovery tick (10s later) clears the cache again before reading
        // it. Net result: a stuck recovery loop that *never* acts on the
        // archive, even though the archive is reachable and well ahead of
        // our ledger. Reproduced on Quickstart local/rpc shard where the
        // validator sat at ledger=13 with archive already at ledger=719.
        //
        // The cache's own `get_cached()` already spawns a background
        // refresh on BOTH cold and stale reads, so the clear is not
        // needed to trigger a refresh. And accepting a stale-by-TTL
        // value is strictly better than an infinite stall: it just
        // means we may catchup in smaller steps, which is a performance
        // concern, not a correctness one.
        //
        // Left the `backoff_active` branch in place as a no-op so the
        // variable retains documentation value for the read below.
        let _ = backoff_active;

        // Non-blocking read. `None` means cold/stale cache — treat it as
        // the existing `Err(e)` branch (archive unreachable): arm the
        // backoff and fall through to the peer-SCP fallback. The
        // background refresh will warm the cache before the next cycle.
        let archive_latest = if backoff_active {
            tracing::debug!(
                current_ledger,
                next_checkpoint = next_cp,
                "Skipping archive query: previous tick confirmed archive is behind \
                 (backoff active)"
            );
            None
        } else {
            match self.get_cached_archive_checkpoint_nonblocking() {
                Some(latest) if latest >= next_cp => {
                    // Archive is current enough — clear any prior backoff.
                    let mut guard = self.archive_behind_until.write().await;
                    *guard = None;
                    Some(latest)
                }
                Some(latest) => {
                    // Archive responded but is still behind. Arm the backoff.
                    // Use shorter backoff when the next checkpoint is imminent
                    // (within freq/3 ledgers of publication). See #1754.
                    let backoff_secs = Self::archive_behind_backoff_secs(current_ledger);
                    let deadline = self.clock.now() + Duration::from_secs(backoff_secs);
                    let mut guard = self.archive_behind_until.write().await;
                    *guard = Some(deadline);
                    tracing::debug!(
                        archive_latest = latest,
                        next_checkpoint = next_cp,
                        backoff_secs,
                        "Archive behind next checkpoint — arming backoff"
                    );
                    None
                }
                None => {
                    // Cache cold/stale — a background refresh has been
                    // spawned and will complete within a few seconds. Do
                    // NOT arm `archive_behind_until`: that backoff exists
                    // to suppress redundant queries against a known-behind
                    // archive, and `None` is a transient state (refresh in
                    // flight), not a confirmed "archive behind" signal.
                    // Armoring 60s would force a skip across ~5 recovery
                    // ticks even after the refresh completes on tick 2.
                    // Fall through to peer-SCP fallback; the next tick
                    // (10 s later) will see the refreshed cache.
                    tracing::debug!(
                        next_checkpoint = next_cp,
                        "Archive checkpoint cache cold/stale — \
                         falling through to peer-SCP while refresh completes"
                    );
                    None
                }
            }
        };

        let archive_latest = match archive_latest {
            Some(latest) => latest,
            None => {
                tracing::info!(
                    current_ledger,
                    next_checkpoint = next_cp,
                    backoff_active,
                    "Recovery catchup skipped: archive hasn't published checkpoint yet \
                     — requesting SCP state from peers as fallback"
                );

                // While waiting for the archive, actively request SCP state
                // from peers. Some peers may still have tx_sets cached for
                // the missing slots, especially if they are slightly behind
                // the network tip. Without this, the node sits idle for 1-5
                // minutes until the next checkpoint publishes.
                if let Some(overlay) = self.overlay().await {
                    let overlay_clone = std::sync::Arc::clone(&overlay);
                    let ledger = current_ledger;
                    tokio::spawn(async move {
                        if let Err(e) = overlay_clone.request_scp_state(ledger).await {
                            tracing::debug!(
                                error = %e,
                                "Failed to request SCP state during inter-checkpoint recovery"
                            );
                        }
                    });
                }

                // Do NOT re-arm sync_recovery_pending here. Let the
                // SyncRecoveryManager's 10-second timer drive the next
                // attempt. Re-arming caused a 1-second spin loop because
                // the main event loop checks the flag every tick.
                return None;
            }
        };

        // Target the archive's latest checkpoint, not just the next one.
        // When the node is far behind (e.g., captive core bootstrapped
        // at ledger 80 while the validator is at 400+), catching up to
        // just the next checkpoint (87) leaves a huge gap where SCP
        // rejects all far-future EXTERNALIZE messages as Invalid because
        // tracking_index < slot_index.
        let target_cp = archive_latest;

        tracing::info!(
            current_ledger,
            next_checkpoint = next_cp,
            archive_latest = target_cp,
            "Targeting latest archive checkpoint for recovery catchup"
        );

        let result = self
            .spawn_catchup(
                CatchupTarget::Ledger(target_cp),
                "RecoveryEscalation",
                true, // reset_stuck_state
                true, // re_arm_recovery
            )
            .await;

        if result.is_some() {
            // Clear stale state only after confirming spawn succeeded.
            // Previously done unconditionally before spawn, so a failed
            // spawn would needlessly destroy the syncing_ledgers buffer
            // and reset recovery counters.
            {
                let mut buffer =
                    tracked_lock::tracked_write("syncing_ledgers", &self.syncing_ledgers).await;
                buffer.clear();
            }
            self.herder.clear_pending_tx_sets();
            self.reset_tx_set_tracking().await;
            self.recovery_attempts_without_progress
                .store(0, Ordering::SeqCst);
        }

        result
    }

    /// Spawn catchup as a background tokio task.
    ///
    /// Returns `Some(PendingCatchup)` if catchup was successfully spawned,
    /// or `None` if a catchup or ledger close is already in progress.
    /// The event loop polls the returned `PendingCatchup` in a `select!` branch.
    pub(super) async fn spawn_catchup(
        &self,
        target: CatchupTarget,
        label: &str,
        reset_stuck_state: bool,
        re_arm_recovery: bool,
    ) -> Option<PendingCatchup> {
        // Guard: don't start if catchup already running
        if self.catchup_in_progress.swap(true, Ordering::SeqCst) {
            tracing::info!(label, "spawn_catchup: catchup already in progress");
            return None;
        }

        // Guard: don't start if a live ledger close is in progress
        // (LedgerManager cannot be mutated concurrently)
        if self.is_applying_ledger() {
            tracing::info!(label, "spawn_catchup: ledger close in progress, deferring");
            self.catchup_in_progress.store(false, Ordering::SeqCst);
            return None;
        }

        // Transition to CatchingUp/Syncing state now that we know we'll
        // actually start. Previously callers did this before spawn_catchup,
        // which falsely mutated state when spawn returned None (incrementing
        // lost_sync_count, clearing is_tracking).
        self.set_phase_sub(super::phase::PHASE_13_11_SPAWN_CATCHUP_SET_STATE);
        self.set_state(AppState::CatchingUp).await;
        self.herder.set_state(henyey_herder::HerderState::Syncing);

        // Issue #1822: a catchup is now in flight; the consensus-stuck
        // signal is stale. Clear it so `/health` reflects the active
        // catchup truthfully rather than continuing to report
        // `post_catchup_stalled`. If the catchup fails, the next
        // `maybe_start_buffered_catchup` tick re-detects the stall and
        // repopulates — that is correct behavior (a failed catchup
        // counts as forward progress in the state machine's decision
        // table).
        {
            let mut guard = self.consensus_stuck_state.write().await;
            *guard = None;
        }

        // Start catchup message caching (belt-and-suspenders for tx_set ordering)
        self.set_phase_sub(super::phase::PHASE_13_12_SPAWN_CATCHUP_MSG_CACHE);
        let message_cache_handle = self.start_catchup_message_caching_from_self().await;

        // Create oneshot channel for result delivery
        let (result_tx, result_rx) = tokio::sync::oneshot::channel();

        // Upgrade self_arc for the spawned task
        self.set_phase_sub(super::phase::PHASE_13_13_SPAWN_CATCHUP_SELF_ARC_READ);
        let app = {
            let weak = self.self_arc.read().await;
            match weak.upgrade() {
                Some(arc) => arc,
                None => {
                    tracing::warn!(label, "spawn_catchup: failed to upgrade self_arc");
                    self.catchup_in_progress.store(false, Ordering::SeqCst);
                    if let Some(h) = message_cache_handle {
                        h.abort();
                    }
                    return None;
                }
            }
        };

        let label_owned = label.to_string();
        let task_handle = tokio::spawn(async move {
            tracing::info!(label = label_owned, "Spawned catchup task starting");
            app.set_phase(14); // 14 = catchup_running

            // Oneshot used only inside this task to capture the ready-to-spawn
            // persist job from `catchup_with_mode`'s Deferred finalizer, so we
            // can forward it to the event loop alongside the CatchupResult.
            let (persist_tx, mut persist_rx) = tokio::sync::oneshot::channel();
            let finalize = super::persist::CatchupFinalizer::deferred(
                app.database().clone(),
                app.ledger_manager().clone(),
                persist_tx,
            );
            let catchup_result = app.catchup(target, finalize).await;

            let persist_ready = match &catchup_result {
                Ok(_) => persist_rx.try_recv().ok(),
                Err(_) => None,
            };

            let _ = result_tx.send(PendingCatchupResult::new(catchup_result, persist_ready));
        });

        tracing::info!(label, "Catchup task spawned");

        Some(PendingCatchup {
            result_rx,
            task_handle,
            message_cache_handle,
            label: label.to_string(),
            reset_stuck_state,
            re_arm_recovery,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{classify_cannot_apply_reason, CannotApplyReason};

    /// Regression for issue #1759 (the original production symptom):
    /// `without_tx_set = 0, sequence_gap > 0` must classify as
    /// `BufferedSequenceGap`, not `MissingTxSets`. Matches the observed
    /// mainnet log line:
    /// `total_buffered=17 with_tx_set=17 without_tx_set=0`, where
    /// `first_buffered=62164651` and `current_ledger=62164648`
    /// (sequence_gap = 62164651 - 62164649 = 2).
    #[test]
    fn test_classify_sequence_gap_not_missing_tx_sets() {
        assert_eq!(
            classify_cannot_apply_reason(0, 2),
            CannotApplyReason::BufferedSequenceGap
        );
    }

    /// When at least one buffered slot is missing its tx_set, the legacy
    /// "missing tx_sets" diagnostic is the correct one, even if a sequence
    /// gap also exists (the tx_set gap is the operator-actionable signal
    /// first).
    #[test]
    fn test_classify_missing_tx_sets_wins_over_gap() {
        assert_eq!(
            classify_cannot_apply_reason(3, 2),
            CannotApplyReason::MissingTxSets
        );
        assert_eq!(
            classify_cannot_apply_reason(1, 0),
            CannotApplyReason::MissingTxSets
        );
    }

    /// Degenerate case: no buffered slots at all (`total == 0`,
    /// `sequence_gap == 0`, `without_tx_set == 0`). The decision must stay
    /// on the legacy path so the follow-up `with_tx_set == 0 && total > 0`
    /// guard in `analyze_externalized_gaps` retains its semantics.
    #[test]
    fn test_classify_empty_buffer_keeps_legacy_label() {
        assert_eq!(
            classify_cannot_apply_reason(0, 0),
            CannotApplyReason::MissingTxSets
        );
    }

    /// Non-regression: a contiguous buffer with every tx_set present
    /// (`without_tx_set == 0, sequence_gap == 0`) should never emit the
    /// sequence-gap diagnostic. This shape is rare in practice — it
    /// would mean the caller reached the outer `else` branch despite
    /// having an applyable next slot — but the classifier must still be
    /// stable.
    #[test]
    fn test_classify_contiguous_buffer_with_all_tx_sets() {
        assert_eq!(
            classify_cannot_apply_reason(0, 0),
            CannotApplyReason::MissingTxSets
        );
    }
}
