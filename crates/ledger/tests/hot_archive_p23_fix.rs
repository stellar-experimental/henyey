//! Regression tests for the CAP-0076 / Protocol 23 hot-archive bug remediation
//! (issue #3061).
//!
//! On the mainnet p23→p24 upgrade ledger, stellar-core's
//! `addHotArchiveBatchWithP23HotArchiveFix` appends 478 hardcoded
//! "correct-state" hot-archive entries to the hot-archive batch (under five
//! release-assert preconditions) so the hot-archive hash — and thus the
//! combined `bucketListHash` — matches the network during replay.
//!
//! These tests pin:
//! - the structural behavior of `add_hot_archive_batch_with_p23_fix`
//!   (seeded entries get their corrected state appended; entries absent from
//!   the hot archive are skipped-with-warning), and
//! - the production-network gate (`is_mainnet()`).

use henyey_ledger::p23_hot_archive_bug::{
    add_hot_archive_batch_with_p23_fix, decode_correct_entry, decode_corrupted_entry,
    P23_CORRUPTED_HOT_ARCHIVE_ENTRIES_COUNT,
};

use henyey_common::entry_to_key;
use stellar_xdr::curr::{LedgerEntry, LedgerKey};

/// A live-state loader that always reports "absent" — matching the real
/// scenario where the 478 corrupted keys were evicted long before the upgrade
/// and are not present in live state.
fn empty_live_state(_key: &LedgerKey) -> Result<Option<LedgerEntry>, henyey_ledger::LedgerError> {
    Ok(None)
}

/// Build a hot-archive loader closure from an explicit seed map of
/// key -> archived entry (mirroring a `HotArchiveBucketList` snapshot).
fn seeded_hot_archive(
    seed: Vec<LedgerEntry>,
) -> impl Fn(&LedgerKey) -> Result<Option<LedgerEntry>, henyey_ledger::LedgerError> {
    move |key: &LedgerKey| {
        for entry in &seed {
            if &entry_to_key(entry) == key {
                return Ok(Some(entry.clone()));
            }
        }
        Ok(None)
    }
}

/// When the seeded hot archive contains a (byte-exact) subset of the hardcoded
/// corrupted entries, the fix appends the *corrected* state for exactly those
/// keys, and skips the rest with a warning.
#[test]
fn test_p23_fix_appends_corrected_entries_for_seeded_subset() {
    // Seed a small subset of the corrupted entries into the "hot archive".
    let seed_indices = [0usize, 1, 5, 100, 477];
    let seed: Vec<LedgerEntry> = seed_indices
        .iter()
        .map(|&i| decode_corrupted_entry(i).expect("decode corrupted"))
        .collect();

    // Natural batch is empty on the upgrade ledger (eviction disabled).
    let natural: Vec<LedgerEntry> = Vec::new();

    let result =
        add_hot_archive_batch_with_p23_fix(natural, seeded_hot_archive(seed), empty_live_state)
            .expect("fix should succeed");

    // Exactly the seeded keys should have been corrected and appended.
    assert_eq!(
        result.len(),
        seed_indices.len(),
        "only seeded entries should be appended"
    );

    let result_keys: std::collections::HashSet<LedgerKey> =
        result.iter().map(entry_to_key).collect();

    for &i in &seed_indices {
        let correct = decode_correct_entry(i).expect("decode correct");
        let key = entry_to_key(&correct);
        assert!(
            result_keys.contains(&key),
            "corrected entry for seeded index {i} must be present"
        );
        // The appended entry must be the *correct* state, not the corrupted one.
        let appended = result
            .iter()
            .find(|e| entry_to_key(e) == key)
            .expect("appended entry");
        assert_eq!(appended, &correct, "appended entry must be correct state");
    }
}

/// Entries absent from the hot archive are skipped (no panic, no append).
#[test]
fn test_p23_fix_skips_entries_absent_from_hot_archive() {
    // Empty hot archive — every hardcoded entry is absent.
    let result = add_hot_archive_batch_with_p23_fix(
        Vec::new(),
        |_key: &LedgerKey| Ok(None),
        empty_live_state,
    )
    .expect("fix should succeed with all-absent hot archive");

    assert!(
        result.is_empty(),
        "no corrected entries appended when none are present in the hot archive"
    );
}

/// The natural batch is preserved (prepended) and corrected entries are
/// appended after it.
#[test]
fn test_p23_fix_preserves_natural_batch() {
    // Use a natural entry that is NOT one of the hardcoded keys, so it can't
    // collide with precondition #5.
    let natural_entry = make_unrelated_entry();
    let natural_key = entry_to_key(&natural_entry);

    let seed = vec![decode_corrupted_entry(0).unwrap()];

    let result = add_hot_archive_batch_with_p23_fix(
        vec![natural_entry.clone()],
        seeded_hot_archive(seed),
        empty_live_state,
    )
    .expect("fix should succeed");

    assert_eq!(result.len(), 2, "natural entry + one corrected entry");
    assert_eq!(result[0], natural_entry, "natural batch comes first");
    let corrected_key = entry_to_key(&decode_correct_entry(0).unwrap());
    assert!(result.iter().any(|e| entry_to_key(e) == corrected_key));
    // Sanity: the natural key is still present exactly once.
    assert_eq!(
        result
            .iter()
            .filter(|e| entry_to_key(e) == natural_key)
            .count(),
        1
    );
}

