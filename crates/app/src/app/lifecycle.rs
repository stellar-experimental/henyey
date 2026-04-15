//! Application lifecycle: overlay message handling, periodic tick, and event loop orchestration.

use super::*;

/// Maximum number of ledger slots used for per-peer rate-limit windows.
/// Matches stellar-core's `Config::MAX_SLOTS_TO_REMEMBER` (default 12).
const MAX_SLOTS_TO_REMEMBER: u64 = 12;

impl App {
    /// Run the main event loop.
    ///
    /// This starts all subsystems and runs until shutdown is signaled.
    pub async fn run(&self) -> anyhow::Result<()> {
        tracing::info!("Starting main event loop");

        // Start overlay network if not already started.
        // (run_cmd may have already started it before catchup)
        {
            let overlay = self.overlay.read().await;
            if overlay.is_none() {
                drop(overlay); // release lock before starting
                self.start_overlay().await?;
            }
        }

        // Get current ledger state (catchup was already done by run_cmd)
        let current_ledger = self.get_current_ledger().await?;

        if current_ledger == 0 {
            // This shouldn't happen if run_cmd did catchup, but handle it just in case
            tracing::info!("No ledger state, running catchup first");
            self.catchup(CatchupTarget::Current).await?;
        }

        // Bootstrap herder with current ledger
        let ledger_seq = self.current_ledger_seq();
        *self.last_processed_slot.write().await = ledger_seq as u64;
        self.herder.start_syncing();
        self.herder.bootstrap(ledger_seq);
        tracing::info!(ledger_seq, "Herder bootstrapped");

        // Populate the initial bucket snapshot for the query server.
        self.update_bucket_snapshot();

        // Wait a short time for initial peer connections, then request SCP state
        self.clock.sleep(Duration::from_millis(500)).await;
        self.request_scp_state_from_peers().await;

        // Set state based on validator mode
        self.restore_operational_state().await;

        // Start sync recovery tracking to enable the consensus stuck timer
        self.start_sync_recovery_tracking();

        // Get message receiver from overlay
        let message_rx = self.overlay().await.map(|o| o.subscribe());

        let mut message_rx = match message_rx {
            Some(rx) => rx,
            None => {
                tracing::warn!("Overlay not started, running without network");
                // Create a dummy receiver that never receives
                let (tx, rx) = tokio::sync::broadcast::channel::<OverlayMessage>(1);
                drop(tx);
                rx
            }
        };

        // Get dedicated SCP message receiver (never drops messages)
        let scp_message_rx = {
            match self.overlay().await {
                Some(o) => o.subscribe_scp().await,
                None => None,
            }
        };

        let mut scp_message_rx = match scp_message_rx {
            Some(rx) => rx,
            None => {
                // Create a dummy receiver that never receives
                let (_tx, rx) = tokio::sync::mpsc::unbounded_channel::<OverlayMessage>();
                rx
            }
        };

        // Get dedicated fetch response receiver
        let fetch_response_rx = {
            match self.overlay().await {
                Some(o) => o.subscribe_fetch_responses().await,
                None => None,
            }
        };

        let mut fetch_response_rx = match fetch_response_rx {
            Some(rx) => rx,
            None => {
                // Create a dummy receiver that never receives
                let (_tx, rx) = tokio::sync::mpsc::channel::<OverlayMessage>(1);
                rx
            }
        };

        // Main run loop
        let mut shutdown_rx = self.shutdown_tx.subscribe();
        let mut consensus_interval = tokio::time::interval(Duration::from_secs(1));
        let mut stats_interval = tokio::time::interval(Duration::from_secs(30));
        stats_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut tx_advert_interval = tokio::time::interval(self.flood_advert_period());
        let mut tx_demand_interval = tokio::time::interval(self.flood_demand_period());
        let mut survey_interval = tokio::time::interval(Duration::from_secs(1));
        let mut survey_phase_interval = tokio::time::interval(Duration::from_secs(5));
        let mut survey_request_interval = tokio::time::interval(Duration::from_secs(1));
        let mut scp_timeout_interval = tokio::time::interval(Duration::from_millis(500));
        let mut ping_interval = tokio::time::interval(Duration::from_secs(5));
        let mut peer_maintenance_interval = tokio::time::interval(Duration::from_secs(10));
        peer_maintenance_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut peer_refresh_interval = tokio::time::interval(Duration::from_secs(60));
        peer_refresh_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut herder_cleanup_interval = tokio::time::interval(Duration::from_secs(30));
        herder_cleanup_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        // Get mutable access to SCP envelope receiver
        let mut scp_rx = self.scp_envelope_rx.lock().await;

        // Process any externalized slots recorded during catchup BEFORE entering the main loop.
        // This ensures we buffer LedgerCloseInfo before new EXTERNALIZE messages trigger cleanup
        // which would remove older externalized slots (only max_externalized_slots are kept).
        let mut pending_catchup: Option<PendingCatchup> = self.process_externalized_slots().await;

        // After the pre-loop process_externalized_slots (which may have triggered a
        // rapid close phase), clear all pending tx_set requests and tracking state.
        // During catchup, SCP state responses bring EXTERNALIZE messages for slots
        // whose tx_sets may already be evicted from peers' caches. The pre-loop
        // process_externalized_slots creates syncing_ledgers entries for these slots
        // and kicks off tx_set requests.  If peers silently drop those requests
        // (because the tx_sets are evicted), the 10-second timeout fires, sets
        // tx_set_all_peers_exhausted, and triggers unnecessary catchup — which
        // then repeats the same cycle infinitely.
        //
        // Clearing the state here ensures the main loop starts clean.  Fresh
        // EXTERNALIZE messages arriving via the dedicated SCP channel will create
        // new entries with current tx_set hashes that peers actually have.
        {
            let current_ledger = self.current_ledger_seq();
            self.herder.clear_pending_tx_sets();
            // Also clear syncing_ledgers entries that have no tx_set — these are
            // unfulfillable entries created from stale EXTERNALIZE messages.
            let mut buffer = self.syncing_ledgers.write().await;
            let pre_count = buffer.len();
            buffer.retain(|seq, info| {
                // Keep entries that are above current_ledger AND have a tx_set.
                // Remove entries that are at or below current_ledger (already closed)
                // or that have no tx_set (unfulfillable from catchup-phase EXTERNALIZE).
                *seq > current_ledger && info.tx_set.is_some()
            });
            let removed = pre_count - buffer.len();
            if removed > 0 {
                tracing::info!(
                    removed,
                    remaining = buffer.len(),
                    current_ledger,
                    "Removed stale/unfulfillable syncing_ledgers entries before main loop"
                );
            }
            // Reset all tx_set tracking state
            self.reset_tx_set_tracking().await;
        }

        tracing::info!("Entering main event loop");

        // Start the std::thread watchdog (independent of tokio runtime).
        self.start_event_loop_watchdog();

        let mut heartbeat_interval = tokio::time::interval(Duration::from_secs(10));
        heartbeat_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        // SCP message flow rate tracking
        let mut scp_messages_received: u64 = 0;
        let mut scp_messages_last_heartbeat: u64 = 0;
        let mut last_scp_message_at = self.clock.now();

        // In-progress background ledger close. Polled in the select loop.
        let mut pending_close: Option<PendingLedgerClose> = None;

        // Maximum messages to drain from SCP/fetch channels per tick.
        // On mainnet with 24+ validators, SCP messages can arrive faster
        // than they're processed.  An unbounded drain starves everything
        // else in the tick (sync recovery, consensus trigger, tx_set requests).
        const MAX_DRAIN_PER_TICK: usize = 200;

        let mut select_iteration: u64 = 0;
        loop {
            select_iteration += 1;
            self.tick_event_loop();
            self.set_phase(0); // 0 = waiting in select

            // Promote deferred catchup from handle_overlay_message / tx_flooding
            if pending_catchup.is_none() {
                let mut deferred = self.deferred_catchup.lock().await;
                if deferred.is_some() {
                    pending_catchup = deferred.take();
                }
            }

            if select_iteration <= 5 || select_iteration % 1000 == 0 {
                tracing::debug!(select_iteration, "Main loop: entering select!");
            }
            tokio::select! {
                // NOTE: Removed biased; to ensure timers get fair polling

                // Await pending ledger close completion
                join_result = async {
                    match pending_close.as_mut() {
                        Some(p) => (&mut p.handle).await,
                        None => std::future::pending().await,
                    }
                } => {
                    self.set_phase(6); // 6 = pending_close
                    tracing::debug!(select_iteration, "BRANCH: pending_close completed");
                    let pending = pending_close.take().unwrap();
                    let success = self.handle_close_complete(pending, join_result).await;
                    // Chain next close if successful.
                    if success {
                        // Publish queued history checkpoints (if any).
                        // This runs synchronously to ensure archives are up to date
                        // before captive core instances need them.
                        self.maybe_publish_history().await;

                        // Trigger consensus immediately after a successful close, matching
                        // stellar-core's triggerNextLedger() call inside closeLedger().
                        if self.is_validator {
                            self.try_trigger_consensus().await;
                        }

                        // Before trying the next close, drain SCP + fetch response channels.
                        // During rapid buffered closes the select! loop may not poll these
                        // channels frequently enough, so EXTERNALIZEs and TxSet responses
                        // can sit unprocessed.  Draining here ensures we have the latest
                        // network state before deciding whether another close is ready.
                        // Bounded to avoid starvation on high-traffic mainnet.
                        for _ in 0..MAX_DRAIN_PER_TICK {
                            match scp_message_rx.try_recv() {
                                Ok(scp_msg) => self.handle_overlay_message(scp_msg).await,
                                Err(_) => break,
                            }
                        }
                        for _ in 0..MAX_DRAIN_PER_TICK {
                            match fetch_response_rx.try_recv() {
                                Ok(fetch_msg) => self.handle_overlay_message(fetch_msg).await,
                                Err(_) => break,
                            }
                        }
                        if pending_catchup.is_none() {
                            if let Some(pc) = self.process_externalized_slots().await {
                                pending_catchup = Some(pc);
                            }
                        }

                        pending_close = self.try_start_ledger_close().await;

                        // If no more buffered ledgers to close, we just finished a rapid
                        // close cycle.
                        if pending_close.is_none() {
                            let current_ledger = self.current_ledger_seq();

                            // Reset last_externalized_at so the heartbeat stall detector
                            // doesn't fire prematurely based on the timestamp of the
                            // EXTERNALIZE that was received 8-10s ago during rapid closes.
                            *self.last_externalized_at.write().await = self.clock.now();

                            // Reset tx_set tracking so fresh requests can be made for
                            // buffered entries that still need tx_sets. Don't evict
                            // the entries themselves — they may be closeable once
                            // their tx_sets arrive from peers.
                            self.reset_tx_set_tracking().await;

                            // Also reset consensus stuck state since we just successfully
                            // closed ledgers — we're not stuck.
                            *self.consensus_stuck_state.write().await = None;

                            // Always request SCP state from peers after a rapid close
                            // cycle ends. The next slot's EXTERNALIZE was likely
                            // broadcast seconds ago and peers won't re-send it unless
                            // asked. Without this, the node waits for the "next natural
                            // EXTERNALIZE" which arrives for slot N+7 (where the network
                            // is now), not N+1 — creating a gap that triggers catchup.
                            let latest_ext = self.herder.latest_externalized_slot().unwrap_or(0);
                            tracing::info!(
                                current_ledger,
                                latest_ext,
                                "Rapid close cycle ended; requesting SCP state from peers"
                            );
                            if let Some(overlay) = self.overlay().await {
                                let _ = overlay.request_scp_state(current_ledger).await;
                            }
                            *self.last_scp_state_request_at.write().await = self.clock.now();
                        }
                    }
                }

                // Await pending catchup completion (spawned background task)
                catchup_result = async {
                    match pending_catchup.as_mut() {
                        Some(p) => (&mut p.result_rx).await,
                        None => std::future::pending().await,
                    }
                } => {
                    self.set_phase(15); // 15 = pending_catchup_complete
                    tracing::info!(select_iteration, "BRANCH: pending_catchup completed");
                    let pending = pending_catchup.take().unwrap();

                    // Abort message cache task
                    if let Some(handle) = pending.message_cache_handle {
                        handle.abort();
                    }

                    // Reset catchup_in_progress
                    self.catchup_in_progress.store(false, Ordering::SeqCst);

                    match catchup_result {
                        Ok(result) => {
                            self.handle_catchup_result(
                                result.result,
                                pending.reset_stuck_state,
                                &pending.label,
                            )
                            .await;

                            if result.made_progress && pending.re_arm_recovery {
                                // Pre-arm recovery so the main event loop checks
                                // again on the next tick instead of waiting 35s.
                                self.recovery_attempts_without_progress
                                    .store(1, Ordering::SeqCst);
                                self.sync_recovery_pending.store(true, Ordering::SeqCst);
                            }
                        }
                        Err(_) => {
                            // Oneshot sender was dropped — task panicked or was cancelled.
                            // Check for panic via the task handle.
                            if pending.task_handle.is_finished() {
                                match pending.task_handle.await {
                                    Err(e) if e.is_panic() => {
                                        tracing::error!(
                                            label = pending.label,
                                            "Catchup task panicked: {e}"
                                        );
                                    }
                                    _ => {
                                        tracing::error!(
                                            label = pending.label,
                                            "Catchup task completed without sending result"
                                        );
                                    }
                                }
                            } else {
                                tracing::error!(
                                    label = pending.label,
                                    "Catchup oneshot dropped but task still running"
                                );
                                pending.task_handle.abort();
                            }
                            // Restore operational state after failed catchup
                            self.restore_operational_state().await;
                        }
                    }

                    // Kick off first buffered close so the pending_close chain
                    // starts immediately rather than waiting for the next tick.
                    if pending_close.is_none() {
                        pending_close = self.try_start_ledger_close().await;
                    }
                }

                // Process SCP messages from dedicated never-drop channel.
                // These are guaranteed to arrive even if the broadcast channel overflows.
                Some(scp_msg) = scp_message_rx.recv() => {
                    self.set_phase(1); // 1 = scp_message
                    tracing::trace!(select_iteration, "BRANCH: scp_message_rx");
                    scp_messages_received += 1;
                    self.scp_messages_received.fetch_add(1, Ordering::Relaxed);
                    last_scp_message_at = self.clock.now();
                    let scp_slot = match &scp_msg.message {
                        StellarMessage::ScpMessage(env) => env.statement.slot_index,
                        _ => 0,
                    };
                    tracing::debug!(
                        scp_slot,
                        peer = %scp_msg.from_peer,
                        latency_ms = scp_msg.received_at.elapsed().as_millis(),
                        "SCP message arrived via dedicated channel"
                    );
                    self.handle_overlay_message(scp_msg).await;
                    // After processing an SCP message (which may buffer an
                    // EXTERNALIZE), kick off a buffered close if none is running.
                    if pending_close.is_none() && pending_catchup.is_none() {
                        pending_close = self.try_start_ledger_close().await;
                    }
                    tracing::trace!(select_iteration, "BRANCH: scp_message_rx done");
                }

                // Process fetch messages from dedicated never-drop channel.
                // Includes both responses (GeneralizedTxSet, TxSet, DontHave, ScpQuorumset)
                // and requests (GetScpState, GetScpQuorumset, GetTxSet) to ensure they
                // are never lost when the broadcast channel overflows.
                Some(fetch_msg) = fetch_response_rx.recv() => {
                    self.set_phase(2); // 2 = fetch_response
                    tracing::trace!(select_iteration, "BRANCH: fetch_response_rx");
                    tracing::debug!(
                        latency_ms = fetch_msg.received_at.elapsed().as_millis(),
                        "Received fetch message via dedicated channel"
                    );
                    self.handle_overlay_message(fetch_msg).await;
                    // After processing a fetch response (which may deliver a
                    // tx_set), kick off a buffered close if none is running.
                    if pending_close.is_none() && pending_catchup.is_none() {
                        pending_close = self.try_start_ledger_close().await;
                    }
                    tracing::trace!(select_iteration, "BRANCH: fetch_response_rx done");
                }

                // Process non-critical overlay messages (TX floods, etc.).
                // SCP, fetch-response, and fetch-request messages no longer arrive here —
                // they are routed exclusively to dedicated channels at the overlay layer.
                // The skip guards below are kept as defensive fallbacks.
                msg = message_rx.recv() => {
                    self.set_phase(3); // 3 = broadcast
                    match msg {
                        Ok(overlay_msg) => {
                            // Skip SCP messages from broadcast channel — they are already
                            // handled via the dedicated SCP channel above.
                            if matches!(overlay_msg.message, StellarMessage::ScpMessage(_)) {
                                continue;
                            }
                            // Skip fetch response and request messages from broadcast channel —
                            // they are handled via the dedicated fetch channel above.
                            if matches!(
                                overlay_msg.message,
                                StellarMessage::GeneralizedTxSet(_)
                                    | StellarMessage::TxSet(_)
                                    | StellarMessage::DontHave(_)
                                    | StellarMessage::ScpQuorumset(_)
                                    | StellarMessage::GetScpState(_)
                                    | StellarMessage::GetScpQuorumset(_)
                                    | StellarMessage::GetTxSet(_)
                            ) {
                                continue;
                            }
                            let delivery_latency = overlay_msg.received_at.elapsed();
                            let msg_type = match &overlay_msg.message {
                                StellarMessage::ScpMessage(_) => "SCP",
                                StellarMessage::Transaction(_) => "TX",
                                StellarMessage::TxSet(_) => "TxSet",
                                StellarMessage::GeneralizedTxSet(_) => {
                                    tracing::debug!(latency_ms = delivery_latency.as_millis(), "Overlay delivery latency for GeneralizedTxSet");
                                    "GeneralizedTxSet"
                                },
                                StellarMessage::ScpQuorumset(_) => {
                                    tracing::debug!(latency_ms = delivery_latency.as_millis(), "Overlay delivery latency for ScpQuorumset");
                                    "ScpQuorumset"
                                },
                                StellarMessage::GetTxSet(_) => "GetTxSet",
                                StellarMessage::Hello(_) => "Hello",
                                StellarMessage::Peers(_) => "Peers",
                                _ => "Other",
                            };
                            tracing::debug!(msg_type, latency_ms = delivery_latency.as_millis(), "Received overlay message");
                            self.handle_overlay_message(overlay_msg).await;
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                            // Only non-critical messages (TX floods) flow through the
                            // broadcast channel now, so lag is expected under load.
                            tracing::debug!(skipped = n, "Overlay broadcast receiver lagged (non-critical messages only)");
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                            tracing::info!("Overlay broadcast channel closed");
                            break;
                        }
                    }
                }

                // Broadcast outbound SCP envelopes
                envelope = scp_rx.recv() => {
                    self.set_phase(4); // 4 = scp_broadcast
                    if let Some(envelope) = envelope {
                        let slot = envelope.statement.slot_index;
                        let pledge_type = match &envelope.statement.pledges {
                            ScpStatementPledges::Nominate(_) => "NOMINATE",
                            ScpStatementPledges::Prepare(_) => "PREPARE",
                            ScpStatementPledges::Confirm(_) => "CONFIRM",
                            ScpStatementPledges::Externalize(_) => "EXTERNALIZE",
                        };
                        let sample = {
                            let mut latency = self.scp_latency.write().await;
                            latency.record_self_sent(slot, self.clock.now())
                        };
                        if let Some(ms) = sample {
                            let mut survey_data = self.survey_data.write().await;
                            survey_data.record_scp_first_to_self_latency(ms);
                        }
                        let msg = StellarMessage::ScpMessage(envelope);
                        if let Some(overlay) = self.overlay().await {
                            match overlay.broadcast(msg).await {
                                Ok(count) => {
                                    self.scp_messages_sent.fetch_add(1, Ordering::Relaxed);
                                    match pledge_type {
                                        "NOMINATE" => self.scp_nominate_sent.fetch_add(1, Ordering::Relaxed),
                                        "PREPARE" => self.scp_prepare_sent.fetch_add(1, Ordering::Relaxed),
                                        "CONFIRM" => self.scp_confirm_sent.fetch_add(1, Ordering::Relaxed),
                                        "EXTERNALIZE" => self.scp_externalize_sent.fetch_add(1, Ordering::Relaxed),
                                        _ => 0,
                                    };
                                    tracing::debug!(slot, peers = count, pledge_type, "Broadcast SCP envelope");
                                }
                                Err(e) => {
                                    tracing::warn!(slot, error = %e, pledge_type, "Failed to broadcast SCP envelope");
                                }
                            }
                        }
                    }
                }

                // Consensus timer - trigger ledger close for validators and process externalized
                _ = consensus_interval.tick() => {
                    self.set_phase(5); // 5 = consensus_tick

                    // Drain pending overlay messages FIRST before any catchup
                    // evaluation.  This ensures tx_sets and SCP envelopes that
                    // arrived since the last tick are processed before we decide
                    // whether to trigger catchup or consensus.

                    // Drain dedicated SCP channel first (highest priority)
                    for _ in 0..MAX_DRAIN_PER_TICK {
                        match scp_message_rx.try_recv() {
                            Ok(scp_msg) => self.handle_overlay_message(scp_msg).await,
                            Err(_) => break,
                        }
                    }

                    // Drain dedicated fetch response channel (tx_sets, dont_have, etc.)
                    for _ in 0..MAX_DRAIN_PER_TICK {
                        match fetch_response_rx.try_recv() {
                            Ok(fetch_msg) => self.handle_overlay_message(fetch_msg).await,
                            Err(_) => break,
                        }
                    }

                    // Check if SyncRecoveryManager requested recovery
                    if pending_catchup.is_none()
                        && self.sync_recovery_pending.swap(false, Ordering::SeqCst)
                    {
                        tracing::info!("Sync recovery requested, starting recovery");
                        // SyncRecoveryManager triggered recovery - perform it now
                        if let Ok(current_ledger) = self.get_current_ledger().await {
                            tracing::info!(current_ledger, "Calling out_of_sync_recovery");
                            pending_catchup =
                                self.out_of_sync_recovery(current_ledger).await;
                            tracing::info!("out_of_sync_recovery completed");
                        }
                        // Also check for buffered catchup (this handles timeout-based catchup)
                        if pending_catchup.is_none() {
                            pending_catchup =
                                self.maybe_start_buffered_catchup().await;
                        }
                    }

                    // Check for externalized slots to process
                    self.set_phase(10); // 10 = process_externalized
                    if pending_catchup.is_none() {
                        if let Some(pc) = self.process_externalized_slots().await {
                            pending_catchup = Some(pc);
                        }
                    }

                    // Start a background ledger close if one isn't already running.
                    if pending_close.is_none() {
                        pending_close = self.try_start_ledger_close().await;

                        // Proactive gap detection: if no close started and the next
                        // slot's EXTERNALIZE is missing while we have later ones,
                        // request SCP state from peers immediately. This catches
                        // missed EXTERNALIZEs within seconds, while peers still have
                        // the data cached (~60s window). Without this, the node waits
                        // for SyncRecoveryManager (35s timeout) which is too late.
                        if pending_close.is_none() && self.herder.state().can_receive_scp() {
                            let cl = self.current_ledger_seq();
                            let latest = self.herder.latest_externalized_slot().unwrap_or(0);
                            let next = cl as u64 + 1;
                            if latest > next
                                && self.herder.get_externalized(next).is_none()
                            {
                                let last_req = *self.last_scp_state_request_at.read().await;
                                // Throttle to at most once every 5 seconds
                                if last_req.elapsed() > Duration::from_secs(5) {
                                    tracing::info!(
                                        current_ledger = cl,
                                        latest_ext = latest,
                                        missing_slot = next,
                                        "Gap detected: next slot EXTERNALIZE missing, requesting SCP state"
                                    );
                                    if let Some(overlay) = self.overlay().await {
                                        let _ = overlay.request_scp_state(cl).await;
                                    }
                                    *self.last_scp_state_request_at.write().await = self.clock.now();
                                }
                            }
                        }
                    }

                    // Request any pending tx sets we need
                    self.request_pending_tx_sets().await;

                    // Publish queued history checkpoints.  This is normally done
                    // from the pending_close arm, but for solo validators the
                    // select may pick the tick arm repeatedly before pending_close.
                    if self.is_validator {
                        self.maybe_publish_history().await;
                    }

                    // For validators, try to trigger next round
                    if self.is_validator {
                        self.try_trigger_consensus().await;
                    }
                }

                // Stats logging
                _ = stats_interval.tick() => {
                    self.set_phase(20); // 20 = stats
                    self.log_stats().await;
                }

                // Batched tx advert flush (parity: ignoreIfOutOfSync)
                _ = tx_advert_interval.tick() => {
                    if self.herder.is_tracking() {
                        self.set_phase(21); // 21 = tx_advert_flush
                        self.flush_tx_adverts().await;
                    }
                }

                // Demand missing transactions from peers (parity: ignoreIfOutOfSync)
                _ = tx_demand_interval.tick() => {
                    if self.herder.is_tracking() {
                        self.set_phase(22); // 22 = tx_demand
                        self.run_tx_demands().await;
                    }
                }

                // Survey scheduler
                _ = survey_interval.tick() => {
                    self.set_phase(23); // 23 = survey
                    if self.config.overlay.auto_survey {
                        self.advance_survey_scheduler().await;
                    }
                }

                // Survey reporting request top-off
                _ = survey_request_interval.tick() => {
                    self.set_phase(24); // 24 = survey_request
                    self.top_off_survey_requests().await;
                }

                // Survey phase maintenance
                _ = survey_phase_interval.tick() => {
                    self.set_phase(25); // 25 = survey_phase
                    self.update_survey_phase().await;
                }

                // SCP nomination/ballot timeouts
                _ = scp_timeout_interval.tick() => {
                    self.set_phase(26); // 26 = scp_timeout
                    self.check_scp_timeouts().await;
                }

                // Ping peers for latency measurements
                _ = ping_interval.tick() => {
                    self.set_phase(27); // 27 = ping
                    self.send_peer_pings().await;
                }

                // Peer maintenance - reconnect if peer count drops too low
                _ = peer_maintenance_interval.tick() => {
                    self.set_phase(28); // 28 = peer_maintenance
                    self.maintain_peers().await;
                }

                // Refresh known peers from config + SQLite cache
                _ = peer_refresh_interval.tick() => {
                    self.set_phase(29); // 29 = peer_refresh
                    if let Some(overlay) = self.overlay().await {
                        let _ = self.refresh_known_peers(&overlay);
                    }
                }

                // Herder cleanup - evict expired data
                _ = herder_cleanup_interval.tick() => {
                    self.set_phase(30); // 30 = herder_cleanup
                    self.herder.cleanup();
                }

                // Shutdown signal (lowest priority)
                _ = shutdown_rx.recv() => {
                    tracing::info!("Shutdown signal received");
                    break;
                }

                // Heartbeat for debugging
                _ = heartbeat_interval.tick() => {
                    self.set_phase(16); // 16 = heartbeat
                    let tracking_slot = self.herder.tracking_slot();
                    let ledger = self.current_ledger_seq();
                    let latest_ext = self.herder.latest_externalized_slot().unwrap_or(0);
                    let peers = self.overlay().await.map(|o| o.peer_count()).unwrap_or(0);

                    // Check quorum status - use latest_ext if available since we have
                    // actual SCP messages for that slot, otherwise fall back to tracking_slot
                    let quorum_check_slot = if latest_ext > 0 { latest_ext } else { tracking_slot };
                    let heard_from_quorum = self.herder.heard_from_quorum(quorum_check_slot);
                    let is_v_blocking = self.herder.is_v_blocking(quorum_check_slot);

                    let scp_sent = self.scp_messages_sent.load(Ordering::Relaxed);
                    let nom_sent = self.scp_nominate_sent.load(Ordering::Relaxed);
                    let prep_sent = self.scp_prepare_sent.load(Ordering::Relaxed);
                    let conf_sent = self.scp_confirm_sent.load(Ordering::Relaxed);
                    let ext_sent = self.scp_externalize_sent.load(Ordering::Relaxed);
                    tracing::info!(
                        tracking_slot,
                        ledger,
                        latest_ext,
                        peers,
                        heard_from_quorum,
                        is_v_blocking,
                        scp_total = scp_messages_received,
                        scp_since_last = scp_messages_received - scp_messages_last_heartbeat,
                        scp_silent_secs = last_scp_message_at.elapsed().as_secs(),
                        scp_sent,
                        scp_sent_nom = nom_sent,
                        scp_sent_prep = prep_sent,
                        scp_sent_conf = conf_sent,
                        scp_sent_ext = ext_sent,
                        "Heartbeat"
                    );
                    scp_messages_last_heartbeat = scp_messages_received;

                    // Warn if we haven't heard from quorum for a while
                    if self.is_validator && !heard_from_quorum && peers > 0 {
                        tracing::warn!(
                            tracking_slot,
                            is_v_blocking,
                            "Have not heard from quorum - may be experiencing network partition"
                        );
                    }

                    // If externalization stalls, ask peers for fresh SCP state.
                    if peers > 0 && self.herder.state().can_receive_scp() {
                        let now = self.clock.now();
                        let last_ext = *self.last_externalized_at.read().await;
                        let last_request = *self.last_scp_state_request_at.read().await;
                        if now.duration_since(last_ext) > Duration::from_secs(20)
                            && now.duration_since(last_request) > Duration::from_secs(10)
                        {
                            let current_ledger = self.current_ledger_seq();
                            let gap = latest_ext.saturating_sub(current_ledger as u64);

                            // Check if the very next slot's EXTERNALIZE is missing.
                            // If it is, request SCP state immediately regardless of
                            // gap size — every second we wait, the chance of peers
                            // still having it in cache decreases.
                            let next_slot = current_ledger as u64 + 1;
                            let next_slot_missing = latest_ext > next_slot
                                && self.herder.get_externalized(next_slot).is_none();

                            if gap <= TX_SET_REQUEST_WINDOW && !next_slot_missing {
                                // Small gap and we have the next slot's EXTERNALIZE.
                                // Don't request SCP state — peers would send stale
                                // EXTERNALIZE for old slots whose tx_sets are evicted.
                                tracing::debug!(
                                    current_ledger,
                                    latest_ext,
                                    gap,
                                    "Heartbeat: essentially caught up, skipping SCP state request"
                                );
                            } else {
                                tracing::warn!(
                                    latest_ext,
                                    tracking_slot,
                                    heard_from_quorum,
                                    gap,
                                    next_slot_missing,
                                    "SCP externalization stalled; requesting SCP state"
                                );
                                *self.last_scp_state_request_at.write().await = now;
                                self.request_scp_state_from_peers().await;
                            }
                        }
                    }

                    // Out-of-sync recovery: purge old slots when we're too far behind.
                    // This mirrors stellar-core's outOfSyncRecovery() behavior.
                    // When we have v-blocking slots that are >100 ahead of older slots,
                    // purge the old slots to free memory and allow recovery.
                    if !self.herder.state().can_receive_scp() || !heard_from_quorum {
                        if let Some(purge_slot) = self.herder.out_of_sync_recovery(ledger as u64) {
                            tracing::info!(
                                purge_slot,
                                ledger,
                                tracking_slot,
                                "Out-of-sync recovery: purged old slots"
                            );
                        }
                    }
                }
            }
        }

        // Clean up pending catchup on shutdown
        if let Some(pending) = pending_catchup.take() {
            tracing::info!(
                label = pending.label,
                "Aborting pending catchup on shutdown"
            );
            pending.task_handle.abort();
            if let Some(handle) = pending.message_cache_handle {
                handle.abort();
            }
            self.catchup_in_progress.store(false, Ordering::SeqCst);
        }

        self.set_state(AppState::ShuttingDown).await;
        self.shutdown_internal().await?;

        Ok(())
    }

