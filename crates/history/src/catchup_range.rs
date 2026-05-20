//! Catchup range calculation for history synchronization.
//!
//! This module implements the stellar-core algorithm for determining
//! which ledgers to download and replay during catchup operations.
//!
//! The algorithm handles cases based on the current LCL (Last Closed Ledger),
//! target ledger, and requested replay count:
//!
//! | Case | Condition | Action |
//! |------|-----------|--------|
//! | 0 | Complete mode, LCL == genesis | Replay from genesis+1 to target |
//! | 1 | LCL > genesis | Replay from LCL+1 to target (no buckets) |
//! | 2 | count >= full replay count, LCL > genesis | Full replay from LCL+1 |
//! | 3 | count=0 AND target is checkpoint | Buckets only, no replay |
//! | 4 | target start in first checkpoint | Full replay from genesis+1 |
//! | 5 | default (LCL == genesis) | Apply buckets at prior checkpoint, replay from there |

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

/// Catchup mode determining history depth (replay count).
///
/// This enum models the spec's `count` field (§3.4), NOT the spec's `Mode`
/// discriminator. For the spec-aligned run-mode axis, see [`CatchupRunMode`].
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

/// Spec §6.1 run-mode discriminator.
///
/// This is orthogonal to [`CatchupMode`] (which controls replay depth / count).
/// The run mode distinguishes the operational context in which catchup runs:
///
/// - `OfflineBasic`: Standalone CLI catchup (no overlay, no SCP).
/// - `OfflineComplete`: Full archive verification with tx-result checking.
///   (Not yet implemented — semantics tracked by #2831.)
/// - `Online`: Live node catchup during startup or recovery (overlay + SCP active).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CatchupRunMode {
    /// Standalone offline catchup (CLI `catchup` command).
    /// No overlay, no SCP context.
    #[default]
    OfflineBasic,
    /// Full archive verification with tx-result set checking.
    /// Semantics not yet implemented — tracked by #2831.
    OfflineComplete,
    /// Live node catchup during startup or recovery.
    /// Overlay and SCP context are active.
    Online,
}

impl std::fmt::Display for CatchupRunMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CatchupRunMode::OfflineBasic => write!(f, "OFFLINE_BASIC"),
            CatchupRunMode::OfflineComplete => write!(f, "OFFLINE_COMPLETE"),
            CatchupRunMode::Online => write!(f, "ONLINE"),
        }
    }
}

/// A catchup configuration carrying both the replay depth and the spec run mode.
///
/// This type separates the two orthogonal axes of catchup configuration:
/// - **depth** ([`CatchupMode`]): how many ledgers to replay (spec `count` field, §3.4)
/// - **run_mode** ([`CatchupRunMode`]): operational context (spec `Mode` discriminator, §6.1)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CatchupConfiguration {
    /// Replay depth / count (Minimal, Complete, or Recent(N)).
    pub depth: CatchupMode,
    /// Spec §6.1 run-mode discriminator.
    pub run_mode: CatchupRunMode,
}

impl CatchupConfiguration {
    /// Create a new catchup configuration.
    pub fn new(depth: CatchupMode, run_mode: CatchupRunMode) -> Self {
        Self { depth, run_mode }
    }

    /// Create an offline-basic configuration with the given depth.
    pub fn offline_basic(depth: CatchupMode) -> Self {
        Self {
            depth,
            run_mode: CatchupRunMode::OfflineBasic,
        }
    }

