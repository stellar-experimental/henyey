//! Pure §7 Ledger Apply Manager decision logic.
//!
//! Spec: `stellar-specs/CATCHUP_SPEC.md` §7 ("Ledger Apply Manager"). Parity:
//! `stellar-core/src/catchup/LedgerApplyManagerImpl.{h,cpp}`.
//!
//! In henyey the §7 Ledger Apply Manager is collapsed into `App` (which owns the
//! `RwLock<BTreeMap>` syncing-ledger buffer, the herder, and the async ledger
//! close). This module extracts only the **pure decision logic** — the drift
//! threshold constant, the §7.3 drift stop-predicate, the §7.4 trim, and the
//! §7.2 process-ledger classifier — so that the normative §7 architecture is
//! visible from `crates/history` and the spec threshold is real and enforced.
//! The async/lock machinery stays correctly App-owned.
//!
//! ## Faithful-but-dormant drift gate
//!
//! Henyey applies ledgers **inline**: there is no `lastQueuedToApply` distinct
//! from the last-closed ledger (LCL) and no parallel-close queue. Under inline
//! apply `next_to_apply - lcl == 1` always, so [`apply_drift_exceeded`] never
//! fires — exactly as in stellar-core, where the check only fires under
//! parallel-close queue depth. We deliberately do NOT repurpose the threshold
//! to gate the catchup trigger (stellar-core's drift check gates ONLY
//! sequential-apply scheduling, never the catchup trigger — that is §7.2
//! step-8's distinct "first ledger of checkpoint" rule).

use std::collections::BTreeMap;

use crate::checkpoint::{checkpoint_frequency, checkpoint_start, is_checkpoint_start};

/// Maximum allowed drift, in ledgers, between the next ledger queued for
/// sequential apply and the last-closed ledger (LCL) before the node stops
/// scheduling sequential applies and lets itself fall into catchup.
///
/// Parity: `LedgerApplyManagerImpl.cpp:28` (`= 12`). Spec: CATCHUP_SPEC §7.3.
pub const MAX_EXTERNALIZE_LEDGER_APPLY_DRIFT: u32 = 12;

/// §7.3 sequential-apply drift stop-condition.
///
/// Returns `true` when the drift between the next ledger to apply and the
/// last-closed ledger has reached [`MAX_EXTERNALIZE_LEDGER_APPLY_DRIFT`], at
/// which point sequential-apply scheduling must stop so the node can gracefully
/// transition to catchup.
///
/// `next_to_apply` is the sequence of the ledger about to be scheduled — i.e.
/// `last_queued_to_apply + 1` — to be byte-identical to the stellar-core check
/// `nextToApply - lcl >= MAX_EXTERNALIZE_LEDGER_APPLY_DRIFT`
/// (`LedgerApplyManagerImpl.cpp:516`, with `nextToApply = *mLastQueuedToApply + 1`).
///
/// `saturating_sub` is used so the port cannot panic where core relies on
/// `releaseAssert(mLastQueuedToApply >= lcl)` (`.cpp:515`); under inline apply
/// `next_to_apply == lcl + 1 > lcl`, so the assertion holds trivially.
///
/// # Examples
///
/// ```
/// use henyey_history::ledger_apply_manager::{apply_drift_exceeded, MAX_EXTERNALIZE_LEDGER_APPLY_DRIFT};
///
/// // Inline apply: next_to_apply is always lcl + 1 → never fires.
/// assert!(!apply_drift_exceeded(101, 100));
/// // At the threshold (drift == 12) → fires.
/// assert!(apply_drift_exceeded(100 + MAX_EXTERNALIZE_LEDGER_APPLY_DRIFT, 100));
/// ```
#[inline]
pub fn apply_drift_exceeded(next_to_apply: u32, lcl: u32) -> bool {
    next_to_apply.saturating_sub(lcl) >= MAX_EXTERNALIZE_LEDGER_APPLY_DRIFT
}

/// The §7.2 process-ledger decision for a newly received externalized ledger.
///
/// This captures the spec's decision tree (§7.2 steps 3/5/6) as a pure
/// classification over the relevant scalar inputs, independent of the buffer
/// storage and lock machinery (which stay App-owned).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessLedgerDecision {
    /// §7.2 step 3 — `S <= last_queued_to_apply`: the ledger is stale and is
    /// silently dropped.
    SkipStale,
    /// §7.2 step 5 — `S == last_queued_to_apply + 1` and no catchup running:
    /// the ledger is contiguous with the apply stream and is applied
    /// sequentially (via §7.3).
    SequentialApply,
    /// §7.2 step 6/8 — the ledger is buffered and the node waits (either a gap
    /// exists, or a catchup is currently running so even a contiguous ledger is
    /// buffered rather than applied).
    BufferAndWait,
}