/// Precondition #3: if the hot-archive entry's state does NOT match the
/// hardcoded corrupted entry, the fix is a hard error (mirrors releaseAssert).
#[test]
fn test_p23_fix_errors_on_hot_archive_state_mismatch() {
    // Seed the hot archive with the *correct* entry at the corrupted key,
    // i.e. a state that does not match the expected corrupted state.
    let mismatched = decode_correct_entry(0).unwrap();

    let err = add_hot_archive_batch_with_p23_fix(
        Vec::new(),
        seeded_hot_archive(vec![mismatched]),
        empty_live_state,
    )
    .expect_err("state mismatch must be a hard error");

    let msg = err.to_string();
    assert!(
        msg.contains("hot archive") || msg.contains("state"),
        "error should mention hot-archive state mismatch, got: {msg}"
    );
}

/// Precondition #4: if the key is present in live state, the fix is a hard
/// error (mirrors releaseAssert(!ltx.loadWithoutRecord(key))).
#[test]
fn test_p23_fix_errors_when_key_present_in_live_state() {
    let corrupted = decode_corrupted_entry(0).unwrap();
    let corrupted_key = entry_to_key(&corrupted);

    let live = move |key: &LedgerKey| {
        if key == &corrupted_key {
            Ok(Some(decode_correct_entry(0).unwrap()))
        } else {
            Ok(None)
        }
    };

    let err =
        add_hot_archive_batch_with_p23_fix(Vec::new(), seeded_hot_archive(vec![corrupted]), live)
            .expect_err("present-in-live-state must be a hard error");

    assert!(err.to_string().contains("live state"));
}

#[test]
fn test_count_constant_is_478() {
    assert_eq!(P23_CORRUPTED_HOT_ARCHIVE_ENTRIES_COUNT, 478);
}

// ---------------------------------------------------------------------------
// Note on manager-level coverage
//
// stellar-core gates the fix on the mainnet p23→p24 *upgrade ledger*, i.e.
// `prev_version == 23` (initialLedgerVers) with the closing ledger at v24. In
// the henyey injection point (`manager.rs`), the fix runs inside the
// `protocol_version_starts_from(prev_version, V23)` hot-archive branch under
// the additional gate `prev_version < 24 && protocol_version >= 24 &&
// network_id.is_mainnet()`.
//
// That close path is NOT reachable through the public `initialize` +
// `close_ledger` API: henyey supports protocol 24+ only, and the ledger
// manager panics on any genesis/init at a version in 1..=23 (manager.rs ~2085,
// "unsupported protocol version"). Reaching `prev_version == 23` would require
// first closing a v23 ledger, which requires initializing at v23 — rejected.
// The pre-existing mainnet p23→p24 fee-pool correction (`prev_version == 23`,
// manager.rs ~3011) is likewise exercised only at the function level for the
// same reason.
//
// The gate itself is three plain conjunctions over `prev_version`,
// `protocol_version`, and `network_id.is_mainnet()` (verified by the
// `is_mainnet()` equality check below); the batch-construction behavior it
// guards is fully covered by the `add_hot_archive_batch_with_p23_fix` tests
// above, which decode the real hardcoded data and assert the structural
// outcome. So the close-driven assertion reduces to those function-level tests
// plus the gate's boolean logic.
// ---------------------------------------------------------------------------

use henyey_common::NetworkId;

/// Pin the production-network gate: only the real mainnet passphrase trips
/// `is_mainnet()` (a synthesized id will not), and testnet does not — matching
/// stellar-core's `gIsProductionNetwork`.
#[test]
fn test_is_mainnet_gate() {
    assert!(
        NetworkId::from_passphrase("Public Global Stellar Network ; September 2015").is_mainnet()
    );
    assert!(NetworkId::mainnet().is_mainnet());
    assert!(!NetworkId::from_passphrase("Test SDF Network ; September 2015").is_mainnet());
    assert!(!NetworkId::testnet().is_mainnet());
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Construct a `LedgerEntry` whose key is guaranteed not to collide with any of
/// the 478 hardcoded keys (an Account entry — the hardcoded entries are all
/// ContractData).
fn make_unrelated_entry() -> LedgerEntry {
    use stellar_xdr::curr::{
        AccountEntry, AccountEntryExt, AccountId, LedgerEntryData, LedgerEntryExt, PublicKey,
        SequenceNumber, String32, Thresholds, Uint256, VecM,
    };
    let account = AccountEntry {
        account_id: AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([7u8; 32]))),
        balance: 100,
        seq_num: SequenceNumber(1),
        num_sub_entries: 0,
        inflation_dest: None,
        flags: 0,
        home_domain: String32::default(),
        thresholds: Thresholds([0u8; 4]),
        signers: VecM::default(),
        ext: AccountEntryExt::V0,
    };
    LedgerEntry {
        last_modified_ledger_seq: 1,
        data: LedgerEntryData::Account(account),
        ext: LedgerEntryExt::V0,
    }
}
