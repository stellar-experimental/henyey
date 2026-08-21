//! SCP consensus: triggering rounds, out-of-sync recovery, and quorum set management.

use super::archive_cache::CacheResult;
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

/// Relationship between the node's current ledger and the latest externalized
/// SCP slot, eliminating the `saturating_sub` ambiguity where `gap == 0` could
/// mean either "caught up" or "ahead of consensus" (see issue #1861).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum LedgerRelation {
    /// The node is behind consensus by `gap` ledgers (gap > 0).
    Behind { gap: u64 },
    /// The node is at the tip of consensus (`current_ledger == latest_externalized`).
    AtTip,
    /// The node is ahead of the latest externalized slot (e.g., `latest_externalized == 0`
    /// on startup, or consensus hasn't reported a slot yet).
    Ahead,
}

impl LedgerRelation {
    /// Compute the relation from the node's current ledger and the latest
    /// externalized SCP slot.
    pub fn from_ledgers(current_ledger: u32, latest_externalized: u64) -> Self {
        let current = current_ledger as u64;
        if latest_externalized > current {
            Self::Behind {
                gap: latest_externalized - current,
            }
        } else if latest_externalized == current {
            Self::AtTip
        } else {
            Self::Ahead
        }
    }

    /// Returns `true` if the node is behind consensus.
    pub fn is_behind(&self) -> bool {
        matches!(self, Self::Behind { .. })
    }

    /// Returns `Some(gap)` if the node is behind consensus, `None` otherwise.
    pub fn behind_gap(&self) -> Option<u64> {
        match self {
            Self::Behind { gap } => Some(*gap),
            _ => None,
        }
    }

    /// Returns `true` when the node is ahead of consensus AND has never
    /// externalized (`latest_externalized == 0`). This is the startup state
    /// for captive-core: it closes ledgers from the validator's EXTERNALIZE
    /// messages but never externalizes itself, so recovery needs an
    /// escalation path to catchup instead of looping on SCP state requests.
    pub fn is_ahead_without_externalization(&self, latest_externalized: u64) -> bool {
        matches!(self, Self::Ahead) && latest_externalized == 0
    }
}

/// Determine whether checkpoint SCP messages should be delayed when responding
/// to a `GetScpState` request.
///
/// Parity: stellar-core `HerderImpl::sendSCPStateToPeer` (HerderImpl.cpp:1007-1028).
/// Returns `true` when there is a gap between the most recent checkpoint and
/// the first sequential slot, meaning the receiver needs time to establish
/// tracking before processing checkpoint envelopes.
pub(super) fn should_delay_checkpoint(checkpoint: u64, first_sequential_ledger: u64) -> bool {
    checkpoint < first_sequential_ledger
}

/// The action to take when an SCP timer event fires.
///
/// Pure classification of the three-way split from stellar-core's
/// `HerderSCPDriver::timerCallbackWrapper`: old-slot timers are dropped,
/// future-slot timers are re-armed at 1 second while tracking, and
/// current-slot timers fire immediately.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TimerEventAction {
    /// Old-slot timer — discard without re-arming.
    Drop,
    /// Future-slot timer while tracking — re-arm with 1-second delay.
    Rearm,
    /// Current slot or non-tracking — execute the timeout handler.
    Fire,
}

/// Classify a timer event into an action based on the event's slot, the next
/// consensus slot, and whether the herder is currently tracking.
///
/// Spec: HERDER_SPEC §5.4-2 (future-slot timer reschedule).
///
/// Parity: stellar-core's `timerCallbackWrapper` only applies the slot-based
/// defer/drop logic when `isTracking()` is true. When not tracking, all timer
/// callbacks fire immediately regardless of slot relationship to
/// `nextConsensusLedgerIndex()`. We match this behavior exactly — stale events
/// from prior tracking epochs are rejected by the epoch guard in
/// `handle_scp_timer_event()` before reaching this classifier.
pub(super) fn classify_timer_event(
    event_slot: u64,
    next_consensus_slot: u64,
    is_tracking: bool,
) -> TimerEventAction {
    if !is_tracking {
        // Not tracking: fire unconditionally. Matches stellar-core's
        // timerCallbackWrapper which only gates on slot when isTracking().
        // Stale events from prior epochs are already rejected by the
        // epoch guard before reaching this point.
        TimerEventAction::Fire
    } else if event_slot < next_consensus_slot {
        TimerEventAction::Drop
    } else if event_slot > next_consensus_slot {
        TimerEventAction::Rearm
    } else {
        TimerEventAction::Fire
    }
}

/// Decide whether out-of-sync recovery should escalate to a forced catchup.
///
/// Pure predicate extracted from `out_of_sync_recovery` (#3728). Escalation
/// requires the attempt floor `RECOVERY_ESCALATION_CATCHUP` to be crossed, and
/// then one of:
///   - `ahead_no_ext` — captive-core startup (Ahead with no externalization);
///     escalate so recovery converges instead of looping on SCP requests.
///   - the node is `Behind` by MORE than `RECOVERY_ESCALATION_NEAR_TIP_GAP`.
///
/// A pure single-ledger behind gap (`Behind { gap: 1 }`) is deliberately NOT
/// escalated *while it stays momentary*: it self-recovers via the peer-SCP
/// back-fill path. It falls through to the `gap <= TX_SET_REQUEST_WINDOW`
/// branch in `out_of_sync_recovery`, which requests SCP state from peers.
/// Note the Step-2 fast-track in that branch does NOT fire for
/// `Behind { gap: 1 }` (it requires `AtTip` or ahead-no-ext), so the
/// fall-through lands in the request-SCP-state path, not a second hidden
/// catchup. See `RECOVERY_ESCALATION_NEAR_TIP_GAP` for the safety argument
/// (gap grows past 1 → escalation restored).
///
/// The `gap == 1` suppression is *bounded*, not unconditional (#3728 review
/// follow-up): once such a gap has persisted for
/// `RECOVERY_ESCALATION_NEAR_TIP_GAP_STALL_ATTEMPTS` consecutive no-progress
/// attempts, escalation resumes. This restores the peer-independent
/// archive-catchup backstop and the `forcing_catchup_behind` alarm coverage for
/// a node genuinely wedged at exactly `gap == 1` (one whose SCP visibility has
/// itself stalled, so the tip never climbs to `N+2` to flip the relation). The
/// momentary blip (cleared by `attempts=8` in production) is still suppressed.
///
/// `AtTip` / `Ahead` (with externalization) are handled downstream and never
/// escalate here.
/// The near-tip single-ledger stall resume is additionally gated on wall-clock
/// time (#3748): the `attempts >= STALL` count alone is not sufficient, because
/// a post-park `MissedTickBehavior::Burst` replay of missed 1s ticks can inflate
/// `recovery_attempts_without_progress` past the threshold in sub-second real
/// time (see #3756 for the event-loop park). A genuine persistent stall must be
/// backed by REAL elapsed time, so escalation only resumes once the no-progress
/// streak has lasted at least `RECOVERY_ESCALATION_NEAR_TIP_GAP_STALL_ATTEMPTS`
/// seconds of wall-clock time (the recovery interval is ~1s, so the count and
/// the elapsed-seconds threshold are the same constant by construction).
pub(super) fn near_tip_stall_is_wall_clock_backed(
    _attempts: u64,
    streak_elapsed: Duration,
) -> bool {
    streak_elapsed >= Duration::from_secs(RECOVERY_ESCALATION_NEAR_TIP_GAP_STALL_ATTEMPTS)
}

pub(super) fn should_escalate_to_catchup(
    attempts: u64,
    relation: LedgerRelation,
    ahead_no_ext: bool,
    streak_elapsed: Duration,
) -> bool {
    if attempts < RECOVERY_ESCALATION_CATCHUP {
        return false;
    }
    if ahead_no_ext {
        return true;
    }
    match relation.behind_gap() {
        // Multi-ledger behind gap — escalate immediately (unchanged). This is a
        // genuine fall-behind and is deliberately NOT wall-clock-gated.
        Some(gap) if gap > RECOVERY_ESCALATION_NEAR_TIP_GAP => true,
        // Single-ledger near-tip gap — suppress only while momentary; resume
        // escalation once it persists past the stall threshold AND that streak
        // is backed by real wall-clock time (#3748), so a genuinely wedged node
        // keeps its archive backstop + alarm coverage while a post-park Burst
        // replay that inflates `attempts` in sub-second time does not.
        Some(_) => {
            attempts >= RECOVERY_ESCALATION_NEAR_TIP_GAP_STALL_ATTEMPTS
                && near_tip_stall_is_wall_clock_backed(attempts, streak_elapsed)
        }
        None => false,
    }
}

/// Whether this recovery tick is suppressing catchup escalation *solely*
/// because of the momentary single-ledger near-tip carve-out (#3728).
///
/// True iff the attempt floor is crossed, the node is `Behind` by exactly a
/// near-tip gap (`<= RECOVERY_ESCALATION_NEAR_TIP_GAP`), and the persistent-stall
/// threshold has not yet been reached — i.e. the exact tick where the old code
/// would have fired `forcing_catchup_behind` but the carve-out holds it back.
/// The call site increments the non-alarming `near_tip_gap1_suppressed` metric
/// on these ticks so operators retain visibility into a node parked at `gap==1`
/// without re-tripping the `forcing_catchup_behind` streak-3 alarm.
pub(super) fn is_near_tip_gap_suppressed(
    attempts: u64,
    relation: LedgerRelation,
    ahead_no_ext: bool,
    streak_elapsed: Duration,
) -> bool {
    attempts >= RECOVERY_ESCALATION_CATCHUP
        && !ahead_no_ext
        && matches!(relation.behind_gap(), Some(gap) if gap <= RECOVERY_ESCALATION_NEAR_TIP_GAP)
        && !should_escalate_to_catchup(attempts, relation, ahead_no_ext, streak_elapsed)
}

/// Whether this recovery tick is suppressing catchup escalation *solely*
/// because the near-tip stall resume is not yet wall-clock-backed (#3748).
///
/// True iff the attempt floor is crossed, the node is `Behind` by a near-tip
/// gap (`<= RECOVERY_ESCALATION_NEAR_TIP_GAP`), the attempt count has already
/// crossed the persistent-stall threshold (so the OLD attempt-only gate would
/// have fired `forcing_catchup_behind`), but the no-progress streak is NOT yet
/// backed by real wall-clock time — i.e. a post-park `Burst` replay inflated
/// `attempts` in sub-second time. These ticks increment the distinct
/// non-alarming `near_tip_park_inflated` metric instead of re-tripping the
/// `forcing_catchup_behind` streak-3 alarm.
///
/// This is deliberately classified BEFORE `is_near_tip_gap_suppressed` at the
/// call site: at `attempts >= STALL` with a non-wall-clock-backed streak, BOTH
/// predicates are true (escalation is held back, so `is_near_tip_gap_suppressed`
/// also returns true), so the caller must attribute the tick to
/// `near_tip_park_inflated` first to avoid double-counting (#3748 Critic A).
pub(super) fn is_near_tip_park_inflated(
    attempts: u64,
    relation: LedgerRelation,
    ahead_no_ext: bool,
    streak_elapsed: Duration,
) -> bool {
    attempts >= RECOVERY_ESCALATION_NEAR_TIP_GAP_STALL_ATTEMPTS
        && !ahead_no_ext
        && matches!(relation.behind_gap(), Some(gap) if gap <= RECOVERY_ESCALATION_NEAR_TIP_GAP)
        && !near_tip_stall_is_wall_clock_backed(attempts, streak_elapsed)
}

/// Whether an `is_behind()` relation at the emit site of `trigger_recovery_catchup`
/// is a *spurious near-tip blip* rather than a genuine fall-behind (#3733).
///
/// When inbound peers authenticate-then-drop, EXTERNALIZE envelopes for slots
/// `> current_ledger + 1` briefly arm `sync_recovery_pending`; by the time the
/// consensus tick runs, the node has closed those ledgers and is glued to the
/// tip with `effective_peer_gap == 0` (verified `max_verified_scp_slot ==
/// current_ledger`). But `herder.latest_externalized_slot()` can still be
/// transiently 1–2 ahead of `current_ledger`, so `LedgerRelation` reports
/// `Behind { gap: 1..2 }`. That transient must NOT be counted as
/// `forcing_catchup_behind` (the +81/+431 alarm bursts) — the authoritative
/// near-tip signal is the verified peer gap, exactly what the heartbeat reports
/// as `peer_gap=0`.
///
/// The predicate is deliberately stricter than a bare `behind_gap <= N` test:
/// it additionally requires `peer_gap == 0`, so a node that is genuinely behind
/// the network (peers verifiably ahead, `peer_gap >= PEER_AHEAD_ESCALATION_THRESHOLD`)
/// is never mis-classified as near-tip. The `behind_gap <= TX_SET_REQUEST_WINDOW`
/// bound keeps the carve-out to the near-tip band; a large local behind gap is
/// never spurious even at `peer_gap == 0`.
pub(super) fn is_spurious_near_tip_behind(behind_gap: u64, peer_gap: u64) -> bool {
    behind_gap <= TX_SET_REQUEST_WINDOW && peer_gap == 0
}

