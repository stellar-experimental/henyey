//! Fine-grained sub-phase constants for event-loop watchdog attribution.
//!
//! See `App::event_loop_phase_sub`. These constants are stamped by
//! [`App::set_phase_sub`](super::App::set_phase_sub) immediately before
//! each notable `.await` on a coarse phase whose callees may block. The
//! WATCHDOG thread (in `App::start_event_loop_watchdog`) prints both
//! `phase` and `phase_sub` in its warn/error lines, so the next freeze
//! heartbeat names the exact `.await`.
//!
//! ## Phase 6 (`pending_close`)
//!
//! Issue #1921 context: the coarse `phase=6` covers the entire
//! post-close window — from `handle_close_complete_inner` (ledger_close.rs)
//! through `maybe_publish_history`, `try_trigger_consensus`, SCP/fetch
//! drain loops, and `process_externalized_slots` (lifecycle.rs:261-370).
//! The `PHASE_6_*` constants below provide coarse attribution within
//! phase 6, narrowing a stall to one of 12 sub-regions.
//!
//! ## Phase 13 (`buffered_catchup`)
//!
//! Issue #1788 context: the coarse `phase=13 buffered_catchup` label
//! covers everything from
//! `process_externalized_slots` (ledger_close.rs) stamping `set_phase(13)`
//! up to `spawn_catchup` setting `phase=14` *inside* its spawned task —
//! a large transitive subtree including `maybe_start_buffered_catchup`,
//! `out_of_sync_recovery`, `trigger_recovery_catchup`, and the
//! synchronous prefix of `spawn_catchup`. The `PHASE_13_*` constants
//! below disambiguate which `.await` is currently parked.
//!
//! Convention: constants are one-based dense integers within a coarse
//! phase. Zero means "coarse phase entered, sub-phase not yet set".

// ── Phase 6: pending_close (issue #1921) ─────────────────────────────

/// `handle_close_complete_inner`: about to acquire
/// `syncing_ledgers.write().await` on hash-mismatch error path.
pub(crate) const PHASE_6_1_SYNCING_LEDGERS_HASH_MISMATCH: u32 = 1;

/// `handle_close_complete_inner`: about to call
/// `writer.write_meta(...).await` for metadata emission.
pub(crate) const PHASE_6_2_WRITE_META: u32 = 2;

/// `handle_close_complete_inner`: about to call
/// `overlay().await` + `clear_ledgers_below()`.
pub(crate) const PHASE_6_3_OVERLAY_CLEAR_LEDGERS: u32 = 3;

/// `handle_close_complete_inner`: about to call
/// `overlay().await` + `handle_max_tx_size_increase().await` (two awaits).
pub(crate) const PHASE_6_4_OVERLAY_MAX_TX_SIZE: u32 = 4;

/// `handle_close_complete_inner`: about to acquire
/// `survey_limiter.write().await` for clearing old ledgers.
pub(crate) const PHASE_6_5_SURVEY_LIMITER_WRITE: u32 = 5;

/// `handle_close_complete_inner`: about to call
/// `join.await` on `spawn_blocking` tx-queue work.
pub(crate) const PHASE_6_6_TX_QUEUE_JOIN: u32 = 6;

/// `handle_close_complete_inner`: about to acquire
/// `last_processed_slot.write().await`.
pub(crate) const PHASE_6_7_LAST_PROCESSED_SLOT_WRITE: u32 = 7;

/// `handle_close_complete_inner`: about to call
/// `clear_tx_advert_history(...)` (internally acquires 4 write locks:
/// `tx_adverts_by_peer`, `tx_demand_history`, `tx_set_dont_have`,
/// `tx_set_last_request`).
pub(crate) const PHASE_6_8_CLEAR_TX_ADVERT_HISTORY: u32 = 8;

/// `lifecycle.rs` post-close: about to call `maybe_publish_history().await`.
pub(crate) const PHASE_6_9_MAYBE_PUBLISH_HISTORY: u32 = 9;

/// `lifecycle.rs` post-close: about to call `try_trigger_consensus().await`.
pub(crate) const PHASE_6_10_TRY_TRIGGER_CONSENSUS: u32 = 10;

/// `lifecycle.rs` post-close: about to drain fetch response channel.
/// Each iteration calls `handle_overlay_message` which may have
/// internal awaits.
pub(crate) const PHASE_6_11_FETCH_DRAIN: u32 = 11;

/// `lifecycle.rs` post-close: about to call
/// `process_externalized_slots().await`. Only covers the pre-transition
/// prefix; the method overrides coarse phase to 11/13 internally.
pub(crate) const PHASE_6_12_PROCESS_EXTERNALIZED_SLOTS: u32 = 12;

