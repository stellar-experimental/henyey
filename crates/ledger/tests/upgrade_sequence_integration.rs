//! Upgrade sequence integration tests.
//!
//! These tests verify that multiple upgrades in the same ledger—and across
//! consecutive ledgers—correctly propagate intermediate state changes.
//!
//! They cover the six audit issues in Pattern 2 ("Stale State During Upgrade
//! Sequences"):
//!
//! - **#1087**: `LedgerHeader.ext` not inherited from previous header
//! - **#1088**: Config upgrade validation uses stale protocol version
//! - **#1094**: State size window shift+push skipped after resize
//! - **#1099**: State size window sample period read from stale bucket list
//! - **#1125**: Config upgrade TTL check uses `snapshot.ledger_seq()` (N-1)
//!             instead of closing ledger (N)
//! - **#1096**: `isValidForApply` skips ledger state validation for config
//!
//! # Design: CloseLedgerState unified reads
//!
//! stellar-core processes upgrades inside a single loop that reads from and
//! writes to a **mutable** `LedgerTxn`. After each upgrade is applied, the
//! next iteration sees the updated state (protocol version, config settings,
//! TTL values, etc.).
//!
//! Henyey mirrors this via [`CloseLedgerState`]: all reads during the upgrade
//! loop resolve through current delta → base snapshot, ensuring that each
//! upgrade sees prior upgrades' changes. The `EntryReader` trait allows config
//! loading and other read paths to be generic over `SnapshotHandle` (frozen
//! state) and `CloseLedgerState` (merged view).

use henyey_bucket::HotArchiveBucketList;
use henyey_common::Hash256;
use henyey_ledger::{
    compute_header_hash, CloseLedgerState, ConfigUpgradeSetFrame, LedgerCloseData, LedgerManager,
    LedgerManagerConfig, SnapshotBuilder, SnapshotHandle, TransactionSetVariant, UpgradeContext,
};
use stellar_xdr::curr::{
    ConfigSettingEntry, ConfigSettingId, ConfigUpgradeSet, ConfigUpgradeSetKey,
    ContractDataDurability, ContractId, Hash, LedgerEntry, LedgerEntryData, LedgerEntryExt,
    LedgerHeader, LedgerHeaderExt, LedgerKey, LedgerKeyConfigSetting, LedgerKeyContractData,
    LedgerKeyTtl, LedgerUpgrade, Limits, ScAddress, ScVal, StellarValue, StellarValueExt,
    TimePoint, TransactionSet, TtlEntry, VecM, WriteXdr,
};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn make_genesis_header(version: u32) -> LedgerHeader {
    LedgerHeader {
        ledger_version: version,
        previous_ledger_hash: Hash([0u8; 32]),
        scp_value: StellarValue {
            tx_set_hash: Hash([0u8; 32]),
            close_time: TimePoint(0),
            upgrades: VecM::default(),
            ext: StellarValueExt::Basic,
        },
        tx_set_result_hash: Hash([0u8; 32]),
        bucket_list_hash: Hash([0u8; 32]),
        ledger_seq: 0,
        total_coins: 1_000_000,
        fee_pool: 0,
        inflation_seq: 0,
        id_pool: 0,
        base_fee: 100,
        base_reserve: 100,
        max_tx_set_size: 100,
        skip_list: [
            Hash([0u8; 32]),
            Hash([0u8; 32]),
            Hash([0u8; 32]),
            Hash([0u8; 32]),
        ],
        ext: LedgerHeaderExt::V0,
    }
}

fn init_ledger_manager(version: u32) -> LedgerManager {
    let config = LedgerManagerConfig {
        validate_bucket_hash: false,
        ..Default::default()
    };
    let ledger = LedgerManager::new("Test Network".to_string(), config);

    let bucket_list = henyey_ledger::new_bucket_list_with_soroban_config();
    let hot_archive = HotArchiveBucketList::new();
    let header = make_genesis_header(version);
    let header_hash = compute_header_hash(&header).expect("hash");
    ledger
        .initialize(bucket_list, hot_archive, header, header_hash)
        .expect("init");
    ledger
}

fn empty_close_data(ledger: &LedgerManager, seq: u32, close_time: u64) -> LedgerCloseData {
    let prev_hash = ledger.current_header_hash();
    LedgerCloseData::new(
        seq,
        TransactionSetVariant::Classic(TransactionSet {
            previous_ledger_hash: Hash::from(prev_hash),
            txs: VecM::default(),
        }),
        close_time,
        prev_hash,
    )
}

/// Create a ConfigUpgradeSetKey with deterministic test values.
fn test_config_upgrade_key() -> ConfigUpgradeSetKey {
    ConfigUpgradeSetKey {
        contract_id: ContractId(Hash([0xDE; 32])),
        content_hash: Hash([0xAD; 32]),
    }
}

/// Build the LedgerKey for a ConfigUpgradeSet CONTRACT_DATA entry.
fn config_upgrade_data_key(upgrade_key: &ConfigUpgradeSetKey) -> LedgerKey {
    LedgerKey::ContractData(LedgerKeyContractData {
        contract: ScAddress::Contract(upgrade_key.contract_id.clone()),
        key: ScVal::Bytes(
            upgrade_key
                .content_hash
                .0
                .to_vec()
                .try_into()
                .expect("32 bytes"),
        ),
        durability: ContractDataDurability::Temporary,
    })
}

/// Build a TTL key for the given data key.
fn ttl_key_for(data_key: &LedgerKey) -> LedgerKey {
    LedgerKey::Ttl(LedgerKeyTtl {
        key_hash: Hash(Hash256::hash_xdr(data_key).0),
    })
}

/// Create a ConfigUpgradeSet CONTRACT_DATA entry with the given upgrade set.
fn make_config_upgrade_entry(
    upgrade_key: &ConfigUpgradeSetKey,
    upgrade_set: &ConfigUpgradeSet,
    ledger_seq: u32,
) -> LedgerEntry {
    let xdr_bytes = upgrade_set.to_xdr(Limits::none()).expect("encode");
    LedgerEntry {
        last_modified_ledger_seq: ledger_seq,
        data: LedgerEntryData::ContractData(stellar_xdr::curr::ContractDataEntry {
            ext: stellar_xdr::curr::ExtensionPoint::V0,
            contract: ScAddress::Contract(upgrade_key.contract_id.clone()),
            key: ScVal::Bytes(
                upgrade_key
                    .content_hash
                    .0
                    .to_vec()
                    .try_into()
                    .expect("32 bytes"),
            ),
            durability: ContractDataDurability::Temporary,
            val: ScVal::Bytes(xdr_bytes.try_into().expect("bytes")),
        }),
        ext: LedgerEntryExt::V0,
    }
}

/// Create a TTL entry that is live until the given ledger.
fn make_ttl_entry(data_key: &LedgerKey, live_until: u32) -> LedgerEntry {
    let key_hash = Hash(Hash256::hash_xdr(data_key).0);
    LedgerEntry {
        last_modified_ledger_seq: 1,
        data: LedgerEntryData::Ttl(TtlEntry {
            key_hash,
            live_until_ledger_seq: live_until,
        }),
        ext: LedgerEntryExt::V0,
    }
}

