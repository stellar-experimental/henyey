//! Checkpoint utilities for history operations.
//!
//! This module provides additional checkpoint utilities beyond those in `paths.rs`.
//!
//! Stellar history is organized into checkpoints - groups of 64 ledgers.
//! Checkpoints are identified by their final ledger sequence number,
//! which satisfies `(seq + 1) % 64 == 0`.
//!
//! For example:
//! - Checkpoint 63 contains ledgers 0-63
//! - Checkpoint 127 contains ledgers 64-127
//! - Checkpoint 191 contains ledgers 128-191

pub use crate::paths::{
    bucket_path, checkpoint_frequency, checkpoint_ledger, checkpoint_path, is_checkpoint_ledger,
};

/// Alias for checkpoint_ledger to match naming convention.
///
/// This is the same as `checkpoint_ledger` - returns the checkpoint ledger
/// that contains the given sequence.
pub use checkpoint_ledger as checkpoint_containing;

/// Get the checkpoint ledger less than or equal to the given sequence.
///
/// This returns the latest checkpoint that is <= seq.
/// If seq is before the first checkpoint (< 63), returns None.
///
/// # Examples
///
/// ```
/// use henyey_history::checkpoint::latest_checkpoint_before_or_at;
///
/// assert_eq!(latest_checkpoint_before_or_at(62), None);
/// assert_eq!(latest_checkpoint_before_or_at(63), Some(63));
/// assert_eq!(latest_checkpoint_before_or_at(64), Some(63));
/// assert_eq!(latest_checkpoint_before_or_at(127), Some(127));
/// assert_eq!(latest_checkpoint_before_or_at(128), Some(127));
/// ```
pub fn latest_checkpoint_before_or_at(seq: u32) -> Option<u32> {
    let freq = checkpoint_frequency();
    if seq < freq - 1 {
        return None;
    }

    // The checkpoint that covers seq
    let containing = checkpoint_ledger(seq);

    // If seq is exactly at the checkpoint, return it
    if seq == containing {
        Some(seq)
    } else {
        // Otherwise return the previous checkpoint
        Some(containing - freq)
    }
}

/// Get the next checkpoint after the given sequence.
///
/// Returns the checkpoint ledger that follows `seq`.
///
/// # Examples
///
/// ```
/// use henyey_history::checkpoint::next_checkpoint;
///
/// assert_eq!(next_checkpoint(0), 63);
/// assert_eq!(next_checkpoint(63), 127);
/// assert_eq!(next_checkpoint(64), 127);
/// assert_eq!(next_checkpoint(100), 127);
/// assert_eq!(next_checkpoint(127), 191);
/// ```
pub fn next_checkpoint(seq: u32) -> u32 {
    let containing = checkpoint_ledger(seq);
    if seq < containing {
        containing
    } else {
        containing + checkpoint_frequency()
    }
}

/// Get the first ledger in the checkpoint containing `seq`.
///
/// # Examples
///
/// ```
/// use henyey_history::checkpoint::checkpoint_start;
///
/// assert_eq!(checkpoint_start(0), 1);
/// assert_eq!(checkpoint_start(63), 1);
/// assert_eq!(checkpoint_start(64), 64);
/// assert_eq!(checkpoint_start(127), 64);
/// ```
///
/// Spec: CATCHUP_SPEC §4.3 — `firstInCheckpointContaining(L) = checkpointContaining(L) - sizeOf(L) + 1`.
/// For the first checkpoint (L < 64): 63 - 63 + 1 = 1.
pub fn checkpoint_start(seq: u32) -> u32 {
    let freq = checkpoint_frequency();
    if seq < freq {
        1
    } else {
        (seq / freq) * freq
    }
}

/// Alias for checkpoint_start to match stellar-core HistoryManager naming.
///
/// Returns the first ledger in the checkpoint containing `seq`.
pub use checkpoint_start as first_ledger_in_checkpoint_containing;

/// Get the ledger immediately before the checkpoint containing `seq`.
///
/// Returns `None` if `seq` is in the first checkpoint (ledgers 1-63).
/// The first checkpoint starts at ledger 1 (there is no real ledger 0).
///
/// # Examples
///
/// ```
/// use henyey_history::checkpoint::last_ledger_before_checkpoint_containing;
///
/// assert_eq!(last_ledger_before_checkpoint_containing(0), None);
/// assert_eq!(last_ledger_before_checkpoint_containing(63), None);
/// assert_eq!(last_ledger_before_checkpoint_containing(64), Some(63));
/// assert_eq!(last_ledger_before_checkpoint_containing(127), Some(63));
/// assert_eq!(last_ledger_before_checkpoint_containing(128), Some(127));
/// ```
pub fn last_ledger_before_checkpoint_containing(seq: u32) -> Option<u32> {
    let start = checkpoint_start(seq);
    // First checkpoint starts at ledger 1; there's no real ledger before it.
    if start <= 1 {
        None
    } else {
        Some(start - 1)
    }
}

