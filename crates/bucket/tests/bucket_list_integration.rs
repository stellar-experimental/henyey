//! Integration tests for BucketList matching stellar-core BucketListTests.cpp
//!
//! These tests verify parity with the stellar-core bucket list behavior,
//! particularly for eviction, hot archive operations, and bucket list mechanics.

use henyey_bucket::{
    update_starting_eviction_iterator, BucketEntry, BucketList, EvictionIterator,
    HotArchiveBucketList,
};
use henyey_common::Hash256;
use stellar_xdr::curr::*;

const TEST_PROTOCOL: u32 = 25;

// =============================================================================
// Test Helpers
// =============================================================================

fn make_contract_id(seed: u8) -> Hash {
    let mut bytes = [0u8; 32];
    bytes[0] = seed;
    Hash(bytes)
}

fn make_contract_code_entry(seed: u8, last_modified: u32) -> LedgerEntry {
    LedgerEntry {
        last_modified_ledger_seq: last_modified,
        data: LedgerEntryData::ContractCode(ContractCodeEntry {
            ext: ContractCodeEntryExt::V0,
            hash: make_contract_id(seed),
            code: vec![0u8; 100].try_into().unwrap(),
        }),
        ext: LedgerEntryExt::V0,
    }
}

fn make_contract_code_key(seed: u8) -> LedgerKey {
    LedgerKey::ContractCode(LedgerKeyContractCode {
        hash: make_contract_id(seed),
    })
}

fn make_contract_data_entry(
    seed: u8,
    durability: ContractDataDurability,
    last_modified: u32,
) -> LedgerEntry {
    let mut key_bytes = [0u8; 32];
    key_bytes[0] = seed;

    LedgerEntry {
        last_modified_ledger_seq: last_modified,
        data: LedgerEntryData::ContractData(ContractDataEntry {
            ext: ExtensionPoint::V0,
            contract: ScAddress::Contract(ContractId(Hash(key_bytes))),
            key: ScVal::U64(seed as u64),
            durability,
            val: ScVal::U64(100),
        }),
        ext: LedgerEntryExt::V0,
    }
}

#[allow(dead_code)]
fn make_contract_data_key(seed: u8, durability: ContractDataDurability) -> LedgerKey {
    let mut key_bytes = [0u8; 32];
    key_bytes[0] = seed;

    LedgerKey::ContractData(LedgerKeyContractData {
        contract: ScAddress::Contract(ContractId(Hash(key_bytes))),
        key: ScVal::U64(seed as u64),
        durability,
    })
}

fn make_ttl_entry(key: &LedgerKey, live_until: u32, last_modified: u32) -> LedgerEntry {
    use sha2::{Digest, Sha256};

    let key_bytes = key.to_xdr(Limits::none()).unwrap();
    let mut hasher = Sha256::new();
    hasher.update(&key_bytes);
    let hash = hasher.finalize();
    let mut key_hash = [0u8; 32];
    key_hash.copy_from_slice(&hash);

    LedgerEntry {
        last_modified_ledger_seq: last_modified,
        data: LedgerEntryData::Ttl(TtlEntry {
            key_hash: Hash(key_hash),
            live_until_ledger_seq: live_until,
        }),
        ext: LedgerEntryExt::V0,
    }
}

fn make_account_entry(seed: u8, balance: i64) -> LedgerEntry {
    let mut bytes = [0u8; 32];
    bytes[0] = seed;

    LedgerEntry {
        last_modified_ledger_seq: 1,
        data: LedgerEntryData::Account(AccountEntry {
            account_id: AccountId(PublicKey::PublicKeyTypeEd25519(Uint256(bytes))),
            balance,
            seq_num: SequenceNumber(1),
            num_sub_entries: 0,
            inflation_dest: None,
            flags: 0,
            home_domain: String32::default(),
            thresholds: Thresholds([1, 0, 0, 0]),
            signers: Vec::new().try_into().unwrap(),
            ext: AccountEntryExt::V0,
        }),
        ext: LedgerEntryExt::V0,
    }
}

fn make_account_key(seed: u8) -> LedgerKey {
    let mut bytes = [0u8; 32];
    bytes[0] = seed;

    LedgerKey::Account(LedgerKeyAccount {
        account_id: AccountId(PublicKey::PublicKeyTypeEd25519(Uint256(bytes))),
    })
}

/// Like `make_account_entry` but accepts a u32 seed to avoid key collisions
/// when generating more than 256 unique entries.
fn make_account_entry_wide(seed: u32, balance: i64) -> LedgerEntry {
    let mut bytes = [0u8; 32];
    bytes[0..4].copy_from_slice(&seed.to_be_bytes());

    LedgerEntry {
        last_modified_ledger_seq: 1,
        data: LedgerEntryData::Account(AccountEntry {
            account_id: AccountId(PublicKey::PublicKeyTypeEd25519(Uint256(bytes))),
            balance,
            seq_num: SequenceNumber(1),
            num_sub_entries: 0,
            inflation_dest: None,
            flags: 0,
            home_domain: String32::default(),
            thresholds: Thresholds([1, 0, 0, 0]),
            signers: Vec::new().try_into().unwrap(),
            ext: AccountEntryExt::V0,
        }),
        ext: LedgerEntryExt::V0,
    }
}

fn make_account_key_wide(seed: u32) -> LedgerKey {
    let mut bytes = [0u8; 32];
    bytes[0..4].copy_from_slice(&seed.to_be_bytes());

    LedgerKey::Account(LedgerKeyAccount {
        account_id: AccountId(PublicKey::PublicKeyTypeEd25519(Uint256(bytes))),
    })
}

// =============================================================================
// Hot Archive Integration Tests
// =============================================================================

/// Test matching stellar-core: "hot archive bucket tombstones expire at bottom level"
///
/// This test verifies that when entries are archived and tombstones (Live markers)
/// are added for restoration, the tombstones expire at the bottom level while
/// the archived entries remain.
#[tokio::test(flavor = "multi_thread")]
async fn test_hot_archive_tombstones_expire_at_bottom_level() {
    // Use a smaller depth for faster testing (matching stellar-core testutil::BucketListDepthModifier)
    // We'll manually control the number of levels we test
    let mut bl = HotArchiveBucketList::new();

    // Create some entries to archive
    let mut archived_entries = Vec::new();
    let mut restored_keys = Vec::new();

    for i in 0..5 {
        let entry = make_contract_code_entry(i, 1);
        let key = make_contract_code_key(i);
        archived_entries.push(entry);
        restored_keys.push(key);
    }

    // Add archived entries to the bucket list
    bl.add_batch(
        1,
        TEST_PROTOCOL,
        archived_entries.clone(),
        vec![], // No restorations yet
    )
    .unwrap();

    // Add restoration markers (Live tombstones) for some entries
    let keys_to_restore: Vec<_> = restored_keys.iter().take(2).cloned().collect();
    bl.add_batch(2, TEST_PROTOCOL, vec![], keys_to_restore.clone())
        .unwrap();

    // Continue adding empty batches to push entries through levels
    // This simulates ledger progression
    for ledger in 3..50 {
        bl.add_batch(ledger, TEST_PROTOCOL, vec![], vec![]).unwrap();
    }

    // Verify that archived entries for non-restored keys still exist
    for i in 2..5 {
        let key = make_contract_code_key(i);
        let result = bl.get(&key).unwrap();
        assert!(result.is_some(), "Archived entry {} should exist", i);
    }

    // Verify that restored entries have been shadowed by the Live tombstone
    // (the tombstone indicates restoration, so entry shouldn't be in archive)
    for key in &keys_to_restore {
        let result = bl.get(key).unwrap();
        // After enough merges, restored entries should not be findable in archive
        // because Live tombstones shadow them
        assert!(
            result.is_none(),
            "Restored entry should be shadowed by tombstone"
        );
    }
}