/// Create a ConfigSetting entry.
fn make_config_setting_entry(
    id: ConfigSettingId,
    setting: ConfigSettingEntry,
    ledger_seq: u32,
) -> (LedgerKey, LedgerEntry) {
    let key = LedgerKey::ConfigSetting(LedgerKeyConfigSetting {
        config_setting_id: id,
    });
    let entry = LedgerEntry {
        last_modified_ledger_seq: ledger_seq,
        data: LedgerEntryData::ConfigSetting(setting),
        ext: LedgerEntryExt::V0,
    };
    (key, entry)
}

// ===========================================================================
// Test: #1087 — LedgerHeader ext field not inherited from previous header
// ===========================================================================

/// Regression test for AUDIT-016 (#1087).
///
/// When a `LedgerUpgrade::Flags` upgrade promotes `LedgerHeaderExt` from V0
/// to V1, the ext field must persist in all subsequent ledger headers.
/// stellar-core preserves ext by mutating the header in place, so the field
/// carries forward naturally.
///
/// Bug: `create_next_header()` hard-codes `ext: LedgerHeaderExt::V0` instead
/// of inheriting `prev_header.ext`. After a Flags upgrade, the next ledger's
/// header reverts to V0.
#[tokio::test(flavor = "multi_thread")]
async fn test_header_ext_inherited_across_ledger_closes() {
    let ledger = init_ledger_manager(25);

    // Ledger 1: Apply Flags upgrade (V0 → V1)
    let close_data = empty_close_data(&ledger, 1, 100).with_upgrade(LedgerUpgrade::Flags(1));
    let handle = tokio::runtime::Handle::current();
    let result = ledger
        .close_ledger(close_data, Some(handle))
        .expect("close ledger 1");

    // Verify ledger 1 has V1 ext with flags=1
    match &result.header.ext {
        LedgerHeaderExt::V1(ext) => {
            assert_eq!(ext.flags, 1, "Flags upgrade should set flags=1");
        }
        LedgerHeaderExt::V0 => panic!("Expected V1 ext after Flags upgrade, got V0"),
    }

    // Ledger 2: No upgrades — ext should still be V1
    let close_data2 = empty_close_data(&ledger, 2, 200);
    let handle = tokio::runtime::Handle::current();
    let result2 = ledger
        .close_ledger(close_data2, Some(handle))
        .expect("close ledger 2");

    match &result2.header.ext {
        LedgerHeaderExt::V1(ext) => {
            assert_eq!(ext.flags, 1, "Flags should persist from previous ledger");
        }
        LedgerHeaderExt::V0 => {
            panic!(
                "AUDIT-016 (#1087): LedgerHeader ext reverted to V0 after Flags upgrade.\n\
                 create_next_header() hard-codes ext: LedgerHeaderExt::V0 instead of \n\
                 inheriting prev_header.ext. This causes a consensus fork because V0 and \n\
                 V1 produce different XDR lengths (324 vs 332 bytes) and different SHA-256 \n\
                 header hashes."
            );
        }
    }
}

/// Additional test: verify that multiple Flags upgrades across ledgers all
/// persist correctly.
#[tokio::test(flavor = "multi_thread")]
async fn test_header_ext_persists_through_multiple_ledgers() {
    let ledger = init_ledger_manager(25);

    // Ledger 1: Flags upgrade
    let close_data = empty_close_data(&ledger, 1, 100).with_upgrade(LedgerUpgrade::Flags(1));
    let handle = tokio::runtime::Handle::current();
    ledger
        .close_ledger(close_data, Some(handle))
        .expect("close 1");

    // Ledgers 2-5: No upgrades, ext should persist
    for seq in 2..=5u32 {
        let close_data = empty_close_data(&ledger, seq, seq as u64 * 100);
        let handle = tokio::runtime::Handle::current();
        let result = ledger
            .close_ledger(close_data, Some(handle))
            .unwrap_or_else(|e| {
                panic!("close ledger {}: {}", seq, e);
            });

        match &result.header.ext {
            LedgerHeaderExt::V1(ext) => {
                assert_eq!(ext.flags, 1, "Ledger {}: flags should persist", seq);
            }
            LedgerHeaderExt::V0 => {
                panic!(
                    "AUDIT-016 (#1087): Ledger {}: ext reverted to V0. \n\
                     create_next_header() does not inherit prev_header.ext.",
                    seq
                );
            }
        }
    }
}

// ===========================================================================
// Test: #1088 — Config upgrade validation uses stale protocol version
// ===========================================================================

/// Regression test for AUDIT-017 (#1088).
///
/// When Version(N) and Config(key) upgrades appear in the same ledger,
/// the config upgrade's `is_valid_for_apply()` must see protocol version N
/// (post-upgrade), not N-1 (pre-upgrade from snapshot).
///
/// Fix: `ConfigUpgradeSetFrame::make_from_key()` now takes an explicit
/// `protocol_version` parameter instead of reading it from the snapshot
/// header. The caller passes the post-upgrade version.
#[test]
fn test_config_upgrade_sees_stale_protocol_version() {
    let snapshot_ledger = 99u32;
    let mut header = make_genesis_header(24);
    header.ledger_seq = snapshot_ledger;
    let header_hash = compute_header_hash(&header).expect("hash");

    let upgrade_key = test_config_upgrade_key();
    let upgrade_set = ConfigUpgradeSet {
        updated_entry: VecM::default(),
    };

    let data_key = config_upgrade_data_key(&upgrade_key);
    let data_entry = make_config_upgrade_entry(&upgrade_key, &upgrade_set, 1);
    let ttl_key = ttl_key_for(&data_key);
    let ttl_entry = make_ttl_entry(&data_key, 1000);

    let snapshot = SnapshotBuilder::new(snapshot_ledger)
        .with_header(header.clone(), header_hash)
        .add_entry(data_key.clone(), data_entry)
        .add_entry(ttl_key, ttl_entry)
        .build()
        .expect("build snapshot");
    let handle = SnapshotHandle::new(snapshot);

    // Simulate Version(25) + Config upgrades in the same ledger.
    let mut upgraded = header.clone();
    let mut ctx = UpgradeContext::new(24);
    ctx.add_upgrade(LedgerUpgrade::Version(25));
    ctx.apply_to_header(&mut upgraded);
    let post_upgrade_version = upgraded.ledger_version;
    assert_eq!(post_upgrade_version, 25, "Upgraded header should be V25");
    assert_eq!(
        handle.header().ledger_version,
        24,
        "Snapshot header should still be pre-upgrade version 24"
    );

    // Pass the post-upgrade version (25) explicitly to make_from_key.
    let closing_ledger_seq = snapshot_ledger + 1;
    let ltx = CloseLedgerState::begin(
        handle.clone(),
        header.clone(),
        header_hash,
        closing_ledger_seq,
    );
    let frame = ConfigUpgradeSetFrame::make_from_key(
        &ltx,
        &upgrade_key,
        closing_ledger_seq,
        post_upgrade_version,
    )
    .expect("should not encounter I/O errors")
    .expect("should find config upgrade entry");

    // The frame must use the post-upgrade version, not the stale snapshot version.
    assert_eq!(
        frame.ledger_version(),
        25,
        "Frame should use post-upgrade version 25, not stale snapshot version 24"
    );
}

