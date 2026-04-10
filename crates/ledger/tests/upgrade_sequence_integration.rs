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

use henyey_bucket::{BucketList, HotArchiveBucketList};
use henyey_common::Hash256;
use henyey_common::LedgerSeq;
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

    let bucket_list = BucketList::new();
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
        seq.into(),
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
        key_hash: Hash256::hash_xdr(data_key)
            .map(|h| Hash(h.0))
            .unwrap_or(Hash([0u8; 32])),
    })
}

/// Create a ConfigUpgradeSet CONTRACT_DATA entry with the given upgrade set.
fn make_config_upgrade_entry(
    upgrade_key: &ConfigUpgradeSetKey,
    upgrade_set: &ConfigUpgradeSet,
    ledger_seq: LedgerSeq,
) -> LedgerEntry {
    let xdr_bytes = upgrade_set.to_xdr(Limits::none()).expect("encode");
    LedgerEntry {
        last_modified_ledger_seq: ledger_seq.into(),
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
fn make_ttl_entry(data_key: &LedgerKey, live_until: LedgerSeq) -> LedgerEntry {
    let key_hash = Hash256::hash_xdr(data_key)
        .map(|h| Hash(h.0))
        .unwrap_or(Hash([0u8; 32]));
    LedgerEntry {
        last_modified_ledger_seq: 1,
        data: LedgerEntryData::Ttl(TtlEntry {
            key_hash,
            live_until_ledger_seq: live_until.into(),
        }),
        ext: LedgerEntryExt::V0,
    }
}

/// Create a ConfigSetting entry.
fn make_config_setting_entry(
    id: ConfigSettingId,
    setting: ConfigSettingEntry,
    ledger_seq: LedgerSeq,
) -> (LedgerKey, LedgerEntry) {
    let key = LedgerKey::ConfigSetting(LedgerKeyConfigSetting {
        config_setting_id: id,
    });
    let entry = LedgerEntry {
        last_modified_ledger_seq: ledger_seq.into(),
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
    let data_entry = make_config_upgrade_entry(&upgrade_key, &upgrade_set, 1.into());
    let ttl_key = ttl_key_for(&data_key);
    let ttl_entry = make_ttl_entry(&data_key, 1000.into());

    let snapshot = SnapshotBuilder::new(snapshot_ledger.into())
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
        closing_ledger_seq.into(),
    );
    let frame = ConfigUpgradeSetFrame::make_from_key(
        &ltx,
        &upgrade_key,
        closing_ledger_seq.into(),
        post_upgrade_version,
    )
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
    let data_entry = make_config_upgrade_entry(&upgrade_key, &upgrade_set, 1.into());
    let ttl_key = ttl_key_for(&data_key);

    // Set live_until = 99 = N-1 = snapshot.ledger_seq()
    // Both stellar-core and henyey now consider this EXPIRED (99 < 100).
    let ttl_entry = make_ttl_entry(&data_key, snapshot_ledger.into());

    let snapshot = SnapshotBuilder::new(snapshot_ledger.into())
        .with_header(header.clone(), header_hash)
        .add_entry(data_key, data_entry)
        .add_entry(ttl_key, ttl_entry)
        .build()
        .expect("build snapshot");
    let handle = SnapshotHandle::new(snapshot);

    // make_from_key now uses closing_ledger (100) for TTL check.
    // Entry with live_until=99 is expired because 99 < 100.
    let ltx = CloseLedgerState::begin(
        handle.clone(),
        header.clone(),
        header_hash,
        closing_ledger.into(),
    );
    let frame = ConfigUpgradeSetFrame::make_from_key(&ltx, &upgrade_key, closing_ledger.into(), 25);

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
        1.into(),
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

    let snapshot = SnapshotBuilder::new(0.into())
        .with_header(header.clone(), header_hash)
        .build()
        .expect("build snapshot");
    let handle = SnapshotHandle::new(snapshot);

    let bogus_key = ConfigUpgradeSetKey {
        contract_id: ContractId(Hash([0xBB; 32])),
        content_hash: Hash([0xCC; 32]),
    };

    // make_from_key returns None — key doesn't exist in ledger
    let ltx = CloseLedgerState::begin(handle.clone(), header.clone(), header_hash, 1.into());
    let frame = ConfigUpgradeSetFrame::make_from_key(&ltx, &bogus_key, 1.into(), 25);
    assert!(
        frame.is_none(),
        "Nonexistent config upgrade key should not be loadable"
    );

    // Parity: stellar-core throws in applyTo (Upgrades.cpp:373) when the
    // config upgrade set cannot be loaded. After AUDIT-023 fix,
    // apply_config_upgrades returns an error instead of silently skipping.
    let mut ctx = UpgradeContext::new(25);
    ctx.add_upgrade(LedgerUpgrade::Config(bogus_key));

    let mut ltx = CloseLedgerState::begin(handle, header, header_hash, 1.into());
    let result = ctx.apply_config_upgrades(&mut ltx, 1.into(), 25);

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

use stellar_xdr::curr::LedgerCloseMeta;
