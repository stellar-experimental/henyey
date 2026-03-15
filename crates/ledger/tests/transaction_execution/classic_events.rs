use super::*;

#[test]
fn test_classic_events_emitted_for_payment() {
    let secret = SecretKey::from_seed(&[21u8; 32]);
    let source_id: AccountId = (&secret.public_key()).into();
    let dest_id = AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([4u8; 32])));

    let (source_key, source_entry) = create_account_entry(source_id.clone(), 1, 20_000_000);
    let (dest_key, dest_entry) = create_account_entry(dest_id.clone(), 1, 1_000_000);
    let snapshot = SnapshotBuilder::new(1)
        .add_entry(source_key, source_entry)
        .add_entry(dest_key, dest_entry)
        .build_with_default_header();
    let snapshot = SnapshotHandle::new(snapshot);

    let operation = Operation {
        source_account: None,
        body: OperationBody::Payment(stellar_xdr::curr::PaymentOp {
            destination: MuxedAccount::Ed25519(Uint256([4u8; 32])),
            asset: stellar_xdr::curr::Asset::Native,
            amount: 100,
        }),
    };

    let tx = Transaction {
        source_account: MuxedAccount::Ed25519(Uint256(*secret.public_key().as_bytes())),
        fee: 100,
        seq_num: SequenceNumber(2),
        cond: Preconditions::None,
        memo: Memo::None,
        operations: vec![operation].try_into().unwrap(),
        ext: TransactionExt::V0,
    };

    let mut envelope = TransactionEnvelope::Tx(TransactionV1Envelope {
        tx,
        signatures: VecM::default(),
    });

    let network_id = NetworkId::testnet();
    let decorated = sign_envelope(&envelope, &secret, &network_id);
    if let TransactionEnvelope::Tx(ref mut env) = envelope {
        env.signatures = vec![decorated].try_into().unwrap();
    }

    let classic_events = ClassicEventConfig {
        emit_classic_events: true,
        backfill_stellar_asset_events: false,
    };
    let context = henyey_tx::LedgerContext::new(1, 1_000, 100, 5_000_000, 25, network_id);
    let mut executor = TransactionExecutor::new(
        &context,
        0,
        SorobanConfig::default(),
        classic_events,
    );
    let result = executor
        .execute_transaction(&snapshot, &envelope, 100, None)
        .expect("execute");

    assert!(
        result.success,
        "unexpected result: {:?}",
        result.operation_results
    );
    let tx_meta = result.tx_meta.expect("tx meta");
    let TransactionMeta::V4(meta) = tx_meta else {
        panic!("unexpected tx meta");
    };

    let tx_events: &[stellar_xdr::curr::TransactionEvent] = meta.events.as_ref();
    assert_eq!(tx_events.len(), 0);

    let contract_id = native_asset_contract_id(&network_id);
    let op_events: &[stellar_xdr::curr::OperationMetaV2] = meta.operations.as_ref();
    assert_eq!(op_events.len(), 1);
    let op_event_list: &[stellar_xdr::curr::ContractEvent] = op_events[0].events.as_ref();
    assert_eq!(op_event_list.len(), 1);
    let op_event = &op_event_list[0];
    assert_eq!(op_event.contract_id, Some(contract_id));
    let ContractEventBody::V0(op_body) = &op_event.body;
    let op_topics: &[ScVal] = op_body.topics.as_ref();
    assert_eq!(op_topics.len(), 4);
    assert_eq!(
        op_topics[0],
        ScVal::Symbol(ScSymbol(StringM::try_from("transfer").unwrap()))
    );
    assert_eq!(
        op_topics[1],
        ScVal::Address(ScAddress::Account(source_id.clone()))
    );
    assert_eq!(
        op_topics[2],
        ScVal::Address(ScAddress::Account(dest_id.clone()))
    );
    assert_eq!(
        op_topics[3],
        ScVal::String(ScString(StringM::try_from("native").unwrap()))
    );
    assert_eq!(op_body.data, ScVal::I128(i128_parts(100)));
}

#[test]
fn test_classic_events_payment_with_muxed_destination() {
    let secret = SecretKey::from_seed(&[41u8; 32]);
    let source_id: AccountId = (&secret.public_key()).into();
    let dest_account_id = AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([7u8; 32])));

    let (source_key, source_entry) = create_account_entry(source_id.clone(), 1, 20_000_000);
    let (dest_key, dest_entry) = create_account_entry(dest_account_id.clone(), 1, 1_000_000);
    let snapshot = SnapshotBuilder::new(1)
        .add_entry(source_key, source_entry)
        .add_entry(dest_key, dest_entry)
        .build_with_default_header();
    let snapshot = SnapshotHandle::new(snapshot);

    let muxed_dest = MuxedAccount::MuxedEd25519(MuxedAccountMed25519 {
        id: 42,
        ed25519: Uint256([7u8; 32]),
    });
    let operation = Operation {
        source_account: None,
        body: OperationBody::Payment(stellar_xdr::curr::PaymentOp {
            destination: muxed_dest,
            asset: stellar_xdr::curr::Asset::Native,
            amount: 200,
        }),
    };

    let tx = Transaction {
        source_account: MuxedAccount::Ed25519(Uint256(*secret.public_key().as_bytes())),
        fee: 100,
        seq_num: SequenceNumber(2),
        cond: Preconditions::None,
        memo: Memo::None,
        operations: vec![operation].try_into().unwrap(),
        ext: TransactionExt::V0,
    };

    let mut envelope = TransactionEnvelope::Tx(TransactionV1Envelope {
        tx,
        signatures: VecM::default(),
    });

    let network_id = NetworkId::testnet();
    let decorated = sign_envelope(&envelope, &secret, &network_id);
    if let TransactionEnvelope::Tx(ref mut env) = envelope {
        env.signatures = vec![decorated].try_into().unwrap();
    }

    let classic_events = ClassicEventConfig {
        emit_classic_events: true,
        backfill_stellar_asset_events: false,
    };
    let context = henyey_tx::LedgerContext::new(1, 1_000, 100, 5_000_000, 25, network_id);
    let mut executor = TransactionExecutor::new(
        &context,
        0,
        SorobanConfig::default(),
        classic_events,
    );
    let result = executor
        .execute_transaction(&snapshot, &envelope, 100, None)
        .expect("execute");

    assert!(
        result.success,
        "unexpected result: {:?}",
        result.operation_results
    );
    let tx_meta = result.tx_meta.expect("tx meta");
    let TransactionMeta::V4(meta) = tx_meta else {
        panic!("unexpected tx meta");
    };

    let op_events: &[stellar_xdr::curr::OperationMetaV2] = meta.operations.as_ref();
    assert_eq!(op_events.len(), 1);
    let op_event_list: &[stellar_xdr::curr::ContractEvent] = op_events[0].events.as_ref();
    assert_eq!(op_event_list.len(), 1);
    let op_event = &op_event_list[0];
    let ContractEventBody::V0(op_body) = &op_event.body;
    let ScVal::Map(Some(map)) = &op_body.data else {
        panic!("expected map data for muxed destination");
    };
    let entries: &[stellar_xdr::curr::ScMapEntry] = map.0.as_ref();
    assert_eq!(entries.len(), 2);
    let amount_entry = entries
        .iter()
        .find(|entry| entry.key == scval_symbol("amount"))
        .expect("amount entry");
    assert_eq!(amount_entry.val, ScVal::I128(i128_parts(200)));
    let muxed_entry = entries
        .iter()
        .find(|entry| entry.key == scval_symbol("to_muxed_id"))
        .expect("muxed entry");
    assert_eq!(muxed_entry.val, ScVal::U64(42));
}