    /// Start the overlay network.
    pub async fn start_overlay(&self) -> anyhow::Result<()> {
        tracing::info!("Starting overlay network");

        self.store_config_peers();

        // Create local node info
        let mut local_node =
            if self.config.network.passphrase == "Test SDF Network ; September 2015" {
                LocalNode::new_testnet(self.keypair.clone())
            } else {
                LocalNode::new_mainnet(self.keypair.clone())
            };
        local_node.listening_port = self.config.overlay.peer_port;
        local_node.set_commit_hash(&self.config.build.commit_hash);

        // Start with testnet or mainnet defaults for seed peers, but only if
        // the app config doesn't explicitly set known_peers (which includes the
        // compat config case where known_peers is intentionally cleared).
        let mut overlay_config = if !self.config.overlay.known_peers.is_empty() {
            // Explicit peers configured — start from empty defaults
            OverlayManagerConfig {
                known_peers: self
                    .config
                    .overlay
                    .known_peers
                    .iter()
                    .filter_map(|s| Self::parse_peer_address(s))
                    .collect(),
                ..OverlayManagerConfig::default()
            }
        } else if self.config.is_compat_config {
            // Compat config with no known peers (e.g., local standalone mode) —
            // do NOT inject testnet/mainnet seed peers.
            OverlayManagerConfig::default()
        } else if self.config.network.passphrase == "Test SDF Network ; September 2015" {
            OverlayManagerConfig::testnet()
        } else {
            OverlayManagerConfig::mainnet()
        };

        // Override with app config settings
        overlay_config.max_inbound_peers = self.config.overlay.max_inbound_peers;
        overlay_config.max_outbound_peers = self.config.overlay.max_outbound_peers;
        overlay_config.target_outbound_peers = self.config.overlay.target_outbound_peers;
        overlay_config.listen_port = self.config.overlay.peer_port;
        overlay_config.listen_enabled = self.is_validator; // Validators listen for connections
        overlay_config.is_validator = self.is_validator; // Watchers filter non-essential messages
        overlay_config.network_passphrase = self.config.network.passphrase.clone();

        if let Ok(persisted) = self.load_persisted_peers() {
            for addr in persisted {
                if !overlay_config.known_peers.contains(&addr) {
                    overlay_config.known_peers.push(addr);
                }
            }
        }

        // Convert preferred peers
        if !self.config.overlay.preferred_peers.is_empty() {
            overlay_config.preferred_peers = self
                .config
                .overlay
                .preferred_peers
                .iter()
                .filter_map(|s| {
                    let parts: Vec<&str> = s.split(':').collect();
                    match parts.len() {
                        1 => Some(PeerAddress::new(parts[0], 11625)),
                        2 => parts[1]
                            .parse()
                            .ok()
                            .map(|port| PeerAddress::new(parts[0], port)),
                        _ => None,
                    }
                })
                .collect();
        }

        let (peer_event_tx, mut peer_event_rx) = mpsc::channel(1024);
        overlay_config.peer_event_tx = Some(peer_event_tx);

        let db = self.db.clone();
        tokio::spawn(async move {
            while let Some(event) = peer_event_rx.recv().await {
                update_peer_record(&db, event);
            }
        });

        tracing::info!(
            listen_port = overlay_config.listen_port,
            known_peers = overlay_config.known_peers.len(),
            listen_enabled = overlay_config.listen_enabled,
            "Creating overlay with config"
        );

        let mut overlay = OverlayManager::new_with_connection_factory(
            overlay_config,
            local_node,
            Arc::clone(&self.overlay_connection_factory),
        )?;
        overlay.set_scp_callback(Arc::new(super::HerderScpCallback {
            herder: Arc::clone(&self.herder),
        }));
        if let Ok(bans) = self.db.load_bans() {
            for ban in bans {
                if let Some(peer_id) = Self::strkey_to_peer_id(&ban) {
                    overlay.ban_peer(peer_id).await;
                } else {
                    tracing::warn!(node = %ban, "Ignoring invalid ban entry");
                }
            }
        }

        overlay.start().await?;

        let peer_count = overlay.peer_count();
        tracing::info!(peer_count, "Overlay network started");

        *self.overlay.write().await = Some(Arc::new(overlay));
        Ok(())
    }