/// Test helper: return the highest phase-6 sub-phase constant.
#[cfg(test)]
pub(crate) const fn max_defined_phase_6_sub_phase() -> u32 {
    PHASE_6_12_PROCESS_EXTERNALIZED_SLOTS
}

// ── Phase 13: buffered_catchup (issue #1788) ─────────────────────────

/// `maybe_start_buffered_catchup`: about to acquire
/// `syncing_ledgers.write().await` for trim/evict (catchup_impl.rs).
pub(crate) const PHASE_13_1_BUFFERED_SYNCING_LEDGERS_WRITE: u32 = 1;

/// `maybe_start_buffered_catchup`: about to acquire
/// `syncing_ledgers.read().await` for sequential-tx-set check.
pub(crate) const PHASE_13_2_BUFFERED_SYNCING_LEDGERS_READ: u32 = 2;

/// `maybe_start_buffered_catchup`: about to snapshot
/// `consensus_stuck_state.read().await` in the stuck-state arm.
pub(crate) const PHASE_13_3_BUFFERED_CONSENSUS_STUCK_WRITE: u32 = 3;

/// `maybe_start_buffered_catchup`: about to read
/// `last_catchup_completed_at.read().await` for the post-catchup cooldown.
pub(crate) const PHASE_13_4_BUFFERED_LAST_CATCHUP_COMPLETED_READ: u32 = 4;

/// `maybe_start_buffered_catchup`: about to read
/// `archive_behind_until.read().await` for archive-behind check.
pub(crate) const PHASE_13_5_BUFFERED_ARCHIVE_BEHIND_READ: u32 = 5;

/// `out_of_sync_recovery`: about to acquire
/// `syncing_ledgers.read().await` for buffer count telemetry.
pub(crate) const PHASE_13_6_OUT_OF_SYNC_BUFFER_COUNT_READ: u32 = 6;

/// `out_of_sync_recovery`: about to acquire
/// `syncing_ledgers.write().await` to evict closed slots
/// (essentially-caught-up branch).
pub(crate) const PHASE_13_7_OUT_OF_SYNC_CLEAR_SYNCING_WRITE: u32 = 7;

/// `out_of_sync_recovery`: inside `analyze_externalized_gaps`, about
/// to either acquire `syncing_ledgers.read/write` or fall through to
/// SCP broadcast.
pub(crate) const PHASE_13_8_OUT_OF_SYNC_ANALYZE_GAPS: u32 = 8;

/// `out_of_sync_recovery`: about to call `broadcast_recovery_scp_state`
/// (which acquires `overlay().await` before spawning).
pub(crate) const PHASE_13_9_BROADCAST_RECOVERY: u32 = 9;

/// `out_of_sync_recovery`: escalation path — about to call
/// `trigger_recovery_catchup`.
pub(crate) const PHASE_13_10_TRIGGER_RECOVERY_CATCHUP: u32 = 10;

/// `spawn_catchup`: about to `set_state(CatchingUp).await` (acquires
/// `App::state` write lock).
pub(crate) const PHASE_13_11_SPAWN_CATCHUP_SET_STATE: u32 = 11;

/// `spawn_catchup`: about to
/// `start_catchup_message_caching_from_self().await` (transitively
/// acquires `self_arc.read().await` + `overlay().await`).
pub(crate) const PHASE_13_12_SPAWN_CATCHUP_MSG_CACHE: u32 = 12;

/// `spawn_catchup`: about to `self_arc.read().await` to upgrade the
/// weak reference for the spawned task.
pub(crate) const PHASE_13_13_SPAWN_CATCHUP_SELF_ARC_READ: u32 = 13;

/// `compute_target_and_spawn_buffered_catchup`: about to call
/// `validate_target_checkpoint_published` (transitively reads the
/// archive-checkpoint cache and may arm the backoff).
pub(crate) const PHASE_13_14_VALIDATE_TARGET_CHECKPOINT: u32 = 14;

/// `compute_target_and_spawn_buffered_catchup`: about to call
/// `validate_archive_has_newer_checkpoint`.
pub(crate) const PHASE_13_15_VALIDATE_ARCHIVE_NEWER: u32 = 15;

/// Test helper: return the highest phase-13 sub-phase constant in use.
/// New constants added above must increase this.
#[cfg(test)]
pub(crate) const fn max_defined_phase_13_sub_phase() -> u32 {
    PHASE_13_15_VALIDATE_ARCHIVE_NEWER
}
