use std::sync::Arc;

use henyey_bucket::HotArchiveBucketList;
use henyey_common::Hash256;
use henyey_ledger::{
    compute_header_hash, LedgerCloseData, LedgerManager, LedgerManagerConfig, TransactionSetVariant,
};
use stellar_xdr::curr::{
    AccountEntry, AccountEntryExt, AccountId, BucketListType, BytesM, ContractCodeEntry,
    ContractCodeEntryExt, ContractEventBody, DecoratedSignature, ExtendFootprintTtlOp,
    ExtensionPoint, FeeBumpTransaction, FeeBumpTransactionEnvelope, FeeBumpTransactionInnerTx,
    Hash, LedgerCloseMeta, LedgerEntry, LedgerEntryData, LedgerEntryExt, LedgerFootprint,
    LedgerHeader, LedgerHeaderExt, LedgerKey, LedgerKeyContractCode, Memo, MuxedAccount, Operation,
    OperationBody, Preconditions, PublicKey, ScVal, SequenceNumber, Signature as XdrSignature,
    SignatureHint, SorobanResources, SorobanTransactionData, SorobanTransactionDataExt,
    StellarValue, StellarValueExt, Thresholds, TimePoint, Transaction, TransactionEnvelope,
    TransactionEventStage, TransactionExt, TransactionMeta, TransactionResultSet, TransactionSet,
    TransactionV1Envelope, TtlEntry, Uint256, VecM,
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

    let bucket_list = henyey_ledger::new_bucket_list_with_soroban_config();
    let hot_archive = HotArchiveBucketList::new();
    let header = make_genesis_header();
    let header_hash = compute_header_hash(&header).expect("hash");
    ledger
        .initialize(bucket_list, hot_archive, header, header_hash)
        .expect("init");

    let close_data = LedgerCloseData::new(
        1,
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
    let expected_hash = Hash256::hash_xdr(&empty_results);
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

    let bucket_list = henyey_ledger::new_bucket_list_with_soroban_config();
    let hot_archive = HotArchiveBucketList::new();
    let header = make_genesis_header();
    let header_hash = compute_header_hash(&header).expect("hash");
    ledger
        .initialize(bucket_list, hot_archive, header.clone(), header_hash)
        .expect("init");

    let close_data = LedgerCloseData::new(
        1,
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

    let bucket_list = henyey_ledger::new_bucket_list_with_soroban_config();
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
        1,
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

    let bucket_list = henyey_ledger::new_bucket_list_with_soroban_config();
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
            seq,
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
    let bucket_list = henyey_ledger::new_bucket_list_with_soroban_config();
    let hot_archive = HotArchiveBucketList::new();
    ledger
        .initialize(bucket_list, hot_archive, header, header_hash)
        .expect("init");

    // Close at current version should work
    let close_data = LedgerCloseData::new(
        1,
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
        2,
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
    let bucket_list = henyey_ledger::new_bucket_list_with_soroban_config();
    let hot_archive = HotArchiveBucketList::new();
    ledger
        .initialize(bucket_list, hot_archive, header, header_hash)
        .expect("init");

    // Close at current version should work
    let close_data = LedgerCloseData::new(
        1,
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
        2,
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

    let bucket_list = henyey_ledger::new_bucket_list_with_soroban_config();
    let hot_archive = HotArchiveBucketList::new();
    let header = make_genesis_header();
    let header_hash = compute_header_hash(&header).expect("hash");
    ledger
        .initialize(bucket_list, hot_archive, header, header_hash)
        .expect("init");

    let close_data = LedgerCloseData::new(
        1,
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
    let expected_hash = Hash256::hash_xdr(&empty_results);
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

    let bucket_list = henyey_ledger::new_bucket_list_with_soroban_config();
    let hot_archive = HotArchiveBucketList::new();
    let header = make_genesis_header();
    let header_hash = compute_header_hash(&header).expect("hash");
    ledger
        .initialize(bucket_list, hot_archive, header, header_hash)
        .expect("init");

    // Close ledger 1.
    let prev_hash = ledger.current_header_hash();
    let close_data1 = LedgerCloseData::new(
        1,
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
        2,
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

// --- Fee event regression tests ---

use henyey_common::NetworkId;
use henyey_crypto::{sign_hash, SecretKey};

fn sign_envelope(
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

fn i128_val(val: &ScVal) -> i128 {
    match val {
        ScVal::I128(parts) => ((parts.hi as i128) << 64) | (parts.lo as i128),
        _ => panic!("expected ScVal::I128, got {:?}", val),
    }
}

fn make_source_account_entry(account_id: AccountId, seq_num: i64, balance: i64) -> LedgerEntry {
    LedgerEntry {
        last_modified_ledger_seq: 0,
        data: LedgerEntryData::Account(AccountEntry {
            account_id,
            balance,
            seq_num: SequenceNumber(seq_num),
            num_sub_entries: 0,
            inflation_dest: None,
            flags: 0,
            home_domain: Default::default(),
            thresholds: Thresholds([1, 0, 0, 0]),
            signers: VecM::default(),
            ext: AccountEntryExt::V0,
        }),
        ext: LedgerEntryExt::V0,
    }
}

/// Regression test: close_ledger fee event uses pre-refund fee for BeforeAllTxs event.
///
/// Verifies that after a Soroban transaction with a non-zero refund, the BeforeAllTxs
/// fee event in tx_apply_processing records fee_charged + fee_refund (the pre-refund
/// fee), while TransactionResult.fee_charged remains the post-refund value.
#[test]
fn test_close_ledger_fee_event_uses_pre_refund_fee() {
    let network_id = NetworkId::testnet();
    let secret = SecretKey::from_seed(&[1u8; 32]);
    let source_id = AccountId(PublicKey::PublicKeyTypeEd25519(Uint256(
        *secret.public_key().as_bytes(),
    )));

    // Build bucket list with required entries
    let mut bucket_list = henyey_ledger::new_bucket_list_with_soroban_config();
    let source_entry = make_source_account_entry(source_id.clone(), 1, 20_000_000);

    let code_hash = Hash([9u8; 32]);
    let contract_code_entry = LedgerEntry {
        last_modified_ledger_seq: 0,
        data: LedgerEntryData::ContractCode(ContractCodeEntry {
            ext: ContractCodeEntryExt::V0,
            hash: code_hash.clone(),
            code: BytesM::try_from(vec![1u8, 2u8, 3u8]).unwrap(),
        }),
        ext: LedgerEntryExt::V0,
    };

    let contract_key = LedgerKey::ContractCode(LedgerKeyContractCode {
        hash: code_hash.clone(),
    });
    let key_hash: Hash = henyey_common::Hash256::hash_xdr(&contract_key).into();
    let ttl_entry = LedgerEntry {
        last_modified_ledger_seq: 0,
        data: LedgerEntryData::Ttl(TtlEntry {
            key_hash: key_hash.clone(),
            live_until_ledger_seq: 10,
        }),
        ext: LedgerEntryExt::V0,
    };

    bucket_list
        .add_batch(
            1,
            25,
            BucketListType::Live,
            vec![source_entry, contract_code_entry, ttl_entry],
            vec![],
            vec![],
        )
        .expect("add_batch");

    // Initialize LedgerManager
    let config = LedgerManagerConfig {
        emit_classic_events: true,
        validate_bucket_hash: false,
        ..Default::default()
    };
    let ledger = LedgerManager::new("Test SDF Network ; September 2015".to_string(), config);
    let hot_archive = HotArchiveBucketList::new();
    let header = make_genesis_header();
    let header_hash = compute_header_hash(&header).expect("hash");
    ledger
        .initialize(bucket_list, hot_archive, header, header_hash)
        .expect("init");

    // Build the Soroban ExtendFootprintTtl transaction.
    // resource_fee must exceed the non-refundable portion (compute + read + bandwidth fees)
    // so that max_refundable_fee > 0, producing a meaningful refund.
    let soroban_data = SorobanTransactionData {
        ext: SorobanTransactionDataExt::V0,
        resources: SorobanResources {
            footprint: LedgerFootprint {
                read_only: vec![contract_key].try_into().unwrap(),
                read_write: VecM::default(),
            },
            instructions: 0,
            disk_read_bytes: 100,
            write_bytes: 0,
        },
        resource_fee: 100_000,
    };

    let tx = Transaction {
        source_account: MuxedAccount::Ed25519(Uint256(*secret.public_key().as_bytes())),
        fee: 110_000,
        seq_num: SequenceNumber(2),
        cond: Preconditions::None,
        memo: Memo::None,
        operations: vec![Operation {
            source_account: None,
            body: OperationBody::ExtendFootprintTtl(ExtendFootprintTtlOp {
                ext: ExtensionPoint::V0,
                extend_to: 100,
            }),
        }]
        .try_into()
        .unwrap(),
        ext: TransactionExt::V1(soroban_data),
    };

    let mut envelope = TransactionEnvelope::Tx(TransactionV1Envelope {
        tx,
        signatures: VecM::default(),
    });
    let decorated = sign_envelope(&envelope, &secret, &network_id);
    if let TransactionEnvelope::Tx(ref mut env) = envelope {
        env.signatures = vec![decorated].try_into().unwrap();
    }

    // Close the ledger
    let prev_hash = ledger.current_header_hash();
    let close_data = LedgerCloseData::new(
        1,
        TransactionSetVariant::Classic(TransactionSet {
            previous_ledger_hash: Hash::from(prev_hash),
            txs: vec![envelope].try_into().unwrap(),
        }),
        100,
        prev_hash,
    );

    let result = ledger.close_ledger(close_data, None).expect("close ledger");

    // Assertions
    let meta = result.meta.expect("ledger close meta");
    let LedgerCloseMeta::V2(v2) = meta else {
        panic!("expected V2 meta");
    };
    assert_eq!(
        v2.tx_processing.len(),
        1,
        "should have one tx processing entry"
    );

    let tx_processing = &v2.tx_processing[0];
    let TransactionMeta::V4(ref meta_v4) = tx_processing.tx_apply_processing else {
        panic!("expected TransactionMeta::V4");
    };

    // Find BeforeAllTxs fee event
    let before_event = meta_v4
        .events
        .iter()
        .find(|e| e.stage == TransactionEventStage::BeforeAllTxs)
        .expect("should have BeforeAllTxs event");

    let ContractEventBody::V0(ref before_body) = before_event.event.body;
    let fee_event_amount = i128_val(&before_body.data);

    // fee_to_charge = resource_fee + min(inclusion_fee, base_fee * ops) = 100_000 + min(10_000, 100) = 100_100
    let expected_pre_refund_fee: i128 = 100_100;

    // The pre-refund fee should be fee_charged + fee_refund.
    // tx_results gives us the post-refund fee_charged.
    let post_refund_fee = result.tx_results[0].result.fee_charged;
    assert!(post_refund_fee > 0, "post-refund fee should be positive");
    // The fee event amount should be GREATER than the post-refund fee
    // (because it includes the refund that hasn't been applied yet)
    assert!(
        fee_event_amount > post_refund_fee as i128,
        "BeforeAllTxs event ({}) should be greater than post-refund fee_charged ({})",
        fee_event_amount,
        post_refund_fee
    );
    // The fee event amount should equal the pre-refund fee (fee_to_charge)
    assert_eq!(
        fee_event_amount, expected_pre_refund_fee,
        "BeforeAllTxs event should equal the full pre-refund fee (fee_to_charge)"
    );

    // Verify AfterAllTxs refund event is present with negative amount
    let after_event = meta_v4
        .events
        .iter()
        .find(|e| e.stage == TransactionEventStage::AfterAllTxs)
        .expect("should have AfterAllTxs refund event");

    let ContractEventBody::V0(ref after_body) = after_event.event.body;
    let refund_amount = i128_val(&after_body.data);
    assert!(
        refund_amount < 0,
        "AfterAllTxs refund event amount should be negative, got {}",
        refund_amount
    );

    // Confirm post-refund fee_charged is less than the pre-refund fee (there was a refund)
    assert!(
        (post_refund_fee as i128) < expected_pre_refund_fee,
        "post-refund fee_charged ({}) should be less than pre-refund fee ({})",
        post_refund_fee,
        expected_pre_refund_fee
    );
}

/// Regression test: close_ledger fee event uses outer fee source for fee-bump Soroban tx.
///
/// Verifies that when a Soroban transaction is wrapped in a FeeBumpTransaction:
/// - The BeforeAllTxs fee event uses the pre-refund fee amount
/// - The fee event references the outer (fee-bump) source, not the inner tx source
/// - TransactionResult.fee_charged remains post-refund
#[test]
fn test_close_ledger_fee_event_fee_bump_soroban() {
    let network_id = NetworkId::testnet();
    let inner_secret = SecretKey::from_seed(&[1u8; 32]);
    let outer_secret = SecretKey::from_seed(&[2u8; 32]);
    let inner_source_id = AccountId(PublicKey::PublicKeyTypeEd25519(Uint256(
        *inner_secret.public_key().as_bytes(),
    )));
    let outer_source_id = AccountId(PublicKey::PublicKeyTypeEd25519(Uint256(
        *outer_secret.public_key().as_bytes(),
    )));

    // Build bucket list with both accounts
    let mut bucket_list = henyey_ledger::new_bucket_list_with_soroban_config();
    let inner_entry = make_source_account_entry(inner_source_id.clone(), 1, 20_000_000);
    let outer_entry = make_source_account_entry(outer_source_id.clone(), 0, 20_000_000);

    let code_hash = Hash([9u8; 32]);
    let contract_code_entry = LedgerEntry {
        last_modified_ledger_seq: 0,
        data: LedgerEntryData::ContractCode(ContractCodeEntry {
            ext: ContractCodeEntryExt::V0,
            hash: code_hash.clone(),
            code: BytesM::try_from(vec![1u8, 2u8, 3u8]).unwrap(),
        }),
        ext: LedgerEntryExt::V0,
    };

    let contract_key = LedgerKey::ContractCode(LedgerKeyContractCode {
        hash: code_hash.clone(),
    });
    let key_hash: Hash = henyey_common::Hash256::hash_xdr(&contract_key).into();
    let ttl_entry = LedgerEntry {
        last_modified_ledger_seq: 0,
        data: LedgerEntryData::Ttl(TtlEntry {
            key_hash: key_hash.clone(),
            live_until_ledger_seq: 10,
        }),
        ext: LedgerEntryExt::V0,
    };

    bucket_list
        .add_batch(
            1,
            25,
            BucketListType::Live,
            vec![inner_entry, outer_entry, contract_code_entry, ttl_entry],
            vec![],
            vec![],
        )
        .expect("add_batch");

    // Initialize LedgerManager
    let config = LedgerManagerConfig {
        emit_classic_events: true,
        validate_bucket_hash: false,
        ..Default::default()
    };
    let ledger = LedgerManager::new("Test SDF Network ; September 2015".to_string(), config);
    let hot_archive = HotArchiveBucketList::new();
    let header = make_genesis_header();
    let header_hash = compute_header_hash(&header).expect("hash");
    ledger
        .initialize(bucket_list, hot_archive, header, header_hash)
        .expect("init");

    // Build the inner Soroban transaction
    let soroban_data = SorobanTransactionData {
        ext: SorobanTransactionDataExt::V0,
        resources: SorobanResources {
            footprint: LedgerFootprint {
                read_only: vec![contract_key].try_into().unwrap(),
                read_write: VecM::default(),
            },
            instructions: 0,
            disk_read_bytes: 100,
            write_bytes: 0,
        },
        resource_fee: 100_000,
    };

    let inner_tx = Transaction {
        source_account: MuxedAccount::Ed25519(Uint256(*inner_secret.public_key().as_bytes())),
        fee: 110_000,
        seq_num: SequenceNumber(2),
        cond: Preconditions::None,
        memo: Memo::None,
        operations: vec![Operation {
            source_account: None,
            body: OperationBody::ExtendFootprintTtl(ExtendFootprintTtlOp {
                ext: ExtensionPoint::V0,
                extend_to: 100,
            }),
        }]
        .try_into()
        .unwrap(),
        ext: TransactionExt::V1(soroban_data),
    };

    // Sign the inner tx
    let mut inner_envelope = TransactionEnvelope::Tx(TransactionV1Envelope {
        tx: inner_tx.clone(),
        signatures: VecM::default(),
    });
    let inner_sig = sign_envelope(&inner_envelope, &inner_secret, &network_id);
    if let TransactionEnvelope::Tx(ref mut env) = inner_envelope {
        env.signatures = vec![inner_sig].try_into().unwrap();
    }
    let inner_v1 = match inner_envelope {
        TransactionEnvelope::Tx(env) => env,
        _ => unreachable!(),
    };

    // Build the fee-bump envelope
    let fee_bump_tx = FeeBumpTransaction {
        fee_source: MuxedAccount::Ed25519(Uint256(*outer_secret.public_key().as_bytes())),
        fee: 200_000,
        inner_tx: FeeBumpTransactionInnerTx::Tx(inner_v1),
        ext: stellar_xdr::curr::FeeBumpTransactionExt::V0,
    };

    let mut fee_bump_envelope = TransactionEnvelope::TxFeeBump(FeeBumpTransactionEnvelope {
        tx: fee_bump_tx,
        signatures: VecM::default(),
    });
    let outer_sig = sign_envelope(&fee_bump_envelope, &outer_secret, &network_id);
    if let TransactionEnvelope::TxFeeBump(ref mut env) = fee_bump_envelope {
        env.signatures = vec![outer_sig].try_into().unwrap();
    }

    // Close the ledger
    let prev_hash = ledger.current_header_hash();
    let close_data = LedgerCloseData::new(
        1,
        TransactionSetVariant::Classic(TransactionSet {
            previous_ledger_hash: Hash::from(prev_hash),
            txs: vec![fee_bump_envelope].try_into().unwrap(),
        }),
        100,
        prev_hash,
    );

    let result = ledger.close_ledger(close_data, None).expect("close ledger");

    // Assertions
    let meta = result.meta.expect("ledger close meta");
    let LedgerCloseMeta::V2(v2) = meta else {
        panic!("expected V2 meta");
    };
    assert_eq!(v2.tx_processing.len(), 1);

    let tx_processing = &v2.tx_processing[0];
    let TransactionMeta::V4(ref meta_v4) = tx_processing.tx_apply_processing else {
        panic!("expected TransactionMeta::V4");
    };

    // Find BeforeAllTxs fee event
    let before_event = meta_v4
        .events
        .iter()
        .find(|e| e.stage == TransactionEventStage::BeforeAllTxs)
        .expect("should have BeforeAllTxs event");

    let ContractEventBody::V0(ref before_body) = before_event.event.body;
    let fee_event_amount = i128_val(&before_body.data);

    // The fee event should use the pre-refund fee (full fee_to_charge from fee-bump source)
    // fee_to_charge = resource_fee + min(inclusion_fee_from_outer, base_fee * ops)
    // inclusion_fee_from_outer = outer_fee - resource_fee = 200_000 - 100_000 = 100_000
    // For fee-bump: resource_operation_count = num_ops + 1 = 2, so min_inclusion_fee = 100 * 2 = 200
    // fee_to_charge = 100_000 + min(100_000, 200) = 100_200
    let expected_pre_refund_fee: i128 = 100_200;
    let post_refund_fee = result.tx_results[0].result.fee_charged;
    assert!(
        fee_event_amount > post_refund_fee as i128,
        "BeforeAllTxs event ({}) should be greater than post-refund fee_charged ({})",
        fee_event_amount,
        post_refund_fee
    );

    // The fee event amount should equal the pre-refund fee (fee_to_charge)
    assert_eq!(
        fee_event_amount, expected_pre_refund_fee,
        "BeforeAllTxs event should equal the full pre-refund fee (fee_to_charge)"
    );

    // Verify the fee event references the outer (fee-bump) source account via the
    // SAC transfer's `from` topic. The BeforeAllTxs event is a native SAC transfer
    // from the fee source to the fee pool.
    let topics = &before_body.topics;
    // topics[0] = "transfer", topics[1] = from (fee source), topics[2] = to (fee pool)
    assert!(topics.len() >= 2, "fee event should have from topic");
    let from_address = &topics[1];
    // Verify it's the outer source (fee-bump source), not the inner tx source
    if let ScVal::Address(addr) = from_address {
        let outer_strkey = henyey_crypto::account_id_to_strkey(&outer_source_id);
        let from_str = format!("{:?}", addr);
        assert!(
            from_str.contains(&outer_strkey) || {
                // Compare the raw bytes: the Address should correspond to the outer source
                match addr {
                    stellar_xdr::curr::ScAddress::Account(aid) => aid == &outer_source_id,
                    _ => false,
                }
            },
            "fee event source should be the outer (fee-bump) account"
        );
    } else {
        panic!(
            "expected Address in fee event from topic, got {:?}",
            from_address
        );
    }

    // Verify AfterAllTxs refund event is present
    let after_event = meta_v4
        .events
        .iter()
        .find(|e| e.stage == TransactionEventStage::AfterAllTxs);
    assert!(
        after_event.is_some(),
        "should have AfterAllTxs refund event"
    );

    // Confirm there was a refund (post-refund < pre-refund)
    assert!(
        (post_refund_fee as i128) < expected_pre_refund_fee,
        "post-refund fee_charged ({}) should be less than pre-refund fee ({})",
        post_refund_fee,
        expected_pre_refund_fee
    );
}

/// Parity: LedgerCloseMetaFrame.cpp:170-187 (populateEvictedEntries)
///
/// Verifies that after a ledger close with mixed temporary and persistent
/// Soroban entries that have expired TTLs, the emitted
/// `LedgerCloseMetaV2.evicted_keys` ordering matches stellar-core's two-phase
/// rule: deleted_keys first (temp data + all TTL keys in scan order), then
/// persistent data keys from archived_entries.
///
/// This exercises the full inline eviction scan path through ledger close,
/// not just the unit-level ResolvedEviction ordering.
#[test]
fn test_ledger_close_eviction_meta_key_ordering() {
    // The eviction scan path triggers bucket list merges that require a tokio
    // runtime (spawn_blocking in add_batch_internal).
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        test_ledger_close_eviction_meta_key_ordering_impl();
    });
}

fn test_ledger_close_eviction_meta_key_ordering_impl() {
    use henyey_bucket::{BucketList, EvictionIterator};
    use henyey_common::xdr_to_bytes;
    use sha2::{Digest, Sha256};
    use stellar_xdr::curr::{
        ConfigSettingContractBandwidthV0, ConfigSettingContractComputeV0,
        ConfigSettingContractEventsV0, ConfigSettingContractExecutionLanesV0,
        ConfigSettingContractHistoricalDataV0, ConfigSettingContractLedgerCostV0,
        ConfigSettingEntry, ContractCostParamEntry, ContractCostParams, ContractDataDurability,
        ContractDataEntry, ContractId, LedgerKeyContractData, LedgerKeyTtl, ScAddress, ScBytes,
        StateArchivalSettings, WriteXdr,
    };

    // --- Helper: compute the TTL key for a given data key ---
    let ttl_key_for = |data_key: &LedgerKey| -> LedgerKey {
        let key_bytes = data_key.to_xdr(stellar_xdr::curr::Limits::none()).unwrap();
        let hash_bytes: [u8; 32] = Sha256::digest(&key_bytes).into();
        LedgerKey::Ttl(LedgerKeyTtl {
            key_hash: Hash(hash_bytes),
        })
    };

    // --- Build the 4 contract data entries ---
    let contract = ScAddress::Contract(ContractId(Hash([1u8; 32])));

    let make_data_entry =
        |key_byte: u8, durability: ContractDataDurability| -> (LedgerEntry, LedgerKey) {
            let key = LedgerKey::ContractData(LedgerKeyContractData {
                contract: contract.clone(),
                key: ScVal::Bytes(ScBytes(vec![key_byte].try_into().unwrap())),
                durability,
            });
            let entry = LedgerEntry {
                last_modified_ledger_seq: 1,
                data: LedgerEntryData::ContractData(ContractDataEntry {
                    ext: ExtensionPoint::V0,
                    contract: contract.clone(),
                    key: ScVal::Bytes(ScBytes(vec![key_byte].try_into().unwrap())),
                    durability,
                    val: ScVal::I32(42),
                }),
                ext: LedgerEntryExt::V0,
            };
            (entry, key)
        };

    // Entries in expected XDR sort order: (key_byte, durability)
    // Temporary(0) < Persistent(1), so:
    // A: (0x01, Temporary)
    // B: (0x01, Persistent)
    // C: (0x02, Temporary)
    // D: (0x02, Persistent)
    let (entry_a, key_a) = make_data_entry(0x01, ContractDataDurability::Temporary);
    let (entry_b, key_b) = make_data_entry(0x01, ContractDataDurability::Persistent);
    let (entry_c, key_c) = make_data_entry(0x02, ContractDataDurability::Temporary);
    let (entry_d, key_d) = make_data_entry(0x02, ContractDataDurability::Persistent);

    // TTL entries with live_until = 5 (will expire at close ledger 10, since 5 < 10)
    let make_ttl_entry_for = |data_key: &LedgerKey| -> LedgerEntry {
        let key_bytes = xdr_to_bytes(data_key);
        let hash_bytes: [u8; 32] = Sha256::digest(&key_bytes).into();
        LedgerEntry {
            last_modified_ledger_seq: 1,
            data: LedgerEntryData::Ttl(TtlEntry {
                key_hash: Hash(hash_bytes),
                live_until_ledger_seq: 5,
            }),
            ext: LedgerEntryExt::V0,
        }
    };

    let ttl_a = make_ttl_entry_for(&key_a);
    let ttl_b = make_ttl_entry_for(&key_b);
    let ttl_c = make_ttl_entry_for(&key_c);
    let ttl_d = make_ttl_entry_for(&key_d);

    // --- Build custom BucketList with eviction config ---
    let cost_param = ContractCostParamEntry {
        ext: ExtensionPoint::V0,
        const_term: 0,
        linear_term: 0,
    };
    let cost_params = ContractCostParams(vec![cost_param].try_into().unwrap());

    let make_config = |setting: ConfigSettingEntry| LedgerEntry {
        last_modified_ledger_seq: 1,
        data: LedgerEntryData::ConfigSetting(setting),
        ext: LedgerEntryExt::V0,
    };

    let config_entries = vec![
        make_config(ConfigSettingEntry::ContractMaxSizeBytes(2_000)),
        make_config(ConfigSettingEntry::ContractDataKeySizeBytes(200)),
        make_config(ConfigSettingEntry::ContractDataEntrySizeBytes(2_000)),
        make_config(ConfigSettingEntry::ContractComputeV0(
            ConfigSettingContractComputeV0 {
                ledger_max_instructions: 2_500_000,
                tx_max_instructions: 2_500_000,
                fee_rate_per_instructions_increment: 100,
                tx_memory_limit: 2_000_000,
            },
        )),
        make_config(ConfigSettingEntry::ContractLedgerCostV0(
            ConfigSettingContractLedgerCostV0 {
                ledger_max_disk_read_entries: 3,
                ledger_max_disk_read_bytes: 3_200,
                ledger_max_write_ledger_entries: 2,
                ledger_max_write_bytes: 3_200,
                tx_max_disk_read_entries: 3,
                tx_max_disk_read_bytes: 3_200,
                tx_max_write_ledger_entries: 2,
                tx_max_write_bytes: 3_200,
                fee_disk_read_ledger_entry: 5_000,
                fee_write_ledger_entry: 20_000,
                fee_disk_read1_kb: 1_000,
                soroban_state_target_size_bytes: 1_000_000,
                rent_fee1_kb_soroban_state_size_low: 1_000,
                rent_fee1_kb_soroban_state_size_high: 10_000,
                soroban_state_rent_fee_growth_factor: 1,
            },
        )),
        make_config(ConfigSettingEntry::ContractHistoricalDataV0(
            ConfigSettingContractHistoricalDataV0 {
                fee_historical1_kb: 100,
            },
        )),
        make_config(ConfigSettingEntry::ContractEventsV0(
            ConfigSettingContractEventsV0 {
                tx_max_contract_events_size_bytes: 200,
                fee_contract_events1_kb: 200,
            },
        )),
        make_config(ConfigSettingEntry::ContractBandwidthV0(
            ConfigSettingContractBandwidthV0 {
                ledger_max_txs_size_bytes: 10_000,
                tx_max_size_bytes: 10_000,
                fee_tx_size1_kb: 2_000,
            },
        )),
        make_config(ConfigSettingEntry::ContractExecutionLanes(
            ConfigSettingContractExecutionLanesV0 {
                ledger_max_tx_count: 1,
            },
        )),
        make_config(ConfigSettingEntry::ContractCostParamsCpuInstructions(
            cost_params.clone(),
        )),
        make_config(ConfigSettingEntry::ContractCostParamsMemoryBytes(
            cost_params,
        )),
        // Test shortcut: starting_eviction_scan_level = 0 so we scan level 0
        // where add_batch places entries. Non-production (triggers warning).
        make_config(ConfigSettingEntry::StateArchival(StateArchivalSettings {
            max_entry_ttl: 1_054_080,
            min_persistent_ttl: 4_096,
            min_temporary_ttl: 16,
            persistent_rent_rate_denominator: 252_480,
            temp_rent_rate_denominator: 2_524_800,
            max_entries_to_archive: 100,
            live_soroban_state_size_window_sample_size: 30,
            live_soroban_state_size_window_sample_period: 64,
            eviction_scan_size: 100_000,
            starting_eviction_scan_level: 0,
        })),
        make_config(ConfigSettingEntry::LiveSorobanStateSizeWindow(
            vec![0u64; 30].try_into().unwrap(),
        )),
        // Test shortcut: EvictionIterator at level 0
        make_config(ConfigSettingEntry::EvictionIterator(EvictionIterator {
            bucket_list_level: 0,
            is_curr_bucket: true,
            bucket_file_offset: 0,
        })),
    ];

    // Combine config + data + TTL entries into one add_batch call
    let all_entries: Vec<LedgerEntry> = config_entries
        .into_iter()
        .chain(vec![
            entry_a.clone(),
            entry_b.clone(),
            entry_c.clone(),
            entry_d.clone(),
            ttl_a,
            ttl_b,
            ttl_c,
            ttl_d,
        ])
        .collect();

    let mut bucket_list = BucketList::new();
    bucket_list
        .add_batch(1, 25, BucketListType::Live, all_entries, vec![], vec![])
        .expect("add_batch");

    // --- Initialize LedgerManager ---
    let config = LedgerManagerConfig {
        validate_bucket_hash: false,
        ..Default::default()
    };
    let ledger = LedgerManager::new("Test Network".to_string(), config);

    let hot_archive = HotArchiveBucketList::new();
    // Custom header at ledger_seq 9 so close produces ledger 10
    let header = LedgerHeader {
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
        ledger_seq: 9,
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
    };
    let header_hash = compute_header_hash(&header).expect("hash");
    ledger
        .initialize(bucket_list, hot_archive, header, header_hash)
        .expect("init");

    // --- Close ledger 10 with empty tx set ---
    let close_data = LedgerCloseData::new(
        10,
        TransactionSetVariant::Classic(TransactionSet {
            previous_ledger_hash: Hash([0u8; 32]),
            txs: VecM::default(),
        }),
        1,
        ledger.current_header_hash(),
    );

    let result = ledger.close_ledger(close_data, None).expect("close ledger");
    assert_eq!(result.header.ledger_seq, 10);

    // --- Extract and verify evicted_keys ordering ---
    let meta = result.meta.expect("ledger close meta");
    let evicted_keys = match meta {
        LedgerCloseMeta::V2(ref v2) => v2.evicted_keys.to_vec(),
        other => panic!("expected V2 meta, got {:?}", other),
    };

    // Expected two-phase ordering:
    // Phase 1 (deleted_keys): temp data + ALL TTL keys in scan order
    //   A is temp: A_data, A_ttl
    //   B is persistent: B_ttl only
    //   C is temp: C_data, C_ttl
    //   D is persistent: D_ttl only
    // Phase 2 (persistent data from archived_entries): B_data, D_data
    let expected = vec![
        key_a.clone(),       // A_data (temp)
        ttl_key_for(&key_a), // A_ttl
        ttl_key_for(&key_b), // B_ttl (persistent TTL still goes to deleted_keys)
        key_c.clone(),       // C_data (temp)
        ttl_key_for(&key_c), // C_ttl
        ttl_key_for(&key_d), // D_ttl
        key_b.clone(),       // B_data (persistent, from archived_entries)
        key_d.clone(),       // D_data (persistent, from archived_entries)
    ];

    assert_eq!(
        evicted_keys.len(),
        expected.len(),
        "evicted_keys count mismatch: got {}, expected {}",
        evicted_keys.len(),
        expected.len()
    );
    assert_eq!(
        evicted_keys, expected,
        "evicted_keys ordering must match stellar-core two-phase rule: \
         deleted_keys (temp data + all TTL) first, then persistent data keys"
    );
}
