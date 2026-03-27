//! Catchup range calculation for history synchronization.
//!
//! This module implements the stellar-core algorithm for determining
//! which ledgers to download and replay during catchup operations.
//!
//! The algorithm handles five cases based on the current LCL (Last Closed Ledger),
//! target ledger, and requested replay count:
//!
//! | Case | Condition | Action |
//! |------|-----------|--------|
//! | 1 | LCL > genesis | Replay from LCL+1 to target (no buckets) |
//! | 2 | count >= full replay count | Full replay from genesis+1 |
//! | 3 | count=0 AND target is checkpoint | Buckets only, no replay |
//! | 4 | target start in first checkpoint | Full replay from genesis+1 |
//! | 5 | default | Apply buckets at prior checkpoint, replay from there |

use crate::checkpoint::{
    first_ledger_in_checkpoint_containing, is_checkpoint_ledger,
    last_ledger_before_checkpoint_containing,
};

/// Genesis ledger sequence number.
pub const GENESIS_LEDGER_SEQ: u32 = 1;

/// A half-open range of ledgers [first, first+count).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LedgerRange {
    /// First ledger in the range.
    pub first: u32,
    /// Number of ledgers in the range.
    pub count: u32,
}

impl LedgerRange {
    /// Create a new ledger range.
    pub fn new(first: u32, count: u32) -> Self {
        Self { first, count }
    }

    /// Create an empty range.
    pub fn empty() -> Self {
        Self { first: 0, count: 0 }
    }

    /// Check if the range is empty.
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// Get the limit (one past the last ledger).
    pub fn limit(&self) -> u32 {
        self.first + self.count
    }

    /// Get the last ledger in the range (panics if empty).
    pub fn last(&self) -> u32 {
        assert!(self.count > 0, "cannot get last of empty range");
        self.first + self.count - 1
    }
}

/// Catchup mode determining history depth.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CatchupMode {
    /// Download only the latest bucket state (fastest).
    /// Equivalent to count=0 in stellar-core.
    #[default]
    Minimal,
    /// Download complete history from genesis.
    /// Equivalent to count=UINT32_MAX in stellar-core.
    Complete,
    /// Download the last N ledgers of history.
    Recent(u32),
}

impl CatchupMode {
    /// Get the count value for this mode.
    ///
    /// - Minimal: 0
    /// - Complete: u32::MAX
    /// - Recent(n): n
    pub fn count(&self) -> u32 {
        match self {
            CatchupMode::Minimal => 0,
            CatchupMode::Complete => u32::MAX,
            CatchupMode::Recent(n) => *n,
        }
    }
}

/// Error type for parsing CatchupMode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseCatchupModeError(String);

impl std::fmt::Display for ParseCatchupModeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for ParseCatchupModeError {}

impl std::str::FromStr for CatchupMode {
    type Err = ParseCatchupModeError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "minimal" => Ok(Self::Minimal),
            "complete" => Ok(Self::Complete),
            _ => {
                // Try to parse as "recent:N"
                if let Some(count) = s.strip_prefix("recent:") {
                    let n: u32 = count.parse().map_err(|_| {
                        ParseCatchupModeError(format!("Invalid recent count: {}", count))
                    })?;
                    Ok(Self::Recent(n))
                } else {
                    Err(ParseCatchupModeError(format!(
                        "Unknown catchup mode: {}",
                        s
                    )))
                }
            }
        }
    }
}

impl std::fmt::Display for CatchupMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CatchupMode::Minimal => write!(f, "minimal"),
            CatchupMode::Complete => write!(f, "complete"),
            CatchupMode::Recent(n) => write!(f, "recent:{}", n),
        }
    }
}

/// Range required to perform a catchup operation.
///
/// Contains information about whether to apply buckets and which ledgers to replay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatchupRange {
    /// Whether to apply buckets from a checkpoint.
    apply_buckets: bool,
    /// Which ledger to apply buckets at (0 if !apply_buckets).
    apply_buckets_at_ledger: u32,
    /// Range of ledgers to replay after bucket application.
    replay_range: LedgerRange,
}