/// Test matching stellar-core: "hot archive accepts multiple archives and restores for same key"
///
/// Simulates: archive V0 → restore → re-archive V1 → verify V1 wins at bottom
#[tokio::test(flavor = "multi_thread")]
async fn test_hot_archive_multiple_archives_and_restores() {
    let mut bl = HotArchiveBucketList::new();

    // Create initial archived entry V0
    let entry_v0 = make_contract_code_entry(1, 1);
    let key = make_contract_code_key(1);

    // Create updated archived entry V1 (same key, different last_modified)
    let entry_v1 = make_contract_code_entry(1, 10);

    // Archive V0
    bl.add_batch(1, TEST_PROTOCOL, vec![entry_v0.clone()], vec![])
        .unwrap();

    // Restore (adds Live tombstone)
    bl.add_batch(2, TEST_PROTOCOL, vec![], vec![key.clone()])
        .unwrap();

    // Re-archive V1
    bl.add_batch(3, TEST_PROTOCOL, vec![entry_v1.clone()], vec![])
        .unwrap();

    // Push through more ledgers to merge everything
    for ledger in 4..100 {
        bl.add_batch(ledger, TEST_PROTOCOL, vec![], vec![]).unwrap();
    }

    // The newest archived version (V1) should ultimately win
    let result = bl.get(&key).unwrap();
    assert!(result.is_some(), "Entry should exist in archive");

    let found = result.unwrap();
    // Check it's V1 (last_modified = 10)
    assert_eq!(
        found.last_modified_ledger_seq, 10,
        "Should have V1 (newest) version"
    );
}

/// Test that archive and restore in the same batch works correctly
#[tokio::test(flavor = "multi_thread")]
async fn test_hot_archive_concurrent_archive_and_restore() {
    let mut bl = HotArchiveBucketList::new();

    // Archive entry 1
    let entry1 = make_contract_code_entry(1, 1);
    let key1 = make_contract_code_key(1);
    bl.add_batch(1, TEST_PROTOCOL, vec![entry1], vec![])
        .unwrap();

    // Archive entry 2 and restore entry 1 in the same batch
    let entry2 = make_contract_code_entry(2, 2);
    let key2 = make_contract_code_key(2);
    bl.add_batch(2, TEST_PROTOCOL, vec![entry2], vec![key1.clone()])
        .unwrap();

    // Entry 1 should be shadowed (was restored)
    let result1 = bl.get(&key1).unwrap();
    assert!(
        result1.is_none(),
        "Entry 1 should be shadowed after restoration"
    );

    // Entry 2 should exist
    let result2 = bl.get(&key2).unwrap();
    assert!(result2.is_some(), "Entry 2 should exist");
}

// =============================================================================
// BucketList Integration Tests
// =============================================================================

/// Test matching stellar-core: "bucket list" basic operations
///
/// Adds batches to bucket list and verifies level sizes stay within bounds.
#[tokio::test(flavor = "multi_thread")]
async fn test_bucket_list_basic_operations() {
    let mut bl = BucketList::new();

    // Add batches and verify bucket sizes stay reasonable
    for i in 1..=130u32 {
        let entries: Vec<_> = (0..8u32)
            .map(|j| make_account_entry_wide(i * 10 + j, 100 * i as i64))
            .collect();

        let dead_keys: Vec<_> = if i > 5 {
            // Delete some old entries
            (0..3u32)
                .map(|j| make_account_key_wide((i - 5) * 10 + j))
                .collect()
        } else {
            vec![]
        };

        bl.add_batch(
            i,
            TEST_PROTOCOL,
            BucketListType::Live,
            entries,
            vec![],
            dead_keys,
        )
        .unwrap();

        // Verify level sizes are bounded
        for level_idx in 0..bl.levels().len() {
            if let Some(level) = bl.level(level_idx) {
                let curr_entries = level.curr.len();
                let snap_entries = level.snap.len();

                // Entry counts should be bounded by level half * some factor
                // (accounting for metadata and test entry sizes)
                let level_half = henyey_bucket::level_half(level_idx as u32);
                assert!(
                    curr_entries <= level_half as usize * 100,
                    "Level {} curr has {} entries, expected <= {}",
                    level_idx,
                    curr_entries,
                    level_half * 100
                );
                assert!(
                    snap_entries <= level_half as usize * 100,
                    "Level {} snap has {} entries, expected <= {}",
                    level_idx,
                    snap_entries,
                    level_half * 100
                );
            }
        }
    }

    // Verify hash is non-zero after adding entries
    assert_ne!(
        bl.hash(),
        Hash256::ZERO,
        "Bucket list hash should be non-zero"
    );
}

/// Test matching stellar-core: "BucketList snap reaches steady state"
///
/// Verifies that after sufficient ledgers, snap bucket sizes stabilize.
#[tokio::test(flavor = "multi_thread")]
async fn test_bucket_list_snap_steady_state() {
    let mut bl = BucketList::new();

    // Add many batches to reach steady state
    for i in 1..=500u32 {
        let entries: Vec<_> = (0..4u32)
            .map(|j| make_account_entry_wide(i * 10 + j, 100))
            .collect();

        bl.add_batch(
            i,
            TEST_PROTOCOL,
            BucketListType::Live,
            entries,
            vec![],
            vec![],
        )
        .unwrap();
    }

    // At steady state, snap buckets should have consistent sizes
    // Level 0 snap should have ~ levelHalf(0) = 2 entries worth
    if let Some(level0) = bl.level(0) {
        let snap_count = level0.snap.len();
        // Snap should have entries (not be empty at steady state)
        assert!(
            snap_count > 0,
            "Level 0 snap should have entries at steady state"
        );
    }
}

/// Test matching stellar-core: "BucketList deepest curr accumulates"
///
/// Verifies that the deepest level's curr bucket accumulates entries.
#[tokio::test(flavor = "multi_thread")]
async fn test_bucket_list_deepest_curr_accumulates() {
    let mut bl = BucketList::new();

    // Add batches to push entries to deeper levels
    for i in 1..=1000u32 {
        let entries: Vec<_> = (0..2u32)
            .map(|j| make_account_entry_wide(i * 10 + j, 100))
            .collect();

        bl.add_batch(
            i,
            TEST_PROTOCOL,
            BucketListType::Live,
            entries,
            vec![],
            vec![],
        )
        .unwrap();
    }

    // Check that entries have propagated to deeper levels
    let mut found_entries_in_deep_level = false;
    for level_idx in 3..bl.levels().len() {
        if let Some(level) = bl.level(level_idx) {
            if level.curr.len() > 0 {
                found_entries_in_deep_level = true;
                break;
            }
        }
    }

    assert!(
        found_entries_in_deep_level,
        "Entries should propagate to deeper levels"
    );
}

/// Test matching stellar-core: "single entry bubbling up"
///
/// Verifies that a single entry bubbles up through all levels correctly.
#[tokio::test(flavor = "multi_thread")]
async fn test_single_entry_bubbling_up() {
    let mut bl = BucketList::new();

    // Add a single entry
    let entry = make_account_entry(1, 1000);
    let key = make_account_key(1);

    bl.add_batch(
        1,
        TEST_PROTOCOL,
        BucketListType::Live,
        vec![entry.clone()],
        vec![],
        vec![],
    )
    .unwrap();

    // Verify entry exists
    let found = bl.get(&key).unwrap();
    assert!(found.is_some(), "Entry should exist after adding");

    // Add many empty batches to push entry through levels
    for i in 2..=5000 {
        bl.add_batch(
            i,
            TEST_PROTOCOL,
            BucketListType::Live,
            vec![],
            vec![],
            vec![],
        )
        .unwrap();

        // Entry should still be findable at every step
        let found = bl.get(&key).unwrap();
        assert!(found.is_some(), "Entry should exist at ledger {}", i);
    }

    // Final verification
    let found = bl.get(&key).unwrap().unwrap();
    if let LedgerEntryData::Account(account) = &found.data {
        assert_eq!(account.balance, 1000, "Entry should have correct balance");
    } else {
        panic!("Expected Account entry");
    }
}