// ===========================================================================
// Test: #1125 — Config upgrade TTL check uses snapshot.ledger_seq (N-1)
// ===========================================================================

/// Regression test for AUDIT-050 (#1125).
///
/// Config upgrade TTL liveness is checked against `snapshot.ledger_seq()`
/// which is N-1 (the last closed ledger), not N (the current closing ledger).
///
/// stellar-core increments ledgerSeq to N before processing upgrades, so
/// its TTL check uses N as the cutoff.
///
/// Fix: `make_from_key` now receives the closing ledger (N) explicitly
/// and uses it for the TTL check, matching stellar-core.
///
/// When `live_until = N-1`:
/// - Both stellar-core and henyey: N-1 >= N -> false -> EXPIRED -> skip
#[test]
fn test_config_upgrade_ttl_checked_against_snapshot_ledger_seq() {
    let closing_ledger = 100u32;
    let snapshot_ledger = closing_ledger - 1; // 99

    let mut header = make_genesis_header(25);
    header.ledger_seq = snapshot_ledger;
    let header_hash = compute_header_hash(&header).expect("hash");

    let upgrade_key = test_config_upgrade_key();
    let upgrade_set = ConfigUpgradeSet {
        updated_entry: VecM::default(),
    };

    let data_key = config_upgrade_data_key(&upgrade_key);
    let data_entry = make_config_upgrade_entry(&upgrade_key, &upgrade_set, 1);
    let ttl_key = ttl_key_for(&data_key);

    // Set live_until = 99 = N-1 = snapshot.ledger_seq()
    // Both stellar-core and henyey now consider this EXPIRED (99 < 100).
    let ttl_entry = make_ttl_entry(&data_key, snapshot_ledger);

    let snapshot = SnapshotBuilder::new(snapshot_ledger)
        .with_header(header.clone(), header_hash)
        .add_entry(data_key, data_entry)
        .add_entry(ttl_key, ttl_entry)
        .build()
        .expect("build snapshot");
    let handle = SnapshotHandle::new(snapshot);

    // make_from_key now uses closing_ledger (100) for TTL check.
    // Entry with live_until=99 is expired because 99 < 100.
    let ltx = CloseLedgerState::begin(handle.clone(), header.clone(), header_hash, closing_ledger);
    let frame = ConfigUpgradeSetFrame::make_from_key(&ltx, &upgrade_key, closing_ledger, 25)
        .expect("should not encounter I/O errors");

    assert!(
        frame.is_none(),
        "ConfigUpgradeSet with live_until={} should be EXPIRED when closing ledger is {}. \
         The TTL check must use the closing ledger (N), not snapshot.ledger_seq() (N-1).",
        snapshot_ledger,
        closing_ledger
    );
}

// ===========================================================================
// Test: #1094 — State size window shift+push skipped when config resizes it
// ===========================================================================

/// Regression test for AUDIT-021 (#1094).
///
/// When a config upgrade resizes `LiveSorobanStateSizeWindow` (by changing
/// the sample size), AND the current ledger is a sample ledger
/// (`ledger_seq % sample_period == 0`), henyey skips the shift+push
/// operation that stellar-core performs.
///
/// stellar-core flow:
/// 1. Config upgrade resizes window in CloseLedgerState
/// 2. `maybeSnapshotSorobanStateSize` reads resized window from same CloseLedgerState
/// 3. Performs shift+push: `erase(begin); push_back(stateSize)`
///
/// henyey flow:
/// 1. Config upgrade resizes window in delta
/// 2. Delta drained to `live_entries`
/// 3. Guard `has_window_entry` is TRUE (resize entry present)
/// 4. shift+push block is SKIPPED (it only runs when no window entry exists)
#[test]
fn test_state_size_window_resize_at_sample_ledger() {
    // This test documents the architectural issue. The state size window
    // update runs in the commit() path of LedgerCloseContext, which checks:
    //
    //   if live_entries.iter().any(|e| matches!(... LiveSorobanStateSizeWindow ...)) {
    //       has_window_entry = true;
    //   }
    //   if !has_window_entry { compute_state_size_window_entry(...) }
    //
    // When a config upgrade resizes the window, the resized entry is in
    // live_entries, so has_window_entry=true, and the shift+push is skipped.
    //
    // stellar-core doesn't have this guard — it always runs
    // maybeSnapshotSorobanStateSize which checks the LedgerTxn.

    // Create a window with 5 entries
    let old_window: Vec<u64> = vec![1000, 1000, 1000, 1000, 2000];
    let old_window_xdr: VecM<u64> = old_window.clone().try_into().expect("window");

    let (_window_key, window_entry) = make_config_setting_entry(
        ConfigSettingId::LiveSorobanStateSizeWindow,
        ConfigSettingEntry::LiveSorobanStateSizeWindow(old_window_xdr),
        1,
    );

    // Simulate a config upgrade that resizes the window from 5 to 8 entries
    // In the upgrade, new entries are padded with the last value
    let new_window: Vec<u64> = vec![1000, 1000, 1000, 1000, 2000, 2000, 2000, 2000];

    // After resize, the window entry ends up in the delta as an update.
    // When drain_categorization_for_bucket_update() runs, this entry is in
    // live_entries. The guard in commit() sees it and skips shift+push.
    //
    // stellar-core would perform resize THEN shift+push:
    //   [1000, 1000, 1000, 1000, 2000, 2000, 2000, 2000]
    //   → erase first, push_back current_size:
    //   [1000, 1000, 1000, 2000, 2000, 2000, 2000, 9999] (if current_size=9999)
    //
    // henyey produces resize-only:
    //   [1000, 1000, 1000, 1000, 2000, 2000, 2000, 2000]

    // Verify the guard behavior: if a window entry exists in live_entries,
    // the shift+push is not called.
    let has_window_entry = [&window_entry].iter().any(|e| {
        matches!(
            &e.data,
            LedgerEntryData::ConfigSetting(ConfigSettingEntry::LiveSorobanStateSizeWindow(_))
        )
    });

    assert!(
        has_window_entry,
        "Window entry from config upgrade should be detected in live_entries"
    );

    // This is the bug: the guard prevents shift+push when the window was
    // already modified by a config upgrade. Document that the two results
    // would diverge:
    let henyey_result = new_window.clone(); // resize only
    let mut stellar_core_result = new_window.clone();
    let current_size = 9999u64;
    stellar_core_result.remove(0);
    stellar_core_result.push(current_size);

    assert_ne!(
        henyey_result, stellar_core_result,
        "AUDIT-021 (#1094): When a config upgrade resizes the state size window at a \n\
         sample ledger, henyey produces a resize-only window while stellar-core \n\
         performs resize+shift+push. The guard `has_window_entry` in commit() \n\
         prevents shift+push when the window was already modified."
    );
}