impl CatchupRange {
    /// Create a range that only applies buckets (no replay).
    pub fn buckets_only(apply_at: u32) -> Self {
        let range = Self {
            apply_buckets: true,
            apply_buckets_at_ledger: apply_at,
            replay_range: LedgerRange::empty(),
        };
        range.check_invariants();
        range
    }

    /// Create a range that only replays ledgers (no bucket application).
    pub fn replay_only(replay_range: LedgerRange) -> Self {
        let range = Self {
            apply_buckets: false,
            apply_buckets_at_ledger: 0,
            replay_range,
        };
        range.check_invariants();
        range
    }

    /// Create a range that applies buckets then replays ledgers.
    pub fn buckets_and_replay(apply_at: u32, replay_range: LedgerRange) -> Self {
        let range = Self {
            apply_buckets: true,
            apply_buckets_at_ledger: apply_at,
            replay_range,
        };
        range.check_invariants();
        range
    }

    /// Calculate the catchup range based on LCL, target, and mode.
    ///
    /// # Arguments
    ///
    /// * `lcl` - Last closed ledger (must be >= GENESIS_LEDGER_SEQ)
    /// * `target` - Target ledger to catch up to (must be > lcl)
    /// * `mode` - Catchup mode determining history depth
    ///
    /// # Panics
    ///
    /// Panics if preconditions are not met.
    pub fn calculate(lcl: u32, target: u32, mode: CatchupMode) -> Self {
        // Validate preconditions
        assert!(
            lcl >= GENESIS_LEDGER_SEQ,
            "lcl {} must be >= genesis {}",
            lcl,
            GENESIS_LEDGER_SEQ
        );
        assert!(target > lcl, "target {} must be > lcl {}", target, lcl);
        assert!(
            target > GENESIS_LEDGER_SEQ,
            "target {} must be > genesis {}",
            target,
            GENESIS_LEDGER_SEQ
        );

        let count = mode.count();
        let full_replay_count = target - lcl;

        // Case 1: LCL is past genesis — replay from LCL+1, unless the gap is large enough
        // to justify a fresh checkpoint download.
        //
        // For Minimal mode with a LARGE gap (> MINIMAL_BUCKET_DOWNLOAD_THRESHOLD), fall
        // through to the checkpoint-based logic. Downloading a fresh checkpoint is faster
        // than replaying thousands of ledgers (e.g., 22k ledger startup gap).
        //
        // For small gaps, ALWAYS replay from LCL+1. A 4-minute bucket download for a
        // 93-ledger gap would block the event loop and trigger an infinite catchup loop
        // (the network advances faster than the bucket download completes).
        const MINIMAL_BUCKET_DOWNLOAD_THRESHOLD: u32 = 10_000;
        if lcl > GENESIS_LEDGER_SEQ {
            if mode != CatchupMode::Minimal
                || full_replay_count <= MINIMAL_BUCKET_DOWNLOAD_THRESHOLD
            {
                let replay = LedgerRange::new(lcl + 1, full_replay_count);
                return Self::replay_only(replay);
            }
            // Fall through: Minimal mode + large gap → use checkpoint download below.
        }

        // Remaining cases: lcl == genesis, OR lcl > genesis with Minimal mode + large gap.
        // full_replay covers from lcl+1 to target in both situations.
        let full_replay = LedgerRange::new(lcl + 1, full_replay_count);

        // Case 2: count >= full replay count — full replay from current state.
        //
        // When lcl == genesis, prefer bucket-apply over full replay. Replaying
        // through protocol upgrades from genesis (e.g. 0→25) produces different
        // state hashes than the live network because `apply_upgrades_to_delta`
        // creates intermediate config entries that differ from the validator's
        // live upgrade path. stellar-core handles this the same way: online
        // catchup always involves a bucket-apply step, never raw replay from
        // genesis.
        if count >= full_replay_count && lcl > GENESIS_LEDGER_SEQ {
            return Self::replay_only(full_replay);
        }

        // Case 3: count=0 and target is a checkpoint, buckets only
        if count == 0 && is_checkpoint_ledger(target) {
            return Self::buckets_only(target);
        }

        // Calculate target start ledger (first ledger we want to replay)
        let target_start = target.saturating_sub(count) + 1;
        let first_in_checkpoint = first_ledger_in_checkpoint_containing(target_start);

        // Case 4: target start is in first checkpoint, full replay
        // (only when we already have post-upgrade state from a prior catchup)
        if first_in_checkpoint <= GENESIS_LEDGER_SEQ && lcl > GENESIS_LEDGER_SEQ {
            return Self::replay_only(full_replay);
        }

        // Case 4b: target start is in first checkpoint AND lcl is genesis.
        // We can't replay from genesis (protocol upgrade hashes won't match),
        // and there's no earlier checkpoint to apply buckets from. If the target
        // is a checkpoint, do bucket-apply there. Otherwise, find the nearest
        // checkpoint at or after target to apply buckets, then there's nothing
        // to replay (the target is before that checkpoint).
        if first_in_checkpoint <= GENESIS_LEDGER_SEQ && lcl == GENESIS_LEDGER_SEQ {
            if is_checkpoint_ledger(target) {
                return Self::buckets_only(target);
            }
            // Target is before the first checkpoint. No HAS is available yet.
            // Fall back to replay from genesis — this only happens when the
            // network has fewer ledgers than a single checkpoint period.
            return Self::replay_only(full_replay);
        }

        // Case 5: apply buckets at checkpoint before target_start, then replay
        let apply_buckets_at = last_ledger_before_checkpoint_containing(target_start)
            .expect("target_start not in first checkpoint, so there must be a previous checkpoint");
        let replay = LedgerRange::new(first_in_checkpoint, target - apply_buckets_at);
        Self::buckets_and_replay(apply_buckets_at, replay)
    }