    /// Set the weak reference to self for spawning background tasks.
    /// Must be called after wrapping App in Arc.
    pub async fn set_self_arc(self: &Arc<Self>) {
        *self.self_arc.write().await = Arc::downgrade(self);
    }

    /// Handle a message from the overlay network.
    async fn handle_overlay_message(&self, msg: OverlayMessage) {
        match msg.message {
            StellarMessage::ScpMessage(envelope) => {
                let slot = envelope.statement.slot_index;
                let tracking = self.herder.tracking_slot();

                let sample = {
                    let mut latency = self.scp_latency.write().await;
                    let now = self.clock.now();
                    latency.record_first_seen(slot, now);
                    latency.record_other_after_self(slot, now)
                };
                if let Some(ms) = sample {
                    let mut survey_data = self.survey_data.write().await;
                    survey_data.record_scp_self_to_other_latency(ms);
                }

                // Check if this is an EXTERNALIZE message so we can request the tx set
                let is_externalize = matches!(
                    &envelope.statement.pledges,
                    stellar_xdr::curr::ScpStatementPledges::Externalize(_)
                );
                let tx_set_hash = match &envelope.statement.pledges {
                    stellar_xdr::curr::ScpStatementPledges::Externalize(ext) => {
                        match StellarValue::from_xdr(
                            &ext.commit.value.0,
                            stellar_xdr::curr::Limits::none(),
                        ) {
                            Ok(stellar_value) => {
                                Some(Hash256::from_bytes(stellar_value.tx_set_hash.0))
                            }
                            Err(err) => {
                                tracing::warn!(slot, error = %err, "Failed to parse externalized StellarValue");
                                None
                            }
                        }
                    }
                    _ => None,
                };

                let hash = henyey_common::scp_quorum_set_hash(&envelope.statement);
                let hash256 = henyey_common::Hash256::from_bytes(hash.0);
                let sender_node_id = envelope.statement.node_id.clone();

                let envelope_result = self.herder.receive_scp_envelope(envelope);

                // Request quorum set only after receive_scp_envelope validates
                // the envelope (signature check, slot range, etc.). This prevents
                // forged envelopes from creating immortal pending qset entries.
                // Matches stellar-core which verifies before any fetch logic.
                //
                // Fetching is included because process_scp_envelope may return
                // Fetching when tx_sets are cached but the quorum set is unknown.
                // The envelope is buffered in FetchingEnvelopes; we still need
                // the app layer to register the pending qset request and send
                // the network fetch so handle_quorum_set accepts the response
                // (AUDIT-104).
                if matches!(
                    envelope_result,
                    EnvelopeState::Valid | EnvelopeState::Pending | EnvelopeState::Fetching
                ) {
                    if self
                        .herder
                        .request_quorum_set(hash256, sender_node_id, slot)
                    {
                        // New pending request - need to fetch from network
                        let peer = msg.from_peer.clone();
                        if let Some(overlay) = self.overlay().await {
                            let request =
                                StellarMessage::GetScpQuorumset(stellar_xdr::curr::Uint256(hash.0));
                            if let Err(e) = overlay.try_send_to(&peer, request) {
                                tracing::debug!(peer = %peer, error = %e, "Failed to request quorum set");
                            }
                        }
                    }
                }

                match envelope_result {
                    EnvelopeState::Valid => {
                        tracing::debug!(slot, tracking, "SCP envelope accepted (Valid)");
                        // NOTE: We intentionally do NOT send a sync recovery
                        // heartbeat here.  SCP messages flowing (including
                        // EXTERNALIZE) is not evidence of ledger progress —
                        // the node may have all the EXTERNALIZE messages but
                        // be missing tx_sets needed to actually close ledgers.
                        // The heartbeat is sent only on actual ledger close
                        // (see ledger_close.rs handle_close_complete) so the
                        // stuck timer fires when ledgers stop advancing.

                        // For EXTERNALIZE messages, immediately try to close ledger and request tx set
                        if is_externalize {
                            tracing::debug!(slot, tracking, "EXTERNALIZE Valid — processing slot");
                            if let Some(tx_set_hash) = tx_set_hash {
                                self.herder.scp_driver().request_tx_set(tx_set_hash, slot);
                                self.maybe_request_tx_set_from_peer(&tx_set_hash, &msg.from_peer)
                                    .await;
                            }
                            // First, process externalized slots to register pending tx set requests
                            if let Some(pc) = self.process_externalized_slots().await {
                                *self.deferred_catchup.lock().await = Some(pc);
                            }
                            // Then, immediately request any pending tx sets
                            self.request_pending_tx_sets().await;

                            let current_ledger = self.current_ledger_seq() as u64;
                            if slot > current_ledger + 1 {
                                self.sync_recovery_pending.store(true, Ordering::SeqCst);
                                // If the gap is large, fast-track to catchup
                                if slot > current_ledger + 2 {
                                    self.recovery_attempts_without_progress
                                        .store(RECOVERY_ESCALATION_CATCHUP, Ordering::SeqCst);
                                }
                            }
                        }
                    }
                    EnvelopeState::Pending => {
                        tracing::debug!(slot, tracking, "SCP envelope buffered for future slot");
                        // If we receive a Pending EXTERNALIZE for a slot far ahead of
                        // our current ledger, the network has moved on without us.
                        // Immediately escalate recovery to trigger catchup rather than
                        // waiting 60+ seconds for the normal escalation path.
                        //
                        // IMPORTANT: Only fast-track if the next slot (current_ledger+1)
                        // is NOT in syncing_ledgers. After catchup, the node has buffered
                        // entries with tx_sets ready for rapid close. Fresh EXTERNALIZE
                        // envelopes from SCP state responses are always far ahead (gap 10+),
                        // and fast-tracking would destroy the buffer via
                        // trigger_recovery_catchup → buffer.clear(), preventing convergence.
                        if is_externalize {
                            let current_ledger = self.current_ledger_seq() as u64;
                            if slot > current_ledger + 2 {
                                let next_slot = current_ledger as u32 + 1;
                                let have_next = self
                                    .syncing_ledgers
                                    .read()
                                    .await
                                    .get(&next_slot)
                                    .map(|info| info.tx_set.is_some())
                                    .unwrap_or(false);
                                if have_next {
                                    tracing::debug!(
                                        slot,
                                        current_ledger,
                                        gap = slot - current_ledger,
                                        "Pending EXTERNALIZE far ahead but next slot buffered — \
                                         letting rapid close proceed"
                                    );
                                } else {
                                    tracing::info!(
                                        slot,
                                        current_ledger,
                                        gap = slot - current_ledger,
                                        "Pending EXTERNALIZE far ahead — fast-tracking catchup"
                                    );
                                    self.recovery_attempts_without_progress
                                        .store(RECOVERY_ESCALATION_CATCHUP, Ordering::SeqCst);
                                    self.sync_recovery_pending.store(true, Ordering::SeqCst);
                                }
                            }
                        }
                    }
                    EnvelopeState::Duplicate => {
                        // Expected, ignore silently
                    }
                    EnvelopeState::TooOld => {
                        tracing::debug!(slot, tracking, "SCP envelope rejected (TooOld)");
                    }
                    EnvelopeState::Invalid => {
                        tracing::debug!(slot, peer = %msg.from_peer, "SCP envelope rejected (Invalid)");
                    }
                    EnvelopeState::InvalidSignature => {
                        tracing::warn!(slot, peer = %msg.from_peer, "SCP envelope with invalid signature");
                    }
                    EnvelopeState::Fetching => {
                        // Envelope is waiting for its tx set to be fetched.
                        // Request the tx set from the peer that sent this envelope.
                        tracing::debug!(
                            slot,
                            peer = %msg.from_peer,
                            "SCP EXTERNALIZE waiting for tx set (Fetching)"
                        );
                        if let Some(tx_set_hash) = tx_set_hash {
                            let peer = msg.from_peer.clone();
                            if let Some(overlay) = self.overlay().await {
                                let request = StellarMessage::GetTxSet(stellar_xdr::curr::Uint256(
                                    tx_set_hash.0,
                                ));
                                if let Err(e) = overlay.try_send_to(&peer, request) {
                                    tracing::debug!(
                                        peer = %peer,
                                        error = %e,
                                        "Failed to request tx set for fetching envelope"
                                    );
                                }
                            }
                        }
                        // Also request any other pending tx sets
                        self.request_pending_tx_sets().await;
                    }
                }
            }

            StellarMessage::Transaction(tx_env) => {
                let tx_hash = Hash256::hash_xdr(&tx_env).ok();
                match self.herder.receive_transaction(tx_env.clone()) {
                    henyey_herder::TxQueueResult::Added => {
                        tracing::debug!(peer = %msg.from_peer, "Transaction added to queue");
                        if let Some(hash) = tx_hash {
                            self.record_tx_pull_latency(hash, &msg.from_peer).await;
                        }
                        self.enqueue_tx_advert(&tx_env).await;
                    }
                    henyey_herder::TxQueueResult::Duplicate => {
                        if let Some(hash) = tx_hash {
                            self.record_tx_pull_latency(hash, &msg.from_peer).await;
                        }
                        // Expected, ignore
                    }
                    henyey_herder::TxQueueResult::QueueFull => {
                        // Aggregate count emitted per ledger close in Herder::ledger_closed()
                    }
                    henyey_herder::TxQueueResult::FeeTooLow => {
                        tracing::debug!("Transaction fee too low, rejected");
                    }
                    henyey_herder::TxQueueResult::Invalid(code) => {
                        tracing::debug!(?code, "Invalid transaction rejected");
                    }
                    henyey_herder::TxQueueResult::Banned => {
                        tracing::debug!("Transaction from banned source rejected");
                    }
                    henyey_herder::TxQueueResult::Filtered => {
                        tracing::debug!("Transaction filtered by operation type");
                    }
                    henyey_herder::TxQueueResult::TryAgainLater => {
                        tracing::debug!(
                            "Transaction rejected: account already has pending transaction"
                        );
                    }
                }
            }

            StellarMessage::FloodAdvert(advert) => {
                // Parity: ignoreIfOutOfSync (Peer.cpp:1164-1172)
                if !self.herder.is_tracking() {
                    tracing::trace!("Ignoring FloodAdvert: not tracking");
                } else {
                    self.handle_flood_advert(&msg.from_peer, advert).await;
                }
            }

            StellarMessage::FloodDemand(demand) => {
                // Parity: ignoreIfOutOfSync (Peer.cpp:1164-1172)
                if !self.herder.is_tracking() {
                    tracing::trace!("Ignoring FloodDemand: not tracking");
                } else {
                    self.handle_flood_demand(&msg.from_peer, demand).await;
                }
            }

            StellarMessage::DontHave(dont_have) => {
                let is_tx_set = matches!(
                    dont_have.type_,
                    stellar_xdr::curr::MessageType::TxSet
                        | stellar_xdr::curr::MessageType::GeneralizedTxSet
                );
                let is_ping = matches!(
                    dont_have.type_,
                    stellar_xdr::curr::MessageType::ScpQuorumset
                );
                if is_tx_set {
                    tracing::debug!(
                        peer = %msg.from_peer,
                        hash = hex::encode(dont_have.req_hash.0),
                        "Peer reported DontHave for TxSet"
                    );
                    let hash = Hash256::from_bytes(dont_have.req_hash.0);
                    let dont_have_count = {
                        let mut map = self.tx_set_dont_have.write().await;
                        map.entry(hash).or_default().insert(msg.from_peer.clone());
                        map.get(&hash).map(|s| s.len()).unwrap_or(0)
                    };
                    let peer_count = self.get_peer_count().await;
                    let all_peers_dont_have = dont_have_count >= peer_count && peer_count > 0;

                    if self.herder.needs_tx_set(&hash) {
                        if all_peers_dont_have {
                            // All peers don't have this tx_set - log but DON'T trigger catchup.
                            // Like stellar-core, we rely on slot eviction to eventually
                            // clean up old slots when we're >100 slots behind the highest
                            // v-blocking slot. Triggering catchup on DontHave creates loops
                            // because catchup targets checkpoints, leaving gaps that also
                            // get DontHave responses.
                            // Only log once per hash to avoid spam during recovery.
                            let already_warned =
                                self.tx_set_exhausted_warned.read().await.contains(&hash);
                            if !already_warned {
                                self.tx_set_exhausted_warned.write().await.insert(hash);
                                tracing::info!(
                                    hash = %hash,
                                    dont_have_count,
                                    peer_count,
                                    "All peers reported DontHave for needed TxSet; relying on slot eviction"
                                );
                            }
                            // Reset request tracking to allow retry later
                            let mut last_request = self.tx_set_last_request.write().await;
                            last_request.remove(&hash);
                        } else {
                            {
                                let mut last_request = self.tx_set_last_request.write().await;
                                last_request.remove(&hash);
                            }
                            self.request_pending_tx_sets().await;
                        }
                    }
                }
                if is_ping {
                    self.process_ping_response(&msg.from_peer, dont_have.req_hash.0)
                        .await;
                }
            }

            StellarMessage::GetScpState(ledger_seq) => {
                if self
                    .check_peer_rate_limit(
                        &self.scp_state_query_info,
                        &msg.from_peer,
                        GET_SCP_STATE_MAX_RATE,
                    )
                    .await
                {
                    tracing::debug!(ledger_seq, peer = %msg.from_peer, "Peer requested SCP state");
                    self.send_scp_state(&msg.from_peer, ledger_seq).await;
                } else {
                    tracing::debug!(peer = %msg.from_peer, "Dropping GET_SCP_STATE request (rate limited)");
                }
            }

            StellarMessage::GetScpQuorumset(hash) => {
                let max_rate =
                    self.rate_limit_window().as_secs() as u32 * QUERY_RESPONSE_MULTIPLIER;
                if self
                    .check_peer_rate_limit(&self.qset_query_info, &msg.from_peer, max_rate)
                    .await
                {
                    tracing::debug!(hash = hex::encode(hash.0), peer = %msg.from_peer, "Peer requested quorum set");
                    self.send_quorum_set(&msg.from_peer, hash).await;
                } else {
                    tracing::debug!(peer = %msg.from_peer, "Dropping GET_SCP_QUORUMSET request (rate limited)");
                }
            }

            StellarMessage::ScpQuorumset(quorum_set) => {
                tracing::debug!(peer = %msg.from_peer, "Received quorum set");
                let hash = henyey_scp::hash_quorum_set(&quorum_set);
                self.process_ping_response(&msg.from_peer, hash.0).await;
                self.handle_quorum_set(&msg.from_peer, quorum_set).await;
            }

            StellarMessage::TimeSlicedSurveyStartCollecting(start) => {
                self.handle_survey_start_collecting(&msg.from_peer, start)
                    .await;
            }

            StellarMessage::TimeSlicedSurveyStopCollecting(stop) => {
                self.handle_survey_stop_collecting(&msg.from_peer, stop)
                    .await;
            }

            StellarMessage::TimeSlicedSurveyRequest(request) => {
                self.handle_survey_request(&msg.from_peer, request).await;
            }

            StellarMessage::TimeSlicedSurveyResponse(response) => {
                self.handle_survey_response(&msg.from_peer, response).await;
            }

            StellarMessage::Peers(peer_list) => {
                tracing::debug!(count = peer_list.len(), peer = %msg.from_peer, "Received peer list");
                self.process_peer_list(peer_list).await;
            }

            StellarMessage::TxSet(tx_set) => {
                // Compute hash for logging
                let xdr_bytes =
                    stellar_xdr::curr::WriteXdr::to_xdr(&tx_set, stellar_xdr::curr::Limits::none())
                        .unwrap_or_default();
                let computed_hash = henyey_common::Hash256::hash(&xdr_bytes);
                tracing::info!(
                    peer = %msg.from_peer,
                    computed_hash = %computed_hash,
                    prev_ledger = hex::encode(tx_set.previous_ledger_hash.0),
                    tx_count = tx_set.txs.len(),
                    "APP: Received TxSet from overlay"
                );
                self.handle_tx_set(tx_set).await;
            }

            StellarMessage::GeneralizedTxSet(gen_tx_set) => {
                // Compute hash for logging
                let xdr_bytes = stellar_xdr::curr::WriteXdr::to_xdr(
                    &gen_tx_set,
                    stellar_xdr::curr::Limits::none(),
                )
                .unwrap_or_default();
                let computed_hash = henyey_common::Hash256::hash(&xdr_bytes);
                tracing::debug!(
                    peer = %msg.from_peer,
                    computed_hash = %computed_hash,
                    "APP: Received GeneralizedTxSet from overlay"
                );
                self.handle_generalized_tx_set(gen_tx_set).await;
            }

            StellarMessage::GetTxSet(hash) => {
                let max_rate =
                    self.rate_limit_window().as_secs() as u32 * QUERY_RESPONSE_MULTIPLIER;
                if self
                    .check_peer_rate_limit(&self.tx_set_query_info, &msg.from_peer, max_rate)
                    .await
                {
                    tracing::debug!(hash = hex::encode(hash.0), peer = %msg.from_peer, "Peer requested TxSet");
                    self.send_tx_set(&msg.from_peer, &henyey_common::Hash256(hash.0))
                        .await;
                } else {
                    tracing::debug!(peer = %msg.from_peer, "Dropping GET_TX_SET request (rate limited)");
                }
            }

            _ => {
                // Other message types (Hello, Auth, etc.) are handled by overlay
                tracing::trace!(msg_type = ?std::mem::discriminant(&msg.message), "Ignoring message type");
            }
        }
    }

