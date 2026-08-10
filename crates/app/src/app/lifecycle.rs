//! Application lifecycle: overlay message handling, periodic tick, and event loop orchestration.

use super::*;

/// Name of the structured tracing field emitted by
/// [`App::trigger_fatal_shutdown`].
///
/// This field is **reserved exclusively** for the fatal-shutdown-with-wipe
/// path.  No other code path should emit an event with this field name.
///
/// External monitoring tools (e.g. monitor-tick) grep rendered log output
/// for this field to detect fatal state corruption requiring a data wipe.
/// The constant is a documentation anchor — the real mechanical guard is
/// the `test_fatal_shutdown_emits_wipe_field_*` tests.  **Do not rename
/// this field without updating the tests and all monitoring consumers.**
#[cfg(test)]
pub(crate) const FATAL_WIPE_FIELD: &str = "fatal_wipe_required";

/// Structured tracing field name for the summary heartbeat event.
///
/// External monitoring tools (e.g. monitor-tick) grep rendered log output
/// for this field to detect heartbeat presence without relying on fragile
/// prose-string matching.  This field is **reserved exclusively** for the
/// summary heartbeat event — do not add it to other code paths.
/// The `test_heartbeat_emits_field_*` tests guard the rendering contract.
/// **Do not rename this field without updating the tests and all monitoring
/// consumers.**
#[cfg(test)]
pub(crate) const HEARTBEAT_FIELD: &str = "heartbeat";

/// Emit the summary heartbeat log event with the `heartbeat = true` sentinel.
///
/// This is the **sole** emitter of the `heartbeat` structured field. External
/// monitoring tools grep for this field to detect heartbeat events. Extracting
/// the log call into a standalone function enables direct unit testing of the
/// monitoring contract without spinning up the full event loop.
#[allow(clippy::too_many_arguments)]
pub(crate) fn emit_heartbeat_log(
    tracking_slot: u64,
    ledger: u32,
    latest_ext: u64,
    peers: usize,
    heard_from_quorum: bool,
    is_v_blocking: bool,
    scp_total: u64,
    scp_since_last: u64,
    scp_silent_secs: u64,
    scp_sent: u64,
    scp_sent_nom: u64,
    scp_sent_prep: u64,
    scp_sent_conf: u64,
    scp_sent_ext: u64,
    peer_max_verified: u64,
    peer_gap: u64,
) {
    tracing::info!(
        heartbeat = true,
        tracking_slot,
        ledger,
        latest_ext,
        peers,
        heard_from_quorum,
        is_v_blocking,
        scp_total,
        scp_since_last,
        scp_silent_secs,
        scp_sent,
        scp_sent_nom,
        scp_sent_prep,
        scp_sent_conf,
        scp_sent_ext,
        peer_max_verified,
        peer_gap,
        "Heartbeat"
    );
}

/// Compute the query rate-limit window (parity: Peer.cpp:1426-1429).
///
/// Re-exported from the overlay's shared query policy module.
pub(crate) fn query_rate_limit_window(close_duration: Duration) -> Duration {
    henyey_overlay::query_policy::query_rate_limit_window(close_duration)
}

/// Offload one periodic tx-set GC purge onto the blocking thread pool, keeping
/// purges strictly serial via a loop-local in-flight handle (#3532).
///
/// The purge (`Herder::purge_persisted_tx_sets` → `BEGIN IMMEDIATE` read-then-
/// delete over the persisted-tx-set table) is a synchronous SQLite write
/// transaction. Running it inline in the tokio `select!` event loop froze the
/// loop for tens of seconds (observed 39s, `watchdog_freeze`) when the table
/// was large or the WAL write lock was contended, stalling SCP processing,
/// broadcast, and fetch-response draining (the climbing `fetch_channel_depth`
/// symptom). This moves the blocking work to `spawn_blocking` so the loop arm
/// returns immediately.
///
/// Parity: stellar-core's `HerderImpl::purgeOldPersistedTxSets()`
/// (HerderImpl.cpp:2448-2487) runs in an `async_wait` callback and reschedules
/// `startTxSetGCTimer()` (HerderImpl.cpp:2440-2444) only AFTER the purge
/// returns — i.e. purges never overlap. The `is_finished()` guard reproduces
/// that serial, non-overlapping cadence across the async boundary (the timer's
/// `MissedTickBehavior::Skip` only coalesces ticks, not in-flight tasks). The
/// purge *contents* are unchanged (same atomic SQL, #2770), and GC only deletes
/// orphaned/unreferenced persisted tx-sets — never ledger/consensus/bucket
/// state — so the thread it runs on is not observable and parity is preserved.
///
/// `slot` is the loop-local in-flight handle. If a prior purge is still
/// running, the tick is skipped (coalesced); otherwise a fresh blocking task is
/// spawned and its handle stored. The handle is fire-and-not-awaited: GC is
/// idempotent and periodic, so a skipped tick simply re-runs next interval, and
/// on shutdown the loop `abort()`s any in-flight handle (harmless — the purge
/// is a single atomic transaction).
fn dispatch_tx_set_gc<F>(work: F, slot: &mut Option<tokio::task::JoinHandle<()>>)
where
    F: FnOnce() + Send + 'static,
{
    // Serial cadence: skip if a prior purge is still running (mirrors
    // stellar-core rescheduling startTxSetGCTimer() only after the purge
    // returns — purges never overlap).
    if slot.as_ref().is_some_and(|h| !h.is_finished()) {
        return;
    }
    *slot = Some(tokio::task::spawn_blocking(work));
}

/// Offload one round of peer maintenance (phase=28) off the main `tokio::select!`
/// event loop into a coalesced, fire-and-not-awaited background task.
///
/// The phase=28 arm previously did `self.maintain_peers().await` *inside* the
/// `select!` body. A `select!` does not poll its other branches while the chosen
/// arm's future is awaiting, so that await blocked the entire loop. `maintain_peers`
/// first awaits `db_blocking("remove-failed-peers", …)`, whose pooled connection
/// has `PRAGMA busy_timeout = 30000`; under SQLite write-lock contention the
/// `DELETE FROM peers …` retries for up to 30 s — and the arm *also* awaits a 20 s
/// bounded reconnect phase. Both stalled the loop, starving SCP → lost sync
/// (#3689, recurrence of #3582 despite the #3598 fix being present).
///
/// This mirrors `dispatch_tx_set_gc` exactly: a loop-local in-flight `JoinHandle`
/// guard keeps runs strictly serial (skip this tick if a prior round is still
/// running — the timer's `MissedTickBehavior::Skip` only coalesces ticks, not
/// in-flight tasks), otherwise `tokio::spawn` the maintenance future and store the
/// handle. The future is `Send` (`maintain_peers` holds only `tokio::sync` async
/// guards + `Arc<OverlayManager>` across awaits — no `Rc`/`RefCell`/std-sync guard).
///
/// `slot` is the loop-local in-flight handle. The handle is fire-and-not-awaited:
/// peer maintenance is idempotent (`remove_peers_with_failures` is a single
/// `DELETE FROM peers WHERE numfailures >= ?`) and periodic, so a skipped tick
/// simply re-runs next interval, and on shutdown the loop `abort()`s any in-flight
/// handle (harmless — the only side effects are the idempotent DELETE and
/// best-effort reconnects). This bounds the loop's time in phase=28 to a
/// `tokio::spawn` (sub-millisecond) regardless of DB-lock or reconnect contention.
fn dispatch_peer_maintenance<F>(work: F, slot: &mut Option<tokio::task::JoinHandle<()>>)
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    // Serial cadence: skip if a prior maintenance round is still running.
    if slot.as_ref().is_some_and(|h| !h.is_finished()) {
        return;
    }
    *slot = Some(tokio::spawn(work));
}

/// Whether a transaction queue result requires removal of the FloodGate record,
/// allowing re-delivery to be treated as new.
///
/// Mirrors stellar-core: OverlayManagerImpl.cpp:1231-1236 — `forgetFloodedMsg`
/// is called when the add result is NOT `ADD_STATUS_PENDING` or `ADD_STATUS_DUPLICATE`.
/// Static label for the receive-result metric/log (diagnostic only).
fn tx_receive_result_label(result: &henyey_herder::TxQueueResult) -> &'static str {
    use henyey_herder::TxQueueResult;
    match result {
        TxQueueResult::Added => "added",
        TxQueueResult::Duplicate => "duplicate",
        TxQueueResult::QueueFull => "queue_full",
        TxQueueResult::FeeTooLow => "fee_too_low",
        TxQueueResult::Invalid(_) => "invalid",
        TxQueueResult::Banned => "banned",
        TxQueueResult::Filtered => "filtered",
        TxQueueResult::TryAgainLater => "try_again_later",
    }
}

fn should_forget_tx_flood_record(result: &henyey_herder::TxQueueResult) -> bool {
    use henyey_herder::TxQueueResult;
    match result {
        // Parity: ADD_STATUS_PENDING — keep flood record for relay accounting
        TxQueueResult::Added => false,
        // Parity: ADD_STATUS_DUPLICATE — keep flood record for relay accounting
        TxQueueResult::Duplicate => false,
        // All rejections: forget so redelivery is treated as new.
        //
        // TryAgainLater is henyey-specific (herder.rs:1894-1907 — the node
        // hasn't reached Tracking state yet). stellar-core does not gate on
        // tracking state, so this variant has no upstream analog. We still
        // forget because: (a) the tx was not accepted, (b) the peer should
        // be able to re-send once the node is tracking, and (c) keeping the
        // record provides no relay value since the tx won't be broadcast.
        TxQueueResult::QueueFull
        | TxQueueResult::FeeTooLow
        | TxQueueResult::Invalid(_)
        | TxQueueResult::Banned
        | TxQueueResult::Filtered
        | TxQueueResult::TryAgainLater => true,
    }
}

/// Whether a post-verify SCP outcome requires removal of the corresponding
/// FloodGate record, allowing re-delivery to be treated as new.
///
/// Mirrors stellar-core: Peer.cpp:1672-1678 calls `forgetFloodedMsg` when
/// `recvSCPEnvelope` returns `ENVELOPE_STATUS_DISCARDED`.
fn should_forget_flood_record(
    state: henyey_herder::EnvelopeState,
    reason: henyey_herder::scp_verify::PostVerifyReason,
) -> bool {
    use henyey_herder::scp_verify::PostVerifyReason as R;
    use henyey_herder::EnvelopeState;

    // Self-messages: stellar-core returns SKIPPED_SELF (not DISCARDED).
    // Keep the flood record for relay accounting.
    if matches!(reason, R::SelfMessage) {
        return false;
    }

    // Deferred (henyey-specific closing gate): will be replayed after
    // ledger_closed. Keep the flood record so relay tracking stays intact.
    if matches!(state, EnvelopeState::Deferred) {
        return false;
    }

    // Duplicate: stellar-core returns ENVELOPE_STATUS_PROCESSED (not
    // DISCARDED) — keep the flood record so relay accounting is preserved.
    // Discarded (standalone manual-close: manual_close + run_standalone):
    // stellar-core Peer.cpp:1672-1678 calls
    // forgetFloodedMsg on ENVELOPE_STATUS_DISCARDED.
    matches!(
        state,
        EnvelopeState::TooOld
            | EnvelopeState::InvalidSignature
            | EnvelopeState::Invalid
            | EnvelopeState::Discarded
    )
}

impl App {
    /// Run the main event loop.
    ///
    /// This starts all subsystems and runs until shutdown is signaled.
    ///
    /// `fallback_catchup` controls behavior when no ledger state is found:
    /// - [`FallbackCatchup::Allow`]: perform catchup from history archives (Full/Validator mode).
    /// - [`FallbackCatchup::Skip`]: proceed without catchup (Watcher mode).
    pub async fn run(&self, fallback_catchup: FallbackCatchup) -> anyhow::Result<()> {
        tracing::info!("Starting main event loop");
        if !self.config.store_rpc_data() {
            tracing::info!(
                "Per-tx RPC row storage disabled (validator without JSON-RPC): \
                 skipping transactions/events table writes at ledger close"
            );
        }

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
            match fallback_catchup {
                FallbackCatchup::Allow => {
                    // This shouldn't happen if run_cmd did catchup, but handle it
                    // as a safety net for Full/Validator mode.
                    tracing::info!("No ledger state, running catchup first");

                    // We're inside `App::run()` which is itself called from a
                    // `tokio::spawn` task (see run_cmd::run_node). Calling
                    // spawn_blocking here risks the deadlock class from #1713 if
                    // the blocking pool is saturated. Use the Deferred finalizer
                    // pattern exactly as the event-loop recovery path does.
                    let (persist_tx, mut persist_rx) = tokio::sync::oneshot::channel();
                    let finalize = super::persist::CatchupFinalizer::deferred(
                        self.db.clone(),
                        self.ledger_manager.clone(),
                        persist_tx,
                    );
                    // Honor the operator's configured CATCHUP_COMPLETE /
                    // CATCHUP_RECENT policy on this no-state-startup fallback,
                    // matching the explicit run_cmd path. See #2104.
                    let mode = self.live_catchup_mode();
                    let _result = self
                        .catchup_with_run_mode(
                            CatchupTarget::Current,
                            mode,
                            CatchupRunMode::Online,
                            finalize,
                        )
                        .await?;
                    if let Ok(ready) = persist_rx.try_recv() {
                        // Drive the persist task to completion before continuing.
                        // The persist task aborts the process on failure, so we
                        // only observe success here.
                        let pending = ready.spawn();
                        if let Err(e) = pending.handle.await {
                            anyhow::bail!("startup catchup persist task failed: {e}");
                        }
                    }

                    // Refresh overlay dynamic state after fallback catchup — the
                    // protocol may have advanced during catchup.
                    self.refresh_overlay_query_window().await;
                    self.refresh_max_tx_size_bytes().await;
                }
                FallbackCatchup::Skip => {
                    // Watcher mode: no persisted state, proceeding without catchup.
                    // The node will observe SCP/overlay traffic and receive its
                    // first ledger via the out-of-sync recovery path.
                    tracing::info!("Watcher mode: no persisted state, proceeding without catchup");
                }
            }
        }

        // Bootstrap herder with current ledger
        let ledger_seq = self.current_ledger_seq();
        *self.last_processed_slot.write().await = ledger_seq as u64;
        self.herder.start_syncing();
        self.herder.bootstrap(ledger_seq);
        tracing::info!(ledger_seq, "Herder bootstrapped");

        // Restore persisted SCP state for in-flight future slots so a node
        // recovering from a crash resumes local SCP tracking without waiting
        // for fresh network envelopes. Parity: stellar-core
        // `HerderImpl::start()` calls `restoreSCPState()` immediately after
        // `setTrackingSCPState(lcl, ..., true)` + `trackingHeartBeat()`
        // (HerderImpl.cpp:2455-2471) — and in henyey's split lifecycle
        // `bootstrap(lcl)` above is that tracking step. Restore therefore runs
        // here, AFTER bootstrap and BEFORE we request SCP state from peers.
        // Only future slots (`slot > lcl`) are replayed; see
        // `Herder::restore_persisted_scp_state` for the #2797-stall guard.
        self.herder.restore_persisted_scp_state(ledger_seq as u64);

        // Wire overlay tracking state to herder. The herder is now syncing,
        // so the overlay's maybe_drop_random_peer() should know the node is
        // tracking (parity: stellar-core Config::REALLY_DEAD_NUM_FAILURES_CUTOFF
        // peer rotation only fires when !isSynced).
        if let Some(overlay) = self.overlay().await {
            overlay.set_tracking(true);
            // Only mark synced if we have actual ledger state. No-state watchers
            // (ledger_seq == 0) aren't synced yet — they reach synced after first
            // successful catchup or watcher re-bootstrap.
            if ledger_seq > 0 {
                overlay.set_synced(true);
            }
        }

        // Populate the initial bucket snapshot for the query server.
        self.update_bucket_snapshot();

        // Signal query server readiness only when we have actual ledger state.
        // For no-state watchers (current_ledger == 0 + FallbackCatchup::Skip),
        // readiness is deferred until the first successful ledger close to avoid
        // serving 500 errors from empty bucket snapshots. Matches stellar-core's
        // `setReady()` which is only called after `loadLastKnownLedger()` succeeds.
        if self.ledger_manager.is_initialized() {
            self.query_is_ready
                .store(true, std::sync::atomic::Ordering::Release);
        }

        // Wait a short time for initial peer connections, then request SCP state
        self.clock.sleep(Duration::from_millis(500)).await;
        self.request_scp_state_and_record().await;

        // Set state based on validator mode. Gate the extension readiness
        // signal the same way as `overlay.set_synced` and query readiness
        // above: a no-state node (ledger_seq == 0, uninitialized ledger
        // manager) must not be reported operational to extensions — it has no
        // ledger state to act on. Readiness arrives with the first successful
        // catchup or live ledger close.
        if self.ledger_manager.is_initialized() && ledger_seq > 0 {
            self.restore_operational_state().await;
        } else {
            self.restore_app_state_without_readiness().await;
        }

        // Start sync recovery tracking to enable the consensus stuck timer
        self.start_sync_recovery_tracking();

        // Get message receiver from overlay
        let message_rx = self.overlay().await.map(|o| o.subscribe());

        let message_rx = match message_rx {
            Some(rx) => rx,
            None => {
                tracing::warn!("Overlay not started, running without network");
                // Create a dummy receiver that never receives
                let (tx, rx) = tokio::sync::broadcast::channel::<OverlayMessage>(1);
                drop(tx);
                rx
            }
        };

        // Dedicated inbound-flood consumer (maxtps iter 6): bulk overlay
        // traffic (Transaction / FloodAdvert / FloodDemand / peer chatter) is
        // consumed on its own spawned task instead of a main-select arm. At
        // the ~1470 tx/s ceiling this arm did ~20 s of inline work per 30 s
        // window (maxtps_loop: broadcast ≈ 0.5 ms × 36-44 k msgs), starving
        // SCP intake (median 92-109 ms channel wait) and the close/trigger
        // path. SCP envelopes and fetch responses keep their dedicated
        // channels + select arms; only the broadcast-channel firehose moves.
        // The task holds a Weak<App> (upgrade per message) so it never keeps
        // the App alive after shutdown; it exits on channel close or upgrade
        // failure and is aborted on loop exit.
        let flood_consumer_task = {
            let weak = { self.self_arc.read().await.clone() };
            tokio::spawn(Self::run_flood_consumer(weak, message_rx))
        };

        // Get dedicated SCP message receiver (never drops messages)
        let scp_message_rx = {
            match self.overlay().await {
                Some(o) => o.subscribe_scp().await,
                None => None,
            }
        };

