//! Rate-limiting helpers for repeated log messages during sync recovery.
//!
//! These types throttle log output without changing any recovery semantics.
//! All diagnostic information is preserved at `debug!` level.

use std::sync::atomic::{AtomicU64, Ordering};

/// Allows one `true` return per distinct ledger value (monotonically increasing).
///
/// Initialized with `u64::MAX` sentinel so the first call always returns `true`.
/// Since `current_ledger` only increases, a simple `swap` + equality check
/// suffices: the only duplicate case is same-value calls.
pub(crate) struct LogOncePerLedger(AtomicU64);

impl LogOncePerLedger {
    pub fn new() -> Self {
        Self(AtomicU64::new(u64::MAX))
    }

    /// Returns `true` the first time called for a given `ledger` value.
    /// Subsequent calls with the same `ledger` return `false`.
    /// When `ledger` advances, the first call with the new value returns `true`.
    pub fn should_log(&self, ledger: u64) -> bool {
        let prev = self.0.swap(ledger, Ordering::Relaxed);
        prev != ledger
    }

    /// Reset to initial state so the next sync-loss episode gets a fresh
    /// info-level log.
    pub fn reset(&self) {
        self.0.store(u64::MAX, Ordering::Relaxed);
    }
}

/// Allows one `true` return per `interval` seconds.
///
/// Uses elapsed seconds from a reference instant (e.g., `start_instant`).
/// Initialized with `u64::MAX` sentinel so the first call always returns `true`,
/// avoiding the "first N seconds suppressed" bug.
///
/// Single-caller context assumed (tokio single-threaded async).
pub(crate) struct LogThrottleSecs {
    last_logged: AtomicU64,
    interval: u64,
}

impl LogThrottleSecs {
    pub fn new(interval_secs: u64) -> Self {
        Self {
            last_logged: AtomicU64::new(u64::MAX),
            interval: interval_secs,
        }
    }

    /// Returns `true` if this is the first call (sentinel) or at least
    /// `interval` seconds have elapsed since the last `true` return.
    pub fn should_log(&self, now_secs: u64) -> bool {
        let last = self.last_logged.load(Ordering::Relaxed);
        if last == u64::MAX || now_secs >= last + self.interval {
            self.last_logged.store(now_secs, Ordering::Relaxed);
            true
        } else {
            false
        }
    }

    /// Reset to initial state so the next episode gets an immediate log.
    pub fn reset(&self) {
        self.last_logged.store(u64::MAX, Ordering::Relaxed);
    }
}

/// Default throttle interval for new recovery warnings (30 seconds).
///
/// The recovery tick fires every 10s (`OUT_OF_SYNC_RECOVERY_TIMER_SECS`).
/// A 10s throttle barely suppresses; 30s means the first occurrence fires
/// immediately, then the next 2 ticks are demoted to `debug!`.
const RECOVERY_THROTTLE_SECS: u64 = 30;

/// Groups all recovery-related log throttles into a single struct to avoid
/// per-field growth on [`super::App`].
///
/// All `LogThrottleSecs` fields use a 30-second interval except the two
/// original `cannot_apply_*` throttles from issue #1860 which keep their
/// deployed 10-second interval for backward compatibility.
///
/// The spawned-task warning in `broadcast_recovery_scp_state` (line ~693)
/// and the watchdog SCP verifier errors are excluded: both are already
/// naturally rate-limited to at most once per 10s tick.
pub(crate) struct RecoveryLogThrottles {
    /// Rate-limits "Pending EXTERNALIZE far ahead" to once per distinct ledger.
    pub far_ahead: LogOncePerLedger,
    /// Rate-limits "cannot apply — buffered sequence gap" (10s, #1860).
    pub cannot_apply_gap: LogThrottleSecs,
    /// Rate-limits "cannot apply — missing tx_sets" (10s, #1860).
    pub cannot_apply_txset: LogThrottleSecs,
    /// "Next slot EXTERNALIZE missing — requesting SCP state immediately"
    pub next_slot_missing: LogThrottleSecs,
    /// "Receiving SCP messages but no externalization — fast-tracking catchup"
    pub scp_no_externalization: LogThrottleSecs,
    /// "Essentially caught up but no progress — requesting SCP state"
    pub caught_up_no_progress: LogThrottleSecs,
    /// "Detected gap in externalized slots"
    pub gap_in_externalized: LogThrottleSecs,
    /// "Next slot permanently missing — triggering catchup"
    pub permanently_missing: LogThrottleSecs,
    /// "No tx_sets available for any buffered slot — forcing catchup"
    pub no_txsets_forcing: LogThrottleSecs,
    /// "Recovery escalation blocked: previous fatal catchup failure"
    pub fatal_catchup_blocked: LogThrottleSecs,
}

impl RecoveryLogThrottles {
    pub fn new() -> Self {
        Self {
            far_ahead: LogOncePerLedger::new(),
            cannot_apply_gap: LogThrottleSecs::new(10),
            cannot_apply_txset: LogThrottleSecs::new(10),
            next_slot_missing: LogThrottleSecs::new(RECOVERY_THROTTLE_SECS),
            scp_no_externalization: LogThrottleSecs::new(RECOVERY_THROTTLE_SECS),
            caught_up_no_progress: LogThrottleSecs::new(RECOVERY_THROTTLE_SECS),
            gap_in_externalized: LogThrottleSecs::new(RECOVERY_THROTTLE_SECS),
            permanently_missing: LogThrottleSecs::new(RECOVERY_THROTTLE_SECS),
            no_txsets_forcing: LogThrottleSecs::new(RECOVERY_THROTTLE_SECS),
            fatal_catchup_blocked: LogThrottleSecs::new(RECOVERY_THROTTLE_SECS),
        }
    }