#[test]
fn test_classic_events_payment_with_memo_data() {
    let secret = SecretKey::from_seed(&[51u8; 32]);
    let source_id: AccountId = (&secret.public_key()).into();
    let dest_id = AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([8u8; 32])));

    let (source_key, source_entry) = create_account_entry(source_id.clone(), 1, 20_000_000);
    let (dest_key, dest_entry) = create_account_entry(dest_id.clone(), 1, 1_000_000);
    let snapshot = SnapshotBuilder::new(1)
        .add_entry(source_key, source_entry)
        .add_entry(dest_key, dest_entry)
        .build_with_default_header();
    let snapshot = SnapshotHandle::new(snapshot);

    let operation = Operation {
        source_account: None,
        body: OperationBody::Payment(stellar_xdr::curr::PaymentOp {
            destination: MuxedAccount::Ed25519(Uint256([8u8; 32])),
            asset: stellar_xdr::curr::Asset::Native,
            amount: 150,
        }),
    };

    let memo_text = StringM::try_from("test memo").unwrap();
    let tx = Transaction {
        source_account: MuxedAccount::Ed25519(Uint256(*secret.public_key().as_bytes())),
        fee: 100,
        seq_num: SequenceNumber(2),
        cond: Preconditions::None,
        memo: Memo::Text(memo_text.clone()),
        operations: vec![operation].try_into().unwrap(),
        ext: TransactionExt::V0,
    };

    let mut envelope = TransactionEnvelope::Tx(TransactionV1Envelope {
        tx,
        signatures: VecM::default(),
    });

    let network_id = NetworkId::testnet();
    let decorated = sign_envelope(&envelope, &secret, &network_id);
    if let TransactionEnvelope::Tx(ref mut env) = envelope {
        env.signatures = vec![decorated].try_into().unwrap();
    }

    let classic_events = ClassicEventConfig {
        emit_classic_events: true,
        backfill_stellar_asset_events: false,
    };
    let context = henyey_tx::LedgerContext::new(1, 1_000, 100, 5_000_000, 25, network_id);
    let mut executor = TransactionExecutor::new(
        &context,
        0,
        SorobanConfig::default(),
        classic_events,
    );
    let result = executor
        .execute_transaction(&snapshot, &envelope, 100, None)
        .expect("execute");

    assert!(
        result.success,
        "unexpected result: {:?}",
        result.operation_results
    );
    let tx_meta = result.tx_meta.expect("tx meta");
    let TransactionMeta::V4(meta) = tx_meta else {
        panic!("unexpected tx meta");
    };

    let op_events: &[stellar_xdr::curr::OperationMetaV2] = meta.operations.as_ref();
    assert_eq!(op_events.len(), 1);
    let op_event_list: &[stellar_xdr::curr::ContractEvent] = op_events[0].events.as_ref();
    assert_eq!(op_event_list.len(), 1);
    let op_event = &op_event_list[0];
    let ContractEventBody::V0(op_body) = &op_event.body;
    let ScVal::Map(Some(map)) = &op_body.data else {
        panic!("expected map data for memo");
    };
    let entries: &[stellar_xdr::curr::ScMapEntry] = map.0.as_ref();
    assert_eq!(entries.len(), 2);
    let amount_entry = entries
        .iter()
        .find(|entry| entry.key == scval_symbol("amount"))
        .expect("amount entry");
    assert_eq!(amount_entry.val, ScVal::I128(i128_parts(150)));
    let memo_entry = entries
        .iter()
        .find(|entry| entry.key == scval_symbol("to_muxed_id"))
        .expect("memo entry");
    let expected_memo = ScVal::String(ScString(StringM::try_from("test memo").unwrap()));
    assert_eq!(memo_entry.val, expected_memo);
}

#[test]
fn test_classic_events_payment_memo_precedence() {
    let secret = SecretKey::from_seed(&[61u8; 32]);
    let source_id: AccountId = (&secret.public_key()).into();
    let dest_account_id = AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([9u8; 32])));

    let (source_key, source_entry) = create_account_entry(source_id.clone(), 1, 20_000_000);
    let (dest_key, dest_entry) = create_account_entry(dest_account_id.clone(), 1, 1_000_000);
    let snapshot = SnapshotBuilder::new(1)
        .add_entry(source_key, source_entry)
        .add_entry(dest_key, dest_entry)
        .build_with_default_header();
    let snapshot = SnapshotHandle::new(snapshot);

    let muxed_dest = MuxedAccount::MuxedEd25519(MuxedAccountMed25519 {
        id: 77,
        ed25519: Uint256([9u8; 32]),
    });
    let operation = Operation {
        source_account: None,
        body: OperationBody::Payment(stellar_xdr::curr::PaymentOp {
            destination: muxed_dest,
            asset: stellar_xdr::curr::Asset::Native,
            amount: 250,
        }),
    };

    let memo_text = StringM::try_from("memo wins?").unwrap();
    let tx = Transaction {
        source_account: MuxedAccount::Ed25519(Uint256(*secret.public_key().as_bytes())),
        fee: 100,
        seq_num: SequenceNumber(2),
        cond: Preconditions::None,
        memo: Memo::Text(memo_text),
        operations: vec![operation].try_into().unwrap(),
        ext: TransactionExt::V0,
    };

    let mut envelope = TransactionEnvelope::Tx(TransactionV1Envelope {
        tx,
        signatures: VecM::default(),
    });

    let network_id = NetworkId::testnet();
    let decorated = sign_envelope(&envelope, &secret, &network_id);
    if let TransactionEnvelope::Tx(ref mut env) = envelope {
        env.signatures = vec![decorated].try_into().unwrap();
    }

    let classic_events = ClassicEventConfig {
        emit_classic_events: true,
        backfill_stellar_asset_events: false,
    };
    let context = henyey_tx::LedgerContext::new(1, 1_000, 100, 5_000_000, 25, network_id);
    let mut executor = TransactionExecutor::new(
        &context,
        0,
        SorobanConfig::default(),
        classic_events,
    );
    let result = executor
        .execute_transaction(&snapshot, &envelope, 100, None)
        .expect("execute");

    assert!(
        result.success,
        "unexpected result: {:?}",
        result.operation_results
    );
    let tx_meta = result.tx_meta.expect("tx meta");
    let TransactionMeta::V4(meta) = tx_meta else {
        panic!("unexpected tx meta");
    };

    let op_events: &[stellar_xdr::curr::OperationMetaV2] = meta.operations.as_ref();
    assert_eq!(op_events.len(), 1);
    let op_event_list: &[stellar_xdr::curr::ContractEvent] = op_events[0].events.as_ref();
    assert_eq!(op_event_list.len(), 1);
    let op_event = &op_event_list[0];
    let ContractEventBody::V0(op_body) = &op_event.body;
    let ScVal::Map(Some(map)) = &op_body.data else {
        panic!("expected map data for muxed destination");
    };
    let entries: &[stellar_xdr::curr::ScMapEntry] = map.0.as_ref();
    assert_eq!(entries.len(), 2);
    let muxed_entry = entries
        .iter()
        .find(|entry| entry.key == scval_symbol("to_muxed_id"))
        .expect("muxed entry");
    assert_eq!(muxed_entry.val, ScVal::U64(77));
}

#[test]
fn test_classic_events_emitted_for_account_merge() {
    let secret = SecretKey::from_seed(&[71u8; 32]);
    let source_id: AccountId = (&secret.public_key()).into();
    let dest_id = AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([10u8; 32])));

    let source_balance = 20_000_000;
    let (source_key, source_entry) = create_account_entry(source_id.clone(), 1, source_balance);
    let (dest_key, dest_entry) = create_account_entry(dest_id.clone(), 1, 1_000_000);
    let snapshot = SnapshotBuilder::new(1)
        .add_entry(source_key, source_entry)
        .add_entry(dest_key, dest_entry)
        .build_with_default_header();
    let snapshot = SnapshotHandle::new(snapshot);

    let operation = Operation {
        source_account: None,
        body: OperationBody::AccountMerge(MuxedAccount::Ed25519(Uint256([10u8; 32]))),
    };

    let tx = Transaction {
        source_account: MuxedAccount::Ed25519(Uint256(*secret.public_key().as_bytes())),
        fee: 100,
        seq_num: SequenceNumber(2),
        cond: Preconditions::None,
        memo: Memo::None,
        operations: vec![operation].try_into().unwrap(),
        ext: TransactionExt::V0,
    };

    let mut envelope = TransactionEnvelope::Tx(TransactionV1Envelope {
        tx,
        signatures: VecM::default(),
    });

    let network_id = NetworkId::testnet();
    let decorated = sign_envelope(&envelope, &secret, &network_id);
    if let TransactionEnvelope::Tx(ref mut env) = envelope {
        env.signatures = vec![decorated].try_into().unwrap();
    }

    let classic_events = ClassicEventConfig {
        emit_classic_events: true,
        backfill_stellar_asset_events: false,
    };
    let context = henyey_tx::LedgerContext::new(1, 1_000, 100, 5_000_000, 25, network_id);
    let mut executor = TransactionExecutor::new(
        &context,
        0,
        SorobanConfig::default(),
        classic_events,
    );
    let result = executor
        .execute_transaction(&snapshot, &envelope, 100, None)
        .expect("execute");

    assert!(
        result.success,
        "unexpected result: {:?}",
        result.operation_results
    );
    let tx_meta = result.tx_meta.expect("tx meta");
    let TransactionMeta::V4(meta) = tx_meta else {
        panic!("unexpected tx meta");
    };

    let op_events: &[stellar_xdr::curr::OperationMetaV2] = meta.operations.as_ref();
    assert_eq!(op_events.len(), 1);
    let op_event_list: &[stellar_xdr::curr::ContractEvent] = op_events[0].events.as_ref();
    assert_eq!(op_event_list.len(), 1);
    let op_event = &op_event_list[0];
    let ContractEventBody::V0(op_body) = &op_event.body;
    let op_topics: &[ScVal] = op_body.topics.as_ref();
    assert_eq!(op_topics.len(), 4);
    assert_eq!(op_topics[0], scval_symbol("transfer"));
    assert_eq!(
        op_topics[1],
        ScVal::Address(ScAddress::Account(source_id.clone()))
    );
    assert_eq!(
        op_topics[2],
        ScVal::Address(ScAddress::Account(dest_id.clone()))
    );
    assert_eq!(
        op_topics[3],
        ScVal::String(ScString(StringM::try_from("native").unwrap()))
    );
    assert_eq!(
        op_body.data,
        ScVal::I128(i128_parts(i128::from(source_balance - 100)))
    );
}

