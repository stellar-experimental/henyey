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
            // This shouldn't happen if run_cmd did catchup, but handle it just
            // in case (RunMode::Watcher skips startup catchup, so we can
            // legitimately reach here with no persisted state).
            tracing::info!("No ledger state, running catchup first");

            // We're inside `App::run()` which is itself called from a
            // `tokio::spawn` task (see run_cmd::run_node). Calling
            // spawn_blocking here risks the deadlock class from #1713 if the
            // blocking pool is saturated. Use the Deferred finalizer pattern
            // exactly as the event-loop recovery path does: collect persist
            // data via a oneshot, then drive the PersistJob::Catchup task to
            // completion.
            let (persist_tx, mut persist_rx) = tokio::sync::oneshot::channel();
            let finalize = super::persist::CatchupFinalizer::deferred(persist_tx);
            let _result = self.catchup(CatchupTarget::Current, finalize).await?;
            if let Ok(persist_data) = persist_rx.try_recv() {
                let seq = persist_data.header.ledger_seq;
                let pending = super::persist::spawn_persist_task(
                    super::persist::PersistJob::Catchup {
                        data: Box::new(persist_data),
                        db: self.db.clone(),
                        ledger_manager: self.ledger_manager.clone(),
                    },
                    seq,
                );
                // Drive the persist task to completion before continuing.
                // The persist task aborts the process on failure, so we only
                // observe success here.
                if let Err(e) = pending.handle.await {
                    anyhow::bail!("startup catchup persist task failed: {e}");
                }
            }
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
                let (_tx, rx) = tokio::sync::mpsc::unbounded_channel::<OverlayMessage>();
                rx
            }
        };

        // Take the verified-SCP-envelope receiver from the herder. The
        // verifier worker is a core component — if it failed to spawn,
        // Herder::build would have panicked. `take_verified_rx` must
        // succeed exactly once.
        let mut verified_rx = self
            .herder
            .take_verified_rx()
            .expect("scp-verify verified_rx must be taken exactly once at startup");

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
            let mut buffer =
                tracked_lock::tracked_write("syncing_ledgers", &self.syncing_ledgers).await;
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

        // In-progress background persist (DB writes + bucket flush).
        // Gated: next close won't start until persist completes.
        let mut pending_persist: Option<PendingPersist> = None;

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
                    let (persist_tx, mut persist_rx) = tokio::sync::oneshot::channel();
                    let success = self
                        .handle_close_complete(
                            pending,
                            join_result,
                            super::persist::LedgerCloseFinalizer::deferred(persist_tx),
                        )
                        .await;
                    // Chain persist and next close if successful.
                    if success {
                        // Track the deferred persist task. Deferred always
                        // sends on success (see handle_close_complete
                        // dispatch at ledger_close.rs); the `try_recv` is
                        // non-blocking because the send already happened
                        // synchronously inside handle_close_complete.
                        debug_assert!(
                            pending_persist.is_none(),
                            "new close persist while previous persist still pending"
                        );
                        match persist_rx.try_recv() {
                            Ok(pt) => {
                                pending_persist = Some(pt);
                            }
                            Err(e) => {
                                tracing::error!(
                                    ?e,
                                    "persist_rx empty after successful close — unreachable"
                                );
                                debug_assert!(false, "success without persist send");
                            }
                        }

                        // Publish queued history checkpoints (if any).
                        self.maybe_publish_history().await;

                        // Trigger consensus immediately after a successful close.
                        if self.is_validator {
                            self.try_trigger_consensus().await;
                        }

                        // Drain SCP + fetch response channels.
                        // Timed (#1759 diagnostics): if either drain takes
                        // >= SLOW_OP_THRESHOLD, emit a WARN naming the arm
                        // and the number of items handled.
                        let scp_drain_start = std::time::Instant::now();
                        let mut scp_drained: u64 = 0;
                        for _ in 0..MAX_DRAIN_PER_TICK {
                            match scp_message_rx.try_recv() {
                                Ok(scp_msg) => {
                                    self.pump_scp_intake(scp_msg, &mut verified_rx).await;
                                    scp_drained += 1;
                                }
                                Err(_) => break,
                            }
                        }
                        super::warn_if_slow(
                            scp_drain_start.elapsed(),
                            "post_close_scp_drain",
                            scp_drained,
                        );
                        let fetch_drain_start = std::time::Instant::now();
                        let mut fetch_drained: u64 = 0;
                        for _ in 0..MAX_DRAIN_PER_TICK {
                            match fetch_response_rx.try_recv() {
                                Ok(fetch_msg) => {
                                    self.decrement_fetch_channel_depth();
                                    self.handle_overlay_message(fetch_msg).await;
                                    fetch_drained += 1;
                                }
                                Err(_) => break,
                            }
                        }
                        super::warn_if_slow(
                            fetch_drain_start.elapsed(),
                            "post_close_fetch_drain",
                            fetch_drained,
                        );
                        if pending_catchup.is_none() {
                            if let Some(pc) = self.process_externalized_slots().await {
                                pending_catchup = Some(pc);
                            }
                        }

                        // Don't start the next close here — wait for
                        // pending_persist to complete first. This ensures
                        // the DB has the previous ledger's data before the
                        // next close references it. `pending_persist` is
                        // always Some on success (handle_close_complete's
                        // Err arm at line 1924 calls fatal_persist_error),
                        // so starting the next close here is unreachable.
                        // `finish_rapid_close_cycle` is also unreachable
                        // here — it fires from the `persist_result` arm
                        // once the deferred persist completes.
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
                                self.recovery_attempts_without_progress
                                    .store(1, Ordering::SeqCst);
                                self.sync_recovery_pending.store(true, Ordering::SeqCst);
                            }

                            // Spawn catchup persist task on a blocking thread.
                            // Dispatched from the event loop (not inside the catchup
                            // task) to avoid nested spawn_blocking (#1713, #1735).
                            if let Some(persist_data) = result.persist_data {
                                let seq = persist_data.header.ledger_seq;
                                debug_assert!(
                                    pending_persist.is_none(),
                                    "new catchup persist while previous persist still pending"
                                );
                                pending_persist =
                                    Some(super::persist::spawn_persist_task(
                                        super::persist::PersistJob::Catchup {
                                            data: Box::new(persist_data),
                                            db: self.db.clone(),
                                            ledger_manager: self.ledger_manager.clone(),
                                        },
                                        seq,
                                    ));
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

                    // Kick off first buffered close — but only if no persist
                    // is pending (it will start close when persist completes).
                    if pending_close.is_none() && pending_persist.is_none() {
                        pending_close = self.try_start_ledger_close().await;
                    }
                }

                // Await deferred persist completion.
                // Once the DB writes and bucket flush finish, we can start
                // the next ledger close.
                persist_result = async {
                    match pending_persist.as_mut() {
                        Some(p) => (&mut p.handle).await,
                        None => std::future::pending().await,
                    }
                } => {
                    let persist = pending_persist.take().unwrap();
                    if let Err(e) = persist_result {
                        tracing::error!(
                            error = %e,
                            ledger_seq = persist.ledger_seq,
                            "Persist task panicked"
                        );
                        std::process::abort();
                    }
                    tracing::debug!(
                        ledger_seq = persist.ledger_seq,
                        "Persist completed, starting next close"
                    );

                    // Now start the next close (persist is done, DB is up to date).
                    if pending_close.is_none() {
                        pending_close = self.try_start_ledger_close().await;

                        // If no more closes ready, rapid close cycle ended.
                        if pending_close.is_none() {
                            self.finish_rapid_close_cycle().await;
                        }
                    }
                }

                // Process verified SCP envelopes from the dedicated verifier
                // worker thread (issue #1734 Phase B). Placed alongside the
                // overlay channels — NOT biased — so timers and other intake
                // stay fair under verified-backlog bursts.
                Some(ve) = verified_rx.recv() => {
                    self.set_phase(32); // 32 = scp_verified
                    tracing::trace!(select_iteration, "BRANCH: verified_rx");
                    self.process_verified(ve).await;
                    self.scp_verify_output_backlog
                        .store(verified_rx.len() as u64, Ordering::Relaxed);
                    if pending_close.is_none() && pending_catchup.is_none() {
                        pending_close = self.try_start_ledger_close().await;
                    }
                    tracing::trace!(select_iteration, "BRANCH: verified_rx done");
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
                    self.pump_scp_intake(scp_msg, &mut verified_rx).await;
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
                    self.decrement_fetch_channel_depth();
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

                    // Drain dedicated SCP channel first (highest priority).
                    // Timed (#1759 diagnostics).
                    let scp_drain_start = std::time::Instant::now();
                    let mut scp_drained: u64 = 0;
                    for _ in 0..MAX_DRAIN_PER_TICK {
                        match scp_message_rx.try_recv() {
                            Ok(scp_msg) => {
                                self.pump_scp_intake(scp_msg, &mut verified_rx).await;
                                scp_drained += 1;
                            }
                            Err(_) => break,
                        }
                    }
                    super::warn_if_slow(
                        scp_drain_start.elapsed(),
                        "consensus_tick_scp_drain",
                        scp_drained,
                    );

                    // Drain dedicated fetch response channel (tx_sets, dont_have, etc.).
                    // Timed (#1759 diagnostics).
                    let fetch_drain_start = std::time::Instant::now();
                    let mut fetch_drained: u64 = 0;
                    for _ in 0..MAX_DRAIN_PER_TICK {
                        match fetch_response_rx.try_recv() {
                            Ok(fetch_msg) => {
                                self.decrement_fetch_channel_depth();
                                self.handle_overlay_message(fetch_msg).await;
                                fetch_drained += 1;
                            }
                            Err(_) => break,
                        }
                    }
                    super::warn_if_slow(
                        fetch_drain_start.elapsed(),
                        "consensus_tick_fetch_drain",
                        fetch_drained,
                    );

                    // Check if SyncRecoveryManager requested recovery
                    if pending_catchup.is_none()
                        && self.sync_recovery_pending.swap(false, Ordering::SeqCst)
                    {
                        tracing::debug!("Sync recovery requested, starting recovery");
                        // SyncRecoveryManager triggered recovery - perform it now
                        if let Ok(current_ledger) = self.get_current_ledger().await {
                            tracing::debug!(current_ledger, "Calling out_of_sync_recovery");
                            pending_catchup =
                                self.out_of_sync_recovery(current_ledger).await;
                            tracing::debug!("out_of_sync_recovery completed");
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

        // Drain the close pipeline before shutdown (parity: stellar-core
        // joins the ledger-close thread first in idempotentShutdown).
        self.drain_close_pipeline(&mut pending_persist, &mut pending_close)
            .await;

        self.set_state(AppState::ShuttingDown).await;
        self.shutdown_internal().await?;

        Ok(())
    }

    /// Reset state after a rapid close cycle ends (no more closes or persists pending).
    ///
    /// Called when we've finished draining all buffered closes and the DB is
    /// fully up to date. Requests fresh SCP state from peers to resume normal
    /// consensus participation.
    async fn finish_rapid_close_cycle(&self) {
        let current_ledger = self.current_ledger_seq();
        *self.last_externalized_at.write().await = self.clock.now();
        self.reset_tx_set_tracking().await;
        *self.consensus_stuck_state.write().await = None;
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

        let mut overlay = OverlayManager::new_with_fetch_metrics(
            overlay_config,
            local_node,
            Arc::clone(&self.overlay_connection_factory),
            Arc::clone(&self.fetch_channel_depth),
            Arc::clone(&self.fetch_channel_depth_max),
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
            StellarMessage::ScpMessage(_) => {
                // SCP envelopes are routed through the dedicated scp_message_rx
                // channel (issue #1734 Phase B): the main loop admits them via
                // `pump_scp_intake`, which pre-filters and dispatches to the
                // dedicated verifier worker. If one reaches this legacy path
                // via the generic broadcast channel, it is a bug — the main
                // select! arms currently skip SCP on that channel. Log and
                // drop rather than silently re-verifying on the event loop.
                tracing::warn!(
                    peer = %msg.from_peer,
                    "SCP envelope reached generic overlay handler; dropping \
                     (should arrive via dedicated SCP channel)"
                );
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

    /// Drain the close pipeline on shutdown.
    ///
    /// stellar-core parity: `idempotentShutdown()` joins the ledger-close
    /// thread first before tearing down subsystems.
    ///
    /// Order matters:
    /// 1. Drain `pending_persist` first — a prior close's (or catchup's) DB
    ///    writes may be in-flight.  Persist panics abort the process, matching
    ///    the normal event-loop behavior (`lifecycle.rs` persist-complete arm).
    /// 2. Drain `pending_close` — await the `spawn_blocking` task, then call
    ///    `handle_close_complete()` with `LedgerCloseFinalizer::inline()` so
    ///    the close's own persist runs to completion before we return.
    pub(super) async fn drain_close_pipeline(
        &self,
        pending_persist: &mut Option<super::types::PendingPersist>,
        pending_close: &mut Option<super::types::PendingLedgerClose>,
    ) {
        // 1. Drain prior persist (abort on panic, matching the normal path).
        if let Some(persist) = pending_persist.take() {
            tracing::info!(
                ledger_seq = persist.ledger_seq,
                "Awaiting pending persist on shutdown"
            );
            if let Err(e) = persist.handle.await {
                tracing::error!(
                    error = %e,
                    ledger_seq = persist.ledger_seq,
                    "Persist task panicked during shutdown"
                );
                std::process::abort();
            }
        }

        // 2. Drain pending close (parity: stellar-core joins ledger-close
        //    thread first in idempotentShutdown).
        if let Some(mut pending) = pending_close.take() {
            tracing::info!(
                ledger_seq = pending.ledger_seq,
                "Awaiting pending ledger close on shutdown"
            );
            let join_result = (&mut pending.handle).await;
            // Use inline finalizer — persist must complete before
            // shutdown_internal tears down the database.
            let _ = self
                .handle_close_complete(
                    pending,
                    join_result,
                    super::persist::LedgerCloseFinalizer::inline(),
                )
                .await;
        }
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

    // ---------------------------------------------------------------
    // SCP envelope pipeline (issue #1734 Phase B)
    // ---------------------------------------------------------------

    /// Maximum number of verified envelopes drained per `pump_scp_intake`
    /// call before yielding to the outer select! (so timers and intake
    /// from other channels stay responsive under verified-backlog bursts).
    const VERIFIED_DRAIN_BUDGET: usize = 32;

    /// Pure helper: drain up to `budget` already-queued envelopes from an
    /// unbounded verified-output channel via non-blocking `try_recv`, calling
    /// `f` on each. Returns the number of envelopes drained.
    ///
    /// This helper does NOT await incoming envelopes; it only consumes what
    /// is immediately available. The real `pump_scp_intake` uses a biased
    /// `select!` that additionally awaits `verified_rx.recv()` while it
    /// waits for verifier-channel capacity — which `drain_verified_bounded`
    /// does not model. The helper exists to make the "stop at budget"
    /// invariant unit-testable without spinning up an App.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(super) async fn drain_verified_bounded<F, Fut>(
        verified_rx: &mut tokio::sync::mpsc::UnboundedReceiver<
            henyey_herder::scp_verify::VerifiedEnvelope,
        >,
        budget: usize,
        mut f: F,
    ) -> usize
    where
        F: FnMut(henyey_herder::scp_verify::VerifiedEnvelope) -> Fut,
        Fut: std::future::Future<Output = ()>,
    {
        let mut drained = 0;
        while drained < budget {
            match verified_rx.try_recv() {
                Ok(ve) => {
                    f(ve).await;
                    drained += 1;
                }
                Err(_) => break,
            }
        }
        drained
    }

    /// Drain one new SCP message into the dedicated verifier worker while
    /// opportunistically draining already-verified envelopes from the
    /// worker's output channel.
    ///
    /// On entry this method has an [`OverlayMessage`] that the main loop
    /// already decided is an SCP envelope. The helper:
    ///
    /// 1. Pre-filters the envelope (cheap state gates) on the event loop.
    /// 2. Reserves capacity in the verifier queue via `Sender::reserve().await`
    ///    — this is the backpressure point: the event loop parks here rather
    ///    than dropping envelopes when the worker is saturated.
    /// 3. While waiting for capacity, pulls up to [`VERIFIED_DRAIN_BUDGET`]
    ///    verified envelopes from `verified_rx` via a biased inner select
    ///    (the bias is *local* to this helper — the outer select! remains
    ///    non-biased, preserving timer fairness).
    ///
    /// Envelopes the pre-filter rejects never reach the worker.
    pub(super) async fn pump_scp_intake(
        &self,
        scp_msg: OverlayMessage,
        verified_rx: &mut tokio::sync::mpsc::UnboundedReceiver<
            henyey_herder::scp_verify::VerifiedEnvelope,
        >,
    ) {
        use henyey_herder::scp_verify::PreFilter;

        // Phase 31 marks time spent in this helper: pre-filtering, reserving
        // verifier-queue capacity (the backpressure park point), and draining
        // verified envelopes interleaved with that wait. The watchdog uses
        // this to distinguish "stuck waiting for the verify worker" from
        // "stuck inside select!".
        self.set_phase(31); // 31 = scp_verifier
        self.scp_verify_output_backlog
            .store(verified_rx.len() as u64, Ordering::Relaxed);

        let envelope = match scp_msg.message {
            StellarMessage::ScpMessage(e) => e,
            other => {
                tracing::warn!("pump_scp_intake called with non-SCP message: {other:?}");
                return;
            }
        };

        let from_peer = scp_msg.from_peer;
        let verifier = self.herder.scp_verifier_handle();

        let mut drained: usize = 0;
        loop {
            tokio::select! {
                biased;

                Some(ve) = verified_rx.recv(), if drained < Self::VERIFIED_DRAIN_BUDGET => {
                    self.process_verified(ve).await;
                    drained += 1;
                    // `process_verified` set phase=32; restore the pump's
                    // phase so the watchdog sees us back in "waiting on
                    // verifier reserve" while we loop.
                    self.set_phase(31);
                    self.scp_verify_output_backlog
                        .store(verified_rx.len() as u64, Ordering::Relaxed);
                }

                permit_res = verifier.tx.reserve() => {
                    let permit = match permit_res {
                        Ok(p) => p,
                        Err(_closed) => {
                            tracing::error!(
                                "scp-verify worker channel closed (worker likely dead); \
                                 dropping envelope"
                            );
                            return;
                        }
                    };
                    // Time-wrapped (#1759 diagnostics): this is the
                    // event-loop-side pre-filter for every SCP envelope,
                    // acquiring `Herder::state` + `ScpDriver::externalized`
                    // (both parking_lot::RwLock) before handing off to
                    // the verify worker.
                    match tracked_lock::time_call(
                        "herder.pre_filter_scp_envelope",
                        || self.herder.pre_filter_scp_envelope(&envelope),
                    ) {
                        PreFilter::Accept(mut intake) => {
                            intake.peer_id = Some(from_peer);
                            permit.send(intake);
                        }
                        PreFilter::Reject(reason) => {
                            self.record_prefilter_reject(reason);
                            drop(permit);
                        }
                    }
                    return;
                }
            }
        }
    }

    fn record_prefilter_reject(&self, reason: henyey_herder::scp_verify::PreFilterRejectReason) {
        use henyey_herder::scp_verify::PreFilterRejectReason as R;
        let counter = match reason {
            R::CannotReceiveScp => &self.scp_prefilter_reject_cannot_receive,
            R::CloseTime => &self.scp_prefilter_reject_close_time,
            R::Range => &self.scp_prefilter_reject_range,
        };
        counter.fetch_add(1, Ordering::Relaxed);
    }

    /// Process a fully-verified envelope on the event loop, running the
    /// post-verify gates and side-effect block that used to live inline in
    /// `handle_overlay_message`'s SCP arm.
    pub(super) async fn process_verified(&self, ve: henyey_herder::scp_verify::VerifiedEnvelope) {
        use henyey_herder::scp_verify::Verdict;
        self.set_phase(32); // 32 = scp_verified

        let slot = ve.intake.slot;
        let tracking = self.herder.tracking_slot();
        let is_externalize = ve.intake.is_externalize;

        // Record verify latency (enqueue → post-verify dispatch) into the
        // poor-man's histogram (sum + count) so the average can be read
        // from the /metrics endpoint.
        let verify_latency_us = ve.intake.enqueue_at.elapsed().as_micros() as u64;
        self.scp_verify_latency_us_sum
            .fetch_add(verify_latency_us, Ordering::Relaxed);
        self.scp_verify_latency_count
            .fetch_add(1, Ordering::Relaxed);

        // SCP latency bookkeeping.
        //
        // IMPORTANT ordering: we intentionally record `first_seen` / the
        // self-to-other latency *here*, AFTER the worker has verified the
        // signature, rather than at overlay dispatch. This makes the
        // recorded latency reflect "user-visible processing" (time from
        // envelope admit to post-verify handling) including any time the
        // envelope spent queued on the verifier. Pre-verify bookkeeping
        // would undercount under verifier backpressure.
        {
            let mut latency = self.scp_latency.write().await;
            let now = self.clock.now();
            latency.record_first_seen(slot, now);
            if let Some(ms) = latency.record_other_after_self(slot, now) {
                let mut survey_data = self.survey_data.write().await;
                survey_data.record_scp_self_to_other_latency(ms);
            }
        }

        // Fast-path reject surfaced by the worker (invalid signature or
        // panic) — log, emit the same warning handle_overlay_message used
        // to emit, and skip the rest of the side-effect block.
        if !matches!(ve.verdict, Verdict::Ok) {
            let peer = ve
                .intake
                .peer_id
                .as_ref()
                .map(|p| format!("{}", p))
                .unwrap_or_else(|| "<unknown>".into());
            match ve.verdict {
                Verdict::InvalidSignature => {
                    tracing::warn!(slot, peer = %peer, "SCP envelope with invalid signature");
                }
                Verdict::Panic => {
                    tracing::error!(slot, peer = %peer, "SCP envelope verification panicked");
                }
                Verdict::Ok => unreachable!(),
            }
            // Feed into Herder so internal accounting stays consistent
            // (pre-filter drop reasons are not re-run here; the Herder's
            // `process_verified` handles the InvalidSignature/Panic cases
            // without running downstream logic).
            let (_, reason) = self.herder.process_verified(ve);
            self.record_post_verify_reason(reason);
            return;
        }

        let envelope = ve.intake.envelope.clone();
        let from_peer_opt = ve.intake.peer_id.clone();

        let tx_set_hash = if is_externalize {
            match &envelope.statement.pledges {
                stellar_xdr::curr::ScpStatementPledges::Externalize(ext) => {
                    match StellarValue::from_xdr(
                        &ext.commit.value.0,
                        stellar_xdr::curr::Limits::none(),
                    ) {
                        Ok(stellar_value) => Some(Hash256::from_bytes(stellar_value.tx_set_hash.0)),
                        Err(err) => {
                            tracing::warn!(
                                slot, error = %err,
                                "Failed to parse externalized StellarValue"
                            );
                            None
                        }
                    }
                }
                _ => None,
            }
        } else {
            None
        };

        let hash = henyey_common::scp_quorum_set_hash(&envelope.statement);
        let hash256 = henyey_common::Hash256::from_bytes(hash.0);
        let sender_node_id = envelope.statement.node_id.clone();

        // Hand off to Herder for gate recheck + self-message skip +
        // non-quorum reject + slot_quorum_tracker + prefetch + pending.add.
        let (envelope_result, reason) = self.herder.process_verified(ve);

        // Per-reason post-verify metric.
        self.record_post_verify_reason(reason);

        // Aggregate post-verify drop counter (backward compat): envelopes that
        // were accepted by pre_filter but dropped downstream.
        if matches!(
            envelope_result,
            EnvelopeState::TooOld | EnvelopeState::Invalid | EnvelopeState::InvalidSignature
        ) {
            self.scp_post_verify_drops.fetch_add(1, Ordering::Relaxed);
        }

        // Request quorum set only after Herder has validated the envelope.
        if matches!(
            envelope_result,
            EnvelopeState::Valid | EnvelopeState::Pending | EnvelopeState::Fetching
        ) {
            if self.herder.request_quorum_set(hash256, sender_node_id) {
                if let Some(peer) = from_peer_opt.as_ref() {
                    if let Some(overlay) = self.overlay().await {
                        let request =
                            StellarMessage::GetScpQuorumset(stellar_xdr::curr::Uint256(hash.0));
                        if let Err(e) = overlay.try_send_to(peer, request) {
                            tracing::debug!(peer = %peer, error = %e, "Failed to request quorum set");
                        }
                    }
                }
            }
        }

        match envelope_result {
            EnvelopeState::Valid => {
                tracing::debug!(slot, tracking, "SCP envelope accepted (Valid)");
                if is_externalize {
                    tracing::debug!(slot, tracking, "EXTERNALIZE Valid — processing slot");
                    if let Some(tx_set_hash) = tx_set_hash {
                        self.herder.scp_driver().request_tx_set(tx_set_hash, slot);
                        if let Some(peer) = from_peer_opt.as_ref() {
                            self.maybe_request_tx_set_from_peer(&tx_set_hash, peer)
                                .await;
                        }
                    }
                    if let Some(pc) = self.process_externalized_slots().await {
                        *self.deferred_catchup.lock().await = Some(pc);
                    }
                    self.request_pending_tx_sets().await;

                    let current_ledger = self.current_ledger_seq() as u64;
                    if slot > current_ledger + 1 {
                        self.sync_recovery_pending.store(true, Ordering::SeqCst);
                        if slot > current_ledger + 2 {
                            self.recovery_attempts_without_progress
                                .store(RECOVERY_ESCALATION_CATCHUP, Ordering::SeqCst);
                        }
                    }
                }
            }
            EnvelopeState::Pending => {
                tracing::debug!(slot, tracking, "SCP envelope buffered for future slot");
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
            EnvelopeState::Duplicate => {}
            EnvelopeState::TooOld => {
                tracing::debug!(slot, tracking, "SCP envelope rejected (TooOld)");
            }
            EnvelopeState::Invalid => {
                let peer_str = from_peer_opt
                    .as_ref()
                    .map(|p| format!("{p}"))
                    .unwrap_or_else(|| "<unknown>".into());
                tracing::debug!(slot, peer = %peer_str, "SCP envelope rejected (Invalid)");
            }
            EnvelopeState::InvalidSignature => {
                let peer_str = from_peer_opt
                    .as_ref()
                    .map(|p| format!("{p}"))
                    .unwrap_or_else(|| "<unknown>".into());
                tracing::warn!(slot, peer = %peer_str, "SCP envelope with invalid signature");
            }
            EnvelopeState::Fetching => {
                let peer_str = from_peer_opt
                    .as_ref()
                    .map(|p| format!("{p}"))
                    .unwrap_or_else(|| "<unknown>".into());
                tracing::debug!(
                    slot,
                    peer = %peer_str,
                    "SCP EXTERNALIZE waiting for tx set (Fetching)"
                );
                if let Some(tx_set_hash) = tx_set_hash {
                    if let Some(peer) = from_peer_opt.as_ref() {
                        if let Some(overlay) = self.overlay().await {
                            let request =
                                StellarMessage::GetTxSet(stellar_xdr::curr::Uint256(tx_set_hash.0));
                            if let Err(e) = overlay.try_send_to(peer, request) {
                                tracing::debug!(
                                    peer = %peer,
                                    error = %e,
                                    "Failed to request tx set for fetching envelope"
                                );
                            }
                        }
                    }
                }
                self.request_pending_tx_sets().await;
            }
        }
    }

    /// Increment the per-reason post-verify counter.
    fn record_post_verify_reason(&self, reason: henyey_herder::scp_verify::PostVerifyReason) {
        use henyey_herder::scp_verify::PostVerifyReason;
        let counter = match reason {
            PostVerifyReason::GateDriftRange => &self.scp_pv_drift_range,
            PostVerifyReason::GateDriftCloseTime => &self.scp_pv_drift_close_time,
            PostVerifyReason::GateDriftCannotReceive => &self.scp_pv_drift_cannot_receive,
            PostVerifyReason::SelfMessage => &self.scp_pv_self_message,
            PostVerifyReason::NonQuorum => &self.scp_pv_non_quorum,
            PostVerifyReason::PendingAddTooFar => &self.scp_pv_too_far,
            PostVerifyReason::PendingAddBufferFull => &self.scp_pv_buffer_full,
            PostVerifyReason::Accepted => &self.scp_pv_accepted,
            PostVerifyReason::PendingAddBuffered => &self.scp_pv_buffered,
            PostVerifyReason::PendingAddDuplicate => &self.scp_pv_duplicate,
            PostVerifyReason::PendingAddProcessedDirectly => &self.scp_pv_processed_directly,
            PostVerifyReason::InvalidSignature => &self.scp_pv_invalid_sig,
            PostVerifyReason::PanicVerdict => &self.scp_pv_panic,
            _ => return, // #[non_exhaustive] future variants
        };
        counter.fetch_add(1, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod pump_tests {
    use super::App;
    use henyey_herder::scp_verify::{PipelinedIntake, Verdict, VerifiedEnvelope};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Instant;
    use stellar_xdr::curr::{
        NodeId, PublicKey as XdrPublicKey, ScpBallot, ScpEnvelope, ScpStatement,
        ScpStatementPledges, ScpStatementPrepare, Signature, Uint256, Value,
    };

    fn ve(slot: u64) -> VerifiedEnvelope {
        let node_id = NodeId(XdrPublicKey::PublicKeyTypeEd25519(Uint256([1u8; 32])));
        let value = Value(vec![].try_into().unwrap());
        let pledges = ScpStatementPledges::Prepare(ScpStatementPrepare {
            quorum_set_hash: stellar_xdr::curr::Hash([0u8; 32]),
            ballot: ScpBallot {
                counter: 1,
                value: value.clone(),
            },
            prepared: None,
            prepared_prime: None,
            n_c: 0,
            n_h: 0,
        });
        let statement = ScpStatement {
            node_id,
            slot_index: slot,
            pledges,
        };
        VerifiedEnvelope {
            intake: PipelinedIntake {
                envelope: ScpEnvelope {
                    statement,
                    signature: Signature(vec![0u8; 64].try_into().unwrap()),
                },
                slot,
                is_externalize: false,
                peer_id: None,
                enqueue_at: Instant::now(),
            },
            verdict: Verdict::Ok,
        }
    }

    /// Seed the channel with 100 envelopes and call `drain_verified_bounded`
    /// with budget 32. Exactly 32 must be drained; 68 must remain.
    #[tokio::test]
    async fn test_pump_bounded_drain() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<VerifiedEnvelope>();
        for i in 0..100 {
            tx.send(ve(i)).unwrap();
        }

        let seen = Arc::new(AtomicUsize::new(0));
        let seen_clone = Arc::clone(&seen);
        let drained = App::drain_verified_bounded(&mut rx, 32, |_ve| {
            let s = Arc::clone(&seen_clone);
            async move {
                s.fetch_add(1, Ordering::SeqCst);
            }
        })
        .await;

        assert_eq!(drained, 32, "must stop at budget");
        assert_eq!(seen.load(Ordering::SeqCst), 32, "callback ran 32 times");
        assert_eq!(rx.len(), 68, "68 envelopes must remain queued");
    }

    /// When fewer than `budget` envelopes are queued, drain all of them and
    /// return without blocking.
    #[tokio::test]
    async fn test_pump_drain_stops_on_empty() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<VerifiedEnvelope>();
        for i in 0..5 {
            tx.send(ve(i)).unwrap();
        }
        let drained = App::drain_verified_bounded(&mut rx, 32, |_ve| async {}).await;
        assert_eq!(drained, 5);
        assert_eq!(rx.len(), 0);
    }
}