/// Get the size (number of ledgers) in the checkpoint containing `seq`.
///
/// The first checkpoint contains ledgers 1-63 (63 ledgers, since ledger 0
/// is not a real ledger). All subsequent checkpoints contain exactly 64 ledgers.
/// Spec: CATCHUP_SPEC §4.3 — `sizeOfCheckpointContaining(L) = L < freq ? freq - 1 : freq`.
///
/// # Examples
///
/// ```
/// use henyey_history::checkpoint::size_of_checkpoint_containing;
///
/// assert_eq!(size_of_checkpoint_containing(0), 63);
/// assert_eq!(size_of_checkpoint_containing(63), 63);
/// assert_eq!(size_of_checkpoint_containing(64), 64);
/// assert_eq!(size_of_checkpoint_containing(127), 64);
/// ```
pub fn size_of_checkpoint_containing(seq: u32) -> u32 {
    let freq = checkpoint_frequency();
    if seq < freq {
        freq - 1
    } else {
        freq
    }
}

/// Get the range of ledgers in the checkpoint identified by `checkpoint_ledger_seq`.
///
/// Returns (start, end) inclusive range where end == checkpoint_ledger_seq.
///
/// # Panics
///
/// Panics if `checkpoint_ledger_seq` is not a valid checkpoint ledger.
pub fn checkpoint_range(checkpoint_ledger_seq: u32) -> (u32, u32) {
    assert!(
        is_checkpoint_ledger(checkpoint_ledger_seq),
        "not a checkpoint ledger: {}",
        checkpoint_ledger_seq
    );
    let freq = checkpoint_frequency();
    // Handle first checkpoint — starts at ledger 1 per CATCHUP_SPEC §4.3
    let start = if checkpoint_ledger_seq < freq {
        1
    } else {
        checkpoint_ledger_seq - freq + 1
    };
    (start, checkpoint_ledger_seq)
}

/// Get the ledger that should trigger catchup for a buffered checkpoint.
///
/// Given the first ledger of a buffered checkpoint range, returns the ledger
/// that should trigger catchup processing. This is `first_ledger + 1`,
/// matching stellar-core's `LedgerManager::ledgerToTriggerCatchup`.
///
/// # Panics
///
/// Panics if `first_ledger_of_buffered_checkpoint` is not a checkpoint start
/// (i.e., not a multiple of the checkpoint frequency).
///
/// # Examples
///
/// ```
/// use henyey_history::checkpoint::ledger_to_trigger_catchup;
///
/// assert_eq!(ledger_to_trigger_catchup(0), 1);
/// assert_eq!(ledger_to_trigger_catchup(64), 65);
/// assert_eq!(ledger_to_trigger_catchup(128), 129);
/// ```
pub fn ledger_to_trigger_catchup(first_ledger_of_buffered_checkpoint: u32) -> u32 {
    assert_eq!(
        first_ledger_of_buffered_checkpoint % checkpoint_frequency(),
        0,
        "not a checkpoint start: {}",
        first_ledger_of_buffered_checkpoint
    );
    first_ledger_of_buffered_checkpoint + 1
}