/// Test matching stellar-core: "bucket tombstones mutually-annihilate init entries"
///
/// Verifies CAP-0020 semantics: INIT + DEAD = annihilation
#[tokio::test(flavor = "multi_thread")]
async fn test_init_dead_annihilation() {
    let mut bl = BucketList::new();

    // Add an entry (creates INIT at level 0)
    let entry = make_account_entry(1, 1000);
    let key = make_account_key(1);

    bl.add_batch(
        1,
        TEST_PROTOCOL,
        BucketListType::Live,
        vec![entry],
        vec![],
        vec![],
    )
    .unwrap();

    // Delete the entry (creates DEAD at level 0)
    bl.add_batch(
        2,
        TEST_PROTOCOL,
        BucketListType::Live,
        vec![],
        vec![],
        vec![key.clone()],
    )
    .unwrap();

    // Entry should not be found
    let found = bl.get(&key).unwrap();
    assert!(found.is_none(), "Entry should be annihilated");

    // Push through more ledgers to ensure INIT + DEAD merge and annihilate
    for i in 3..=100 {
        bl.add_batch(
            i,
            TEST_PROTOCOL,
            BucketListType::Live,
            vec![],
            vec![],
            vec![],
        )
        .unwrap();
    }

    // Should still be gone
    let found = bl.get(&key).unwrap();
    assert!(
        found.is_none(),
        "Entry should remain annihilated after merges"
    );

    // Verify live_entries doesn't include the deleted entry
    let live: Vec<_> = bl
        .live_entries_iter()
        .collect::<std::result::Result<Vec<_>, _>>()
        .unwrap();
    let has_key = live.iter().any(|e| {
        if let LedgerEntryData::Account(a) = &e.data {
            a.account_id
                == AccountId(PublicKey::PublicKeyTypeEd25519(Uint256(
                    [1u8; 32].map(|_| 1),
                )))
        } else {
            false
        }
    });
    assert!(!has_key, "Annihilated entry should not be in live_entries");
}

/// Test matching stellar-core: "live bucket tombstones expire at bottom level"
///
/// Verifies that DEAD entries (tombstones) are dropped at the bottom level.
#[tokio::test(flavor = "multi_thread")]
async fn test_tombstones_expire_at_bottom_level() {
    let mut bl = BucketList::new();

    // Add an entry
    let entry = make_account_entry(1, 1000);
    let key = make_account_key(1);

    bl.add_batch(
        1,
        TEST_PROTOCOL,
        BucketListType::Live,
        vec![entry],
        vec![],
        vec![],
    )
    .unwrap();

    // Delete the entry on a later ledger (not same ledger as creation)
    // This means the INIT will be in snap, DEAD in curr, and they won't annihilate immediately
    for i in 2..=10 {
        bl.add_batch(
            i,
            TEST_PROTOCOL,
            BucketListType::Live,
            vec![],
            vec![],
            vec![],
        )
        .unwrap();
    }

    // Now delete
    bl.add_batch(
        11,
        TEST_PROTOCOL,
        BucketListType::Live,
        vec![],
        vec![],
        vec![key.clone()],
    )
    .unwrap();

    // Entry should not be found
    let found = bl.get(&key).unwrap();
    assert!(found.is_none(), "Deleted entry should not be found");

    // Push through many more ledgers to get tombstone to bottom level
    for i in 12..=5000 {
        bl.add_batch(
            i,
            TEST_PROTOCOL,
            BucketListType::Live,
            vec![],
            vec![],
            vec![],
        )
        .unwrap();
    }

    // Entry should still be gone
    let found = bl.get(&key).unwrap();
    assert!(found.is_none(), "Entry should remain deleted");
}

// =============================================================================
// Searchable Snapshot Tests
// =============================================================================

/// Test matching stellar-core: "Searchable BucketListDB snapshots"
///
/// Verifies that bucket list snapshots can be searched correctly.
#[tokio::test(flavor = "multi_thread")]
async fn test_searchable_bucket_list_snapshots() {
    let mut bl = BucketList::new();

    // Add an entry that we'll update multiple times
    let mut entry = make_account_entry(1, 0);
    let key = make_account_key(1);

    // Update entry every 5 ledgers
    for ledger_seq in 1..=100 {
        if (ledger_seq - 1) % 5 == 0 {
            if let LedgerEntryData::Account(ref mut account) = entry.data {
                account.balance += 1;
            }
            entry.last_modified_ledger_seq = ledger_seq;

            bl.add_batch(
                ledger_seq,
                TEST_PROTOCOL,
                BucketListType::Live,
                vec![],
                vec![entry.clone()],
                vec![],
            )
            .unwrap();
        } else {
            bl.add_batch(
                ledger_seq,
                TEST_PROTOCOL,
                BucketListType::Live,
                vec![],
                vec![],
                vec![],
            )
            .unwrap();
        }

        // Entry should always be findable with current value
        let found = bl.get(&key).unwrap().unwrap();
        if let LedgerEntryData::Account(account) = &found.data {
            let expected_balance = ((ledger_seq - 1) / 5 + 1) as i64;
            assert_eq!(
                account.balance, expected_balance,
                "Balance should be {} at ledger {}",
                expected_balance, ledger_seq
            );
        }
    }
}

// =============================================================================
// Eviction Iterator Tests (Integration Level)
// =============================================================================

/// Test that eviction iterator positioning is preserved across add_batch calls
/// when no spill occurs.
#[tokio::test(flavor = "multi_thread")]
async fn test_eviction_iterator_preserved_when_no_spill() {
    // EvictionIterator and update_starting_eviction_iterator are imported at top

    let mut iter = EvictionIterator {
        bucket_list_level: 6,
        is_curr_bucket: true,
        bucket_file_offset: 1000,
    };

    // At ledger 100, level 6 shouldn't spill (spills at multiples of 8192)
    // Note: update_starting_eviction_iterator takes (iter, first_scan_level, ledger_seq)
    let was_reset = update_starting_eviction_iterator(&mut iter, 6, 100);

    assert!(!was_reset, "No reset should occur at ledger 100");
    assert_eq!(iter.bucket_file_offset, 1000, "Offset should be preserved");
    assert_eq!(iter.bucket_list_level, 6, "Level should be preserved");
    assert!(iter.is_curr_bucket, "is_curr_bucket should be preserved");
}

/// Test that eviction iterator resets when a spill occurs at its level.
#[tokio::test(flavor = "multi_thread")]
async fn test_eviction_iterator_resets_on_spill() {
    // EvictionIterator and update_starting_eviction_iterator are imported at top

    let mut iter = EvictionIterator {
        bucket_list_level: 6,
        is_curr_bucket: true,
        bucket_file_offset: 1000,
    };

    // Level 6 curr receives new data when level 5 spills.
    // Level 5 spills at level_half(5) = 2048.
    // update_starting_eviction_iterator checks prev_ledger = ledger_seq - 1,
    // so at ledger_seq = 2049, prev_ledger = 2048 which is a spill boundary for level 5.
    let was_reset = update_starting_eviction_iterator(&mut iter, 6, 2049);

    // A spill occurred, iterator should reset
    assert!(
        was_reset,
        "Reset should occur at ledger 2049 (prev_ledger = 2048 is spill boundary for level 5)"
    );
    assert_eq!(iter.bucket_file_offset, 0, "Offset should reset");
}

// =============================================================================
// Full Eviction Scan Tests (Matching stellar-core "eviction scan" test)
// =============================================================================