/// Classify a newly received externalized ledger with sequence `s` per §7.2.
///
/// `last_queued_to_apply` is the highest ledger sequence already queued for
/// apply (in henyey's inline model, the last-closed ledger). `catchup_running`
/// is whether a `CatchupWork` is currently in flight.
///
/// Parity: mirrors the early branches of
/// `LedgerApplyManagerImpl::processLedger` — skip-stale (`S <= lastQueued`),
/// sequential (`S == lastQueued + 1` with no catchup), otherwise buffer-and-wait.
///
/// # Examples
///
/// ```
/// use henyey_history::ledger_apply_manager::{classify_process_ledger, ProcessLedgerDecision};
///
/// assert_eq!(classify_process_ledger(100, 100, false), ProcessLedgerDecision::SkipStale);
/// assert_eq!(classify_process_ledger(101, 100, false), ProcessLedgerDecision::SequentialApply);
/// assert_eq!(classify_process_ledger(105, 100, false), ProcessLedgerDecision::BufferAndWait);
/// assert_eq!(classify_process_ledger(101, 100, true), ProcessLedgerDecision::BufferAndWait);
/// ```
#[inline]
pub fn classify_process_ledger(
    s: u32,
    last_queued_to_apply: u32,
    catchup_running: bool,
) -> ProcessLedgerDecision {
    if s <= last_queued_to_apply {
        ProcessLedgerDecision::SkipStale
    } else if !catchup_running && s == last_queued_to_apply + 1 {
        ProcessLedgerDecision::SequentialApply
    } else {
        ProcessLedgerDecision::BufferAndWait
    }
}

/// §7.4 buffer trimming — discard ledgers that cannot contribute to a future
/// catchup operation.
///
/// Operating on a generic `BTreeMap<u32, V>` keyed by ledger sequence (so it is
/// independent of the `LedgerCloseInfo` value type, which lives in the herder
/// crate), this performs the spec's trim:
///
/// 1. Remove all entries with `ledgerSeq < last_queued_to_apply + 1` (stale).
/// 2. Let `last_buffered` be the largest remaining key. If it is the first
///    ledger of a checkpoint, retain only `last_buffered` plus the entire prior
///    checkpoint (i.e. entries `>= checkpoint_start(last_buffered - 1)`).
/// 3. Otherwise retain only entries with sequence
///    `>= firstLedgerInCheckpointContaining(last_buffered)`.
///
/// The result satisfies the §7.1(c) invariant: at most `CHECKPOINT_FREQUENCY+1`
/// entries spanning one full checkpoint plus the following checkpoint's first
/// ledger.
///
/// Parity: `LedgerApplyManagerImpl::trimSyncingLedgers`.
pub fn trim_syncing_buffer<V>(buffer: &mut BTreeMap<u32, V>, last_queued_to_apply: u32) {
    // Step 1: drop stale entries (already queued / applied).
    let min_keep = last_queued_to_apply.saturating_add(1);
    buffer.retain(|seq, _| *seq >= min_keep);
    if buffer.is_empty() {
        return;
    }

    // Steps 2/3: trim to the checkpoint boundary implied by the last buffered
    // ledger.
    let last_buffered = *buffer
        .keys()
        .next_back()
        .expect("buffer checked non-empty above");
    let trim_before = match trim_boundary_for_last_buffered(last_buffered) {
        Some(b) => b,
        None => return,
    };
    buffer.retain(|seq, _| *seq >= trim_before);
}

/// Compute the §7.4 trim boundary for the given `last_buffered` slot.
///
/// Returns `None` when no checkpoint-boundary trim should occur (the degenerate
/// `last_buffered <= 1` checkpoint-start case). Otherwise returns
/// `Some(trim_before)` — entries with sequence `< trim_before` should be
/// removed.
///
/// - If `last_buffered` is the first ledger of a checkpoint, its checkpoint has
///   not yet been published, so retain it plus the entire prior checkpoint:
///   boundary is `checkpoint_start(last_buffered - 1)`.
/// - Otherwise its checkpoint has begun publishing, so retain only
///   `>= checkpoint_start(last_buffered)`.
///
/// Parity: the boundary arithmetic in `trimSyncingLedgers`.
pub fn trim_boundary_for_last_buffered(last_buffered: u32) -> Option<u32> {
    if is_checkpoint_start(last_buffered) {
        if last_buffered <= 1 {
            None
        } else {
            Some(checkpoint_start(last_buffered - 1))
        }
    } else {
        Some(checkpoint_start(last_buffered))
    }
}

/// The §7.1(c) buffer-size invariant bound: at most one full checkpoint plus the
/// following checkpoint's first ledger.
#[inline]
pub fn max_buffer_invariant_entries() -> usize {
    (checkpoint_frequency() + 1) as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drift_predicate_pins_constant() {
        assert_eq!(MAX_EXTERNALIZE_LEDGER_APPLY_DRIFT, 12);
        for drift in 0..=11u32 {
            assert!(!apply_drift_exceeded(1000 + drift, 1000));
        }
        for drift in 12..=24u32 {
            assert!(apply_drift_exceeded(1000 + drift, 1000));
        }
    }

    #[test]
    fn drift_predicate_saturates() {
        assert!(!apply_drift_exceeded(0, 1000));
    }

    #[test]
    fn trim_boundary_checkpoint_start_retains_prior_checkpoint() {
        // freq 64: last_buffered = 192 (checkpoint start) → retain from 128.
        assert_eq!(trim_boundary_for_last_buffered(192), Some(128));
        // mid-checkpoint last_buffered = 200 → retain from 192.
        assert_eq!(trim_boundary_for_last_buffered(200), Some(192));
        // degenerate ledger 1 → no trim.
        assert_eq!(trim_boundary_for_last_buffered(1), None);
    }

    #[test]
    fn trim_buffer_respects_invariant() {
        let mut buf: BTreeMap<u32, ()> = (10u32..=192).map(|s| (s, ())).collect();
        trim_syncing_buffer(&mut buf, 9);
        assert!(buf.len() <= max_buffer_invariant_entries());
        assert_eq!(*buf.keys().next().unwrap(), 128);
    }
}