impl App {
    /// Try to trigger consensus for the next ledger.
    ///
    /// Validators: proposes a signed nomination value and starts SCP.
    /// Watchers (non-validators): builds and caches the candidate tx set
    /// without entering SCP nomination (parity: stellar-core §5.2).
    ///
    /// Matches stellar-core's triggerNextLedger() gate: only proceed when
    /// the node is tracking the network AND the ledger manager is synced
    /// (LCL == tracking slot). Without this, a node that is behind would
    /// propose stale transaction sets for slots it hasn't closed yet.
    pub(super) async fn try_trigger_consensus(&self) {
        if self.config.node.manual_close {
            return;
        }

        let tracking_slot = self.herder.tracking_slot().get();

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

            // Parity: HerderImpl.cpp:1440-1447 — if a ledger close is in
            // progress, do not start nomination.
            if self.is_applying_ledger() {
                self.consensus_trigger_skipped_applying
                    .fetch_add(1, Ordering::Relaxed);
                tracing::debug!(
                    current_ledger,
                    tracking_slot,
                    "Skipping consensus trigger: ledger close in progress"
                );
                return;
            }

            let next_slot = current_ledger + 1;

            // Watcher per-slot latch: because henyey polls try_trigger_consensus
            // periodically (unlike stellar-core's one-shot timer), watchers must
            // not rebuild/cache the same slot repeatedly. Validators don't need
            // this because their idempotency is handled by herder's
            // AlreadyNominating guard.
            //
            // We latch BEFORE spawning the blocking task so that concurrent
            // callers (or tight polling intervals) cannot start a second build
            // for the same slot while the first is still running.
            if !self.is_validator {
                let last = self.watcher_last_triggered_slot.load(Ordering::Relaxed);
                if last >= next_slot as u64 {
                    return;
                }
                // Atomically claim this slot. If another caller raced us,
                // the compare_exchange fails and we bail out.
                if self
                    .watcher_last_triggered_slot
                    .compare_exchange(last, next_slot as u64, Ordering::AcqRel, Ordering::Relaxed)
                    .is_err()
                {
                    return;
                }
            }

            tracing::debug!(next_slot, "Checking if we should trigger consensus");

            // Parity: HerderImpl::setupTriggerNextLedger — delay nomination until
            // prepareStart + expectedClose + ctValidityOffset says the proposed
            // close time is valid. This preserves the poller architecture while
            // preventing earlier triggers than stellar-core would allow.
            let expected_close = self.herder.ledger_close_duration();
            let last_slot = current_ledger as u64;
            let last_ballot_start = self.herder.prepare_start(last_slot);
            let now_instant = std::time::Instant::now();
            let trigger_time = match last_ballot_start {
                Some(ballot_start) => ballot_start + expected_close,
                None => now_instant,
            };
            if trigger_time > now_instant {
                tracing::debug!(
                    next_slot,
                    "Skipping consensus trigger: trigger_time not yet reached (setupTriggerNextLedger parity)"
                );
                // Roll back the latch we just claimed so a later tick — once
                // the trigger time is reached — can re-attempt this slot.
                self.rollback_watcher_latch(next_slot);
                return;
            }

            // Parity: stellar-core HerderImpl.cpp:1281-1282 passes
            // `triggerTime - now` as the trigger offset. In this poller, we only
            // reach this point once `trigger_time <= now_instant` (the
            // early-return above bails out otherwise), so that offset is always
            // 0 — the trigger fires at or after its scheduled time, never ahead.
            // Pass 0 directly rather than computing a value that is provably
            // zero on this path.
            let ct_offset = self
                .herder
                .ct_validity_offset(self.herder.lcl_close_time().saturating_add(1), 0);
            if ct_offset > 0 {
                tracing::debug!(
                    next_slot,
                    ct_offset,
                    "Skipping consensus trigger: ctValidityOffset requires additional delay"
                );
                // Roll back the latch so a later tick — once the proposed
                // close time falls within the validity window — can retry.
                self.rollback_watcher_latch(next_slot);
                return;
            }

            // In a full implementation, we would:
            // 1. Check if enough time has passed since last close
            // 2. Build a transaction set from queued transactions
            // 3. Create a StellarValue with the tx set hash and close time
            // 4. Start SCP nomination with that value

            // For now, trigger the herder
            self.consensus_trigger_attempts
                .fetch_add(1, Ordering::Relaxed);

            // trigger_next_ledger is entirely synchronous (parking_lot locks +
            // CPU-heavy SCP nomination). Run it on the blocking pool so the
            // tokio worker thread stays free to service other task wake-ups.
            let herder = std::sync::Arc::clone(&self.herder);
            match henyey_common::spawn_blocking_logged("trigger_next_ledger", move || {
                herder.trigger_next_ledger(next_slot)
            })
            .await
            {
                Ok(Ok(henyey_herder::TriggerOutcome::Triggered)) => {
                    // Record local close time for drift tracking only after
                    // nomination actually starts. Recording before
                    // trigger_next_ledger() would poison the drift tracker
                    // on retryable skips (e.g. SkippedInvalidCloseTime).
                    let local_time = self
                        .clock
                        .system_now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .expect("system clock before UNIX epoch")
                        .as_secs();
                    if let Ok(mut tracker) = self.drift_tracker.lock() {
                        tracker.record_local_close_time(next_slot, local_time);
                    }
                    self.consensus_trigger_successes
                        .fetch_add(1, Ordering::Relaxed);
                }
                Ok(Ok(henyey_herder::TriggerOutcome::SkippedStale)) => {
                    self.consensus_trigger_skipped_stale
                        .fetch_add(1, Ordering::Relaxed);
                    // Benign retryable skip — roll back the watcher per-slot
                    // latch so the next tick can re-attempt this slot (the
                    // skip did not build/cache anything). Only clear if the
                    // latch still holds OUR slot.
                    self.rollback_watcher_latch(next_slot);
                }
                Ok(Ok(henyey_herder::TriggerOutcome::AlreadyNominating)) => {
                    // Idempotent re-trigger; not a new success, not an error.
                }
                Ok(Ok(henyey_herder::TriggerOutcome::ObserverCached)) => {
                    self.consensus_trigger_successes
                        .fetch_add(1, Ordering::Relaxed);
                    // Latch was already set before spawning (compare_exchange
                    // above), so no need to store again here.
                }
                Ok(Ok(henyey_herder::TriggerOutcome::SkippedInvalidCloseTime)) => {
                    tracing::debug!(
                        slot = next_slot,
                        "Consensus trigger: close time invalid, will retry"
                    );
                    // Benign retryable skip (close time too far ahead of the
                    // wall clock). Roll back the latch so a later tick — once
                    // the clock catches up — can re-attempt this slot.
                    self.rollback_watcher_latch(next_slot);
                }
                Ok(Err(e)) => {
                    self.consensus_trigger_failures
                        .fetch_add(1, Ordering::Relaxed);
                    // Roll back the watcher per-slot latch so a retry can
                    // re-attempt the build for this slot on the next tick.
                    self.rollback_watcher_latch(next_slot);
                    tracing::error!(error = %e, slot = next_slot, "Failed to trigger ledger");
                }
                Err(_join_error) => {
                    // Already logged by spawn_blocking_logged
                    self.consensus_trigger_failures
                        .fetch_add(1, Ordering::Relaxed);
                    // Roll back latch on join failure as well.
                    self.rollback_watcher_latch(next_slot);
                }
            }
        }
    }

    /// Roll back the watcher per-slot trigger latch for `slot` after a
    /// non-success trigger outcome (hard error, join failure, or a benign
    /// retryable skip such as `SkippedStale` / `SkippedInvalidCloseTime`).
    ///
    /// Only clears the latch if it still holds OUR slot — a newer slot may
    /// have been claimed while we were awaiting, and that claim must survive
    /// (regression: stale rollback must not wipe a newer claim). No-op for
    /// validators, which do not use the watcher latch.
    fn rollback_watcher_latch(&self, slot: u32) {
        if !self.is_validator {
            let _ = self.watcher_last_triggered_slot.compare_exchange(
                slot as u64,
                0,
                Ordering::AcqRel,
                Ordering::Relaxed,
            );
        }
    }

    /// Arm the event-driven consensus-trigger timer for the next ledger (#2702).
    ///
    /// Port of stellar-core's `HerderImpl::setupTriggerNextLedger`
    /// (HerderImpl.cpp:1236-1303): compute the instant at which the next ledger
    /// should be nominated (`prepareStart(lastSlot) + expectedClose`, clamped to
    /// `now`, plus any `ctValidityOffset` delay) and arm a single-shot
    /// `TimerType::TriggerNextLedger` timer for that delay. When the timer fires,
    /// the main loop calls [`Self::try_trigger_consensus`] for the slot.
    ///
    /// This is the *primary* nomination scheduler. The 1-second maintenance tick
    /// retains a now-idempotent `try_trigger_consensus()` call as a safety-net:
    /// because `try_trigger_consensus` self-gates on the same trigger-time /
    /// `ctValidityOffset` boundary (#2816), the tick can never trigger earlier
    /// than this timer would, and a redundant call is absorbed by the herder's
    /// `AlreadyNominating` guard / the watcher per-slot latch.
    ///
    /// Self-gates on `manual_close` (stellar-core skips `async_wait` under
    /// MANUAL_CLOSE, HerderImpl.cpp:1298-1303) and on `is_tracking`. Always
    /// cancels any prior trigger timer for the next slot before re-arming.
    /// Test-only mirror of the post-close consensus-trigger subsequence run at
    /// the two production post-close sites (`lifecycle.rs` select-loop close
    /// completion and the `try_apply_buffered_ledgers` burst helper). The
    /// regression test `test_post_close_arms_trigger_without_immediate_nomination`
    /// asserts on this canonical sequence; it is edited in lockstep with the two
    /// production sites so it stays a faithful mirror of what they do.
    #[cfg(test)]
    pub(super) async fn post_close_trigger_and_arm(&self) {
        // Arm-only (#3014): no immediate `try_trigger_consensus()` — the armed
        // `TriggerNextLedger` timer is the sole nomination scheduler, matching
        // stellar-core's arm-only `lastClosedLedgerIncreased`
        // (HerderImpl.cpp:1218-1233).
        self.setup_trigger_next_ledger().await;
    }

    pub(super) async fn setup_trigger_next_ledger(&self) {
        // Parity: MANUAL_CLOSE skips arming the trigger timer.
        if self.config.node.manual_close {
            return;
        }
        // Only arm while tracking — mirrors stellar-core arming the trigger from
        // `lastClosedLedgerIncreased`, which only runs once the node is in-sync.
        if !self.herder.is_tracking() {
            return;
        }

        let current_ledger = self.current_ledger_seq();
        let next_slot = current_ledger as u64 + 1;

        // Compute the trigger instant exactly as `try_trigger_consensus` does
        // (kept in lock-step so the timer and the safety-net agree on timing).
        //
        // NOTE (maxtps iteration 2, 2026-07-02, REJECTED experiment): arming
        // this trigger EARLY by the measured nomination overhead (EWMA of
        // trigger→ballot-start) does work mechanically — loaded cadence drops
        // from ~5.5 s to ~4.95 s (+17% ledger rate) — but does NOT raise the
        // max-TPS ceiling: ledgers arrive proportionally emptier and the
        // loadgen's per-account inclusion-latency guillotine does not relax
        // (measured max 1460-1470 vs 1487 baseline). Do not re-attempt without
        // a mechanism that raises per-WALL-SECOND drain, not ledger rate.
        // See docs/maxtps-optimization/2026-07-02-target2000.md iteration 2.
        let expected_close = self.herder.ledger_close_duration();
        let last_slot = current_ledger as u64;
        let now_instant = std::time::Instant::now();
        let trigger_time = match self.herder.prepare_start(last_slot) {
            Some(ballot_start) => ballot_start + expected_close,
            None => now_instant,
        };
        // Base delay from prepareStart + expectedClose, clamped to "now".
        let base_delay = trigger_time.saturating_duration_since(now_instant);
        // ctValidityOffset (seconds) — additional delay when the proposed close
        // time (lcl.closeTime + 1) is still ahead of the wall-clock validity
        // window. Mirrors HerderImpl.cpp:1281-1291.
        let ct_offset_secs = self
            .herder
            .ct_validity_offset(self.herder.lcl_close_time().saturating_add(1), 0);
        let delay = base_delay + std::time::Duration::from_secs(ct_offset_secs);

        // Always cancel any prior trigger timer for this slot before re-arming
        // (parity: `mTriggerTimer.cancel()` at the top of setupTriggerNextLedger).
        self.timer_manager_handle
            .cancel_trigger_next_ledger(next_slot)
            .await;
        self.timer_manager_handle
            .schedule_trigger_next_ledger(next_slot, delay)
            .await;
        tracing::debug!(
            next_slot,
            delay_ms = delay.as_millis(),
            ct_offset_secs,
            "Armed event-driven consensus trigger timer (setupTriggerNextLedger)"
        );

        // NOTE (maxtps, REVERTED): a ~300 ms nomination-value PREBUILD lived
        // here (arm a sleep task at trigger−300 ms; `trigger_next_ledger`
        // reused the cached value). It cut convergence median 580→320 ms but
        // is a SUSTAINED-LOAD liability: every candidate becomes ~300 ms
        // staler than a trigger-time build (stellar-core builds AT trigger),
        // so txs arriving in that window systematically miss the next tx-set
        // on EVERY node — a two-build miss wedges the tx past its loadgen
        // account's minimum resubmission gap and insta-fails 5-minute runs
        // (#3638). Forensics: pending_age=2 collisions with wedge-push counts
        // of 6-22/node/ledger. Build at trigger time, like core.
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
        let relation = LedgerRelation::from_ledgers(current_ledger, latest_externalized);

        // Track consecutive recovery attempts without progress.
        let baseline = self.recovery_baseline_ledger.load(Ordering::SeqCst);
        let mut did_full_reset = false;
        if current_ledger as u64 > baseline {
            // Progress — always update the baseline ledger.
            self.recovery_baseline_ledger
                .store(current_ledger as u64, Ordering::SeqCst);

            let peer_gap = self.effective_peer_gap(current_ledger);
            let still_behind = relation.is_behind() && peer_gap >= PEER_AHEAD_ESCALATION_THRESHOLD;

            if still_behind {
                // Partial progress: node advanced (e.g. via fast-track) but
                // is still significantly behind verified peers. Re-seed the
                // attempt counter high for faster re-escalation (~30s instead
                // of ~120s). See RecoveryResetMode::Partial docs.
                self.clear_archive_recovery_state(ArchiveRecoveryClear::PartialProgress {
                    seed: PARTIAL_PROGRESS_RESEED,
                })
                .await;
                tracing::info!(
                    current_ledger,
                    peer_gap,
                    "Partial recovery progress — still behind, re-seeding escalation"
                );
            } else {
                // Full progress: node is at or near the tip. Clear all
                // escalation state (#1867).
                self.clear_archive_recovery_state(ArchiveRecoveryClear::FullProgress)
                    .await;
                did_full_reset = true;
            }
        }

        // Root cause fix for #2664: when the node is at-tip with
        // current_ledger > 0 and the progress detection didn't fire
        // (baseline == current_ledger from a prior tick), clear all stale
        // recovery/escalation state. Prevents the callers below from seeing a
        // stale archive-behind flag and escalating to a pointless hard reset.
        if !did_full_reset && matches!(relation, LedgerRelation::AtTip) && current_ledger > 0 {
            let was_behind = self.archive_recovery_snapshot().await.is_confirmed_behind();
            if was_behind {
                self.clear_archive_recovery_state(ArchiveRecoveryClear::FullProgress)
                    .await;
                did_full_reset = true;
                tracing::debug!(
                    current_ledger,
                    "Cleared stale archive-behind state: node is at-tip without \
                     baseline progress — full recovery reset applied (#2664)"
                );
            }
        }

        let attempts = self
            .recovery_attempts_without_progress
            .fetch_add(1, Ordering::SeqCst);

        // Wall-clock anchor for the near-tip stall gate (#3748). Stamp the start
        // of the current no-progress streak the first time we observe it with no
        // active anchor (sentinel 0). Using compare_exchange from 0 (rather than
        // only the `attempts == 0` 0→1 transition) also covers streaks kicked off
        // by an external `escalate_recovery_to_catchup` fetch_max, which jumps the
        // counter past 0→1 without routing through here. `max(1)` keeps a genuine
        // streak that begins at elapsed==0ms from colliding with the 0 sentinel.
        // The anchor is cleared on every `reset_recovery_attempts` (both Full and
        // Partial), so a fresh streak always re-stamps. `streak_elapsed` is
        // measured against the monotonic `start_instant` (never the system clock).
        let now_ms = self.start_instant.elapsed().as_millis() as u64;
        let _ = self.recovery_streak_start.compare_exchange(
            0,
            now_ms.max(1),
            Ordering::SeqCst,
            Ordering::SeqCst,
        );
        let streak_start = self.recovery_streak_start.load(Ordering::SeqCst);
        let streak_elapsed = Duration::from_millis(now_ms.saturating_sub(streak_start));

        tracing::debug!(
            current_ledger,
            latest_externalized,
            last_processed,
            pending_tx_sets = pending_tx_sets.len(),
            buffer_count,
            gap = relation.behind_gap().unwrap_or(0),
            attempts,
            "Performing out-of-sync recovery"
        );

        // Onset diagnostic: emit a structured info-level snapshot exactly once
        // per recovery episode, gated on Synced/Validating to suppress startup
        // and catchup noise. Skip on progress ticks that just performed a Full
        // reset — those are episode-ending, not episode-starting. See #2568.
        if !did_full_reset {
            let app_state = self.state().await;
            if matches!(app_state, AppState::Synced | AppState::Validating)
                && self.recovery_episode_latch.try_mark_onset()
            {
                let peer_gap = self.effective_peer_gap(current_ledger);
                let last_close_ms = self.last_close_stats.read().close_time_ms;
                let tx_set_peers_exhausted = self.tx_set_all_peers_exhausted.load(Ordering::SeqCst);
                let (_, auth_peer_count) = self.peer_counts().await;
                let herder_state = self.herder.state();
                crate::metrics::RECOVERY_STALL_ONSET_TOTAL.increment(1);
                tracing::info!(
                    current_ledger,
                    latest_externalized,
                    gap = relation.behind_gap().unwrap_or(0),
                    peer_gap,
                    last_close_ms,
                    buffer_count,
                    pending_tx_sets = pending_tx_sets.len(),
                    tx_set_peers_exhausted,
                    auth_peer_count,
                    %herder_state,
                    %app_state,
                    "Recovery stall onset — diagnostic snapshot"
                );
            }
        }

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
        // Escalate when the node is behind consensus by MORE than a single
        // ledger, OR when the node is in the "Ahead" state with no SCP
        // externalization yet (latest_ext=0). The latter case covers
        // captive-core in quickstart/local mode: it closes ledgers from the
        // validator's EXTERNALIZE messages but never externalizes itself, so
        // without this escalation the recovery loop would request SCP state
        // forever without converging.
        //
        // A pure single-ledger behind gap (gap==1) is deliberately NOT
        // escalated (#3728) *while it stays momentary*: it self-recovers via
        // the peer-SCP back-fill path below, so a forced catchup is redundant
        // work. The suppression is bounded — once the gap==1 persists past
        // RECOVERY_ESCALATION_NEAR_TIP_GAP_STALL_ATTEMPTS, escalation resumes so
        // a genuinely wedged node keeps its archive backstop + alarm coverage.
        // See `should_escalate_to_catchup` and `RECOVERY_ESCALATION_NEAR_TIP_GAP`.
        let ahead_no_ext = relation.is_ahead_without_externalization(latest_externalized);
        if should_escalate_to_catchup(attempts, relation, ahead_no_ext, streak_elapsed) {
            self.set_phase_sub(PHASE_13_10_TRIGGER_RECOVERY_CATCHUP);
            return self
                .trigger_recovery_catchup(current_ledger, latest_externalized, relation, attempts)
                .await;
        }
        // Observability for the suppressed near-tip carve-out (#3728 review
        // follow-up): on the exact ticks where the old code would have fired
        // `forcing_catchup_behind` but the gap==1 carve-out holds escalation
        // back, emit a distinct non-alarming counter. This keeps a node parked
        // at gap==1 visible to operators (and monitor-tick can scrape it)
        // WITHOUT re-tripping the `forcing_catchup_behind` streak-3 alarm that
        // Check 12b keys on. A genuinely wedged node still crosses the stall
        // threshold above and re-fires `forcing_catchup_behind`.
        //
        // #3748: classify `near_tip_park_inflated` FIRST. At `attempts >= STALL`
        // with a non-wall-clock-backed streak, escalation is held back, so
        // `is_near_tip_gap_suppressed` is ALSO true on the same tick — the
        // else-if keeps the tick counted exactly once (Critic A).
        if is_near_tip_park_inflated(attempts, relation, ahead_no_ext, streak_elapsed) {
            crate::metrics::RECOVERY_STALLED_TICK_TOTAL.increment("near_tip_park_inflated", 1);
        } else if is_near_tip_gap_suppressed(attempts, relation, ahead_no_ext, streak_elapsed) {
            crate::metrics::RECOVERY_STALLED_TICK_TOTAL.increment("near_tip_gap1_suppressed", 1);
        }

        // When the node is essentially caught up (small or zero gap), the
        // recovery strategy is a 4-step decision:
        //
        //   1. next_slot_missing → request SCP state immediately (peers may
        //      still have it cached).
        //   2. fast-track → AtTip/Ahead-no-ext with SCP activity means tx_sets
        //      are evicted from peers' caches; skip straight to catchup. Fires
        //      at ANY attempt count (the cumulative scp_total > 0 gate is an
        //      existing heuristic this code preserves).
        //   3. low attempts → wait for a fresh EXTERNALIZE.
        //   4. high attempts → request SCP state despite small gap.
        if relation
            .behind_gap()
            .map_or(true, |g| g <= TX_SET_REQUEST_WINDOW)
        {
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
                // Step 1: The next slot's EXTERNALIZE was missed (network blip,
                // peer disconnection, etc.). Request SCP state immediately —
                // peers should still have it cached if we act quickly.
                let now_secs = self.start_instant.elapsed().as_secs();
                if self
                    .recovery_throttles
                    .next_slot_missing
                    .should_log(now_secs)
                {
                    tracing::warn!(
                        current_ledger,
                        latest_externalized,
                        gap = relation.behind_gap().unwrap_or(0),
                        attempts,
                        "Next slot EXTERNALIZE missing — requesting SCP state immediately"
                    );
                } else {
                    tracing::debug!(
                        current_ledger,
                        latest_externalized,
                        gap = relation.behind_gap().unwrap_or(0),
                        attempts,
                        "Next slot EXTERNALIZE missing — requesting SCP state immediately \
                         (repeated)"
                    );
                }
                // Fall through to the SCP state request below
            } else {
                // Step 2: Fast-track — AtTip or Ahead-no-ext with SCP activity.
                // If we've received SCP messages since the last recovery
                // reset/re-arm but none result in externalization, the
                // tx_sets for our slots are gone from peers' caches (~60s
                // window). Skip straight to catchup.
                //
                // `scp_since_reset > 0` means SCP traffic arrived since
                // the last reset — not proof of truly fresh quorum-level
                // SCP state. In a long stall with no resets, messages from
                // early in the stall window still qualify.
                //
                // Fires at ANY attempt count (>= 1) — the stall evidence
                // is the gate, not the attempt counter. This is critical
                // for captive-core following a standalone validator in
                // quickstart/local mode.
                let scp_total = self.scp_messages_received.load(Ordering::Relaxed);
                let scp_baseline = self.recovery_baseline_scp_received.load(Ordering::SeqCst);
                let scp_since_reset = scp_total.saturating_sub(scp_baseline);
                let at_tip_or_ahead_no_ext = matches!(relation, LedgerRelation::AtTip)
                    || relation.is_ahead_without_externalization(latest_externalized);
                if attempts >= 1 && scp_since_reset > 0 && at_tip_or_ahead_no_ext {
                    let now_secs = self.start_instant.elapsed().as_secs();
                    if self
                        .recovery_throttles
                        .scp_no_externalization
                        .should_log(now_secs)
                    {
                        tracing::warn!(
                            current_ledger,
                            latest_externalized,
                            gap = relation.behind_gap().unwrap_or(0),
                            attempts,
                            scp_total,
                            scp_baseline,
                            scp_since_reset,
                            "Receiving SCP messages but no externalization — \
                             tx_sets evicted from peers, fast-tracking catchup"
                        );
                    } else {
                        tracing::debug!(
                            current_ledger,
                            latest_externalized,
                            gap = relation.behind_gap().unwrap_or(0),
                            attempts,
                            scp_total,
                            scp_baseline,
                            scp_since_reset,
                            "Receiving SCP messages but no externalization — \
                             fast-tracking catchup (repeated)"
                        );
                    }
                    // Jump directly to catchup
                    self.set_phase_sub(PHASE_13_10_TRIGGER_RECOVERY_CATCHUP);
                    return self
                        .trigger_recovery_catchup(
                            current_ledger,
                            latest_externalized,
                            relation,
                            attempts,
                        )
                        .await;
                }

                // Step 3: Low attempts — wait for a fresh EXTERNALIZE.
                if attempts < RECOVERY_ESCALATION_SCP_REQUEST {
                    tracing::debug!(
                        current_ledger,
                        latest_externalized,
                        gap = relation.behind_gap().unwrap_or(0),
                        attempts,
                        "Essentially caught up — waiting for fresh EXTERNALIZE"
                    );
                    return None;
                }

                // Step 4: High attempts — request SCP state despite small gap.
                let now_secs = self.start_instant.elapsed().as_secs();
                if self
                    .recovery_throttles
                    .caught_up_no_progress
                    .should_log(now_secs)
                {
                    tracing::warn!(
                        current_ledger,
                        latest_externalized,
                        gap = relation.behind_gap().unwrap_or(0),
                        attempts,
                        "Essentially caught up but no progress — requesting SCP state from peers"
                    );
                } else {
                    tracing::debug!(
                        current_ledger,
                        latest_externalized,
                        gap = relation.behind_gap().unwrap_or(0),
                        attempts,
                        "Essentially caught up but no progress — requesting SCP state \
                         (repeated)"
                    );
                }
                // ── No-SCP hard-reset escalation (issue #2349) ──────────
                // When we've been at tip with no SCP progress for a long
                // time AND the archive is confirmed behind, escalate to
                // hard reset. Uses a higher threshold than the fast-track
                // path because absence of SCP traffic is weaker evidence.
                //
                // #3263: gate the escalation behind a peer_gap floor, matching
                // the sibling peer-ahead site below (consensus.rs:1876). At the
                // live tip the archive is normally behind, so without a verified
                // peer ahead this site would otherwise escalate-at-tip. The
                // `peer_gap >= PEER_AHEAD_ESCALATION_THRESHOLD` floor is the
                // henyey proxy for "genuinely behind the network" and aligns
                // with stellar-core's SCP-only `HerderImpl::outOfSyncRecovery`,
                // which never archive-escalates at tip. #1862 far-behind
                // (peer_gap >= 12 >= 3) is unaffected; a genuinely-stuck node at
                // peer_gap < 3 is still caught by the independent #2789 120s
                // wall-clock backstop, so the floor cannot create a new wedge.
                let archive_is_confirmed_behind =
                    self.archive_recovery_snapshot().await.is_confirmed_behind();
                let peer_gap = self.effective_peer_gap(current_ledger);
                if archive_is_confirmed_behind
                    && peer_gap >= PEER_AHEAD_ESCALATION_THRESHOLD
                    && attempts >= RECOVERY_HARD_RESET_ESCALATION_ATTEMPTS_NO_SCP
                    && !self.is_hard_reset_on_cooldown(peer_gap)
                {
                    use super::types::HardResetReason;
                    tracing::warn!(
                        current_ledger,
                        latest_externalized,
                        peer_gap,
                        attempts,
                        "Recovery stuck: AtTip, no SCP progress, archive behind \
                         — escalating to hard reset (time-based)"
                    );
                    crate::metrics::RECOVERY_STALLED_TICK_TOTAL
                        .increment("at_tip_no_scp_hard_reset", 1);
                    self.set_phase_sub(PHASE_13_10_TRIGGER_RECOVERY_CATCHUP);
                    // If the inner cooldown blocks, fall through to
                    // the SCP state request rather than returning idle.
                    if let Some(pc) = self
                        .force_post_catchup_hard_reset(
                            current_ledger,
                            HardResetReason::ArchiveBehindStallWallClock,
                        )
                        .await
                    {
                        return Some(pc);
                    }
                }
                // Fall through to the SCP state request below
            }
        }

        // Detect gaps in externalized slots and potentially trigger catchup.
        self.set_phase_sub(PHASE_13_8_OUT_OF_SYNC_ANALYZE_GAPS);
        if let Some(result) = self
            .analyze_externalized_gaps(current_ledger, latest_externalized, relation, attempts)
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
        relation: LedgerRelation,
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
                    gap = relation.behind_gap().unwrap_or(0),
                    attempts,
                    "Next slot EXTERNALIZE in-flight (waiting for tx_set fetch) — waiting"
                );
                // Fall through to SCP state request for other missing slots.
            } else {
                let now_secs = self.start_instant.elapsed().as_secs();
                if self
                    .recovery_throttles
                    .gap_in_externalized
                    .should_log(now_secs)
                {
                    tracing::warn!(
                        current_ledger,
                        latest_externalized,
                        missing_count,
                        first_missing,
                        last_missing,
                        missing_slots = ?if missing_count <= 10 { missing_slots.clone() } else { vec![] },
                        "Detected gap in externalized slots - missing EXTERNALIZE messages"
                    );
                } else {
                    tracing::debug!(
                        current_ledger,
                        latest_externalized,
                        missing_count,
                        first_missing,
                        last_missing,
                        "Detected gap in externalized slots (repeated)"
                    );
                }
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
                        gap = relation.behind_gap().unwrap_or(0),
                        attempts,
                        "Already at target checkpoint — waiting for escalation"
                    );
                    // Fall through to SCP broadcast.
                } else {
                    let now_secs = self.start_instant.elapsed().as_secs();
                    if self
                        .recovery_throttles
                        .permanently_missing
                        .should_log(now_secs)
                    {
                        tracing::warn!(
                            current_ledger,
                            next_slot,
                            latest_externalized,
                            gap = relation.behind_gap().unwrap_or(0),
                            "Next slot permanently missing — triggering catchup to skip gap"
                        );
                    } else {
                        tracing::debug!(
                            current_ledger,
                            next_slot,
                            latest_externalized,
                            gap = relation.behind_gap().unwrap_or(0),
                            "Next slot permanently missing — triggering catchup (repeated)"
                        );
                    }
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
                            relation,
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
                    // Proactively populate gap slots from externalized
                    // SCP cache. This provides immediate population rather
                    // than waiting for the next process_externalized_slots
                    // tick.
                    self.populate_gap_slots(current_ledger).await;

                    let now_secs = self.start_instant.elapsed().as_secs();
                    if self
                        .recovery_throttles
                        .cannot_apply_gap
                        .should_log(now_secs)
                    {
                        tracing::warn!(
                            current_ledger,
                            latest_externalized,
                            total_buffered = total,
                            first_buffered,
                            last_buffered,
                            required_first,
                            sequence_gap,
                            "All slots externalized but cannot apply - buffered sequence \
                             gap (tx_sets present, but missing ledgers between \
                             current_ledger+1 and first_buffered)"
                        );
                    } else {
                        tracing::debug!(
                            current_ledger,
                            latest_externalized,
                            total_buffered = total,
                            first_buffered,
                            last_buffered,
                            required_first,
                            sequence_gap,
                            "All slots externalized but cannot apply - buffered sequence \
                             gap (repeated)"
                        );
                    }
                }
                CannotApplyReason::MissingTxSets => {
                    let now_secs = self.start_instant.elapsed().as_secs();
                    if self
                        .recovery_throttles
                        .cannot_apply_txset
                        .should_log(now_secs)
                    {
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
                    } else {
                        tracing::debug!(
                            current_ledger,
                            latest_externalized,
                            total_buffered = total,
                            with_tx_set,
                            without_tx_set,
                            first_buffered,
                            last_buffered,
                            required_first,
                            sequence_gap,
                            "All slots externalized but cannot apply - missing tx_sets \
                             (repeated)"
                        );
                    }
                }
            }

            if with_tx_set == 0 && total > 0 {
                let now_secs = self.start_instant.elapsed().as_secs();
                if self
                    .recovery_throttles
                    .no_txsets_forcing
                    .should_log(now_secs)
                {
                    tracing::warn!(
                        current_ledger,
                        latest_externalized,
                        "No tx_sets available for any buffered slot — forcing catchup"
                    );
                } else {
                    tracing::debug!(
                        current_ledger,
                        latest_externalized,
                        "No tx_sets available for any buffered slot — forcing catchup \
                         (repeated)"
                    );
                }
                self.escalate_recovery_to_catchup();
                return Some(None); // Wait for next tick to trigger escalation.
            }
        }

        None // Fall through to SCP broadcast.
    }

    /// Broadcast recent SCP envelopes and request SCP state from peers.
    ///
    /// Spawns a background task to avoid blocking the event loop.
    pub(crate) async fn broadcast_recovery_scp_state(&self, current_ledger: u32) {
        // Parity: stellar-core `HerderImpl::maybeTriggerNextLedger` (HerderImpl.cpp:554-560)
        // rebroadcasts only the current slot's latest messages, then issues a
        // bounded GetScpState pull. Previously we dumped historical envelopes
        // from get_scp_state(current_ledger - 5) and broadcast GetScpState to
        // ALL peers, causing periodic backpressure episodes (#2909).
        let next_slot = current_ledger as u64 + 1;
        let envelopes = self.herder.scp().get_latest_messages_send(next_slot);
        tracing::debug!(
            next_slot,
            envelope_count = envelopes.len(),
            "Got latest messages for recovery rebroadcast"
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

        // Record timestamp before dispatching so heartbeat/gap throttle sees
        // this attempt and does not immediately duplicate it.
        *self.last_scp_state_request_at.write().await = self.clock.now();

        // Step 1: Rebroadcast current-slot latest envelopes to all peers.
        let overlay_clone = Arc::clone(&overlay);
        let envelope_count = envelopes.len();
        // Step 2: Bounded pull — GetScpState to up to 2 random peers.
        // `scp_state_request_ledger_seq()` delegates to
        // `Herder::get_min_ledger_seq_to_ask_peers()` (§15.3 parity).
        let ledger_seq = self.scp_state_request_ledger_seq();

        henyey_common::spawn_observed("scp_envelope_forwarding", async move {
            // Rebroadcast current-slot envelopes.
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
                    "Broadcast current-slot SCP envelopes during out-of-sync recovery"
                );
            }

            // Bounded pull: request SCP state from up to 2 random peers.
            tracing::info!(
                ledger_seq,
                "Requesting SCP state from peers (recovery task, bounded pull)"
            );
            match overlay_clone.request_scp_state(ledger_seq) {
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
    ///
    /// Parity: stellar-core `HerderImpl::sendSCPStateToPeer` (HerderImpl.cpp:984-1070).
    /// When a gap exists between the most recent checkpoint and the first
    /// sequential slot, checkpoint messages are delayed by
    /// `SEND_LATEST_CHECKPOINT_DELAY` to give the receiver time to establish
    /// `trackingConsensusLedgerIndex` before processing checkpoint envelopes
    /// (which require it for close-time drift validation).
    pub(super) async fn send_scp_state(&self, peer_id: &henyey_overlay::PeerId, from_ledger: u32) {
        /// Delay before sending checkpoint SCP messages to a syncing peer.
        /// Matches stellar-core `Herder::SEND_LATEST_CHECKPOINT_DELAY` (2s).
        /// See stellar-core/src/herder/Herder.cpp:13-14.
        const SEND_LATEST_CHECKPOINT_DELAY: Duration = Duration::from_secs(2);

        let from_slot = from_ledger as u64;
        let envelopes = self.herder.get_scp_state(from_slot);

        let Some(overlay) = self.overlay().await else {
            return;
        };

        // Parity: stellar-core delays the checkpoint ledger's messages when
        // there is a gap between the checkpoint and the first sequential slot.
        let checkpoint = self.herder.get_most_recent_checkpoint_seq();
        let first_sequential = self.herder.get_first_sequential_ledger_for_send();
        let delay_checkpoint = should_delay_checkpoint(checkpoint, first_sequential);

        if delay_checkpoint {
            // Send non-checkpoint envelopes immediately
            for envelope in &envelopes {
                if envelope.statement.slot_index == checkpoint {
                    continue; // Skip checkpoint slot — delayed below
                }
                let msg = StellarMessage::ScpMessage(envelope.clone());
                if let Err(e) = overlay.try_send_to(peer_id, msg) {
                    tracing::debug!(peer = %peer_id, error = %e, "Failed to send SCP envelope");
                    break;
                }
            }

            // Spawn a delayed task to send checkpoint envelopes after the delay.
            // Re-fetches at fire time (not snapshot) matching stellar-core's
            // processCurrentState(checkpoint, ...) in the delayed callback.
            let herder = Arc::clone(&self.herder);
            let overlay_weak = Arc::downgrade(&overlay);
            let peer_id = peer_id.clone();
            henyey_common::spawn_observed("send_scp_state_checkpoint_delay", async move {
                tokio::time::sleep(SEND_LATEST_CHECKPOINT_DELAY).await;

                let Some(overlay) = overlay_weak.upgrade() else {
                    return; // Overlay gone — peer disconnected or shutdown
                };

                let checkpoint_envelopes = herder.get_current_state_for_slot(checkpoint);
                for envelope in checkpoint_envelopes {
                    let msg = StellarMessage::ScpMessage(envelope);
                    if let Err(e) = overlay.try_send_to(&peer_id, msg) {
                        tracing::debug!(
                            peer = %peer_id,
                            error = %e,
                            "Failed to send delayed checkpoint SCP envelope"
                        );
                        break;
                    }
                }
            });
        } else {
            // No gap — send all envelopes immediately
            for envelope in envelopes {
                let msg = StellarMessage::ScpMessage(envelope);
                if let Err(e) = overlay.try_send_to(peer_id, msg) {
                    tracing::debug!(peer = %peer_id, error = %e, "Failed to send SCP envelope");
                    break;
                }
            }
        }

        tracing::debug!(peer = %peer_id, from_ledger, delay_checkpoint, "Sent SCP state response");
    }

    /// Respond to a GetScpQuorumset message.
    pub(super) async fn send_quorum_set(
        &self,
        peer_id: &henyey_overlay::PeerId,
        requested_hash: stellar_xdr::Uint256,
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
            let msg = StellarMessage::DontHave(stellar_xdr::DontHave {
                type_: stellar_xdr::MessageType::ScpQuorumset,
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
        quorum_set: stellar_xdr::ScpQuorumSet,
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

        // Offload synchronous DB write to the blocking pool so the async
        // event loop stays responsive (#1924).
        {
            let db = self.db.clone();
            let ledger_seq = self.ledger_manager.current_ledger_seq();
            let qs = quorum_set.clone();
            let h = hash;
            match henyey_common::spawn_blocking_logged("store_scp_quorum_set", move || {
                db.store_scp_quorum_set(&h, ledger_seq, &qs)
            })
            .await
            {
                Ok(Ok(())) => {}
                Ok(Err(db_err)) => {
                    tracing::warn!(error = %db_err, "Failed to store quorum set");
                }
                Err(_) => {
                    // JoinError already logged by spawn_blocking_logged
                }
            }
        }

        for node_id in &node_ids {
            tracing::debug!(%hash, node_id = ?node_id, "Storing quorum set for node");
            self.herder.store_quorum_set(node_id, quorum_set.clone());
        }

        // Drain envelopes that became ready after quorum-set arrival,
        // on a blocking-pool thread so the event loop stays responsive.
        // Mirrors receive_tx_set() pattern (#1904).
        self.herder
            .drain_ready_envelopes_blocking("quorum-set envelope drain")
            .await;

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

    /// Handle a single SCP timer event from the TimerManager.
    ///
    /// This replaces the 500ms polling `check_scp_timeouts()` with precise
    /// single-shot timer delivery matching stellar-core's VirtualTimer pattern.
    ///
    /// Returns `true` if the caller should pump the close pipeline
    /// (`process_externalized_slots()` + `try_start_ledger_close()`) after this
    /// event. This is `true` only for a fired `TriggerNextLedger` event (#2702):
    /// on a solo validator, `try_trigger_consensus()` externalizes the node's
    /// *own* slot synchronously, and `publish_externalized()` does NOT wake the
    /// event loop (unlike a network EXTERNALIZE arriving over `scp_message_rx`,
    /// which the corresponding select arm already follows with a pipeline pump).
    /// Without this signal, a self-externalized slot would not be applied until
    /// the next 1-second maintenance tick — the cold-start ledger-production
    /// stall behind the `test_horizon_ingesting` Quickstart failure. stellar-core
    /// has no analog because its asio externalize callback drives the close
    /// directly; this return is henyey-specific glue, not a parity divergence.
    pub(super) async fn handle_scp_timer_event(
        &self,
        event: scp_timer_bridge::ScpTimerEvent,
    ) -> bool {
        // Gate: only validators process SCP timeouts
        if !self.is_validator {
            return false;
        }
        // Gate: must be in a state that can receive SCP messages
        if !self.herder.state().can_receive_scp() {
            return false;
        }
        // Gate: reject events from a prior tracking epoch. These were already
        // queued in scp_timer_rx before on_lost_sync() incremented the epoch,
        // so cancel_all_timers_nonblocking() could not retract them.
        let current_epoch = self.scp_timer_epoch.load(Ordering::Acquire);
        if event.epoch != current_epoch {
            tracing::debug!(
                slot = event.slot,
                event_epoch = event.epoch,
                current_epoch,
                "Dropping stale timer event from prior tracking epoch"
            );
            return false;
        }

        // Event-driven consensus trigger (#2702). This is NOT an SCP
        // nomination/ballot timeout, so it bypasses the future-slot defer /
        // old-slot drop classification below (which is specific to SCP timers).
        // A trigger for a stale slot (already closed) is harmlessly absorbed by
        // try_trigger_consensus's own LCL/tracking gates.
        if event.timer_type == henyey_herder::TimerType::TriggerNextLedger {
            self.consensus_trigger_timer_fires
                .fetch_add(1, Ordering::Relaxed);
            self.try_trigger_consensus().await;
            // Single-shot per ledger (parity: stellar-core arms mTriggerTimer
            // only from `lastClosedLedgerIncreased`). We deliberately do NOT
            // re-arm here. While a slot is still being nominated, LCL has not
            // advanced, so `next_slot` is unchanged and `prepare_start(last) +
            // expectedClose` is already in the past — a re-arm would compute
            // `delay == 0` and the timer would fire again immediately, spinning
            // at hundreds of Hz for the whole nomination→externalize window and
            // starving the event loop that services inbound SCP envelopes
            // (maxtps: slow/bimodal nomination convergence). The next trigger is
            // armed by the post-close path (`setup_trigger_next_ledger` after a
            // ledger closes); the 1-second maintenance-tick
            // `try_trigger_consensus` backstops any gated/never-armed case.
            // Signal the caller to pump the close pipeline for a possible
            // self-externalized slot (see the doc comment).
            return true;
        }

        // Use max(tracking_slot, current_ledger + 1) as the effective next slot.
        // In the INV-H2 corrective window where LCL + 1 > tracking_slot, a timer
        // for LCL + 1 is current from the app's perspective and must fire immediately
        // rather than being deferred as a "future" slot.
        let tracking_next = self.herder.next_consensus_ledger_index().get();
        let lcl_next = self.current_ledger_seq() as u64 + 1;
        let next_slot = tracking_next.max(lcl_next);
        let action = classify_timer_event(event.slot, next_slot, self.herder.is_tracking());

        match action {
            TimerEventAction::Drop => {
                // Stale old-slot timer — cancel all timers for this slot.
                // Parity: stellar-core's timerCallbackWrapper routes mismatched
                // slots through setupTimer(), which erases the entire slot's
                // timer set when slotIndex <= trackingConsensusLedgerIndex().
                // This ensures sibling timers for the same stale slot are also
                // cleaned up rather than firing later.
                self.timer_manager_handle
                    .cancel_slot_timers(event.slot)
                    .await;
            }
            TimerEventAction::Rearm => {
                // Future-slot timer while tracking — re-arm for 1 second.
                // Spec: HERDER_SPEC §5.4-2: stellar-core's timerCallbackWrapper
                // defers future-slot timer callbacks by re-scheduling them at a
                // 1-second interval until the slot becomes current.
                //
                // Preserve the *originating* epoch (`event.epoch`) across the
                // defer loop rather than letting the timer manager re-sample the
                // current global epoch. Otherwise, if on_lost_sync() bumps the
                // epoch between this event firing and the re-arm command being
                // processed, the re-armed timer would be stamped with the new
                // epoch and survive into Syncing — a "rebirth" that stellar-core
                // cannot do, since its timerCallbackWrapper/setupTimer defer loop
                // operates on the same in-process timer state. The event already
                // passed the epoch guard above (event.epoch == current_epoch), so
                // carrying event.epoch keeps the re-armed timer rejectable once
                // the epoch advances.
                let one_second = std::time::Duration::from_secs(1);
                match event.timer_type {
                    henyey_herder::TimerType::Nomination => {
                        self.timer_manager_handle
                            .reschedule_nomination_timeout_with_epoch(
                                event.slot,
                                one_second,
                                event.epoch,
                            )
                            .await;
                    }
                    henyey_herder::TimerType::Ballot => {
                        self.timer_manager_handle
                            .reschedule_ballot_timeout_with_epoch(
                                event.slot,
                                one_second,
                                event.epoch,
                            )
                            .await;
                    }
                    // TriggerNextLedger is handled by the early return above and
                    // never reaches the SCP-timeout defer/drop classification.
                    henyey_herder::TimerType::TriggerNextLedger => {}
                }
            }
            TimerEventAction::Fire => {
                // Current slot or non-tracking — execute the timeout handler.
                match event.timer_type {
                    henyey_herder::TimerType::Nomination => {
                        self.nomination_timeout_fires
                            .fetch_add(1, Ordering::Relaxed);
                        let outcome = self
                            .herder
                            .handle_nomination_timeout_blocking(event.slot)
                            .await;
                        if matches!(outcome, henyey_herder::TimeoutOutcome::SkippedStale) {
                            self.nomination_timeout_skipped_stale
                                .fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    henyey_herder::TimerType::Ballot => {
                        self.ballot_timeout_fires.fetch_add(1, Ordering::Relaxed);
                        self.herder.handle_ballot_timeout(event.slot);
                    }
                    // TriggerNextLedger is handled by the early return above.
                    henyey_herder::TimerType::TriggerNextLedger => {}
                }
            }
        }
        // SCP nomination/ballot timeouts do not externalize a new slot, so the
        // caller need not pump the close pipeline.
        false
    }

    /// Trigger a recovery catchup: clear stale state and run catchup to current.
    ///
    /// Used by both the normal escalation path (after RECOVERY_ESCALATION_CATCHUP
    /// attempts) and the fast-track path (when SCP messages arrive but tx_sets
    /// are evicted from peers' caches).
    pub(super) async fn trigger_recovery_catchup(
        &self,
        current_ledger: u32,
        latest_externalized: u64,
        relation: LedgerRelation,
        attempts: u64,
    ) -> Option<PendingCatchup> {
        // Fatal-failure guard: block further catchup/recovery after an
        // unrecoverable local state failure.
        if self.fatal_state_failure.load(Ordering::SeqCst) {
            let now_secs = self.start_instant.elapsed().as_secs();
            if self
                .recovery_throttles
                .fatal_state_blocked
                .should_log(now_secs)
            {
                tracing::warn!(
                    "Recovery escalation blocked: previous fatal state failure — \
                     manual intervention required"
                );
            } else {
                tracing::debug!(
                    "Recovery escalation blocked: previous fatal state failure (repeated)"
                );
            }
            return None;
        }

        // Read backoff state early so the entry log can be demoted (#1843).
        self.set_phase_sub(super::phase::PHASE_13_5_BUFFERED_ARCHIVE_BEHIND_READ);
        let backoff_active = self
            .archive_recovery_snapshot()
            .await
            .is_backoff_active(self.clock.now());

        if backoff_active {
            crate::metrics::RECOVERY_STALLED_TICK_TOTAL.increment("backoff_active", 1);
            tracing::debug!(
                current_ledger,
                latest_externalized,
                gap = relation.behind_gap().unwrap_or(0),
                attempts,
                "Recovery stalled (archive-behind backoff active)"
            );
        } else {
            let cache_age = self.archive_checkpoint_cache.last_query_age();
            let cache_age_secs = cache_age.map(|d| d.as_secs());
            let urgent = self.archive_checkpoint_cache.is_urgent();
            // Demote to debug when the node is not actually behind consensus.
            // The fast-track caller already emits its own WARN before entering
            // this function, so the INFO here is redundant noise at gap=0.
            if !relation.is_behind() {
                crate::metrics::RECOVERY_STALLED_TICK_TOTAL
                    .increment("forcing_catchup_not_behind", 1);
                tracing::debug!(
                    current_ledger,
                    latest_externalized,
                    gap = relation.behind_gap().unwrap_or(0),
                    attempts,
                    ?cache_age_secs,
                    urgent,
                    "Recovery stalled for too long — forcing catchup"
                );
            } else if is_spurious_near_tip_behind(
                relation.behind_gap().unwrap_or(0),
                self.effective_peer_gap(current_ledger),
            ) {
                // #3733: latest_externalized is transiently 1–2 ahead of
                // current_ledger (LedgerRelation reports Behind) but verified
                // peers are NOT ahead (effective_peer_gap == 0) — the
                // inbound-peer-flap near-tip blip, not a genuine fall-behind.
                // Count it in the benign bucket and log at debug so it does not
                // inflate the forcing_catchup_behind alarm (the +81/+431 bursts).
                crate::metrics::RECOVERY_STALLED_TICK_TOTAL
                    .increment("forcing_catchup_near_tip", 1);
                tracing::debug!(
                    current_ledger,
                    latest_externalized,
                    gap = relation.behind_gap().unwrap_or(0),
                    peer_gap = self.effective_peer_gap(current_ledger),
                    attempts,
                    ?cache_age_secs,
                    urgent,
                    "Recovery stalled near tip (verified peer_gap==0) — forcing catchup (#3733)"
                );
            } else {
                crate::metrics::RECOVERY_STALLED_TICK_TOTAL.increment("forcing_catchup_behind", 1);
                tracing::info!(
                    current_ledger,
                    latest_externalized,
                    gap = relation.behind_gap().unwrap_or(0),
                    attempts,
                    ?cache_age_secs,
                    urgent,
                    "Recovery stalled for too long — forcing catchup"
                );
            }
        }

        let next_cp = henyey_history::checkpoint::checkpoint_containing(current_ledger + 1);

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

        // Non-blocking read. `Cold` means cache never populated — fall
        // through to the peer-SCP fallback. `Fresh`/`Stale` with a value
        // below `next_cp` means archive is behind — also fall through. The
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
                CacheResult::Fresh(latest) | CacheResult::Stale(latest) if latest >= next_cp => {
                    // Archive is current enough — clear any prior backoff,
                    // urgent mode, and the confirmed-behind signal (#1867).
                    self.clear_archive_recovery_state(
                        ArchiveRecoveryClear::ArchiveConfirmedCurrent,
                    )
                    .await;
                    Some(latest)
                }
                CacheResult::Fresh(latest) | CacheResult::Stale(latest) => {
                    // Archive responded but is still behind the next
                    // checkpoint.  Do NOT arm backoff here:
                    // the cache's own TTL already throttles actual HTTP
                    // queries, and adding a 15–60 s backoff on top prevents
                    // this function from even reading the cache for that
                    // window — delaying detection of a newly-published
                    // checkpoint by up to 120 s (see #1847).
                    //
                    // Instead, signal the stuck state machine via the
                    // archive recovery status (#1867) so it can see
                    // `archive_behind=true` on the next evaluation without
                    // waiting for the slower TriggerCatchup→validation→backoff
                    // pipeline.
                    //
                    self.mark_archive_confirmed_behind().await;
                    tracing::debug!(
                        archive_latest = latest,
                        next_checkpoint = next_cp,
                        "Archive behind next checkpoint — signaled stuck state machine"
                    );
                    None
                }
                CacheResult::Cold => {
                    // Cache cold — a background refresh has been spawned
                    // and will complete within a few seconds. Do NOT arm
                    // backoff: that suppression exists to prevent redundant
                    // queries against a known-behind archive, and `Cold` is
                    // a transient state (refresh in flight), not a confirmed
                    // "archive behind" signal.
                    // Armoring 60s would force a skip across ~5 recovery
                    // ticks even after the refresh completes on tick 2.
                    // Fall through to peer-SCP fallback; the next tick
                    // (10 s later) will see the refreshed cache.
                    if self.tx_set_all_peers_exhausted.load(Ordering::SeqCst) {
                        self.archive_checkpoint_cache.set_urgent(true);
                    }
                    tracing::debug!(
                        next_checkpoint = next_cp,
                        "Archive checkpoint cache cold — \
                         falling through to peer-SCP while refresh completes"
                    );
                    None
                }
            }
        };

        let archive_latest = match archive_latest {
            Some(latest) => latest,
            None => {
                // ── #3848 wall-clock wedge escape (BEFORE the near-tip /
                // far-behind split) ─────────────────────────────────────
                // The archive is not ahead (this whole block), so the
                // near-tip branch below would route every tick into peer-SCP
                // back-fill + retry_exhausted_tx_sets and return None. That is
                // correct while peers CAN eventually serve the slot, but when
                // all peers have been exhausted for the tx_set past the
                // wall-clock deadline, no retry produces the missing hash and
                // no gap signal (latest_ext, peer_gap) ever moves — the node
                // wedges at tip (the 5m39s incident). Force the state-clearing
                // hard reset here, ahead of the split, so the escape is
                // reachable even when the archive-confirmed-behind sub-state is
                // not (yet) set (placement moved earlier per Critic A). The
                // hard reset's internal cooldown / livelock breaker still
                // apply; on suppression it returns None and we fall through to
                // the existing near-tip / far-behind handling below.
                if self.tx_set_wall_clock_wedged() {
                    use super::types::HardResetReason;
                    if let Some(pc) = self
                        .force_post_catchup_hard_reset(
                            current_ledger,
                            HardResetReason::ArchiveBehindTxSetExhausted,
                        )
                        .await
                    {
                        return Some(pc);
                    }
                }

                // ── Peer-ahead hard-reset escalation (issue #2349) ──────
                // When the archive is confirmed behind AND verified peers
                // are ahead by a meaningful gap AND we've been stuck for
                // enough recovery ticks, escalate to hard reset.
                let archive_is_confirmed_behind =
                    self.archive_recovery_snapshot().await.is_confirmed_behind();
                let peer_gap = self.effective_peer_gap(current_ledger);
                // Near-tip band (#3181/#3197): peers verified ahead by less
                // than one checkpoint interval. The guard below
                // (`near_tip && archive_is_confirmed_behind`) imposes NO lower
                // bound on peer_gap — it also fires for tiny gaps (peer_gap of
                // 0/1/2, below PEER_AHEAD_ESCALATION_THRESHOLD=3). That is
                // intentional and harmless: re-broadcasting SCP + a bounded
                // peer pull is a strict improvement over the request-only
                // fallback even for tiny gaps, and mirrors stellar-core
                // outOfSyncRecovery, which runs regardless of gap. The
                // upper-bounded `peer_gap < checkpoint_frequency()` predicate
                // is the only band constraint; read checkpoint_frequency()
                // live (64 default / 8 accelerated) for parity.
                let near_tip = peer_gap < checkpoint_frequency() as u64;

                // Recovery-ROUTING fix (#3197, PRIMARY — this is the dominant
                // production path because escalate_recovery_to_catchup() pins
                // recovery attempts at RECOVERY_ESCALATION_CATCHUP on every
                // far-ahead EXTERNALIZE, short-circuiting out_of_sync_recovery
                // into this function before it reaches its own
                // broadcast_recovery_scp_state tail). In the near-tip /
                // archive-confirmed-behind band, archive catchup is
                // structurally doomed: the next checkpoint is unpublished, so
                // the ProbeAhead fetch in force_post_catchup_hard_reset bails
                // on every fire (#3187 escalated the TIMING but routed here).
                // stellar-core in this regime does NO archive interaction — it
                // re-broadcasts its own latest SCP from lcl+1 and pulls SCP
                // state from 2 peers (HerderImpl::outOfSyncRecovery,
                // HerderImpl.cpp:521-561), then applies the buffered ledgers
                // once the gap is back-filled (tryApplySyncingLedgers,
                // LedgerApplyManagerImpl.cpp:156-160). So instead of the doomed
                // ProbeAhead HardReset, run the EXISTING full outOfSyncRecovery
                // analog (broadcast_recovery_scp_state — re-broadcast + bounded
                // peer pull) every recovery tick and return None, letting the
                // buffered-apply path advance. The genuine 120s wall-clock
                // stall backstop (#2789) still escalates via the decide-fn
                // (ArchiveBehindStallWallClock), so peer-SCP failure cannot
                // wedge the node forever. The far-behind (#1862) band keeps the
                // unchanged ProbeAhead/archive escalation below.
                if near_tip && archive_is_confirmed_behind {
                    tracing::info!(
                        current_ledger,
                        peer_max_verified = self.max_verified_scp_slot.load(Ordering::Relaxed),
                        peer_gap,
                        attempts,
                        next_checkpoint = next_cp,
                        "Near-tip recovery: archive confirmed behind unpublished checkpoint \
                         — routing to peer-SCP back-fill (no archive catchup), mirroring \
                         stellar-core outOfSyncRecovery"
                    );
                    crate::metrics::RECOVERY_STALLED_TICK_TOTAL
                        .increment("near_tip_peer_scp_recovery", 1);
                    // Full outOfSyncRecovery analog: re-broadcast own latest
                    // SCP from lcl+1 + bounded peer SCP pull (#3197 parity gap:
                    // the inline fallback below only does the request half).
                    self.broadcast_recovery_scp_state(current_ledger).await;
                    // Also retry exhausted tx_set fetches so the small
                    // contiguous gap can be back-filled from peers that
                    // re-acquired the tx_sets.
                    self.retry_exhausted_tx_sets().await;

                    // ── #3318 wider peer-SCP pull (operator-approved henyey
                    // deviation from upstream's fixed 2-peer getMoreSCPState) ──
                    // The steady-state bounded re-broadcast + 2-peer pull above
                    // is an EXACT upstream mirror and is always performed. But
                    // when the connected set collectively cannot serve the
                    // missing slot — `peers_could_serve(watermark) == 0` — the
                    // 2-peer pull re-lands on unserviceable peers every 10s and
                    // the node wedges (lcl frozen, attempts climbing). Once that
                    // unserviceable condition has persisted past the wall-clock
                    // deadline (the SAME #2789 stuck-onset clock, NOT the
                    // per-tick `last_scp_state_request_at` which advances every
                    // pull), issue ONE bounded wider GetScpState so the node can
                    // obtain the back-fill instead of restarting. Strictly
                    // additive and provably gated: it cannot fire while any peer
                    // can serve, and cannot fire before the deadline.
                    self.maybe_widen_near_tip_scp_pull().await;

                    // Do NOT re-arm sync_recovery_pending; the recovery timer
                    // drives the next tick (avoids a 1s spin loop).
                    return None;
                }

                // Far-behind (#1862) path: not near-tip (the near-tip band
                // returned early above into peer-SCP recovery). Keep the
                // unchanged 11-tick archive HardReset/ProbeAhead escalation.
                if archive_is_confirmed_behind
                    && peer_gap >= PEER_AHEAD_ESCALATION_THRESHOLD
                    && attempts >= RECOVERY_HARD_RESET_ESCALATION_ATTEMPTS
                    && !self.is_hard_reset_on_cooldown(peer_gap)
                {
                    use super::types::HardResetReason;
                    tracing::warn!(
                        current_ledger,
                        peer_max_verified = self.max_verified_scp_slot.load(Ordering::Relaxed),
                        peer_gap,
                        attempts,
                        next_checkpoint = next_cp,
                        "Recovery stuck: archive confirmed behind, peers verified ahead \
                         — escalating to hard reset"
                    );
                    crate::metrics::RECOVERY_STALLED_TICK_TOTAL
                        .increment("archive_behind_peer_ahead_hard_reset", 1);
                    // If the inner cooldown blocks, fall through to the
                    // peer-SCP fallback rather than returning an idle None
                    // (the inner cooldown uses a different gap metric).
                    if let Some(pc) = self
                        .force_post_catchup_hard_reset(
                            current_ledger,
                            HardResetReason::ArchiveBehindStallWallClock,
                        )
                        .await
                    {
                        return Some(pc);
                    }
                }

                // Demote to debug when backoff is already active (#1843):
                // the first "skipped" log is useful; repeating it every 10s
                // for up to 5 minutes is pure noise.
                if backoff_active {
                    tracing::debug!(
                        current_ledger,
                        next_checkpoint = next_cp,
                        "Recovery catchup skipped: archive behind (backoff active) \
                         — requesting SCP state from peers"
                    );
                } else {
                    tracing::info!(
                        current_ledger,
                        next_checkpoint = next_cp,
                        backoff_active,
                        "Recovery catchup skipped: archive hasn't published checkpoint yet \
                         — requesting SCP state from peers as fallback"
                    );
                }

                // While waiting for the archive, actively request SCP state
                // from peers. Some peers may still have tx_sets cached for
                // the missing slots, especially if they are slightly behind
                // the network tip. Without this, the node sits idle for 1-5
                // minutes until the next checkpoint publishes.
                if let Some(overlay) = self.overlay().await {
                    // Record timestamp before spawning so heartbeat/gap throttle
                    // sees this attempt and does not immediately duplicate it.
                    *self.last_scp_state_request_at.write().await = self.clock.now();
                    // Use the shared low-watermark helper (delegates to
                    // `Herder::get_min_ledger_seq_to_ask_peers()`, §15.3 parity)
                    // instead of raw current_ledger (#2904). `request_scp_state`
                    // is non-blocking, so call it inline rather than spawning.
                    let ledger = self.scp_state_request_ledger_seq();
                    if let Err(e) = overlay.request_scp_state(ledger) {
                        tracing::debug!(
                            error = %e,
                            "Failed to request SCP state during inter-checkpoint recovery"
                        );
                    }
                }

                // Retry exhausted tx_set fetches with 30s per-hash backoff.
                // Peers may have re-acquired the tx_set since they last said
                // DontHave (e.g., a slow peer catching up).
                self.retry_exhausted_tx_sets().await;

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
            self.reset_recovery_attempts(RecoveryResetMode::Full);
            // Cancel all SCP timers — recovery catchup resets consensus
            // state so any pending timers are stale.
            self.timer_manager_handle.cancel_all_timers().await;
        }

        result
    }

    /// #3318 — issue ONE bounded *wider* peer-SCP pull when the near-tip /
    /// archive-confirmed-behind recovery loop has been unable to find ANY peer
    /// that can serve the missing slot for longer than the wall-clock deadline.
    ///
    /// This is the henyey-specific deviation from upstream's fixed 2-peer
    /// `getMoreSCPState` (operator-approved). It is strictly additive on top of
    /// the steady-state bounded re-broadcast + 2-peer pull, and provably gated:
    ///
    /// - **Serviceability gate:** fires only when
    ///   `peers_could_serve(watermark) == 0` — i.e. NO connected peer has been
    ///   observed externalizing recently enough to still hold the missing slot.
    ///   While any peer can serve, the 2-peer pull is sufficient and this is a
    ///   no-op (exact upstream behavior).
    /// - **Deadline gate:** fires only after the stuck condition has persisted
    ///   past `NEAR_TIP_WIDEN_STALL_SECS`, measured from the #2789 stuck-ONSET
    ///   clock (`consensus_stuck_state.stuck_start`). It deliberately does NOT
    ///   use `last_scp_state_request_at`, which advances on every pull and would
    ///   never show a sustained stall. When no stuck state is recorded (the
    ///   episode hasn't been classified as stuck), the deadline is treated as
    ///   not-yet-reached and this is a no-op.
    ///
    /// Fan-out is bounded: `min(serviceable, NEAR_TIP_WIDEN_MAX_PEERS)`, falling
    /// back to ALL authenticated peers when serviceable == 0 (a peer not
    /// recently *observed* externalizing may still hold the slot). The send is
    /// non-blocking (`try_send_to` inside `request_scp_state_widened`), so it
    /// can never freeze the event loop.
    async fn maybe_widen_near_tip_scp_pull(&self) {
        let Some(overlay) = self.overlay().await else {
            return;
        };

        let ledger_seq = self.scp_state_request_ledger_seq();
        let (serviceable, _connected) =
            overlay.peers_could_serve(ledger_seq, MAX_SLOTS_TO_REMEMBER);

        // Serviceability gate: while any peer can serve, the steady-state
        // 2-peer pull is sufficient — widening here would be a pure upstream
        // deviation for no benefit.
        if serviceable > 0 {
            return;
        }

        // Deadline gate: the unserviceable condition must have persisted past
        // the wall-clock deadline, measured from the stuck ONSET (#2789), not
        // the per-tick request timestamp.
        let stuck_secs = self
            .consensus_stuck_state
            .read()
            .await
            .as_ref()
            .map(|s| s.stuck_start.elapsed().as_secs())
            .unwrap_or(0);
        if stuck_secs < NEAR_TIP_WIDEN_STALL_SECS {
            return;
        }

        // serviceable == 0 → fall back to all authenticated peers (a peer not
        // recently observed externalizing may still hold the slot). Otherwise
        // bound at NEAR_TIP_WIDEN_MAX_PEERS.
        let cap = NEAR_TIP_WIDEN_MAX_PEERS;

        match overlay.request_scp_state_widened(ledger_seq, cap) {
            Ok(sent) => {
                crate::metrics::RECOVERY_STALLED_TICK_TOTAL.increment("near_tip_peer_scp_widen", 1);
                tracing::warn!(
                    ledger_seq,
                    peers_widened = sent,
                    stuck_secs,
                    "Near-tip recovery: no peer could serve the missing slot past the \
                     deadline — issuing ONE bounded wider GetScpState (#3318 recovery escape)"
                );
            }
            Err(e) => {
                tracing::debug!(
                    ledger_seq,
                    error = %e,
                    "Failed to issue wider GetScpState during near-tip recovery"
                );
            }
        }
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
        // Tell the overlay we're no longer synced so it sheds flood traffic
        // during catchup (parity: LedgerManager transitions to LM_CATCHING_UP_STATE).
        if let Some(flag) = self.overlay_synced.lock().unwrap().as_ref() {
            flag.store(false, Ordering::Relaxed);
        }

        // A catchup is now in flight — the stuck signal is stale. Clear it
        // so /health reflects CatchingUp truthfully. If the catchup fails,
        // the next buffered-catchup tick re-detects the stall and
        // repopulates the state.
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
            // Honor the operator's configured CATCHUP_COMPLETE /
            // CATCHUP_RECENT policy on online recovery, matching
            // stellar-core's getCatchupCount() behavior. See #2104.
            let mode = app.live_catchup_mode();
            let catchup_result = app
                .catchup_with_run_mode(target, mode, CatchupRunMode::Online, finalize)
                .await;

            let persist_ready = match &catchup_result {
                Ok(_) => persist_rx.try_recv().ok(),
                Err(_) => None,
            };

            // #3282: forward whether this catchup was seeded from cloned local
            // state, so the event loop's `handle_catchup_result` can route a
            // local-vs-archive divergence to an archive rebuild (self-heal)
            // instead of a terminal wipe. Read after the catchup returns —
            // race-free because `catchup_in_progress` serializes catchups.
            let seeded_from_local_clone = app
                .last_catchup_seeded_from_local_clone
                .load(Ordering::SeqCst);

            let _ = result_tx.send(PendingCatchupResult::new(
                catchup_result,
                persist_ready,
                seeded_from_local_clone,
            ));
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
    use super::{
        classify_cannot_apply_reason, is_near_tip_gap_suppressed, is_near_tip_park_inflated,
        is_spurious_near_tip_behind, near_tip_stall_is_wall_clock_backed,
        should_escalate_to_catchup, CannotApplyReason, LedgerRelation,
        RECOVERY_ESCALATION_NEAR_TIP_GAP_STALL_ATTEMPTS,
    };
    use std::time::Duration;

    /// A `streak_elapsed` comfortably above the near-tip wall-clock threshold
    /// (#3748), so the #3728 attempt-count assertions hold under the new gate.
    const WALL_CLOCK_BACKED: Duration = Duration::from_secs(60);

    // ── is_spurious_near_tip_behind tests (#3733) ───────────────────────

    #[test]
    fn test_near_tip_behind_not_counted_as_forcing_catchup_behind() {
        // #3733: when herder.latest_externalized is transiently 1–2 ahead of
        // current_ledger BUT verified peers are NOT ahead (peer_gap == 0), the
        // `is_behind()` relation is a spurious near-tip blip caused by
        // inbound-peer flapping — not a genuine fall-behind. It must NOT be
        // counted as `forcing_catchup_behind`.
        assert!(
            is_spurious_near_tip_behind(1, 0),
            "gap=1, peer_gap=0 is a spurious near-tip blip"
        );
        assert!(
            is_spurious_near_tip_behind(2, 0),
            "gap=2, peer_gap=0 is a spurious near-tip blip"
        );
        // Genuine behind-the-network: peers ARE ahead → not spurious, must
        // still fire forcing_catchup_behind and escalate.
        assert!(
            !is_spurious_near_tip_behind(1, 3),
            "gap=1 but peer_gap=3 is genuine behind — peers are ahead"
        );
        // Large local behind gap: even with peer_gap==0 this exceeds the
        // near-tip window (TX_SET_REQUEST_WINDOW=12) → not treated as a
        // near-tip blip.
        assert!(
            !is_spurious_near_tip_behind(15, 0),
            "gap=15 exceeds the near-tip window — not spurious"
        );
    }

    // ── should_escalate_to_catchup tests (#3728) ────────────────────────

    #[test]
    fn test_should_escalate_to_catchup_suppresses_single_ledger_gap() {
        // Exact issue #3728 scenario: attempts=8, gap==1 → no forced catchup.
        // The node self-recovers via the peer-SCP back-fill path instead.
        // attempts=8 is below the persistent-stall threshold (12), so the
        // momentary-blip carve-out holds.
        assert!(!should_escalate_to_catchup(
            8,
            LedgerRelation::Behind { gap: 1 },
            false,
            WALL_CLOCK_BACKED,
        ));
    }

    #[test]
    fn test_should_escalate_to_catchup_resumes_on_persistent_single_ledger_stall() {
        // #3728 review follow-up: a gap==1 that persists past the stall
        // threshold must RESUME escalation so a genuinely-wedged node keeps its
        // peer-independent archive backstop + forcing_catchup_behind alarm.
        // #3748: the streak must also be backed by real wall-clock time.
        let stall = RECOVERY_ESCALATION_NEAR_TIP_GAP_STALL_ATTEMPTS;
        // Just below the threshold: still suppressed.
        assert!(!should_escalate_to_catchup(
            stall - 1,
            LedgerRelation::Behind { gap: 1 },
            false,
            WALL_CLOCK_BACKED,
        ));
        // At and above the threshold, with a wall-clock-backed streak:
        // escalation resumes.
        assert!(should_escalate_to_catchup(
            stall,
            LedgerRelation::Behind { gap: 1 },
            false,
            WALL_CLOCK_BACKED,
        ));
        assert!(should_escalate_to_catchup(
            stall + 5,
            LedgerRelation::Behind { gap: 1 },
            false,
            WALL_CLOCK_BACKED,
        ));
    }

    #[test]
    fn test_is_near_tip_gap_suppressed_identifies_carveout_ticks() {
        let stall = RECOVERY_ESCALATION_NEAR_TIP_GAP_STALL_ATTEMPTS;
        // The exact #3728 tick: attempt floor crossed, gap==1, below stall
        // threshold → suppressed (observability metric fires here).
        assert!(is_near_tip_gap_suppressed(
            8,
            LedgerRelation::Behind { gap: 1 },
            false,
            WALL_CLOCK_BACKED,
        ));
        // Below the attempt floor: not a suppression tick (no escalation would
        // have fired anyway).
        assert!(!is_near_tip_gap_suppressed(
            5,
            LedgerRelation::Behind { gap: 1 },
            false,
            WALL_CLOCK_BACKED,
        ));
        // At/above the stall threshold WITH a wall-clock-backed streak:
        // escalation resumes, so this is NOT a suppressed tick —
        // forcing_catchup_behind fires instead.
        assert!(!is_near_tip_gap_suppressed(
            stall,
            LedgerRelation::Behind { gap: 1 },
            false,
            WALL_CLOCK_BACKED,
        ));
        // Multi-ledger gap escalates directly — never a suppression tick.
        assert!(!is_near_tip_gap_suppressed(
            8,
            LedgerRelation::Behind { gap: 2 },
            false,
            WALL_CLOCK_BACKED,
        ));
        // AtTip / ahead-no-ext are not near-tip-behind suppressions.
        assert!(!is_near_tip_gap_suppressed(
            8,
            LedgerRelation::AtTip,
            false,
            WALL_CLOCK_BACKED,
        ));
        assert!(!is_near_tip_gap_suppressed(
            8,
            LedgerRelation::Ahead,
            true,
            WALL_CLOCK_BACKED,
        ));
    }

    #[test]
    fn test_should_escalate_to_catchup_fires_on_multi_ledger_gap() {
        // A genuine multi-ledger stall still escalates once the attempt floor
        // is crossed.
        assert!(should_escalate_to_catchup(
            6,
            LedgerRelation::Behind { gap: 2 },
            false,
            WALL_CLOCK_BACKED,
        ));
        assert!(should_escalate_to_catchup(
            8,
            LedgerRelation::Behind { gap: 12 },
            false,
            WALL_CLOCK_BACKED,
        ));
    }

    #[test]
    fn test_should_escalate_to_catchup_respects_attempt_floor() {
        // Below the attempt threshold, never escalate regardless of gap.
        assert!(!should_escalate_to_catchup(
            5,
            LedgerRelation::Behind { gap: 5 },
            false,
            WALL_CLOCK_BACKED,
        ));
    }

    #[test]
    fn test_should_escalate_to_catchup_preserves_ahead_no_ext() {
        // Captive-core startup path (Ahead with no externalization) must still
        // escalate so recovery converges. The ahead-no-ext branch is NOT
        // wall-clock-gated, so even a tiny streak_elapsed escalates.
        assert!(should_escalate_to_catchup(
            6,
            LedgerRelation::Ahead,
            true,
            Duration::from_millis(1),
        ));
        // AtTip never escalates here — it is handled downstream.
        assert!(!should_escalate_to_catchup(
            6,
            LedgerRelation::AtTip,
            false,
            WALL_CLOCK_BACKED,
        ));
    }

    // ── near-tip wall-clock gate tests (#3748) ──────────────────────────

    #[test]
    fn test_near_tip_burst_park_does_not_resume_escalation() {
        // #3748: a post-park `MissedTickBehavior::Burst` replay inflates
        // `attempts` past the stall threshold in sub-second real time. The
        // wall-clock gate must hold near-tip gap==1 escalation back so the
        // spurious `forcing_catchup_behind` alarm does not fire.
        let stall = RECOVERY_ESCALATION_NEAR_TIP_GAP_STALL_ATTEMPTS;
        assert!(!should_escalate_to_catchup(
            stall,
            LedgerRelation::Behind { gap: 1 },
            false,
            Duration::from_millis(500),
        ));
    }

    #[test]
    fn test_near_tip_genuine_stall_still_escalates() {
        // A genuinely wedged node at gap==1 whose no-progress streak is backed
        // by real wall-clock time (>= ~12s) must still escalate, preserving the
        // peer-independent archive backstop + `forcing_catchup_behind` alarm
        // (#3728 review intent).
        let stall = RECOVERY_ESCALATION_NEAR_TIP_GAP_STALL_ATTEMPTS;
        assert!(should_escalate_to_catchup(
            stall,
            LedgerRelation::Behind { gap: 1 },
            false,
            Duration::from_secs(15),
        ));
    }

    #[test]
    fn test_multi_ledger_gap_escalates_regardless_of_park() {
        // A multi-ledger fall-behind is a genuine catchup trigger and is NOT
        // wall-clock-gated: it must escalate immediately even with a tiny
        // streak_elapsed (post-park).
        assert!(should_escalate_to_catchup(
            6,
            LedgerRelation::Behind { gap: 5 },
            false,
            Duration::from_millis(100),
        ));
    }

    #[test]
    fn test_near_tip_stall_wall_clock_backed_boundary() {
        let stall_secs = RECOVERY_ESCALATION_NEAR_TIP_GAP_STALL_ATTEMPTS;
        // Just below the wall-clock threshold: not backed.
        assert!(!near_tip_stall_is_wall_clock_backed(
            stall_secs,
            Duration::from_secs(stall_secs) - Duration::from_millis(1),
        ));
        // At the threshold: backed.
        assert!(near_tip_stall_is_wall_clock_backed(
            stall_secs,
            Duration::from_secs(stall_secs),
        ));
        // Above the threshold: backed.
        assert!(near_tip_stall_is_wall_clock_backed(
            stall_secs,
            Duration::from_secs(stall_secs) + Duration::from_secs(5),
        ));
    }

    #[test]
    fn test_near_tip_park_inflated_predicate_classifies_park_ticks() {
        // `is_near_tip_park_inflated` is true exactly when escalation WOULD have
        // fired on the old attempt-only gate (attempts >= STALL, gap <=
        // NEAR_TIP_GAP) but the wall-clock gate holds it back.
        let stall = RECOVERY_ESCALATION_NEAR_TIP_GAP_STALL_ATTEMPTS;
        assert!(is_near_tip_park_inflated(
            stall,
            LedgerRelation::Behind { gap: 1 },
            false,
            Duration::from_millis(500),
        ));
        // Wall-clock-backed genuine stall → escalates, not park-inflated.
        assert!(!is_near_tip_park_inflated(
            stall,
            LedgerRelation::Behind { gap: 1 },
            false,
            Duration::from_secs(15),
        ));
        // Below the stall threshold → classic momentary carve-out, not
        // park-inflated (that tick is `near_tip_gap1_suppressed`).
        assert!(!is_near_tip_park_inflated(
            8,
            LedgerRelation::Behind { gap: 1 },
            false,
            Duration::from_millis(100),
        ));
    }

    // ── LedgerRelation tests ────────────────────────────────────────────

    #[test]
    fn test_ledger_relation_behind() {
        assert_eq!(
            LedgerRelation::from_ledgers(100, 150),
            LedgerRelation::Behind { gap: 50 }
        );
    }

    #[test]
    fn test_ledger_relation_at_tip() {
        assert_eq!(
            LedgerRelation::from_ledgers(100, 100),
            LedgerRelation::AtTip
        );
    }

    #[test]
    fn test_ledger_relation_ahead() {
        assert_eq!(LedgerRelation::from_ledgers(100, 50), LedgerRelation::Ahead);
    }

    #[test]
    fn test_ledger_relation_ahead_startup() {
        // On startup, latest_externalized is 0 while current_ledger >= 1.
        assert_eq!(LedgerRelation::from_ledgers(1, 0), LedgerRelation::Ahead);
    }

    #[test]
    fn test_ledger_relation_behind_by_one() {
        assert_eq!(
            LedgerRelation::from_ledgers(100, 101),
            LedgerRelation::Behind { gap: 1 }
        );
    }

    #[test]
    fn test_ledger_relation_at_tip_zero() {
        assert_eq!(LedgerRelation::from_ledgers(0, 0), LedgerRelation::AtTip);
    }

    #[test]
    fn test_ledger_relation_at_tip_u32_max() {
        assert_eq!(
            LedgerRelation::from_ledgers(u32::MAX, u32::MAX as u64),
            LedgerRelation::AtTip
        );
    }

    #[test]
    fn test_ledger_relation_behind_max() {
        assert_eq!(
            LedgerRelation::from_ledgers(0, u64::MAX),
            LedgerRelation::Behind { gap: u64::MAX }
        );
    }

    #[test]
    fn test_behind_gap_returns_some_for_behind() {
        let r = LedgerRelation::Behind { gap: 42 };
        assert_eq!(r.behind_gap(), Some(42));
    }

    #[test]
    fn test_behind_gap_returns_none_for_at_tip_and_ahead() {
        assert_eq!(LedgerRelation::AtTip.behind_gap(), None);
        assert_eq!(LedgerRelation::Ahead.behind_gap(), None);
    }

    #[test]
    fn test_is_behind() {
        assert!(LedgerRelation::Behind { gap: 1 }.is_behind());
        assert!(!LedgerRelation::AtTip.is_behind());
        assert!(!LedgerRelation::Ahead.is_behind());
    }

    // ── classify_cannot_apply_reason tests ──────────────────────────────

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

    // ── is_ahead_without_externalization tests ──────────────────────────

    #[test]
    fn test_ahead_no_ext_true_for_startup_state() {
        let rel = LedgerRelation::from_ledgers(29, 0);
        assert!(rel.is_ahead_without_externalization(0));
    }

    #[test]
    fn test_ahead_no_ext_false_for_ahead_with_nonzero_ext() {
        // Ahead but latest_ext > 0 → NOT the startup livelock case.
        let rel = LedgerRelation::from_ledgers(100, 50);
        assert!(!rel.is_ahead_without_externalization(50));
    }

    #[test]
    fn test_ahead_no_ext_false_for_at_tip() {
        let rel = LedgerRelation::from_ledgers(0, 0);
        assert!(!rel.is_ahead_without_externalization(0));
    }

    #[test]
    fn test_ahead_no_ext_false_for_behind() {
        let rel = LedgerRelation::from_ledgers(50, 100);
        assert!(!rel.is_ahead_without_externalization(100));
    }

    // ── should_delay_checkpoint tests ───────────────────────────────────

    use super::should_delay_checkpoint;

    /// When checkpoint is below first_sequential (gap exists), delay is needed.
    /// Example: checkpoint=64, first_sequential=88 (tracking=100, MAX_SLOTS_TO_REMEMBER=12)
    #[test]
    fn test_should_delay_checkpoint_gap_exists() {
        assert!(should_delay_checkpoint(64, 88));
    }

    /// When checkpoint equals first_sequential, no gap — no delay.
    #[test]
    fn test_should_delay_checkpoint_at_boundary() {
        assert!(!should_delay_checkpoint(88, 88));
    }

    /// When checkpoint is above first_sequential, no gap — no delay.
    /// Example: checkpoint=128, first_sequential=88
    #[test]
    fn test_should_delay_checkpoint_no_gap() {
        assert!(!should_delay_checkpoint(128, 88));
    }

    /// Early ledgers: checkpoint=1 (genesis), first_sequential=1 — no delay.
    #[test]
    fn test_should_delay_checkpoint_early_ledgers() {
        assert!(!should_delay_checkpoint(1, 1));
    }

    /// Checkpoint at 1 with first_sequential > 1 — gap exists.
    /// This happens when the node is tracking well beyond the first checkpoint.
    #[test]
    fn test_should_delay_checkpoint_first_checkpoint_behind() {
        assert!(should_delay_checkpoint(1, 5));
    }

    // ── TimerEventAction / classify_timer_event tests ────────────────────

    use super::{classify_timer_event, TimerEventAction};

    /// Future-slot nomination timer while tracking → Rearm (1-second defer).
    /// Spec: HERDER_SPEC §5.4-2.
    #[test]
    fn test_classify_timer_event_future_nomination_tracking_rearms() {
        // next_slot = 100, event_slot = 105, tracking = true → Rearm
        assert_eq!(
            classify_timer_event(105, 100, true),
            TimerEventAction::Rearm
        );
    }

    /// Future-slot ballot timer while tracking → Rearm (1-second defer).
    /// Spec: HERDER_SPEC §5.4-2.
    #[test]
    fn test_classify_timer_event_future_ballot_tracking_rearms() {
        // next_slot = 50, event_slot = 51, tracking = true → Rearm
        assert_eq!(classify_timer_event(51, 50, true), TimerEventAction::Rearm);
    }

    /// Old-slot timer while tracking → Drop without re-arming.
    #[test]
    fn test_classify_timer_event_old_slot_tracking_drops() {
        // next_slot = 100, event_slot = 99, tracking = true → Drop
        assert_eq!(classify_timer_event(99, 100, true), TimerEventAction::Drop);
        // Far behind, tracking
        assert_eq!(classify_timer_event(1, 100, true), TimerEventAction::Drop);
    }

    /// Old-slot timer while NOT tracking → Drop (defense-in-depth).
    /// Parity: stellar-core fires timer callbacks unconditionally when !isTracking().
    /// Old-slot timers from prior tracking epochs are rejected by the epoch guard
    /// before reaching the classifier, so this path is safe.
    #[test]
    fn test_classify_timer_event_old_slot_not_tracking_fires() {
        // Not tracking: all timers fire (parity with stellar-core timerCallbackWrapper).
        assert_eq!(classify_timer_event(99, 100, false), TimerEventAction::Fire);
        assert_eq!(classify_timer_event(1, 100, false), TimerEventAction::Fire);
    }

    /// Current-slot timer → Fire immediately regardless of tracking state.
    #[test]
    fn test_classify_timer_event_current_slot_fires() {
        assert_eq!(classify_timer_event(100, 100, true), TimerEventAction::Fire);
        assert_eq!(
            classify_timer_event(100, 100, false),
            TimerEventAction::Fire
        );
    }

    /// Future-slot timer while NOT tracking → Fire (don't enter defer loop).
    #[test]
    fn test_classify_timer_event_future_slot_not_tracking_fires() {
        // Not tracking: even future-slot events should fire, not rearm.
        assert_eq!(
            classify_timer_event(105, 100, false),
            TimerEventAction::Fire
        );
    }

    /// Edge case: event_slot == next_slot + 1 while tracking → Rearm (just barely future).
    #[test]
    fn test_classify_timer_event_one_ahead_tracking_rearms() {
        assert_eq!(
            classify_timer_event(101, 100, true),
            TimerEventAction::Rearm
        );
    }

    // ── End-to-end handle_scp_timer_event tests ─────────────────────────
    //
    // These exercise the full App::handle_scp_timer_event path including the
    // timer_manager_handle scheduling and fire-counter side effects. Uses
    // paused tokio time to verify exact 1-second rearm timing.

    use super::super::App;
    use std::sync::atomic::Ordering;

    /// Helper: build a minimal validator App with paused-time-compatible config.
    async fn mk_validator_app() -> (tempfile::TempDir, App) {
        let dir = tempfile::tempdir().expect("temp dir");
        let db_path = dir.path().join("timer-event-test.db");
        let config = crate::config::ConfigBuilder::new()
            .database_path(db_path)
            .validator(true)
            .node_seed("SAFTEV5U6QDFE2DRMSD7HBE76XG7SQZJD6VIUTHIXTJGO77RUQYVURLA")
            .build();
        let app = App::new(config).await.unwrap();
        (dir, app)
    }

    /// End-to-end: future-slot nomination timer while tracking is re-armed at
    /// exactly 1 second and fires back through the timer bridge.
    ///
    /// Verifies:
    /// - nomination_timeout_fires counter does NOT increment on the defer path
    /// - the same timer type (nomination) is re-delivered after exactly 1 second
    #[tokio::test(start_paused = true)]
    async fn test_handle_scp_timer_event_e2e_future_nomination_rearms_one_second() {
        let (_dir, app) = mk_validator_app().await;

        // Bootstrap herder at ledger 1 → tracking slot = 2, is_tracking = true
        app.herder.bootstrap(1);
        assert!(app.herder.is_tracking());
        let next_slot = app.herder.next_consensus_ledger_index().get();
        assert_eq!(next_slot, 2);

        // Inject a nomination timer event for a future slot (slot 5 > next_slot 2)
        let future_slot = 5u64;
        let event = super::super::scp_timer_bridge::ScpTimerEvent {
            slot: future_slot,
            timer_type: henyey_herder::TimerType::Nomination,
            epoch: 0,
        };

        let fires_before = app.nomination_timeout_fires.load(Ordering::Relaxed);

        app.handle_scp_timer_event(event).await;

        // Fire counter must NOT have incremented (defer path, not fire path)
        assert_eq!(
            app.nomination_timeout_fires.load(Ordering::Relaxed),
            fires_before,
            "nomination_timeout_fires must not increment on the rearm path"
        );

        // Give the timer manager task a chance to receive and process the
        // ScheduleNominationTimeout command before we advance time.
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }

        // The timer manager should have scheduled a 1-second timeout.
        // Advance time by just under 1 second — no event should arrive yet.
        let mut rx = app.scp_timer_rx.lock().await;
        tokio::time::advance(std::time::Duration::from_millis(999)).await;
        // Yield multiple times to let the timer manager task process
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        assert!(
            rx.try_recv().is_err(),
            "timer must not fire before 1 second"
        );

        // Advance past the 1-second mark
        tokio::time::advance(std::time::Duration::from_millis(2)).await;
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }

        // The timer bridge should have delivered a new ScpTimerEvent
        let redelivered = rx
            .try_recv()
            .expect("expected re-delivered nomination timer event");
        assert_eq!(redelivered.slot, future_slot);
        assert_eq!(redelivered.timer_type, henyey_herder::TimerType::Nomination);
    }

    /// Regression (maxtps): handling a `TriggerNextLedger` timer event must NOT
    /// re-arm another `TriggerNextLedger` timer for the *same* slot. The
    /// trigger timer is single-shot per ledger (parity: stellar-core arms it
    /// only from `lastClosedLedgerIncreased`); the post-close path is the sole
    /// re-arm site.
    ///
    /// Pre-fix bug: `handle_scp_timer_event` re-armed unconditionally after
    /// every trigger fire. While a slot is still being nominated (LCL has not
    /// advanced, so `next_slot` is unchanged and `prepare_start(last) + 5s` is
    /// already in the past), the re-arm computed `delay == 0`, so the timer
    /// fired again immediately — a ~hundreds-of-Hz busy-loop that ran for the
    /// entire nomination→externalize window of every ledger, starving the
    /// event loop that services inbound SCP envelopes (slow/bimodal
    /// convergence). This test bootstraps that exact precondition (cold start,
    /// `prepare_start(LCL) == None` → trigger-time gate open, solo node does not
    /// externalize without the event loop, so LCL stays put) and asserts no
    /// self-perpetuating trigger event is re-delivered.
    #[tokio::test(start_paused = true)]
    async fn test_trigger_event_does_not_rearm_same_slot_spin() {
        let (_dir, app) = mk_validator_app().await;

        // LCL 1 → tracking, next slot 2. prepare_start(1) is None on cold start,
        // so try_trigger_consensus's trigger-time gate is open and the re-arm
        // (pre-fix) would compute delay 0 for slot 2 — the spin precondition.
        app.herder.bootstrap(1);
        assert!(app.herder.is_tracking());
        assert!(app.herder.prepare_start(1).is_none());

        // Deliver one TriggerNextLedger event for the current next slot.
        let event = super::super::scp_timer_bridge::ScpTimerEvent {
            slot: 2,
            timer_type: henyey_herder::TimerType::TriggerNextLedger,
            epoch: app.scp_timer_epoch.load(Ordering::Acquire),
        };
        app.handle_scp_timer_event(event).await;

        // Let the timer manager process any scheduled command and (pre-fix) fire
        // the zero-delay re-arm back into scp_timer_rx.
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }

        // No self-perpetuating TriggerNextLedger event must have been delivered.
        // Pre-fix: the delay-0 re-arm fires immediately and delivers one here
        // (and would do so forever) → this assertion FAILS. Post-fix: the
        // handler does not re-arm, so nothing is delivered.
        let mut rx = app.scp_timer_rx.lock().await;
        let mut spurious_triggers = 0;
        while let Ok(ev) = rx.try_recv() {
            if ev.timer_type == henyey_herder::TimerType::TriggerNextLedger {
                spurious_triggers += 1;
            }
        }
        assert_eq!(
            spurious_triggers, 0,
            "handling a TriggerNextLedger event must not re-arm a same-slot \
             trigger (busy-loop regression)"
        );
    }

    /// End-to-end: future-slot ballot timer while tracking is re-armed at
    /// exactly 1 second and fires back through the timer bridge.
    #[tokio::test(start_paused = true)]
    async fn test_handle_scp_timer_event_e2e_future_ballot_rearms_one_second() {
        let (_dir, app) = mk_validator_app().await;

        app.herder.bootstrap(1);
        let next_slot = app.herder.next_consensus_ledger_index().get();

        // Future-slot ballot event
        let future_slot = next_slot + 3;
        let event = super::super::scp_timer_bridge::ScpTimerEvent {
            slot: future_slot,
            timer_type: henyey_herder::TimerType::Ballot,
            epoch: 0,
        };

        let fires_before = app.ballot_timeout_fires.load(Ordering::Relaxed);

        app.handle_scp_timer_event(event).await;

        // Fire counter must NOT have incremented
        assert_eq!(
            app.ballot_timeout_fires.load(Ordering::Relaxed),
            fires_before,
            "ballot_timeout_fires must not increment on the rearm path"
        );

        // Give the timer manager task a chance to process the schedule command
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }

        // Advance time past 1 second and verify the ballot timer re-fires
        let mut rx = app.scp_timer_rx.lock().await;
        tokio::time::advance(std::time::Duration::from_millis(1001)).await;
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }

        let redelivered = rx
            .try_recv()
            .expect("expected re-delivered ballot timer event");
        assert_eq!(redelivered.slot, future_slot);
        assert_eq!(redelivered.timer_type, henyey_herder::TimerType::Ballot);
    }

    /// End-to-end: old-slot timer while tracking is dropped — no re-arm occurs,
    /// no fire counters advance, and no timer event is re-delivered.
    #[tokio::test(start_paused = true)]
    async fn test_handle_scp_timer_event_e2e_old_slot_drops_without_rearm() {
        let (_dir, app) = mk_validator_app().await;

        app.herder.bootstrap(5);
        let next_slot = app.herder.next_consensus_ledger_index().get();
        assert_eq!(next_slot, 6);

        // Old-slot nomination event (slot 3 < next_slot 6)
        let old_slot = 3u64;
        let event = super::super::scp_timer_bridge::ScpTimerEvent {
            slot: old_slot,
            timer_type: henyey_herder::TimerType::Nomination,
            epoch: 0,
        };

        let nom_fires_before = app.nomination_timeout_fires.load(Ordering::Relaxed);
        let ballot_fires_before = app.ballot_timeout_fires.load(Ordering::Relaxed);

        app.handle_scp_timer_event(event).await;

        // No fire counters should have incremented
        assert_eq!(
            app.nomination_timeout_fires.load(Ordering::Relaxed),
            nom_fires_before,
            "nomination_timeout_fires must not increment on the drop path"
        );
        assert_eq!(
            app.ballot_timeout_fires.load(Ordering::Relaxed),
            ballot_fires_before,
            "ballot_timeout_fires must not increment on the drop path"
        );

        // Advance time well past any possible re-arm delay and verify nothing arrives
        let mut rx = app.scp_timer_rx.lock().await;
        tokio::time::advance(std::time::Duration::from_secs(5)).await;
        tokio::task::yield_now().await;

        assert!(
            rx.try_recv().is_err(),
            "no timer event should be re-delivered for a dropped old-slot timer"
        );
    }

    /// End-to-end: old-slot timer cancels sibling timers for the same slot.
    ///
    /// Parity: stellar-core's setupTimer() erases the entire slot's timer set
    /// when slotIndex <= trackingConsensusLedgerIndex(). This test verifies
    /// that when an old-slot timer fires through the Drop path, any
    /// previously-scheduled sibling timer for the same slot is also cancelled.
    #[tokio::test(start_paused = true)]
    async fn test_handle_scp_timer_event_e2e_old_slot_cancels_sibling_timers() {
        let (_dir, app) = mk_validator_app().await;

        // Bootstrap at ledger 1 → next_slot = 2, tracking
        app.herder.bootstrap(1);
        assert!(app.herder.is_tracking());
        assert_eq!(app.herder.next_consensus_ledger_index().get(), 2);

        // Schedule a ballot timer for slot 3 (currently a future slot) via the
        // Rearm path. This simulates a ballot timer that was deferred.
        let slot = 3u64;
        app.timer_manager_handle
            .schedule_ballot_timeout(slot, std::time::Duration::from_secs(2))
            .await;

        // Yield to let the timer manager process the schedule command
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }

        // Now advance herder so slot 3 becomes an old slot (bootstrap at 5 →
        // next_slot = 6, so slot 3 < 6).
        app.herder.bootstrap(5);
        assert_eq!(app.herder.next_consensus_ledger_index().get(), 6);
        assert!(app.herder.is_tracking());

        // Fire a nomination timer event for the now-old slot 3.
        // This should hit the Drop path AND cancel all timers for slot 3.
        let event = super::super::scp_timer_bridge::ScpTimerEvent {
            slot,
            timer_type: henyey_herder::TimerType::Nomination,
            epoch: 0,
        };
        app.handle_scp_timer_event(event).await;

        // Yield to let cancel propagate
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }

        // Advance time well past the 2-second ballot timer and verify it was
        // cancelled — no timer event should arrive from the sibling ballot.
        let mut rx = app.scp_timer_rx.lock().await;
        tokio::time::advance(std::time::Duration::from_secs(5)).await;
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }

        assert!(
            rx.try_recv().is_err(),
            "sibling ballot timer for the old slot must be cancelled"
        );
    }

    /// Regression test for INV-H2 corrective window: when LCL + 1 > tracking_slot,
    /// a timer for LCL + 1 must fire immediately (not be deferred as a "future" slot).
    ///
    /// This exercises the max(tracking_slot, current_ledger + 1) floor that prevents
    /// an unintended 1-second delay in the corrective recovery path.
    #[tokio::test(start_paused = true)]
    async fn test_handle_scp_timer_event_e2e_lcl_ahead_of_tracking_fires_immediately() {
        let (_dir, app) = mk_validator_app().await;

        // Bootstrap herder at ledger 1 → tracking slot = 2, is_tracking = true
        app.herder.bootstrap(1);
        assert!(app.herder.is_tracking());
        assert_eq!(app.herder.next_consensus_ledger_index().get(), 2);

        // Advance the LCL to ledger 5 without advancing herder tracking.
        // This creates the INV-H2 corrective window: LCL=5, tracking_slot=2,
        // so current_ledger + 1 = 6 > tracking_slot = 2.
        {
            let mut header = app.ledger_manager().current_header();
            header.ledger_seq = 5;
            app.ledger_manager()
                .set_header_for_test(header, henyey_common::Hash256::default());
        }
        assert_eq!(app.current_ledger_seq(), 5);

        // A nomination timer for slot 6 (= current_ledger + 1) should fire
        // immediately — it is "current" from the app's perspective even though
        // it's ahead of the tracking slot.
        let event = super::super::scp_timer_bridge::ScpTimerEvent {
            slot: 6,
            timer_type: henyey_herder::TimerType::Nomination,
            epoch: 0,
        };

        let fires_before = app.nomination_timeout_fires.load(Ordering::Relaxed);
        app.handle_scp_timer_event(event).await;

        // The fire counter must increment — this timer should NOT be rearmed.
        assert_eq!(
            app.nomination_timeout_fires.load(Ordering::Relaxed),
            fires_before + 1,
            "timer for LCL+1 must fire immediately in the corrective window, not be rearmed"
        );
    }

    /// Regression test: stale SCP timers must NOT fire after Tracking → Syncing.
    ///
    /// Scenario: the node was tracking at slot 2 and loses sync. While syncing,
    /// the LCL advances (via catchup) to ledger 5. A stale ballot timer from the
    /// old tracking epoch (slot 2) fires. The handler must drop it because
    /// slot 2 < next_slot (6), not execute bump_ballot on outdated state.
    ///
    /// This regression was introduced when the non-tracking path unconditionally
    /// returned `Fire` for all slots. The fix drops old-slot timers regardless
    /// of tracking state.
    #[tokio::test(start_paused = true)]
    async fn test_handle_scp_timer_event_e2e_stale_timer_after_sync_loss_drops() {
        use henyey_herder::sync_recovery::SyncRecoveryCallback;

        let (_dir, app) = mk_validator_app().await;

        // Bootstrap herder at ledger 1 → tracking slot = 2, is_tracking = true
        app.herder.bootstrap(1);
        assert!(app.herder.is_tracking());
        assert_eq!(app.herder.next_consensus_ledger_index().get(), 2);

        // Simulate losing sync — transitions herder to Syncing and cancels timers
        app.on_lost_sync();
        assert!(!app.herder.is_tracking());
        assert_eq!(app.herder.state(), henyey_herder::HerderState::Syncing);

        // Allow timer manager to process the cancel command
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }

        // Simulate LCL advancing via catchup while syncing (ledger 5 → next = 6)
        {
            let mut header = app.ledger_manager().current_header();
            header.ledger_seq = 5;
            app.ledger_manager()
                .set_header_for_test(header, henyey_common::Hash256::default());
        }
        assert_eq!(app.current_ledger_seq(), 5);

        // Now inject a stale ballot timer event from the old tracking epoch (slot 2).
        // This simulates a timer that was already in-flight in the event channel
        // when on_lost_sync cancelled timers.
        let stale_event = super::super::scp_timer_bridge::ScpTimerEvent {
            slot: 2,
            timer_type: henyey_herder::TimerType::Ballot,
            epoch: 0,
        };

        let fires_before = app.ballot_timeout_fires.load(Ordering::Relaxed);
        app.handle_scp_timer_event(stale_event).await;

        // The stale timer must NOT have fired — slot 2 < next_slot 6, so it's dropped.
        assert_eq!(
            app.ballot_timeout_fires.load(Ordering::Relaxed),
            fires_before,
            "stale ballot timer from prior tracking epoch must not fire after sync loss"
        );

        // Also verify a stale nomination timer for the old tracking slot is dropped.
        let stale_nom_event = super::super::scp_timer_bridge::ScpTimerEvent {
            slot: 2,
            timer_type: henyey_herder::TimerType::Nomination,
            epoch: 0,
        };

        let nom_fires_before = app.nomination_timeout_fires.load(Ordering::Relaxed);
        app.handle_scp_timer_event(stale_nom_event).await;

        assert_eq!(
            app.nomination_timeout_fires.load(Ordering::Relaxed),
            nom_fires_before,
            "stale nomination timer from prior tracking epoch must not fire after sync loss"
        );
    }

    /// Regression test: future-slot timers queued before sync loss must be
    /// dropped after the epoch advances, even when their slot >= next_slot.
    ///
    /// Scenario: the node is tracking at slot 2. A future-slot ballot timer
    /// for slot 5 fires and gets queued in scp_timer_rx (the channel). Before
    /// the main loop processes it, on_lost_sync() is called, incrementing the
    /// tracking epoch. When the event is finally dequeued, its epoch (0) does
    /// not match the current epoch (1), so it must be dropped — even though
    /// slot 5 >= next_slot and would otherwise hit the Fire branch.
    ///
    /// Without the epoch guard, this would call handle_ballot_timeout() /
    /// handle_nomination_timeout() against stale consensus state.
    #[tokio::test(start_paused = true)]
    async fn test_handle_scp_timer_event_future_slot_stale_epoch_drops() {
        use henyey_herder::sync_recovery::SyncRecoveryCallback;

        let (_dir, app) = mk_validator_app().await;

        // Bootstrap herder at ledger 1 → tracking slot = 2, is_tracking = true
        app.herder.bootstrap(1);
        assert!(app.herder.is_tracking());
        assert_eq!(app.herder.next_consensus_ledger_index().get(), 2);

        // The timer epoch starts at 0
        assert_eq!(
            app.scp_timer_epoch
                .load(std::sync::atomic::Ordering::Acquire),
            0
        );

        // Simulate losing sync — this increments the epoch to 1
        app.on_lost_sync();
        assert!(!app.herder.is_tracking());
        assert_eq!(
            app.scp_timer_epoch
                .load(std::sync::atomic::Ordering::Acquire),
            1
        );

        // Allow timer manager to process the cancel command
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }

        // Inject a FUTURE-slot ballot timer event from the OLD epoch (epoch 0).
        // This simulates a timer that was already queued in scp_timer_rx before
        // on_lost_sync() was called. Slot 5 >= next_slot (2), so without the
        // epoch guard it would hit the Fire branch.
        let stale_future_ballot = super::super::scp_timer_bridge::ScpTimerEvent {
            slot: 5,
            timer_type: henyey_herder::TimerType::Ballot,
            epoch: 0, // old epoch — before sync loss
        };

        let fires_before = app.ballot_timeout_fires.load(Ordering::Relaxed);
        app.handle_scp_timer_event(stale_future_ballot).await;

        assert_eq!(
            app.ballot_timeout_fires.load(Ordering::Relaxed),
            fires_before,
            "future-slot ballot timer from prior epoch must not fire after sync loss"
        );

        // Same for nomination timers — verify uniform rejection
        let stale_future_nom = super::super::scp_timer_bridge::ScpTimerEvent {
            slot: 5,
            timer_type: henyey_herder::TimerType::Nomination,
            epoch: 0, // old epoch
        };

        let nom_fires_before = app.nomination_timeout_fires.load(Ordering::Relaxed);
        app.handle_scp_timer_event(stale_future_nom).await;

        assert_eq!(
            app.nomination_timeout_fires.load(Ordering::Relaxed),
            nom_fires_before,
            "future-slot nomination timer from prior epoch must not fire after sync loss"
        );

        // Verify that a timer with the CURRENT epoch (1) IS processed normally.
        // This confirms the epoch guard is checking the right thing and not
        // blanket-rejecting all post-sync-loss timers.
        let current_epoch_event = super::super::scp_timer_bridge::ScpTimerEvent {
            slot: 5,
            timer_type: henyey_herder::TimerType::Ballot,
            epoch: 1, // current epoch — after sync loss
        };

        // This event passes the epoch check, passes can_receive_scp (Syncing allows it),
        // and hits the Fire branch (non-tracking, slot >= next_slot). It will actually
        // call handle_ballot_timeout — confirming the epoch guard is selective.
        let fires_before_current = app.ballot_timeout_fires.load(Ordering::Relaxed);
        app.handle_scp_timer_event(current_epoch_event).await;
        assert_eq!(
            app.ballot_timeout_fires.load(Ordering::Relaxed),
            fires_before_current + 1,
            "event with current epoch must pass the epoch guard and fire"
        );
    }

    /// Regression test for #2803 (PR #2821 final bounce, Parity comment 30):
    /// the future-slot 1-second defer loop must NOT "rebirth" a pre-sync-loss
    /// timer into the new epoch.
    ///
    /// Scenario: tracking at slot 2. A future-slot nomination timer (slot 5,
    /// epoch 0) fires and is classified `Rearm`, re-scheduling for 1 second.
    /// Before the re-armed timer fires, `on_lost_sync()` bumps the tracking
    /// epoch to 1. When the re-armed timer finally fires and is re-delivered
    /// through the production TimerManager → ScpTimerBridge path, it must carry
    /// the ORIGINATING epoch (0), not the bumped epoch (1) — so the epoch guard
    /// in `handle_scp_timer_event()` rejects it instead of firing it into the
    /// timeout handler while the node is Syncing.
    ///
    /// stellar-core's in-process `timerCallbackWrapper`/`setupTimer` defer loop
    /// cannot rebirth a pre-sync-loss timer this way; this test pins henyey's
    /// split-architecture equivalent.
    #[tokio::test(start_paused = true)]
    async fn test_handle_scp_timer_event_e2e_rearm_preserves_epoch_across_sync_loss() {
        use henyey_herder::sync_recovery::SyncRecoveryCallback;

        let (_dir, app) = mk_validator_app().await;

        // Bootstrap at ledger 1 → tracking slot = 2, is_tracking = true.
        app.herder.bootstrap(1);
        assert!(app.herder.is_tracking());
        assert_eq!(app.herder.next_consensus_ledger_index().get(), 2);
        assert_eq!(app.scp_timer_epoch.load(Ordering::Acquire), 0);

        // Future-slot nomination event from the current (epoch 0) tracking
        // epoch — classified Rearm and re-scheduled for 1 second carrying
        // epoch 0.
        let future_slot = 5u64;
        let event = super::super::scp_timer_bridge::ScpTimerEvent {
            slot: future_slot,
            timer_type: henyey_herder::TimerType::Nomination,
            epoch: 0,
        };
        app.handle_scp_timer_event(event).await;

        // Let the timer manager process the re-arm command.
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }

        // Lose sync mid-defer: bump the global epoch to 1 (and cancel timers).
        // The re-arm command was already processed above, so the re-armed timer
        // is in flight with its originating epoch 0.
        app.on_lost_sync();
        assert!(!app.herder.is_tracking());
        assert_eq!(app.scp_timer_epoch.load(Ordering::Acquire), 1);
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }

        // Note: on_lost_sync() also cancels all timers, but to prove the
        // epoch-preservation property independently of cancellation ordering we
        // re-arm explicitly with the originating epoch (this is exactly what the
        // Rearm branch does) and then advance time. Whichever wins the race, the
        // re-delivered event must carry epoch 0.
        app.timer_manager_handle
            .reschedule_nomination_timeout_with_epoch(
                future_slot,
                std::time::Duration::from_secs(1),
                0,
            )
            .await;
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }

        // Advance past the 1-second defer; the re-armed timer fires and is
        // re-delivered through the production bridge.
        let redelivered = {
            let mut rx = app.scp_timer_rx.lock().await;
            tokio::time::advance(std::time::Duration::from_millis(1001)).await;
            for _ in 0..10 {
                tokio::task::yield_now().await;
            }
            rx.try_recv()
                .expect("expected re-delivered nomination timer event")
        };

        // The re-delivered event MUST carry the originating epoch (0), not the
        // bumped global epoch (1). This is the core of the fix.
        assert_eq!(
            redelivered.epoch, 0,
            "re-armed timer must carry the originating epoch across sync loss, not be reborn into the new epoch"
        );
        assert_eq!(redelivered.slot, future_slot);

        // Feeding the re-delivered event back into the handler must DROP it via
        // the epoch guard (event.epoch 0 != current_epoch 1) — no fire, no SCP
        // state mutation while Syncing.
        let nom_fires_before = app.nomination_timeout_fires.load(Ordering::Relaxed);
        app.handle_scp_timer_event(redelivered).await;
        assert_eq!(
            app.nomination_timeout_fires.load(Ordering::Relaxed),
            nom_fires_before,
            "re-delivered timer from the prior epoch must be dropped after sync loss, not fired"
        );
    }

    // ── Event-driven consensus trigger (#2702) ──────────────────────────

    /// #2702: once tracking, `setup_trigger_next_ledger` arms a single-shot
    /// `TriggerNextLedger` timer that fires back through the bridge as a
    /// `TriggerNextLedger` event. Drives the primary event-driven nomination
    /// schedule (replacing the 1-second poll as the primary trigger).
    #[tokio::test(start_paused = true)]
    async fn test_setup_trigger_next_ledger_arms_timer_when_tracking() {
        let (_dir, app) = mk_validator_app().await;
        app.herder.bootstrap(1);
        assert!(app.herder.is_tracking());

        app.setup_trigger_next_ledger().await;
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }

        // prepare_start is None on cold start, so the delay is just the
        // ctValidityOffset (0 here) → fires effectively immediately.
        let mut rx = app.scp_timer_rx.lock().await;
        tokio::time::advance(std::time::Duration::from_secs(2)).await;
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        let delivered = rx
            .try_recv()
            .expect("expected a TriggerNextLedger timer event to be delivered");
        assert_eq!(
            delivered.timer_type,
            henyey_herder::TimerType::TriggerNextLedger
        );
        // Slot is current_ledger + 1 (LCL 1 → slot 2).
        assert_eq!(delivered.slot, 2);
    }

    /// #2702: `setup_trigger_next_ledger` is a no-op before the herder is
    /// tracking (cold start before bootstrap) — mirrors stellar-core arming
    /// only from `lastClosedLedgerIncreased` while in-sync.
    #[tokio::test(start_paused = true)]
    async fn test_setup_trigger_next_ledger_skips_when_not_tracking() {
        let (_dir, app) = mk_validator_app().await;
        // No bootstrap → not tracking.
        assert!(!app.herder.is_tracking());

        app.setup_trigger_next_ledger().await;
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        let mut rx = app.scp_timer_rx.lock().await;
        tokio::time::advance(std::time::Duration::from_secs(5)).await;
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        assert!(
            rx.try_recv().is_err(),
            "no trigger timer should be armed while not tracking"
        );
    }

    /// #2702: handling a fired `TriggerNextLedger` event increments the
    /// `consensus_trigger_timer_fires` counter and returns `true` to signal the
    /// caller to pump the close pipeline (the cold-start self-externalize fix).
    #[tokio::test(start_paused = true)]
    async fn test_trigger_next_ledger_event_fires_counter_and_signals_pump() {
        let (_dir, app) = mk_validator_app().await;
        app.herder.bootstrap(1);

        let fires_before = app.consensus_trigger_timer_fires.load(Ordering::Relaxed);
        let event = super::super::scp_timer_bridge::ScpTimerEvent {
            slot: 2,
            timer_type: henyey_herder::TimerType::TriggerNextLedger,
            epoch: 0,
        };
        let pump = app.handle_scp_timer_event(event).await;

        assert!(
            pump,
            "a fired TriggerNextLedger event must signal the caller to pump the close pipeline"
        );
        assert_eq!(
            app.consensus_trigger_timer_fires.load(Ordering::Relaxed),
            fires_before + 1,
            "consensus_trigger_timer_fires must increment on a fired trigger event"
        );
    }

    /// #2702: an SCP nomination/ballot timeout event must NOT signal a
    /// close-pipeline pump (only the trigger event does).
    #[tokio::test(start_paused = true)]
    async fn test_scp_timeout_event_does_not_signal_pump() {
        let (_dir, app) = mk_validator_app().await;
        app.herder.bootstrap(1);
        let next_slot = app.herder.next_consensus_ledger_index().get();

        let event = super::super::scp_timer_bridge::ScpTimerEvent {
            slot: next_slot,
            timer_type: henyey_herder::TimerType::Nomination,
            epoch: 0,
        };
        let pump = app.handle_scp_timer_event(event).await;
        assert!(
            !pump,
            "SCP nomination timeout must not signal a close-pipeline pump"
        );
    }

    /// #3014: the post-close path must be **arm-only** — it arms the
    /// `TriggerNextLedger` timer but must NOT call `try_trigger_consensus`
    /// synchronously. This mirrors stellar-core's `lastClosedLedgerIncreased`
    /// (HerderImpl.cpp:1218-1233), which calls only `setupTriggerNextLedger`;
    /// the immediate fire happens via the timer's `triggerTime < now` clamp.
    ///
    /// Discrimination: the test bootstraps the steady-state regime where
    /// `prepare_start(last_slot)` is `None` (cold start), so the trigger-time
    /// gate inside `try_trigger_consensus` does NOT block. This is the only
    /// regime that distinguishes pre/post: a fast/solo close already self-gates
    /// on `origin/main` and would prove nothing.
    ///
    /// - Pre-fix (`origin/main`): the post-close sequence ran
    ///   `try_trigger_consensus().await` immediately, so `consensus_trigger_attempts`
    ///   incremented synchronously — this assertion FAILS.
    /// - Post-fix: the post-close sequence is arm-only, so the counter does NOT
    ///   move until the armed timer fires and is handled — this assertion PASSES.
    #[tokio::test(start_paused = true)]
    async fn test_post_close_arms_trigger_without_immediate_nomination() {
        let (_dir, app) = mk_validator_app().await;

        // Bootstrap at ledger 1 → tracking, LCL 1, next slot 2. prepare_start(1)
        // is None on cold start, so the trigger-time gate in
        // try_trigger_consensus does NOT block — the steady-state regime that
        // discriminates pre/post.
        app.herder.bootstrap(1);
        assert!(app.herder.is_tracking());
        assert!(
            app.herder.prepare_start(1).is_none(),
            "test precondition: prepare_start(LCL) must be None so the \
             trigger-time gate does not block (the only regime that \
             discriminates pre/post)"
        );

        let attempts_before = app.consensus_trigger_attempts.load(Ordering::Relaxed);

        // Run the exact post-close subsequence the production sites run.
        app.post_close_trigger_and_arm().await;

        // (a) The post-close arm must NOT have triggered nomination
        // synchronously. FAILS on origin/main (immediate try_trigger_consensus
        // bumps the counter before any timer fires); PASSES post-fix.
        assert_eq!(
            app.consensus_trigger_attempts.load(Ordering::Relaxed),
            attempts_before,
            "post-close must be arm-only: try_trigger_consensus must NOT run \
             synchronously (parity: lastClosedLedgerIncreased is arm-only)"
        );

        // (b) A TriggerNextLedger timer was armed for slot == current_ledger + 1.
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        let mut rx = app.scp_timer_rx.lock().await;
        // prepare_start is None → delay is only ctValidityOffset (0) → fires
        // effectively immediately under paused time.
        tokio::time::advance(std::time::Duration::from_secs(2)).await;
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        let delivered = rx
            .try_recv()
            .expect("expected a TriggerNextLedger timer event to be delivered");
        assert_eq!(
            delivered.timer_type,
            henyey_herder::TimerType::TriggerNextLedger
        );
        assert_eq!(
            delivered.slot, 2,
            "armed trigger timer must target current_ledger + 1"
        );
        drop(rx);

        // (c) Handling the fired TriggerNextLedger event triggers nomination
        // (attempts increments) and signals the close-pipeline pump — proving
        // the trigger moved from immediate to timer-driven without losing
        // close→nominate liveness.
        let pump = app.handle_scp_timer_event(delivered).await;
        assert!(
            pump,
            "a fired TriggerNextLedger event must signal the caller to pump the close pipeline"
        );
        assert_eq!(
            app.consensus_trigger_attempts.load(Ordering::Relaxed),
            attempts_before + 1,
            "handling the armed TriggerNextLedger event must trigger nomination \
             (timer-driven, not immediate)"
        );
    }
}