    /// Create an online configuration with the given depth.
    pub fn online(depth: CatchupMode) -> Self {
        Self {
            depth,
            run_mode: CatchupRunMode::Online,
        }
    }
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
/// Three variants encode the post-#2677 invariant "bucket-apply only when
/// LCL == genesis" at the type level:
///
/// - [`CatchupRange::ReplayOnly`]: pure replay of `replay.first ..=
///   replay.last()`. Used for Case 0 (Complete from genesis), Case 1 (LCL >
///   genesis), Case 4 fallback, and the Case 4b fallback when target is
///   before the first checkpoint.
/// - [`CatchupRange::BucketApplyAndReplay`]: apply buckets at `checkpoint`,
///   then replay `replay` (which always starts at `checkpoint + 1`). Used
///   for Case 5 (LCL == genesis, target past the first checkpoint).
/// - [`CatchupRange::BucketsOnly`]: apply buckets at `checkpoint` and stop
///   (no replay). Used for Case 3 (count=0 and target is a checkpoint) and
///   Case 4b checkpoint (LCL == genesis, target is a checkpoint inside the
///   first checkpoint period).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CatchupRange {
    /// Replay ledgers without applying buckets.
    ReplayOnly {
        /// Range of ledgers to replay (must be non-empty).
        replay: LedgerRange,
    },
    /// Apply buckets at `checkpoint`, then replay ledgers starting at
    /// `checkpoint + 1`. Invariants: `replay.first == checkpoint + 1` and
    /// `replay.count > 0`. Enforced by [`CatchupRange::buckets_and_replay`].
    BucketApplyAndReplay {
        /// Checkpoint ledger at which to apply buckets.
        checkpoint: u32,
        /// Range of ledgers to replay after bucket application.
        replay: LedgerRange,
    },
    /// Apply buckets at `checkpoint` and stop (no replay).
    BucketsOnly {
        /// Checkpoint ledger at which to apply buckets.
        checkpoint: u32,
    },
}

impl CatchupRange {
    /// Create a range that only applies buckets (no replay).
    pub(crate) fn buckets_only(checkpoint: u32) -> Self {
        assert!(checkpoint > 0, "buckets_only checkpoint must be > 0");
        Self::BucketsOnly { checkpoint }
    }

    /// Create a range that only replays ledgers (no bucket application).
    pub(crate) fn replay_only(replay: LedgerRange) -> Self {
        assert!(
            replay.count > 0,
            "replay_only requires a non-empty replay range"
        );
        assert!(replay.first > 0, "replay_only replay.first must be > 0");
        Self::ReplayOnly { replay }
    }