/// Test matching stellar-core: "eviction scan" -> "basic eviction test"
///
/// Creates Soroban entries with TTL, advances ledgers until they expire,
/// then verifies they are properly evicted (temporary deleted, persistent archived).
#[tokio::test(flavor = "multi_thread")]
async fn test_eviction_scan_basic() {
    let mut bl = BucketList::new();

    // Create some Soroban entries (ContractCode is persistent)
    let mut entries_with_ttl = Vec::new();
    let current_ledger = 1u32;
    let ttl_expiration = current_ledger + 5; // Entries expire at ledger 6

    for i in 0..5 {
        let code_entry = make_contract_code_entry(i, current_ledger);
        let code_key = make_contract_code_key(i);
        let ttl_entry = make_ttl_entry(&code_key, ttl_expiration, current_ledger);

        entries_with_ttl.push(code_entry);
        entries_with_ttl.push(ttl_entry);
    }

    // Add entries to bucket list
    bl.add_batch(
        current_ledger,
        TEST_PROTOCOL,
        BucketListType::Live,
        entries_with_ttl,
        vec![],
        vec![],
    )
    .unwrap();

    // Verify entries exist before expiration
    for i in 0..5 {
        let key = make_contract_code_key(i);
        let found = bl.get(&key).unwrap();
        assert!(
            found.is_some(),
            "Entry {} should exist before expiration",
            i
        );
    }

    // Advance ledgers (add empty batches)
    for ledger in 2..=5 {
        bl.add_batch(
            ledger,
            TEST_PROTOCOL,
            BucketListType::Live,
            vec![],
            vec![],
            vec![],
        )
        .unwrap();
    }

    // Entries should still exist (not yet expired)
    for i in 0..5 {
        let key = make_contract_code_key(i);
        let found = bl.get(&key).unwrap();
        assert!(
            found.is_some(),
            "Entry {} should still exist at ledger 5",
            i
        );
    }

    // Now perform eviction scan at ledger 7 (after TTL expiration at 6)
    let eviction_ledger = 7;
    let (archived_entries, deleted_keys) = bl.scan_for_eviction(eviction_ledger).unwrap();

    // All 5 ContractCode entries should be archived (they're persistent)
    assert_eq!(
        archived_entries.len(),
        5,
        "Should have 5 archived persistent entries"
    );

    // No temporary entries were created, so no deletions
    assert_eq!(
        deleted_keys.len(),
        0,
        "Should have no deleted keys (no temporary entries)"
    );

    // Verify the archived entries are the ones we created
    for archived in &archived_entries {
        if let LedgerEntryData::ContractCode(code) = &archived.data {
            assert!(
                code.hash.0[0] < 5,
                "Archived entry should be one of our created entries"
            );
        } else {
            panic!("Expected ContractCode entry in archived list");
        }
    }
}

/// Test matching stellar-core: "eviction scan" -> "shadowed entries not evicted"
///
/// Creates entries, updates their TTL before expiration, verifies they're not evicted.
#[tokio::test(flavor = "multi_thread")]
async fn test_eviction_scan_shadowed_entries_not_evicted() {
    let mut bl = BucketList::new();

    let current_ledger = 1u32;
    let initial_ttl = current_ledger + 3; // Initially expires at ledger 4

    // Create a ContractCode entry with short TTL
    let code_entry = make_contract_code_entry(1, current_ledger);
    let code_key = make_contract_code_key(1);
    let ttl_entry = make_ttl_entry(&code_key, initial_ttl, current_ledger);

    bl.add_batch(
        current_ledger,
        TEST_PROTOCOL,
        BucketListType::Live,
        vec![code_entry, ttl_entry],
        vec![],
        vec![],
    )
    .unwrap();

    // Advance to ledger 2
    bl.add_batch(
        2,
        TEST_PROTOCOL,
        BucketListType::Live,
        vec![],
        vec![],
        vec![],
    )
    .unwrap();

    // Update the TTL to extend it (shadow the old TTL)
    let extended_ttl = 20; // Now expires at ledger 20
    let updated_ttl_entry = make_ttl_entry(&code_key, extended_ttl, 2);

    bl.add_batch(
        3,
        TEST_PROTOCOL,
        BucketListType::Live,
        vec![],
        vec![updated_ttl_entry], // Update existing entry
        vec![],
    )
    .unwrap();

    // Perform eviction scan at ledger 10 (after original TTL but before extended TTL)
    let (archived_entries, deleted_keys) = bl.scan_for_eviction(10).unwrap();

    // Entry should NOT be evicted because TTL was extended
    assert_eq!(
        archived_entries.len(),
        0,
        "Entry should not be archived - TTL was extended"
    );
    assert_eq!(deleted_keys.len(), 0, "No entries should be deleted");

    // Verify entry still exists
    let found = bl.get(&code_key).unwrap();
    assert!(
        found.is_some(),
        "Entry should still exist after TTL extension"
    );
}

/// Test incremental eviction scan with settings
#[tokio::test(flavor = "multi_thread")]
async fn test_eviction_scan_incremental() {
    use henyey_bucket::EvictionIterator;
    use stellar_xdr::curr::StateArchivalSettings;

    let mut bl = BucketList::new();

    let current_ledger = 1u32;
    let ttl_expiration = current_ledger + 2;

    // Create entries
    let mut entries = Vec::new();
    for i in 0..10 {
        let code_entry = make_contract_code_entry(i, current_ledger);
        let code_key = make_contract_code_key(i);
        let ttl_entry = make_ttl_entry(&code_key, ttl_expiration, current_ledger);
        entries.push(code_entry);
        entries.push(ttl_entry);
    }

    bl.add_batch(
        current_ledger,
        TEST_PROTOCOL,
        BucketListType::Live,
        entries,
        vec![],
        vec![],
    )
    .unwrap();

    // Create settings for incremental scan
    let settings = StateArchivalSettings {
        starting_eviction_scan_level: 0,
        eviction_scan_size: 10_000,   // Scan 10KB at a time
        max_entries_to_archive: 1000, // Default limit
        ..Default::default()
    };

    // Create iterator starting at level 0
    let iter = EvictionIterator {
        bucket_list_level: 0,
        is_curr_bucket: true,
        bucket_file_offset: 0,
    };

    // Perform incremental scan at ledger 5 (after expiration)
    let result = bl
        .scan_for_eviction_incremental(iter, 5, &settings)
        .unwrap();

    // Should have scanned some bytes
    assert!(result.bytes_scanned > 0, "Should have scanned some bytes");

    // Should have found expired entries
    // Note: The exact count depends on how many fit in the scan size
    // For this test we just verify the mechanism works
    assert!(
        result.candidates.len() > 0 || result.scan_complete,
        "Incremental scan should either find entries or complete the scan"
    );

    // If scan didn't complete in one pass, iterator should have advanced.
    // If scan_complete is true, the iterator may have wrapped back to start.
    if !result.scan_complete {
        assert!(
            result.end_iterator.bucket_file_offset > 0
                || result.end_iterator.bucket_list_level > 0
                || !result.end_iterator.is_curr_bucket,
            "Iterator should have advanced (scan not complete)"
        );
    }
    // scan_complete means we wrapped around all levels, which is fine
}