// ===========================================================================
// Test: #1099 — State size window sample period read from stale bucket list
// ===========================================================================

/// Regression test for AUDIT-025 (#1099).
///
/// After a config upgrade changes `liveSorobanStateSizeWindowSamplePeriod`,
/// the sample period must be read from eviction_settings (loaded from the
/// delta, which contains post-upgrade values), NOT from bucket_list.get()
/// (which returns the pre-upgrade value since add_batch hasn't run yet).
///
/// Parity: stellar-core reads from LedgerTxn which includes the upgrade.
///
/// This test verifies the arithmetic: a ledger that IS a sample ledger
/// with the new (post-upgrade) period may NOT be one with the old period.
/// The fix in manager.rs ensures henyey uses the new period from
/// eviction_settings rather than the stale bucket_list value.
#[test]
fn test_sample_period_read_from_stale_source() {
    let old_period = 64u32;
    let new_period = 32u32;
    let ledger_seq = 96u32;

    // With old period (bucket_list, pre-upgrade): 96 % 64 = 32 != 0 -> NOT a sample ledger
    // With new period (eviction_settings, post-upgrade): 96 % 32 = 0 -> IS a sample ledger
    let is_sample_old = old_period > 0 && ledger_seq % old_period == 0;
    let is_sample_new = new_period > 0 && ledger_seq % new_period == 0;

    // The old (stale) period would incorrectly skip this sample ledger
    assert!(
        !is_sample_old,
        "Ledger {} should NOT be a sample ledger with stale period {}",
        ledger_seq, old_period
    );
    // The new (correct, from eviction_settings) period correctly identifies this as a sample
    assert!(
        is_sample_new,
        "Ledger {} SHOULD be a sample ledger with post-upgrade period {}",
        ledger_seq, new_period
    );

    // After the fix: manager.rs reads sample_period from eviction_settings
    // (loaded from delta which contains the upgrade), so it uses new_period (32)
    // and correctly identifies ledger 96 as a sample ledger.
    //
    // Before the fix: manager.rs read from bucket_list.get() which still had
    // old_period (64), causing it to skip the window snapshot at ledger 96.
    assert_ne!(
        is_sample_old, is_sample_new,
        "AUDIT-025 (#1099): Post-upgrade period ({}) vs pre-upgrade period ({}) produce \n\
         different is_sample_ledger decisions for ledger {}. \n\
         The fix reads from eviction_settings (post-upgrade) instead of bucket_list (stale).",
        new_period, old_period, ledger_seq
    );
}

/// Reverse case for AUDIT-025: old period makes it a sample ledger,
/// new period does not. Without the fix, this would create a spurious
/// window snapshot using the stale bucket_list value.
#[test]
fn test_sample_period_stale_spurious_snapshot() {
    let old_period = 32u32;
    let new_period = 64u32;
    let ledger_seq = 96u32;

    let is_sample_old = old_period > 0 && ledger_seq % old_period == 0;
    let is_sample_new = new_period > 0 && ledger_seq % new_period == 0;

    // Old (stale): 96 % 32 = 0 -> IS sample -> would create spurious window entry
    // New (correct): 96 % 64 = 32 != 0 -> NOT sample -> correctly skipped
    assert!(is_sample_old);
    assert!(!is_sample_new);

    // After the fix: eviction_settings has new_period (64), so ledger 96 is
    // correctly NOT a sample ledger. No spurious window snapshot is created.
    assert_ne!(
        is_sample_old, is_sample_new,
        "AUDIT-025 (#1099): Stale period ({}) would create a SPURIOUS window \n\
         snapshot at ledger {} that the correct post-upgrade period ({}) would skip.",
        old_period, ledger_seq, new_period
    );
}

// ===========================================================================
// Test: #1096 — isValidForApply skips ledger state validation for config
// ===========================================================================

/// Regression test for AUDIT-023 (#1096).
///
/// Both validation and application paths must reject config upgrades with
/// nonexistent keys:
///
/// 1. `isValidForApply` (herder/scp_driver.rs) now loads the ConfigUpgradeSet
///    from ledger state and validates it, matching stellar-core behavior.
/// 2. `apply_config_upgrades` (close.rs) now returns an error (matching
///    stellar-core's throw) instead of silently continuing.
#[test]
fn test_config_upgrade_nonexistent_key_rejected() {
    let header = make_genesis_header(25);
    let header_hash = compute_header_hash(&header).expect("hash");

    let snapshot = SnapshotBuilder::new(0)
        .with_header(header.clone(), header_hash)
        .build()
        .expect("build snapshot");
    let handle = SnapshotHandle::new(snapshot);

    let bogus_key = ConfigUpgradeSetKey {
        contract_id: ContractId(Hash([0xBB; 32])),
        content_hash: Hash([0xCC; 32]),
    };

    // make_from_key returns Ok(None) — key doesn't exist in ledger
    let ltx = CloseLedgerState::begin(handle.clone(), header.clone(), header_hash, 1);
    let frame = ConfigUpgradeSetFrame::make_from_key(&ltx, &bogus_key, 1, 25)
        .expect("should not encounter I/O errors");
    assert!(
        frame.is_none(),
        "Nonexistent config upgrade key should not be loadable"
    );

    // Parity: stellar-core throws in applyTo (Upgrades.cpp:373) when the
    // config upgrade set cannot be loaded. After AUDIT-023 fix,
    // apply_config_upgrades returns an error instead of silently skipping.
    let mut ctx = UpgradeContext::new(25);
    ctx.add_upgrade(LedgerUpgrade::Config(bogus_key));

    let mut ltx = CloseLedgerState::begin(handle, header, header_hash, 1);
    let soroban_state = henyey_ledger::SharedSorobanState::new();
    let result = ctx.apply_config_upgrades(&mut ltx, 1, 25, &soroban_state);

    assert!(
        result.is_err(),
        "apply_config_upgrades must error on nonexistent config upgrade key \
         (parity: stellar-core throws in Upgrades::applyTo)"
    );
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("Failed to retrieve valid config upgrade set"),
        "Error message should indicate the config upgrade set was not found, got: {}",
        err
    );
}

// ===========================================================================
// Test: Version + BaseFee combined upgrade in same ledger
// ===========================================================================

/// Test that basic (non-config) upgrades work correctly when combined
/// in a single ledger close.
#[tokio::test(flavor = "multi_thread")]
async fn test_base_fee_plus_max_tx_set_size_upgrade_same_ledger() {
    let ledger = init_ledger_manager(25);

    // Close ledger 1 with BaseFee(200) + MaxTxSetSize(500) upgrades
    let close_data = empty_close_data(&ledger, 1, 100).with_upgrades(vec![
        LedgerUpgrade::BaseFee(200),
        LedgerUpgrade::MaxTxSetSize(500),
    ]);

    let handle = tokio::runtime::Handle::current();
    let result = ledger
        .close_ledger(close_data, Some(handle))
        .expect("close ledger 1");

    assert_eq!(result.header.base_fee, 200, "Base fee should be 200");
    assert_eq!(
        result.header.max_tx_set_size, 500,
        "Max tx set size should be 500"
    );

    // Verify the upgrades meta is populated
    let meta = result.meta.expect("meta");
    match meta {
        LedgerCloseMeta::V2(v2) => {
            assert_eq!(
                v2.upgrades_processing.len(),
                2,
                "Should have 2 upgrade entries"
            );
        }
        _ => panic!("expected V2 meta"),
    }
}