        // The dummy fallback path is only taken when there is no overlay
        // (degraded/test mode). Use a bounded channel to match the production
        // SCP receiver type ([`henyey_overlay::SCP_CHANNEL_CAPACITY`]); the
        // capacity is irrelevant here since nothing ever sends on `_dummy_scp_tx`.
        // We retain the sender for the lifetime of `run()` (`_dummy_scp_tx`) so
        // the channel never closes: a closed `mpsc::Receiver` yields
        // `recv() == None`, which under the `Some(scp_msg) = …recv()` select arm
        // would leave the arm permanently disabled (matching the prior unbounded
        // behavior where `_tx` was likewise kept implicitly). Keeping the sender
        // alive preserves "this arm stays pending forever" rather than ever
        // observing a spurious close.
        let (mut scp_message_rx, _dummy_scp_tx) = match scp_message_rx {
            Some(rx) => (rx, None),
            None => {
                let (tx, rx) = tokio::sync::mpsc::channel::<OverlayMessage>(
                    henyey_overlay::SCP_CHANNEL_CAPACITY,
                );
                (rx, Some(tx))
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
                // Create a dummy receiver that never receives. Bounded to match
                // the real fetch channel type (#3661); capacity is irrelevant
                // since `_tx` is dropped immediately so the channel is closed.
                let (_tx, rx) = tokio::sync::mpsc::channel::<OverlayMessage>(
                    henyey_overlay::FETCH_CHANNEL_CAPACITY,
                );
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

        // Wire up the FetchingEnvelopes broadcast callback — the sole relay
        // path for all SCP envelopes. Fires when deps are resolved, either
        // immediately at recv_envelope or later at check_and_move_to_ready.
        // The callback is synchronous (Fn) so we bridge to the async event
        // loop via an unbounded channel.
        // Parity: stellar-core PendingEnvelopes::envelopeReady() broadcasts
        // once isFullyFetched() is true.
        let (fetching_relay_tx, mut fetching_relay_rx) =
            tokio::sync::mpsc::unbounded_channel::<henyey_herder::ScpRelayEnvelope>();
        self.herder.set_fetching_broadcast(move |relay_env| {
            let _ = fetching_relay_tx.send(relay_env);
        });

        // Main run loop
        let mut shutdown_rx = self.take_initial_shutdown_receiver().await;
        let mut consensus_interval = tokio::time::interval(Duration::from_secs(1));
        let mut stats_interval = tokio::time::interval(Duration::from_secs(30));
        stats_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut tx_advert_interval = tokio::time::interval(self.flood_tx_period());
        let mut tx_demand_interval = tokio::time::interval(self.flood_demand_period());
        let mut survey_interval = tokio::time::interval(Duration::from_secs(1));
        let mut survey_phase_interval = tokio::time::interval(Duration::from_secs(5));
        let mut survey_request_interval = tokio::time::interval(Duration::from_secs(1));
        let mut ping_interval = tokio::time::interval(Duration::from_secs(5));
        let mut peer_maintenance_interval = tokio::time::interval(Duration::from_secs(10));
        peer_maintenance_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut peer_refresh_interval = tokio::time::interval(Duration::from_secs(60));
        peer_refresh_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut herder_cleanup_interval = tokio::time::interval(Duration::from_secs(30));
        herder_cleanup_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        // Arm the first tick at +TX_SET_GC_DELAY_SECS rather than firing
        // immediately on entry, to mirror stellar-core's
        // VirtualTimer::expires_from_now(TX_SET_GC_DELAY) shape
        // (HerderImpl.cpp:2442). Functionally harmless either way (the purge
        // is idempotent and a no-op on an empty DB), but matches upstream
        // cadence exactly.
        let tx_set_gc_period = Duration::from_secs(henyey_herder::TX_SET_GC_DELAY_SECS);
        let mut tx_set_gc_interval = tokio::time::interval_at(
            tokio::time::Instant::now() + tx_set_gc_period,
            tx_set_gc_period,
        );
        tx_set_gc_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        // Loop-local in-flight handle for the offloaded tx-set GC purge (#3532).
        // The purge runs on the blocking pool via `spawn_blocking`; this handle
        // keeps purges strictly serial (skip a tick if the prior purge is still
        // running) and is aborted on shutdown. See `dispatch_tx_set_gc`.
        let mut tx_set_gc_task: Option<tokio::task::JoinHandle<()>> = None;
        // Loop-local in-flight handle for offloaded peer maintenance (phase=28,
        // #3689). `maintain_peers` awaits `db_blocking("remove-failed-peers")`
        // (busy_timeout up to 30s under write-lock contention) plus a 20s
        // reconnect; running it inline inside the `select!` arm froze the loop and
        // dropped the validator out of sync. It is offloaded via
        // `dispatch_peer_maintenance`, which keeps rounds strictly serial (skip a
        // tick if a prior round is still running) and is aborted on shutdown.
        let mut peer_maintenance_task: Option<tokio::task::JoinHandle<()>> = None;

        // In-flight guard for the offloaded batched tx-advert flush (maxtps
        // iter 6). Same coalesced serial pattern as peer maintenance above:
        // one flush at a time, skipped ticks re-run next period, aborted on
        // shutdown (the flush is periodic + idempotent-enough: an aborted
        // flush leaves un-adverted hashes queued for the next period).
        let mut tx_advert_flush_task: Option<tokio::task::JoinHandle<()>> = None;

        // Get mutable access to SCP envelope receiver
        let mut scp_rx = self.scp_envelope_rx.lock().await;

        // Get mutable access to SCP timer event receiver
        let mut scp_timer_rx = self.scp_timer_rx.lock().await;

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

        // Cold-start arm of the event-driven consensus trigger (#2702). Runs
        // after the operational-state restore above (which set herder tracking state)
        // and after the pre-loop process_externalized_slots(), so by now a
        // tracking validator has a stable LCL to schedule the next ledger from.
        // Self-gates on is_tracking()/manual_close; if not yet tracking, it is a
        // no-op and the post-close path / 1-second tick safety-net arm it later.
        self.setup_trigger_next_ledger().await;

        tracing::info!("Entering main event loop");

        // Start the std::thread watchdog (independent of tokio runtime).
        // The guard's Drop signals the watchdog to exit, covering all exit
        // paths (normal shutdown, task abort, panic unwind).
        let watchdog_guard = self.start_event_loop_watchdog();

        let mut heartbeat_interval = tokio::time::interval(Duration::from_secs(10));
        heartbeat_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        // `scp_messages_received` is post-cache (bumped in `pump_scp_intake`);
        // `last_scp_message_at` stays pre-cache so in-flight duplicates still
        // count as liveness for the no-quorum warn.
        let mut scp_messages_last_heartbeat: u64 = 0;
        let mut last_scp_message_at = self.clock.now();

        // Close + persist pipeline state machine. See close_pipeline.rs.
        let mut close_pipeline = super::close_pipeline::ClosePipeline::new();

        // Maximum messages to drain from SCP/fetch channels per tick.
        // On mainnet with 24+ validators, SCP messages can arrive faster
        // than they're processed.  An unbounded drain starves everything
        // else in the tick (sync recovery, consensus trigger, tx_set requests).
        const MAX_DRAIN_PER_TICK: usize = 200;

        let mut select_iteration: u64 = 0;
        // Loop-top anchor for loop-side exact stall accounting (#3795). Same
        // anchor as `tick_event_loop()` (and therefore `last_event_loop_tick_ms`),
        // so the loop-side number and the sampler's `stale_secs` are the same
        // quantity measured two ways. Measured at loop top — NOT reusing
        // `phase_dispatch_start` — because that starts after the
        // `deferred_catchup.lock().await` preamble, a tokio Mutex acquire that
        // can itself block and would be invisible to a later anchor.
        let mut last_tick_at = std::time::Instant::now();
        loop {
            select_iteration += 1;
            // Report the exact inter-tick gap BEFORE `set_phase(0)` clears the
            // sub-phase, so the just-ran arm is attributed exactly. Complete by
            // construction: every stall that ends produces exactly one report
            // at its true duration, independent of the sampler's phase (#3795).
            let now = std::time::Instant::now();
            let gap = now.duration_since(last_tick_at);
            last_tick_at = now;
            self.report_event_loop_stall(gap);
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
            // Generic per-phase loop guard (#3582). Time the whole branch
            // dispatch; after the select! completes, the branch has already
            // stamped its coarse `phase` via `set_phase(N)`, so reading
            // `event_loop_phase` names exactly which branch ran. This logs
            // whichever phase (28 peer_maintenance, 5 consensus_tick, or any
            // other) holds up the loop > threshold — resolving the
            // phase-number ambiguity in #3582 on the deployed node. Additive
            // and behavior-preserving: one `Instant` + one `Duration` compare,
            // no control-flow change. Complements the finer phase=5 sub-step
            // WARNs below.
            let phase_dispatch_start = std::time::Instant::now();
            tokio::select! {
                // NOTE: Removed biased; to ensure timers get fair polling

                // Await pending ledger close or persist completion.
                // The pipeline is a state machine: at most one operation is
                // active. poll_completion() awaits whichever is in progress,
                // or pends forever if idle (letting other branches fire).
                pipeline_event = close_pipeline.poll_completion() => {
                    match pipeline_event {
                        super::close_pipeline::PipelineEvent::CloseComplete(join_result) => {
                    let join_result = *join_result;
                    self.set_phase(6); // 6 = pending_close
                    // Phase 6: Record close-cycle metric (deferred pipeline only).
                    {
                        let mut last_start = self.close_cycle_last_start.lock();
                        if let Some(prev) = *last_start {
                            crate::metrics::CLOSE_CYCLE_SECONDS
                                .record(prev.elapsed().as_secs_f64());
                        }
                        *last_start = Some(std::time::Instant::now());
                    }
                    tracing::debug!(select_iteration, "BRANCH: pending_close completed");
                    let pending = close_pipeline.take_close();
                    // Close-cycle decomposition (#1909): dispatch-to-join latency.
                    crate::metrics::CLOSE_DISPATCH_TO_JOIN_SECONDS
                        .record(pending.dispatch_time.elapsed().as_secs_f64());
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
                        // Close-cycle decomposition (#1909): post-close lifecycle work.
                        let post_complete_start = std::time::Instant::now();

                        // Track the deferred persist task. Deferred always
                        // sends on success (see handle_close_complete
                        // dispatch at ledger_close.rs); the `try_recv` is
                        // non-blocking because the send already happened
                        // synchronously inside handle_close_complete.
                        match persist_rx.try_recv() {
                            Ok(pt) => {
                                close_pipeline.start_persist(pt);
                            }
                            Err(e) => {
                                tracing::error!(
                                    ?e,
                                    "persist_rx empty after successful close — unreachable"
                                );
                                panic!("success without persist send");
                            }
                        }

                        // Publish queued history checkpoints (if any).
                        self.set_phase_sub(super::phase::PHASE_6_9_MAYBE_PUBLISH_HISTORY);
                        self.maybe_publish_history().await;

                        // Arm the event-driven trigger for the *next* ledger
                        // (#2702/#3014). A close just advanced LCL, so schedule
                        // the next nomination via a single-shot
                        // `TriggerNextLedger` timer. This is the henyey analog of
                        // stellar-core's `lastClosedLedgerIncreased` →
                        // `setupTriggerNextLedger` (HerderImpl.cpp:1218-1233),
                        // which is **arm-only**: it does NOT call
                        // `try_trigger_consensus()` synchronously here. The
                        // immediate fire happens via the timer's
                        // `triggerTime < now` clamp (in steady state the delay
                        // clamps to ~0, so the timer fires on the next event-loop
                        // iteration), and the timer-fire path carries the
                        // close-pipeline pump signal the inline call lacked
                        // (preserving the solo self-externalize cold-start). Both
                        // validators and watchers participate via the fired timer
                        // (parity: HERDER §5.2). The 1-second maintenance tick
                        // remains as the safety-net backstop.
                        self.set_phase_sub(super::phase::PHASE_6_10_TRY_TRIGGER_CONSENSUS);
                        self.setup_trigger_next_ledger().await;

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
                        self.set_phase_sub(super::phase::PHASE_6_11_FETCH_DRAIN);
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
                            self.set_phase_sub(super::phase::PHASE_6_12_PROCESS_EXTERNALIZED_SLOTS);
                            if let Some(pc) = self.process_externalized_slots().await {
                                pending_catchup = Some(pc);
                            }
                        }

                        // Close-cycle decomposition (#1909): record post-complete duration.
                        // Only recorded on the success path (inside `if success` branch).
                        crate::metrics::CLOSE_POST_COMPLETE_SECONDS
                            .record(post_complete_start.elapsed().as_secs_f64());

                        // Don't start the next close here — wait for
                        // persist to complete first. This ensures the DB
                        // has the previous ledger's data before the next
                        // close references it. The pipeline is now in
                        // Persisting state (start_persist above), so
                        // is_idle() returns false and no close can start.
                        // `finish_rapid_close_cycle` fires from the
                        // PersistComplete arm once persist completes.
                    }
                        }
                        super::close_pipeline::PipelineEvent::PersistComplete(persist_result) => {
                    let persist = close_pipeline.take_persist();
                    // Persist-cycle decomposition (#1916): dispatch-to-join latency.
                    crate::metrics::PERSIST_DISPATCH_TO_JOIN_SECONDS
                        .record(persist.dispatch_time.elapsed().as_secs_f64());
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
                    if close_pipeline.is_idle() {
                        let next = self.try_start_ledger_close().await;
                        close_pipeline.try_start_close(next);

                        // If no more closes ready, rapid close cycle ended.
                        if close_pipeline.is_idle() {
                            self.finish_rapid_close_cycle().await;
                        }
                    }
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
                        Ok(mut result) => {
                            // Take persist_ready before moving result.result
                            let persist_ready = result.take_persist_ready();
                            let made_progress = result.made_progress;
                            let seeded_from_local_clone = result.seeded_from_local_clone;

                            let fatal = self.handle_catchup_result(
                                result.result,
                                pending.reset_stuck_state,
                                &pending.label,
                                seeded_from_local_clone,
                            )
                            .await;

                            if !fatal {
                                if made_progress && pending.re_arm_recovery {
                                    self.reset_recovery_attempts(RecoveryResetMode::Partial { seed: 1 });
                                    self.sync_recovery_pending.store(true, Ordering::SeqCst);
                                    // Clear hard-reset livelock tracking (#2389).
                                    self.hard_reset_livelock_start
                                        .store(0, Ordering::Relaxed);
                                }

                                // Refresh the overlay query window after catchup — the
                                // protocol may have advanced, changing the close duration.
                                self.refresh_overlay_query_window().await;

                                // Refresh max tx size after catchup — if the protocol
                                // advanced (e.g., Soroban activation), notify existing
                                // peers of the increased byte limit.
                                self.refresh_max_tx_size_bytes().await;

                                // Spawn catchup persist task on a blocking thread.
                                // Dispatched from the event loop (not inside the catchup
                                // task) to avoid nested spawn_blocking (#1713, #1735).
                                if let Some(ready) = persist_ready {
                                    close_pipeline.start_persist(ready.spawn());
                                }
                            }
                            // If fatal, all post-catchup work is skipped — shutdown
                            // signal will break the loop on the next select iteration.
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
                            // Restore AppState after the failed (panicked /
                            // cancelled) catchup — extension readiness stays
                            // false until a catchup succeeds or a live ledger
                            // closes.
                            self.restore_app_state_without_readiness().await;
                        }
                    }

                    // Kick off first buffered close — but only if pipeline is idle
                    // (no persist pending from catchup or prior close).
                    if close_pipeline.is_idle() {
                        let next = self.try_start_ledger_close().await;
                        close_pipeline.try_start_close(next);
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
                    if close_pipeline.is_idle() && pending_catchup.is_none() {
                        let next = self.try_start_ledger_close().await;
                        close_pipeline.try_start_close(next);
                    }
                    tracing::trace!(select_iteration, "BRANCH: verified_rx done");
                }

                // Process SCP messages from dedicated never-drop channel.
                // These are guaranteed to arrive even if the broadcast channel overflows.
                Some(scp_msg) = scp_message_rx.recv() => {
                    self.set_phase(1); // 1 = scp_message
                    tracing::trace!(select_iteration, "BRANCH: scp_message_rx");
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
                    // [maxtps_scp] time spent queued in the dedicated SCP
                    // channel before the event loop picked it up (slow-only).
                    {
                        let intake_ms = scp_msg.received_at.elapsed().as_millis() as u64;
                        if intake_ms > 50 {
                            tracing::info!(
                                target: "maxtps_scp",
                                slot = scp_slot,
                                intake_ms,
                                "slow_intake"
                            );
                        }
                    }
                    self.pump_scp_intake(scp_msg, &mut verified_rx).await;
                    // After processing an SCP message (which may buffer an
                    // EXTERNALIZE), kick off a buffered close if none is running.
                    if close_pipeline.is_idle() && pending_catchup.is_none() {
                        let next = self.try_start_ledger_close().await;
                        close_pipeline.try_start_close(next);
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
                    if close_pipeline.is_idle() && pending_catchup.is_none() {
                        let next = self.try_start_ledger_close().await;
                        close_pipeline.try_start_close(next);
                    }
                    tracing::trace!(select_iteration, "BRANCH: fetch_response_rx done");
                }

                // Relay SCP envelopes whose deps are now resolved.
                // This is the sole relay path for all SCP envelopes — fires
                // from FetchingEnvelopes (both immediate-ready and deferred-ready).
                // Parity: stellar-core PendingEnvelopes::envelopeReady().
                Some(relay_env) = fetching_relay_rx.recv() => {
                    let slot = relay_env.envelope.statement.slot_index;
                    let received_at = relay_env.received_at;
                    let ready_path = relay_env.ready_path;
                    let relay_msg = StellarMessage::ScpMessage(relay_env.envelope);
                    if let Some(overlay) = self.overlay().await {
                        match overlay.broadcast(relay_msg).await {
                            Ok(count) => {
                                tracing::debug!(slot, peers = count, "Relayed SCP envelope");
                                // Record receive-to-relay latency (#2648).
                                // Only sample on successful broadcast to ≥1 peer.
                                if count > 0 {
                                    if let Some(t) = received_at {
                                        let label = match ready_path {
                                            henyey_herder::ReadyPath::Immediate => "immediate",
                                            henyey_herder::ReadyPath::Deferred => "deferred",
                                        };
                                        metrics::histogram!(
                                            "henyey_scp_receive_to_relay_seconds",
                                            "path" => label
                                        )
                                        .record(t.elapsed().as_secs_f64());
                                    }
                                }
                            }
                            Err(e) => {
                                tracing::warn!(slot, error = %e, "Failed to relay SCP envelope");
                            }
                        }
                    }
                }

                // Process non-critical overlay messages (TX floods, etc.).
                // SCP, fetch-response, and fetch-request messages no longer arrive here —
                // they are routed exclusively to dedicated channels at the overlay layer.
                // The skip guards below are kept as defensive fallbacks.
                // NOTE (maxtps iter 6): the broadcast-channel (inbound flood)
                // arm that lived here moved to the dedicated
                // `run_flood_consumer` task spawned before this loop.

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
                            let mut survey_state = self.survey_state.write().await;
                            survey_state.data_mut().record_scp_first_to_self_latency(ms);
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

                    // Check for externalized slots to process.
                    //
                    // Each consensus-tick (phase=5) sub-op below is wrapped
                    // with `Instant`-based timing + `warn_consensus_substep_if_slow`
                    // so a deployed node names the sub-step that blocks the
                    // event loop while contending the SQLite write lock — the
                    // ~20s phase=5 stall that is the root cause of the #3497
                    // recoverable-shutdowns (#3582). Instrumentation only: the
                    // measurement is a cheap `Duration` comparison and does not
                    // alter control flow.
                    self.set_phase(10); // 10 = process_externalized
                    if pending_catchup.is_none() {
                        let substep_start = std::time::Instant::now();
                        if let Some(pc) = self.process_externalized_slots().await {
                            pending_catchup = Some(pc);
                        }
                        super::warn_consensus_substep_if_slow(
                            substep_start.elapsed(),
                            "process_externalized_slots",
                        );
                    }

                    // Start a background ledger close if one isn't already running.
                    if close_pipeline.is_idle() {
                        let substep_start = std::time::Instant::now();
                        let next = self.try_start_ledger_close().await;
                        close_pipeline.try_start_close(next);

                        // Proactive gap detection: if no close started and the next
                        // slot's EXTERNALIZE is missing while we have later ones,
                        // request SCP state from peers immediately. This catches
                        // missed EXTERNALIZEs within seconds, while peers still have
                        // the data cached (~60s window). Without this, the node waits
                        // for SyncRecoveryManager (35s timeout) which is too late.
                        if close_pipeline.is_idle() && self.herder.state().can_receive_scp() {
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
                                    self.request_scp_state_and_record().await;
                                }
                            }
                        }
                        // Covers try_start_ledger_close + the proactive
                        // gap-detection request_scp_state_and_record (same
                        // idle-close branch).
                        super::warn_consensus_substep_if_slow(
                            substep_start.elapsed(),
                            "try_start_ledger_close",
                        );
                    }

                    // Request any pending tx sets we need
                    let substep_start = std::time::Instant::now();
                    self.request_pending_tx_sets().await;
                    super::warn_consensus_substep_if_slow(
                        substep_start.elapsed(),
                        "request_pending_tx_sets",
                    );

                    // Publish queued history checkpoints.  This is normally done
                    // from the close_pipeline completion arm, but for solo validators
                    // the select may pick the tick arm repeatedly before close completes.
                    if self.is_validator {
                        let substep_start = std::time::Instant::now();
                        self.maybe_publish_history().await;
                        super::warn_consensus_substep_if_slow(
                            substep_start.elapsed(),
                            "maybe_publish_history",
                        );
                    }

                    // Safety-net trigger (#2702). The *primary* nomination
                    // scheduler is now the event-driven TriggerNextLedger timer
                    // (armed by setup_trigger_next_ledger after each close and at
                    // cold start). This retained 1-second call is an idempotent
                    // backstop: try_trigger_consensus self-gates on the same
                    // prepareStart + expectedClose + ctValidityOffset boundary
                    // (#2816), so it can never trigger earlier than the timer
                    // would, and a redundant attempt is absorbed by the herder's
                    // AlreadyNominating guard / the watcher per-slot latch. It
                    // covers the case where a trigger timer was never armed (e.g.
                    // arming was gated out at cold start before tracking settled)
                    // — without it, a missed arm could stall ledger production.
                    let substep_start = std::time::Instant::now();
                    self.try_trigger_consensus().await;
                    super::warn_consensus_substep_if_slow(
                        substep_start.elapsed(),
                        "try_trigger_consensus",
                    );
                }

                // Stats logging
                _ = stats_interval.tick() => {
                    self.set_phase(20); // 20 = stats
                    self.log_stats().await;
                }

                // Batched tx advert flush (parity: ignoreIfOutOfSync).
                // Offloaded off the event loop (maxtps iter 6): at the ~1470
                // tx/s ceiling one flush costs ~100 ms inline (22-peer ×
                // per-hash advert bookkeeping under the tx-queue store lock),
                // occupying up to 60% of the loop and starving SCP intake
                // (measured via maxtps_loop: tx_advert_flush up to 18.3 s per
                // 30 s window). Same coalesced fire-and-not-awaited pattern as
                // dispatch_peer_maintenance: strictly serial via the in-flight
                // guard, skipped tick re-runs next period, abort on shutdown.
                _ = tx_advert_interval.tick() => {
                    if self.herder.is_tracking() {
                        self.set_phase(21); // 21 = tx_advert_flush
                        // Upgrade the self Weak into an owned Arc for the
                        // detached task (same mechanism as peer maintenance).
                        let app = self.self_arc.read().await.upgrade();
                        if let Some(app) = app {
                            dispatch_peer_maintenance(
                                async move { app.flush_tx_adverts().await },
                                &mut tx_advert_flush_task,
                            );
                        }
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

                // SCP nomination/ballot timeouts + event-driven consensus
                // trigger (#2702), single-shot via TimerManager.
                Some(event) = scp_timer_rx.recv() => {
                    self.set_phase(26); // 26 = scp_timeout
                    let pump = self.handle_scp_timer_event(event).await;
                    // A fired TriggerNextLedger may have self-externalized this
                    // node's own slot (solo validator). publish_externalized()
                    // does not wake the loop, so pump the close pipeline here —
                    // mirroring what the scp_message_rx / verified_rx arms do
                    // after a network EXTERNALIZE. Without this, a solo
                    // validator stalls until the 1-second maintenance tick
                    // (the test_horizon_ingesting cold-start failure mode).
                    if pump {
                        if pending_catchup.is_none() {
                            if let Some(pc) = self.process_externalized_slots().await {
                                pending_catchup = Some(pc);
                            }
                        }
                        if close_pipeline.is_idle() && pending_catchup.is_none() {
                            let next = self.try_start_ledger_close().await;
                            close_pipeline.try_start_close(next);
                        }
                    }
                }

                // Ping peers for latency measurements
                _ = ping_interval.tick() => {
                    self.set_phase(27); // 27 = ping
                    self.send_peer_pings().await;
                }

                // Peer maintenance - reconnect if peer count drops too low.
                // Offloaded off the event loop (#3689): `maintain_peers` awaits
                // `db_blocking("remove-failed-peers")` (busy_timeout up to 30s
                // under SQLite write-lock contention) and a 20s reconnect, both
                // of which previously froze the whole `select!` loop, starving
                // SCP and dropping the validator out of sync (recurrence of
                // #3582 despite the #3598 fix). `set_phase(28)` stays on the loop
                // thread so the watchdog phase label is preserved; the work runs
                // on a detached, coalesced task via `dispatch_peer_maintenance`.
                _ = peer_maintenance_interval.tick() => {
                    self.set_phase(28); // 28 = peer_maintenance
                    // Upgrade the self Weak into an owned Arc for the detached
                    // task (same mechanism as spawn_catchup/start_sync_recovery).
                    // Skip this tick if upgrade fails (shutting down / test App
                    // that never called set_self_arc).
                    let app = self.self_arc.read().await.upgrade();
                    if let Some(app) = app {
                        dispatch_peer_maintenance(
                            async move { app.maintain_peers().await },
                            &mut peer_maintenance_task,
                        );
                    }
                }

                // Refresh known peers from config + SQLite cache
                _ = peer_refresh_interval.tick() => {
                    self.set_phase(29); // 29 = peer_refresh
                    if let Some(overlay) = self.overlay().await {
                        let _ = self.refresh_known_peers(&overlay).await;
                    }
                }

                // Herder cleanup - evict expired data
                _ = herder_cleanup_interval.tick() => {
                    self.set_phase(30); // 30 = herder_cleanup
                    self.herder.cleanup();
                }

                // Periodic GC of unreferenced persisted SCP tx sets.
                // Parity: stellar-core HerderImpl::startTxSetGCTimer() at
                // HerderImpl.cpp:2440-2444. The purge is a synchronous
                // BEGIN IMMEDIATE SQLite write transaction; running it inline
                // froze the event loop for tens of seconds (39s watchdog_freeze,
                // #3532). It is offloaded to the blocking pool via
                // `dispatch_tx_set_gc`, which keeps purges strictly serial
                // (mirroring stellar-core's serial reschedule cadence) through
                // the loop-local in-flight handle. `set_phase(33)` stays on the
                // loop thread so the watchdog phase label is preserved.
                _ = tx_set_gc_interval.tick() => {
                    self.set_phase(33); // 33 = tx_set_gc
                    let herder = Arc::clone(&self.herder);
                    dispatch_tx_set_gc(
                        move || herder.purge_persisted_tx_sets(),
                        &mut tx_set_gc_task,
                    );
                }

                // Shutdown signal (lowest priority)
                _ = shutdown_rx.recv() => {
                    tracing::info!("Shutdown signal received");
                    // Abort any in-flight tx-set GC purge so the offloaded
                    // blocking task does not survive loop exit (#3532). The
                    // purge is a single atomic, idempotent transaction (#2770),
                    // so an abort mid-flight is harmless — it either completes
                    // (spawn_blocking closures are not force-cancelled) or the
                    // process is exiting; a skipped purge simply re-runs next
                    // interval on the next startup.
                    if let Some(h) = tx_set_gc_task.take() {
                        h.abort();
                    }
                    // Abort any in-flight peer maintenance round (#3689). Its
                    // only side effects are an idempotent `DELETE FROM peers …`
                    // and best-effort reconnects, so an abort mid-flight is
                    // harmless — a skipped round simply re-runs on next startup.
                    if let Some(h) = peer_maintenance_task.take() {
                        h.abort();
                    }
                    // Abort any in-flight advert flush (maxtps iter 6) — the
                    // process is exiting; un-flushed adverts are moot.
                    if let Some(h) = tx_advert_flush_task.take() {
                        h.abort();
                    }
                    break;
                }

                // Heartbeat for debugging
                _ = heartbeat_interval.tick() => {
                    self.set_phase(16); // 16 = heartbeat
                    let tracking_slot = self.herder.tracking_slot().get();
                    let ledger = self.current_ledger_seq();
                    let latest_ext = self.herder.latest_externalized_slot().unwrap_or(0);
                    let peers = self.overlay().await.map(|o| o.peer_count()).unwrap_or(0);

                    // Check quorum status - use latest_ext if available since we have
                    // actual SCP messages for that slot, otherwise fall back to tracking_slot
                    let quorum_check_slot = if latest_ext > 0 { latest_ext } else { tracking_slot };
                    // heard_from_quorum reports the SCP ballot protocol's per-slot
                    // flag (parity with stellar-core's BallotProtocol::mHeardFromQuorum:
                    // includes the local node, and is set for slots the node only
                    // *followed* via a quorum of EXTERNALIZE). The henyey-only
                    // SlotQuorumTracker never records the local node and only reaches a
                    // v-blocking subset for an already-externalized followed slot, so
                    // it gets stuck false after a restart while the qset is healthy
                    // (#3250). The tracker is still the source for is_v_blocking /
                    // out-of-sync recovery accounting (#1874).
                    let heard_from_quorum = self.herder.scp_heard_from_quorum(quorum_check_slot);
                    let is_v_blocking = self.herder.is_v_blocking(quorum_check_slot);

                    let scp_sent = self.scp_messages_sent.load(Ordering::Relaxed);
                    let nom_sent = self.scp_nominate_sent.load(Ordering::Relaxed);
                    let prep_sent = self.scp_prepare_sent.load(Ordering::Relaxed);
                    let conf_sent = self.scp_confirm_sent.load(Ordering::Relaxed);
                    let ext_sent = self.scp_externalize_sent.load(Ordering::Relaxed);
                    let peer_max_verified = self.max_verified_scp_slot.load(Ordering::Relaxed);
                    let peer_gap = self.effective_peer_gap(ledger);
                    let scp_messages_received =
                        self.scp_messages_received.load(Ordering::Relaxed);
                    emit_heartbeat_log(
                        tracking_slot,
                        ledger,
                        latest_ext,
                        peers,
                        heard_from_quorum,
                        is_v_blocking,
                        scp_messages_received,
                        scp_messages_received.saturating_sub(scp_messages_last_heartbeat),
                        last_scp_message_at.elapsed().as_secs(),
                        scp_sent,
                        nom_sent,
                        prep_sent,
                        conf_sent,
                        ext_sent,
                        peer_max_verified,
                        peer_gap,
                    );
                    scp_messages_last_heartbeat = scp_messages_received;

                    // Warn if we are not even hearing from a v-blocking set, which
                    // is the real partition signal. stellar-core keys partition /
                    // recovery off v-blocking, never off heardFromQuorum — a node
                    // that is v-blocking is demonstrably hearing from its quorum at
                    // the ballot level, so gating this WARN on !heard_from_quorum
                    // produced a false "network partition" warning every heartbeat
                    // after a restart (#3250). Gate on !is_v_blocking instead.
                    if self.is_validator && !is_v_blocking && peers > 0 {
                        tracing::warn!(
                            tracking_slot,
                            heard_from_quorum,
                            "Have not heard from a v-blocking set - may be experiencing network partition"
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
                                self.request_scp_state_and_record().await;
                            }
                        }
                    }

                    // Out-of-sync recovery: purge old slots when we're too far behind.
                    // This mirrors stellar-core's outOfSyncRecovery() behavior, which
                    // keys off gotVBlocking (HerderImpl.cpp:521) — never off
                    // heardFromQuorum. out_of_sync_recovery() itself early-returns
                    // when Tracking and otherwise scans v-blocking slots. Gating the
                    // call on !heard_from_quorum (the previously-stuck tracker flag)
                    // spuriously triggered recovery on a healthy node after a restart
                    // (#3250); drop that disjunct to match core.
                    if !self.herder.state().can_receive_scp() {
                        if let Some(purge_slot) = self.herder.out_of_sync_recovery() {
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

            // Generic per-phase guard (#3582): the branch that just ran
            // stamped its coarse `phase`; read it back and WARN by identity
            // (number + name) if the dispatch crossed the threshold. Covers
            // ALL ~15 branches — including phase=28 (peer_maintenance) which
            // #3582 names literally — so the deployed node logs whichever
            // phase held up the loop.
            super::warn_phase_if_slow(
                phase_dispatch_start.elapsed(),
                self.event_loop_phase.load(Ordering::Relaxed),
            );
            // [maxtps_loop] accumulate per-phase BODY time for the stats-tick
            // dump (which arm occupies the event loop under load). Measured
            // from the arm's `set_phase` stamp — NOT from the loop top — so
            // the select-wait is excluded (the first version attributed idle
            // waits to whichever arm fired next, misattributing up to 60% of
            // a window). Capped at the whole-iteration elapsed in case the
            // fired arm never stamped a phase (stale stamp guard).
            {
                let ph = self.event_loop_phase.load(Ordering::Relaxed) as usize;
                if ph < self.phase_time_us.len() {
                    let now_ns = self.start_instant.elapsed().as_nanos() as u64;
                    let entered = self.phase_entered_ns.load(Ordering::Relaxed);
                    let body_us = now_ns.saturating_sub(entered) / 1_000;
                    let iter_us = phase_dispatch_start.elapsed().as_micros() as u64;
                    self.phase_time_us[ph].fetch_add(body_us.min(iter_us), Ordering::Relaxed);
                    self.phase_count[ph].fetch_add(1, Ordering::Relaxed);
                }
            }
        }

        // Event loop has exited — stop the watchdog immediately (before
        // pipeline drain which may take time). The guard's Drop signals the
        // thread and resets tick_ms to 0.
        drop(watchdog_guard);

        // Stop the dedicated flood consumer (maxtps iter 6) — covers every
        // loop-exit path, not just the shutdown-signal arm.
        flood_consumer_task.abort();

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
        let drain_start = std::time::Instant::now();
        self.drain_close_pipeline(&mut close_pipeline).await;
        tracing::info!(
            elapsed_ms = drain_start.elapsed().as_millis() as u64,
            "Close pipeline drained"
        );

        self.set_state(AppState::ShuttingDown).await;
        let shutdown_start = std::time::Instant::now();
        self.shutdown_internal().await?;
        tracing::info!(
            elapsed_ms = shutdown_start.elapsed().as_millis() as u64,
            "Shutdown cleanup complete"
        );

        // If we shut down due to a fatal state failure, return an error
        // so the supervisor sees a nonzero exit code and can trigger
        // recovery (state wipe + restart).
        if self.fatal_state_failure.load(Ordering::SeqCst) {
            anyhow::bail!("Node shutdown due to fatal state failure — state wipe required");
        }

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
        self.request_scp_state_and_record().await;
    }

    /// Start the overlay network.
    pub async fn start_overlay(&self) -> anyhow::Result<()> {
        tracing::info!("Starting overlay network");

        self.store_config_peers().await;

        // Create local node info with the actual configured passphrase,
        // not a hardcoded testnet/mainnet default. SSC-generated configs
        // use a unique passphrase (e.g. "Private test network 'ssc-xxx'")
        // so the network ID must be derived from it.
        let mut local_node = LocalNode::new(self.keypair.clone(), &self.config.network.passphrase);
        local_node.listening_port = self.config.overlay.peer_port;
        if let Some(hash) = self.config.build.commit_hash() {
            local_node.set_commit_hash(hash);
        }

        // Start with testnet or mainnet defaults for seed peers, but only if
        // the app config doesn't explicitly set known_peers (which includes the
        // compat config case where known_peers is intentionally cleared).
        let mut overlay_config = if !self.config.overlay.known_peers.is_empty() {
            // Explicit peers configured — start from empty defaults
            OverlayManagerConfig {
                known_peers: self.config.overlay.known_peers.clone(),
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

        // Resolve config hostnames to IPs so the merge with persisted (already
        // resolved) peers can dedup correctly via dial_key().
        overlay_config.known_peers =
            Self::resolve_peers_for_storage(&overlay_config.known_peers).await;

        match self.load_persisted_peers().await {
            Ok(persisted) => {
                let mut existing_keys: std::collections::HashSet<henyey_overlay::DialKey> =
                    overlay_config
                        .known_peers
                        .iter()
                        .map(|p| p.dial_key())
                        .collect();
                for addr in persisted {
                    if existing_keys.insert(addr.dial_key()) {
                        overlay_config.known_peers.push(addr);
                    }
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "Failed to load persisted peers");
            }
        }

        // Convert preferred peers
        if !self.config.overlay.preferred_peers.is_empty() {
            overlay_config.preferred_peers = self.config.overlay.preferred_peers.clone();
        }

        // Convert preferred peer keys (node-ID-based preference)
        for key_str in &self.config.overlay.preferred_peer_keys {
            match henyey_overlay::PeerId::from_strkey(key_str) {
                Ok(peer_id) => {
                    overlay_config.preferred_peer_keys.insert(peer_id);
                }
                Err(e) => {
                    // Config::validate() should catch this first, but guard here too.
                    tracing::error!(key = key_str, error = %e, "Invalid preferred_peer_keys entry");
                }
            }
        }
        overlay_config.preferred_peers_only = self.config.overlay.preferred_peers_only;

        if let Some(timeout) = self.config.overlay.connect_timeout_secs {
            overlay_config.connect_timeout_secs = timeout;
        }

        // Map flow control byte config overrides.
        // Validated in AppConfig::validate(), so unwrap is safe here.
        overlay_config.flow_control_bytes_config = henyey_overlay::FlowControlBytesConfig::new(
            self.config.overlay.peer_flood_reading_capacity_bytes,
            self.config.overlay.flow_control_send_more_batch_size_bytes,
        )
        .expect("flow control bytes config already validated");

        let (peer_event_tx, mut peer_event_rx) = mpsc::channel(1024);
        overlay_config.peer_event_tx = Some(peer_event_tx);

        let db = self.db.clone();
        tokio::task::spawn_blocking(move || {
            while let Some(event) = peer_event_rx.blocking_recv() {
                if let Err(err) = update_peer_record(&db, event) {
                    // This writer is the classic `database is locked` victim
                    // when another connection pins SQLite's single WAL write
                    // lock past the busy-timeout (#3702). Tag it with
                    // `db_write_ctx` so it correlates in log aggregation with
                    // the holder-side `WriteCtxGuard` events that name the
                    // long-holding writer.
                    tracing::warn!(
                        db_write_ctx = "peer-record-update",
                        ?err,
                        "Failed to update peer record"
                    );
                }
            }
        });

        tracing::info!(
            listen_port = overlay_config.listen_port,
            known_peers = overlay_config.known_peers.len(),
            listen_enabled = overlay_config.listen_enabled,
            "Creating overlay with config"
        );

        let flow_control_bytes_config = overlay_config.flow_control_bytes_config;
        let mut overlay = OverlayManager::new_with_fetch_metrics(
            overlay_config,
            local_node,
            Arc::clone(&self.overlay_connection_factory),
            Arc::clone(&self.fetch_channel_depth),
            Arc::clone(&self.fetch_channel_depth_max),
            Arc::clone(&self.max_tx_size_bytes),
        )?;
        overlay.set_scp_callback(Arc::new(super::HerderScpCallback {
            herder: Arc::clone(&self.herder),
        }));
        // In-flight SCP dedup in the peer tasks (maxtps iter 7): share the
        // scheduled cache with the overlay so duplicate flood copies are
        // dropped before they transit the dedicated SCP channel. Parity:
        // stellar-core `checkScheduledAndCache` runs in the peer thread.
        {
            let scheduled = Arc::clone(&self.scp_scheduled);
            overlay.set_scp_inbound_filter(Arc::new(move |hash| scheduled.check_and_insert(*hash)));
        }
        match self.banned_peers().await {
            Ok(peers) => {
                for peer_id in peers {
                    overlay.ban_peer(peer_id).await;
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "Failed to load bans from DB during overlay start");
            }
        }

        // Set the initial per-peer query rate-limit window from the current
        // ledger close duration before the overlay starts accepting messages.
        overlay.set_query_rate_limit_window(self.rate_limit_window());

        // Initialize max_tx_size_bytes from current protocol state so the
        // first ledger close computes an accurate diff. The overlay isn't
        // stored in self.overlay yet, so refresh_max_tx_size_bytes won't
        // try to notify peers (which is correct — there are none yet).
        self.refresh_max_tx_size_bytes().await;

        // Runtime headroom validation for fixed flow control byte config.
        // Mirrors HerderImpl.cpp:2354-2372 — the configured capacity minus
        // batch must be at least max_tx_size_bytes (which now reflects the
        // real Soroban-aware value from the ledger).
        flow_control_bytes_config
            .validate_headroom(self.max_tx_size_bytes.load(Ordering::Relaxed))
            .map_err(|e| anyhow::anyhow!("{e}"))?;

        // Extract the pre-bound listener before the await point to avoid
        // holding a std::sync::MutexGuard across an async yield.
        let pre_bound = self.pre_bound_listener.lock().unwrap().take();
        overlay.start(pre_bound).await?;

        let peer_count = overlay.peer_count();
        tracing::info!(peer_count, "Overlay network started");

        *self.overlay.write().await = Some(Arc::new(overlay));

        // Grab the tracking flag handle so synchronous callbacks can update it.
        if let Some(ref om) = *self.overlay.read().await {
            *self.overlay_tracking.lock().unwrap() = Some(om.tracking_flag());
            *self.overlay_synced.lock().unwrap() = Some(om.synced_flag());
        }
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
                let tx_hash = Hash256::hash_xdr(&tx_env);
                let flood_msg_hash = henyey_overlay::compute_message_hash(
                    &StellarMessage::Transaction(tx_env.clone()),
                );
                let result = self.herder.receive_transaction(tx_env);
                // Parity: OverlayManagerImpl.cpp:1231-1236 — forgetFloodedMsg
                // for any result that is not PENDING or DUPLICATE.
                if should_forget_tx_flood_record(&result) {
                    if let Some(overlay) = self.overlay().await {
                        overlay.forget_flooded_msg(&flood_msg_hash);
                    }
                }
                // Receive-result distribution: names the rejection that eats
                // re-pushed stranded txs (age-2 wedged-tx bodies systematically
                // fail to enter ANY peer queue — 2026-07-04 forensics).
                metrics::counter!(
                    "henyey_overlay_tx_receive_result_total",
                    "result" => tx_receive_result_label(&result)
                )
                .increment(1);
                if !matches!(
                    result,
                    henyey_herder::TxQueueResult::Added | henyey_herder::TxQueueResult::Duplicate
                ) {
                    // Sampled 1/64: late flood copies of already-included txs
                    // legitimately return Banned at high volume; the counter
                    // above carries exact totals.
                    static REJECT_SAMPLE: std::sync::atomic::AtomicU64 =
                        std::sync::atomic::AtomicU64::new(0);
                    if REJECT_SAMPLE.fetch_add(1, std::sync::atomic::Ordering::Relaxed) % 64 == 0 {
                        tracing::warn!(
                            target: "maxtps_ban",
                            hash8 = %&tx_hash.to_hex()[..16],
                            peer = %msg.from_peer,
                            result = tx_receive_result_label(&result),
                            "flooded tx rejected by local queue (sampled 1/64)"
                        );
                    }
                }
                match result {
                    henyey_herder::TxQueueResult::Added => {
                        tracing::debug!(peer = %msg.from_peer, "Transaction added to queue");
                        self.record_tx_pull_latency(tx_hash, &msg.from_peer).await;
                        // No explicit advert enqueue — flush_tx_adverts() reads
                        // the herder queue in priority order each flood period.
                    }
                    henyey_herder::TxQueueResult::Duplicate => {
                        self.record_tx_pull_latency(tx_hash, &msg.from_peer).await;
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
                    stellar_xdr::MessageType::TxSet | stellar_xdr::MessageType::GeneralizedTxSet
                );
                let is_ping = matches!(dont_have.type_, stellar_xdr::MessageType::ScpQuorumset);
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
                // Rate limiting enforced by the overlay pre-filter
                // (QueryRateLimiter in peer_loop.rs).
                tracing::debug!(ledger_seq, peer = %msg.from_peer, "Peer requested SCP state");
                self.send_scp_state(&msg.from_peer, ledger_seq).await;
            }

            StellarMessage::GetScpQuorumset(hash) => {
                // Rate limiting enforced by the overlay pre-filter
                // (QueryRateLimiter in peer_loop.rs).
                tracing::debug!(hash = hex::encode(hash.0), peer = %msg.from_peer, "Peer requested quorum set");
                self.send_quorum_set(&msg.from_peer, hash).await;
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
                let computed_hash =
                    match stellar_xdr::WriteXdr::to_xdr(&tx_set, stellar_xdr::Limits::none()) {
                        Ok(xdr_bytes) => format!("{}", henyey_common::Hash256::hash(&xdr_bytes)),
                        Err(e) => format!("<encoding failed: {e}>"),
                    };
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
                let computed_hash =
                    match stellar_xdr::WriteXdr::to_xdr(&gen_tx_set, stellar_xdr::Limits::none()) {
                        Ok(xdr_bytes) => format!("{}", henyey_common::Hash256::hash(&xdr_bytes)),
                        Err(e) => format!("<encoding failed: {e}>"),
                    };
                tracing::debug!(
                    peer = %msg.from_peer,
                    computed_hash = %computed_hash,
                    "APP: Received GeneralizedTxSet from overlay"
                );
                self.handle_generalized_tx_set(gen_tx_set).await;
            }

            StellarMessage::GetTxSet(hash) => {
                // Rate limiting enforced by the overlay pre-filter
                // (QueryRateLimiter in peer_loop.rs).
                tracing::debug!(hash = hex::encode(hash.0), peer = %msg.from_peer, "Peer requested TxSet");
                self.send_tx_set(&msg.from_peer, &henyey_common::Hash256(hash.0))
                    .await;
            }

            _ => {
                // Other message types (Hello, Auth, etc.) are handled by overlay
                tracing::trace!(msg_type = ?std::mem::discriminant(&msg.message), "Ignoring message type");
            }
        }
    }

    /// Dedicated consumer for the overlay broadcast channel (inbound flood:
    /// Transaction / FloodAdvert / FloodDemand / peer chatter). See the spawn
    /// site in [`Self::run_event_loop`] for the rationale (maxtps iter 6 —
    /// bulk flood work off the consensus event loop). SCP messages and fetch
    /// responses are filtered out here exactly as the old select arm did:
    /// they arrive via their dedicated channels and are handled on the loop.
    ///
    /// Holds only a `Weak<App>`; exits when the channel closes or the app is
    /// dropped. Processing stays strictly serial (one message at a time),
    /// preserving the old arm's ordering semantics — it just no longer
    /// competes with SCP/close work for the main loop.
    async fn run_flood_consumer(
        weak: std::sync::Weak<Self>,
        mut rx: tokio::sync::broadcast::Receiver<OverlayMessage>,
    ) {
        loop {
            match rx.recv().await {
                Ok(overlay_msg) => {
                    // Skip SCP messages — handled via the dedicated SCP channel.
                    if matches!(overlay_msg.message, StellarMessage::ScpMessage(_)) {
                        continue;
                    }
                    // Skip fetch response/request messages — handled via the
                    // dedicated fetch channel.
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
                    let Some(app) = weak.upgrade() else {
                        tracing::info!("Flood consumer exiting: app dropped");
                        return;
                    };
                    let delivery_latency = overlay_msg.received_at.elapsed();
                    tracing::debug!(
                        latency_ms = delivery_latency.as_millis(),
                        "Received overlay message (flood consumer)"
                    );
                    app.handle_overlay_message(overlay_msg).await;
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    // Only non-critical messages (TX floods) flow through the
                    // broadcast channel now, so lag is expected under load.
                    // [maxtps_diag] Count dropped flood messages — under load
                    // these are demanded txs the node never receives, forcing
                    // re-demands (stellar_overlay_demand_timeout_total) and
                    // leaving the agreed set short. Core never drops flooded
                    // txs (flow-controlled per-peer queues).
                    metrics::counter!("stellar_overlay_broadcast_lagged_dropped_total")
                        .increment(n);
                    tracing::debug!(
                        skipped = n,
                        "Overlay broadcast receiver lagged (non-critical messages only)"
                    );
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    tracing::info!("Overlay broadcast channel closed; flood consumer exiting");
                    return;
                }
            }
        }
    }

    /// Log current stats.
    async fn log_stats(&self) {
        // [maxtps_loop] drain + dump per-phase event-loop busy time since the
        // last stats tick; one line listing phases with >50 ms accumulated.
        {
            let mut parts: Vec<String> = Vec::new();
            let mut total_ms = 0u64;
            for ph in 0..self.phase_time_us.len() {
                let us = self.phase_time_us[ph].swap(0, Ordering::Relaxed);
                let n = self.phase_count[ph].swap(0, Ordering::Relaxed);
                let ms = us / 1000;
                total_ms += ms;
                if ms > 50 {
                    parts.push(format!(
                        "{}={}ms/{}",
                        super::event_loop_phase_name(ph as u64),
                        ms,
                        n
                    ));
                }
            }
            if !parts.is_empty() {
                parts.sort_by_key(|p| {
                    std::cmp::Reverse(
                        p.split('=')
                            .nth(1)
                            .and_then(|v| v.split("ms").next())
                            .and_then(|v| v.parse::<u64>().ok())
                            .unwrap_or(0),
                    )
                });
                tracing::info!(
                    target: "maxtps_loop",
                    total_busy_ms = total_ms,
                    breakdown = parts.join(" "),
                    "loop_busy"
                );
            }
        }

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
            tracking_slot = stats.tracking_slot.get(),
            pending_txs = stats.pending_transactions,
            ledger,
            peers = peer_count,
            is_validator = self.is_validator,
            "Node status"
        );

        if let Some(fs) = flood_stats {
            tracing::debug!(
                seen_messages = fs.seen_count,
                duplicate_messages = fs.duplicate_messages,
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
        let request = StellarMessage::GetTxSet(stellar_xdr::Uint256(tx_set_hash.0));
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
    ///
    /// # Deferred effect before/during startup
    ///
    /// The signal only takes effect when the main event loop's `select`
    /// observes it. A shutdown requested before [`App::run`] starts is
    /// retained (the initial broadcast receiver is created together with the
    /// channel), and one requested while startup catchup is still running is
    /// likewise buffered — but in both cases it is honored only at the first
    /// main-loop select iteration. In-flight startup/catchup work is not
    /// interrupted by this call.
    pub fn shutdown(&self) {
        tracing::info!("Shutdown requested");
        let _ = self.shutdown_tx.send(());
    }

    /// Trigger shutdown due to unrecoverable local state failure.
    ///
    /// Called when the node detects that its local ledger state cannot be
    /// trusted (fatal catchup verification failure, pre-close hash mismatch,
    /// etc.).  Sets [`fatal_state_failure`] to block further catchup and
    /// ledger-close attempts, then signals the main loop to exit.
    ///
    /// # Monitoring contract
    ///
    /// This method emits a `tracing::error!` event with the structured field
    /// `fatal_wipe_required = true` (see [`FATAL_WIPE_FIELD`]).  This field
    /// is **reserved exclusively** for this method — no other code path emits
    /// it.  External monitoring tools grep rendered log output for this field
    /// to trigger automatic state wipes.
    ///
    /// The contract depends on the shipped `tracing_subscriber::fmt` Text and
    /// JSON formatters rendering the field detectably.  The
    /// `test_fatal_shutdown_emits_wipe_field_*` tests guard this mechanically
    /// using the same formatter construction as `logging.rs`.
    ///
    /// After shutdown, [`App::run`] returns `Err` (exit code 1) so the
    /// supervisor can detect the failure and trigger recovery.
    ///
    /// **Delayed shutdown**: the shutdown signal is processed in a subsequent
    /// `select` iteration of the main event loop.  Between the call and the
    /// shutdown-signal arm firing, other select arms may execute once.  This
    /// is acceptable because the `fatal_state_failure` flag immediately blocks
    /// new catchup attempts and new ledger closes.
    pub fn trigger_fatal_shutdown(&self, reason: &str) {
        tracing::error!(
            fatal_wipe_required = true,
            "FATAL: unrecoverable local state failure — {}. \
             Node will shut down. State wipe required before restart.",
            reason
        );
        self.fatal_state_failure.store(true, Ordering::SeqCst);
        self.shutdown();
    }

    /// Trigger a clean, recoverable shutdown due to a transient environmental
    /// failure (e.g. ENOSPC/EDQUOT during bucket persist — #3478).
    ///
    /// Unlike [`trigger_fatal_shutdown`](Self::trigger_fatal_shutdown), this:
    /// - does **NOT** emit `fatal_wipe_required = true` (see [`FATAL_WIPE_FIELD`]) —
    ///   the on-disk state is intact (no partial state was committed), so a
    ///   wipe would be both wasteful and wrong; the operator just needs to free
    ///   space and restart; and
    /// - does **NOT** set `fatal_state_failure` — the condition is recoverable,
    ///   so a restart after disk recovery may proceed normally.
    ///
    /// # Parity (stellar-core v26.0.1)
    ///
    /// A bucket merge/flush IO failure in core throws a plain `runtime_error`
    /// tagged `POSSIBLY_CORRUPTED_LOCAL_FS` ("ensure enough space") that
    /// propagates uncaught out of `closeLedger` → **clean process exit**. Core
    /// auto-wipes on NEITHER ENOSPC nor corruption (wipe is operator-driven),
    /// and there is no `std::abort`/`terminate` on the bucket path. This method
    /// is the parity-faithful henyey equivalent: a clean shutdown with the
    /// free-space operator guidance and no wipe — distinct from corruption,
    /// which retains the abort + wipe path.
    pub fn trigger_recoverable_shutdown(&self, reason: &str) {
        tracing::error!(
            "RECOVERABLE: transient local IO failure — {}. \
             Node will shut down cleanly. Free disk space and restart; \
             no state wipe required (on-disk state is intact).",
            reason
        );
        self.shutdown();
    }

    /// A handle that lets a detached persist task request a clean recoverable
    /// shutdown (no state wipe) on a transient-IO persist failure (#3478).
    pub(crate) fn recoverable_shutdown_handle(&self) -> super::persist::RecoverableShutdownHandle {
        super::persist::RecoverableShutdownHandle::new(self.shutdown_tx.clone())
    }

    /// Subscribe to shutdown notifications.
    pub fn subscribe_shutdown(&self) -> tokio::sync::broadcast::Receiver<()> {
        self.shutdown_tx.subscribe()
    }

    /// Take the shutdown receiver created together with the channel, so a
    /// shutdown signalled before `App::run` starts is retained and honored.
    ///
    /// Only the first call gets the pre-run receiver. Subsequent calls (e.g.
    /// a `NodeRunner` embedding that retries `run()` after a transient error)
    /// fall back to a fresh subscription: signals sent before the fallback
    /// subscribe are lost, matching plain `subscribe_shutdown` semantics.
    pub(super) async fn take_initial_shutdown_receiver(
        &self,
    ) -> tokio::sync::broadcast::Receiver<()> {
        match self.initial_shutdown_rx.lock().await.take() {
            Some(rx) => rx,
            None => {
                tracing::debug!(
                    "Initial shutdown receiver already taken; subscribing a fresh receiver"
                );
                self.shutdown_tx.subscribe()
            }
        }
    }

    /// Drain the close pipeline on shutdown.
    ///
    /// stellar-core parity: `idempotentShutdown()` joins the ledger-close
    /// thread first before tearing down subsystems.
    ///
    /// Order matters:
    /// 1. Drain persist first — a prior close's (or catchup's) DB writes may
    ///    be in-flight.  Persist panics abort the process, matching the normal
    ///    event-loop behavior (persist-complete arm).
    /// 2. Drain close — await the `spawn_blocking` task, then call
    ///    `handle_close_complete()` with `LedgerCloseFinalizer::inline()` so
    ///    the close's own persist runs to completion before we return.
    pub(super) async fn drain_close_pipeline(
        &self,
        pipeline: &mut super::close_pipeline::ClosePipeline,
    ) {
        // With the enum-based pipeline, only one state is active at a time.
        // Drain whichever operation is in progress.
        if pipeline.is_persisting() {
            let persist = pipeline.take_persist();
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

        if pipeline.is_closing() {
            let mut pending = pipeline.take_close();
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

        // Shut down sync recovery manager gracefully: send Shutdown command,
        // then await the task handle for clean ordering.
        let sync_recovery = self.sync_recovery_handle.read().clone();
        if let Some(ref handle) = sync_recovery {
            handle.shutdown().await;
        }
        let sync_task = self.sync_recovery_task.write().take();
        if let Some(task) = sync_task {
            let _ = task.await;
        }

        // Shut down the SCP timer manager: send Shutdown command, then
        // await the task handle for clean ordering.
        self.timer_manager_handle.shutdown().await;
        if let Some(task) = self.timer_manager_join.lock().await.take() {
            let _ = task.await;
        }

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

        // Take the overlay out and drop the write guard before calling
        // shutdown().await — holding the guard across the await would block
        // all concurrent readers for the duration of connection teardown.
        // After take(), concurrent readers see None (same as post-shutdown).
        let overlay_arc = self.overlay.write().await.take();
        if let Some(overlay_arc) = overlay_arc {
            match Arc::try_unwrap(overlay_arc) {
                Ok(mut overlay_owned) => {
                    if let Err(err) = overlay_owned.shutdown().await {
                        tracing::warn!(error = %err, "Overlay shutdown reported error");
                    }
                }
                Err(arc) => {
                    // Other references still exist; signal shutdown through
                    // &self so peers still receive the shutdown message even
                    // though we can't join handles without &mut ownership.
                    tracing::warn!(
                        "Overlay still has outstanding references at shutdown, signaling"
                    );
                    arc.signal_shutdown();
                }
            }
        }

        Ok(())
    }

    /// Compute the standard per-peer rate-limit window.
    fn rate_limit_window(&self) -> Duration {
        query_rate_limit_window(self.herder.ledger_close_duration())
    }

    /// Push the current query rate-limit window to the overlay.
    ///
    /// Called after startup, catchup, and each ledger close so the overlay's
    /// per-peer pre-filter stays in sync with the dynamic close duration.
    /// Parity: stellar-core recomputes per-call in Peer::process() (Peer.cpp:1426-1429).
    pub(crate) async fn refresh_overlay_query_window(&self) {
        if let Some(overlay) = self.overlay().await {
            overlay.set_query_rate_limit_window(self.rate_limit_window());
        }
    }

    /// Recompute `max_tx_size_bytes` from the given protocol state and store
    /// it. Returns the increase (saturating) over the previous value, or 0 if
    /// the max stayed the same or decreased.
    ///
    /// This is a pure bookkeeping update — it does NOT notify overlay peers.
    /// Callers that need peer notification should use [`refresh_max_tx_size_bytes`].
    pub(super) fn update_max_tx_size_bytes(
        &self,
        protocol_version: u32,
        soroban_tx_max: Option<u32>,
    ) -> u32 {
        let new_max = compute_max_tx_size(protocol_version, soroban_tx_max);
        let old_max = self.max_tx_size_bytes.swap(new_max, Ordering::Relaxed);
        new_max.saturating_sub(old_max)
    }

    /// Refresh `max_tx_size_bytes` from current ledger state and notify
    /// overlay peers if the value increased.
    ///
    /// Called at startup (before overlay starts — no peers to notify),
    /// after catchup, and on each ledger close. The overlay reads this
    /// atomic to compute dynamic initial byte grants for new peers via
    /// `FlowControlBytesConfig::bytes_total()`. Existing peers are notified
    /// of increases via `handle_max_tx_size_increase()`.
    ///
    /// Mirrors upstream `HerderImpl::maybeHandleUpgrade()` max-tx-size
    /// tracking plus the startup initialization in `HerderImpl::start()`.
    pub(crate) async fn refresh_max_tx_size_bytes(&self) {
        let snap = self.ledger_manager.header_snapshot();
        let protocol_version = snap.header.ledger_version;
        let soroban_tx_max = snap.soroban_network_info.map(|info| info.tx_max_size_bytes);
        let increase = self.update_max_tx_size_bytes(protocol_version, soroban_tx_max);
        if increase > 0 {
            if let Some(overlay) = self.overlay().await {
                overlay.handle_max_tx_size_increase(increase).await;
            }
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
        mut scp_msg: OverlayMessage,
        verified_rx: &mut tokio::sync::mpsc::UnboundedReceiver<
            henyey_herder::scp_verify::VerifiedEnvelope,
        >,
    ) {
        use henyey_herder::scp_verify::{PipelinedIntake, PreFilter};

        // #3625 — drain-gated inbound flow control. Take the SCP message's
        // `FlowControlRelease` token; it is released (firing the peer's
        // `end_message_processing` and, at the 40-message / byte batch
        // boundary, a `SEND_MORE_EXTENDED` grant) when `_flow_release` drops at
        // the end of this consumer-side handler — i.e. only AFTER the envelope
        // has been drained from the bounded channel and handed to the verify
        // worker / herder. A stalled event loop never reaches here, so it never
        // grants SEND_MORE, back-pressuring senders. The `Drop`-based release
        // also covers every early-return path below (dedup reject, worker dead,
        // non-SCP) so the held inbound credit can never leak. This is the only
        // app-side touch for Phase 1; it applies to all three SCP drain sites
        // (the `recv()` arm and both `try_recv` drains), which all route here.
        let _flow_release = scp_msg.take_flow_release();

        // Phase 31 marks time spent in this helper: pre-filtering, reserving
        // verifier-queue capacity (the backpressure park point), and draining
        // verified envelopes interleaved with that wait. The watchdog uses
        // this to distinguish "stuck waiting for the verify worker" from
        // "stuck inside select!".
        self.set_phase(31); // 31 = scp_verifier
        self.scp_verify_output_backlog
            .store(verified_rx.len() as u64, Ordering::Relaxed);

        // Reuse the hash + in-flight token claimed by the overlay peer task's
        // dedup filter (maxtps iter 7) when present; compute/claim locally
        // otherwise (locally-sourced envelopes, overlay without the filter,
        // tests). The hash covers the full StellarMessage XDR, so it must be
        // taken BEFORE destructuring.
        let peer_task_token = scp_msg.scp_inflight_token.take();
        let flood_msg_hash = scp_msg
            .message_hash
            .unwrap_or_else(|| henyey_overlay::compute_message_hash(&scp_msg.message));

        // Capture overlay arrival time before destructuring — used for
        // receive-to-relay latency histogram (#2648).
        let overlay_received_at = scp_msg.received_at;

        let envelope = match scp_msg.message {
            StellarMessage::ScpMessage(e) => e,
            other => {
                tracing::warn!("pump_scp_intake called with non-SCP message: {other:?}");
                return;
            }
        };

        let from_peer = scp_msg.from_peer;
        let verifier = self.herder.scp_verifier_handle();

        // In-flight dedup: reject envelopes already dispatched to the verify
        // worker. Uses the full StellarMessage hash matching stellar-core's
        // checkScheduledAndCache (Peer.cpp:1113-1117, OverlayManagerImpl.cpp:1190-1212).
        // The returned Arc<()> token keeps the cache entry alive; dropping it
        // (on pre-filter reject, channel close, or end of processing) auto-expires
        // the entry. When the overlay peer task already claimed the token via
        // the inbound filter (maxtps iter 7), reuse it instead of re-checking
        // — re-checking would see our own live entry and self-reject.
        let inflight_token = match peer_task_token {
            Some(token) => token,
            None => match self.scp_scheduled.check_and_insert(flood_msg_hash) {
                Some(token) => token,
                None => return,
            },
        };

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
                            // Token dropped here — cache entry auto-expires.
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
                        PreFilter::Accept(intake) => {
                            // Parity: HerderImpl.cpp:810 `mEnvelopeReceive.Mark()`
                            // — fires post-MANUAL_CLOSE gate, pre-validity.
                            self.scp_messages_received
                                .fetch_add(1, Ordering::Relaxed);
                            // Reconstruct with overlay context via from_overlay,
                            // which requires the inflight_token at compile time.
                            let (envelope, slot, is_externalize) = intake.into_parts();
                            let intake = PipelinedIntake::from_overlay(
                                envelope,
                                slot,
                                is_externalize,
                                from_peer,
                                flood_msg_hash,
                                inflight_token,
                                overlay_received_at,
                            );
                            permit.send(intake);
                        }
                        PreFilter::Reject(reason) => {
                            use henyey_herder::scp_verify::PreFilterRejectReason;
                            // Parity: HerderImpl.cpp:810 — counter fires for all
                            // rejects except ManualClose (which returns before the
                            // Mark() call at line 805-808).
                            if !matches!(reason, PreFilterRejectReason::ManualClose) {
                                self.scp_messages_received
                                    .fetch_add(1, Ordering::Relaxed);
                            }
                            self.record_prefilter_reject(reason);
                            // Pre-filter rejects are terminal — forget the
                            // FloodGate record so a re-delivered copy is treated
                            // as new (parity: Peer.cpp:1672-1678).
                            if let Some(overlay) = self.overlay().await {
                                overlay.forget_flooded_msg(&flood_msg_hash);
                            }
                            // Token dropped here — cache entry auto-expires.
                            drop(permit);
                        }
                    }
                    return;
                }
            }
        }
    }

    fn record_prefilter_reject(&self, reason: henyey_herder::scp_verify::PreFilterRejectReason) {
        self.scp_prefilter_counters[reason].fetch_add(1, Ordering::Relaxed);
    }

    /// Process a fully-verified envelope on the event loop, running the
    /// post-verify gates and side-effect block that used to live inline in
    /// `handle_overlay_message`'s SCP arm.
    pub(super) async fn process_verified(&self, ve: henyey_herder::scp_verify::VerifiedEnvelope) {
        use henyey_herder::scp_verify::Verdict;
        self.set_phase(32); // 32 = scp_verified

        // In-flight dedup cleanup is automatic: the `inflight_token` in
        // `ve.intake` will be dropped when `ve` goes out of scope at the end
        // of this function, auto-expiring the ScpScheduledCache entry.

        let slot = ve.intake.slot();
        let tracking = self.herder.tracking_slot().get();
        let is_externalize = ve.intake.is_externalize();
        // Extract the FloodGate message hash (if set by pump_scp_intake) before
        // the VerifiedEnvelope is consumed by herder.process_verified().
        let flood_msg_hash = ve.intake.flood_msg_hash().copied();

        // Record verify latency (enqueue → post-verify dispatch) into the
        // poor-man's histogram (sum + count) so the average can be read
        // from the /metrics endpoint.
        let verify_latency_us = ve.intake.enqueue_at().elapsed().as_micros() as u64;
        self.scp_verify_latency_us_sum
            .fetch_add(verify_latency_us, Ordering::Relaxed);
        self.scp_verify_latency_count
            .fetch_add(1, Ordering::Relaxed);

        // [maxtps_scp] full-pipeline latency for one SCP envelope: overlay
        // recv → dedicated channel → intake → verify worker → this dispatch.
        // Only slow envelopes are logged (bounds volume); used to attribute
        // the nomination-convergence tail (slow slots) to pipeline queueing
        // vs fetch service time. Parity-irrelevant logging.
        if let Some(recv_at) = ve.intake.received_at() {
            let total_ms = recv_at.elapsed().as_millis() as u64;
            if total_ms > 50 {
                tracing::info!(
                    target: "maxtps_scp",
                    slot,
                    total_ms,
                    verify_us = verify_latency_us,
                    "slow_pipeline"
                );
            }
        }

        // SCP latency bookkeeping.
        //
        // IMPORTANT ordering: we intentionally record `first_seen` / the
        // self-to-other latency *here*, AFTER the worker has verified the
        // signature, rather than at overlay dispatch. This makes the
        // recorded latency reflect "user-visible processing" (time from
        // envelope admit to post-verify handling) including any time the
        // envelope spent queued on the verifier. Pre-verify bookkeeping
        // would undercount under verifier backpressure.
        // Scope scp_latency so the write guard is dropped before acquiring
        // survey_state — matching the pattern at ~602-609. Holding both locks
        // simultaneously is a latent deadlock if a future code path acquires
        // them in reverse order.
        let self_to_other_ms = {
            let mut latency = self.scp_latency.write().await;
            let now = self.clock.now();
            latency.record_first_seen(slot, now);
            latency.record_other_after_self(slot, now)
        };
        if let Some(ms) = self_to_other_ms {
            let mut survey_state = self.survey_state.write().await;
            survey_state.data_mut().record_scp_self_to_other_latency(ms);
        }

        // Fast-path reject surfaced by the worker (invalid signature or
        // panic) — log, emit the same warning handle_overlay_message used
        // to emit, and skip the rest of the side-effect block.
        if !matches!(ve.verdict, Verdict::Ok) {
            let peer = ve
                .intake
                .peer_id()
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
            let node_id = ve.intake.envelope().statement.node_id.clone();
            let (envelope_result, reason) = self.herder.process_verified(ve);
            self.record_post_verify_reason(reason);
            // Forget the FloodGate record when the outcome is DISCARDED
            // (parity: Peer.cpp:1672-1678 forgetFloodedMsg).
            if should_forget_flood_record(envelope_result, reason) {
                if let Some(hash) = &flood_msg_hash {
                    if let Some(overlay) = self.overlay().await {
                        overlay.forget_flooded_msg(hash);
                    }
                }
            }
            // Debug-level envelope path attribution for verify-rejected envelopes.
            // Demoted from info! per #2341; enable with RUST_LOG=henyey::envelope_path=debug.
            tracing::debug!(
                target: "henyey::envelope_path",
                slot,
                node_id = ?node_id,
                result = ?envelope_result,
                reason = ?reason,
                "envelope path outcome (verify-rejected)",
            );
            return;
        }

        let envelope = ve.intake.envelope().clone();
        let from_peer_opt = ve.intake.peer_id().cloned();

        let tx_set_hash = if is_externalize {
            match &envelope.statement.pledges {
                stellar_xdr::ScpStatementPledges::Externalize(ext) => {
                    match StellarValue::from_xdr(&ext.commit.value.0, stellar_xdr::Limits::none()) {
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

        // Tx-set hashes referenced by NOMINATE values. During nomination only
        // the value's builder is guaranteed to already hold the set, so the
        // round-robin fetcher in `request_pending_tx_sets` can burn its first
        // asks on peers that don't have it yet and time out the whole
        // nomination round. stellar-core's Tracker always asks peers that sent
        // envelopes referencing the item first (Tracker::tryNextPeer via
        // getPeersKnows); mirror that by asking the envelope sender directly
        // when the herder reports Fetching (see the EnvelopeState::Fetching arm).
        let nominate_tx_set_hashes: Vec<Hash256> = match &envelope.statement.pledges {
            stellar_xdr::ScpStatementPledges::Nominate(nom) => {
                let mut hashes: Vec<Hash256> = nom
                    .votes
                    .iter()
                    .chain(nom.accepted.iter())
                    .filter_map(|v| StellarValue::from_xdr(&v.0, stellar_xdr::Limits::none()).ok())
                    .map(|sv| Hash256::from_bytes(sv.tx_set_hash.0))
                    .collect();
                hashes.sort_unstable();
                hashes.dedup();
                hashes
            }
            _ => Vec::new(),
        };

        let hash = henyey_common::scp_quorum_set_hash(&envelope.statement);
        let hash256 = henyey_common::Hash256::from_bytes(hash.0);
        let sender_node_id = envelope.statement.node_id.clone();

        // Hand off to Herder for gate recheck + self-message skip +
        // non-quorum reject + slot_quorum_tracker + prefetch + pending.add.
        let herder_t0 = std::time::Instant::now();
        let (envelope_result, reason) = self.herder.process_verified(ve);
        // [maxtps_scp] campaign-2 iter-3: single scp_verified branch calls
        // were observed blocking the event loop for 18+ s at the full-window
        // edge — attribute whether the stall is inside the herder call.
        let herder_ms = herder_t0.elapsed().as_millis() as u64;
        if herder_ms > 1_000 {
            tracing::info!(
                target: "maxtps_scp",
                slot,
                herder_ms,
                is_externalize,
                result = ?envelope_result,
                "slow_process_verified"
            );
        }

        // Per-reason post-verify metric.
        self.record_post_verify_reason(reason);

        // Forget the FloodGate record when the outcome is DISCARDED
        // (parity: Peer.cpp:1672-1678 forgetFloodedMsg).
        if should_forget_flood_record(envelope_result, reason) {
            if let Some(hash) = &flood_msg_hash {
                if let Some(overlay) = self.overlay().await {
                    overlay.forget_flooded_msg(hash);
                }
            }
        }

        // Track highest verified SCP slot from peers for recovery stall detection.
        // Only fires for Verdict::Ok envelopes (verify-rejected early return above).
        // Excludes self-messages (own SCP output reflected back).
        if !matches!(
            reason,
            henyey_herder::scp_verify::PostVerifyReason::SelfMessage
        ) && slot > 0
        {
            self.max_verified_scp_slot
                .fetch_max(slot, Ordering::Relaxed);
        }

        // Envelope path attribution log (Issue #1806, demoted to debug per #2341).
        // Enable with RUST_LOG=henyey::envelope_path=debug to see per-envelope
        // outcomes. Fields:
        //   reason (PostVerifyReason): which post-verify gate the envelope hit.
        //     `Accepted` = passed all gates, entered downstream intake.
        //   result (EnvelopeState): outcome after downstream processing
        //     (Valid, Invalid, TooOld, Fetching, etc.).
        // So `result=Invalid reason=Accepted` means the envelope passed all
        // pre/post-verify gates but was rejected during downstream processing
        // (e.g., stale ballot in SCP, discarded during fetch intake).
        tracing::debug!(
            target: "henyey::envelope_path",
            slot,
            node_id = ?sender_node_id,
            result = ?envelope_result,
            reason = ?reason,
            "envelope path outcome",
        );

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
                        let request = StellarMessage::GetScpQuorumset(stellar_xdr::Uint256(hash.0));
                        if let Err(e) = overlay.try_send_to(peer, request) {
                            tracing::debug!(peer = %peer, error = %e, "Failed to request quorum set");
                        }
                    }
                }
            }
        }

        // SCP envelope relay is handled entirely by the FetchingEnvelopes
        // broadcast callback (wired at startup). When deps are satisfied —
        // either immediately (recv_envelope) or later (check_and_move_to_ready)
        // — the callback fires, sending the envelope through an unbounded
        // channel to the event loop which broadcasts via overlay.
        //
        // Parity: stellar-core PendingEnvelopes::envelopeReady() broadcasts
        // once isFullyFetched() is true, before SCP processes the envelope.
        // Our FetchingEnvelopes fires at the same point (deps resolved).
        //
        // No relay here at the app layer — all relay flows through one path
        // to prevent double-relay and simplify reasoning.

        match envelope_result {
            EnvelopeState::Valid => {
                tracing::debug!(slot, tracking, "SCP envelope accepted (Valid)");
                if is_externalize {
                    // Track the highest accepted EXTERNALIZE slot (Valid or Pending only).
                    // Used by submit_transaction() to gate user-facing submissions when
                    // the node is behind. Must NOT fire for Invalid/TooOld.  See #1812.
                    self.max_observed_externalize_slot
                        .fetch_max(slot, Ordering::SeqCst);
                    // Observability-only (#3270): record this peer's highest
                    // observed externalized slot via live SCP gossip, so the
                    // GetScpState re-request log can report how many connected
                    // peers could still serve the requested slot. Pure
                    // side-write — does not alter envelope control flow.
                    if let Some(peer) = from_peer_opt.as_ref() {
                        if let Some(overlay) = self.overlay().await {
                            overlay.record_peer_externalized(peer, slot);
                        }
                    }
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
                            self.escalate_recovery_to_catchup();
                        }
                    }
                }
            }
            EnvelopeState::Pending => {
                tracing::debug!(slot, tracking, "SCP envelope buffered for future slot");
                if is_externalize {
                    self.max_observed_externalize_slot
                        .fetch_max(slot, Ordering::SeqCst);
                    // Observability-only (#3270): record this peer's highest
                    // observed externalized slot via live SCP gossip. Pure
                    // side-write — does not alter envelope control flow.
                    if let Some(peer) = from_peer_opt.as_ref() {
                        if let Some(overlay) = self.overlay().await {
                            overlay.record_peer_externalized(peer, slot);
                        }
                    }
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
                            self.escalate_recovery_to_catchup();
                            self.sync_recovery_pending.store(true, Ordering::SeqCst);

                            if self.recovery_throttles.far_ahead.should_log(current_ledger) {
                                tracing::info!(
                                    slot,
                                    current_ledger,
                                    gap = slot - current_ledger,
                                    "Pending EXTERNALIZE far ahead — fast-tracking catchup"
                                );
                            } else {
                                tracing::debug!(
                                    slot,
                                    current_ledger,
                                    gap = slot - current_ledger,
                                    "Pending EXTERNALIZE far ahead — fast-tracking catchup \
                                     (repeated)"
                                );
                            }
                        }
                    }
                }
            }
            EnvelopeState::Duplicate => {}
            EnvelopeState::Deferred => {
                // Envelope buffered by the closing gate — will be replayed
                // from `ledger_closed` after LCL advances. No side effects.
                tracing::trace!(slot, "SCP envelope deferred by closing gate");
            }
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
                                StellarMessage::GetTxSet(stellar_xdr::Uint256(tx_set_hash.0));
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
                // Ask the nominate sender directly for any tx set we still
                // need (advertiser-first fetch, core Tracker parity). Throttled
                // through `tx_set_last_request` so the ~22 flood echoes of the
                // same value don't multiply into parallel full-set downloads.
                if !nominate_tx_set_hashes.is_empty() {
                    if let Some(peer) = from_peer_opt.as_ref() {
                        if let Some(overlay) = self.overlay().await {
                            let now = self.clock.now();
                            for h in &nominate_tx_set_hashes {
                                if !self.herder.needs_tx_set(h) {
                                    continue;
                                }
                                let should_send = {
                                    let mut last_request = self.tx_set_last_request.write().await;
                                    match last_request.get_mut(h) {
                                        Some(st)
                                            if now.duration_since(st.last_request)
                                                < std::time::Duration::from_millis(500) =>
                                        {
                                            false
                                        }
                                        Some(st) => {
                                            st.last_request = now;
                                            true
                                        }
                                        None => {
                                            last_request.insert(
                                                *h,
                                                super::types::TxSetRequestState {
                                                    last_request: now,
                                                    first_requested: now,
                                                    next_peer_offset: 0,
                                                },
                                            );
                                            true
                                        }
                                    }
                                };
                                if should_send {
                                    let request =
                                        StellarMessage::GetTxSet(stellar_xdr::Uint256(h.0));
                                    if overlay.try_send_to(peer, request).is_ok() {
                                        tracing::debug!(
                                            target: "maxtps_fetch",
                                            hash = %h,
                                            peer = %peer,
                                            "direct GetTxSet to nominate sender"
                                        );
                                    }
                                }
                            }
                        }
                    }
                }
                self.request_pending_tx_sets().await;
            }
            EnvelopeState::Discarded => {
                // Standalone manual-close mode: silently dropped (no metrics, no logging at info level).
                tracing::trace!(
                    slot,
                    "SCP envelope discarded (standalone manual-close mode)"
                );
            }
        }
    }

    /// Increment the per-reason post-verify counter.
    fn record_post_verify_reason(&self, reason: henyey_herder::scp_verify::PostVerifyReason) {
        self.scp_pv_counters[reason].fetch_add(1, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod pump_tests {
    use super::App;
    use henyey_herder::scp_verify::{PipelinedIntake, Verdict, VerifiedEnvelope};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use stellar_xdr::{
        NodeId, PublicKey as XdrPublicKey, ScpBallot, ScpEnvelope, ScpStatement,
        ScpStatementPledges, ScpStatementPrepare, Signature, Uint256, Value,
    };

    fn ve(slot: u64) -> VerifiedEnvelope {
        let node_id = NodeId(XdrPublicKey::PublicKeyTypeEd25519(Uint256([1u8; 32])));
        let value = Value(vec![].try_into().unwrap());
        let pledges = ScpStatementPledges::Prepare(ScpStatementPrepare {
            quorum_set_hash: stellar_xdr::Hash([0u8; 32]),
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
            intake: PipelinedIntake::from_local(
                ScpEnvelope {
                    statement,
                    signature: Signature(vec![0u8; 64].try_into().unwrap()),
                },
                slot,
                false,
            ),
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

/// Companion regression coverage for #3623: the SCP-intake channel the event
/// loop wires up via `OverlayManager::subscribe_scp()` must be a BOUNDED
/// `tokio::sync::mpsc::Receiver` with a finite capacity, so a stalled loop
/// cannot accumulate SCP envelopes without limit and OOM the validator. The
/// primary mechanism reproduction lives in
/// `henyey_overlay::manager::tests::test_scp_channel_bounded_drops_when_receiver_stalled`;
/// this is the lighter-weight app-boundary check that the wiring is bounded.
#[cfg(test)]
mod scp_intake_bound_tests {
    use henyey_overlay::{LocalNode, OverlayConfig, OverlayManager, SCP_CHANNEL_CAPACITY};

    /// The receiver returned to the event loop is the bounded type with a
    /// finite max capacity equal to `SCP_CHANNEL_CAPACITY`. On origin/main the
    /// channel is `unbounded_channel()`, whose `Receiver` has no `max_capacity`
    /// at all — so the bounded type itself is the assertion.
    #[tokio::test]
    async fn test_scp_intake_channel_is_bounded() {
        let secret = henyey_crypto::SecretKey::generate();
        let manager =
            OverlayManager::new(OverlayConfig::default(), LocalNode::new_testnet(secret)).unwrap();

        let rx = manager
            .subscribe_scp()
            .await
            .expect("first subscribe_scp() returns the receiver");

        // A bounded `mpsc::Receiver` exposes a finite `max_capacity`. An
        // unbounded receiver does not have this method — so this both asserts
        // the value and pins the bounded type.
        assert_eq!(
            rx.max_capacity(),
            SCP_CHANNEL_CAPACITY,
            "SCP intake channel must be bounded at SCP_CHANNEL_CAPACITY"
        );

        // Finite, non-zero — a flood without draining can never exceed this.
        assert!(rx.max_capacity() > 0 && rx.max_capacity() <= SCP_CHANNEL_CAPACITY);
    }
}

#[cfg(test)]
mod scp_dedup_pipeline_tests {
    use super::App;
    use henyey_overlay::OverlayMessage;
    use stellar_xdr::{
        Hash as XdrHash, Limits, NodeId as XdrNodeId, PublicKey as XdrPublicKey, ScpBallot,
        ScpEnvelope, ScpStatement, ScpStatementPledges, ScpStatementPrepare,
        Signature as XdrSignature, StellarMessage, StellarValue, StellarValueExt, TimePoint,
        Uint256, Value, WriteXdr,
    };

    fn value_with_close_time(close_time: u64) -> Value {
        let sv = StellarValue {
            tx_set_hash: XdrHash([0u8; 32]),
            close_time: TimePoint(close_time),
            upgrades: vec![].try_into().unwrap(),
            ext: StellarValueExt::Basic,
        };
        Value(sv.to_xdr(Limits::none()).unwrap().try_into().unwrap())
    }

    fn make_overlay_scp_msg(envelope: ScpEnvelope) -> OverlayMessage {
        let peer_id =
            henyey_overlay::PeerId(XdrPublicKey::PublicKeyTypeEd25519(Uint256([0xAA; 32])));
        OverlayMessage::new(
            peer_id,
            StellarMessage::ScpMessage(envelope),
            std::time::Instant::now(),
        )
    }

    fn make_test_envelope(slot: u64, close_time: u64) -> ScpEnvelope {
        let node_id = XdrNodeId(XdrPublicKey::PublicKeyTypeEd25519(Uint256([1u8; 32])));
        ScpEnvelope {
            statement: ScpStatement {
                node_id,
                slot_index: slot,
                pledges: ScpStatementPledges::Prepare(ScpStatementPrepare {
                    quorum_set_hash: XdrHash([0u8; 32]),
                    ballot: ScpBallot {
                        counter: 1,
                        value: value_with_close_time(close_time),
                    },
                    prepared: None,
                    prepared_prime: None,
                    n_c: 0,
                    n_h: 0,
                }),
            },
            signature: XdrSignature(vec![0u8; 64].try_into().unwrap()),
        }
    }

    /// Pipeline test: pump_scp_intake handles pre-filter reject correctly.
    ///
    /// The herder is in Booting state (can't receive SCP), so the pre-filter
    /// rejects. With the RAII token model:
    /// 1. First call: check_and_insert passes (new hash), inserts entry, returns
    ///    token. Pre-filter rejects → token dropped at end of pump_scp_intake →
    ///    entry becomes a tombstone (Weak expired).
    /// 2. Second call: check_and_insert finds expired Weak, cleans tombstone,
    ///    re-inserts → passes again (no dedup rejection).
    /// 3. The dedup counter stays at 0 (no live duplicates detected).
    #[tokio::test]
    async fn test_pump_scp_intake_no_poison_on_prefilter_reject() {
        let dir = tempfile::tempdir().expect("temp dir");
        let db_path = dir.path().join("rs-stellar-dedup-test.db");
        let config = crate::config::ConfigBuilder::new()
            .database_path(db_path)
            .build();

        let app = App::new(config).await.unwrap();
        let mut verified_rx = app.herder.take_verified_rx().expect("take verified_rx");

        let envelope = make_test_envelope(100, 2_000_000_000);
        let msg = make_overlay_scp_msg(envelope);

        // Herder is in Booting → pre-filter rejects → token dropped → tombstone.
        app.pump_scp_intake(msg.clone(), &mut verified_rx).await;

        // Second call: tombstone cleaned, re-inserts. No dedup rejection.
        app.pump_scp_intake(msg, &mut verified_rx).await;
        assert_eq!(app.scp_scheduled.dedup_count(), 0, "no dedup rejections");
    }

    /// #3625 (app-side consumer touch): when the SCP consumer drains a
    /// token-bearing `OverlayMessage` via `pump_scp_intake`, the attached
    /// `FlowControlRelease` fires `end_message_processing`. Draining a full
    /// 40-message batch (sharing one `FlowControl`) therefore yields exactly one
    /// `SEND_MORE_EXTENDED` grant for the batch — proving the credit is released
    /// at consumer-drain time (not enqueue), and that the release fires on every
    /// drain path including the herder-Booting pre-filter-reject path.
    #[tokio::test]
    async fn test_scp_consumer_drain_releases_flow_control_credit() {
        use henyey_overlay::{FlowControl, FlowControlConfig};
        use std::sync::Arc;

        let dir = tempfile::tempdir().expect("temp dir");
        let db_path = dir.path().join("rs-stellar-drain-release.db");
        let config = crate::config::ConfigBuilder::new()
            .database_path(db_path)
            .build();
        let app = App::new(config).await.unwrap();
        let mut verified_rx = app.herder.take_verified_rx().expect("take verified_rx");

        let fc_config = FlowControlConfig::default();
        let batch = fc_config.flow_control_send_more_batch_size; // 40
        let flow_control = Arc::new(FlowControl::new(fc_config));

        // Observer channel for granted SEND_MORE_EXTENDED num_messages.
        let (grant_tx, grant_rx) = std::sync::mpsc::channel::<u32>();

        // Drain a full batch of DISTINCT token-bearing SCP messages. The herder
        // is in Booting → pre-filter rejects, but the flow-control token still
        // releases at the end of pump_scp_intake (Drop on every path).
        for slot in 0..batch {
            let envelope = make_test_envelope(1000 + slot, 2_000_000_000);
            let mut msg = make_overlay_scp_msg(envelope);
            msg.attach_test_flow_release(Arc::clone(&flow_control), grant_tx.clone());
            app.pump_scp_intake(msg, &mut verified_rx).await;
            // Let the grant-observer forwarding task run.
            tokio::task::yield_now().await;
        }
        // Allow the final forwarding task to deliver before asserting.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        drop(grant_tx);

        let grants: Vec<u32> = grant_rx.try_iter().collect();
        assert_eq!(
            grants.len(),
            1,
            "draining a {batch}-message batch via the SCP consumer must grant \
             exactly one SEND_MORE_EXTENDED (got {grants:?})"
        );
        assert_eq!(
            grants[0] as u64, batch,
            "the grant must request the full batch of flood messages"
        );
    }

    /// Pipeline test: pump_scp_intake dedup works end-to-end with dispatch.
    ///
    /// Sets up herder in Tracking state with proper tracking slot so the
    /// pre-filter accepts envelopes. Tests:
    /// 1. First pump_scp_intake: dispatches → token stored in intake → entry live
    /// 2. Second pump_scp_intake: dedup rejects (entry alive) → counter increments
    /// 3. process_verified: intake dropped → token dropped → entry expires
    /// 4. Third pump_scp_intake: re-dispatch succeeds (tombstone cleaned)
    #[tokio::test]
    async fn test_pump_scp_intake_dedup_full_dispatch() {
        use henyey_herder::HerderState;

        let dir = tempfile::tempdir().expect("temp dir");
        let db_path = dir.path().join("rs-stellar-dedup-dispatch.db");
        let config = crate::config::ConfigBuilder::new()
            .database_path(db_path)
            .build();

        let app = App::new(config).await.unwrap();
        let mut verified_rx = app.herder.take_verified_rx().expect("take verified_rx");

        // Configure herder for accepting envelopes.
        // Transition: Booting → Syncing → Tracking
        app.herder.set_state(HerderState::Syncing);
        app.herder.set_state(HerderState::Tracking);

        // Set tracking state so the close-time and slot-range pre-filters pass.
        let tracking_slot = 101u64;
        let close_time = 2_000_000_000u64;
        app.herder
            .set_tracking_for_testing(tracking_slot, close_time);
        app.herder
            .set_pending_current_slot_for_testing(tracking_slot);

        // Set the test clock so close-time checks use our controlled time.
        app.herder.set_test_clock_seconds(close_time + 5);

        // Build an envelope for the tracking slot with valid close time.
        let envelope = make_test_envelope(tracking_slot, close_time + 5);
        let msg = make_overlay_scp_msg(envelope.clone());

        // --- 1. First dispatch: dedup passes, pre-filter accepts, token in intake ---
        app.pump_scp_intake(msg.clone(), &mut verified_rx).await;
        assert_eq!(
            app.scp_scheduled.len(),
            1,
            "entry should be in cache after dispatch"
        );

        // --- 2. Second call: dedup rejects (token still alive in verifier channel) ---
        app.pump_scp_intake(msg.clone(), &mut verified_rx).await;
        assert_eq!(app.scp_scheduled.dedup_count(), 1, "one dedup rejection");
        assert_eq!(app.scp_scheduled.len(), 1, "cache unchanged");

        // --- 3. Drain verified_rx and call process_verified ---
        let ve = tokio::time::timeout(std::time::Duration::from_secs(5), verified_rx.recv())
            .await
            .expect("timeout waiting for verified envelope")
            .expect("verified_rx closed");

        // Before process_verified: entry is still live (token in VerifiedEnvelope).
        assert_eq!(
            app.scp_scheduled.len(),
            1,
            "entry still in cache before process_verified"
        );

        // process_verified consumes `ve` → intake dropped → token dropped →
        // entry becomes a tombstone.
        app.process_verified(ve).await;

        // --- 4. Re-dispatch: tombstone cleaned, new entry inserted ---
        app.pump_scp_intake(msg, &mut verified_rx).await;
        assert_eq!(
            app.scp_scheduled.len(),
            1,
            "re-dispatch succeeds after token drop"
        );
        // Dedup count unchanged (still 1 from step 2).
        assert_eq!(
            app.scp_scheduled.dedup_count(),
            1,
            "dedup count unchanged on re-dispatch"
        );
    }

    /// Parity test: `scp_messages_received` is incremented post-cache.
    ///
    /// Stellar-core's `HerderImpl.cpp:810 mSCPMetrics.mEnvelopeReceive.Mark()`
    /// fires after `checkScheduledAndCache` (i.e., on cache-miss only) but
    /// before any validity gate. In henyey the corresponding placement is
    /// after `scp_scheduled.check_and_insert` returns `Some(token)` and
    /// before `pre_filter_scp_envelope`. See issue #2644.
    #[tokio::test]
    async fn test_scp_messages_received_increments_post_cache() {
        use henyey_herder::HerderState;
        use std::sync::atomic::Ordering;

        let dir = tempfile::tempdir().expect("temp dir");
        let db_path = dir.path().join("rs-stellar-counter-parity.db");
        let config = crate::config::ConfigBuilder::new()
            .database_path(db_path)
            .build();

        let app = App::new(config).await.unwrap();
        let mut verified_rx = app.herder.take_verified_rx().expect("take verified_rx");

        // Configure herder so the pre-filter accepts envelopes for the
        // first phase of the test (cache-miss path).
        app.herder.set_state(HerderState::Syncing);
        app.herder.set_state(HerderState::Tracking);
        let tracking_slot = 101u64;
        let close_time = 2_000_000_000u64;
        app.herder
            .set_tracking_for_testing(tracking_slot, close_time);
        app.herder
            .set_pending_current_slot_for_testing(tracking_slot);
        app.herder.set_test_clock_seconds(close_time + 5);

        assert_eq!(
            app.scp_messages_received.load(Ordering::Relaxed),
            0,
            "baseline counter == 0"
        );

        // --- Cache-miss path: counter increments by 1 ---
        let envelope = make_test_envelope(tracking_slot, close_time + 5);
        let msg = make_overlay_scp_msg(envelope);
        app.pump_scp_intake(msg.clone(), &mut verified_rx).await;
        assert_eq!(
            app.scp_messages_received.load(Ordering::Relaxed),
            1,
            "cache-miss bumps counter"
        );

        // --- Cache-hit path: token still alive → check_and_insert returns
        // None → counter does NOT increment ---
        app.pump_scp_intake(msg, &mut verified_rx).await;
        assert_eq!(
            app.scp_messages_received.load(Ordering::Relaxed),
            1,
            "cache-hit (in-flight duplicate) does NOT bump counter"
        );

        // --- Pre-filter-reject path: counter still increments. We construct
        // a fresh envelope (different slot → different hash, so the cache
        // is missed) with a slot that the pre-filter will reject. Herder is
        // in Tracking, so envelopes for very-old slots are rejected with
        // Range. The counter must still tick — stellar-core's
        // `mEnvelopeReceive.Mark()` fires before any validity check.
        let stale_envelope = make_test_envelope(1, close_time + 5);
        let stale_msg = make_overlay_scp_msg(stale_envelope);
        app.pump_scp_intake(stale_msg, &mut verified_rx).await;
        assert_eq!(
            app.scp_messages_received.load(Ordering::Relaxed),
            2,
            "pre-filter reject still bumps counter (parity with mEnvelopeReceive.Mark())"
        );
    }
}

#[cfg(test)]
mod rate_limit_tests {
    use super::query_rate_limit_window;
    use std::time::Duration;

    #[test]
    fn test_query_rate_limit_window_4500ms() {
        // Bug case: premature truncation gave 4s * 12 = 48s.
        // Correct: 4500 * 12 = 54000ms / 1000 = 54s.
        let window = query_rate_limit_window(Duration::from_millis(4500));
        assert_eq!(window, Duration::from_secs(54));
    }

    #[test]
    fn test_query_rate_limit_window_4300ms() {
        // Non-round: 4300 * 12 = 51600ms / 1000 = 51s (truncation after multiply).
        let window = query_rate_limit_window(Duration::from_millis(4300));
        assert_eq!(window, Duration::from_secs(51));
    }

    #[test]
    fn test_query_rate_limit_window_4999ms() {
        // Boundary: 4999 * 12 = 59988ms / 1000 = 59s.
        // Proves truncation happens after multiplication, not before.
        let window = query_rate_limit_window(Duration::from_millis(4999));
        assert_eq!(window, Duration::from_secs(59));
    }

    #[test]
    fn test_query_rate_limit_window_5000ms() {
        // Standard/fallback: 5000 * 12 = 60000ms / 1000 = 60s.
        let window = query_rate_limit_window(Duration::from_secs(5));
        assert_eq!(window, Duration::from_secs(60));
    }
}

#[cfg(test)]
mod max_tx_size_tests {
    use henyey_herder::flow_control::{compute_max_tx_size, MAX_CLASSIC_TX_SIZE_BYTES};
    use std::sync::atomic::{AtomicU32, Ordering};

    /// Mirrors the pure logic of `App::update_max_tx_size_bytes` without
    /// needing a full `App` instance. The real method is three lines:
    ///   new_max = compute_max_tx_size(protocol_version, soroban_tx_max)
    ///   old_max = self.max_tx_size_bytes.swap(new_max)
    ///   return new_max.saturating_sub(old_max)
    ///
    /// The real `refresh_max_tx_size_bytes` (async wrapper) and its call-site
    /// wiring (startup, catchup, ledger close) are covered indirectly by the
    /// existing ledger close integration tests which now go through the helper.
    fn update(atomic: &AtomicU32, protocol_version: u32, soroban_tx_max: Option<u32>) -> u32 {
        let new_max = compute_max_tx_size(protocol_version, soroban_tx_max);
        let old_max = atomic.swap(new_max, Ordering::Relaxed);
        new_max.saturating_sub(old_max)
    }

    #[test]
    fn test_startup_soroban_initializes_correctly() {
        // Node starts on a Soroban-enabled protocol with tx_max > classic default.
        let atomic = AtomicU32::new(MAX_CLASSIC_TX_SIZE_BYTES);
        let soroban_max: u32 = 130_000;
        let expected = compute_max_tx_size(25, Some(soroban_max));
        let increase = update(&atomic, 25, Some(soroban_max));

        assert_eq!(atomic.load(Ordering::Relaxed), expected);
        assert_eq!(increase, expected - MAX_CLASSIC_TX_SIZE_BYTES);
        assert!(increase > 0, "Soroban max should exceed classic default");
    }

    #[test]
    fn test_startup_classic_no_change() {
        // Node starts on pre-Soroban protocol — no change from default.
        let atomic = AtomicU32::new(MAX_CLASSIC_TX_SIZE_BYTES);
        let increase = update(&atomic, 19, None);

        assert_eq!(atomic.load(Ordering::Relaxed), MAX_CLASSIC_TX_SIZE_BYTES);
        assert_eq!(increase, 0);
    }

    #[test]
    fn test_decrease_no_notification() {
        // Max decreases (hypothetical config change) — diff is 0, no notification.
        let high_max = compute_max_tx_size(25, Some(200_000));
        let low_max = compute_max_tx_size(25, Some(150_000));
        let atomic = AtomicU32::new(high_max);
        let increase = update(&atomic, 25, Some(150_000));

        assert_eq!(atomic.load(Ordering::Relaxed), low_max);
        assert_eq!(increase, 0);
    }

    #[test]
    fn test_increase_after_catchup() {
        // After catchup, protocol advanced from classic to Soroban.
        let atomic = AtomicU32::new(MAX_CLASSIC_TX_SIZE_BYTES);
        let soroban_max: u32 = 200_000;
        let expected = compute_max_tx_size(25, Some(soroban_max));
        let increase = update(&atomic, 25, Some(soroban_max));

        assert_eq!(atomic.load(Ordering::Relaxed), expected);
        assert_eq!(increase, expected - MAX_CLASSIC_TX_SIZE_BYTES);
        assert!(increase > 0);
    }

    #[test]
    fn test_no_spurious_notification_after_correct_init() {
        // If startup correctly initialized to Soroban max, a subsequent
        // ledger close with the same config should produce diff = 0.
        let soroban_max: u32 = 130_000;
        let atomic = AtomicU32::new(MAX_CLASSIC_TX_SIZE_BYTES);

        // Startup refresh
        let startup_increase = update(&atomic, 25, Some(soroban_max));
        assert!(startup_increase > 0);

        // First ledger close — no upgrade, same config
        let close_increase = update(&atomic, 25, Some(soroban_max));
        assert_eq!(close_increase, 0);
    }

    #[test]
    fn test_soroban_below_classic_uses_classic() {
        // Soroban max is below classic max — compute_max_tx_size returns classic.
        let atomic = AtomicU32::new(MAX_CLASSIC_TX_SIZE_BYTES);
        let small_soroban: u32 = 50_000;
        let increase = update(&atomic, 25, Some(small_soroban));

        assert_eq!(atomic.load(Ordering::Relaxed), MAX_CLASSIC_TX_SIZE_BYTES);
        assert_eq!(increase, 0);
    }
}

#[cfg(test)]
mod forget_flood_record_tests {
    use super::should_forget_flood_record;
    use henyey_herder::scp_verify::PostVerifyReason as R;
    use henyey_herder::EnvelopeState;

    /// Every (EnvelopeState, PostVerifyReason) combination that maps to
    /// stellar-core's ENVELOPE_STATUS_DISCARDED should return true.
    #[test]
    fn test_forget_cases() {
        // Terminal reject states — always forget (unless SelfMessage / Deferred).
        for state in [
            EnvelopeState::TooOld,
            EnvelopeState::InvalidSignature,
            EnvelopeState::Invalid,
            EnvelopeState::Discarded,
        ] {
            for reason in R::ALL {
                if matches!(reason, R::SelfMessage) {
                    continue;
                }
                assert!(
                    should_forget_flood_record(state, reason),
                    "expected forget for ({state:?}, {reason:?})"
                );
            }
        }
    }

    /// Envelopes that should NOT cause forget: Valid, Pending, Fetching, Duplicate, Deferred.
    #[test]
    fn test_no_forget_valid_pending_fetching() {
        for state in [
            EnvelopeState::Valid,
            EnvelopeState::Pending,
            EnvelopeState::Fetching,
            EnvelopeState::Duplicate,
        ] {
            for reason in R::ALL {
                assert!(
                    !should_forget_flood_record(state, reason),
                    "expected no forget for ({state:?}, {reason:?})"
                );
            }
        }
    }

    /// Deferred is a henyey-specific closing gate — never forget.
    #[test]
    fn test_no_forget_deferred() {
        for reason in R::ALL {
            assert!(
                !should_forget_flood_record(EnvelopeState::Deferred, reason),
                "expected no forget for (Deferred, {reason:?})"
            );
        }
    }

    /// SelfMessage reason — never forget regardless of state.
    #[test]
    fn test_no_forget_self_message() {
        for state in [
            EnvelopeState::TooOld,
            EnvelopeState::InvalidSignature,
            EnvelopeState::Invalid,
        ] {
            assert!(
                !should_forget_flood_record(state, R::SelfMessage),
                "expected no forget for ({state:?}, SelfMessage)"
            );
        }
    }
}

/// Tests for [`FATAL_WIPE_FIELD`] — the monitoring contract.
///
/// These tests guard that `trigger_fatal_shutdown()` emits the
/// `fatal_wipe_required` structured field and that both the Text and JSON
/// `tracing_subscriber::fmt` formatters render it in grep-able form.
#[cfg(test)]
mod fatal_wipe_field_tests {
    use super::FATAL_WIPE_FIELD;
    use std::io;
    use std::sync::{Arc, Mutex};

    /// Verify `trigger_fatal_shutdown()` emits the structured field
    /// `fatal_wipe_required = true` via a capturing subscriber.
    ///
    /// This calls the actual `App::trigger_fatal_shutdown()` method, not a raw
    /// `tracing::error!`, so it will break if the field is ever removed from
    /// the method.
    #[tokio::test]
    async fn test_fatal_shutdown_emits_wipe_field_structured() {
        use std::sync::atomic::Ordering;
        use tracing::{
            field::{Field, Visit},
            subscriber::with_default,
            Event, Metadata, Subscriber,
        };

        #[derive(Default)]
        struct CapturedBool {
            value: Option<bool>,
        }
        impl Visit for CapturedBool {
            fn record_bool(&mut self, field: &Field, value: bool) {
                if field.name() == FATAL_WIPE_FIELD {
                    self.value = Some(value);
                }
            }
            fn record_debug(&mut self, _: &Field, _: &dyn std::fmt::Debug) {}
        }

        #[derive(Default, Clone)]
        struct WipeFieldSubscriber {
            captured: Arc<Mutex<Option<bool>>>,
        }
        impl Subscriber for WipeFieldSubscriber {
            fn enabled(&self, _: &Metadata<'_>) -> bool {
                true
            }
            fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
                tracing::span::Id::from_u64(1)
            }
            fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
            fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
            fn event(&self, event: &Event<'_>) {
                let mut cap = CapturedBool::default();
                event.record(&mut cap);
                if let Some(v) = cap.value {
                    *self.captured.lock().unwrap() = Some(v);
                }
            }
            fn enter(&self, _: &tracing::span::Id) {}
            fn exit(&self, _: &tracing::span::Id) {}
        }

        let dir = tempfile::tempdir().expect("temp dir");
        let db_path = dir.path().join("rs-stellar-test.db");
        let config = crate::config::ConfigBuilder::new()
            .database_path(db_path)
            .build();
        let app = super::super::App::new(config).await.unwrap();

        let sub = WipeFieldSubscriber::default();
        let captured = sub.captured.clone();

        with_default(sub, || {
            app.trigger_fatal_shutdown("test reason");
        });

        assert_eq!(
            *captured.lock().unwrap(),
            Some(true),
            "trigger_fatal_shutdown must emit {FATAL_WIPE_FIELD}=true"
        );
        assert!(
            app.fatal_state_failure.load(Ordering::SeqCst),
            "fatal_state_failure must be set"
        );
    }

    /// #3478: `trigger_recoverable_shutdown()` must NOT emit
    /// `fatal_wipe_required` and must NOT set `fatal_state_failure` — a
    /// transient ENOSPC is environmental, the on-disk state is intact, and a
    /// wipe would be wrong. The inverse of the fatal-shutdown contract above.
    #[tokio::test]
    async fn test_recoverable_shutdown_does_not_emit_wipe_field_3478() {
        use std::sync::atomic::Ordering;
        use tracing::{
            field::{Field, Visit},
            subscriber::with_default,
            Event, Metadata, Subscriber,
        };

        #[derive(Default)]
        struct CapturedBool {
            value: Option<bool>,
        }
        impl Visit for CapturedBool {
            fn record_bool(&mut self, field: &Field, value: bool) {
                if field.name() == FATAL_WIPE_FIELD {
                    self.value = Some(value);
                }
            }
            fn record_debug(&mut self, _: &Field, _: &dyn std::fmt::Debug) {}
        }

        #[derive(Default, Clone)]
        struct WipeFieldSubscriber {
            captured: Arc<Mutex<Option<bool>>>,
        }
        impl Subscriber for WipeFieldSubscriber {
            fn enabled(&self, _: &Metadata<'_>) -> bool {
                true
            }
            fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
                tracing::span::Id::from_u64(1)
            }
            fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
            fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
            fn event(&self, event: &Event<'_>) {
                let mut cap = CapturedBool::default();
                event.record(&mut cap);
                if let Some(v) = cap.value {
                    *self.captured.lock().unwrap() = Some(v);
                }
            }
            fn enter(&self, _: &tracing::span::Id) {}
            fn exit(&self, _: &tracing::span::Id) {}
        }

        let dir = tempfile::tempdir().expect("temp dir");
        let db_path = dir.path().join("rs-stellar-test.db");
        let config = crate::config::ConfigBuilder::new()
            .database_path(db_path)
            .build();
        let app = super::super::App::new(config).await.unwrap();

        let sub = WipeFieldSubscriber::default();
        let captured = sub.captured.clone();

        with_default(sub, || {
            app.trigger_recoverable_shutdown("disk full (test)");
        });

        assert_eq!(
            *captured.lock().unwrap(),
            None,
            "trigger_recoverable_shutdown must NOT emit {FATAL_WIPE_FIELD}"
        );
        assert!(
            !app.fatal_state_failure.load(Ordering::SeqCst),
            "trigger_recoverable_shutdown must NOT set fatal_state_failure (recoverable)"
        );
    }

    /// Verify the Text formatter renders the field as `fatal_wipe_required=true`,
    /// matching the production formatter construction in `logging.rs:334-341`.
    #[test]
    fn test_fatal_shutdown_emits_wipe_field_text_format() {
        use tracing::subscriber::with_default;
        use tracing_subscriber::{fmt, layer::SubscriberExt, EnvFilter};

        let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
        let buf_clone = buf.clone();

        // Mirror production Text formatter construction (logging.rs:334-341).
        let fmt_layer = fmt::layer()
            .with_writer(move || -> Box<dyn io::Write> { Box::new(BufWriter(buf_clone.clone())) })
            .with_ansi(false)
            .with_target(true);

        let subscriber = tracing_subscriber::registry()
            .with(EnvFilter::new("error"))
            .with(fmt_layer);

        with_default(subscriber, || {
            tracing::error!(fatal_wipe_required = true, "test fatal shutdown");
        });

        let output = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
        assert!(
            output.contains("fatal_wipe_required=true"),
            "Text format must render field as 'fatal_wipe_required=true' for grep. Got: {output}"
        );
    }

    /// Verify the JSON formatter renders the field as `"fatal_wipe_required":true`,
    /// matching the production formatter construction in `logging.rs:353-357`.
    #[test]
    fn test_fatal_shutdown_emits_wipe_field_json_format() {
        use tracing::subscriber::with_default;
        use tracing_subscriber::{fmt, layer::SubscriberExt, EnvFilter};

        let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
        let buf_clone = buf.clone();

        // Mirror production JSON formatter construction (logging.rs:353-357).
        let fmt_layer = fmt::layer()
            .with_writer(move || -> Box<dyn io::Write> { Box::new(BufWriter(buf_clone.clone())) })
            .json()
            .with_span_list(true)
            .with_current_span(true);

        let subscriber = tracing_subscriber::registry()
            .with(EnvFilter::new("error"))
            .with(fmt_layer);

        with_default(subscriber, || {
            tracing::error!(fatal_wipe_required = true, "test fatal shutdown");
        });

        let output = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
        assert!(
            output.contains("\"fatal_wipe_required\":true"),
            "JSON format must render field as '\"fatal_wipe_required\":true' for grep. Got: {output}"
        );
    }

    /// A `Write` adapter that appends to a shared `Vec<u8>`.
    #[derive(Clone)]
    struct BufWriter(Arc<Mutex<Vec<u8>>>);

    impl io::Write for BufWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
}

/// Tests for [`HEARTBEAT_FIELD`] — the monitoring contract.
///
/// These tests guard that the summary heartbeat event emits the `heartbeat`
/// structured field and that both the Text and JSON `tracing_subscriber::fmt`
/// formatters render it in grep-able form.
#[cfg(test)]
mod heartbeat_field_tests {
    use super::{emit_heartbeat_log, HEARTBEAT_FIELD};
    use std::io;
    use std::sync::{Arc, Mutex};

    /// Call `emit_heartbeat_log` with representative dummy values.
    fn emit_test_heartbeat() {
        emit_heartbeat_log(
            100,  // tracking_slot
            99,   // ledger
            98,   // latest_ext
            5,    // peers
            true, // heard_from_quorum
            true, // is_v_blocking
            1000, // scp_total
            10,   // scp_since_last
            5,    // scp_silent_secs
            50,   // scp_sent
            20,   // scp_sent_nom
            15,   // scp_sent_prep
            10,   // scp_sent_conf
            5,    // scp_sent_ext
            97,   // peer_max_verified
            3,    // peer_gap
        );
    }

    /// Verify `emit_heartbeat_log` emits the structured field
    /// `heartbeat = true` via a capturing subscriber.
    ///
    /// This calls the real production helper, not a raw `tracing::info!`,
    /// so it will break if the field is ever removed from `emit_heartbeat_log`.
    #[test]
    fn test_heartbeat_emits_field_structured() {
        use tracing::{
            field::{Field, Visit},
            subscriber::with_default,
            Event, Metadata, Subscriber,
        };

        #[derive(Default)]
        struct CapturedBool {
            value: Option<bool>,
        }
        impl Visit for CapturedBool {
            fn record_bool(&mut self, field: &Field, value: bool) {
                if field.name() == HEARTBEAT_FIELD {
                    self.value = Some(value);
                }
            }
            fn record_debug(&mut self, _: &Field, _: &dyn std::fmt::Debug) {}
        }

        #[derive(Default, Clone)]
        struct HeartbeatFieldSubscriber {
            captured: Arc<Mutex<Option<bool>>>,
        }
        impl Subscriber for HeartbeatFieldSubscriber {
            fn enabled(&self, _: &Metadata<'_>) -> bool {
                true
            }
            fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
                tracing::span::Id::from_u64(1)
            }
            fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
            fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
            fn event(&self, event: &Event<'_>) {
                let mut cap = CapturedBool::default();
                event.record(&mut cap);
                if let Some(v) = cap.value {
                    *self.captured.lock().unwrap() = Some(v);
                }
            }
            fn enter(&self, _: &tracing::span::Id) {}
            fn exit(&self, _: &tracing::span::Id) {}
        }

        let sub = HeartbeatFieldSubscriber::default();
        let captured = sub.captured.clone();

        with_default(sub, || {
            emit_test_heartbeat();
        });

        assert_eq!(
            *captured.lock().unwrap(),
            Some(true),
            "emit_heartbeat_log must emit {HEARTBEAT_FIELD}=true"
        );
    }

    /// Verify the Text formatter renders the field as `heartbeat=true`,
    /// matching the production formatter construction in `logging.rs:334-341`.
    #[test]
    fn test_heartbeat_emits_field_text_format() {
        use tracing::subscriber::with_default;
        use tracing_subscriber::{fmt, layer::SubscriberExt, EnvFilter};

        let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
        let buf_clone = buf.clone();

        // Mirror production Text formatter construction (logging.rs:334-341).
        let fmt_layer = fmt::layer()
            .with_writer(move || -> Box<dyn io::Write> { Box::new(BufWriter(buf_clone.clone())) })
            .with_ansi(false)
            .with_target(true);

        let subscriber = tracing_subscriber::registry()
            .with(EnvFilter::new("info"))
            .with(fmt_layer);

        with_default(subscriber, || {
            emit_test_heartbeat();
        });

        let output = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
        assert!(
            output.contains("heartbeat=true"),
            "Text format must render field as 'heartbeat=true' for grep. Got: {output}"
        );
        assert!(
            output.contains("Heartbeat"),
            "Text format must still contain the prose message 'Heartbeat'. Got: {output}"
        );
    }

    /// Verify the JSON formatter renders the field as `"heartbeat":true`,
    /// matching the production formatter construction in `logging.rs:353-357`.
    #[test]
    fn test_heartbeat_emits_field_json_format() {
        use tracing::subscriber::with_default;
        use tracing_subscriber::{fmt, layer::SubscriberExt, EnvFilter};

        let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
        let buf_clone = buf.clone();

        // Mirror production JSON formatter construction (logging.rs:353-357).
        let fmt_layer = fmt::layer()
            .with_writer(move || -> Box<dyn io::Write> { Box::new(BufWriter(buf_clone.clone())) })
            .json()
            .with_span_list(true)
            .with_current_span(true);

        let subscriber = tracing_subscriber::registry()
            .with(EnvFilter::new("info"))
            .with(fmt_layer);

        with_default(subscriber, || {
            emit_test_heartbeat();
        });

        let output = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
        assert!(
            output.contains("\"heartbeat\":true"),
            "JSON format must render field as '\"heartbeat\":true' for grep. Got: {output}"
        );
        assert!(
            output.contains("Heartbeat"),
            "JSON format must still contain the prose message 'Heartbeat'. Got: {output}"
        );
    }

    /// A `Write` adapter that appends to a shared `Vec<u8>`.
    #[derive(Clone)]
    struct BufWriter(Arc<Mutex<Vec<u8>>>);

    impl io::Write for BufWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tx_flood_forget_tests {
    use super::should_forget_tx_flood_record;
    use henyey_herder::TxQueueResult;

    #[test]
    fn test_added_does_not_forget() {
        assert!(!should_forget_tx_flood_record(&TxQueueResult::Added));
    }

    #[test]
    fn test_duplicate_does_not_forget() {
        assert!(!should_forget_tx_flood_record(&TxQueueResult::Duplicate));
    }

    #[test]
    fn test_queue_full_forgets() {
        assert!(should_forget_tx_flood_record(&TxQueueResult::QueueFull));
    }

    #[test]
    fn test_fee_too_low_forgets() {
        assert!(should_forget_tx_flood_record(&TxQueueResult::FeeTooLow));
    }

    #[test]
    fn test_invalid_forgets() {
        assert!(should_forget_tx_flood_record(&TxQueueResult::Invalid(None)));
        assert!(should_forget_tx_flood_record(&TxQueueResult::Invalid(
            Some(henyey_tx::TxResultCode::TxBadAuth,)
        )));
    }

    #[test]
    fn test_banned_forgets() {
        assert!(should_forget_tx_flood_record(&TxQueueResult::Banned));
    }

    #[test]
    fn test_filtered_forgets() {
        assert!(should_forget_tx_flood_record(&TxQueueResult::Filtered));
    }

    #[test]
    fn test_try_again_later_forgets() {
        assert!(should_forget_tx_flood_record(&TxQueueResult::TryAgainLater));
    }
}

#[cfg(test)]
mod tx_set_gc_offload_tests {
    //! Regression tests for #3532 — the tx_set_gc event-loop freeze.
    //!
    //! On `origin/main` the `tx_set_gc_interval.tick()` arm called
    //! `self.herder.purge_persisted_tx_sets()` inline on the tokio event-loop
    //! thread. A large persisted-tx-set table (or write contention) made that
    //! synchronous `BEGIN IMMEDIATE` purge block the loop for tens of seconds
    //! (observed 39s, watchdog_freeze). The fix offloads the purge via
    //! `spawn_blocking` behind a loop-local in-flight guard so the loop arm
    //! returns promptly and purges stay strictly serial (mirroring
    //! stellar-core's serial reschedule cadence).
    use super::{dispatch_peer_maintenance, dispatch_tx_set_gc};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Barrier};
    use std::thread::ThreadId;
    use std::time::{Duration, Instant};
    use tokio::task::JoinHandle;

    /// The dispatch must return promptly (loop not blocked) and the work must
    /// run on a DIFFERENT thread than the dispatcher (the event-loop thread).
    ///
    /// Fails on `origin/main`: the arm runs the purge inline, so the dispatch
    /// would block for the full ~1s barrier and the work thread id would equal
    /// the dispatcher's thread id.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_tx_set_gc_purge_runs_off_event_loop_thread() {
        let dispatcher_tid = std::thread::current().id();
        let work_tid: Arc<std::sync::Mutex<Option<ThreadId>>> =
            Arc::new(std::sync::Mutex::new(None));
        // Barrier of 2: the work closure waits until the test releases it, so
        // the work is guaranteed still-running while we assert prompt return.
        let barrier = Arc::new(Barrier::new(2));

        let mut slot: Option<JoinHandle<()>> = None;
        let work_tid_c = Arc::clone(&work_tid);
        let barrier_c = Arc::clone(&barrier);

        let start = Instant::now();
        dispatch_tx_set_gc(
            move || {
                *work_tid_c.lock().unwrap() = Some(std::thread::current().id());
                // Block to simulate a slow purge. If this ran inline on the
                // dispatcher thread, the dispatch call below would not return.
                barrier_c.wait();
            },
            &mut slot,
        );
        let dispatch_elapsed = start.elapsed();

        // (1) Dispatch returned promptly — the event loop was not blocked.
        assert!(
            dispatch_elapsed < Duration::from_millis(100),
            "dispatch_tx_set_gc blocked the caller for {:?} (expected <100ms); \
             purge was not offloaded off the event-loop thread",
            dispatch_elapsed
        );

        // Release the work closure and let the spawned task complete.
        barrier.wait();
        let handle = slot.take().expect("dispatch should have stored a handle");
        handle.await.expect("spawned purge task should complete");

        // (2) The work ran on a different thread than the dispatcher.
        let observed = work_tid
            .lock()
            .unwrap()
            .expect("work should have recorded a thread id");
        assert_ne!(
            observed, dispatcher_tid,
            "purge ran on the dispatcher (event-loop) thread; expected a spawn_blocking thread"
        );
    }

    /// A second dispatch while the first purge is still in-flight must NOT
    /// spawn a second concurrent purge (serial in-flight guard), mirroring
    /// stellar-core rescheduling the GC timer only after the purge returns.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_tx_set_gc_serial_no_overlap() {
        // Counts how many purge closures are running concurrently and the peak.
        let running = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let total = Arc::new(AtomicUsize::new(0));
        // Barrier of 2: test + first purge. The first purge holds here so it is
        // unambiguously still-running when the second dispatch is attempted.
        let barrier = Arc::new(Barrier::new(2));

        let mut slot: Option<JoinHandle<()>> = None;

        let make_work = || {
            let running = Arc::clone(&running);
            let peak = Arc::clone(&peak);
            let total = Arc::clone(&total);
            let barrier = Arc::clone(&barrier);
            move || {
                total.fetch_add(1, Ordering::SeqCst);
                let cur = running.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(cur, Ordering::SeqCst);
                barrier.wait();
                running.fetch_sub(1, Ordering::SeqCst);
            }
        };

        // First dispatch: spawns and starts running (blocks on the barrier).
        dispatch_tx_set_gc(make_work(), &mut slot);
        // Wait until the first purge is observably running before the 2nd tick.
        let spin_start = Instant::now();
        while running.load(Ordering::SeqCst) == 0 {
            assert!(
                spin_start.elapsed() < Duration::from_secs(5),
                "first purge never started"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }

        // Second dispatch while the first is still in-flight: must be skipped.
        dispatch_tx_set_gc(make_work(), &mut slot);

        // Give a skipped-or-spawned second task a moment to (not) start.
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            peak.load(Ordering::SeqCst),
            1,
            "a second purge ran concurrently with the first; in-flight guard failed"
        );

        // Release the first purge and let it finish.
        barrier.wait();
        let handle = slot.take().expect("a handle should be stored");
        handle.await.expect("first purge should complete");

        // Exactly one purge ran in total — the second tick was coalesced.
        assert_eq!(
            total.load(Ordering::SeqCst),
            1,
            "expected exactly one purge to run (second tick coalesced by guard)"
        );
    }

    // --- #3689: dispatch_peer_maintenance (phase=28 offload) ------------------
    //
    // Before #3689 the phase=28 arm did `self.maintain_peers().await` inline on
    // the event-loop thread. Under SQLite write-lock contention the inner
    // `db_blocking("remove-failed-peers")` retries up to busy_timeout=30s, and
    // the arm also awaits a 20s reconnect — both froze the loop, starving SCP →
    // lost sync. These tests cover the offload helper's public contract
    // (prompt return, off-thread, serial-coalesce), mirroring the
    // `dispatch_tx_set_gc` tests above.

    /// The dispatch must return promptly (loop not blocked) and the maintenance
    /// future must run on a DIFFERENT thread than the dispatcher (the event-loop
    /// thread).
    ///
    /// Fails on `origin/main`: there is no dispatcher — the arm awaits
    /// `maintain_peers()` inline, so the caller would block for the full barrier
    /// and the work would run on the dispatcher thread.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_peer_maintenance_runs_off_event_loop_thread() {
        let dispatcher_tid = std::thread::current().id();
        let work_tid: Arc<std::sync::Mutex<Option<ThreadId>>> =
            Arc::new(std::sync::Mutex::new(None));
        // Barrier of 2: the maintenance future blocks until the test releases it,
        // so it is guaranteed still-running while we assert prompt return.
        let barrier = Arc::new(Barrier::new(2));

        let mut slot: Option<JoinHandle<()>> = None;
        let work_tid_c = Arc::clone(&work_tid);
        let barrier_c = Arc::clone(&barrier);

        let start = Instant::now();
        dispatch_peer_maintenance(
            async move {
                *work_tid_c.lock().unwrap() = Some(std::thread::current().id());
                // Block on a blocking barrier to simulate a slow contended
                // maintenance round. If this ran inline on the dispatcher
                // thread, the dispatch call below would not return.
                barrier_c.wait();
            },
            &mut slot,
        );
        let dispatch_elapsed = start.elapsed();

        // (1) Dispatch returned promptly — the event loop was not blocked.
        assert!(
            dispatch_elapsed < Duration::from_millis(100),
            "dispatch_peer_maintenance blocked the caller for {:?} (expected <100ms); \
             peer maintenance was not offloaded off the event-loop thread",
            dispatch_elapsed
        );

        // Release the maintenance future and let the spawned task complete.
        barrier.wait();
        let handle = slot.take().expect("dispatch should have stored a handle");
        handle
            .await
            .expect("spawned maintenance task should complete");

        // (2) The work ran on a different thread than the dispatcher.
        let observed = work_tid
            .lock()
            .unwrap()
            .expect("work should have recorded a thread id");
        assert_ne!(
            observed, dispatcher_tid,
            "maintenance ran on the dispatcher (event-loop) thread; expected a spawned task thread"
        );
    }

    /// A second dispatch while the first maintenance round is still in-flight
    /// must NOT spawn a second concurrent round (serial in-flight guard), so two
    /// detached rounds never race the peer table / overlay connection pool.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_peer_maintenance_serial_no_overlap() {
        let running = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let total = Arc::new(AtomicUsize::new(0));
        // Barrier of 2: test + first round. The first round holds here so it is
        // unambiguously still-running when the second dispatch is attempted.
        let barrier = Arc::new(Barrier::new(2));

        let mut slot: Option<JoinHandle<()>> = None;

        let make_work = || {
            let running = Arc::clone(&running);
            let peak = Arc::clone(&peak);
            let total = Arc::clone(&total);
            let barrier = Arc::clone(&barrier);
            async move {
                total.fetch_add(1, Ordering::SeqCst);
                let cur = running.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(cur, Ordering::SeqCst);
                barrier.wait();
                running.fetch_sub(1, Ordering::SeqCst);
            }
        };

        // First dispatch: spawns and starts running (blocks on the barrier).
        dispatch_peer_maintenance(make_work(), &mut slot);
        // Wait until the first round is observably running before the 2nd tick.
        let spin_start = Instant::now();
        while running.load(Ordering::SeqCst) == 0 {
            assert!(
                spin_start.elapsed() < Duration::from_secs(5),
                "first maintenance round never started"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }

        // Second dispatch while the first is still in-flight: must be skipped.
        dispatch_peer_maintenance(make_work(), &mut slot);

        // Give a skipped-or-spawned second task a moment to (not) start.
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            peak.load(Ordering::SeqCst),
            1,
            "a second maintenance round ran concurrently with the first; in-flight guard failed"
        );

        // Release the first round and let it finish.
        barrier.wait();
        let handle = slot.take().expect("a handle should be stored");
        handle
            .await
            .expect("first maintenance round should complete");

        // Exactly one round ran in total — the second tick was coalesced.
        assert_eq!(
            total.load(Ordering::SeqCst),
            1,
            "expected exactly one maintenance round to run (second tick coalesced by guard)"
        );
    }

    /// Regression for #3689: a contended SQLite write lock must NOT stall the
    /// dispatch (and therefore the event loop). We hold a long-lived
    /// `BEGIN IMMEDIATE` write transaction on the app's DB from a SECOND rusqlite
    /// connection, then drive the REAL phase=28 path
    /// (`dispatch_peer_maintenance(app.maintain_peers())`) and assert the
    /// dispatch returns well under the 30s busy_timeout — inside a 2s watchdog so
    /// a regressed inline-await build FAILS via timeout rather than hanging ~30s.
    ///
    /// Pre-#3689 the arm awaited `maintain_peers()` inline: `remove-failed-peers`
    /// would contend with the held lock and retry up to busy_timeout=30s, so the
    /// caller blocked ~30s and the 2s watchdog would trip → FAIL. After the
    /// offload the dispatch returns immediately (the contention is absorbed by
    /// the detached task) → PASS.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_contended_peer_maintenance_does_not_stall_loop() {
        use crate::App;

        let dir = tempfile::tempdir().expect("temp dir");
        let db_path = dir.path().join("peer-maint-contention.db");
        let mut config = crate::config::ConfigBuilder::new()
            .database_path(&db_path)
            .build();
        // Hermetic: no overlay peers / no network so maintain_peers only touches
        // the `peers` table DELETE then returns (peer_count below threshold path
        // still only does brief overlay-lock work; no real connects in tests).
        config.is_compat_config = true;
        config.overlay.known_peers = vec![];
        config.overlay.target_outbound_peers = 0;
        config.overlay.max_outbound_peers = 0;
        let app = Arc::new(App::new(config).await.expect("build App"));

        // Resolve the actual on-disk DB file the app opened. App::new may append
        // a fixed filename or normalize the path; glob the temp dir for the .db.
        let app_db_file = {
            let mut found = db_path.clone();
            if !found.exists() {
                for entry in std::fs::read_dir(dir.path()).expect("read temp dir") {
                    let p = entry.expect("dir entry").path();
                    if p.extension().map(|e| e == "db").unwrap_or(false) {
                        found = p;
                        break;
                    }
                }
            }
            found
        };

        // Second connection holds a write lock for the duration via BEGIN
        // IMMEDIATE (acquires the RESERVED lock immediately). A short busy_timeout
        // here just avoids the holder itself blocking on open.
        let holder = rusqlite::Connection::open(&app_db_file).expect("open holder conn");
        holder
            .busy_timeout(Duration::from_millis(100))
            .expect("set holder busy_timeout");
        holder
            .execute_batch(
                "BEGIN IMMEDIATE; CREATE TABLE IF NOT EXISTS _lk(x); INSERT INTO _lk VALUES (1);",
            )
            .expect("acquire write lock via BEGIN IMMEDIATE");

        // Drive the real phase=28 dispatch: spawn app.maintain_peers() via the
        // offload helper and assert the DISPATCH returns promptly even though the
        // write lock is held (the detached task absorbs the busy_timeout retry).
        let mut slot: Option<JoinHandle<()>> = None;
        let app_for_task = Arc::clone(&app);

        let dispatch_fut = async {
            let start = Instant::now();
            dispatch_peer_maintenance(
                async move { app_for_task.maintain_peers().await },
                &mut slot,
            );
            start.elapsed()
        };

        let dispatch_elapsed = tokio::time::timeout(Duration::from_secs(2), dispatch_fut)
            .await
            .expect(
                "dispatch_peer_maintenance did not return within the 2s watchdog — the \
                 contended write lock stalled the caller (event loop); phase=28 was \
                 not offloaded (regression of #3689)",
            );

        assert!(
            dispatch_elapsed < Duration::from_millis(500),
            "dispatch_peer_maintenance blocked the caller for {:?} (expected <500ms) while \
             the DB write lock was contended; the busy_timeout retry was not offloaded",
            dispatch_elapsed
        );

        // Release the lock so the detached maintenance task can complete, then
        // reap the handle to avoid leaking the task into other tests.
        drop(holder);
        if let Some(h) = slot.take() {
            let _ = tokio::time::timeout(Duration::from_secs(35), h).await;
        }
    }
}