/// Test that BucketListSnapshot eviction scan produces the same results as
/// BucketList eviction scan. This is the core correctness guarantee for the
/// background eviction optimization: the snapshot-based scan must be identical.
#[tokio::test(flavor = "multi_thread")]
async fn test_snapshot_eviction_scan_matches_bucket_list() {
    use henyey_bucket::{BucketListSnapshot, EvictionIterator};

    let mut bl = BucketList::new();

    let current_ledger = 1u32;
    let ttl_expiration = current_ledger + 2;

    // Create 10 contract code entries with TTLs that expire at ledger 3
    let mut entries = Vec::new();
    for i in 0..10 {
        let code_entry = make_contract_code_entry(i, current_ledger);
        let code_key = make_contract_code_key(i);
        let ttl_entry = make_ttl_entry(&code_key, ttl_expiration, current_ledger);
        entries.push(code_entry);
        entries.push(ttl_entry);
    }

    bl.add_batch(
        current_ledger,
        TEST_PROTOCOL,
        BucketListType::Live,
        entries,
        vec![],
        vec![],
    )
    .unwrap();

    let settings = StateArchivalSettings {
        starting_eviction_scan_level: 0,
        eviction_scan_size: 100_000,
        max_entries_to_archive: 1000,
        ..Default::default()
    };

    let iter = EvictionIterator {
        bucket_list_level: 0,
        is_curr_bucket: true,
        bucket_file_offset: 0,
    };

    let scan_ledger = 5; // After TTL expiration

    // Run scan on the bucket list directly
    let bl_result = bl
        .scan_for_eviction_incremental(iter.clone(), scan_ledger, &settings)
        .unwrap();

    // Take a snapshot and run the same scan on it
    let header = LedgerHeader {
        ledger_version: TEST_PROTOCOL,
        previous_ledger_hash: Hash([0; 32]),
        scp_value: StellarValue {
            tx_set_hash: Hash([0; 32]),
            close_time: TimePoint(0),
            upgrades: vec![].try_into().unwrap(),
            ext: StellarValueExt::Basic,
        },
        tx_set_result_hash: Hash([0; 32]),
        bucket_list_hash: Hash([0; 32]),
        ledger_seq: current_ledger,
        total_coins: 0,
        fee_pool: 0,
        inflation_seq: 0,
        id_pool: 0,
        base_fee: 100,
        base_reserve: 5000000,
        max_tx_set_size: 100,
        skip_list: [Hash([0; 32]), Hash([0; 32]), Hash([0; 32]), Hash([0; 32])],
        ext: LedgerHeaderExt::V0,
    };
    let snapshot = BucketListSnapshot::new(&bl, header);
    let snap_result = snapshot
        .scan_for_eviction_incremental(iter, scan_ledger, &settings)
        .unwrap();

    // Results must be identical
    assert_eq!(
        bl_result.candidates.len(),
        snap_result.candidates.len(),
        "Snapshot and BucketList should find the same number of candidates"
    );
    assert_eq!(
        bl_result.bytes_scanned, snap_result.bytes_scanned,
        "Snapshot and BucketList should scan the same number of bytes"
    );
    assert_eq!(
        bl_result.scan_complete, snap_result.scan_complete,
        "Snapshot and BucketList should agree on scan completion"
    );
    assert_eq!(
        bl_result.end_iterator, snap_result.end_iterator,
        "Snapshot and BucketList should end at the same iterator position"
    );

    // Verify candidates match entry-by-entry
    for (i, (bl_c, snap_c)) in bl_result
        .candidates
        .iter()
        .zip(snap_result.candidates.iter())
        .enumerate()
    {
        assert_eq!(
            bl_c.data_key, snap_c.data_key,
            "Candidate {} data_key mismatch",
            i
        );
        assert_eq!(
            bl_c.ttl_key, snap_c.ttl_key,
            "Candidate {} ttl_key mismatch",
            i
        );
        assert_eq!(
            bl_c.is_temporary, snap_c.is_temporary,
            "Candidate {} is_temporary mismatch",
            i
        );
        assert_eq!(
            bl_c.position, snap_c.position,
            "Candidate {} position mismatch",
            i
        );
    }
}

/// Test that the snapshot eviction scan works correctly on a background thread.
/// This simulates the actual background eviction scan pattern: take a snapshot,
/// send it to another thread, run the scan there, and collect the result.
#[tokio::test(flavor = "multi_thread")]
async fn test_snapshot_eviction_scan_on_background_thread() {
    use henyey_bucket::{BucketListSnapshot, EvictionIterator};

    let mut bl = BucketList::new();

    let current_ledger = 1u32;
    let ttl_expiration = current_ledger + 2;

    let mut entries = Vec::new();
    for i in 0..5 {
        let code_entry = make_contract_code_entry(i, current_ledger);
        let code_key = make_contract_code_key(i);
        let ttl_entry = make_ttl_entry(&code_key, ttl_expiration, current_ledger);
        entries.push(code_entry);
        entries.push(ttl_entry);
    }

    bl.add_batch(
        current_ledger,
        TEST_PROTOCOL,
        BucketListType::Live,
        entries,
        vec![],
        vec![],
    )
    .unwrap();

    let settings = StateArchivalSettings {
        starting_eviction_scan_level: 0,
        eviction_scan_size: 100_000,
        max_entries_to_archive: 1000,
        ..Default::default()
    };

    let iter = EvictionIterator {
        bucket_list_level: 0,
        is_curr_bucket: true,
        bucket_file_offset: 0,
    };

    let target_ledger = 5u32;

    // Take snapshot and run scan on background thread (the actual pattern)
    let header = LedgerHeader {
        ledger_version: TEST_PROTOCOL,
        previous_ledger_hash: Hash([0; 32]),
        scp_value: StellarValue {
            tx_set_hash: Hash([0; 32]),
            close_time: TimePoint(0),
            upgrades: vec![].try_into().unwrap(),
            ext: StellarValueExt::Basic,
        },
        tx_set_result_hash: Hash([0; 32]),
        bucket_list_hash: Hash([0; 32]),
        ledger_seq: current_ledger,
        total_coins: 0,
        fee_pool: 0,
        inflation_seq: 0,
        id_pool: 0,
        base_fee: 100,
        base_reserve: 5000000,
        max_tx_set_size: 100,
        skip_list: [Hash([0; 32]), Hash([0; 32]), Hash([0; 32]), Hash([0; 32])],
        ext: LedgerHeaderExt::V0,
    };
    let snapshot = BucketListSnapshot::new(&bl, header);

    let handle = std::thread::spawn(move || {
        snapshot.scan_for_eviction_incremental(iter, target_ledger, &settings)
    });

    let result = handle.join().expect("thread should not panic").unwrap();

    // Should have found all 5 expired entries
    assert_eq!(
        result.candidates.len(),
        5,
        "Should find 5 expired entries on background thread"
    );
    assert!(result.bytes_scanned > 0);

    // Verify all candidates are persistent contract code entries
    for candidate in &result.candidates {
        assert!(
            !candidate.is_temporary,
            "Contract code entries are persistent"
        );
    }
}

/// Test that the snapshot eviction scan correctly handles shadowed TTL entries.
/// When a TTL is updated (extended), the snapshot must see the latest version.
#[tokio::test(flavor = "multi_thread")]
async fn test_snapshot_eviction_scan_respects_extended_ttl() {
    use henyey_bucket::{BucketListSnapshot, EvictionIterator};

    let mut bl = BucketList::new();

    let current_ledger = 1u32;
    let initial_ttl = current_ledger + 3; // Expires at ledger 4

    // Create entry with short TTL
    let code_entry = make_contract_code_entry(1, current_ledger);
    let code_key = make_contract_code_key(1);
    let ttl_entry = make_ttl_entry(&code_key, initial_ttl, current_ledger);

    bl.add_batch(
        current_ledger,
        TEST_PROTOCOL,
        BucketListType::Live,
        vec![code_entry, ttl_entry],
        vec![],
        vec![],
    )
    .unwrap();

    // Advance and extend the TTL
    bl.add_batch(
        2,
        TEST_PROTOCOL,
        BucketListType::Live,
        vec![],
        vec![],
        vec![],
    )
    .unwrap();

    let extended_ttl = 20; // Now expires at ledger 20
    let updated_ttl = make_ttl_entry(&code_key, extended_ttl, 3);

    bl.add_batch(
        3,
        TEST_PROTOCOL,
        BucketListType::Live,
        vec![],
        vec![updated_ttl],
        vec![],
    )
    .unwrap();

    // Take snapshot AFTER the TTL extension
    let header = LedgerHeader {
        ledger_version: TEST_PROTOCOL,
        previous_ledger_hash: Hash([0; 32]),
        scp_value: StellarValue {
            tx_set_hash: Hash([0; 32]),
            close_time: TimePoint(0),
            upgrades: vec![].try_into().unwrap(),
            ext: StellarValueExt::Basic,
        },
        tx_set_result_hash: Hash([0; 32]),
        bucket_list_hash: Hash([0; 32]),
        ledger_seq: 3,
        total_coins: 0,
        fee_pool: 0,
        inflation_seq: 0,
        id_pool: 0,
        base_fee: 100,
        base_reserve: 5000000,
        max_tx_set_size: 100,
        skip_list: [Hash([0; 32]), Hash([0; 32]), Hash([0; 32]), Hash([0; 32])],
        ext: LedgerHeaderExt::V0,
    };
    let snapshot = BucketListSnapshot::new(&bl, header);

    let settings = StateArchivalSettings {
        starting_eviction_scan_level: 0,
        eviction_scan_size: 100_000,
        max_entries_to_archive: 1000,
        ..Default::default()
    };
    let iter = EvictionIterator {
        bucket_list_level: 0,
        is_curr_bucket: true,
        bucket_file_offset: 0,
    };

    // Scan at ledger 10: after original TTL (4) but before extended TTL (20)
    let result = snapshot
        .scan_for_eviction_incremental(iter.clone(), 10, &settings)
        .unwrap();

    // Entry should NOT be evicted because the snapshot sees the extended TTL
    assert_eq!(
        result.candidates.len(),
        0,
        "Entry should not be a candidate - TTL was extended before snapshot"
    );

    // Now scan at ledger 25: after the extended TTL
    let result2 = snapshot
        .scan_for_eviction_incremental(iter, 25, &settings)
        .unwrap();

    // Entry SHOULD be evicted now
    assert_eq!(
        result2.candidates.len(),
        1,
        "Entry should be a candidate after extended TTL expires"
    );
}