    fn check_invariants(&self) {
        // Must be applying buckets and/or replaying
        assert!(
            self.apply_buckets || self.replay_ledgers(),
            "must apply buckets or replay ledgers"
        );

        if !self.apply_buckets && self.replay_ledgers() {
            // Cases 1, 2, 4: no buckets, only replay
            assert_eq!(self.apply_buckets_at_ledger, 0);
            assert_ne!(self.replay_range.first, 0);
        } else if self.apply_buckets && self.replay_ledgers() {
            // Case 5: buckets and replay
            assert_ne!(self.apply_buckets_at_ledger, 0);
            assert_ne!(self.replay_range.first, 0);
            assert_eq!(
                self.apply_buckets_at_ledger + 1,
                self.replay_range.first,
                "replay must start immediately after bucket apply"
            );
        } else {
            // Case 3: buckets only, no replay
            assert!(self.apply_buckets && !self.replay_ledgers());
            assert_eq!(self.replay_range.first, 0);
        }
    }

    /// Whether buckets should be applied.
    pub fn apply_buckets(&self) -> bool {
        self.apply_buckets
    }

    /// Get the ledger at which to apply buckets.
    ///
    /// # Panics
    ///
    /// Panics if `apply_buckets()` is false.
    pub fn bucket_apply_ledger(&self) -> u32 {
        assert!(
            self.apply_buckets,
            "bucket_apply_ledger called when apply_buckets is false"
        );
        self.apply_buckets_at_ledger
    }

    /// Whether ledgers should be replayed.
    pub fn replay_ledgers(&self) -> bool {
        self.replay_range.count > 0
    }

    /// Get the replay range.
    pub fn replay_range(&self) -> LedgerRange {
        self.replay_range
    }

    /// Get the first ledger to replay.
    pub fn replay_first(&self) -> u32 {
        self.replay_range.first
    }

    /// Get the number of ledgers to replay.
    pub fn replay_count(&self) -> u32 {
        self.replay_range.count
    }

    /// Get one past the last ledger to replay.
    pub fn replay_limit(&self) -> u32 {
        self.replay_range.limit()
    }

    /// Get the first ledger in the full range (bucket apply or replay start).
    pub fn first(&self) -> u32 {
        if self.apply_buckets {
            self.apply_buckets_at_ledger
        } else {
            self.replay_range.first
        }
    }