    /// Log current stats.
    async fn log_stats(&self) {
        let stats = self.herder.stats();
        let ledger = self.current_ledger_seq();

        // Get overlay stats if available
        let (peer_count, flood_stats) = {
            match self.overlay().await {
                Some(o) => (o.peer_count(), Some(o.flood_stats())),
                None => (0, None),
            }
        };

        tracing::debug!(
            state = ?stats.state,
            tracking_slot = stats.tracking_slot,
            pending_txs = stats.pending_transactions,
            ledger,
            peers = peer_count,
            is_validator = self.is_validator,
            "Node status"
        );

        if let Some(fs) = flood_stats {
            tracing::debug!(
                seen_messages = fs.seen_count,
                dropped_messages = fs.dropped_messages,
                "Flood gate stats"
            );
        }
    }

    /// Request a tx set from a peer if the herder still needs it.
    async fn maybe_request_tx_set_from_peer(
        &self,
        tx_set_hash: &henyey_common::Hash256,
        peer: &PeerId,
    ) {
        if !self.herder.needs_tx_set(tx_set_hash) {
            return;
        }
        let Some(overlay) = self.overlay().await else {
            return;
        };
        let request = StellarMessage::GetTxSet(stellar_xdr::curr::Uint256(tx_set_hash.0));
        if let Err(e) = overlay.try_send_to(peer, request) {
            tracing::debug!(
                peer = %peer,
                error = %e,
                "Failed to request tx set from externalize peer"
            );
        }
    }

