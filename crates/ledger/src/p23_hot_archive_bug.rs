//! CAP-0076 / Protocol 23 hot-archive bug remediation (issue #3061).
//!
//! Port of stellar-core's `addHotArchiveBatchWithP23HotArchiveFix`
//! (`stellar-core/src/ledger/P23HotArchiveBug.cpp:37-109`).
//!
//! On the **mainnet** p23→p24 upgrade ledger only, stellar-core appends 478
//! hardcoded "correct-state" hot-archive entries to the hot-archive batch
//! before `addHotArchiveBatch`, repairing a set of entries that were corrupted
//! during protocol 23. This makes the hot-archive bucket hash — and thus the
//! combined `bucketListHash` — match the network during replay.
//!
//! ## Preconditions (mirroring stellar-core's `releaseAssert`s)
//!
//! For each of the 478 hardcoded corrupted/correct pairs:
//!  1. `key(corrupted) == key(correct)` — a hardcoded-data invariant. Hard
//!     error if violated (the data table is generated, so this should never
//!     fire unless the table is broken).
//!  2. The corrupted key must exist in the **hot archive**. If absent, log a
//!     WARNING and *skip* this entry (NOT a hard error) — matches stellar-core.
//!  3. The hot-archive entry's archived state must byte-match the hardcoded
//!     corrupted entry. Hard error if not.
//!  4. The key must be **absent from live state**. Hard error if present.
//!  5. The key must NOT be in the current (natural) batch. Hard error if it is.
//!
//! Preconditions 1/3/4/5 mirror `releaseAssert` (consensus-fatal): on
//! violation we return an `Err`, which aborts the ledger close — the
//! Rust-idiomatic equivalent of aborting the process, and divergence-safe
//! (a node that hits one of these would otherwise commit a wrong
//! `bucketListHash`).
//!
//! ## Scope boundary
//!
//! stellar-core's `Protocol23CorruptionDataVerifier` (CSV-file-driven catchup
//! verification) and `Protocol23CorruptionEventReconciler` (SAC mint/burn
//! event reconciliation) live in the same upstream file but are config-gated
//! optional machinery that never mutate the hot-archive batch:
//!  - The data verifier is a pure no-op exclusion here — Henyey has no
//!    corruption-file config concept, so there is nothing to wire up.
//!  - The SAC-event reconciler (gated on `BACKFILL_STELLAR_ASSET_EVENTS`) is a
//!    distinct spec gap tracked separately; it has zero `bucketListHash`
//!    effect and is out of scope for this remediation.

use stellar_xdr::curr::{LedgerEntry, LedgerKey, Limits, ReadXdr};

use henyey_common::entry_to_key;

use crate::error::LedgerError;
use crate::Result;

pub use crate::p23_hot_archive_bug_data::{
    P23_CORRUPTED_HOT_ARCHIVE_ENTRIES, P23_CORRUPTED_HOT_ARCHIVE_ENTRIES_COUNT,
    P23_CORRUPTED_HOT_ARCHIVE_ENTRY_CORRECT_STATE,
};

/// Decode the `i`-th hardcoded *corrupted* hot-archive `LedgerEntry`.
pub fn decode_corrupted_entry(i: usize) -> Result<LedgerEntry> {
    decode_ledger_entry(P23_CORRUPTED_HOT_ARCHIVE_ENTRIES[i])
}

/// Decode the `i`-th hardcoded *correct-state* hot-archive `LedgerEntry`.
pub fn decode_correct_entry(i: usize) -> Result<LedgerEntry> {
    decode_ledger_entry(P23_CORRUPTED_HOT_ARCHIVE_ENTRY_CORRECT_STATE[i])
}

fn decode_ledger_entry(encoded_base64: &str) -> Result<LedgerEntry> {
    LedgerEntry::from_xdr_base64(encoded_base64, Limits::none()).map_err(|e| {
        LedgerError::Serialization(format!(
            "failed to decode P23 hot-archive bug LedgerEntry: {e}"
        ))
    })
}