/// Test that snapshot eviction scan handles temporary entries correctly
/// (they should be marked as temporary, not persistent).
#[tokio::test(flavor = "multi_thread")]
async fn test_snapshot_eviction_scan_temporary_entries() {
    use henyey_bucket::{BucketListSnapshot, EvictionIterator};

    let mut bl = BucketList::new();

    let current_ledger = 1u32;
    let ttl_expiration = current_ledger + 2;

    // Create temporary contract data entries
    let mut entries = Vec::new();
    for i in 0..3 {
        let data_entry =
            make_contract_data_entry(i, ContractDataDurability::Temporary, current_ledger);
        let data_key = make_contract_data_key(i, ContractDataDurability::Temporary);
        let ttl_entry = make_ttl_entry(&data_key, ttl_expiration, current_ledger);
        entries.push(data_entry);
        entries.push(ttl_entry);
    }

    // Also add 2 persistent entries
    for i in 10..12 {
        let code_entry = make_contract_code_entry(i, current_ledger);
        let code_key = make_contract_code_key(i);
        let ttl_entry = make_ttl_entry(&code_key, ttl_expiration, current_ledger);
        entries.push(code_entry);
        entries.push(ttl_entry);
    }

    bl.add_batch(
        current_ledger,
        TEST_PROTOCOL,
        BucketListType::Live,
        entries,
        vec![],
        vec![],
    )
    .unwrap();

    let header = LedgerHeader {
        ledger_version: TEST_PROTOCOL,
        previous_ledger_hash: Hash([0; 32]),
        scp_value: StellarValue {
            tx_set_hash: Hash([0; 32]),
            close_time: TimePoint(0),
            upgrades: vec![].try_into().unwrap(),
            ext: StellarValueExt::Basic,
        },
        tx_set_result_hash: Hash([0; 32]),
        bucket_list_hash: Hash([0; 32]),
        ledger_seq: current_ledger,
        total_coins: 0,
        fee_pool: 0,
        inflation_seq: 0,
        id_pool: 0,
        base_fee: 100,
        base_reserve: 5000000,
        max_tx_set_size: 100,
        skip_list: [Hash([0; 32]), Hash([0; 32]), Hash([0; 32]), Hash([0; 32])],
        ext: LedgerHeaderExt::V0,
    };
    let snapshot = BucketListSnapshot::new(&bl, header);

    let settings = StateArchivalSettings {
        starting_eviction_scan_level: 0,
        eviction_scan_size: 100_000,
        max_entries_to_archive: 1000,
        ..Default::default()
    };
    let iter = EvictionIterator {
        bucket_list_level: 0,
        is_curr_bucket: true,
        bucket_file_offset: 0,
    };

    let result = snapshot
        .scan_for_eviction_incremental(iter, 5, &settings)
        .unwrap();

    // Should find all 5 expired entries (3 temporary + 2 persistent)
    assert_eq!(
        result.candidates.len(),
        5,
        "Should find all 5 expired entries"
    );

    let temp_count = result.candidates.iter().filter(|c| c.is_temporary).count();
    let persistent_count = result.candidates.iter().filter(|c| !c.is_temporary).count();

    assert_eq!(temp_count, 3, "Should have 3 temporary candidates");
    assert_eq!(persistent_count, 2, "Should have 2 persistent candidates");
}

// =============================================================================
// BucketManager Persistence Tests
// =============================================================================

/// Test matching stellar-core: BucketManager persistence across restart
///
/// Creates a BucketManager, adds buckets, closes it, reopens it, and verifies
/// the state is preserved.
#[tokio::test(flavor = "multi_thread")]
async fn test_bucket_manager_persistence() {
    use henyey_bucket::{BucketEntry, BucketManager};
    use tempfile::tempdir;

    let temp_dir = tempdir().unwrap();
    let bucket_dir = temp_dir.path().to_path_buf();

    // Create unique entries for testing
    let entries: Vec<_> = (0..10)
        .map(|i| make_account_entry(i, 1000 + i as i64))
        .collect();

    let bucket_hash;

    // Phase 1: Create manager, add bucket, save
    {
        let manager = BucketManager::new(bucket_dir.clone()).unwrap();

        // Create a bucket from entries (create_bucket handles saving to disk)
        let bucket_entries: Vec<BucketEntry> = entries
            .iter()
            .map(|e| BucketEntry::Initentry(e.clone()))
            .collect();

        let bucket = manager.create_bucket(bucket_entries).unwrap();
        bucket_hash = bucket.hash();

        // Verify bucket exists
        assert!(
            manager.bucket_exists(&bucket_hash),
            "Bucket should exist after create"
        );
    }

    // Phase 2: Reopen manager, verify bucket persists
    {
        let manager = BucketManager::new(bucket_dir.clone()).unwrap();

        // Bucket should still exist
        assert!(
            manager.bucket_exists(&bucket_hash),
            "Bucket should persist across manager restart"
        );

        // Load and verify the bucket
        let loaded_bucket = manager.load_bucket(&bucket_hash).unwrap();
        assert_eq!(
            loaded_bucket.hash(),
            bucket_hash,
            "Loaded bucket hash should match"
        );
        assert_eq!(
            loaded_bucket.len(),
            10,
            "Loaded bucket should have 10 entries"
        );

        // Verify entry contents
        for i in 0..10 {
            let key = make_account_key(i);
            let found = loaded_bucket.get(&key).unwrap();
            assert!(found.is_some(), "Entry {} should exist in loaded bucket", i);

            if let Some(BucketEntry::Initentry(entry)) = found {
                if let LedgerEntryData::Account(account) = &entry.data {
                    assert_eq!(
                        account.balance,
                        1000 + i as i64,
                        "Entry {} should have correct balance",
                        i
                    );
                }
            }
        }
    }

    // Cleanup is automatic via tempdir drop
}

/// Test BucketManager can load buckets by hash
#[tokio::test(flavor = "multi_thread")]
async fn test_bucket_manager_load_by_hash() {
    use henyey_bucket::{BucketEntry, BucketManager};
    use tempfile::tempdir;

    let temp_dir = tempdir().unwrap();
    let bucket_dir = temp_dir.path().to_path_buf();

    let manager = BucketManager::new(bucket_dir).unwrap();

    // Create multiple buckets
    let mut bucket_hashes = Vec::new();

    for batch in 0..3 {
        let entries: Vec<_> = (0..5)
            .map(|i| {
                let seed = (batch * 10 + i) as u8;
                make_account_entry(seed, 100 * (batch + 1) as i64)
            })
            .collect();

        let bucket_entries: Vec<BucketEntry> = entries
            .iter()
            .map(|e| BucketEntry::Initentry(e.clone()))
            .collect();

        let bucket = manager.create_bucket(bucket_entries).unwrap();
        bucket_hashes.push(bucket.hash());
    }

    // Verify all buckets can be loaded
    for (idx, hash) in bucket_hashes.iter().enumerate() {
        assert!(manager.bucket_exists(hash), "Bucket {} should exist", idx);

        let loaded = manager.load_bucket(hash).unwrap();
        assert_eq!(loaded.hash(), *hash, "Bucket {} hash should match", idx);
        assert_eq!(loaded.len(), 5, "Bucket {} should have 5 entries", idx);
    }

    // Verify non-existent bucket returns error or None
    let fake_hash = Hash256::from_bytes([99u8; 32]);
    assert!(
        !manager.bucket_exists(&fake_hash),
        "Fake bucket should not exist"
    );
}