    /// Get the last ledger in the full range.
    pub fn last(&self) -> u32 {
        if self.replay_range.count > 0 {
            self.replay_range.last()
        } else {
            assert!(self.apply_buckets);
            self.apply_buckets_at_ledger
        }
    }

    /// Get the total count of ledgers (bucket apply counts as 1).
    pub fn count(&self) -> u32 {
        if self.apply_buckets {
            self.replay_range.count + 1
        } else {
            self.replay_range.count
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ledger_range() {
        let range = LedgerRange::new(100, 10);
        assert_eq!(range.first, 100);
        assert_eq!(range.count, 10);
        assert_eq!(range.limit(), 110);
        assert_eq!(range.last(), 109);
        assert!(!range.is_empty());

        let empty = LedgerRange::empty();
        assert!(empty.is_empty());
    }

    #[test]
    fn test_catchup_mode_count() {
        assert_eq!(CatchupMode::Minimal.count(), 0);
        assert_eq!(CatchupMode::Complete.count(), u32::MAX);
        assert_eq!(CatchupMode::Recent(128).count(), 128);
    }

    #[test]
    fn test_case1_lcl_past_genesis_non_minimal() {
        // LCL is past genesis with non-Minimal mode — replay from LCL+1
        let range = CatchupRange::calculate(100, 200, CatchupMode::Complete);
        assert!(!range.apply_buckets());
        assert!(range.replay_ledgers());
        assert_eq!(range.replay_first(), 101);
        assert_eq!(range.replay_count(), 100);
    }

    #[test]
    fn test_minimal_lcl_past_genesis_small_gap_replays() {
        // Minimal mode with persisted LCL and a SMALL gap (< 10_000) — must replay,
        // not download buckets. A 4-minute bucket download for a 93-ledger gap would
        // block the event loop and cause an infinite catchup loop.
        // target=127 is a checkpoint ledger, but gap=27 < 10_000 → replay_only.
        let range = CatchupRange::calculate(100, 127, CatchupMode::Minimal);
        assert!(!range.apply_buckets());
        assert!(range.replay_ledgers());
        assert_eq!(range.replay_first(), 101);
        assert_eq!(range.replay_count(), 27);
    }

    #[test]
    fn test_minimal_lcl_past_genesis_buffered_catchup_gap() {
        // Simulate the buffered-catchup scenario: LCL=61551871, gap=93.
        // Must use replay_only, not bucket download.
        let range = CatchupRange::calculate(61551871, 61551964, CatchupMode::Minimal);
        assert!(!range.apply_buckets());
        assert!(range.replay_ledgers());
        assert_eq!(range.replay_first(), 61551872);
        assert_eq!(range.replay_count(), 93);
    }

    #[test]
    fn test_minimal_mainnet_scenario() {
        // Startup scenario: persisted at L61529351, target L61551615 (checkpoint).
        // Gap=22264 > 10_000 → download checkpoint, not replay 22k ledgers.
        let range = CatchupRange::calculate(61529351, 61551615, CatchupMode::Minimal);
        assert!(range.apply_buckets());
        assert!(!range.replay_ledgers());
        assert_eq!(range.bucket_apply_ledger(), 61551615);
    }

    #[test]
    fn test_case2_full_replay() {
        // count >= full replay count, do full replay
        let range = CatchupRange::calculate(1, 100, CatchupMode::Complete);
        assert!(!range.apply_buckets());
        assert!(range.replay_ledgers());
        assert_eq!(range.replay_first(), 2);
        assert_eq!(range.replay_count(), 99);
    }

    #[test]
    fn test_case3_buckets_only() {
        // count=0 and target is checkpoint, buckets only
        let range = CatchupRange::calculate(1, 127, CatchupMode::Minimal);
        assert!(range.apply_buckets());
        assert!(!range.replay_ledgers());
        assert_eq!(range.bucket_apply_ledger(), 127);
    }

    #[test]
    fn test_case3_minimal_non_checkpoint() {
        // count=0 but target is NOT a checkpoint
        // This falls through to case 5
        let range = CatchupRange::calculate(1, 100, CatchupMode::Minimal);
        assert!(range.apply_buckets());
        assert!(range.replay_ledgers());
        // Should apply buckets at checkpoint 63, replay from 64 to 100
        assert_eq!(range.bucket_apply_ledger(), 63);
        assert_eq!(range.replay_first(), 64);
        assert_eq!(range.replay_count(), 37); // 100 - 63 = 37
    }

    #[test]
    fn test_case4_target_in_first_checkpoint() {
        // target start is in first checkpoint, full replay
        let range = CatchupRange::calculate(1, 50, CatchupMode::Recent(10));
        // target_start = 50 - 10 + 1 = 41, which is in first checkpoint (0-63)
        assert!(!range.apply_buckets());
        assert!(range.replay_ledgers());
        assert_eq!(range.replay_first(), 2);
        assert_eq!(range.replay_count(), 49);
    }

    #[test]
    fn test_case5_buckets_and_replay() {
        // Apply buckets at prior checkpoint, then replay
        let range = CatchupRange::calculate(1, 200, CatchupMode::Recent(50));
        // target_start = 200 - 50 + 1 = 151
        // first_in_checkpoint(151) = 128
        // last_before_checkpoint(151) = 127
        assert!(range.apply_buckets());
        assert!(range.replay_ledgers());
        assert_eq!(range.bucket_apply_ledger(), 127);
        assert_eq!(range.replay_first(), 128);
        assert_eq!(range.replay_count(), 73); // 200 - 127 = 73
    }

    #[test]
    fn test_complete_from_genesis_to_checkpoint() {
        // Quickstart captive core scenario: CATCHUP_COMPLETE from genesis to a
        // checkpoint. Must use bucket-apply, NOT replay from genesis, because
        // replaying through protocol upgrades from genesis (0→25) produces
        // different state hashes than the live validator.
        //
        // With accelerated checkpoint frequency (8): checkpoints at 7, 15, 23...
        // Use default frequency (64): checkpoints at 63, 127, 191...
        let range = CatchupRange::calculate(1, 63, CatchupMode::Complete);
        assert!(
            range.apply_buckets(),
            "should use bucket-apply, not replay from genesis"
        );
        assert!(!range.replay_ledgers());
        assert_eq!(range.bucket_apply_ledger(), 63);
    }

    #[test]
    fn test_complete_from_genesis_non_checkpoint() {
        // Complete mode from genesis to a non-checkpoint target. No HAS is
        // available, so we must replay from genesis (fallback).
        let range = CatchupRange::calculate(1, 100, CatchupMode::Complete);
        assert!(!range.apply_buckets());
        assert!(range.replay_ledgers());
        assert_eq!(range.replay_first(), 2);
        assert_eq!(range.replay_count(), 99);
    }

    #[test]
    fn test_complete_from_non_genesis() {
        // Complete mode from a non-genesis LCL should still replay.
        let range = CatchupRange::calculate(50, 100, CatchupMode::Complete);
        assert!(!range.apply_buckets());
        assert!(range.replay_ledgers());
        assert_eq!(range.replay_first(), 51);
        assert_eq!(range.replay_count(), 50);
    }

    #[test]
    fn test_recent_128_current_ledger() {
        // Typical "recent:128" catchup to a recent ledger
        let target = 843007; // A checkpoint ledger
        let range = CatchupRange::calculate(1, target, CatchupMode::Recent(128));

        // target_start = 843007 - 128 + 1 = 842880
        // first_in_checkpoint(842880) = 842880 (it's at checkpoint start)
        // last_before_checkpoint(842880) = 842879
        assert!(range.apply_buckets());
        assert!(range.replay_ledgers());
        // Should apply buckets at 842879, replay 128 ledgers
        assert_eq!(range.bucket_apply_ledger(), 842879);
        assert_eq!(range.replay_first(), 842880);
        assert_eq!(range.replay_count(), 128); // 843007 - 842879 = 128
    }
}