#[test]
fn test_classic_events_emitted_for_create_account() {
    let secret = SecretKey::from_seed(&[81u8; 32]);
    let source_id: AccountId = (&secret.public_key()).into();
    let dest_id = AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([11u8; 32])));

    let (source_key, source_entry) = create_account_entry(source_id.clone(), 1, 200_000_000);
    let snapshot = SnapshotBuilder::new(1)
        .add_entry(source_key, source_entry)
        .build_with_default_header();
    let snapshot = SnapshotHandle::new(snapshot);

    let operation = Operation {
        source_account: None,
        body: OperationBody::CreateAccount(CreateAccountOp {
            destination: dest_id.clone(),
            starting_balance: 20_000_000,
        }),
    };

    let tx = Transaction {
        source_account: MuxedAccount::Ed25519(Uint256(*secret.public_key().as_bytes())),
        fee: 100,
        seq_num: SequenceNumber(2),
        cond: Preconditions::None,
        memo: Memo::None,
        operations: vec![operation].try_into().unwrap(),
        ext: TransactionExt::V0,
    };

    let mut envelope = TransactionEnvelope::Tx(TransactionV1Envelope {
        tx,
        signatures: VecM::default(),
    });

    let network_id = NetworkId::testnet();
    let decorated = sign_envelope(&envelope, &secret, &network_id);
    if let TransactionEnvelope::Tx(ref mut env) = envelope {
        env.signatures = vec![decorated].try_into().unwrap();
    }

    let classic_events = ClassicEventConfig {
        emit_classic_events: true,
        backfill_stellar_asset_events: false,
    };
    let context = henyey_tx::LedgerContext::new(1, 1_000, 100, 5_000_000, 25, network_id);
    let mut executor = TransactionExecutor::new(
        &context,
        0,
        SorobanConfig::default(),
        classic_events,
    );
    let result = executor
        .execute_transaction(&snapshot, &envelope, 100, None)
        .expect("execute");

    assert!(
        result.success,
        "unexpected result: {:?}",
        result.operation_results
    );
    let tx_meta = result.tx_meta.expect("tx meta");
    let TransactionMeta::V4(meta) = tx_meta else {
        panic!("unexpected tx meta");
    };

    let op_events: &[stellar_xdr::curr::OperationMetaV2] = meta.operations.as_ref();
    assert_eq!(op_events.len(), 1);
    let op_event_list: &[stellar_xdr::curr::ContractEvent] = op_events[0].events.as_ref();
    assert_eq!(op_event_list.len(), 1);
    let op_event = &op_event_list[0];
    let ContractEventBody::V0(op_body) = &op_event.body;
    let op_topics: &[ScVal] = op_body.topics.as_ref();
    assert_eq!(op_topics.len(), 4);
    assert_eq!(op_topics[0], scval_symbol("transfer"));
    assert_eq!(
        op_topics[1],
        ScVal::Address(ScAddress::Account(source_id.clone()))
    );
    assert_eq!(
        op_topics[2],
        ScVal::Address(ScAddress::Account(dest_id.clone()))
    );
    assert_eq!(
        op_topics[3],
        ScVal::String(ScString(StringM::try_from("native").unwrap()))
    );
    assert_eq!(op_body.data, ScVal::I128(i128_parts(20_000_000)));
}

#[test]
fn test_classic_events_emitted_for_create_claimable_balance() {
    let secret = SecretKey::from_seed(&[91u8; 32]);
    let source_id: AccountId = (&secret.public_key()).into();
    let claimant_id = AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([12u8; 32])));

    let (source_key, source_entry) = create_account_entry(source_id.clone(), 1, 200_000_000);
    let snapshot = SnapshotBuilder::new(1)
        .add_entry(source_key, source_entry)
        .build_with_default_header();
    let snapshot = SnapshotHandle::new(snapshot);

    let claimant = Claimant::ClaimantTypeV0(ClaimantV0 {
        destination: claimant_id,
        predicate: ClaimPredicate::Unconditional,
    });
    let operation = Operation {
        source_account: None,
        body: OperationBody::CreateClaimableBalance(CreateClaimableBalanceOp {
            asset: Asset::Native,
            amount: 20_000_000,
            claimants: vec![claimant].try_into().unwrap(),
        }),
    };

    let tx = Transaction {
        source_account: MuxedAccount::Ed25519(Uint256(*secret.public_key().as_bytes())),
        fee: 100,
        seq_num: SequenceNumber(2),
        cond: Preconditions::None,
        memo: Memo::None,
        operations: vec![operation].try_into().unwrap(),
        ext: TransactionExt::V0,
    };

    let mut envelope = TransactionEnvelope::Tx(TransactionV1Envelope {
        tx,
        signatures: VecM::default(),
    });

    let network_id = NetworkId::testnet();
    let decorated = sign_envelope(&envelope, &secret, &network_id);
    if let TransactionEnvelope::Tx(ref mut env) = envelope {
        env.signatures = vec![decorated].try_into().unwrap();
    }

    let classic_events = ClassicEventConfig {
        emit_classic_events: true,
        backfill_stellar_asset_events: false,
    };
    let context = henyey_tx::LedgerContext::new(1, 1_000, 100, 5_000_000, 25, network_id);
    let mut executor = TransactionExecutor::new(
        &context,
        0,
        SorobanConfig::default(),
        classic_events,
    );
    let result = executor
        .execute_transaction(&snapshot, &envelope, 100, None)
        .expect("execute");

    assert!(
        result.success,
        "unexpected result: {:?}",
        result.operation_results
    );
    let balance_id = match result.operation_results.get(0).expect("operation result") {
        OperationResult::OpInner(OperationResultTr::CreateClaimableBalance(
            CreateClaimableBalanceResult::Success(balance_id),
        )) => balance_id.clone(),
        other => panic!("unexpected result: {:?}", other),
    };

    let tx_meta = result.tx_meta.expect("tx meta");
    let TransactionMeta::V4(meta) = tx_meta else {
        panic!("unexpected tx meta");
    };

    let op_events: &[stellar_xdr::curr::OperationMetaV2] = meta.operations.as_ref();
    assert_eq!(op_events.len(), 1);
    let op_event_list: &[stellar_xdr::curr::ContractEvent] = op_events[0].events.as_ref();
    assert_eq!(op_event_list.len(), 1);
    let op_event = &op_event_list[0];
    let ContractEventBody::V0(op_body) = &op_event.body;
    let op_topics: &[ScVal] = op_body.topics.as_ref();
    assert_eq!(op_topics.len(), 4);
    assert_eq!(op_topics[0], scval_symbol("transfer"));
    assert_eq!(
        op_topics[1],
        ScVal::Address(ScAddress::Account(source_id.clone()))
    );
    assert_eq!(
        op_topics[2],
        ScVal::Address(ScAddress::ClaimableBalance(balance_id.clone()))
    );
    assert_eq!(
        op_topics[3],
        ScVal::String(ScString(StringM::try_from("native").unwrap()))
    );
    assert_eq!(op_body.data, ScVal::I128(i128_parts(20_000_000)));
}