/// Build the HAS (History Archive State) path for a given checkpoint.
///
/// Re-export of [`crate::paths::has_path`].
pub use crate::paths::has_path;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_latest_checkpoint_before_or_at() {
        assert_eq!(latest_checkpoint_before_or_at(0), None);
        assert_eq!(latest_checkpoint_before_or_at(62), None);
        assert_eq!(latest_checkpoint_before_or_at(63), Some(63));
        assert_eq!(latest_checkpoint_before_or_at(64), Some(63));
        assert_eq!(latest_checkpoint_before_or_at(126), Some(63));
        assert_eq!(latest_checkpoint_before_or_at(127), Some(127));
        assert_eq!(latest_checkpoint_before_or_at(128), Some(127));
    }

    #[test]
    fn test_next_checkpoint() {
        assert_eq!(next_checkpoint(0), 63);
        assert_eq!(next_checkpoint(1), 63);
        assert_eq!(next_checkpoint(62), 63);
        assert_eq!(next_checkpoint(63), 127);
        assert_eq!(next_checkpoint(64), 127);
        assert_eq!(next_checkpoint(127), 191);
    }

    #[test]
    fn test_checkpoint_start() {
        // First checkpoint starts at ledger 1 (ledger 0 is not a real ledger).
        // Spec: CATCHUP_SPEC §4.3 — firstInCheckpointContaining(L < freq) = 1.
        assert_eq!(checkpoint_start(0), 1);
        assert_eq!(checkpoint_start(63), 1);
        assert_eq!(checkpoint_start(64), 64);
        assert_eq!(checkpoint_start(127), 64);
        assert_eq!(checkpoint_start(128), 128);
    }

    #[test]
    fn test_checkpoint_containing_matches_stellar_core() {
        for seq in 0..=63 {
            assert_eq!(checkpoint_containing(seq), 63);
        }
        for seq in 64..=127 {
            assert_eq!(checkpoint_containing(seq), 127);
        }
        for seq in 128..=191 {
            assert_eq!(checkpoint_containing(seq), 191);
        }
        for seq in 192..=255 {
            assert_eq!(checkpoint_containing(seq), 255);
        }
        for seq in 256..=258 {
            assert_eq!(checkpoint_containing(seq), 319);
        }
    }

    #[test]
    fn test_checkpoint_range() {
        assert_eq!(checkpoint_range(63), (1, 63));
        assert_eq!(checkpoint_range(127), (64, 127));
        assert_eq!(checkpoint_range(191), (128, 191));
    }

    #[test]
    #[should_panic(expected = "not a checkpoint ledger")]
    fn test_checkpoint_range_invalid() {
        checkpoint_range(64);
    }

    #[test]
    fn test_has_path() {
        let path = has_path(127);
        assert_eq!(path, "history/00/00/00/history-0000007f.json");

        let path = has_path(0xaabbcc00 + 63);
        assert!(path.starts_with("history/"));
        assert!(path.ends_with(".json"));
    }

    #[test]
    fn test_last_ledger_before_checkpoint_containing() {
        // First checkpoint (1-63) has no real ledger before it.
        assert_eq!(last_ledger_before_checkpoint_containing(0), None);
        assert_eq!(last_ledger_before_checkpoint_containing(63), None);
        // Second checkpoint (64-127) is preceded by ledger 63
        assert_eq!(last_ledger_before_checkpoint_containing(64), Some(63));
        assert_eq!(last_ledger_before_checkpoint_containing(127), Some(63));
        // Third checkpoint (128-191) is preceded by ledger 127
        assert_eq!(last_ledger_before_checkpoint_containing(128), Some(127));
        assert_eq!(last_ledger_before_checkpoint_containing(191), Some(127));
    }

    #[test]
    fn test_size_of_checkpoint_containing() {
        // First checkpoint has 63 ledgers (1-63), not 64.
        // Spec: CATCHUP_SPEC §4.3 — sizeOfCheckpointContaining(L < freq) = freq - 1.
        assert_eq!(size_of_checkpoint_containing(0), 63);
        assert_eq!(size_of_checkpoint_containing(63), 63);
        assert_eq!(size_of_checkpoint_containing(64), 64);
        assert_eq!(size_of_checkpoint_containing(1000), 64);
    }

    #[test]
    fn test_first_ledger_in_checkpoint_containing() {
        // Alias for checkpoint_start
        assert_eq!(first_ledger_in_checkpoint_containing(0), 1);
        assert_eq!(first_ledger_in_checkpoint_containing(63), 1);
        assert_eq!(first_ledger_in_checkpoint_containing(64), 64);
        assert_eq!(first_ledger_in_checkpoint_containing(127), 64);
        assert_eq!(first_ledger_in_checkpoint_containing(128), 128);
    }

    // Item 5: ledger_to_trigger_catchup tests
    #[test]
    fn test_ledger_to_trigger_catchup_first_checkpoint() {
        assert_eq!(ledger_to_trigger_catchup(0), 1);
    }

    #[test]
    fn test_ledger_to_trigger_catchup_normal() {
        assert_eq!(ledger_to_trigger_catchup(64), 65);
        assert_eq!(ledger_to_trigger_catchup(128), 129);
        assert_eq!(ledger_to_trigger_catchup(192), 193);
    }

    #[test]
    #[should_panic(expected = "not a checkpoint start")]
    fn test_ledger_to_trigger_catchup_panics_on_non_start() {
        ledger_to_trigger_catchup(63); // 63 is not a checkpoint start
    }
}
