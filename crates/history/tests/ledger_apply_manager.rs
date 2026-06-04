//! Integration tests for the pure §7 Ledger Apply Manager decision logic.
//!
//! These exercise the spec CATCHUP_SPEC §7.2/§7.3/§7.4 pure functions extracted
//! into `henyey_history::ledger_apply_manager`. They mirror the parity-relevant
//! behavior of stellar-core `LedgerApplyManagerImpl.{h,cpp}` without the
//! async/lock machinery (which stays App-owned).

use std::collections::BTreeMap;

use henyey_history::ledger_apply_manager::{
    apply_drift_exceeded, classify_process_ledger, trim_syncing_buffer, ProcessLedgerDecision,
    MAX_EXTERNALIZE_LEDGER_APPLY_DRIFT,
};

/// §7.3 drift stop-condition: parity with `LedgerApplyManagerImpl.cpp:516`
/// (`nextToApply - lcl >= MAX_EXTERNALIZE_LEDGER_APPLY_DRIFT`). The predicate
/// is parameterized on `next_to_apply` (= last-queued + 1) to be byte-identical
/// to core. False for drift 0..=11, true at 12 and above. Pins the constant.
#[test]
fn test_apply_drift_exceeded_threshold() {
    assert_eq!(MAX_EXTERNALIZE_LEDGER_APPLY_DRIFT, 12);

    let lcl = 1_000u32;
    // next_to_apply - lcl in 0..=11 → not exceeded.
    for drift in 0..=11u32 {
        let next_to_apply = lcl + drift;
        assert!(
            !apply_drift_exceeded(next_to_apply, lcl),
            "drift {drift} (next_to_apply={next_to_apply}) must NOT exceed threshold"
        );
    }
    // 12 and above → exceeded.
    for drift in 12..=20u32 {
        let next_to_apply = lcl + drift;
        assert!(
            apply_drift_exceeded(next_to_apply, lcl),
            "drift {drift} (next_to_apply={next_to_apply}) MUST exceed threshold"
        );
    }
}

/// `apply_drift_exceeded` must not panic when `next_to_apply < lcl`
/// (saturating_sub yields 0, well under the threshold). Core relies on
/// `releaseAssert(mLastQueuedToApply >= lcl)`; the Rust port saturates instead
/// of asserting, which holds trivially under inline apply.
#[test]
fn test_apply_drift_exceeded_saturates_below_lcl() {
    assert!(!apply_drift_exceeded(5, 100));
}

/// §7.4 trim: when `last_buffered` is the first ledger of a checkpoint, retain
/// that ledger plus the entire prior checkpoint; the §7.1(c) ≤ 65-entry
/// invariant must hold. When `last_buffered` is mid-checkpoint, retain only
/// entries `>= firstLedgerInCheckpointContaining(last_buffered)`.
#[test]
fn test_trim_retains_last_checkpoint_plus_boundary() {
    // Mid-checkpoint last_buffered (default freq 64): last = 200 lives in the
    // checkpoint [192, 255]; retain only entries >= 192.
    let mut buf: BTreeMap<u32, ()> = (100u32..=200).map(|s| (s, ())).collect();
    trim_syncing_buffer(&mut buf, /* last_queued_to_apply */ 99);
    assert_eq!(*buf.keys().next().unwrap(), 192);
    assert_eq!(*buf.keys().next_back().unwrap(), 200);

    // last_buffered at a checkpoint start (192): retain 192 + the entire prior
    // checkpoint [128, 191]. First retained key is 128; ≤ 65 entries.
    let mut buf2: BTreeMap<u32, ()> = (10u32..=192).map(|s| (s, ())).collect();
    trim_syncing_buffer(&mut buf2, /* last_queued_to_apply */ 9);
    assert_eq!(*buf2.keys().next().unwrap(), 128);
    assert_eq!(*buf2.keys().next_back().unwrap(), 192);
    assert!(
        buf2.len() <= (henyey_history::checkpoint_frequency() + 1) as usize,
        "§7.1(c): at most CHECKPOINT_FREQUENCY+1 entries"
    );
}

/// §7.4 step 1: stale ledgers (`< last_queued_to_apply + 1`) are always removed.
/// After stale removal the §7.4 checkpoint-boundary trim still applies, so for a
/// mid-checkpoint `last_buffered = 70` (checkpoint_start = 64) the surviving
/// floor is the larger of the stale floor (61) and the checkpoint floor (64).
#[test]
fn test_trim_removes_stale() {
    // Stale floor 61 is below the checkpoint floor 64 → checkpoint floor wins.
    let mut buf: BTreeMap<u32, ()> = (50u32..=70).map(|s| (s, ())).collect();
    trim_syncing_buffer(&mut buf, /* last_queued_to_apply */ 60);
    assert_eq!(*buf.keys().next().unwrap(), 64);

    // Stale floor 100 is above the checkpoint floor (64) → stale removal alone
    // governs; entry 99 and below are gone, last_buffered 120 keeps from 100.
    let mut buf2: BTreeMap<u32, ()> = (90u32..=120).map(|s| (s, ())).collect();
    trim_syncing_buffer(&mut buf2, /* last_queued_to_apply */ 99);
    assert_eq!(*buf2.keys().next().unwrap(), 100);
}

/// §7.2 step 3: `S <= last_queued_to_apply` ⇒ the ledger is silently dropped.
#[test]
fn test_process_ledger_decision_skip_stale() {
    assert_eq!(
        classify_process_ledger(/* s */ 100, /* last_queued_to_apply */ 100, false),
        ProcessLedgerDecision::SkipStale
    );
    assert_eq!(
        classify_process_ledger(/* s */ 99, /* last_queued_to_apply */ 100, false),
        ProcessLedgerDecision::SkipStale
    );
}

/// §7.2 step 5: `S == last_queued_to_apply + 1` and no catchup running ⇒
/// sequential apply.
#[test]
fn test_process_ledger_decision_sequential() {
    assert_eq!(
        classify_process_ledger(/* s */ 101, /* last_queued_to_apply */ 100, false),
        ProcessLedgerDecision::SequentialApply
    );
}

/// §7.2: a ledger beyond `last_queued_to_apply + 1` (gap) buffers and waits;
/// likewise any new ledger while catchup is running buffers and waits.
#[test]
fn test_process_ledger_decision_buffer_and_wait() {
    assert_eq!(
        classify_process_ledger(/* s */ 105, /* last_queued_to_apply */ 100, false),
        ProcessLedgerDecision::BufferAndWait
    );
    // Catchup running: even a contiguous ledger is buffered, not applied.
    assert_eq!(
        classify_process_ledger(/* s */ 101, /* last_queued_to_apply */ 100, true),
        ProcessLedgerDecision::BufferAndWait
    );
}
