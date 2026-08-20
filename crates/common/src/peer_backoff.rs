//! Shared connect-backoff schedule for the overlay peer-retry logic.
//!
//! This module is the single source of truth for the drift-prone parts of the
//! peer connect-backoff schedule: the two schedule constants and the
//! deterministic backoff ceiling. Prior to consolidation the schedule was
//! copied verbatim at three call sites (`henyey-overlay`'s `PeerManager` and
//! the overlay tick loop, plus `henyey-app`'s persisted peer-record update),
//! so a change to one copy would silently leave the others stale. Routing all
//! three through this module makes any future schedule edit a single-point
//! change that all sites read, and pins the constants so drift fails loudly.
//!
//! Mirrors stellar-core `PeerManager::computeBackoff`
//! (`stellar-core/src/overlay/PeerManager.cpp:365-410`) and `OVERLAY_SPEC
//! §10.3-1`: a random delay in
//! `[1, 2^min(num_failures, MAX_BACKOFF_EXPONENT) * SECONDS_PER_BACKOFF]`
//! seconds, doubling the ceiling per consecutive failure and capped at
//! exponent 10 (~2.84h ceiling).
//!
//! Only the *ceiling* (the `min(n, MAX_BACKOFF_EXPONENT)` cap and the
//! `<<`/`*` arithmetic) and the constants live here — the drift-prone parts.
//! The unbiased uniform sampling (`gen_range(1..=ceiling)`) stays at each call
//! site, so `henyey-common` need not depend on `rand`.

/// Seconds per backoff unit.
///
/// Matches stellar-core `PeerManager.cpp:365-410` and `OVERLAY_SPEC §10.3-1`.
pub const SECONDS_PER_BACKOFF: u64 = 10;

/// Maximum backoff exponent. The ceiling doubles per consecutive failure up to
/// this exponent, then stays clamped.
///
/// Matches stellar-core `PeerManager.cpp:365-410` and `OVERLAY_SPEC §10.3-1`.
pub const MAX_BACKOFF_EXPONENT: u32 = 10;

/// Deterministic upper bound (inclusive, in seconds) for the connect-backoff
/// delay given a consecutive failure count.
///
/// Returns `2^min(num_failures, MAX_BACKOFF_EXPONENT) * SECONDS_PER_BACKOFF`.
/// The value is always `>= SECONDS_PER_BACKOFF` (>= 10), so callers can safely
/// sample `gen_range(1..=ceiling)` without an empty-range guard.
///
/// Call sites sample uniformly in `[1, ceiling]` to produce the actual delay
/// (see module docs); this function centralizes only the drift-prone ceiling.
pub fn connect_backoff_ceiling_secs(num_failures: u32) -> u64 {
    let exponent = num_failures.min(MAX_BACKOFF_EXPONENT);
    (1u64 << exponent) * SECONDS_PER_BACKOFF
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The ceiling doubles per failure from 10s and clamps at exponent 10
    /// (10240s), exercising the `min(n, MAX_BACKOFF_EXPONENT)` cap.
    #[test]
    fn test_ceiling_schedule() {
        let expected = [10u64, 20, 40, 80, 160, 320, 640, 1280, 2560, 5120, 10240];
        for (n, &want) in expected.iter().enumerate() {
            assert_eq!(
                connect_backoff_ceiling_secs(n as u32),
                want,
                "ceiling for {n} failures"
            );
        }
        // Beyond MAX_BACKOFF_EXPONENT the ceiling stays clamped.
        assert_eq!(connect_backoff_ceiling_secs(11), 10240);
        assert_eq!(connect_backoff_ceiling_secs(1000), 10240);
        assert_eq!(connect_backoff_ceiling_secs(u32::MAX), 10240);
    }

    /// Pin the schedule constants to their spec values. This is the load-bearing
    /// drift guard: after consolidation there is exactly one definition of each
    /// constant and all three call sites read it, so any future schedule edit
    /// flips this test and the sites can no longer diverge from each other.
    #[test]
    fn test_schedule_constants_pinned() {
        assert_eq!(SECONDS_PER_BACKOFF, 10);
        assert_eq!(MAX_BACKOFF_EXPONENT, 10);
    }

    /// The ceiling is always `>= 1` (in fact `>= 10`) for every failure count,
    /// so a call site sampling `gen_range(1..=ceiling)` always sees a non-empty
    /// range and never yields 0. (The sampling itself stays at each site and is
    /// exercised by those sites' tests, e.g. the overlay tick-loop backoff
    /// bounds test — `henyey-common` is intentionally `rand`-free.)
    #[test]
    fn test_ceiling_never_below_one() {
        for &n in &[0u32, 5, 10, 1000, u32::MAX] {
            assert!(connect_backoff_ceiling_secs(n) >= 1);
        }
    }
}