/// Test BucketManager handles empty buckets correctly
#[tokio::test(flavor = "multi_thread")]
async fn test_bucket_manager_empty_bucket() {
    use henyey_bucket::{Bucket, BucketManager};
    use tempfile::tempdir;

    let temp_dir = tempdir().unwrap();
    let bucket_dir = temp_dir.path().to_path_buf();

    let manager = BucketManager::new(bucket_dir).unwrap();

    // Create an empty bucket using create_bucket with empty entries
    let empty_bucket = manager.create_bucket(vec![]).unwrap();
    let empty_hash = empty_bucket.hash();

    // Empty bucket hash should be zero
    assert_eq!(
        empty_hash,
        Hash256::ZERO,
        "Empty bucket should have zero hash"
    );

    // Empty bucket check should work
    assert!(
        empty_bucket.is_empty(),
        "Empty bucket should report as empty"
    );

    // Also test Bucket::empty() directly
    let direct_empty = Bucket::empty();
    assert_eq!(
        direct_empty.hash(),
        Hash256::ZERO,
        "Direct empty bucket should have zero hash"
    );
    assert!(
        direct_empty.is_empty(),
        "Direct empty bucket should report as empty"
    );
}

/// Test bucket verification
#[tokio::test(flavor = "multi_thread")]
async fn test_bucket_manager_verify_buckets() {
    use henyey_bucket::{BucketEntry, BucketManager};
    use tempfile::tempdir;

    let temp_dir = tempdir().unwrap();
    let bucket_dir = temp_dir.path().to_path_buf();

    let manager = BucketManager::new(bucket_dir).unwrap();

    // Create and save a bucket
    let entries: Vec<_> = (0..5).map(|i| make_account_entry(i, 500)).collect();

    let bucket_entries: Vec<BucketEntry> = entries
        .iter()
        .map(|e| BucketEntry::Initentry(e.clone()))
        .collect();

    let bucket = manager.create_bucket(bucket_entries).unwrap();
    let hash = bucket.hash();

    // Verify the bucket using verify_bucket_hashes (plural)
    let mismatches = manager.verify_bucket_hashes(&[hash]).unwrap();
    assert!(
        mismatches.is_empty(),
        "Bucket hash verification should find no mismatches"
    );

    // Also verify bucket exists
    assert!(
        manager.bucket_exists(&hash),
        "Bucket should exist after verification"
    );
}

// =============================================================================
// scan_for_entries_of_types Tests
// =============================================================================

fn make_offer_entry(seed: u8, offer_id: i64) -> LedgerEntry {
    let mut bytes = [0u8; 32];
    bytes[0] = seed;

    LedgerEntry {
        last_modified_ledger_seq: 1,
        data: LedgerEntryData::Offer(OfferEntry {
            seller_id: AccountId(PublicKey::PublicKeyTypeEd25519(Uint256(bytes))),
            offer_id,
            selling: Asset::Native,
            buying: Asset::Native,
            amount: 1000,
            price: Price { n: 1, d: 1 },
            flags: 0,
            ext: OfferEntryExt::V0,
        }),
        ext: LedgerEntryExt::V0,
    }
}

#[allow(dead_code)]
fn make_config_setting_entry(_id: ConfigSettingId) -> LedgerEntry {
    LedgerEntry {
        last_modified_ledger_seq: 1,
        data: LedgerEntryData::ConfigSetting(ConfigSettingEntry::ContractMaxSizeBytes(16384)),
        ext: LedgerEntryExt::V0,
    }
}

/// Test that scan_for_entries_of_types returns entries matching multiple types
/// in a single pass and correctly deduplicates across all types.
#[tokio::test(flavor = "multi_thread")]
async fn test_scan_for_entries_of_types_basic() {
    let mut bl = BucketList::new();

    // Add mixed entry types in a single batch
    let entries = vec![
        make_account_entry(1, 1000),
        make_offer_entry(2, 100),
        make_contract_code_entry(3, 1),
        make_contract_data_entry(4, ContractDataDurability::Persistent, 1),
    ];

    bl.add_batch(
        1,
        TEST_PROTOCOL,
        BucketListType::Live,
        entries,
        vec![],
        vec![],
    )
    .unwrap();

    // Scan for Offer + ContractCode — should find exactly 2 entries
    let mut found_types = Vec::new();
    bl.scan_for_entries_of_types(
        &[LedgerEntryType::Offer, LedgerEntryType::ContractCode],
        |be| {
            if let BucketEntry::Liveentry(entry) | BucketEntry::Initentry(entry) = be {
                match &entry.data {
                    LedgerEntryData::Offer(_) => found_types.push("Offer"),
                    LedgerEntryData::ContractCode(_) => found_types.push("ContractCode"),
                    other => panic!("Unexpected entry type: {:?}", std::mem::discriminant(other)),
                }
            }
            true
        },
    )
    .unwrap();

    assert_eq!(found_types.len(), 2, "Should find exactly 2 entries");
    assert!(found_types.contains(&"Offer"), "Should find Offer");
    assert!(
        found_types.contains(&"ContractCode"),
        "Should find ContractCode"
    );
}

/// Test that scan_for_entries_of_types deduplicates entries that appear in
/// multiple levels (newer level shadows older level).
#[tokio::test(flavor = "multi_thread")]
async fn test_scan_for_entries_of_types_deduplication() {
    let mut bl = BucketList::new();

    // Ledger 1: add an offer and an account
    bl.add_batch(
        1,
        TEST_PROTOCOL,
        BucketListType::Live,
        vec![make_offer_entry(1, 100), make_account_entry(10, 500)],
        vec![],
        vec![],
    )
    .unwrap();

    // Ledger 2: update the same offer (same key, new balance) + add contract code
    bl.add_batch(
        2,
        TEST_PROTOCOL,
        BucketListType::Live,
        vec![make_contract_code_entry(5, 2)],
        vec![make_offer_entry(1, 100)], // update with same offer_id
        vec![],
    )
    .unwrap();

    // Scan for Offer + ContractCode — the offer should appear exactly once (deduped)
    let mut offer_count = 0u32;
    let mut code_count = 0u32;
    bl.scan_for_entries_of_types(
        &[LedgerEntryType::Offer, LedgerEntryType::ContractCode],
        |be| {
            if let BucketEntry::Liveentry(entry) | BucketEntry::Initentry(entry) = be {
                match &entry.data {
                    LedgerEntryData::Offer(_) => offer_count += 1,
                    LedgerEntryData::ContractCode(_) => code_count += 1,
                    _ => {}
                }
            }
            true
        },
    )
    .unwrap();

    assert_eq!(offer_count, 1, "Offer should be deduped to 1");
    assert_eq!(code_count, 1, "ContractCode should appear once");
}

/// Test that scan_for_entries_of_types excludes dead entries and
/// types not in the requested set.
#[tokio::test(flavor = "multi_thread")]
async fn test_scan_for_entries_of_types_excludes_dead_and_unmatched() {
    let mut bl = BucketList::new();

    // Ledger 1: add entries of multiple types
    bl.add_batch(
        1,
        TEST_PROTOCOL,
        BucketListType::Live,
        vec![
            make_offer_entry(1, 100),
            make_account_entry(2, 1000),
            make_contract_code_entry(3, 1),
        ],
        vec![],
        vec![],
    )
    .unwrap();

    // Ledger 2: delete the offer
    let offer_key = LedgerKey::Offer(LedgerKeyOffer {
        seller_id: {
            let mut bytes = [0u8; 32];
            bytes[0] = 1;
            AccountId(PublicKey::PublicKeyTypeEd25519(Uint256(bytes)))
        },
        offer_id: 100,
    });
    bl.add_batch(
        2,
        TEST_PROTOCOL,
        BucketListType::Live,
        vec![],
        vec![],
        vec![offer_key],
    )
    .unwrap();

    // Scan for Offer + Account — dead offer should NOT appear, account should
    let mut found = Vec::new();
    bl.scan_for_entries_of_types(&[LedgerEntryType::Offer, LedgerEntryType::Account], |be| {
        if let BucketEntry::Liveentry(entry) | BucketEntry::Initentry(entry) = be {
            match &entry.data {
                LedgerEntryData::Offer(_) => found.push("Offer"),
                LedgerEntryData::Account(_) => found.push("Account"),
                _ => {}
            }
        }
        true
    })
    .unwrap();

    assert_eq!(
        found,
        vec!["Account"],
        "Only Account should remain (offer is dead)"
    );
}