    /// Reset all throttles so the next sync-loss episode produces fresh logs.
    pub fn reset_all(&self) {
        self.far_ahead.reset();
        self.cannot_apply_gap.reset();
        self.cannot_apply_txset.reset();
        self.next_slot_missing.reset();
        self.scp_no_externalization.reset();
        self.caught_up_no_progress.reset();
        self.gap_in_externalized.reset();
        self.permanently_missing.reset();
        self.no_txsets_forcing.reset();
        self.fatal_catchup_blocked.reset();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_log_once_per_ledger_first_call_returns_true() {
        let throttle = LogOncePerLedger::new();
        assert!(throttle.should_log(100));
    }

    #[test]
    fn test_log_once_per_ledger_same_ledger_returns_false() {
        let throttle = LogOncePerLedger::new();
        assert!(throttle.should_log(100));
        assert!(!throttle.should_log(100));
        assert!(!throttle.should_log(100));
    }

    #[test]
    fn test_log_once_per_ledger_new_ledger_returns_true() {
        let throttle = LogOncePerLedger::new();
        assert!(throttle.should_log(100));
        assert!(!throttle.should_log(100));
        assert!(throttle.should_log(101));
        assert!(!throttle.should_log(101));
        assert!(throttle.should_log(200));
    }

    #[test]
    fn test_log_once_per_ledger_reset_allows_refire() {
        let throttle = LogOncePerLedger::new();
        assert!(throttle.should_log(100));
        assert!(!throttle.should_log(100));
        throttle.reset();
        assert!(throttle.should_log(100));
    }

    #[test]
    fn test_log_throttle_secs_first_call_returns_true() {
        let throttle = LogThrottleSecs::new(10);
        assert!(throttle.should_log(0));
    }

    #[test]
    fn test_log_throttle_secs_within_window_returns_false() {
        let throttle = LogThrottleSecs::new(10);
        assert!(throttle.should_log(0));
        assert!(!throttle.should_log(1));
        assert!(!throttle.should_log(5));
        assert!(!throttle.should_log(9));
    }

    #[test]
    fn test_log_throttle_secs_after_window_returns_true() {
        let throttle = LogThrottleSecs::new(10);
        assert!(throttle.should_log(0));
        assert!(!throttle.should_log(5));
        assert!(throttle.should_log(10));
        assert!(!throttle.should_log(15));
        assert!(throttle.should_log(20));
    }

    #[test]
    fn test_log_throttle_secs_reset_allows_refire() {
        let throttle = LogThrottleSecs::new(10);
        assert!(throttle.should_log(0));
        assert!(!throttle.should_log(5));
        throttle.reset();
        assert!(throttle.should_log(5));
    }

    #[test]
    fn test_log_throttle_secs_sentinel_always_fires_first() {
        // Even at time 0, first call should return true (sentinel is u64::MAX).
        let throttle = LogThrottleSecs::new(10);
        assert!(throttle.should_log(0));
    }

    #[test]
    fn test_recovery_throttles_reset_all() {
        let t = RecoveryLogThrottles::new();

        // Fire all throttles once.
        assert!(t.far_ahead.should_log(100));
        assert!(t.cannot_apply_gap.should_log(0));
        assert!(t.cannot_apply_txset.should_log(0));
        assert!(t.next_slot_missing.should_log(0));
        assert!(t.scp_no_externalization.should_log(0));
        assert!(t.caught_up_no_progress.should_log(0));
        assert!(t.gap_in_externalized.should_log(0));
        assert!(t.permanently_missing.should_log(0));
        assert!(t.no_txsets_forcing.should_log(0));
        assert!(t.fatal_catchup_blocked.should_log(0));

        // All suppressed on immediate retry.
        assert!(!t.far_ahead.should_log(100));
        assert!(!t.cannot_apply_gap.should_log(0));
        assert!(!t.next_slot_missing.should_log(0));

        // After reset_all, all re-arm.
        t.reset_all();
        assert!(t.far_ahead.should_log(100));
        assert!(t.cannot_apply_gap.should_log(0));
        assert!(t.cannot_apply_txset.should_log(0));
        assert!(t.next_slot_missing.should_log(0));
        assert!(t.scp_no_externalization.should_log(0));
        assert!(t.caught_up_no_progress.should_log(0));
        assert!(t.gap_in_externalized.should_log(0));
        assert!(t.permanently_missing.should_log(0));
        assert!(t.no_txsets_forcing.should_log(0));
        assert!(t.fatal_catchup_blocked.should_log(0));
    }

    #[test]
    fn test_recovery_throttle_30s_cadence() {
        // Simulates 10s recovery tick cadence with 30s throttle.
        let t = RecoveryLogThrottles::new();
        // First call: fires.
        assert!(t.next_slot_missing.should_log(0));
        // +10s: suppressed.
        assert!(!t.next_slot_missing.should_log(10));
        // +20s: suppressed.
        assert!(!t.next_slot_missing.should_log(20));
        // +30s: fires again.
        assert!(t.next_slot_missing.should_log(30));
        // +40s: suppressed.
        assert!(!t.next_slot_missing.should_log(40));
    }
}