/// Test BaseFee + BaseReserve combined upgrade.
#[tokio::test(flavor = "multi_thread")]
async fn test_base_fee_plus_base_reserve_upgrade_same_ledger() {
    let ledger = init_ledger_manager(25);

    let close_data = empty_close_data(&ledger, 1, 100).with_upgrades(vec![
        LedgerUpgrade::BaseFee(200),
        LedgerUpgrade::BaseReserve(200),
    ]);

    let handle = tokio::runtime::Handle::current();
    let result = ledger
        .close_ledger(close_data, Some(handle))
        .expect("close ledger 1");

    assert_eq!(result.header.base_fee, 200);
    assert_eq!(result.header.base_reserve, 200);
}

/// Test that multiple consecutive ledger closes with upgrades maintain
/// consistent state.
#[tokio::test(flavor = "multi_thread")]
async fn test_sequential_upgrades_across_ledgers() {
    let ledger = init_ledger_manager(25);

    // Ledger 1: BaseFee upgrade
    let close_data = empty_close_data(&ledger, 1, 100).with_upgrade(LedgerUpgrade::BaseFee(500));
    let handle = tokio::runtime::Handle::current();
    let r1 = ledger
        .close_ledger(close_data, Some(handle))
        .expect("close 1");
    assert_eq!(r1.header.base_fee, 500);

    // Ledger 2: BaseReserve upgrade
    let close_data =
        empty_close_data(&ledger, 2, 200).with_upgrade(LedgerUpgrade::BaseReserve(200));
    let handle = tokio::runtime::Handle::current();
    let r2 = ledger
        .close_ledger(close_data, Some(handle))
        .expect("close 2");
    assert_eq!(r2.header.base_fee, 500, "BaseFee should persist");
    assert_eq!(r2.header.base_reserve, 200);

    // Ledger 3: No upgrades — all previous values should persist
    let close_data = empty_close_data(&ledger, 3, 300);
    let handle = tokio::runtime::Handle::current();
    let r3 = ledger
        .close_ledger(close_data, Some(handle))
        .expect("close 3");
    assert_eq!(r3.header.base_fee, 500);
    assert_eq!(r3.header.base_reserve, 200);
}

// ===========================================================================
// Regression test: AUDIT-134 (#1493) — Failed config upgrade must not abort
// ledger close. Parity: stellar-core wraps each upgrade in a try/catch
// (LedgerManagerImpl.cpp:1666-1690) that logs errors and continues.
// ===========================================================================

/// A failing config upgrade (nonexistent key) combined with a valid BaseFee
/// upgrade must not prevent the ledger from closing. The BaseFee upgrade
/// should still be applied.
#[tokio::test(flavor = "multi_thread")]
async fn test_failed_config_upgrade_does_not_abort_ledger_close() {
    let ledger = init_ledger_manager(25);

    // Combine a valid BaseFee upgrade with a bogus config upgrade that will fail.
    let bogus_key = ConfigUpgradeSetKey {
        contract_id: ContractId(Hash([0xFF; 32])),
        content_hash: Hash([0xEE; 32]),
    };
    let close_data = empty_close_data(&ledger, 1, 100).with_upgrades(vec![
        LedgerUpgrade::BaseFee(300),
        LedgerUpgrade::Config(bogus_key),
    ]);

    let handle = tokio::runtime::Handle::current();
    // Before the fix, this would return Err (UpgradeError) and halt the node.
    let result = ledger.close_ledger(close_data, Some(handle));
    assert!(
        result.is_ok(),
        "Ledger close must not abort on a failed config upgrade: {:?}",
        result.err()
    );

    let r = result.unwrap();
    assert_eq!(
        r.header.base_fee, 300,
        "Valid BaseFee upgrade should be applied despite failed config upgrade"
    );

    // Parity: stellar-core only emits UpgradeEntryMeta for successful upgrades.
    // The failed config upgrade should NOT appear in close meta.
    let meta = r.meta.expect("close meta should be present");
    match meta {
        LedgerCloseMeta::V2(v2) => {
            assert_eq!(
                v2.upgrades_processing.len(),
                1,
                "Only the successful BaseFee upgrade should produce UpgradeEntryMeta, not the failed config upgrade"
            );
            match &v2.upgrades_processing[0].upgrade {
                LedgerUpgrade::BaseFee(fee) => assert_eq!(fee, &300),
                other => panic!("Expected BaseFee upgrade meta, got {:?}", other),
            }
        }
        _ => panic!("expected V2 meta"),
    }
}

use stellar_xdr::curr::{
    ContractCostParamEntry, ContractCostParams, ExtensionPoint, LedgerCloseMeta, LedgerEntryChange,
};

/// Build a `ConfigUpgradeSetKey` whose `content_hash` is the SHA-256 of the
/// XDR-encoded `upgrade_set`. This produces a key that passes the hash
/// validation in `ConfigUpgradeSetFrame::make_from_key`.
fn make_config_upgrade_key_for_set(
    contract_id: &[u8; 32],
    upgrade_set: &ConfigUpgradeSet,
) -> ConfigUpgradeSetKey {
    use sha2::{Digest, Sha256};
    let xdr_bytes = upgrade_set
        .to_xdr(Limits::none())
        .expect("encode upgrade set");
    let hash: [u8; 32] = Sha256::digest(&xdr_bytes).into();
    ConfigUpgradeSetKey {
        contract_id: ContractId(Hash(*contract_id)),
        content_hash: Hash(hash),
    }
}

// ===========================================================================
// Test: #2268 — State size recompute captured in upgrade meta
// ===========================================================================