/// Build the hot-archive batch for the mainnet p23→p24 upgrade ledger,
/// appending the 478 hardcoded corrected entries to the natural
/// `archived_entries` after running stellar-core's five preconditions.
///
/// Port of `addHotArchiveBatchWithP23HotArchiveFix`
/// (`P23HotArchiveBug.cpp:37-109`). The caller is responsible for the gating
/// (`prev_version < 24 && protocol_version >= 24 && network_id.is_mainnet()`)
/// and for feeding the resulting vec into `HotArchiveBucketList::add_batch`.
///
/// # Arguments
/// * `archived_entries` — the natural archived batch for this ledger (empty on
///   the upgrade ledger, since eviction is disabled).
/// * `hot_archive_load` — loads the *archived* `LedgerEntry` for a key from the
///   hot-archive snapshot as it stood *before* this batch (`None` if absent).
///   Mirrors `hotArchiveSnapshot->load(key)`.
/// * `live_state_load` — loads a `LedgerEntry` from live state (`None` if
///   absent). Mirrors `ltx.loadWithoutRecord(key)`.
///
/// Returns the natural batch with the corrected entries appended. Ordering is
/// hash-irrelevant: `HotArchiveBucket::fresh` sorts and dedups, and the
/// preconditions guarantee no corrected key collides with the natural batch.
pub fn add_hot_archive_batch_with_p23_fix<HF, LF>(
    archived_entries: Vec<LedgerEntry>,
    hot_archive_load: HF,
    live_state_load: LF,
) -> Result<Vec<LedgerEntry>>
where
    HF: Fn(&LedgerKey) -> Result<Option<LedgerEntry>>,
    LF: Fn(&LedgerKey) -> Result<Option<LedgerEntry>>,
{
    // Keys in the natural batch (precondition #5).
    let current_batch_keys: std::collections::HashSet<LedgerKey> =
        archived_entries.iter().map(entry_to_key).collect();

    let mut updated = archived_entries;
    updated.reserve(P23_CORRUPTED_HOT_ARCHIVE_ENTRIES_COUNT);

    for i in 0..P23_CORRUPTED_HOT_ARCHIVE_ENTRIES_COUNT {
        let corrupted_entry = decode_corrupted_entry(i)?;
        let corrupted_key = entry_to_key(&corrupted_entry);

        let fixed_entry = decode_correct_entry(i)?;
        let fixed_key = entry_to_key(&fixed_entry);

        // (1) Hardcoded-data invariant: corrupted and correct share a key.
        if corrupted_key != fixed_key {
            return Err(LedgerError::Internal(format!(
                "P23 hot-archive fix: corrupted/correct key mismatch at index {i}"
            )));
        }

        // (2) Entry must exist in the hot archive. If absent, skip with a
        //     warning (NOT a hard error) — matches stellar-core.
        let hot_archive_entry = match hot_archive_load(&corrupted_key)? {
            Some(e) => e,
            None => {
                tracing::warn!(
                    index = i,
                    key = ?corrupted_key,
                    "Skipping fix of the entry as it does not exist in the Hot Archive"
                );
                continue;
            }
        };

        // (3) The hot-archive state must match the expected corrupted entry.
        if hot_archive_entry != corrupted_entry {
            return Err(LedgerError::Internal(format!(
                "P23 hot-archive fix: hot archive state for index {i} does not match \
                 expected corrupted entry"
            )));
        }

        // (4) The key must be absent from live state.
        if live_state_load(&corrupted_key)?.is_some() {
            return Err(LedgerError::Internal(format!(
                "P23 hot-archive fix: entry at index {i} unexpectedly present in live state"
            )));
        }

        // (5) The key must not be in the current (natural) batch.
        if current_batch_keys.contains(&corrupted_key) {
            return Err(LedgerError::Internal(format!(
                "P23 hot-archive fix: entry at index {i} collides with the natural batch"
            )));
        }

        tracing::info!(index = i, key = ?corrupted_key, "Applied fix to Hot Archive entry");
        updated.push(fixed_entry);
    }

    Ok(updated)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Data-table integrity: both arrays have exactly 478 entries.
    #[test]
    fn data_table_lengths_are_478() {
        assert_eq!(P23_CORRUPTED_HOT_ARCHIVE_ENTRIES_COUNT, 478);
        assert_eq!(P23_CORRUPTED_HOT_ARCHIVE_ENTRIES.len(), 478);
        assert_eq!(P23_CORRUPTED_HOT_ARCHIVE_ENTRY_CORRECT_STATE.len(), 478);
    }

    /// Every pair decodes to a valid `LedgerEntry`, and the corrupted and
    /// correct entries share the same `LedgerKey` (mirrors `verifyHardcodedData`).
    #[test]
    fn every_pair_decodes_and_shares_a_key() {
        for i in 0..P23_CORRUPTED_HOT_ARCHIVE_ENTRIES_COUNT {
            let corrupted = decode_corrupted_entry(i)
                .unwrap_or_else(|e| panic!("corrupted[{i}] failed to decode: {e}"));
            let correct = decode_correct_entry(i)
                .unwrap_or_else(|e| panic!("correct[{i}] failed to decode: {e}"));
            assert_eq!(
                entry_to_key(&corrupted),
                entry_to_key(&correct),
                "key mismatch at index {i}"
            );
        }
    }

    /// The 478 corrupted keys are pairwise distinct.
    #[test]
    fn corrupted_keys_are_pairwise_distinct() {
        let mut keys = std::collections::HashSet::new();
        for i in 0..P23_CORRUPTED_HOT_ARCHIVE_ENTRIES_COUNT {
            let key = entry_to_key(&decode_corrupted_entry(i).unwrap());
            assert!(keys.insert(key), "duplicate key at index {i}");
        }
        assert_eq!(keys.len(), 478);
    }

    /// Corrupted and correct entries differ (otherwise there would be nothing
    /// to fix) for at least the spot-checked indices.
    #[test]
    fn corrupted_and_correct_differ() {
        for i in [0usize, 1, 100, 238, 477] {
            let corrupted = decode_corrupted_entry(i).unwrap();
            let correct = decode_correct_entry(i).unwrap();
            assert_ne!(corrupted, correct, "entry {i} should have been corrected");
        }
    }
}