#[test]
fn test_classic_events_emitted_for_claim_claimable_balance() {
    let secret = SecretKey::from_seed(&[92u8; 32]);
    let source_id: AccountId = (&secret.public_key()).into();
    let balance_id = ClaimableBalanceId::ClaimableBalanceIdTypeV0(Hash([13u8; 32]));

    let (source_key, source_entry) = create_account_entry(source_id.clone(), 1, 20_000_000);
    let claimants: VecM<Claimant, 10> = vec![Claimant::ClaimantTypeV0(ClaimantV0 {
        destination: source_id.clone(),
        predicate: ClaimPredicate::Unconditional,
    })]
    .try_into()
    .unwrap();
    let claimable_entry = LedgerEntry {
        last_modified_ledger_seq: 1,
        data: LedgerEntryData::ClaimableBalance(ClaimableBalanceEntry {
            balance_id: balance_id.clone(),
            claimants,
            asset: Asset::Native,
            amount: 20_000_000,
            ext: ClaimableBalanceEntryExt::V0,
        }),
        ext: LedgerEntryExt::V0,
    };
    let claimable_key = LedgerKey::ClaimableBalance(LedgerKeyClaimableBalance {
        balance_id: balance_id.clone(),
    });

    let snapshot = SnapshotBuilder::new(1)
        .add_entry(source_key, source_entry)
        .add_entry(claimable_key, claimable_entry)
        .build_with_default_header();
    let snapshot = SnapshotHandle::new(snapshot);

    let operation = Operation {
        source_account: None,
        body: OperationBody::ClaimClaimableBalance(ClaimClaimableBalanceOp {
            balance_id: balance_id.clone(),
        }),
    };

    let tx = Transaction {
        source_account: MuxedAccount::Ed25519(Uint256(*secret.public_key().as_bytes())),
        fee: 100,
        seq_num: SequenceNumber(2),
        cond: Preconditions::None,
        memo: Memo::None,
        operations: vec![operation].try_into().unwrap(),
        ext: TransactionExt::V0,
    };

    let mut envelope = TransactionEnvelope::Tx(TransactionV1Envelope {
        tx,
        signatures: VecM::default(),
    });

    let network_id = NetworkId::testnet();
    let decorated = sign_envelope(&envelope, &secret, &network_id);
    if let TransactionEnvelope::Tx(ref mut env) = envelope {
        env.signatures = vec![decorated].try_into().unwrap();
    }

    let classic_events = ClassicEventConfig {
        emit_classic_events: true,
        backfill_stellar_asset_events: false,
    };
    let context = henyey_tx::LedgerContext::new(1, 1_000, 100, 5_000_000, 25, network_id);
    let mut executor = TransactionExecutor::new(
        &context,
        0,
        SorobanConfig::default(),
        classic_events,
    );
    let result = executor
        .execute_transaction(&snapshot, &envelope, 100, None)
        .expect("execute");

    assert!(
        result.success,
        "unexpected result: {:?}",
        result.operation_results
    );
    let tx_meta = result.tx_meta.expect("tx meta");
    let TransactionMeta::V4(meta) = tx_meta else {
        panic!("unexpected tx meta");
    };

    let op_events: &[stellar_xdr::curr::OperationMetaV2] = meta.operations.as_ref();
    assert_eq!(op_events.len(), 1);
    let op_event_list: &[stellar_xdr::curr::ContractEvent] = op_events[0].events.as_ref();
    assert_eq!(op_event_list.len(), 1);
    let op_event = &op_event_list[0];
    let ContractEventBody::V0(op_body) = &op_event.body;
    let op_topics: &[ScVal] = op_body.topics.as_ref();
    assert_eq!(op_topics.len(), 4);
    assert_eq!(op_topics[0], scval_symbol("transfer"));
    assert_eq!(
        op_topics[1],
        ScVal::Address(ScAddress::ClaimableBalance(balance_id.clone()))
    );
    assert_eq!(
        op_topics[2],
        ScVal::Address(ScAddress::Account(source_id.clone()))
    );
    assert_eq!(
        op_topics[3],
        ScVal::String(ScString(StringM::try_from("native").unwrap()))
    );
    assert_eq!(op_body.data, ScVal::I128(i128_parts(20_000_000)));
}

#[test]
fn test_classic_events_emitted_for_allow_trust() {
    let issuer_secret = SecretKey::from_seed(&[93u8; 32]);
    let issuer_id: AccountId = (&issuer_secret.public_key()).into();
    let trustor_id = AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([14u8; 32])));

    let asset_code = AssetCode4([b'U', b'S', b'D', 0]);
    let asset = Asset::CreditAlphanum4(AlphaNum4 {
        asset_code: asset_code.clone(),
        issuer: issuer_id.clone(),
    });
    let trustline_asset = TrustLineAsset::CreditAlphanum4(AlphaNum4 {
        asset_code: asset_code.clone(),
        issuer: issuer_id.clone(),
    });

    let (issuer_key, issuer_entry) =
        create_account_entry_with_flags(issuer_id.clone(), 1, 50_000_000, 0x1);
    let (trustor_key, trustor_entry) = create_account_entry(trustor_id.clone(), 1, 20_000_000);
    let (trustline_key, trustline_entry) =
        create_trustline_entry(trustor_id.clone(), trustline_asset, 0, 100_000_000, 0);

    let snapshot = SnapshotBuilder::new(1)
        .add_entry(issuer_key, issuer_entry)
        .add_entry(trustor_key, trustor_entry)
        .add_entry(trustline_key, trustline_entry)
        .build_with_default_header();
    let snapshot = SnapshotHandle::new(snapshot);

    let operation = Operation {
        source_account: None,
        body: OperationBody::AllowTrust(AllowTrustOp {
            trustor: trustor_id.clone(),
            asset: AssetCode::CreditAlphanum4(asset_code.clone()),
            authorize: 1,
        }),
    };

    let tx = Transaction {
        source_account: MuxedAccount::Ed25519(Uint256(*issuer_secret.public_key().as_bytes())),
        fee: 100,
        seq_num: SequenceNumber(2),
        cond: Preconditions::None,
        memo: Memo::None,
        operations: vec![operation].try_into().unwrap(),
        ext: TransactionExt::V0,
    };

    let mut envelope = TransactionEnvelope::Tx(TransactionV1Envelope {
        tx,
        signatures: VecM::default(),
    });

    let network_id = NetworkId::testnet();
    let decorated = sign_envelope(&envelope, &issuer_secret, &network_id);
    if let TransactionEnvelope::Tx(ref mut env) = envelope {
        env.signatures = vec![decorated].try_into().unwrap();
    }

    let classic_events = ClassicEventConfig {
        emit_classic_events: true,
        backfill_stellar_asset_events: false,
    };
    let context = henyey_tx::LedgerContext::new(1, 1_000, 100, 5_000_000, 25, network_id);
    let mut executor = TransactionExecutor::new(
        &context,
        0,
        SorobanConfig::default(),
        classic_events,
    );
    let result = executor
        .execute_transaction(&snapshot, &envelope, 100, None)
        .expect("execute");

    assert!(
        result.success,
        "unexpected result: {:?}",
        result.operation_results
    );
    let tx_meta = result.tx_meta.expect("tx meta");
    let TransactionMeta::V4(meta) = tx_meta else {
        panic!("unexpected tx meta");
    };

    let op_events: &[stellar_xdr::curr::OperationMetaV2] = meta.operations.as_ref();
    assert_eq!(op_events.len(), 1);
    let op_event_list: &[stellar_xdr::curr::ContractEvent] = op_events[0].events.as_ref();
    assert_eq!(op_event_list.len(), 1);
    let op_event = &op_event_list[0];
    let ContractEventBody::V0(op_body) = &op_event.body;
    let op_topics: &[ScVal] = op_body.topics.as_ref();
    assert_eq!(op_topics.len(), 3);
    assert_eq!(op_topics[0], scval_symbol("set_authorized"));
    assert_eq!(
        op_topics[1],
        ScVal::Address(ScAddress::Account(trustor_id.clone()))
    );
    assert_eq!(op_topics[2], asset_string_scval(&asset));
    assert_eq!(op_body.data, ScVal::Bool(true));
}

