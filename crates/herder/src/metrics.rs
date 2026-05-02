//! SCP metrics counters shared across Herder, ScpDriver, and the async verify
//! worker.
//!
//! All fields are private `AtomicU64`; callers use the typed increment helpers.
//! A [`ScpMetricsSnapshot`] (all `u64`, `Copy`) can be obtained via
//! [`ScpMetrics::snapshot()`] for zero-cost hand-off to the metrics scrape path.

use std::sync::atomic::{AtomicU64, Ordering};

/// Shared SCP event counters.
///
/// Wrap in `Arc<ScpMetrics>` and hand to each component that produces events.
#[derive(Debug, Default)]
pub struct ScpMetrics {
    envelope_sign_total: AtomicU64,
    envelope_validsig_total: AtomicU64,
    envelope_invalidsig_total: AtomicU64,
    value_valid_total: AtomicU64,
    value_invalid_total: AtomicU64,
    combine_candidates_total: AtomicU64,
}

impl ScpMetrics {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn inc_envelope_sign(&self) {
        self.envelope_sign_total.fetch_add(1, Ordering::Relaxed);
    }

    pub fn inc_envelope_validsig(&self) {
        self.envelope_validsig_total.fetch_add(1, Ordering::Relaxed);
    }

    pub fn inc_envelope_invalidsig(&self) {
        self.envelope_invalidsig_total
            .fetch_add(1, Ordering::Relaxed);
    }

    pub fn inc_value_valid(&self) {
        self.value_valid_total.fetch_add(1, Ordering::Relaxed);
    }

    pub fn inc_value_invalid(&self) {
        self.value_invalid_total.fetch_add(1, Ordering::Relaxed);
    }

    pub fn add_combine_candidates(&self, n: u64) {
        self.combine_candidates_total
            .fetch_add(n, Ordering::Relaxed);
    }

    /// Atomic snapshot suitable for the `/metrics` scrape path.
    pub fn snapshot(&self) -> ScpMetricsSnapshot {
        ScpMetricsSnapshot {
            envelope_sign_total: self.envelope_sign_total.load(Ordering::Relaxed),
            envelope_validsig_total: self.envelope_validsig_total.load(Ordering::Relaxed),
            envelope_invalidsig_total: self.envelope_invalidsig_total.load(Ordering::Relaxed),
            value_valid_total: self.value_valid_total.load(Ordering::Relaxed),
            value_invalid_total: self.value_invalid_total.load(Ordering::Relaxed),
            combine_candidates_total: self.combine_candidates_total.load(Ordering::Relaxed),
        }
    }
}

/// Point-in-time snapshot of SCP counters. All fields are plain `u64`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ScpMetricsSnapshot {
    pub envelope_sign_total: u64,
    pub envelope_validsig_total: u64,
    pub envelope_invalidsig_total: u64,
    pub value_valid_total: u64,
    pub value_invalid_total: u64,
    pub combine_candidates_total: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_increment_and_snapshot() {
        let m = ScpMetrics::new();
        m.inc_envelope_sign();
        m.inc_envelope_sign();
        m.inc_envelope_validsig();
        m.inc_envelope_invalidsig();
        m.inc_value_valid();
        m.inc_value_valid();
        m.inc_value_valid();
        m.inc_value_invalid();
        m.add_combine_candidates(5);
        m.add_combine_candidates(3);

        let snap = m.snapshot();
        assert_eq!(snap.envelope_sign_total, 2);
        assert_eq!(snap.envelope_validsig_total, 1);
        assert_eq!(snap.envelope_invalidsig_total, 1);
        assert_eq!(snap.value_valid_total, 3);
        assert_eq!(snap.value_invalid_total, 1);
        assert_eq!(snap.combine_candidates_total, 8);
    }

    #[test]
    fn test_default_is_zero() {
        let snap = ScpMetrics::new().snapshot();
        assert_eq!(snap, ScpMetricsSnapshot::default());
    }
}