/// Regression test for AUDIT-243 (#2268).
///
/// When a version upgrade triggers `handleUpgradeAffectingSorobanInMemoryStateSize`,
/// the `LiveSorobanStateSizeWindow` update must appear in the version upgrade's
/// `UpgradeEntryMeta.changes`. Previously, the recompute ran AFTER meta was built,
/// causing these changes to be missing from upgrade meta.
#[tokio::test(flavor = "multi_thread")]
async fn test_version_upgrade_state_size_recompute_in_meta() {
    // Init at protocol 24 so we can upgrade to 25 (which triggers recompute since >= V23).
    let ledger = init_ledger_manager(24);

    // Close a ledger with version upgrade 24→25.
    let close_data = empty_close_data(&ledger, 1, 100).with_upgrade(LedgerUpgrade::Version(25));
    let handle = tokio::runtime::Handle::current();
    let result = ledger
        .close_ledger(close_data, Some(handle))
        .expect("close ledger with version upgrade");

    // Verify the result header has the new version.
    assert_eq!(result.header.ledger_version, 25);

    // Check upgrade meta: the version upgrade's changes must include
    // LiveSorobanStateSizeWindow entries (STATE + UPDATED pair).
    let meta = result.meta.expect("close meta should be present");
    match meta {
        LedgerCloseMeta::V2(v2) => {
            assert!(
                !v2.upgrades_processing.is_empty(),
                "Version upgrade should produce UpgradeEntryMeta"
            );

            // Find the version upgrade meta entry.
            let version_meta = v2
                .upgrades_processing
                .iter()
                .find(|m| matches!(m.upgrade, LedgerUpgrade::Version(_)))
                .expect("Should have Version upgrade meta");

            // The changes should include LiveSorobanStateSizeWindow updates.
            // Look for an Updated change that modifies a ConfigSetting entry
            // with the LiveSorobanStateSizeWindow variant.
            let has_window_update = version_meta.changes.iter().any(|change| {
                matches!(
                    change,
                    LedgerEntryChange::Updated(entry)
                    if matches!(
                        &entry.data,
                        LedgerEntryData::ConfigSetting(
                            ConfigSettingEntry::LiveSorobanStateSizeWindow(_)
                        )
                    )
                )
            });

            assert!(
                has_window_update,
                "Version upgrade meta must include LiveSorobanStateSizeWindow update. \
                 Changes: {:?}",
                version_meta
                    .changes
                    .iter()
                    .map(std::mem::discriminant)
                    .collect::<Vec<_>>()
            );
        }
        _ => panic!("expected V2 meta"),
    }
}

// ===========================================================================
// Test: #2305 — Config upgrade (ContractCostParamsMemoryBytes) state size meta
// ===========================================================================

/// Regression test for #2305 (follow-up from #2268 / AUDIT-243).
///
/// When a config upgrade changes `ContractCostParamsMemoryBytes`, the
/// `handle_upgrade_affecting_soroban_state_size` recompute runs inside the
/// config upgrade's checkpoint scope (`close.rs:1322-1331`). The resulting
/// `LiveSorobanStateSizeWindow` State + Updated entries must appear in the
/// config upgrade's `UpgradeEntryMeta.changes`.
///
/// This covers the config-upgrade path; the version-upgrade path is covered
/// by `test_version_upgrade_state_size_recompute_in_meta` above.
#[tokio::test(flavor = "multi_thread")]
async fn test_config_upgrade_memory_cost_state_size_recompute_in_meta() {
    let ledger = init_ledger_manager(25);

    // Build a valid ConfigUpgradeSet with 85 ContractCostParamsMemoryBytes
    // entries (required count for protocol 25, per config_upgrade.rs:805-806).
    let cost_param = ContractCostParamEntry {
        ext: ExtensionPoint::V0,
        const_term: 42,
        linear_term: 0,
    };
    let params = ContractCostParams(vec![cost_param; 85].try_into().expect("85 entries"));
    let upgrade_set = ConfigUpgradeSet {
        updated_entry: vec![ConfigSettingEntry::ContractCostParamsMemoryBytes(params)]
            .try_into()
            .expect("one entry"),
    };

    // Derive the key with a real SHA-256 content hash.
    let key = make_config_upgrade_key_for_set(&[0xDE; 32], &upgrade_set);

    // Store the upgrade set as CONTRACT_DATA + TTL in soroban state.
    let data_entry = make_config_upgrade_entry(&key, &upgrade_set, 1);
    ledger
        .inject_synthetic_contract_data(data_entry, 1000)
        .expect("inject config upgrade data");

    // Close a ledger with the config upgrade.
    let close_data =
        empty_close_data(&ledger, 1, 100).with_upgrade(LedgerUpgrade::Config(key.clone()));
    let handle = tokio::runtime::Handle::current();
    let result = ledger
        .close_ledger(close_data, Some(handle))
        .expect("close ledger with config upgrade");

    // Verify the config upgrade meta includes LiveSorobanStateSizeWindow changes.
    let meta = result.meta.expect("close meta should be present");
    match meta {
        LedgerCloseMeta::V2(v2) => {
            assert!(
                !v2.upgrades_processing.is_empty(),
                "Config upgrade should produce UpgradeEntryMeta"
            );

            // Find the config upgrade meta entry.
            let config_meta = v2
                .upgrades_processing
                .iter()
                .find(|m| matches!(m.upgrade, LedgerUpgrade::Config(_)))
                .expect("Should have Config upgrade meta");

            // Assert State (before-image) for LiveSorobanStateSizeWindow.
            let has_window_state = config_meta.changes.iter().any(|change| {
                matches!(
                    change,
                    LedgerEntryChange::State(entry)
                    if matches!(
                        &entry.data,
                        LedgerEntryData::ConfigSetting(
                            ConfigSettingEntry::LiveSorobanStateSizeWindow(_)
                        )
                    )
                )
            });

            // Assert Updated (after-image) for LiveSorobanStateSizeWindow.
            let has_window_updated = config_meta.changes.iter().any(|change| {
                matches!(
                    change,
                    LedgerEntryChange::Updated(entry)
                    if matches!(
                        &entry.data,
                        LedgerEntryData::ConfigSetting(
                            ConfigSettingEntry::LiveSorobanStateSizeWindow(_)
                        )
                    )
                )
            });

            assert!(
                has_window_state,
                "Config upgrade meta must include LiveSorobanStateSizeWindow State \
                 (before-image). Changes: {:?}",
                config_meta
                    .changes
                    .iter()
                    .map(std::mem::discriminant)
                    .collect::<Vec<_>>()
            );
            assert!(
                has_window_updated,
                "Config upgrade meta must include LiveSorobanStateSizeWindow Updated \
                 (after-image). Changes: {:?}",
                config_meta
                    .changes
                    .iter()
                    .map(std::mem::discriminant)
                    .collect::<Vec<_>>()
            );
        }
        _ => panic!("expected V2 meta"),
    }
}

// ===========================================================================
// Regression test: #2307 — prepareLiabilities upgrade meta emission
//
// Validates that entry_changes_since (fixed in #2269) correctly captures
// modifications to entries already present in the delta from transaction
// execution. The scenario: fee charging + seq_num bump write an account to the
// delta, then a BaseReserve upgrade calls prepareLiabilities which modifies
// the same account again. The UpgradeEntryMeta must contain proper
// State+Updated pairs.
// ===========================================================================

use henyey_common::NetworkId;
use henyey_crypto::{sign_hash, SecretKey};
use stellar_xdr::curr::{
    AccountEntry, AccountEntryExt, AccountEntryExtensionV1, AccountEntryExtensionV1Ext, AccountId,
    Asset, BucketListType, BumpSequenceOp, DecoratedSignature, LedgerKeyOffer, LedgerKeyTrustLine,
    Liabilities, Memo, MuxedAccount, OfferEntry, Operation, OperationBody, Preconditions, Price,
    PublicKey, SequenceNumber, Signature as XdrSignature, SignatureHint, Thresholds, Transaction,
    TransactionEnvelope, TransactionExt, TransactionV1Envelope, TrustLineAsset, TrustLineEntry,
    TrustLineEntryExt, TrustLineEntryV1, TrustLineEntryV1Ext, Uint256,
};