#[test]
fn test_classic_events_emitted_for_set_trustline_flags() {
    let issuer_secret = SecretKey::from_seed(&[94u8; 32]);
    let issuer_id: AccountId = (&issuer_secret.public_key()).into();
    let trustor_id = AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([15u8; 32])));

    let asset_code = AssetCode4([b'U', b'S', b'D', 0]);
    let asset = Asset::CreditAlphanum4(AlphaNum4 {
        asset_code: asset_code.clone(),
        issuer: issuer_id.clone(),
    });
    let trustline_asset = TrustLineAsset::CreditAlphanum4(AlphaNum4 {
        asset_code: asset_code.clone(),
        issuer: issuer_id.clone(),
    });

    let (issuer_key, issuer_entry) = create_account_entry(issuer_id.clone(), 1, 50_000_000);
    let (trustor_key, trustor_entry) = create_account_entry(trustor_id.clone(), 1, 20_000_000);
    let (trustline_key, trustline_entry) =
        create_trustline_entry(trustor_id.clone(), trustline_asset, 0, 100_000_000, 0);

    let snapshot = SnapshotBuilder::new(1)
        .add_entry(issuer_key, issuer_entry)
        .add_entry(trustor_key, trustor_entry)
        .add_entry(trustline_key, trustline_entry)
        .build_with_default_header();
    let snapshot = SnapshotHandle::new(snapshot);

    let operation = Operation {
        source_account: None,
        body: OperationBody::SetTrustLineFlags(SetTrustLineFlagsOp {
            trustor: trustor_id.clone(),
            asset: asset.clone(),
            clear_flags: 0,
            set_flags: TrustLineFlags::AuthorizedFlag as u32,
        }),
    };

    let tx = Transaction {
        source_account: MuxedAccount::Ed25519(Uint256(*issuer_secret.public_key().as_bytes())),
        fee: 100,
        seq_num: SequenceNumber(2),
        cond: Preconditions::None,
        memo: Memo::None,
        operations: vec![operation].try_into().unwrap(),
        ext: TransactionExt::V0,
    };

    let mut envelope = TransactionEnvelope::Tx(TransactionV1Envelope {
        tx,
        signatures: VecM::default(),
    });

    let network_id = NetworkId::testnet();
    let decorated = sign_envelope(&envelope, &issuer_secret, &network_id);
    if let TransactionEnvelope::Tx(ref mut env) = envelope {
        env.signatures = vec![decorated].try_into().unwrap();
    }

    let classic_events = ClassicEventConfig {
        emit_classic_events: true,
        backfill_stellar_asset_events: false,
    };
    let context = henyey_tx::LedgerContext::new(1, 1_000, 100, 5_000_000, 25, network_id);
    let mut executor = TransactionExecutor::new(
        &context,
        0,
        SorobanConfig::default(),
        classic_events,
    );
    let result = executor
        .execute_transaction(&snapshot, &envelope, 100, None)
        .expect("execute");

    assert!(
        result.success,
        "unexpected result: {:?}",
        result.operation_results
    );
    let tx_meta = result.tx_meta.expect("tx meta");
    let TransactionMeta::V4(meta) = tx_meta else {
        panic!("unexpected tx meta");
    };

    let op_events: &[stellar_xdr::curr::OperationMetaV2] = meta.operations.as_ref();
    assert_eq!(op_events.len(), 1);
    let op_event_list: &[stellar_xdr::curr::ContractEvent] = op_events[0].events.as_ref();
    assert_eq!(op_event_list.len(), 1);
    let op_event = &op_event_list[0];
    let ContractEventBody::V0(op_body) = &op_event.body;
    let op_topics: &[ScVal] = op_body.topics.as_ref();
    assert_eq!(op_topics.len(), 3);
    assert_eq!(op_topics[0], scval_symbol("set_authorized"));
    assert_eq!(
        op_topics[1],
        ScVal::Address(ScAddress::Account(trustor_id.clone()))
    );
    assert_eq!(op_topics[2], asset_string_scval(&asset));
    assert_eq!(op_body.data, ScVal::Bool(true));
}

#[test]
fn test_classic_events_emitted_for_clawback() {
    let issuer_secret = SecretKey::from_seed(&[95u8; 32]);
    let issuer_id: AccountId = (&issuer_secret.public_key()).into();
    let trustor_id = AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([16u8; 32])));

    let asset_code = AssetCode4([b'U', b'S', b'D', 0]);
    let asset = Asset::CreditAlphanum4(AlphaNum4 {
        asset_code: asset_code.clone(),
        issuer: issuer_id.clone(),
    });
    let trustline_asset = TrustLineAsset::CreditAlphanum4(AlphaNum4 {
        asset_code: asset_code.clone(),
        issuer: issuer_id.clone(),
    });

    let (issuer_key, issuer_entry) =
        create_account_entry_with_flags(issuer_id.clone(), 1, 50_000_000, 0x8);
    let (trustor_key, trustor_entry) = create_account_entry(trustor_id.clone(), 1, 20_000_000);
    // Flags: AUTHORIZED_FLAG (0x1) | TRUSTLINE_CLAWBACK_ENABLED_FLAG (0x4) = 0x5
    let (trustline_key, trustline_entry) = create_trustline_entry(
        trustor_id.clone(),
        trustline_asset,
        50_000_000,
        100_000_000,
        0x5,
    );

    let snapshot = SnapshotBuilder::new(1)
        .add_entry(issuer_key, issuer_entry)
        .add_entry(trustor_key, trustor_entry)
        .add_entry(trustline_key, trustline_entry)
        .build_with_default_header();
    let snapshot = SnapshotHandle::new(snapshot);

    let operation = Operation {
        source_account: None,
        body: OperationBody::Clawback(ClawbackOp {
            asset: asset.clone(),
            from: MuxedAccount::Ed25519(Uint256([16u8; 32])),
            amount: 20_000_000,
        }),
    };

    let tx = Transaction {
        source_account: MuxedAccount::Ed25519(Uint256(*issuer_secret.public_key().as_bytes())),
        fee: 100,
        seq_num: SequenceNumber(2),
        cond: Preconditions::None,
        memo: Memo::None,
        operations: vec![operation].try_into().unwrap(),
        ext: TransactionExt::V0,
    };

    let mut envelope = TransactionEnvelope::Tx(TransactionV1Envelope {
        tx,
        signatures: VecM::default(),
    });

    let network_id = NetworkId::testnet();
    let decorated = sign_envelope(&envelope, &issuer_secret, &network_id);
    if let TransactionEnvelope::Tx(ref mut env) = envelope {
        env.signatures = vec![decorated].try_into().unwrap();
    }

    let classic_events = ClassicEventConfig {
        emit_classic_events: true,
        backfill_stellar_asset_events: false,
    };
    let context = henyey_tx::LedgerContext::new(1, 1_000, 100, 5_000_000, 25, network_id);
    let mut executor = TransactionExecutor::new(
        &context,
        0,
        SorobanConfig::default(),
        classic_events,
    );
    let result = executor
        .execute_transaction(&snapshot, &envelope, 100, None)
        .expect("execute");

    assert!(result.success);
    let tx_meta = result.tx_meta.expect("tx meta");
    let TransactionMeta::V4(meta) = tx_meta else {
        panic!("unexpected tx meta");
    };

    let op_events: &[stellar_xdr::curr::OperationMetaV2] = meta.operations.as_ref();
    assert_eq!(op_events.len(), 1);
    let op_event_list: &[stellar_xdr::curr::ContractEvent] = op_events[0].events.as_ref();
    assert_eq!(op_event_list.len(), 1);
    let op_event = &op_event_list[0];
    let ContractEventBody::V0(op_body) = &op_event.body;
    let op_topics: &[ScVal] = op_body.topics.as_ref();
    assert_eq!(op_topics.len(), 3);
    assert_eq!(op_topics[0], scval_symbol("clawback"));
    assert_eq!(
        op_topics[1],
        ScVal::Address(ScAddress::Account(trustor_id.clone()))
    );
    assert_eq!(op_topics[2], asset_string_scval(&asset));
    assert_eq!(op_body.data, ScVal::I128(i128_parts(20_000_000)));
}

#[test]
fn test_classic_events_emitted_for_clawback_claimable_balance() {
    let issuer_secret = SecretKey::from_seed(&[96u8; 32]);
    let issuer_id: AccountId = (&issuer_secret.public_key()).into();
    let claimant_id = AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([17u8; 32])));
    let balance_id = ClaimableBalanceId::ClaimableBalanceIdTypeV0(Hash([18u8; 32]));

    let asset_code = AssetCode4([b'U', b'S', b'D', 0]);
    let asset = Asset::CreditAlphanum4(AlphaNum4 {
        asset_code: asset_code.clone(),
        issuer: issuer_id.clone(),
    });

    let (issuer_key, issuer_entry) =
        create_account_entry_with_flags(issuer_id.clone(), 1, 50_000_000, 0x8);

    let claimants: VecM<Claimant, 10> = vec![Claimant::ClaimantTypeV0(ClaimantV0 {
        destination: claimant_id,
        predicate: ClaimPredicate::Unconditional,
    })]
    .try_into()
    .unwrap();
    let claimable_entry = LedgerEntry {
        last_modified_ledger_seq: 1,
        data: LedgerEntryData::ClaimableBalance(ClaimableBalanceEntry {
            balance_id: balance_id.clone(),
            claimants,
            asset: asset.clone(),
            amount: 20_000_000,
            ext: ClaimableBalanceEntryExt::V1(ClaimableBalanceEntryExtensionV1 {
                ext: ClaimableBalanceEntryExtensionV1Ext::V0,
                flags: ClaimableBalanceFlags::ClaimableBalanceClawbackEnabledFlag as u32,
            }),
        }),
        ext: LedgerEntryExt::V0,
    };
    let claimable_key = LedgerKey::ClaimableBalance(LedgerKeyClaimableBalance {
        balance_id: balance_id.clone(),
    });

    let snapshot = SnapshotBuilder::new(1)
        .add_entry(issuer_key, issuer_entry)
        .add_entry(claimable_key, claimable_entry)
        .build_with_default_header();
    let snapshot = SnapshotHandle::new(snapshot);

    let operation = Operation {
        source_account: None,
        body: OperationBody::ClawbackClaimableBalance(ClawbackClaimableBalanceOp {
            balance_id: balance_id.clone(),
        }),
    };

    let tx = Transaction {
        source_account: MuxedAccount::Ed25519(Uint256(*issuer_secret.public_key().as_bytes())),
        fee: 100,
        seq_num: SequenceNumber(2),
        cond: Preconditions::None,
        memo: Memo::None,
        operations: vec![operation].try_into().unwrap(),
        ext: TransactionExt::V0,
    };

    let mut envelope = TransactionEnvelope::Tx(TransactionV1Envelope {
        tx,
        signatures: VecM::default(),
    });

    let network_id = NetworkId::testnet();
    let decorated = sign_envelope(&envelope, &issuer_secret, &network_id);
    if let TransactionEnvelope::Tx(ref mut env) = envelope {
        env.signatures = vec![decorated].try_into().unwrap();
    }

    let classic_events = ClassicEventConfig {
        emit_classic_events: true,
        backfill_stellar_asset_events: false,
    };
    let context = henyey_tx::LedgerContext::new(1, 1_000, 100, 5_000_000, 25, network_id);
    let mut executor = TransactionExecutor::new(
        &context,
        0,
        SorobanConfig::default(),
        classic_events,
    );
    let result = executor
        .execute_transaction(&snapshot, &envelope, 100, None)
        .expect("execute");

    assert!(result.success);
    let tx_meta = result.tx_meta.expect("tx meta");
    let TransactionMeta::V4(meta) = tx_meta else {
        panic!("unexpected tx meta");
    };

    let op_events: &[stellar_xdr::curr::OperationMetaV2] = meta.operations.as_ref();
    assert_eq!(op_events.len(), 1);
    let op_event_list: &[stellar_xdr::curr::ContractEvent] = op_events[0].events.as_ref();
    assert_eq!(op_event_list.len(), 1);
    let op_event = &op_event_list[0];
    let ContractEventBody::V0(op_body) = &op_event.body;
    let op_topics: &[ScVal] = op_body.topics.as_ref();
    assert_eq!(op_topics.len(), 3);
    assert_eq!(op_topics[0], scval_symbol("clawback"));
    assert_eq!(
        op_topics[1],
        ScVal::Address(ScAddress::ClaimableBalance(balance_id.clone()))
    );
    assert_eq!(op_topics[2], asset_string_scval(&asset));
    assert_eq!(op_body.data, ScVal::I128(i128_parts(20_000_000)));
}

