use std::sync::Arc;

use henyey_bucket::{BucketList, HotArchiveBucketList};
use henyey_common::Hash256;
use henyey_ledger::{
    compute_header_hash, LedgerCloseData, LedgerManager, LedgerManagerConfig, TransactionSetVariant,
};
use stellar_xdr::curr::{
    Hash, LedgerCloseMeta, LedgerHeader, LedgerHeaderExt, StellarValue, StellarValueExt, TimePoint,
    TransactionResultSet, TransactionSet, VecM,
};

fn make_genesis_header() -> LedgerHeader {
    LedgerHeader {
        ledger_version: 25,
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

#[test]
fn test_ledger_close_with_empty_tx_set() {
    let _bucket_dir = tempfile::tempdir().expect("bucket dir");

    let config = LedgerManagerConfig {
        validate_bucket_hash: false,
        ..Default::default()
    };
    let ledger = LedgerManager::new("Test Network".to_string(), config);

    let bucket_list = BucketList::new();
    let hot_archive = HotArchiveBucketList::new();
    let header = make_genesis_header();
    let header_hash = compute_header_hash(&header).expect("hash");
    ledger
        .initialize(bucket_list, hot_archive, header, header_hash)
        .expect("init");

    let close_data = LedgerCloseData::new(
        1.into(),
        TransactionSetVariant::Classic(TransactionSet {
            previous_ledger_hash: Hash([0u8; 32]),
            txs: VecM::default(),
        }),
        1,
        ledger.current_header_hash(),
    );

    let result = ledger.close_ledger(close_data, None).expect("close ledger");
    assert!(result.tx_results.is_empty());
    assert_eq!(result.header.ledger_seq, 1);
    assert_eq!(ledger.current_ledger_seq(), 1);
    assert_ne!(ledger.current_header_hash(), Hash256::ZERO);

    let empty_results = TransactionResultSet {
        results: VecM::default(),
    };
    let expected_hash = Hash256::hash_xdr(&empty_results).expect("result hash");
    assert_eq!(
        Hash256::from(result.header.tx_set_result_hash),
        expected_hash
    );

    let meta = result.meta.expect("ledger close meta");
    match meta {
        LedgerCloseMeta::V2(v2) => {
            assert_eq!(v2.tx_processing.len(), 0);
        }
        other => panic!("unexpected ledger close meta: {:?}", other),
    }
}

/// Parity: LedgerCloseMetaStreamTests.cpp:280 "meta stream contains reasonable meta"
/// Validates the structural contents of LedgerCloseMeta after a ledger close.
#[test]
fn test_ledger_close_meta_structural_validation() {
    let config = LedgerManagerConfig {
        validate_bucket_hash: false,
        ..Default::default()
    };
    let ledger = LedgerManager::new("Test Network".to_string(), config);

    let bucket_list = BucketList::new();
    let hot_archive = HotArchiveBucketList::new();
    let header = make_genesis_header();
    let header_hash = compute_header_hash(&header).expect("hash");
    ledger
        .initialize(bucket_list, hot_archive, header.clone(), header_hash)
        .expect("init");

    let close_data = LedgerCloseData::new(
        1.into(),
        TransactionSetVariant::Classic(TransactionSet {
            previous_ledger_hash: Hash([0u8; 32]),
            txs: VecM::default(),
        }),
        100,
        ledger.current_header_hash(),
    );

    let result = ledger.close_ledger(close_data, None).expect("close ledger");
    let meta = result.meta.expect("ledger close meta");

    match meta {
        LedgerCloseMeta::V2(ref v2) => {
            // Ledger header in meta should match result header
            assert_eq!(v2.ledger_header.header.ledger_seq, result.header.ledger_seq);
            assert_eq!(v2.ledger_header.header.base_fee, header.base_fee);
            assert_eq!(v2.ledger_header.header.base_reserve, header.base_reserve);

            // Header hash should be non-zero
            assert_ne!(v2.ledger_header.hash, Hash([0u8; 32]));

            // Empty tx set: no transaction processing entries
            assert_eq!(v2.tx_processing.len(), 0);

            // No upgrades: empty upgrades processing
            assert_eq!(v2.upgrades_processing.len(), 0);

            // SCP info should be empty (we didn't set any)
            assert_eq!(v2.scp_info.len(), 0);
        }
        _ => panic!("expected V2 meta, got {:?}", meta),
    }
}

/// Parity: LedgerCloseMetaStreamTests.cpp - meta with SCP history entries
#[test]
fn test_ledger_close_meta_with_scp_history() {
    use stellar_xdr::curr::{LedgerScpMessages, ScpHistoryEntry, ScpHistoryEntryV0};

    let config = LedgerManagerConfig {
        validate_bucket_hash: false,
        ..Default::default()
    };
    let ledger = LedgerManager::new("Test Network".to_string(), config);

    let bucket_list = BucketList::new();
    let hot_archive = HotArchiveBucketList::new();
    let header = make_genesis_header();
    let header_hash = compute_header_hash(&header).expect("hash");
    ledger
        .initialize(bucket_list, hot_archive, header, header_hash)
        .expect("init");

    let scp_entry = ScpHistoryEntry::V0(ScpHistoryEntryV0 {
        quorum_sets: VecM::default(),
        ledger_messages: LedgerScpMessages {
            ledger_seq: 1,
            messages: VecM::default(),
        },
    });

    let close_data = LedgerCloseData::new(
        1.into(),
        TransactionSetVariant::Classic(TransactionSet {
            previous_ledger_hash: Hash([0u8; 32]),
            txs: VecM::default(),
        }),
        100,
        ledger.current_header_hash(),
    )
    .with_scp_history(vec![scp_entry]);

    let result = ledger.close_ledger(close_data, None).expect("close ledger");
    let meta = result.meta.expect("ledger close meta");

    match meta {
        LedgerCloseMeta::V2(v2) => {
            assert_eq!(
                v2.scp_info.len(),
                1,
                "SCP history should be included in meta"
            );
        }
        _ => panic!("expected V2 meta"),
    }
}

/// Parity: LedgerTxnTests.cpp:4215 "InMemoryLedgerTxn close multiple ledgers with merges"
/// Tests multiple consecutive ledger closes without transactions.
#[tokio::test(flavor = "multi_thread")]
async fn test_multiple_consecutive_ledger_closes() {
    let config = LedgerManagerConfig {
        validate_bucket_hash: false,
        ..Default::default()
    };
    let ledger = Arc::new(LedgerManager::new("Test Network".to_string(), config));

    let bucket_list = BucketList::new();
    let hot_archive = HotArchiveBucketList::new();
    let header = make_genesis_header();
    let header_hash = compute_header_hash(&header).expect("hash");
    ledger
        .initialize(bucket_list, hot_archive, header, header_hash)
        .expect("init");

    // Close 5 consecutive ledgers
    for seq in 1..=5u32 {
        let prev_hash = ledger.current_header_hash();
        let close_data = LedgerCloseData::new(
            seq.into(),
            TransactionSetVariant::Classic(TransactionSet {
                previous_ledger_hash: Hash::from(prev_hash),
                txs: VecM::default(),
            }),
            seq as u64 * 10,
            prev_hash,
        );

        let handle = tokio::runtime::Handle::current();
        let lm = ledger.clone();
        let result = tokio::task::spawn_blocking(move || lm.close_ledger(close_data, Some(handle)))
            .await
            .expect("spawn_blocking")
            .unwrap_or_else(|e| panic!("close ledger {}: {}", seq, e));

        assert_eq!(result.header.ledger_seq, seq);
        assert_eq!(ledger.current_ledger_seq(), seq);
    }

    // Verify final state
    assert_eq!(ledger.current_ledger_seq(), 5);
    assert_ne!(ledger.current_header_hash(), Hash256::ZERO);
}

/// Parity: LedgerTests.cpp:15 "cannot close ledger with unsupported ledger version"
/// Tests that close_ledger panics when protocol version exceeds max supported.
#[test]
#[should_panic(expected = "unsupported protocol version")]
fn test_unsupported_protocol_version_too_high_integration() {
    use henyey_common::protocol::CURRENT_LEDGER_PROTOCOL_VERSION;

    let config = LedgerManagerConfig {
        validate_bucket_hash: false,
        ..Default::default()
    };
    let ledger = LedgerManager::new("Test Network".to_string(), config);

    // Initialize with current protocol version
    let mut header = make_genesis_header();
    header.ledger_version = CURRENT_LEDGER_PROTOCOL_VERSION;
    let header_hash = compute_header_hash(&header).expect("hash");
    let bucket_list = BucketList::new();
    let hot_archive = HotArchiveBucketList::new();
    ledger
        .initialize(bucket_list, hot_archive, header, header_hash)
        .expect("init");

    // Close at current version should work
    let close_data = LedgerCloseData::new(
        1.into(),
        TransactionSetVariant::Classic(TransactionSet {
            previous_ledger_hash: Hash::from(header_hash),
            txs: VecM::default(),
        }),
        1,
        header_hash,
    );
    ledger
        .close_ledger(close_data, None)
        .expect("close at current version");

    // Now force the stored header to have CURRENT + 1
    ledger.set_header_version_for_test(CURRENT_LEDGER_PROTOCOL_VERSION + 1);

    let prev_hash = ledger.current_header_hash();
    let close_data2 = LedgerCloseData::new(
        2.into(),
        TransactionSetVariant::Classic(TransactionSet {
            previous_ledger_hash: Hash::from(prev_hash),
            txs: VecM::default(),
        }),
        2,
        prev_hash,
    );

    // This should panic
    let _result = ledger.close_ledger(close_data2, None);
}

/// Tests that close_ledger panics when protocol version is below min supported.
#[test]
#[should_panic(expected = "unsupported protocol version")]
fn test_unsupported_protocol_version_too_low_integration() {
    use henyey_common::protocol::{CURRENT_LEDGER_PROTOCOL_VERSION, MIN_LEDGER_PROTOCOL_VERSION};

    let config = LedgerManagerConfig {
        validate_bucket_hash: false,
        ..Default::default()
    };
    let ledger = LedgerManager::new("Test Network".to_string(), config);

    let mut header = make_genesis_header();
    header.ledger_version = CURRENT_LEDGER_PROTOCOL_VERSION;
    let header_hash = compute_header_hash(&header).expect("hash");
    let bucket_list = BucketList::new();
    let hot_archive = HotArchiveBucketList::new();
    ledger
        .initialize(bucket_list, hot_archive, header, header_hash)
        .expect("init");

    // Close at current version should work
    let close_data = LedgerCloseData::new(
        1.into(),
        TransactionSetVariant::Classic(TransactionSet {
            previous_ledger_hash: Hash::from(header_hash),
            txs: VecM::default(),
        }),
        1,
        header_hash,
    );
    ledger
        .close_ledger(close_data, None)
        .expect("close at current version");

    // Force the stored header to have MIN - 1
    ledger.set_header_version_for_test(MIN_LEDGER_PROTOCOL_VERSION - 1);

    let prev_hash = ledger.current_header_hash();
    let close_data2 = LedgerCloseData::new(
        2.into(),
        TransactionSetVariant::Classic(TransactionSet {
            previous_ledger_hash: Hash::from(prev_hash),
            txs: VecM::default(),
        }),
        2,
        prev_hash,
    );

    // This should panic
    let _result = ledger.close_ledger(close_data2, None);
}

/// Test that close_ledger works from a spawn_blocking thread with an explicit
/// runtime handle. This is the production code path for parallel ledger close.
#[tokio::test(flavor = "multi_thread")]
async fn test_close_ledger_from_spawn_blocking() {
    let config = LedgerManagerConfig {
        validate_bucket_hash: false,
        ..Default::default()
    };
    let ledger = Arc::new(LedgerManager::new("Test Network".to_string(), config));

    let bucket_list = BucketList::new();
    let hot_archive = HotArchiveBucketList::new();
    let header = make_genesis_header();
    let header_hash = compute_header_hash(&header).expect("hash");
    ledger
        .initialize(bucket_list, hot_archive, header, header_hash)
        .expect("init");

    let close_data = LedgerCloseData::new(
        1.into(),
        TransactionSetVariant::Classic(TransactionSet {
            previous_ledger_hash: Hash([0u8; 32]),
            txs: VecM::default(),
        }),
        1,
        ledger.current_header_hash(),
    );

    let handle = tokio::runtime::Handle::current();
    let lm = ledger.clone();

    // Close the ledger from a spawn_blocking thread with Some(handle).
    let result = tokio::task::spawn_blocking(move || lm.close_ledger(close_data, Some(handle)))
        .await
        .expect("spawn_blocking task")
        .expect("close ledger");

    assert!(result.tx_results.is_empty());
    assert_eq!(result.header.ledger_seq, 1);
    assert_eq!(ledger.current_ledger_seq(), 1);
    assert_ne!(ledger.current_header_hash(), Hash256::ZERO);

    // Verify result matches what we'd get from the synchronous path.
    let empty_results = TransactionResultSet {
        results: VecM::default(),
    };
    let expected_hash = Hash256::hash_xdr(&empty_results).expect("result hash");
    assert_eq!(
        Hash256::from(result.header.tx_set_result_hash),
        expected_hash
    );
}

/// Test that two consecutive ledger closes from spawn_blocking work correctly,
/// verifying the runtime handle can be reused across multiple closes.
#[tokio::test(flavor = "multi_thread")]
async fn test_consecutive_close_ledger_from_spawn_blocking() {
    let config = LedgerManagerConfig {
        validate_bucket_hash: false,
        ..Default::default()
    };
    let ledger = Arc::new(LedgerManager::new("Test Network".to_string(), config));

    let bucket_list = BucketList::new();
    let hot_archive = HotArchiveBucketList::new();
    let header = make_genesis_header();
    let header_hash = compute_header_hash(&header).expect("hash");
    ledger
        .initialize(bucket_list, hot_archive, header, header_hash)
        .expect("init");

    // Close ledger 1.
    let prev_hash = ledger.current_header_hash();
    let close_data1 = LedgerCloseData::new(
        1.into(),
        TransactionSetVariant::Classic(TransactionSet {
            previous_ledger_hash: Hash::from(prev_hash),
            txs: VecM::default(),
        }),
        1,
        prev_hash,
    );

    let handle = tokio::runtime::Handle::current();
    let lm = ledger.clone();
    tokio::task::spawn_blocking(move || {
        lm.close_ledger(close_data1, Some(handle))
            .expect("close ledger 1");
    })
    .await
    .expect("task 1");

    assert_eq!(ledger.current_ledger_seq(), 1);

    // Close ledger 2 (chained).
    let prev_hash2 = ledger.current_header_hash();
    let close_data2 = LedgerCloseData::new(
        2.into(),
        TransactionSetVariant::Classic(TransactionSet {
            previous_ledger_hash: Hash::from(prev_hash2),
            txs: VecM::default(),
        }),
        2,
        prev_hash2,
    );

    let handle2 = tokio::runtime::Handle::current();
    let lm2 = ledger.clone();
    tokio::task::spawn_blocking(move || {
        lm2.close_ledger(close_data2, Some(handle2))
            .expect("close ledger 2");
    })
    .await
    .expect("task 2");

    assert_eq!(ledger.current_ledger_seq(), 2);
}