/// Test that scan_for_entries_of_types with a single type produces the same
/// results as scan_for_entries_of_type.
#[tokio::test(flavor = "multi_thread")]
async fn test_scan_for_entries_of_types_matches_single_type_variant() {
    let mut bl = BucketList::new();

    // Add a mix of entries across multiple ledgers
    for i in 1u8..=10 {
        bl.add_batch(
            i as u32,
            TEST_PROTOCOL,
            BucketListType::Live,
            vec![
                make_account_entry(i * 10, 1000),
                make_offer_entry(i * 10 + 1, i as i64 * 100),
                make_contract_code_entry(i * 10 + 2, i as u32),
            ],
            vec![],
            vec![],
        )
        .unwrap();
    }

    // Collect results from single-type scan
    let mut single_type_offers = Vec::new();
    bl.scan_for_entries_of_type(LedgerEntryType::Offer, |be| {
        if let BucketEntry::Liveentry(entry) | BucketEntry::Initentry(entry) = be {
            if let LedgerEntryData::Offer(ref offer) = entry.data {
                single_type_offers.push(offer.offer_id);
            }
        }
        true
    })
    .unwrap();

    // Collect results from multi-type scan (with only Offer)
    let mut multi_type_offers = Vec::new();
    bl.scan_for_entries_of_types(&[LedgerEntryType::Offer], |be| {
        if let BucketEntry::Liveentry(entry) | BucketEntry::Initentry(entry) = be {
            if let LedgerEntryData::Offer(ref offer) = entry.data {
                multi_type_offers.push(offer.offer_id);
            }
        }
        true
    })
    .unwrap();

    single_type_offers.sort();
    multi_type_offers.sort();

    assert_eq!(
        single_type_offers, multi_type_offers,
        "Multi-type scan with single type should match single-type scan"
    );
}

/// Test that scan_for_entries_of_types supports early termination via callback.
#[tokio::test(flavor = "multi_thread")]
async fn test_scan_for_entries_of_types_early_termination() {
    let mut bl = BucketList::new();

    bl.add_batch(
        1,
        TEST_PROTOCOL,
        BucketListType::Live,
        vec![
            make_offer_entry(1, 100),
            make_offer_entry(2, 200),
            make_offer_entry(3, 300),
            make_contract_code_entry(4, 1),
        ],
        vec![],
        vec![],
    )
    .unwrap();

    // Stop after finding the first entry
    let mut count = 0u32;
    let completed = bl
        .scan_for_entries_of_types(
            &[LedgerEntryType::Offer, LedgerEntryType::ContractCode],
            |_be| {
                count += 1;
                false // stop immediately
            },
        )
        .unwrap();

    assert!(!completed, "Should return false when stopped early");
    assert_eq!(
        count, 1,
        "Should have processed exactly 1 entry before stopping"
    );
}

/// Test scan_for_entries_of_types with all four Soroban types combined,
/// matching the pattern used in initialize_all_caches_parallel.
#[tokio::test(flavor = "multi_thread")]
async fn test_scan_for_entries_of_types_soroban_combined() {
    let mut bl = BucketList::new();

    let code_entry = make_contract_code_entry(1, 1);
    let data_entry = make_contract_data_entry(2, ContractDataDurability::Persistent, 1);
    let code_key = make_contract_code_key(1);
    let ttl_entry = make_ttl_entry(&code_key, 1000, 1);

    bl.add_batch(
        1,
        TEST_PROTOCOL,
        BucketListType::Live,
        vec![
            code_entry,
            data_entry,
            ttl_entry,
            make_account_entry(10, 5000), // should be excluded
            make_offer_entry(11, 500),    // should be excluded
        ],
        vec![],
        vec![],
    )
    .unwrap();

    let mut code_count = 0u32;
    let mut data_count = 0u32;
    let mut ttl_count = 0u32;
    let mut config_count = 0u32;
    let mut other_count = 0u32;

    bl.scan_for_entries_of_types(
        &[
            LedgerEntryType::ContractCode,
            LedgerEntryType::ContractData,
            LedgerEntryType::Ttl,
            LedgerEntryType::ConfigSetting,
        ],
        |be| {
            if let BucketEntry::Liveentry(entry) | BucketEntry::Initentry(entry) = be {
                match &entry.data {
                    LedgerEntryData::ContractCode(_) => code_count += 1,
                    LedgerEntryData::ContractData(_) => data_count += 1,
                    LedgerEntryData::Ttl(_) => ttl_count += 1,
                    LedgerEntryData::ConfigSetting(_) => config_count += 1,
                    _ => other_count += 1,
                }
            }
            true
        },
    )
    .unwrap();

    assert_eq!(code_count, 1, "Should find 1 ContractCode");
    assert_eq!(data_count, 1, "Should find 1 ContractData");
    assert_eq!(ttl_count, 1, "Should find 1 TTL");
    assert_eq!(config_count, 0, "Should find 0 ConfigSettings (none added)");
    assert_eq!(other_count, 0, "Should not find Account or Offer entries");
}

// =============================================================================
// AUDIT-152: Zero-budget eviction scan
// =============================================================================

#[tokio::test]
async fn test_eviction_scan_zero_budget_does_not_advance_iterator() {
    use henyey_bucket::EvictionIterator;
    use stellar_xdr::curr::StateArchivalSettings;

    let mut bl = BucketList::new();

    let current_ledger = 1u32;
    let ttl_expiration = current_ledger + 2;

    // Create entries so the bucket is non-empty
    let mut entries = Vec::new();
    for i in 0..5 {
        let code_entry = make_contract_code_entry(i, current_ledger);
        let code_key = make_contract_code_key(i);
        let ttl_entry = make_ttl_entry(&code_key, ttl_expiration, current_ledger);
        entries.push(code_entry);
        entries.push(ttl_entry);
    }

    bl.add_batch(
        current_ledger,
        TEST_PROTOCOL,
        BucketListType::Live,
        entries,
        vec![],
        vec![],
    )
    .unwrap();

    // eviction_scan_size = 0 means zero budget
    let settings = StateArchivalSettings {
        starting_eviction_scan_level: 0,
        eviction_scan_size: 0,
        max_entries_to_archive: 1000,
        ..Default::default()
    };

    let start_iter = EvictionIterator {
        bucket_list_level: 0,
        is_curr_bucket: true,
        bucket_file_offset: 0,
    };

    // Scan at a ledger past expiration
    let result = bl
        .scan_for_eviction_incremental(start_iter.clone(), 100, &settings)
        .unwrap();

    // stellar-core returns immediately with no work when bytesToScan == 0
    assert!(
        result.candidates.is_empty(),
        "Zero budget must produce no eviction candidates"
    );
    assert_eq!(result.bytes_scanned, 0, "Zero budget must scan zero bytes");
    assert_eq!(
        result.end_iterator.bucket_file_offset, start_iter.bucket_file_offset,
        "Zero budget must not advance the eviction iterator"
    );
    assert_eq!(
        result.end_iterator.bucket_list_level, start_iter.bucket_list_level,
        "Zero budget must not change the iterator level"
    );
    assert_eq!(
        result.end_iterator.is_curr_bucket, start_iter.is_curr_bucket,
        "Zero budget must not change the iterator bucket selector"
    );
}