#[test]
fn test_classic_events_emitted_for_liquidity_pool_deposit() {
    let source_secret = SecretKey::from_seed(&[97u8; 32]);
    let source_id: AccountId = (&source_secret.public_key()).into();
    let issuer_secret = SecretKey::from_seed(&[18u8; 32]);
    let issuer_id: AccountId = (&issuer_secret.public_key()).into();

    let asset_a = Asset::Native;
    let asset_b = Asset::CreditAlphanum4(AlphaNum4 {
        asset_code: AssetCode4([b'U', b'S', b'D', 0]),
        issuer: issuer_id.clone(),
    });

    let pool_id = PoolId(Hash([19u8; 32]));
    let (source_key, source_entry) = create_account_entry(source_id.clone(), 1, 500_000_000);
    let (issuer_key, issuer_entry) = create_account_entry(issuer_id.clone(), 1, 100_000_000);
    let (pool_key, pool_entry) = create_liquidity_pool_entry(
        pool_id.clone(),
        asset_a.clone(),
        asset_b.clone(),
        0,
        0,
        0,
        1,
    );
    let (asset_b_key, asset_b_entry) = create_trustline_entry(
        source_id.clone(),
        TrustLineAsset::CreditAlphanum4(match &asset_b {
            Asset::CreditAlphanum4(a) => a.clone(),
            _ => unreachable!(),
        }),
        50_000_000,
        100_000_000,
        TrustLineFlags::AuthorizedFlag as u32,
    );
    let (pool_share_key, pool_share_entry) = create_trustline_entry(
        source_id.clone(),
        TrustLineAsset::PoolShare(pool_id.clone()),
        0,
        100_000_000,
        0,
    );

    let snapshot = SnapshotBuilder::new(1)
        .add_entry(source_key, source_entry)
        .add_entry(issuer_key, issuer_entry)
        .add_entry(pool_key, pool_entry)
        .add_entry(asset_b_key, asset_b_entry)
        .add_entry(pool_share_key, pool_share_entry)
        .build_with_default_header();
    let snapshot = SnapshotHandle::new(snapshot);

    let operation = Operation {
        source_account: None,
        body: OperationBody::LiquidityPoolDeposit(LiquidityPoolDepositOp {
            liquidity_pool_id: pool_id.clone(),
            max_amount_a: 10_000_000,
            max_amount_b: 20_000_000,
            min_price: Price { n: 1, d: 2 },
            max_price: Price { n: 1, d: 2 },
        }),
    };

    let tx = Transaction {
        source_account: MuxedAccount::Ed25519(Uint256(*source_secret.public_key().as_bytes())),
        fee: 100,
        seq_num: SequenceNumber(2),
        cond: Preconditions::None,
        memo: Memo::None,
        operations: vec![operation].try_into().unwrap(),
        ext: TransactionExt::V0,
    };

    let mut envelope = TransactionEnvelope::Tx(TransactionV1Envelope {
        tx,
        signatures: VecM::default(),
    });

    let network_id = NetworkId::testnet();
    let decorated = sign_envelope(&envelope, &source_secret, &network_id);
    if let TransactionEnvelope::Tx(ref mut env) = envelope {
        env.signatures = vec![decorated].try_into().unwrap();
    }

    let classic_events = ClassicEventConfig {
        emit_classic_events: true,
        backfill_stellar_asset_events: false,
    };
    let context = henyey_tx::LedgerContext::new(1, 1_000, 100, 5_000_000, 25, network_id);
    let mut executor = TransactionExecutor::new(
        &context,
        0,
        SorobanConfig::default(),
        classic_events,
    );
    let result = executor
        .execute_transaction(&snapshot, &envelope, 100, None)
        .expect("execute");

    assert!(result.success);
    let tx_meta = result.tx_meta.expect("tx meta");
    let TransactionMeta::V4(meta) = tx_meta else {
        panic!("unexpected tx meta");
    };

    let op_events: &[stellar_xdr::curr::OperationMetaV2] = meta.operations.as_ref();
    assert_eq!(op_events.len(), 1);
    let op_event_list: &[stellar_xdr::curr::ContractEvent] = op_events[0].events.as_ref();
    assert_eq!(op_event_list.len(), 2);

    let pool_address = ScAddress::LiquidityPool(pool_id.clone());

    let first_event = &op_event_list[0];
    let ContractEventBody::V0(first_body) = &first_event.body;
    let first_topics: &[ScVal] = first_body.topics.as_ref();
    assert_eq!(first_topics.len(), 4);
    assert_eq!(first_topics[0], scval_symbol("transfer"));
    assert_eq!(
        first_topics[1],
        ScVal::Address(ScAddress::Account(source_id.clone()))
    );
    assert_eq!(first_topics[2], ScVal::Address(pool_address.clone()));
    assert_eq!(first_topics[3], asset_string_scval(&asset_a));
    assert_eq!(first_body.data, ScVal::I128(i128_parts(10_000_000)));

    let second_event = &op_event_list[1];
    let ContractEventBody::V0(second_body) = &second_event.body;
    let second_topics: &[ScVal] = second_body.topics.as_ref();
    assert_eq!(second_topics.len(), 4);
    assert_eq!(second_topics[0], scval_symbol("transfer"));
    assert_eq!(
        second_topics[1],
        ScVal::Address(ScAddress::Account(source_id.clone()))
    );
    assert_eq!(second_topics[2], ScVal::Address(pool_address));
    assert_eq!(second_topics[3], asset_string_scval(&asset_b));
    assert_eq!(second_body.data, ScVal::I128(i128_parts(20_000_000)));
}