    /// Get the current ledger sequence from the database.
    pub(super) async fn get_current_ledger(&self) -> anyhow::Result<u32> {
        // Check if ledger manager is initialized
        if self.ledger_manager.is_initialized() {
            return Ok(self.ledger_manager.current_ledger_seq());
        }
        // No state yet
        Ok(0)
    }

    /// Get the number of connected peers.
    async fn get_peer_count(&self) -> usize {
        self.overlay().await.map(|o| o.peer_count()).unwrap_or(0)
    }

    /// Signal the application to shut down.
    pub fn shutdown(&self) {
        tracing::info!("Shutdown requested");
        let _ = self.shutdown_tx.send(());
    }

    /// Subscribe to shutdown notifications.
    pub fn subscribe_shutdown(&self) -> tokio::sync::broadcast::Receiver<()> {
        self.shutdown_tx.subscribe()
    }

    /// Internal shutdown cleanup.
    async fn shutdown_internal(&self) -> anyhow::Result<()> {
        tracing::info!("Performing shutdown cleanup");

        self.set_state(AppState::ShuttingDown).await;
        self.stop_survey_reporting().await;

        // Explicitly flush and close the meta stream before shutting down
        // overlay connections. This ensures all streamed LedgerCloseMeta frames
        // are written to the pipe/file before the process exits. The stream
        // uses per-write flush, so this is mostly defensive — but it also
        // ensures the underlying fd is closed promptly (important for pipe
        // consumers like stellar-rpc that detect EOF to know core has stopped).
        if let Some(ref writer) = self.meta_writer {
            tracing::info!("Shutting down MetaWriter");
            writer.shutdown().await;
        }
        {
            let mut guard = self.meta_stream.lock().unwrap();
            if let Some(stream) = guard.take() {
                tracing::info!("Closing metadata output stream");
                drop(stream);
            }
        }

        let mut overlay = self.overlay.write().await;
        if let Some(overlay_arc) = overlay.take() {
            match Arc::try_unwrap(overlay_arc) {
                Ok(mut overlay_owned) => {
                    if let Err(err) = overlay_owned.shutdown().await {
                        tracing::warn!(error = %err, "Overlay shutdown reported error");
                    }
                }
                Err(arc) => {
                    // Other references still exist; just drop and let the
                    // OverlayManager's Drop impl clean up.
                    tracing::warn!(
                        "Overlay still has outstanding references at shutdown, dropping"
                    );
                    drop(arc);
                }
            }
        }

        Ok(())
    }

    /// Compute the standard per-peer rate-limit window.
    fn rate_limit_window(&self) -> Duration {
        let close_time_secs = self.herder.ledger_close_time() as u64;
        Duration::from_secs(close_time_secs * MAX_SLOTS_TO_REMEMBER)
    }

    /// Check per-peer rate limit. Returns true if the request is allowed.
    async fn check_peer_rate_limit(
        &self,
        map: &tokio::sync::RwLock<std::collections::HashMap<henyey_overlay::PeerId, QueryInfo>>,
        peer: &henyey_overlay::PeerId,
        max_rate: u32,
    ) -> bool {
        let window = self.rate_limit_window();
        let mut guard = map.write().await;
        let info = guard.entry(peer.clone()).or_insert_with(QueryInfo::new);
        if info.allow(window, max_rate) {
            info.num_queries += 1;
            true
        } else {
            false
        }
    }
}