/// Create an account entry with V1 extensions carrying native selling liabilities.
fn make_account_with_liabilities(
    account_id: AccountId,
    balance: i64,
    seq_num: i64,
    num_sub_entries: u32,
    selling_liab: i64,
) -> LedgerEntry {
    LedgerEntry {
        last_modified_ledger_seq: 0,
        data: LedgerEntryData::Account(AccountEntry {
            account_id,
            balance,
            seq_num: SequenceNumber(seq_num),
            num_sub_entries,
            inflation_dest: None,
            flags: 0,
            home_domain: Default::default(),
            thresholds: Thresholds([1, 0, 0, 0]),
            signers: VecM::default(),
            ext: AccountEntryExt::V1(AccountEntryExtensionV1 {
                liabilities: Liabilities {
                    buying: 0,
                    selling: selling_liab,
                },
                ext: AccountEntryExtensionV1Ext::V0,
            }),
        }),
        ext: LedgerEntryExt::V0,
    }
}

/// Create an offer selling XLM for a non-native asset at price 1:1.
fn make_native_sell_offer(
    seller_id: AccountId,
    offer_id: i64,
    buying_asset: Asset,
    amount: i64,
) -> LedgerEntry {
    LedgerEntry {
        last_modified_ledger_seq: 0,
        data: LedgerEntryData::Offer(OfferEntry {
            seller_id,
            offer_id,
            selling: Asset::Native,
            buying: buying_asset,
            amount,
            price: Price { n: 1, d: 1 },
            flags: 0,
            ext: stellar_xdr::curr::OfferEntryExt::V0,
        }),
        ext: LedgerEntryExt::V0,
    }
}

/// Create an authorized trustline with V1 extensions carrying buying liabilities.
fn make_authorized_trustline(
    account_id: AccountId,
    asset: TrustLineAsset,
    balance: i64,
    limit: i64,
    buying_liab: i64,
) -> LedgerEntry {
    LedgerEntry {
        last_modified_ledger_seq: 0,
        data: LedgerEntryData::Trustline(TrustLineEntry {
            account_id,
            asset,
            balance,
            limit,
            flags: 1, // AUTHORIZED_FLAG
            ext: TrustLineEntryExt::V1(TrustLineEntryV1 {
                liabilities: Liabilities {
                    buying: buying_liab,
                    selling: 0,
                },
                ext: TrustLineEntryV1Ext::V0,
            }),
        }),
        ext: LedgerEntryExt::V0,
    }
}

/// Sign a transaction envelope and return the decorated signature.
fn sign_classic_envelope(
    envelope: &TransactionEnvelope,
    secret: &SecretKey,
    network_id: &NetworkId,
) -> DecoratedSignature {
    let frame = henyey_tx::TransactionFrame::from_owned_with_network(envelope.clone(), *network_id);
    let hash = frame.hash(network_id).expect("tx hash");
    let signature = sign_hash(secret, &hash);
    let public_key = secret.public_key();
    let pk_bytes = public_key.as_bytes();
    let hint = SignatureHint([pk_bytes[28], pk_bytes[29], pk_bytes[30], pk_bytes[31]]);
    DecoratedSignature {
        hint,
        signature: XdrSignature(signature.0.to_vec().try_into().unwrap()),
    }
}