#[test]
fn test_classic_events_emitted_for_liquidity_pool_withdraw() {
    let source_secret = SecretKey::from_seed(&[98u8; 32]);
    let source_id: AccountId = (&source_secret.public_key()).into();
    let issuer_secret = SecretKey::from_seed(&[19u8; 32]);
    let issuer_id: AccountId = (&issuer_secret.public_key()).into();

    let asset_a = Asset::Native;
    let asset_b = Asset::CreditAlphanum4(AlphaNum4 {
        asset_code: AssetCode4([b'E', b'U', b'R', 0]),
        issuer: issuer_id.clone(),
    });

    let pool_id = PoolId(Hash([20u8; 32]));
    let (source_key, source_entry) = create_account_entry(source_id.clone(), 1, 500_000_000);
    let (issuer_key, issuer_entry) = create_account_entry(issuer_id.clone(), 1, 100_000_000);
    let (pool_key, pool_entry) = create_liquidity_pool_entry(
        pool_id.clone(),
        asset_a.clone(),
        asset_b.clone(),
        50_000_000,
        100_000_000,
        100_000_000,
        1,
    );
    let (asset_b_key, asset_b_entry) = create_trustline_entry(
        source_id.clone(),
        TrustLineAsset::CreditAlphanum4(match &asset_b {
            Asset::CreditAlphanum4(a) => a.clone(),
            _ => unreachable!(),
        }),
        0,
        200_000_000,
        TrustLineFlags::AuthorizedFlag as u32,
    );
    let (pool_share_key, pool_share_entry) = create_trustline_entry(
        source_id.clone(),
        TrustLineAsset::PoolShare(pool_id.clone()),
        20_000_000,
        100_000_000,
        0,
    );

    let snapshot = SnapshotBuilder::new(1)
        .add_entry(source_key, source_entry)
        .add_entry(issuer_key, issuer_entry)
        .add_entry(pool_key, pool_entry)
        .add_entry(asset_b_key, asset_b_entry)
        .add_entry(pool_share_key, pool_share_entry)
        .build_with_default_header();
    let snapshot = SnapshotHandle::new(snapshot);

    let operation = Operation {
        source_account: None,
        body: OperationBody::LiquidityPoolWithdraw(LiquidityPoolWithdrawOp {
            liquidity_pool_id: pool_id.clone(),
            amount: 10_000_000,
            min_amount_a: 0,
            min_amount_b: 0,
        }),
    };

    let tx = Transaction {
        source_account: MuxedAccount::Ed25519(Uint256(*source_secret.public_key().as_bytes())),
        fee: 100,
        seq_num: SequenceNumber(2),
        cond: Preconditions::None,
        memo: Memo::None,
        operations: vec![operation].try_into().unwrap(),
        ext: TransactionExt::V0,
    };

    let mut envelope = TransactionEnvelope::Tx(TransactionV1Envelope {
        tx,
        signatures: VecM::default(),
    });

    let network_id = NetworkId::testnet();
    let decorated = sign_envelope(&envelope, &source_secret, &network_id);
    if let TransactionEnvelope::Tx(ref mut env) = envelope {
        env.signatures = vec![decorated].try_into().unwrap();
    }

    let classic_events = ClassicEventConfig {
        emit_classic_events: true,
        backfill_stellar_asset_events: false,
    };
    let context = henyey_tx::LedgerContext::new(1, 1_000, 100, 5_000_000, 25, network_id);
    let mut executor = TransactionExecutor::new(
        &context,
        0,
        SorobanConfig::default(),
        classic_events,
    );
    let result = executor
        .execute_transaction(&snapshot, &envelope, 100, None)
        .expect("execute");

    assert!(result.success);
    let tx_meta = result.tx_meta.expect("tx meta");
    let TransactionMeta::V4(meta) = tx_meta else {
        panic!("unexpected tx meta");
    };

    let op_events: &[stellar_xdr::curr::OperationMetaV2] = meta.operations.as_ref();
    assert_eq!(op_events.len(), 1);
    let op_event_list: &[stellar_xdr::curr::ContractEvent] = op_events[0].events.as_ref();
    assert_eq!(op_event_list.len(), 2);

    let pool_address = ScAddress::LiquidityPool(pool_id.clone());

    let first_event = &op_event_list[0];
    let ContractEventBody::V0(first_body) = &first_event.body;
    let first_topics: &[ScVal] = first_body.topics.as_ref();
    assert_eq!(first_topics.len(), 4);
    assert_eq!(first_topics[0], scval_symbol("transfer"));
    assert_eq!(first_topics[1], ScVal::Address(pool_address.clone()));
    assert_eq!(
        first_topics[2],
        ScVal::Address(ScAddress::Account(source_id.clone()))
    );
    assert_eq!(first_topics[3], asset_string_scval(&asset_a));
    assert_eq!(first_body.data, ScVal::I128(i128_parts(5_000_000)));

    let second_event = &op_event_list[1];
    let ContractEventBody::V0(second_body) = &second_event.body;
    let second_topics: &[ScVal] = second_body.topics.as_ref();
    assert_eq!(second_topics.len(), 4);
    assert_eq!(second_topics[0], scval_symbol("transfer"));
    assert_eq!(second_topics[1], ScVal::Address(pool_address));
    assert_eq!(
        second_topics[2],
        ScVal::Address(ScAddress::Account(source_id.clone()))
    );
    assert_eq!(second_topics[3], asset_string_scval(&asset_b));
    assert_eq!(second_body.data, ScVal::I128(i128_parts(10_000_000)));
}

#[test]
fn test_classic_events_emitted_for_claim_atoms_order_book() {
    let source_secret = SecretKey::from_seed(&[101u8; 32]);
    let source_id: AccountId = (&source_secret.public_key()).into();
    let seller_secret = SecretKey::from_seed(&[102u8; 32]);
    let seller_id: AccountId = (&seller_secret.public_key()).into();
    let issuer_secret = SecretKey::from_seed(&[103u8; 32]);
    let issuer_id: AccountId = (&issuer_secret.public_key()).into();

    let asset_usd = Asset::CreditAlphanum4(AlphaNum4 {
        asset_code: AssetCode4([b'U', b'S', b'D', 0]),
        issuer: issuer_id.clone(),
    });

    let claim = ClaimAtom::OrderBook(ClaimOfferAtom {
        seller_id: seller_id.clone(),
        offer_id: 7,
        asset_sold: Asset::Native,
        amount_sold: 5_000_000,
        asset_bought: asset_usd.clone(),
        amount_bought: 5_000_000,
    });

    let classic_events = ClassicEventConfig {
        emit_classic_events: true,
        backfill_stellar_asset_events: false,
    };
    let mut op_event_manager = OpEventManager::new(
        true,
        false,
        25,
        NetworkId::testnet(),
        Memo::None,
        classic_events,
    );
    let source_muxed = MuxedAccount::Ed25519(Uint256(*source_secret.public_key().as_bytes()));
    op_event_manager.events_for_claim_atoms(&source_muxed, &[claim.clone()]);

    let events = op_event_manager.finalize();
    assert_eq!(events.len(), 2);
    let index = assert_claim_atom_events(&events, &claim, &source_id, 0);
    assert_eq!(index, 2);
}

#[test]
fn test_classic_events_emitted_for_manage_sell_offer() {
    let source_secret = SecretKey::from_seed(&[101u8; 32]);
    let source_id: AccountId = (&source_secret.public_key()).into();
    let offer_secret = SecretKey::from_seed(&[102u8; 32]);
    let offer_id_account: AccountId = (&offer_secret.public_key()).into();
    let issuer_secret = SecretKey::from_seed(&[103u8; 32]);
    let issuer_id: AccountId = (&issuer_secret.public_key()).into();

    let asset_usd = Asset::CreditAlphanum4(AlphaNum4 {
        asset_code: AssetCode4([b'U', b'S', b'D', 0]),
        issuer: issuer_id.clone(),
    });

    let (source_key, source_entry) = create_account_entry(source_id.clone(), 1, 500_000_000);
    let (offer_key, mut offer_entry) =
        create_account_entry(offer_id_account.clone(), 1, 500_000_000);
    let (issuer_key, issuer_entry) = create_account_entry(issuer_id.clone(), 1, 100_000_000);
    let (source_tl_key, source_tl_entry) = create_trustline_entry(
        source_id.clone(),
        TrustLineAsset::CreditAlphanum4(match &asset_usd {
            Asset::CreditAlphanum4(a) => a.clone(),
            _ => unreachable!(),
        }),
        20_000_000,
        100_000_000,
        TrustLineFlags::AuthorizedFlag as u32,
    );
    let (offer_tl_key, mut offer_tl_entry) = create_trustline_entry(
        offer_id_account.clone(),
        TrustLineAsset::CreditAlphanum4(match &asset_usd {
            Asset::CreditAlphanum4(a) => a.clone(),
            _ => unreachable!(),
        }),
        0,
        100_000_000,
        TrustLineFlags::AuthorizedFlag as u32,
    );
    set_account_liabilities(&mut offer_entry, 50_000_000, 0);
    set_trustline_liabilities(&mut offer_tl_entry, 0, 50_000_000);
    let (offer_entry_key, offer_entry_value) = create_offer_entry(
        offer_id_account.clone(),
        1,
        Asset::Native,
        asset_usd.clone(),
        50_000_000,
        Price { n: 1, d: 1 },
    );

    let snapshot = SnapshotBuilder::new(1)
        .add_entry(source_key, source_entry)
        .add_entry(offer_key.clone(), offer_entry)
        .add_entry(issuer_key, issuer_entry)
        .add_entry(source_tl_key, source_tl_entry)
        .add_entry(offer_tl_key.clone(), offer_tl_entry)
        .add_entry(offer_entry_key.clone(), offer_entry_value)
        .build_with_default_header();
    let snapshot = SnapshotHandle::new(snapshot);

    let operation = Operation {
        source_account: None,
        body: OperationBody::ManageSellOffer(ManageSellOfferOp {
            selling: asset_usd.clone(),
            buying: Asset::Native,
            amount: 10_000_000,
            price: Price { n: 1, d: 1 },
            offer_id: 0,
        }),
    };

    let tx = Transaction {
        source_account: MuxedAccount::Ed25519(Uint256(*source_secret.public_key().as_bytes())),
        fee: 100,
        seq_num: SequenceNumber(2),
        cond: Preconditions::None,
        memo: Memo::None,
        operations: vec![operation].try_into().unwrap(),
        ext: TransactionExt::V0,
    };

    let mut envelope = TransactionEnvelope::Tx(TransactionV1Envelope {
        tx,
        signatures: VecM::default(),
    });

    let network_id = NetworkId::testnet();
    let decorated = sign_envelope(&envelope, &source_secret, &network_id);
    if let TransactionEnvelope::Tx(ref mut env) = envelope {
        env.signatures = vec![decorated].try_into().unwrap();
    }

    let classic_events = ClassicEventConfig {
        emit_classic_events: true,
        backfill_stellar_asset_events: false,
    };
    let context = henyey_tx::LedgerContext::new(1, 1_000, 100, 5_000_000, 25, network_id);
    let mut executor = TransactionExecutor::new(
        &context,
        0,
        SorobanConfig::default(),
        classic_events,
    );
    executor
        .load_orderbook_offers(&snapshot)
        .expect("load orderbook");
    let result = executor
        .execute_transaction(&snapshot, &envelope, 100, None)
        .expect("execute");

    assert!(
        result.success,
        "unexpected result: {:?}",
        result.operation_results
    );
    let op_result = result.operation_results.get(0).expect("operation result");
    let claim_atoms: &[ClaimAtom] = match op_result {
        OperationResult::OpInner(OperationResultTr::ManageSellOffer(
            ManageSellOfferResult::Success(success),
        )) => success.offers_claimed.as_ref(),
        other => panic!("unexpected result: {:?}", other),
    };
    assert!(!claim_atoms.is_empty());

    let tx_meta = result.tx_meta.expect("tx meta");
    let TransactionMeta::V4(meta) = tx_meta else {
        panic!("unexpected tx meta");
    };

    let op_events: &[stellar_xdr::curr::OperationMetaV2] = meta.operations.as_ref();
    assert_eq!(op_events.len(), 1);
    let op_event_list: &[stellar_xdr::curr::ContractEvent] = op_events[0].events.as_ref();
    assert_eq!(op_event_list.len(), claim_atoms.len() * 2);

    let mut index = 0;
    for claim in claim_atoms.iter() {
        index = assert_claim_atom_events(op_event_list, claim, &source_id, index);
    }
}

