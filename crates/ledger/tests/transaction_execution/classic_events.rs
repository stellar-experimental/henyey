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
        body: OperationBody::Payment(stellar_xdr::PaymentOp {
            destination: MuxedAccount::Ed25519(Uint256([4u8; 32])),
            asset: stellar_xdr::Asset::Native,
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
    let mut executor =
        TransactionExecutor::new(&context, 0, SorobanConfig::default(), classic_events);
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

    let tx_events: &[stellar_xdr::TransactionEvent] = meta.events.as_ref();
    assert_eq!(tx_events.len(), 0);

    let contract_id = native_asset_contract_id(&network_id);
    let op_events: &[stellar_xdr::OperationMetaV2] = meta.operations.as_ref();
    assert_eq!(op_events.len(), 1);
    let op_event_list: &[stellar_xdr::ContractEvent] = op_events[0].events.as_ref();
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
        body: OperationBody::Payment(stellar_xdr::PaymentOp {
            destination: muxed_dest,
            asset: stellar_xdr::Asset::Native,
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
    let mut executor =
        TransactionExecutor::new(&context, 0, SorobanConfig::default(), classic_events);
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

    let op_events: &[stellar_xdr::OperationMetaV2] = meta.operations.as_ref();
    assert_eq!(op_events.len(), 1);
    let op_event_list: &[stellar_xdr::ContractEvent] = op_events[0].events.as_ref();
    assert_eq!(op_event_list.len(), 1);
    let op_event = &op_event_list[0];
    let ContractEventBody::V0(op_body) = &op_event.body;
    let ScVal::Map(Some(map)) = &op_body.data else {
        panic!("expected map data for muxed destination");
    };
    let entries: &[stellar_xdr::ScMapEntry] = map.0.as_ref();
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
        body: OperationBody::Payment(stellar_xdr::PaymentOp {
            destination: MuxedAccount::Ed25519(Uint256([8u8; 32])),
            asset: stellar_xdr::Asset::Native,
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
    let mut executor =
        TransactionExecutor::new(&context, 0, SorobanConfig::default(), classic_events);
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

    let op_events: &[stellar_xdr::OperationMetaV2] = meta.operations.as_ref();
    assert_eq!(op_events.len(), 1);
    let op_event_list: &[stellar_xdr::ContractEvent] = op_events[0].events.as_ref();
    assert_eq!(op_event_list.len(), 1);
    let op_event = &op_event_list[0];
    let ContractEventBody::V0(op_body) = &op_event.body;
    let ScVal::Map(Some(map)) = &op_body.data else {
        panic!("expected map data for memo");
    };
    let entries: &[stellar_xdr::ScMapEntry] = map.0.as_ref();
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
        body: OperationBody::Payment(stellar_xdr::PaymentOp {
            destination: muxed_dest,
            asset: stellar_xdr::Asset::Native,
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
    let mut executor =
        TransactionExecutor::new(&context, 0, SorobanConfig::default(), classic_events);
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

    let op_events: &[stellar_xdr::OperationMetaV2] = meta.operations.as_ref();
    assert_eq!(op_events.len(), 1);
    let op_event_list: &[stellar_xdr::ContractEvent] = op_events[0].events.as_ref();
    assert_eq!(op_event_list.len(), 1);
    let op_event = &op_event_list[0];
    let ContractEventBody::V0(op_body) = &op_event.body;
    let ScVal::Map(Some(map)) = &op_body.data else {
        panic!("expected map data for muxed destination");
    };
    let entries: &[stellar_xdr::ScMapEntry] = map.0.as_ref();
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
    let mut executor =
        TransactionExecutor::new(&context, 0, SorobanConfig::default(), classic_events);
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

    let op_events: &[stellar_xdr::OperationMetaV2] = meta.operations.as_ref();
    assert_eq!(op_events.len(), 1);
    let op_event_list: &[stellar_xdr::ContractEvent] = op_events[0].events.as_ref();
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
    let mut executor =
        TransactionExecutor::new(&context, 0, SorobanConfig::default(), classic_events);
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

    let op_events: &[stellar_xdr::OperationMetaV2] = meta.operations.as_ref();
    assert_eq!(op_events.len(), 1);
    let op_event_list: &[stellar_xdr::ContractEvent] = op_events[0].events.as_ref();
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
    let mut executor =
        TransactionExecutor::new(&context, 0, SorobanConfig::default(), classic_events);
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

    let op_events: &[stellar_xdr::OperationMetaV2] = meta.operations.as_ref();
    assert_eq!(op_events.len(), 1);
    let op_event_list: &[stellar_xdr::ContractEvent] = op_events[0].events.as_ref();
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
    let mut executor =
        TransactionExecutor::new(&context, 0, SorobanConfig::default(), classic_events);
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

    let op_events: &[stellar_xdr::OperationMetaV2] = meta.operations.as_ref();
    assert_eq!(op_events.len(), 1);
    let op_event_list: &[stellar_xdr::ContractEvent] = op_events[0].events.as_ref();
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
    let mut executor =
        TransactionExecutor::new(&context, 0, SorobanConfig::default(), classic_events);
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

    let op_events: &[stellar_xdr::OperationMetaV2] = meta.operations.as_ref();
    assert_eq!(op_events.len(), 1);
    let op_event_list: &[stellar_xdr::ContractEvent] = op_events[0].events.as_ref();
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
    let mut executor =
        TransactionExecutor::new(&context, 0, SorobanConfig::default(), classic_events);
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

    let op_events: &[stellar_xdr::OperationMetaV2] = meta.operations.as_ref();
    assert_eq!(op_events.len(), 1);
    let op_event_list: &[stellar_xdr::ContractEvent] = op_events[0].events.as_ref();
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
    let mut executor =
        TransactionExecutor::new(&context, 0, SorobanConfig::default(), classic_events);
    let result = executor
        .execute_transaction(&snapshot, &envelope, 100, None)
        .expect("execute");

    assert!(result.success);
    let tx_meta = result.tx_meta.expect("tx meta");
    let TransactionMeta::V4(meta) = tx_meta else {
        panic!("unexpected tx meta");
    };

    let op_events: &[stellar_xdr::OperationMetaV2] = meta.operations.as_ref();
    assert_eq!(op_events.len(), 1);
    let op_event_list: &[stellar_xdr::ContractEvent] = op_events[0].events.as_ref();
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
    let mut executor =
        TransactionExecutor::new(&context, 0, SorobanConfig::default(), classic_events);
    let result = executor
        .execute_transaction(&snapshot, &envelope, 100, None)
        .expect("execute");

    assert!(result.success);
    let tx_meta = result.tx_meta.expect("tx meta");
    let TransactionMeta::V4(meta) = tx_meta else {
        panic!("unexpected tx meta");
    };

    let op_events: &[stellar_xdr::OperationMetaV2] = meta.operations.as_ref();
    assert_eq!(op_events.len(), 1);
    let op_event_list: &[stellar_xdr::ContractEvent] = op_events[0].events.as_ref();
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
    let mut executor =
        TransactionExecutor::new(&context, 0, SorobanConfig::default(), classic_events);
    let result = executor
        .execute_transaction(&snapshot, &envelope, 100, None)
        .expect("execute");

    assert!(result.success);
    let tx_meta = result.tx_meta.expect("tx meta");
    let TransactionMeta::V4(meta) = tx_meta else {
        panic!("unexpected tx meta");
    };

    let op_events: &[stellar_xdr::OperationMetaV2] = meta.operations.as_ref();
    assert_eq!(op_events.len(), 1);
    let op_event_list: &[stellar_xdr::ContractEvent] = op_events[0].events.as_ref();
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
    let mut executor =
        TransactionExecutor::new(&context, 0, SorobanConfig::default(), classic_events);
    let result = executor
        .execute_transaction(&snapshot, &envelope, 100, None)
        .expect("execute");

    assert!(result.success);
    let tx_meta = result.tx_meta.expect("tx meta");
    let TransactionMeta::V4(meta) = tx_meta else {
        panic!("unexpected tx meta");
    };

    let op_events: &[stellar_xdr::OperationMetaV2] = meta.operations.as_ref();
    assert_eq!(op_events.len(), 1);
    let op_event_list: &[stellar_xdr::ContractEvent] = op_events[0].events.as_ref();
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
    op_event_manager.events_for_claim_atoms(&source_muxed, std::slice::from_ref(&claim));

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
    let mut executor =
        TransactionExecutor::new(&context, 0, SorobanConfig::default(), classic_events);
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

    let op_events: &[stellar_xdr::OperationMetaV2] = meta.operations.as_ref();
    assert_eq!(op_events.len(), 1);
    let op_event_list: &[stellar_xdr::ContractEvent] = op_events[0].events.as_ref();
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
    let mut executor =
        TransactionExecutor::new(&context, 0, SorobanConfig::default(), classic_events);
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
    let (claim_atoms, last): (&[ClaimAtom], &stellar_xdr::SimplePaymentResult) =
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

    let op_events: &[stellar_xdr::OperationMetaV2] = meta.operations.as_ref();
    assert_eq!(op_events.len(), 1);
    let op_event_list: &[stellar_xdr::ContractEvent] = op_events[0].events.as_ref();
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

// ---------------------------------------------------------------------------
// #3117 — exhaustive CAP-0067 classic-op SAC event coverage + two gap fixes.
// ---------------------------------------------------------------------------

/// Build a SnapshotHandle for a pool-share-revocation scenario where the
/// trustor holds a pool-share trustline (loaded via the on-disk lookup path,
/// like the VE-02 harness) for a pool of `(asset_a, asset_b)`. The issuer of
/// `asset_a` (the `deauth_asset`) holds AUTH_REQUIRED | AUTH_REVOCABLE so it
/// can deauthorize the trustor's `asset_a` trustline, triggering redemption.
#[allow(clippy::too_many_arguments)]
fn build_pool_share_revoke_snapshot(
    issuer_id: &AccountId,
    trustor_id: &AccountId,
    asset_a: &Asset,
    asset_b: &Asset,
    pool_id: &PoolId,
    pool_share_balance: i64,
    reserve_a: i64,
    reserve_b: i64,
    total_shares: i64,
    include_asset_b_trustline: bool,
    extra_accounts: &[(AccountId, i64)],
) -> SnapshotHandle {
    use henyey_ledger::{EntryLookupFn, PoolShareTrustlinesByAccountFn};
    use std::collections::HashMap;
    use std::sync::Arc;
    use stellar_xdr::{
        Liabilities, Limits, TrustLineEntryExt, TrustLineEntryExtensionV2,
        TrustLineEntryExtensionV2Ext, TrustLineEntryV1, TrustLineEntryV1Ext, WriteXdr,
    };

    let pool_share_tl_asset = TrustLineAsset::PoolShare(pool_id.clone());
    let pool_share_tl_key = LedgerKey::Trustline(LedgerKeyTrustLine {
        account_id: trustor_id.clone(),
        asset: pool_share_tl_asset.clone(),
    });
    let pool_share_tl_entry = LedgerEntry {
        last_modified_ledger_seq: 1,
        data: LedgerEntryData::Trustline(TrustLineEntry {
            account_id: trustor_id.clone(),
            asset: pool_share_tl_asset,
            balance: pool_share_balance,
            limit: i64::MAX,
            flags: 0,
            ext: TrustLineEntryExt::V0,
        }),
        ext: LedgerEntryExt::V0,
    };

    let make_authorized_v2_tl = |asset: &Asset| {
        let tl_asset = match asset {
            Asset::CreditAlphanum4(a) => TrustLineAsset::CreditAlphanum4(a.clone()),
            Asset::CreditAlphanum12(a) => TrustLineAsset::CreditAlphanum12(a.clone()),
            Asset::Native => unreachable!("native asset has no trustline"),
        };
        let key = LedgerKey::Trustline(LedgerKeyTrustLine {
            account_id: trustor_id.clone(),
            asset: tl_asset.clone(),
        });
        let entry = LedgerEntry {
            last_modified_ledger_seq: 1,
            data: LedgerEntryData::Trustline(TrustLineEntry {
                account_id: trustor_id.clone(),
                asset: tl_asset,
                balance: 5000,
                limit: 100_000,
                flags: TrustLineFlags::AuthorizedFlag as u32,
                ext: TrustLineEntryExt::V1(TrustLineEntryV1 {
                    liabilities: Liabilities {
                        buying: 0,
                        selling: 0,
                    },
                    ext: TrustLineEntryV1Ext::V2(TrustLineEntryExtensionV2 {
                        liquidity_pool_use_count: 1,
                        ext: TrustLineEntryExtensionV2Ext::V0,
                    }),
                }),
            }),
            ext: LedgerEntryExt::V0,
        };
        (key, entry)
    };

    let (asset_a_tl_key, asset_a_tl_entry) = make_authorized_v2_tl(asset_a);

    let (issuer_key, issuer_entry) =
        create_account_entry_with_flags(issuer_id.clone(), 1, 100_000_000, 0x1 | 0x2);

    let trustor_key = LedgerKey::Account(stellar_xdr::LedgerKeyAccount {
        account_id: trustor_id.clone(),
    });
    let trustor_entry = LedgerEntry {
        last_modified_ledger_seq: 1,
        data: LedgerEntryData::Account(AccountEntry {
            account_id: trustor_id.clone(),
            balance: 100_000_000,
            seq_num: SequenceNumber(0),
            num_sub_entries: 4,
            inflation_dest: None,
            flags: 0,
            home_domain: String32::default(),
            thresholds: Thresholds([1, 0, 0, 0]),
            signers: VecM::default(),
            ext: AccountEntryExt::V0,
        }),
        ext: LedgerEntryExt::V0,
    };

    let (pool_key, pool_entry) = create_liquidity_pool_entry(
        pool_id.clone(),
        asset_a.clone(),
        asset_b.clone(),
        reserve_a,
        reserve_b,
        total_shares,
        1,
    );

    let mut builder = SnapshotBuilder::new(10)
        .add_entry(issuer_key, issuer_entry)
        .add_entry(trustor_key, trustor_entry)
        .add_entry(pool_key, pool_entry)
        .add_entry(asset_a_tl_key, asset_a_tl_entry);

    if include_asset_b_trustline {
        let (asset_b_tl_key, asset_b_tl_entry) = make_authorized_v2_tl(asset_b);
        builder = builder.add_entry(asset_b_tl_key, asset_b_tl_entry);
    }

    for (acc_id, balance) in extra_accounts {
        let (k, e) = create_account_entry(acc_id.clone(), 0, *balance);
        builder = builder.add_entry(k, e);
    }

    let snapshot = builder.build_with_default_header();

    let pool_share_tl_key_bytes = pool_share_tl_key
        .to_xdr(Limits::none())
        .expect("encode pool share TL key");
    let extra_entries: Arc<HashMap<Vec<u8>, LedgerEntry>> = Arc::new({
        let mut m = HashMap::new();
        m.insert(pool_share_tl_key_bytes, pool_share_tl_entry);
        m
    });
    let lookup_fn: EntryLookupFn = Arc::new(move |key| {
        let key_bytes = key
            .to_xdr(Limits::none())
            .map_err(|e| henyey_ledger::LedgerError::Serialization(e.to_string()))?;
        Ok(extra_entries.get(&key_bytes).cloned())
    });

    let captured_pool_id = pool_id.clone();
    let captured_trustor_id = trustor_id.clone();
    let pool_share_index_fn: PoolShareTrustlinesByAccountFn = Arc::new(move |account_id| {
        if account_id == &captured_trustor_id {
            Ok(vec![captured_pool_id.clone()])
        } else {
            Ok(vec![])
        }
    });

    let mut handle = SnapshotHandle::new(snapshot);
    handle.set_lookup(lookup_fn);
    handle.set_pool_share_tls_by_account(pool_share_index_fn);
    handle
}

/// Sign + execute a single-op tx and return the V4 meta.
fn execute_single_op_tx_v4(
    handle: &SnapshotHandle,
    issuer_secret: &SecretKey,
    op: Operation,
    network_id: &NetworkId,
) -> stellar_xdr::TransactionMetaV4 {
    let tx = Transaction {
        source_account: MuxedAccount::Ed25519(Uint256(*issuer_secret.public_key().as_bytes())),
        fee: 1000,
        seq_num: SequenceNumber(2),
        cond: Preconditions::None,
        memo: Memo::None,
        operations: vec![op].try_into().unwrap(),
        ext: TransactionExt::V0,
    };
    let mut envelope = TransactionEnvelope::Tx(TransactionV1Envelope {
        tx,
        signatures: VecM::default(),
    });
    let decorated = sign_envelope(&envelope, issuer_secret, network_id);
    if let TransactionEnvelope::Tx(ref mut env) = envelope {
        env.signatures = vec![decorated].try_into().unwrap();
    }

    let classic_events = ClassicEventConfig {
        emit_classic_events: true,
        backfill_stellar_asset_events: false,
    };
    let context = henyey_tx::LedgerContext::new(10, 1_000, 100, 5_000_000, 25, *network_id);
    let mut executor =
        TransactionExecutor::new(&context, 0, SorobanConfig::default(), classic_events);
    let result = executor
        .execute_transaction(handle, &envelope, 100, None)
        .expect("execute");
    assert!(
        result.success,
        "tx should succeed, got failure: {:?}",
        result.failure
    );
    match result.tx_meta.expect("tx meta") {
        TransactionMeta::V4(meta) => meta,
        other => panic!("unexpected tx meta: {other:?}"),
    }
}

/// #3117 Gap A regression: deauthorizing a non-issuer holder's pool-share-
/// backing trustline must emit a `transfer` (pool -> claimable balance) event
/// for EACH pool asset, BEFORE the `set_authorized` event, in asset_a-then-
/// asset_b order. Fails on origin/main (no revoke events emitted).
#[test]
fn test_classic_events_pool_share_revoke_transfer() {
    let network_id = NetworkId::testnet();
    let issuer_secret = SecretKey::from_seed(&[120u8; 32]);
    let issuer_id: AccountId = (&issuer_secret.public_key()).into();
    let trustor_id: AccountId = (&SecretKey::from_seed(&[121u8; 32]).public_key()).into();
    let other_issuer_id: AccountId = (&SecretKey::from_seed(&[122u8; 32]).public_key()).into();

    let asset_a = Asset::CreditAlphanum4(AlphaNum4 {
        asset_code: AssetCode4(*b"RUV\0"),
        issuer: issuer_id.clone(),
    });
    let asset_b = Asset::CreditAlphanum4(AlphaNum4 {
        asset_code: AssetCode4(*b"XLM\0"),
        issuer: other_issuer_id.clone(),
    });
    let pool_id = PoolId(Hash([80u8; 32]));

    let handle = build_pool_share_revoke_snapshot(
        &issuer_id,
        &trustor_id,
        &asset_a,
        &asset_b,
        &pool_id,
        100,
        1000,
        2000,
        500,
        true,
        &[(other_issuer_id.clone(), 100_000_000)],
    );

    let op = Operation {
        source_account: None,
        body: OperationBody::SetTrustLineFlags(SetTrustLineFlagsOp {
            trustor: trustor_id.clone(),
            asset: asset_a.clone(),
            clear_flags: TrustLineFlags::AuthorizedFlag as u32,
            set_flags: 0,
        }),
    };

    let meta = execute_single_op_tx_v4(&handle, &issuer_secret, op, &network_id);
    let op_events: &[stellar_xdr::OperationMetaV2] = meta.operations.as_ref();
    assert_eq!(op_events.len(), 1);
    let events: &[stellar_xdr::ContractEvent] = op_events[0].events.as_ref();

    // Expect exactly 3 events: transfer(asset_a) [0], transfer(asset_b) [1],
    // set_authorized [2]. amount_a = floor(100*1000/500)=200, amount_b=400.
    assert_eq!(
        events.len(),
        3,
        "expected 2 revoke transfers + 1 set_authorized, got {}",
        events.len()
    );

    let pool_address = ScAddress::LiquidityPool(pool_id.clone());
    let cb_a = revoke_cb_address(&issuer_id, 2, 0, &pool_id, &asset_a);
    let cb_b = revoke_cb_address(&issuer_id, 2, 0, &pool_id, &asset_b);

    // [0] transfer pool -> CB for asset_a
    assert_transfer_event(&events[0], &pool_address, &cb_a, &asset_a, 200);
    // [1] transfer pool -> CB for asset_b
    assert_transfer_event(&events[1], &pool_address, &cb_b, &asset_b, 400);

    // [2] set_authorized — emitted AFTER the revoke events.
    let ContractEventBody::V0(body) = &events[2].body;
    let topics: &[ScVal] = body.topics.as_ref();
    assert_eq!(topics[0], scval_symbol("set_authorized"));
    assert_eq!(
        topics[1],
        ScVal::Address(ScAddress::Account(trustor_id.clone()))
    );
    assert_eq!(topics[2], asset_string_scval(&asset_a));
    assert_eq!(body.data, ScVal::Bool(false));
}

/// #3117 Gap A regression: when the holder ISSUES one of the two pool assets,
/// that asset's redemption emits a `burn` (pool -> issuer) via the
/// `is_issuer` early-return path, while the other asset still emits a
/// `transfer`. Locks per-asset independence + the issuer early-return burn.
#[test]
fn test_classic_events_pool_share_revoke_burn_when_issuer() {
    let network_id = NetworkId::testnet();
    let issuer_secret = SecretKey::from_seed(&[123u8; 32]);
    let issuer_id: AccountId = (&issuer_secret.public_key()).into();
    // The trustor is also the issuer of asset_b.
    let trustor_secret = SecretKey::from_seed(&[124u8; 32]);
    let trustor_id: AccountId = (&trustor_secret.public_key()).into();

    let asset_a = Asset::CreditAlphanum4(AlphaNum4 {
        asset_code: AssetCode4(*b"RUV\0"),
        issuer: issuer_id.clone(),
    });
    // asset_b is issued BY the trustor → redemption should burn, not transfer.
    let asset_b = Asset::CreditAlphanum4(AlphaNum4 {
        asset_code: AssetCode4(*b"OWN\0"),
        issuer: trustor_id.clone(),
    });
    let pool_id = PoolId(Hash([81u8; 32]));

    let handle = build_pool_share_revoke_snapshot(
        &issuer_id,
        &trustor_id,
        &asset_a,
        &asset_b,
        &pool_id,
        100,
        1000,
        2000,
        500,
        false, // trustor issues asset_b → no asset_b trustline for it
        &[],
    );

    let op = Operation {
        source_account: None,
        body: OperationBody::SetTrustLineFlags(SetTrustLineFlagsOp {
            trustor: trustor_id.clone(),
            asset: asset_a.clone(),
            clear_flags: TrustLineFlags::AuthorizedFlag as u32,
            set_flags: 0,
        }),
    };

    let meta = execute_single_op_tx_v4(&handle, &issuer_secret, op, &network_id);
    let op_events: &[stellar_xdr::OperationMetaV2] = meta.operations.as_ref();
    assert_eq!(op_events.len(), 1);
    let events: &[stellar_xdr::ContractEvent] = op_events[0].events.as_ref();

    // transfer(asset_a) [0], burn(asset_b) [1], set_authorized [2].
    assert_eq!(events.len(), 3, "expected transfer + burn + set_authorized");

    let pool_address = ScAddress::LiquidityPool(pool_id.clone());
    let cb_a = revoke_cb_address(&issuer_id, 2, 0, &pool_id, &asset_a);

    // [0] transfer pool -> CB for asset_a (trustor not the issuer of asset_a).
    assert_transfer_event(&events[0], &pool_address, &cb_a, &asset_a, 200);

    // [1] burn pool -> (asset_b issuer) for asset_b. Topics: [burn, from, asset].
    let ContractEventBody::V0(burn_body) = &events[1].body;
    let burn_topics: &[ScVal] = burn_body.topics.as_ref();
    assert_eq!(burn_topics.len(), 3);
    assert_eq!(burn_topics[0], scval_symbol("burn"));
    assert_eq!(burn_topics[1], ScVal::Address(pool_address.clone()));
    assert_eq!(burn_topics[2], asset_string_scval(&asset_b));
    assert_eq!(burn_body.data, ScVal::I128(i128_parts(400)));

    // [2] set_authorized AFTER the revoke events.
    let ContractEventBody::V0(sa_body) = &events[2].body;
    let sa_topics: &[ScVal] = sa_body.topics.as_ref();
    assert_eq!(sa_topics[0], scval_symbol("set_authorized"));
}

/// Compute the revoke claimable-balance address for the event `to` field,
/// matching `get_revoke_id` (ENVELOPE_TYPE_POOL_REVOKE_OP_ID).
fn revoke_cb_address(
    source_id: &AccountId,
    seq: i64,
    op_index: u32,
    pool_id: &PoolId,
    asset: &Asset,
) -> ScAddress {
    use stellar_xdr::{ClaimableBalanceId, HashIdPreimage, HashIdPreimageRevokeId};
    let preimage = HashIdPreimage::PoolRevokeOpId(HashIdPreimageRevokeId {
        source_account: source_id.clone(),
        seq_num: SequenceNumber(seq),
        op_num: op_index,
        liquidity_pool_id: pool_id.clone(),
        asset: asset.clone(),
    });
    let hash = henyey_common::Hash256::hash_xdr(&preimage);
    ScAddress::ClaimableBalance(ClaimableBalanceId::ClaimableBalanceIdTypeV0(Hash::from(
        hash,
    )))
}

/// #3117 Gap B regression: CreateClaimableBalance in a tx with a memo emits a
/// `transfer` whose data is a PLAIN `i128` (NOT a `{amount, to_muxed_id}` map),
/// because the recipient is a claimable-balance address, not an ACCOUNT.
/// Fails on origin/main (memo wrongly attached to non-account recipient).
#[test]
fn test_classic_events_create_claimable_balance_with_memo_no_muxed_id() {
    let secret = SecretKey::from_seed(&[125u8; 32]);
    let source_id: AccountId = (&secret.public_key()).into();
    let claimant_id = AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([126u8; 32])));

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
        memo: Memo::Id(42),
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
    let mut executor =
        TransactionExecutor::new(&context, 0, SorobanConfig::default(), classic_events);
    let result = executor
        .execute_transaction(&snapshot, &envelope, 100, None)
        .expect("execute");
    assert!(result.success);

    let TransactionMeta::V4(meta) = result.tx_meta.expect("tx meta") else {
        panic!("unexpected tx meta");
    };
    let op_events: &[stellar_xdr::OperationMetaV2] = meta.operations.as_ref();
    let event_list: &[stellar_xdr::ContractEvent] = op_events[0].events.as_ref();
    assert_eq!(event_list.len(), 1);
    let ContractEventBody::V0(body) = &event_list[0].body;
    assert_eq!(
        body.data,
        ScVal::I128(i128_parts(20_000_000)),
        "CB recipient is not an ACCOUNT — memo must NOT be attached as to_muxed_id"
    );
}

/// CAP-67 coverage: PathPaymentStrictReceive emits claim-atom transfers plus a
/// final transfer to the destination (only strict_send was covered before).
#[test]
fn test_classic_events_emitted_for_path_payment_strict_receive() {
    let source_secret = SecretKey::from_seed(&[130u8; 32]);
    let source_id: AccountId = (&source_secret.public_key()).into();
    let dest_id = AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([131u8; 32])));
    let offer_secret = SecretKey::from_seed(&[132u8; 32]);
    let offer_id_account: AccountId = (&offer_secret.public_key()).into();
    let issuer_secret = SecretKey::from_seed(&[133u8; 32]);
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
        .add_entry(offer_key, offer_entry)
        .add_entry(issuer_key, issuer_entry)
        .add_entry(source_tl_key, source_tl_entry)
        .add_entry(offer_tl_key, offer_tl_entry)
        .add_entry(offer_entry_key, offer_entry_value)
        .build_with_default_header();
    let snapshot = SnapshotHandle::new(snapshot);

    let op_data = PathPaymentStrictReceiveOp {
        send_asset: asset_usd.clone(),
        send_max: 100_000_000,
        destination: dest_id.clone().into(),
        dest_asset: Asset::Native,
        dest_amount: 10_000_000,
        path: VecM::default(),
    };
    let operation = Operation {
        source_account: None,
        body: OperationBody::PathPaymentStrictReceive(op_data.clone()),
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
    let mut executor =
        TransactionExecutor::new(&context, 0, SorobanConfig::default(), classic_events);
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

    let claim_atoms: &[ClaimAtom] = match result.operation_results.get(0).expect("op result") {
        OperationResult::OpInner(OperationResultTr::PathPaymentStrictReceive(
            PathPaymentStrictReceiveResult::Success(PathPaymentStrictReceiveResultSuccess {
                offers,
                ..
            }),
        )) => offers.as_ref(),
        other => panic!("unexpected result: {:?}", other),
    };
    assert!(!claim_atoms.is_empty());

    let TransactionMeta::V4(meta) = result.tx_meta.expect("tx meta") else {
        panic!("unexpected tx meta");
    };
    let op_events: &[stellar_xdr::OperationMetaV2] = meta.operations.as_ref();
    let event_list: &[stellar_xdr::ContractEvent] = op_events[0].events.as_ref();
    assert_eq!(event_list.len(), claim_atoms.len() * 2 + 1);

    let mut index = 0;
    for claim in claim_atoms.iter() {
        index = assert_claim_atom_events(event_list, claim, &source_id, index);
    }
    let last_event = &event_list[event_list.len() - 1];
    assert_transfer_event(
        last_event,
        &ScAddress::Account(source_id),
        &ScAddress::Account(dest_id),
        &op_data.dest_asset,
        op_data.dest_amount,
    );
}

/// CAP-67 coverage: Inflation emits a `mint` (native) per winning payout.
#[test]
fn test_classic_events_emitted_for_inflation() {
    let source_secret = SecretKey::from_seed(&[140u8; 32]);
    let source_id: AccountId = (&source_secret.public_key()).into();
    let winner_id = AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([141u8; 32])));

    // Directly exercise the central emitter with a synthetic Inflation success
    // result, since the Inflation operation returns NotTime on-chain (24+).
    let classic_events = ClassicEventConfig {
        emit_classic_events: true,
        backfill_stellar_asset_events: false,
    };
    let network_id = NetworkId::testnet();
    let mut op_event_manager =
        OpEventManager::new(true, false, 25, network_id, Memo::None, classic_events);
    op_event_manager.new_mint_event(
        &Asset::Native,
        &henyey_tx::make_account_address(&winner_id),
        7_500_000,
        false,
    );
    let events = op_event_manager.finalize();
    assert_eq!(events.len(), 1);
    let ContractEventBody::V0(body) = &events[0].body;
    let topics: &[ScVal] = body.topics.as_ref();
    assert_eq!(topics[0], scval_symbol("mint"));
    assert_eq!(topics[1], ScVal::Address(ScAddress::Account(winner_id)));
    assert_eq!(topics[2], asset_string_scval(&Asset::Native));
    assert_eq!(body.data, ScVal::I128(i128_parts(7_500_000)));
    let _ = source_id;
}

/// CAP-67 coverage: a Payment whose destination is the asset ISSUER emits a
/// `burn` (issuer substitution at the integration level).
#[test]
fn test_classic_events_payment_to_issuer_emits_burn() {
    let issuer_secret = SecretKey::from_seed(&[150u8; 32]);
    let issuer_id: AccountId = (&issuer_secret.public_key()).into();
    let holder_secret = SecretKey::from_seed(&[151u8; 32]);
    let holder_id: AccountId = (&holder_secret.public_key()).into();

    let asset = Asset::CreditAlphanum4(AlphaNum4 {
        asset_code: AssetCode4([b'U', b'S', b'D', 0]),
        issuer: issuer_id.clone(),
    });
    let tl_asset = TrustLineAsset::CreditAlphanum4(AlphaNum4 {
        asset_code: AssetCode4([b'U', b'S', b'D', 0]),
        issuer: issuer_id.clone(),
    });

    let (issuer_key, issuer_entry) = create_account_entry(issuer_id.clone(), 1, 100_000_000);
    let (holder_key, holder_entry) = create_account_entry(holder_id.clone(), 1, 100_000_000);
    let (tl_key, tl_entry) = create_trustline_entry(
        holder_id.clone(),
        tl_asset,
        50_000_000,
        100_000_000,
        TrustLineFlags::AuthorizedFlag as u32,
    );

    let snapshot = SnapshotBuilder::new(1)
        .add_entry(issuer_key, issuer_entry)
        .add_entry(holder_key, holder_entry)
        .add_entry(tl_key, tl_entry)
        .build_with_default_header();
    let snapshot = SnapshotHandle::new(snapshot);

    let operation = Operation {
        source_account: None,
        body: OperationBody::Payment(stellar_xdr::PaymentOp {
            destination: issuer_id.clone().into(),
            asset: asset.clone(),
            amount: 10_000_000,
        }),
    };

    let tx = Transaction {
        source_account: MuxedAccount::Ed25519(Uint256(*holder_secret.public_key().as_bytes())),
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
    let decorated = sign_envelope(&envelope, &holder_secret, &network_id);
    if let TransactionEnvelope::Tx(ref mut env) = envelope {
        env.signatures = vec![decorated].try_into().unwrap();
    }

    let classic_events = ClassicEventConfig {
        emit_classic_events: true,
        backfill_stellar_asset_events: false,
    };
    let context = henyey_tx::LedgerContext::new(1, 1_000, 100, 5_000_000, 25, network_id);
    let mut executor =
        TransactionExecutor::new(&context, 0, SorobanConfig::default(), classic_events);
    let result = executor
        .execute_transaction(&snapshot, &envelope, 100, None)
        .expect("execute");
    assert!(result.success);

    let TransactionMeta::V4(meta) = result.tx_meta.expect("tx meta") else {
        panic!("unexpected tx meta");
    };
    let op_events: &[stellar_xdr::OperationMetaV2] = meta.operations.as_ref();
    let event_list: &[stellar_xdr::ContractEvent] = op_events[0].events.as_ref();
    assert_eq!(event_list.len(), 1);
    let ContractEventBody::V0(body) = &event_list[0].body;
    let topics: &[ScVal] = body.topics.as_ref();
    assert_eq!(topics.len(), 3, "burn has 3 topics");
    assert_eq!(topics[0], scval_symbol("burn"));
    assert_eq!(topics[1], ScVal::Address(ScAddress::Account(holder_id)));
    assert_eq!(topics[2], asset_string_scval(&asset));
    assert_eq!(body.data, ScVal::I128(i128_parts(10_000_000)));
}

/// CAP-67 coverage: a Payment whose SOURCE is the asset ISSUER emits a `mint`.
#[test]
fn test_classic_events_payment_from_issuer_emits_mint() {
    let issuer_secret = SecretKey::from_seed(&[160u8; 32]);
    let issuer_id: AccountId = (&issuer_secret.public_key()).into();
    let holder_id = AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([161u8; 32])));

    let asset = Asset::CreditAlphanum4(AlphaNum4 {
        asset_code: AssetCode4([b'U', b'S', b'D', 0]),
        issuer: issuer_id.clone(),
    });
    let tl_asset = TrustLineAsset::CreditAlphanum4(AlphaNum4 {
        asset_code: AssetCode4([b'U', b'S', b'D', 0]),
        issuer: issuer_id.clone(),
    });

    let (issuer_key, issuer_entry) = create_account_entry(issuer_id.clone(), 1, 100_000_000);
    let (holder_key, holder_entry) = create_account_entry(holder_id.clone(), 1, 100_000_000);
    let (tl_key, tl_entry) = create_trustline_entry(
        holder_id.clone(),
        tl_asset,
        0,
        100_000_000,
        TrustLineFlags::AuthorizedFlag as u32,
    );

    let snapshot = SnapshotBuilder::new(1)
        .add_entry(issuer_key, issuer_entry)
        .add_entry(holder_key, holder_entry)
        .add_entry(tl_key, tl_entry)
        .build_with_default_header();
    let snapshot = SnapshotHandle::new(snapshot);

    let operation = Operation {
        source_account: None,
        body: OperationBody::Payment(stellar_xdr::PaymentOp {
            destination: holder_id.clone().into(),
            asset: asset.clone(),
            amount: 10_000_000,
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
    let mut executor =
        TransactionExecutor::new(&context, 0, SorobanConfig::default(), classic_events);
    let result = executor
        .execute_transaction(&snapshot, &envelope, 100, None)
        .expect("execute");
    assert!(result.success);

    let TransactionMeta::V4(meta) = result.tx_meta.expect("tx meta") else {
        panic!("unexpected tx meta");
    };
    let op_events: &[stellar_xdr::OperationMetaV2] = meta.operations.as_ref();
    let event_list: &[stellar_xdr::ContractEvent] = op_events[0].events.as_ref();
    assert_eq!(event_list.len(), 1);
    let ContractEventBody::V0(body) = &event_list[0].body;
    let topics: &[ScVal] = body.topics.as_ref();
    assert_eq!(topics.len(), 3, "mint has 3 topics");
    assert_eq!(topics[0], scval_symbol("mint"));
    assert_eq!(topics[1], ScVal::Address(ScAddress::Account(holder_id)));
    assert_eq!(topics[2], asset_string_scval(&asset));
    assert_eq!(body.data, ScVal::I128(i128_parts(10_000_000)));
}

/// CAP-67 coverage: the tx-level `fee` event (charge, BeforeAllTxs stage).
/// Topics `[fee, from]`, data `i128(-fee)`. Exercised at the TxEventManager
/// unit level since the integration meta path does not populate tx-level fee
/// events in this harness.
#[test]
fn test_classic_events_fee_charge_and_refund() {
    let source_id = AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([170u8; 32])));
    let network_id = NetworkId::testnet();
    let classic_events = ClassicEventConfig {
        emit_classic_events: true,
        backfill_stellar_asset_events: false,
    };
    let mut tx_mgr = TxEventManager::new(true, 25, network_id, classic_events);

    // Charge: data = -fee, BeforeAllTxs (charge_fee negates the amount, matching
    // the production charge path).
    tx_mgr.charge_fee(
        &source_id,
        100,
        stellar_xdr::TransactionEventStage::BeforeAllTxs,
    );
    // Soroban refund: data = -refund, AfterAllTxs — mirrors apply.rs which calls
    // `new_fee_event(&fee_source, -refund, AfterAllTxs)`.
    tx_mgr.new_fee_event(
        &source_id,
        -30,
        stellar_xdr::TransactionEventStage::AfterAllTxs,
    );

    let events = tx_mgr.finalize();
    assert_eq!(events.len(), 2);

    let from = ScAddress::Account(source_id);

    // [0] charge
    let charge = &events[0];
    assert_eq!(
        charge.stage,
        stellar_xdr::TransactionEventStage::BeforeAllTxs
    );
    let stellar_xdr::ContractEventBody::V0(charge_body) = &charge.event.body;
    let charge_topics: &[ScVal] = charge_body.topics.as_ref();
    assert_eq!(charge_topics[0], scval_symbol("fee"));
    assert_eq!(charge_topics[1], ScVal::Address(from.clone()));
    assert_eq!(charge_body.data, ScVal::I128(i128_parts(-100)));

    // [1] refund
    let refund = &events[1];
    assert_eq!(
        refund.stage,
        stellar_xdr::TransactionEventStage::AfterAllTxs
    );
    let stellar_xdr::ContractEventBody::V0(refund_body) = &refund.event.body;
    let refund_topics: &[ScVal] = refund_body.topics.as_ref();
    assert_eq!(refund_topics[0], scval_symbol("fee"));
    assert_eq!(refund_topics[1], ScVal::Address(from));
    assert_eq!(refund_body.data, ScVal::I128(i128_parts(-30)));
}

/// CAP-67 coverage: claim-atom transfer pairs for the `V0` ClaimAtom variant.
#[test]
fn test_classic_events_emitted_for_claim_atoms_v0() {
    let source_secret = SecretKey::from_seed(&[180u8; 32]);
    let source_id: AccountId = (&source_secret.public_key()).into();
    let issuer_id: AccountId = (&SecretKey::from_seed(&[181u8; 32]).public_key()).into();

    let asset_usd = Asset::CreditAlphanum4(AlphaNum4 {
        asset_code: AssetCode4([b'U', b'S', b'D', 0]),
        issuer: issuer_id.clone(),
    });

    let claim = ClaimAtom::V0(stellar_xdr::ClaimOfferAtomV0 {
        seller_ed25519: Uint256([182u8; 32]),
        offer_id: 9,
        asset_sold: Asset::Native,
        amount_sold: 4_000_000,
        asset_bought: asset_usd,
        amount_bought: 3_000_000,
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
    op_event_manager.events_for_claim_atoms(&source_muxed, std::slice::from_ref(&claim));

    let events = op_event_manager.finalize();
    assert_eq!(events.len(), 2);
    let index = assert_claim_atom_events(&events, &claim, &source_id, 0);
    assert_eq!(index, 2);
}

/// CAP-67 coverage: claim-atom transfer pairs for the `LiquidityPool` variant.
#[test]
fn test_classic_events_emitted_for_claim_atoms_liquidity_pool() {
    let source_secret = SecretKey::from_seed(&[190u8; 32]);
    let source_id: AccountId = (&source_secret.public_key()).into();
    let issuer_id: AccountId = (&SecretKey::from_seed(&[191u8; 32]).public_key()).into();

    let asset_usd = Asset::CreditAlphanum4(AlphaNum4 {
        asset_code: AssetCode4([b'U', b'S', b'D', 0]),
        issuer: issuer_id.clone(),
    });

    let claim = ClaimAtom::LiquidityPool(ClaimLiquidityAtom {
        liquidity_pool_id: PoolId(Hash([92u8; 32])),
        asset_sold: Asset::Native,
        amount_sold: 6_000_000,
        asset_bought: asset_usd,
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
    op_event_manager.events_for_claim_atoms(&source_muxed, std::slice::from_ref(&claim));

    let events = op_event_manager.finalize();
    assert_eq!(events.len(), 2);
    let index = assert_claim_atom_events(&events, &claim, &source_id, 0);
    assert_eq!(index, 2);
}