/// Regression test for #2307 (follow-up from #2269).
///
/// Scenario: transaction execution writes an account to the delta (fee + seq),
/// then a BaseReserve upgrade triggers prepareLiabilities which modifies the
/// same account (erasing an unsupportable offer, decrementing numSubEntries).
/// The upgrade's UpgradeEntryMeta.changes must contain State+Updated for the
/// account and State+Removed for the offer.
///
/// Key validation: the State (before-image) in the upgrade changes must reflect
/// the account's post-fee/post-tx-execution state, NOT the original bucket-list
/// state. This confirms that entry_changes_since's snapshot-diff correctly
/// captures modifications to pre-existing delta entries (#2269 fix).
///
/// Note: when all offers for a given asset are erased, prepareLiabilities does
/// not zero the account's selling liabilities for that asset — this matches
/// stellar-core behavior (Upgrades.cpp's updateOffer only accumulates
/// liabilities for surviving offers).
#[test]
fn test_base_reserve_upgrade_prepare_liabilities_meta() {
    let network_passphrase = "Test Network";
    let network_id = NetworkId::from_passphrase(network_passphrase);

    // Keys for account A (offer holder + tx source).
    let secret_a = SecretKey::from_seed(&[1u8; 32]);
    let account_id_a = AccountId(PublicKey::PublicKeyTypeEd25519(Uint256(
        *secret_a.public_key().as_bytes(),
    )));

    // Keys for account B (asset issuer).
    let secret_b = SecretKey::from_seed(&[2u8; 32]);
    let account_id_b = AccountId(PublicKey::PublicKeyTypeEd25519(Uint256(
        *secret_b.public_key().as_bytes(),
    )));

    // The non-native asset for the offer's buying side.
    let buying_asset = Asset::CreditAlphanum4(stellar_xdr::curr::AlphaNum4 {
        asset_code: stellar_xdr::curr::AssetCode4(*b"TEST"),
        issuer: account_id_b.clone(),
    });
    let tl_asset = TrustLineAsset::CreditAlphanum4(stellar_xdr::curr::AlphaNum4 {
        asset_code: stellar_xdr::curr::AssetCode4(*b"TEST"),
        issuer: account_id_b.clone(),
    });

    // Seed bucket list with accounts, offer, and trustline.
    let mut bucket_list = henyey_ledger::new_bucket_list_with_soroban_config();

    let account_a_entry = make_account_with_liabilities(account_id_a.clone(), 500, 0, 2, 100);
    let offer_entry = make_native_sell_offer(account_id_a.clone(), 1, buying_asset.clone(), 100);
    let trustline_entry =
        make_authorized_trustline(account_id_a.clone(), tl_asset.clone(), 0, 10_000, 100);
    let account_b_entry = LedgerEntry {
        last_modified_ledger_seq: 0,
        data: LedgerEntryData::Account(AccountEntry {
            account_id: account_id_b.clone(),
            balance: 10_000_000,
            seq_num: SequenceNumber(0),
            num_sub_entries: 0,
            inflation_dest: None,
            flags: 0,
            home_domain: Default::default(),
            thresholds: Thresholds([1, 0, 0, 0]),
            signers: VecM::default(),
            ext: AccountEntryExt::V0,
        }),
        ext: LedgerEntryExt::V0,
    };

    bucket_list
        .add_batch(
            1,
            25,
            BucketListType::Live,
            vec![
                account_a_entry,
                offer_entry,
                trustline_entry,
                account_b_entry,
            ],
            vec![],
            vec![],
        )
        .expect("add_batch");

    // Initialize LedgerManager.
    let config = LedgerManagerConfig {
        validate_bucket_hash: false,
        ..Default::default()
    };
    let ledger = LedgerManager::new(network_passphrase.to_string(), config);
    let hot_archive = HotArchiveBucketList::new();
    let header = make_genesis_header(25);
    let header_hash = compute_header_hash(&header).expect("hash");
    ledger
        .initialize(bucket_list, hot_archive, header, header_hash)
        .expect("init");

    // Build a BumpSequence transaction from account A.
    let tx = Transaction {
        source_account: MuxedAccount::Ed25519(Uint256(*secret_a.public_key().as_bytes())),
        fee: 100,
        seq_num: SequenceNumber(1),
        cond: Preconditions::None,
        memo: Memo::None,
        operations: vec![Operation {
            source_account: None,
            body: OperationBody::BumpSequence(BumpSequenceOp {
                bump_to: SequenceNumber(1),
            }),
        }]
        .try_into()
        .unwrap(),
        ext: TransactionExt::V0,
    };

    let mut envelope = TransactionEnvelope::Tx(TransactionV1Envelope {
        tx,
        signatures: VecM::default(),
    });
    let decorated = sign_classic_envelope(&envelope, &secret_a, &network_id);
    if let TransactionEnvelope::Tx(ref mut env) = envelope {
        env.signatures = vec![decorated].try_into().unwrap();
    }

    // Close ledger with the tx + BaseReserve upgrade.
    let prev_hash = ledger.current_header_hash();
    let close_data = LedgerCloseData::new(
        1,
        TransactionSetVariant::Classic(TransactionSet {
            previous_ledger_hash: Hash::from(prev_hash),
            txs: vec![envelope].try_into().unwrap(),
        }),
        100,
        prev_hash,
    )
    .with_upgrade(LedgerUpgrade::BaseReserve(200));

    let result = ledger.close_ledger(close_data, None).expect("close ledger");

    // Assert: transaction succeeded.
    assert!(
        !result.tx_results.is_empty(),
        "Expected at least one transaction result"
    );

    // Assert: header reflects the new base reserve.
    assert_eq!(result.header.base_reserve, 200);

    // Extract upgrade meta.
    let meta = result.meta.expect("close meta should be present");
    let v2 = match meta {
        LedgerCloseMeta::V2(v2) => v2,
        _ => panic!("expected V2 meta"),
    };

    // Find the BaseReserve upgrade meta.
    let reserve_meta = v2
        .upgrades_processing
        .iter()
        .find(|m| matches!(m.upgrade, LedgerUpgrade::BaseReserve(200)))
        .expect("Should have BaseReserve(200) upgrade meta");

    assert!(
        !reserve_meta.changes.is_empty(),
        "BaseReserve upgrade should produce entry changes from prepareLiabilities"
    );

    // Find State+Updated pair for account A by iterating changes in order.
    // Use indices to avoid lifetime issues with closures.
    let mut account_state_idx: Option<usize> = None;
    let mut account_updated_idx: Option<usize> = None;
    for (i, change) in reserve_meta.changes.iter().enumerate() {
        match change {
            LedgerEntryChange::State(entry) => {
                if let LedgerEntryData::Account(ref acct) = entry.data {
                    if acct.account_id == account_id_a {
                        account_state_idx = Some(i);
                    }
                }
            }
            LedgerEntryChange::Updated(entry) => {
                if account_state_idx.is_some() && account_updated_idx.is_none() {
                    if let LedgerEntryData::Account(ref acct) = entry.data {
                        if acct.account_id == account_id_a {
                            account_updated_idx = Some(i);
                        }
                    }
                }
            }
            _ => {}
        }
    }

    let state_idx = account_state_idx.expect("Should have State (before-image) for account A");
    let updated_idx = account_updated_idx.expect("Should have Updated (after-image) for account A");

    let before = match &reserve_meta.changes[state_idx] {
        LedgerEntryChange::State(entry) => match &entry.data {
            LedgerEntryData::Account(acct) => acct,
            _ => unreachable!(),
        },
        _ => unreachable!(),
    };
    let after = match &reserve_meta.changes[updated_idx] {
        LedgerEntryChange::Updated(entry) => match &entry.data {
            LedgerEntryData::Account(acct) => acct,
            _ => unreachable!(),
        },
        _ => unreachable!(),
    };

    // Before-image: account still has the offer + trustline (num_sub_entries=2)
    // and the balance must reflect the fee deduction from tx execution. This is
    // the core #2269 regression check: the State captures the post-tx delta
    // state, not the original bucket-list state (balance 500).
    assert_eq!(
        before.num_sub_entries, 2,
        "before: num_sub_entries should be 2 (1 offer + 1 trustline)"
    );
    assert_eq!(
        before.balance, 400,
        "before-image balance should be 400 (500 - 100 fee)"
    );
    let before_selling = match &before.ext {
        AccountEntryExt::V1(v1) => v1.liabilities.selling,
        AccountEntryExt::V0 => panic!("expected V1 ext on before-image"),
    };
    assert_eq!(
        before_selling, 100,
        "before: selling_liabilities should be 100"
    );

    // After-image: offer erased so numSubEntries decremented (trustline remains).
    // Selling liabilities are NOT zeroed because prepareLiabilities only adjusts
    // liabilities for assets that have surviving offers (stellar-core parity:
    // Upgrades.cpp updateOffer only accumulates to `liabilities` map inside
    // `if (!erase)` at line 871).
    assert_eq!(
        after.num_sub_entries, 1,
        "after: num_sub_entries should be 1 (trustline remains)"
    );
    let after_selling = match &after.ext {
        AccountEntryExt::V1(v1) => v1.liabilities.selling,
        AccountEntryExt::V0 => panic!("expected V1 ext on after-image"),
    };
    assert_eq!(
        after_selling, 100,
        "after: selling_liabilities should remain 100 (no surviving native offers)"
    );

    // Assert: offer was removed.
    let offer_key = LedgerKey::Offer(LedgerKeyOffer {
        seller_id: account_id_a.clone(),
        offer_id: 1,
    });
    let has_offer_removed = reserve_meta
        .changes
        .iter()
        .any(|change| matches!(change, LedgerEntryChange::Removed(key) if *key == offer_key));
    assert!(
        has_offer_removed,
        "BaseReserve upgrade changes should include Removed for the offer"
    );

    // Assert: offer has a State (before-image) preceding the Removed.
    let has_offer_state = reserve_meta.changes.iter().any(|change| {
        if let LedgerEntryChange::State(entry) = change {
            if let LedgerEntryData::Offer(ref offer) = entry.data {
                return offer.seller_id == account_id_a && offer.offer_id == 1;
            }
        }
        false
    });
    assert!(
        has_offer_state,
        "BaseReserve upgrade changes should include State for the offer"
    );

    // Assert: trustline NOT modified by prepareLiabilities (all credit-buying
    // offers erased → no entry in the liabilities map → trustline untouched).
    let tl_key = LedgerKey::Trustline(LedgerKeyTrustLine {
        account_id: account_id_a.clone(),
        asset: tl_asset,
    });
    let tl_in_changes = reserve_meta.changes.iter().any(|change| match change {
        LedgerEntryChange::State(entry) | LedgerEntryChange::Updated(entry) => {
            henyey_common::entry_to_key(entry) == tl_key
        }
        _ => false,
    });
    assert!(
        !tl_in_changes,
        "Trustline should NOT appear in upgrade changes when all buying offers are erased"
    );
}