#[test]
fn test_classic_events_emitted_for_path_payment_strict_send() {
    let source_secret = SecretKey::from_seed(&[104u8; 32]);
    let source_id: AccountId = (&source_secret.public_key()).into();
    let dest_id = AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([105u8; 32])));
    let offer_secret = SecretKey::from_seed(&[106u8; 32]);
    let offer_id_account: AccountId = (&offer_secret.public_key()).into();
    let issuer_secret = SecretKey::from_seed(&[107u8; 32]);
    let issuer_id: AccountId = (&issuer_secret.public_key()).into();

    let asset_usd = Asset::CreditAlphanum4(AlphaNum4 {
        asset_code: AssetCode4([b'U', b'S', b'D', 0]),
        issuer: issuer_id.clone(),
    });

    let (source_key, source_entry) = create_account_entry(source_id.clone(), 1, 500_000_000);
    let (dest_key, dest_entry) = create_account_entry(dest_id.clone(), 1, 200_000_000);
    let (offer_key, mut offer_entry) =
        create_account_entry(offer_id_account.clone(), 1, 500_000_000);
    let (issuer_key, issuer_entry) = create_account_entry(issuer_id.clone(), 1, 100_000_000);
    let (source_tl_key, source_tl_entry) = create_trustline_entry(
        source_id.clone(),
        TrustLineAsset::CreditAlphanum4(match &asset_usd {
            Asset::CreditAlphanum4(a) => a.clone(),
            _ => unreachable!(),
        }),
        20_000_000,
        100_000_000,
        TrustLineFlags::AuthorizedFlag as u32,
    );
    let (offer_tl_key, mut offer_tl_entry) = create_trustline_entry(
        offer_id_account.clone(),
        TrustLineAsset::CreditAlphanum4(match &asset_usd {
            Asset::CreditAlphanum4(a) => a.clone(),
            _ => unreachable!(),
        }),
        0,
        100_000_000,
        TrustLineFlags::AuthorizedFlag as u32,
    );
    set_account_liabilities(&mut offer_entry, 50_000_000, 0);
    set_trustline_liabilities(&mut offer_tl_entry, 0, 50_000_000);
    let (offer_entry_key, offer_entry_value) = create_offer_entry(
        offer_id_account.clone(),
        1,
        Asset::Native,
        asset_usd.clone(),
        50_000_000,
        Price { n: 1, d: 1 },
    );

    let snapshot = SnapshotBuilder::new(1)
        .add_entry(source_key, source_entry)
        .add_entry(dest_key, dest_entry)
        .add_entry(offer_key.clone(), offer_entry)
        .add_entry(issuer_key, issuer_entry)
        .add_entry(source_tl_key, source_tl_entry)
        .add_entry(offer_tl_key.clone(), offer_tl_entry)
        .add_entry(offer_entry_key.clone(), offer_entry_value)
        .build_with_default_header();
    let snapshot = SnapshotHandle::new(snapshot);

    let op_data = PathPaymentStrictSendOp {
        send_asset: asset_usd.clone(),
        send_amount: 10_000_000,
        destination: dest_id.clone().into(),
        dest_asset: Asset::Native,
        dest_min: 1,
        path: VecM::default(),
    };
    let operation = Operation {
        source_account: None,
        body: OperationBody::PathPaymentStrictSend(op_data.clone()),
    };

    let tx = Transaction {
        source_account: MuxedAccount::Ed25519(Uint256(*source_secret.public_key().as_bytes())),
        fee: 100,
        seq_num: SequenceNumber(2),
        cond: Preconditions::None,
        memo: Memo::None,
        operations: vec![operation].try_into().unwrap(),
        ext: TransactionExt::V0,
    };

    let mut envelope = TransactionEnvelope::Tx(TransactionV1Envelope {
        tx,
        signatures: VecM::default(),
    });

    let network_id = NetworkId::testnet();
    let decorated = sign_envelope(&envelope, &source_secret, &network_id);
    if let TransactionEnvelope::Tx(ref mut env) = envelope {
        env.signatures = vec![decorated].try_into().unwrap();
    }

    let classic_events = ClassicEventConfig {
        emit_classic_events: true,
        backfill_stellar_asset_events: false,
    };
    let context = henyey_tx::LedgerContext::new(1, 1_000, 100, 5_000_000, 25, network_id);
    let mut executor = TransactionExecutor::new(
        &context,
        0,
        SorobanConfig::default(),
        classic_events,
    );
    executor
        .load_orderbook_offers(&snapshot)
        .expect("load orderbook");
    let result = executor
        .execute_transaction(&snapshot, &envelope, 100, None)
        .expect("execute");

    assert!(
        result.success,
        "unexpected result: {:?}",
        result.operation_results
    );
    let (claim_atoms, last): (&[ClaimAtom], &stellar_xdr::curr::SimplePaymentResult) =
        match result.operation_results.get(0).expect("op result") {
            OperationResult::OpInner(OperationResultTr::PathPaymentStrictSend(
                PathPaymentStrictSendResult::Success(PathPaymentStrictSendResultSuccess {
                    offers,
                    last,
                    ..
                }),
            )) => (offers.as_ref(), last),
            other => panic!("unexpected result: {:?}", other),
        };
    assert!(!claim_atoms.is_empty());

    let tx_meta = result.tx_meta.expect("tx meta");
    let TransactionMeta::V4(meta) = tx_meta else {
        panic!("unexpected tx meta");
    };

    let op_events: &[stellar_xdr::curr::OperationMetaV2] = meta.operations.as_ref();
    assert_eq!(op_events.len(), 1);
    let op_event_list: &[stellar_xdr::curr::ContractEvent] = op_events[0].events.as_ref();
    assert_eq!(op_event_list.len(), claim_atoms.len() * 2 + 1);

    let mut index = 0;
    for claim in claim_atoms.iter() {
        index = assert_claim_atom_events(op_event_list, claim, &source_id, index);
    }

    let last_event = &op_event_list[op_event_list.len() - 1];
    let dest_address = ScAddress::Account(dest_id);
    assert_transfer_event(
        last_event,
        &ScAddress::Account(source_id),
        &dest_address,
        &op_data.dest_asset,
        last.amount,
    );
}