    /// Create a range that applies buckets then replays ledgers.
    pub(crate) fn buckets_and_replay(checkpoint: u32, replay: LedgerRange) -> Self {
        assert!(checkpoint > 0, "buckets_and_replay checkpoint must be > 0");
        assert!(
            replay.count > 0,
            "buckets_and_replay requires a non-empty replay range"
        );
        assert_eq!(
            checkpoint + 1,
            replay.first,
            "replay must start immediately after bucket apply"
        );
        Self::BucketApplyAndReplay { checkpoint, replay }
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
    pub(crate) fn calculate(lcl: u32, target: u32, mode: CatchupMode) -> Self {
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

        // Case 0: Complete mode from genesis — replay all ledgers.
        //
        // CATCHUP_COMPLETE=true consumers (galexie) need metadata for every
        // ledger from genesis. Bucket-apply would only produce a single
        // synthetic meta for the checkpoint ledger, but the Go SDK expects
        // sequential metadata starting from ledger 2. stellar-core handles
        // this identically: CatchupRange with count=UINT32_MAX from genesis
        // always yields full replay (CatchupRange.cpp Case 2).
        if mode == CatchupMode::Complete && lcl == GENESIS_LEDGER_SEQ {
            let replay = LedgerRange::new(lcl + 1, full_replay_count);
            return Self::replay_only(replay);
        }

        // Case 1: LCL is past genesis — unconditionally replay from LCL+1.
        //
        // This matches stellar-core CatchupRange.cpp:52-57 exactly: when
        // LCL > genesis, always replay the full gap. This structurally
        // guarantees INV-C15 (bucket-apply never targets a ledger older
        // than LCL) because bucket-apply only occurs on the genesis path.
        if lcl > GENESIS_LEDGER_SEQ {
            let replay = LedgerRange::new(lcl + 1, full_replay_count);
            return Self::replay_only(replay);
        }

        // Remaining cases: lcl == genesis (Case 1 above handles lcl > genesis).
        // full_replay covers from lcl+1 (genesis+1) to target.
        let full_replay = LedgerRange::new(lcl + 1, full_replay_count);

        // Case 2: count >= full replay count — full replay from genesis.
        //
        // When lcl == genesis, prefer bucket-apply over full replay. Replaying
        // through protocol upgrades from genesis (e.g. 0→25) produces different
        // state hashes than the live network because `apply_upgrades_to_delta`
        // creates intermediate config entries that differ from the validator's
        // live upgrade path. stellar-core handles this the same way: online
        // catchup always involves a bucket-apply step, never raw replay from
        // genesis.
        // Note: lcl == genesis at this point (Case 1 returns for lcl > genesis),
        // so this case is unreachable. Kept for documentation/safety.
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

    /// Get the replay range, if this variant performs replay.
    #[cfg(test)]
    pub fn replay_range(&self) -> Option<LedgerRange> {
        match self {
            Self::ReplayOnly { replay } | Self::BucketApplyAndReplay { replay, .. } => {
                Some(*replay)
            }
            Self::BucketsOnly { .. } => None,
        }
    }

    /// Get the first ledger in the full range (bucket apply or replay start).
    #[cfg(test)]
    pub fn first(&self) -> u32 {
        match self {
            Self::ReplayOnly { replay } => replay.first,
            Self::BucketApplyAndReplay { checkpoint, .. } | Self::BucketsOnly { checkpoint } => {
                *checkpoint
            }
        }
    }

    /// Get the last ledger in the full range.
    #[cfg(test)]
    pub fn last(&self) -> u32 {
        match self {
            Self::ReplayOnly { replay } | Self::BucketApplyAndReplay { replay, .. } => {
                replay.last()
            }
            Self::BucketsOnly { checkpoint } => *checkpoint,
        }
    }

    /// Get the total count of ledgers (bucket apply counts as 1).
    #[cfg(test)]
    pub fn count(&self) -> u32 {
        match self {
            Self::ReplayOnly { replay } => replay.count,
            Self::BucketApplyAndReplay { replay, .. } => replay.count + 1,
            Self::BucketsOnly { .. } => 1,
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
        assert_eq!(
            range,
            CatchupRange::ReplayOnly {
                replay: LedgerRange::new(101, 100)
            }
        );
    }

    #[test]
    fn test_minimal_lcl_past_genesis_small_gap_replays() {
        // Minimal mode with persisted LCL and a SMALL gap (< threshold) — must replay,
        // not download buckets. A 4-minute bucket download for a 93-ledger gap would
        // block the event loop and cause an infinite catchup loop.
        // target=127 is a checkpoint ledger, but gap=27 < threshold → replay_only.
        let range = CatchupRange::calculate(100, 127, CatchupMode::Minimal);
        assert_eq!(
            range,
            CatchupRange::ReplayOnly {
                replay: LedgerRange::new(101, 27)
            }
        );
    }

    #[test]
    fn test_minimal_lcl_past_genesis_buffered_catchup_gap() {
        // Simulate the buffered-catchup scenario: LCL=61551871, gap=93.
        // Must use replay_only, not bucket download.
        let range = CatchupRange::calculate(61551871, 61551964, CatchupMode::Minimal);
        assert_eq!(
            range,
            CatchupRange::ReplayOnly {
                replay: LedgerRange::new(61551872, 93)
            }
        );
    }

    #[test]
    fn test_minimal_mainnet_scenario() {
        // Startup scenario: persisted at L61529351, target L61551615 (checkpoint).
        // With stellar-core parity (Case 1), LCL > genesis always replays.
        // Previously this used bucket-download optimization; now it replays
        // the full gap to maintain INV-C15.
        let range = CatchupRange::calculate(61529351, 61551615, CatchupMode::Minimal);
        assert_eq!(
            range,
            CatchupRange::ReplayOnly {
                replay: LedgerRange::new(61529352, 22264) // 61551615 - 61529351
            }
        );
    }

    #[test]
    fn test_case2_full_replay() {
        // count >= full replay count, do full replay
        let range = CatchupRange::calculate(1, 100, CatchupMode::Complete);
        assert_eq!(
            range,
            CatchupRange::ReplayOnly {
                replay: LedgerRange::new(2, 99)
            }
        );
    }

    #[test]
    fn test_case3_buckets_only() {
        // count=0 and target is checkpoint, buckets only
        let range = CatchupRange::calculate(1, 127, CatchupMode::Minimal);
        assert_eq!(range, CatchupRange::BucketsOnly { checkpoint: 127 });
    }

    #[test]
    fn test_case3_minimal_non_checkpoint() {
        // count=0 but target is NOT a checkpoint
        // This falls through to case 5: apply buckets at 63, replay 64..=100.
        let range = CatchupRange::calculate(1, 100, CatchupMode::Minimal);
        assert_eq!(
            range,
            CatchupRange::BucketApplyAndReplay {
                checkpoint: 63,
                replay: LedgerRange::new(64, 37) // 100 - 63 = 37
            }
        );
    }

    #[test]
    fn test_case4_target_in_first_checkpoint() {
        // target start is in first checkpoint, full replay
        let range = CatchupRange::calculate(1, 50, CatchupMode::Recent(10));
        // target_start = 50 - 10 + 1 = 41, which is in first checkpoint (0-63)
        assert_eq!(
            range,
            CatchupRange::ReplayOnly {
                replay: LedgerRange::new(2, 49)
            }
        );
    }

    #[test]
    fn test_case5_buckets_and_replay() {
        // Apply buckets at prior checkpoint, then replay
        let range = CatchupRange::calculate(1, 200, CatchupMode::Recent(50));
        // target_start = 200 - 50 + 1 = 151
        // first_in_checkpoint(151) = 128
        // last_before_checkpoint(151) = 127
        assert_eq!(
            range,
            CatchupRange::BucketApplyAndReplay {
                checkpoint: 127,
                replay: LedgerRange::new(128, 73) // 200 - 127 = 73
            }
        );
    }

    #[test]
    fn test_complete_from_genesis_to_checkpoint() {
        // CATCHUP_COMPLETE from genesis: must replay all ledgers so captive
        // core consumers (galexie) receive metadata for every ledger from
        // genesis. stellar-core handles this identically (CatchupRange.cpp
        // Case 2: count=UINT32_MAX >= full_replay_count → replay_only).
        let range = CatchupRange::calculate(1, 63, CatchupMode::Complete);
        assert_eq!(
            range,
            CatchupRange::ReplayOnly {
                replay: LedgerRange::new(2, 62)
            },
            "Complete mode from genesis must replay, not bucket-apply"
        );
    }

    #[test]
    fn test_minimal_from_genesis_to_checkpoint() {
        // Minimal mode from genesis to a checkpoint should use bucket-apply
        // (no replay). Replaying through protocol upgrades from genesis
        // produces different state hashes than the live network.
        let range = CatchupRange::calculate(1, 63, CatchupMode::Minimal);
        assert_eq!(
            range,
            CatchupRange::BucketsOnly { checkpoint: 63 },
            "Minimal mode from genesis should use bucket-apply"
        );
    }

    #[test]
    fn test_complete_from_genesis_non_checkpoint() {
        // Complete mode from genesis to a non-checkpoint target. No HAS is
        // available, so we must replay from genesis (fallback).
        let range = CatchupRange::calculate(1, 100, CatchupMode::Complete);
        assert_eq!(
            range,
            CatchupRange::ReplayOnly {
                replay: LedgerRange::new(2, 99)
            }
        );
    }

    #[test]
    fn test_complete_from_non_genesis() {
        // Complete mode from a non-genesis LCL should still replay.
        let range = CatchupRange::calculate(50, 100, CatchupMode::Complete);
        assert_eq!(
            range,
            CatchupRange::ReplayOnly {
                replay: LedgerRange::new(51, 50)
            }
        );
    }

    #[test]
    fn test_recent_from_genesis_large_count() {
        // Recent mode from genesis where count >= full_replay_count.
        // The lcl > GENESIS guard on Case 2 prevents a full replay from
        // genesis; should fall through to bucket-apply.
        let range = CatchupRange::calculate(1, 63, CatchupMode::Recent(100));
        assert_eq!(
            range,
            CatchupRange::BucketsOnly { checkpoint: 63 },
            "Recent with large count from genesis should bucket-apply, not replay"
        );
    }

    #[test]
    fn test_recent_from_genesis_non_checkpoint_target() {
        // Recent mode from genesis to a non-checkpoint target in the first
        // checkpoint. No HAS available, so falls back to replay from genesis
        // (only valid for very small networks < 63 ledgers).
        let range = CatchupRange::calculate(1, 50, CatchupMode::Recent(40));
        // target_start = 50 - 40 + 1 = 11, first_in_checkpoint(11) = 1 <= GENESIS
        // lcl == GENESIS, target is not a checkpoint -> fallback replay
        assert_eq!(
            range,
            CatchupRange::ReplayOnly {
                replay: LedgerRange::new(2, 49)
            }
        );
    }

    #[test]
    fn test_case1_lcl_past_genesis_target_in_first_checkpoint() {
        // LCL > genesis, target in first checkpoint.
        // Case 1 now unconditionally replays from LCL+1 (stellar-core parity).
        let range = CatchupRange::calculate(2, 50, CatchupMode::Recent(10));
        assert_eq!(
            range,
            CatchupRange::ReplayOnly {
                replay: LedgerRange::new(3, 48) // 50 - 2 = 48
            }
        );
    }

    #[test]
    fn test_recent_128_current_ledger() {
        // Typical "recent:128" catchup to a recent ledger
        let target = 843007; // A checkpoint ledger
        let range = CatchupRange::calculate(1, target, CatchupMode::Recent(128));

        // target_start = 843007 - 128 + 1 = 842880
        // first_in_checkpoint(842880) = 842880 (it's at checkpoint start)
        // last_before_checkpoint(842880) = 842879
        assert_eq!(
            range,
            CatchupRange::BucketApplyAndReplay {
                checkpoint: 842879,
                replay: LedgerRange::new(842880, 128) // 843007 - 842879 = 128
            }
        );
    }

    #[test]
    fn test_minimal_genesis_large_non_checkpoint_target() {
        // Minimal mode from genesis to a large non-checkpoint target.
        // count=0 → target_start = 10001 (past first checkpoint).
        // Falls through to Case 5: bucket-apply at prior checkpoint, then replay.
        // checkpoint_start(10001) = 9984, apply_buckets_at = 9983
        let range = CatchupRange::calculate(1, 10000, CatchupMode::Minimal);
        assert_eq!(
            range,
            CatchupRange::BucketApplyAndReplay {
                checkpoint: 9983,
                replay: LedgerRange::new(9984, 17) // 10000 - 9983 = 17
            }
        );
    }

    #[test]
    fn test_minimal_large_gap_non_checkpoint_target() {
        // Minimal mode from a non-genesis LCL with a large gap. With
        // stellar-core parity (Case 1), LCL > genesis always replays —
        // bucket-apply optimization was removed to enforce INV-C15.
        let range = CatchupRange::calculate(50000, 70000, CatchupMode::Minimal);
        assert_eq!(
            range,
            CatchupRange::ReplayOnly {
                replay: LedgerRange::new(50001, 20000) // 70000 - 50000
            }
        );
    }

    // ── Recent(N) Case 1 regression tests ──────────────────────────────
    // These tests verify stellar-core parity: LCL > genesis always replays.
    // (Previously these tested the Case 1b optimization removed for INV-C15.)

    #[test]
    fn test_recent_500_large_gap_replays_with_parity() {
        // Recent(500), lcl=100, target=10_000 → gap=9900.
        // With stellar-core parity, LCL > genesis always replays from LCL+1.
        let range = CatchupRange::calculate(100, 10_000, CatchupMode::Recent(500));
        assert_eq!(
            range,
            CatchupRange::ReplayOnly {
                replay: LedgerRange::new(101, 9900)
            }
        );
    }

    #[test]
    fn test_recent_10000_small_gap_replays() {
        // Recent(10_000), lcl=100, target=5000 → gap=4900.
        // LCL > genesis → replay from lcl+1.
        let range = CatchupRange::calculate(100, 5000, CatchupMode::Recent(10_000));
        assert_eq!(
            range,
            CatchupRange::ReplayOnly {
                replay: LedgerRange::new(101, 4900)
            }
        );
    }

    #[test]
    fn test_recent_500_buffered_catchup_replays() {
        // Recent(500), lcl=61_551_871, target=61_551_964 → gap=93.
        // LCL > genesis → replay from lcl+1.
        let range = CatchupRange::calculate(61_551_871, 61_551_964, CatchupMode::Recent(500));
        assert_eq!(
            range,
            CatchupRange::ReplayOnly {
                replay: LedgerRange::new(61_551_872, 93)
            }
        );
    }

    #[test]
    fn test_recent_boundary_gap_equals_n() {
        // Recent(100), lcl=100, target=200 → gap=100 = N.
        // LCL > genesis → replay from lcl+1 regardless of count.
        let range = CatchupRange::calculate(100, 200, CatchupMode::Recent(100));
        assert_eq!(
            range,
            CatchupRange::ReplayOnly {
                replay: LedgerRange::new(101, 100)
            }
        );
    }

    #[test]
    fn test_recent_boundary_gap_exceeds_n_by_one() {
        // Recent(99), lcl=100, target=200 → gap=100 > 99.
        // With stellar-core parity, LCL > genesis always replays from LCL+1.
        // (Previously this fell through to checkpoint download — INV-C15 violation.)
        let range = CatchupRange::calculate(100, 200, CatchupMode::Recent(99));
        assert_eq!(
            range,
            CatchupRange::ReplayOnly {
                replay: LedgerRange::new(101, 100)
            }
        );
    }

    #[test]
    fn test_recent_large_gap_checkpoint_target() {
        // Recent(500), lcl=100, target=10_047 (non-checkpoint) → gap > 500.
        // With stellar-core parity, LCL > genesis always replays from LCL+1.
        let range = CatchupRange::calculate(100, 10_047, CatchupMode::Recent(500));
        assert_eq!(
            range,
            CatchupRange::ReplayOnly {
                replay: LedgerRange::new(101, 9947) // 10047 - 100
            }
        );
    }

    #[test]
    fn test_recent_large_gap_target_start_in_first_checkpoint() {
        // Recent(150), lcl=2, target=200 → gap=198.
        // LCL > genesis → Case 1 returns replay_only.
        let range = CatchupRange::calculate(2, 200, CatchupMode::Recent(150));
        assert_eq!(
            range,
            CatchupRange::ReplayOnly {
                replay: LedgerRange::new(3, 198)
            }
        );
    }

    #[test]
    fn test_recent_0_lcl_past_genesis_replays() {
        // Recent(0), lcl=100, target=200 → gap=100.
        // With stellar-core parity, LCL > genesis always replays from LCL+1.
        // (Previously this fell through to checkpoint download.)
        let range = CatchupRange::calculate(100, 200, CatchupMode::Recent(0));
        assert_eq!(
            range,
            CatchupRange::ReplayOnly {
                replay: LedgerRange::new(101, 100)
            }
        );
    }

    // ── Accessor / constructor invariant tests ─────────────────────────

    #[test]
    fn test_accessors_replay_only() {
        let range = CatchupRange::ReplayOnly {
            replay: LedgerRange::new(10, 5),
        };
        assert_eq!(range.first(), 10);
        assert_eq!(range.last(), 14);
        assert_eq!(range.count(), 5);
        assert_eq!(range.replay_range(), Some(LedgerRange::new(10, 5)));
    }

    #[test]
    fn test_accessors_bucket_apply_and_replay() {
        let range = CatchupRange::BucketApplyAndReplay {
            checkpoint: 127,
            replay: LedgerRange::new(128, 73),
        };
        assert_eq!(range.first(), 127);
        assert_eq!(range.last(), 200);
        assert_eq!(range.count(), 74); // 73 replayed + 1 bucket apply
        assert_eq!(range.replay_range(), Some(LedgerRange::new(128, 73)));
    }

    #[test]
    fn test_accessors_buckets_only() {
        let range = CatchupRange::BucketsOnly { checkpoint: 127 };
        assert_eq!(range.first(), 127);
        assert_eq!(range.last(), 127);
        assert_eq!(range.count(), 1);
        assert_eq!(range.replay_range(), None);
    }

    #[test]
    #[should_panic(expected = "replay must start immediately after bucket apply")]
    fn test_buckets_and_replay_rejects_gap() {
        // replay.first must equal checkpoint + 1.
        let _ = CatchupRange::buckets_and_replay(127, LedgerRange::new(130, 10));
    }

    #[test]
    #[should_panic(expected = "buckets_and_replay requires a non-empty replay range")]
    fn test_buckets_and_replay_rejects_empty_replay() {
        let _ = CatchupRange::buckets_and_replay(127, LedgerRange::new(128, 0));
    }

    #[test]
    #[should_panic(expected = "replay_only requires a non-empty replay range")]
    fn test_replay_only_rejects_empty_replay() {
        let _ = CatchupRange::replay_only(LedgerRange::empty());
    }

    #[test]
    fn test_catchup_configuration_online_preserves_depth() {
        // The CatchupConfiguration must store run_mode and depth independently,
        // so the two axes cannot collapse back together.
        let config_recent = CatchupConfiguration::online(CatchupMode::Recent(500));
        assert_eq!(config_recent.depth, CatchupMode::Recent(500));
        assert_eq!(config_recent.run_mode, CatchupRunMode::Online);

        let config_complete = CatchupConfiguration::online(CatchupMode::Complete);
        assert_eq!(config_complete.depth, CatchupMode::Complete);
        assert_eq!(config_complete.run_mode, CatchupRunMode::Online);

        let config_minimal = CatchupConfiguration::online(CatchupMode::Minimal);
        assert_eq!(config_minimal.depth, CatchupMode::Minimal);
        assert_eq!(config_minimal.run_mode, CatchupRunMode::Online);

        // Offline basic with Complete depth is NOT the same as OfflineComplete.
        let offline_complete_depth = CatchupConfiguration::offline_basic(CatchupMode::Complete);
        assert_eq!(offline_complete_depth.depth, CatchupMode::Complete);
        assert_eq!(
            offline_complete_depth.run_mode,
            CatchupRunMode::OfflineBasic
        );
        assert_ne!(
            offline_complete_depth.run_mode,
            CatchupRunMode::OfflineComplete
        );
    }

    #[test]
    fn test_catchup_run_mode_display() {
        assert_eq!(CatchupRunMode::OfflineBasic.to_string(), "OFFLINE_BASIC");
        assert_eq!(
            CatchupRunMode::OfflineComplete.to_string(),
            "OFFLINE_COMPLETE"
        );
        assert_eq!(CatchupRunMode::Online.to_string(), "ONLINE");
    }
}
