use super::*;

#[test]
fn test_execute_transaction_missing_operation() {
    let source = MuxedAccount::Ed25519(Uint256([0u8; 32]));
    let tx = Transaction {
        source_account: source,
        fee: 100,
        seq_num: SequenceNumber(1),
        cond: Preconditions::None,
        memo: Memo::None,
        operations: VecM::default(),
        ext: TransactionExt::V0,
    };
    let envelope = TransactionEnvelope::Tx(TransactionV1Envelope {
        tx,
        signatures: VecM::default(),
    });

    let snapshot = LedgerSnapshot::empty(1);
    let snapshot = SnapshotHandle::new(snapshot);
    let context = henyey_tx::LedgerContext::new(1, 1000, 100, 5_000_000, 25, NetworkId::testnet());
    let mut executor = TransactionExecutor::new(
        &context,
        0,
        SorobanConfig::default(),
        ClassicEventConfig::default(),
    );

    let result = executor
        .execute_transaction(&snapshot, &envelope, 100, None)
        .expect("execute");
    assert_eq!(result.failure, Some(ExecutionFailure::TxMissingOperation));
}

#[test]
fn test_execute_transaction_time_bounds_too_early() {
    let secret = SecretKey::from_seed(&[7u8; 32]);
    let account_id: AccountId = (&secret.public_key()).into();

    let (key, entry) = create_account_entry(account_id.clone(), 1, 10_000_000);
    let snapshot = SnapshotBuilder::new(1)
        .add_entry(key, entry)
        .build_with_default_header();
    let snapshot = SnapshotHandle::new(snapshot);

    let destination = AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([2u8; 32])));
    let operation = Operation {
        source_account: None,
        body: OperationBody::CreateAccount(CreateAccountOp {
            destination,
            starting_balance: 1_000_000,
        }),
    };

    let tx = Transaction {
        source_account: MuxedAccount::Ed25519(Uint256(*secret.public_key().as_bytes())),
        fee: 100,
        seq_num: SequenceNumber(2),
        cond: Preconditions::Time(TimeBounds {
            min_time: TimePoint(2_000),
            max_time: TimePoint(0),
        }),
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

    assert_eq!(result.failure, Some(ExecutionFailure::TxTooEarly));
}

#[test]
fn test_execute_transaction_min_seq_num_precondition() {
    let secret = SecretKey::from_seed(&[9u8; 32]);
    let account_id: AccountId = (&secret.public_key()).into();

    let (key, entry) = create_account_entry(account_id.clone(), 1, 10_000_000);
    let snapshot = SnapshotBuilder::new(1)
        .add_entry(key, entry)
        .build_with_default_header();
    let snapshot = SnapshotHandle::new(snapshot);

    let destination = AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([2u8; 32])));
    let operation = Operation {
        source_account: None,
        body: OperationBody::CreateAccount(CreateAccountOp {
            destination,
            starting_balance: 1_000_000,
        }),
    };

    let preconditions = Preconditions::V2(PreconditionsV2 {
        time_bounds: None,
        ledger_bounds: None,
        min_seq_num: Some(SequenceNumber(5)),
        min_seq_age: Duration(0),
        min_seq_ledger_gap: 0,
        extra_signers: VecM::default(),
    });

    let tx = Transaction {
        source_account: MuxedAccount::Ed25519(Uint256(*secret.public_key().as_bytes())),
        fee: 100,
        seq_num: SequenceNumber(2),
        cond: preconditions,
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

    // When minSeqNum is set, isBadSeq checks: account_seq < minSeqNum || account_seq >= tx_seq.
    // Here account_seq (1) < minSeqNum (5) → txBAD_SEQ, matching stellar-core's isBadSeq.
    assert_eq!(result.failure, Some(ExecutionFailure::TxBadSeq));
}

/// Test that with minSeqNum set, sequence validation is relaxed.
/// Instead of requiring tx.seqNum == account.seqNum + 1,
/// it allows any tx.seqNum where account.seqNum >= minSeqNum AND account.seqNum < tx.seqNum
#[test]
fn test_execute_transaction_min_seq_num_relaxed_sequence() {
    let secret = SecretKey::from_seed(&[14u8; 32]);
    let account_id: AccountId = (&secret.public_key()).into();

    // Account has seq_num = 100
    let (key, entry) = create_account_entry(account_id.clone(), 100, 10_000_000);
    let snapshot = SnapshotBuilder::new(1)
        .add_entry(key, entry)
        .build_with_default_header();
    let snapshot = SnapshotHandle::new(snapshot);

    let destination = AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([2u8; 32])));
    let operation = Operation {
        source_account: None,
        body: OperationBody::CreateAccount(CreateAccountOp {
            destination,
            starting_balance: 1_000_000,
        }),
    };

    // With minSeqNum = 50, account.seqNum (100) >= minSeqNum (50) is satisfied.
    // tx.seqNum = 105, account.seqNum (100) < tx.seqNum (105) is satisfied.
    // Without minSeqNum, this would fail because 100 + 1 != 105.
    // With minSeqNum, this should pass the sequence check.
    let preconditions = Preconditions::V2(PreconditionsV2 {
        time_bounds: None,
        ledger_bounds: None,
        min_seq_num: Some(SequenceNumber(50)),
        min_seq_age: Duration(0),
        min_seq_ledger_gap: 0,
        extra_signers: VecM::default(),
    });

    let tx = Transaction {
        source_account: MuxedAccount::Ed25519(Uint256(*secret.public_key().as_bytes())),
        fee: 100,
        seq_num: SequenceNumber(105), // Not account.seqNum + 1, but within valid range
        cond: preconditions,
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

    let context = henyey_tx::LedgerContext::new(1, 1_000, 100, 5_000_000, 25, network_id);
    let mut executor = TransactionExecutor::new(
        &context,
        0,
        SorobanConfig::default(),
        ClassicEventConfig::default(),
    );
    let result = executor
        .execute_transaction(&snapshot, &envelope, 100, None)
        .expect("execute");

    // Should succeed (or at least not fail with BadSequence)
    // The tx might fail for other reasons (like destination doesn't exist),
    // but the sequence check should pass.
    assert!(
        result.failure != Some(ExecutionFailure::TxBadSeq),
        "Transaction should not fail with BadSequence when minSeqNum is set and sequence is in valid range"
    );
}

/// Test that without minSeqNum, the strict sequence check still applies.
#[test]
fn test_execute_transaction_strict_sequence_without_min_seq_num() {
    let secret = SecretKey::from_seed(&[15u8; 32]);
    let account_id: AccountId = (&secret.public_key()).into();

    // Account has seq_num = 100
    let (key, entry) = create_account_entry(account_id.clone(), 100, 10_000_000);
    let snapshot = SnapshotBuilder::new(1)
        .add_entry(key, entry)
        .build_with_default_header();
    let snapshot = SnapshotHandle::new(snapshot);

    let destination = AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([2u8; 32])));
    let operation = Operation {
        source_account: None,
        body: OperationBody::CreateAccount(CreateAccountOp {
            destination,
            starting_balance: 1_000_000,
        }),
    };

    // Without minSeqNum, tx.seqNum must equal account.seqNum + 1.
    // tx.seqNum = 105, but account.seqNum + 1 = 101, so this should fail.
    let preconditions = Preconditions::V2(PreconditionsV2 {
        time_bounds: None,
        ledger_bounds: None,
        min_seq_num: None, // No minSeqNum
        min_seq_age: Duration(0),
        min_seq_ledger_gap: 0,
        extra_signers: VecM::default(),
    });

    let tx = Transaction {
        source_account: MuxedAccount::Ed25519(Uint256(*secret.public_key().as_bytes())),
        fee: 100,
        seq_num: SequenceNumber(105), // Wrong: should be 101
        cond: preconditions,
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

    let context = henyey_tx::LedgerContext::new(1, 1_000, 100, 5_000_000, 25, network_id);
    let mut executor = TransactionExecutor::new(
        &context,
        0,
        SorobanConfig::default(),
        ClassicEventConfig::default(),
    );
    let result = executor
        .execute_transaction(&snapshot, &envelope, 100, None)
        .expect("execute");

    // Should fail with BadSequence because strict check: 100 + 1 != 105
    assert_eq!(
        result.failure,
        Some(ExecutionFailure::TxBadSeq),
        "Transaction should fail with BadSequence when minSeqNum is not set and sequence doesn't match"
    );
}

#[test]
fn test_execute_transaction_min_seq_age_precondition() {
    let secret = SecretKey::from_seed(&[12u8; 32]);
    let account_id: AccountId = (&secret.public_key()).into();
    let seq_ledger = 5;
    let seq_time = 900;

    // Create account with seq_time set (for min_seq_age checking)
    let (key, entry) =
        create_account_entry_with_seq_info(account_id.clone(), 1, 10_000_000, seq_ledger, seq_time);
    let snapshot = SnapshotBuilder::new(10)
        .add_entry(key, entry)
        .build_with_default_header();
    let snapshot = SnapshotHandle::new(snapshot);

    let destination = AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([2u8; 32])));
    let operation = Operation {
        source_account: None,
        body: OperationBody::CreateAccount(CreateAccountOp {
            destination,
            starting_balance: 1_000_000,
        }),
    };

    // min_seq_age = 200, close_time = 1000, seq_time = 900
    // Check: closeTime - minSeqAge < accSeqTime → 1000 - 200 < 900 → 800 < 900 → TRUE → FAIL
    let preconditions = Preconditions::V2(PreconditionsV2 {
        time_bounds: None,
        ledger_bounds: None,
        min_seq_num: None,
        min_seq_age: Duration(200),
        min_seq_ledger_gap: 0,
        extra_signers: VecM::default(),
    });

    let tx = Transaction {
        source_account: MuxedAccount::Ed25519(Uint256(*secret.public_key().as_bytes())),
        fee: 100,
        seq_num: SequenceNumber(2),
        cond: preconditions,
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

    let context = henyey_tx::LedgerContext::new(
        10, 1_000, // close_time
        100, 5_000_000, 25, network_id,
    );
    let mut executor = TransactionExecutor::new(
        &context,
        0,
        SorobanConfig::default(),
        ClassicEventConfig::default(),
    );
    let result = executor
        .execute_transaction(&snapshot, &envelope, 100, None)
        .expect("execute");

    assert_eq!(result.failure, Some(ExecutionFailure::TxBadMinSeqAgeOrGap));
}

#[test]
fn test_execute_transaction_min_seq_ledger_gap_precondition() {
    let secret = SecretKey::from_seed(&[13u8; 32]);
    let account_id: AccountId = (&secret.public_key()).into();
    let seq_ledger = 8;
    let seq_time = 0;

    // Create account with seq_ledger set (for min_seq_ledger_gap checking)
    let (key, entry) =
        create_account_entry_with_seq_info(account_id.clone(), 1, 10_000_000, seq_ledger, seq_time);
    let snapshot = SnapshotBuilder::new(10)
        .add_entry(key, entry)
        .build_with_default_header();
    let snapshot = SnapshotHandle::new(snapshot);

    let destination = AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([2u8; 32])));
    let operation = Operation {
        source_account: None,
        body: OperationBody::CreateAccount(CreateAccountOp {
            destination,
            starting_balance: 1_000_000,
        }),
    };

    // min_seq_ledger_gap = 5, ledger_seq = 10, seq_ledger = 8
    // Check: ledgerSeq - minSeqLedgerGap < accSeqLedger → 10 - 5 < 8 → 5 < 8 → TRUE → FAIL
    let preconditions = Preconditions::V2(PreconditionsV2 {
        time_bounds: None,
        ledger_bounds: None,
        min_seq_num: None,
        min_seq_age: Duration(0),
        min_seq_ledger_gap: 5,
        extra_signers: VecM::default(),
    });

    let tx = Transaction {
        source_account: MuxedAccount::Ed25519(Uint256(*secret.public_key().as_bytes())),
        fee: 100,
        seq_num: SequenceNumber(2),
        cond: preconditions,
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

    let context = henyey_tx::LedgerContext::new(10, 1_000, 100, 5_000_000, 25, network_id);
    let mut executor = TransactionExecutor::new(
        &context,
        0,
        SorobanConfig::default(),
        ClassicEventConfig::default(),
    );
    let result = executor
        .execute_transaction(&snapshot, &envelope, 100, None)
        .expect("execute");

    assert_eq!(result.failure, Some(ExecutionFailure::TxBadMinSeqAgeOrGap));
}

#[test]
fn test_execute_transaction_extra_signers_missing() {
    let secret = SecretKey::from_seed(&[10u8; 32]);
    let account_id: AccountId = (&secret.public_key()).into();

    let (key, entry) = create_account_entry(account_id.clone(), 1, 10_000_000);
    let snapshot = SnapshotBuilder::new(1)
        .add_entry(key, entry)
        .build_with_default_header();
    let snapshot = SnapshotHandle::new(snapshot);

    let destination = AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([3u8; 32])));
    let operation = Operation {
        source_account: None,
        body: OperationBody::CreateAccount(CreateAccountOp {
            destination,
            starting_balance: 1_000_000,
        }),
    };

    let extra_signer = SignerKey::Ed25519(Uint256([4u8; 32]));
    let preconditions = Preconditions::V2(PreconditionsV2 {
        time_bounds: None,
        ledger_bounds: None,
        min_seq_num: None,
        min_seq_age: Duration(0),
        min_seq_ledger_gap: 0,
        extra_signers: vec![extra_signer].try_into().unwrap(),
    });

    let tx = Transaction {
        source_account: MuxedAccount::Ed25519(Uint256(*secret.public_key().as_bytes())),
        fee: 100,
        seq_num: SequenceNumber(2),
        cond: preconditions,
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

    assert_eq!(result.failure, Some(ExecutionFailure::TxBadAuthExtra));
}

#[test]
fn test_fee_bump_result_encoding() {
    let source = MuxedAccount::Ed25519(Uint256([0u8; 32]));
    let destination = AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([1u8; 32])));

    let operation = Operation {
        source_account: None,
        body: OperationBody::CreateAccount(CreateAccountOp {
            destination,
            starting_balance: 1_000_000,
        }),
    };

    let inner_tx = Transaction {
        source_account: source.clone(),
        fee: 100,
        seq_num: SequenceNumber(1),
        cond: Preconditions::None,
        memo: Memo::None,
        operations: vec![operation].try_into().unwrap(),
        ext: TransactionExt::V0,
    };

    let inner_env = TransactionV1Envelope {
        tx: inner_tx,
        signatures: VecM::default(),
    };

    let fee_bump = FeeBumpTransaction {
        fee_source: source,
        fee: 200,
        inner_tx: FeeBumpTransactionInnerTx::Tx(inner_env),
        ext: stellar_xdr::curr::FeeBumpTransactionExt::V0,
    };

    let envelope = TransactionEnvelope::TxFeeBump(FeeBumpTransactionEnvelope {
        tx: fee_bump,
        signatures: VecM::default(),
    });

    let exec = henyey_ledger::execution::TransactionExecutionResult {
        success: true,
        fee_charged: 200,
        fee_refund: 0,
        operation_results: vec![OperationResult::OpInner(OperationResultTr::CreateAccount(
            CreateAccountResult::Success,
        ))],
        error: None,
        failure: None,
        tx_meta: None,
        fee_changes: None,
        post_fee_changes: None,
        hot_archive_restored_keys: vec![],
        timings: Default::default(),
        tx_hash: None,
        fee_bump_outer_failure: false,
    };

    let pair = build_tx_result_pair(
        &henyey_tx::TransactionFrame::from_owned_with_network(envelope, NetworkId::testnet()),
        &NetworkId::testnet(),
        &exec,
        100, // base_fee
        24,  // protocol_version
    )
    .expect("build tx result");

    match pair.result.result {
        TransactionResultResult::TxFeeBumpInnerSuccess(InnerTransactionResultPair { .. }) => {}
        other => panic!("unexpected fee bump result: {:?}", other),
    }
}

/// Fee-bump outer-wrapper failures (e.g. fee source missing) must produce
/// a top-level result code, not TxFeeBumpInnerFailed. Matches stellar-core's
/// setError() behavior in FeeBumpTransactionFrame::commonValid.
#[test]
fn test_audit_574_fee_bump_outer_failure_is_top_level() {
    let source = MuxedAccount::Ed25519(Uint256([0u8; 32]));
    let destination = AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([1u8; 32])));

    let operation = Operation {
        source_account: None,
        body: OperationBody::CreateAccount(CreateAccountOp {
            destination,
            starting_balance: 1_000_000,
        }),
    };

    let inner_tx = Transaction {
        source_account: source.clone(),
        fee: 100,
        seq_num: SequenceNumber(1),
        cond: Preconditions::None,
        memo: Memo::None,
        operations: vec![operation].try_into().unwrap(),
        ext: TransactionExt::V0,
    };

    let inner_env = TransactionV1Envelope {
        tx: inner_tx,
        signatures: VecM::default(),
    };

    let fee_bump = FeeBumpTransaction {
        fee_source: source,
        fee: 200,
        inner_tx: FeeBumpTransactionInnerTx::Tx(inner_env),
        ext: stellar_xdr::curr::FeeBumpTransactionExt::V0,
    };

    let envelope = TransactionEnvelope::TxFeeBump(FeeBumpTransactionEnvelope {
        tx: fee_bump,
        signatures: VecM::default(),
    });

    // Simulate a fee-bump outer failure (e.g. fee source account not found)
    let exec = henyey_ledger::execution::TransactionExecutionResult {
        success: false,
        fee_charged: 0,
        fee_refund: 0,
        operation_results: vec![],
        error: Some("Fee source account not found".into()),
        failure: Some(TransactionResultCode::TxNoAccount),
        tx_meta: None,
        fee_changes: None,
        post_fee_changes: None,
        hot_archive_restored_keys: vec![],
        timings: Default::default(),
        tx_hash: None,
        fee_bump_outer_failure: true,
    };

    let pair = build_tx_result_pair(
        &henyey_tx::TransactionFrame::from_owned_with_network(envelope, NetworkId::testnet()),
        &NetworkId::testnet(),
        &exec,
        100,
        24,
    )
    .expect("build tx result");

    // Must be a top-level TxNoAccount, NOT TxFeeBumpInnerFailed
    match pair.result.result {
        TransactionResultResult::TxNoAccount => {}
        TransactionResultResult::TxFeeBumpInnerFailed(_) => {
            panic!("Fee-bump outer failure should NOT be wrapped as TxFeeBumpInnerFailed")
        }
        other => panic!("expected TxNoAccount, got {:?}", other),
    }
}

#[test]
fn test_operation_failure_rolls_back_changes() {
    let secret = SecretKey::from_seed(&[11u8; 32]);
    let account_id: AccountId = (&secret.public_key()).into();

    let (key, entry) = create_account_entry(account_id.clone(), 1, 10_000_000);
    let snapshot = SnapshotBuilder::new(1)
        .add_entry(key, entry)
        .build_with_default_header();
    let snapshot = SnapshotHandle::new(snapshot);

    let destination = AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([2u8; 32])));
    let op_create = Operation {
        source_account: None,
        body: OperationBody::CreateAccount(CreateAccountOp {
            destination: destination.clone(),
            starting_balance: 1_000_000,
        }),
    };

    let op_payment = Operation {
        source_account: None,
        body: OperationBody::Payment(stellar_xdr::curr::PaymentOp {
            destination: MuxedAccount::Ed25519(Uint256([9u8; 32])),
            asset: stellar_xdr::curr::Asset::Native,
            amount: 10,
        }),
    };

    let tx = Transaction {
        source_account: MuxedAccount::Ed25519(Uint256(*secret.public_key().as_bytes())),
        fee: 200,
        seq_num: SequenceNumber(2),
        cond: Preconditions::None,
        memo: Memo::None,
        operations: vec![op_create, op_payment].try_into().unwrap(),
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

    assert!(!result.success);
    // The body now executes (no fee-gate skip). The payment to non-existent
    // destination fails, producing TxFailed (not TxInsufficientBalance).
    assert_eq!(result.failure, Some(ExecutionFailure::TxFailed));

    let state = executor.state();
    assert!(state.get_account(&destination).is_none());

    let source = state.get_account(&account_id).expect("source account");
    assert_eq!(source.seq_num.0, 2);
    assert_eq!(source.balance, 10_000_000 - 200);
}

/// Regression test: fee-bump inner TX source account signature must be checked
/// against THRESHOLD_LOW (matching stellar-core's checkAllTransactionSignatures),
/// not THRESHOLD_MEDIUM. If we used medium, this TX would fail with
/// InvalidSignature because the master key weight (1) < medium threshold (2).
#[test]
fn test_fee_bump_inner_signature_uses_low_threshold() {
    let inner_secret = SecretKey::from_seed(&[20u8; 32]);
    let inner_account_id: AccountId = (&inner_secret.public_key()).into();

    let fee_secret = SecretKey::from_seed(&[21u8; 32]);
    let fee_account_id: AccountId = (&fee_secret.public_key()).into();

    // Inner source: master_weight=1, low=1, medium=2, high=3
    // Master key has weight 1 which passes low (1) but fails medium (2)
    let inner_key = LedgerKey::Account(stellar_xdr::curr::LedgerKeyAccount {
        account_id: inner_account_id.clone(),
    });
    let inner_entry = LedgerEntry {
        last_modified_ledger_seq: 1,
        data: LedgerEntryData::Account(AccountEntry {
            account_id: inner_account_id,
            balance: 10_000_000,
            seq_num: SequenceNumber(1),
            num_sub_entries: 0,
            inflation_dest: None,
            flags: 0,
            home_domain: String32::default(),
            thresholds: Thresholds([1, 1, 2, 3]),
            signers: VecM::default(),
            ext: AccountEntryExt::V0,
        }),
        ext: LedgerEntryExt::V0,
    };

    let (fee_key, fee_entry) = create_account_entry(fee_account_id.clone(), 1, 10_000_000);

    let snapshot = SnapshotBuilder::new(1)
        .add_entry(inner_key, inner_entry)
        .add_entry(fee_key, fee_entry)
        .build_with_default_header();
    let snapshot = SnapshotHandle::new(snapshot);

    let destination = AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([22u8; 32])));
    let operation = Operation {
        source_account: None,
        body: OperationBody::CreateAccount(CreateAccountOp {
            destination,
            starting_balance: 1_000_000,
        }),
    };

    let inner_tx = Transaction {
        source_account: MuxedAccount::Ed25519(Uint256(*inner_secret.public_key().as_bytes())),
        fee: 100,
        seq_num: SequenceNumber(2),
        cond: Preconditions::None,
        memo: Memo::None,
        operations: vec![operation].try_into().unwrap(),
        ext: TransactionExt::V0,
    };

    // Sign the inner TX with the inner source's key
    let inner_env = TransactionEnvelope::Tx(TransactionV1Envelope {
        tx: inner_tx.clone(),
        signatures: VecM::default(),
    });
    let network_id = NetworkId::testnet();
    let inner_sig = sign_envelope(&inner_env, &inner_secret, &network_id);

    let inner_v1 = TransactionV1Envelope {
        tx: inner_tx,
        signatures: vec![inner_sig].try_into().unwrap(),
    };

    let fee_bump = FeeBumpTransaction {
        fee_source: MuxedAccount::Ed25519(Uint256(*fee_secret.public_key().as_bytes())),
        fee: 200,
        inner_tx: FeeBumpTransactionInnerTx::Tx(inner_v1),
        ext: stellar_xdr::curr::FeeBumpTransactionExt::V0,
    };

    let mut envelope = TransactionEnvelope::TxFeeBump(FeeBumpTransactionEnvelope {
        tx: fee_bump,
        signatures: VecM::default(),
    });

    // Sign the outer envelope with the fee source's key
    let outer_sig = sign_envelope(&envelope, &fee_secret, &network_id);
    if let TransactionEnvelope::TxFeeBump(ref mut env) = envelope {
        env.signatures = vec![outer_sig].try_into().unwrap();
    }

    let context = henyey_tx::LedgerContext::new(1, 1_000, 100, 5_000_000, 25, network_id);
    let mut executor = TransactionExecutor::new(
        &context,
        0,
        SorobanConfig::default(),
        ClassicEventConfig::default(),
    );

    let result = executor
        .execute_transaction(&snapshot, &envelope, 100, None)
        .expect("execute");

    // Must NOT fail with InvalidSignature — if it does, the inner TX
    // signature was checked against medium threshold instead of low
    assert_ne!(
        result.failure,
        Some(ExecutionFailure::TxBadAuth),
        "fee-bump inner TX signature should use THRESHOLD_LOW, not THRESHOLD_MEDIUM"
    );
}

// ============================================================================
// CAP-77: Frozen key precondition tests
// ============================================================================

/// Fee-bump with frozen outer (fee) source account must fail with TxFrozenKeyAccessed.
#[test]
fn test_fee_bump_outer_source_frozen_key_rejected() {
    // Inner source
    let inner_secret = SecretKey::from_seed(&[30u8; 32]);
    let inner_account_id: AccountId = (&inner_secret.public_key()).into();

    // Fee source (will be frozen)
    let fee_secret = SecretKey::from_seed(&[31u8; 32]);
    let fee_account_id: AccountId = (&fee_secret.public_key()).into();

    // Set up both accounts in the snapshot
    let (inner_key, inner_entry) = create_account_entry(inner_account_id.clone(), 1, 10_000_000);
    let (fee_key, fee_entry) = create_account_entry(fee_account_id.clone(), 1, 10_000_000);
    let snapshot = SnapshotBuilder::new(1)
        .add_entry(inner_key, inner_entry)
        .add_entry(fee_key, fee_entry)
        .build_with_default_header();
    let snapshot = SnapshotHandle::new(snapshot);

    let destination = AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([99u8; 32])));
    let operation = Operation {
        source_account: None,
        body: OperationBody::CreateAccount(CreateAccountOp {
            destination,
            starting_balance: 1_000_000,
        }),
    };

    let inner_tx = Transaction {
        source_account: MuxedAccount::Ed25519(Uint256(*inner_secret.public_key().as_bytes())),
        fee: 100,
        seq_num: SequenceNumber(2),
        cond: Preconditions::None,
        memo: Memo::None,
        operations: vec![operation].try_into().unwrap(),
        ext: TransactionExt::V0,
    };

    let inner_env = TransactionV1Envelope {
        tx: inner_tx,
        signatures: VecM::default(),
    };

    let fee_bump = FeeBumpTransaction {
        fee_source: MuxedAccount::Ed25519(Uint256(*fee_secret.public_key().as_bytes())),
        fee: 200,
        inner_tx: FeeBumpTransactionInnerTx::Tx(inner_env),
        ext: stellar_xdr::curr::FeeBumpTransactionExt::V0,
    };

    let mut envelope = TransactionEnvelope::TxFeeBump(FeeBumpTransactionEnvelope {
        tx: fee_bump,
        signatures: VecM::default(),
    });

    let network_id = NetworkId::testnet();

    // Sign inner with inner secret
    let inner_sig = sign_envelope(
        &{
            let TransactionEnvelope::TxFeeBump(ref fb) = envelope else {
                panic!()
            };
            let FeeBumpTransactionInnerTx::Tx(ref inner) = fb.tx.inner_tx;
            TransactionEnvelope::Tx(inner.clone())
        },
        &inner_secret,
        &network_id,
    );
    if let TransactionEnvelope::TxFeeBump(ref mut fb) = envelope {
        let FeeBumpTransactionInnerTx::Tx(ref mut inner) = fb.tx.inner_tx;
        inner.signatures = vec![inner_sig].try_into().unwrap();
    }

    // Sign outer with fee source secret
    let outer_sig = sign_envelope(&envelope, &fee_secret, &network_id);
    if let TransactionEnvelope::TxFeeBump(ref mut fb) = envelope {
        fb.signatures = vec![outer_sig].try_into().unwrap();
    }

    // Create frozen key config with the fee source account frozen
    let fee_account_ledger_key = LedgerKey::Account(LedgerKeyAccount {
        account_id: fee_account_id.clone(),
    });
    let frozen_key_bytes = fee_account_ledger_key
        .to_xdr(stellar_xdr::curr::Limits::none())
        .unwrap();

    let mut context = henyey_tx::LedgerContext::new(1, 1_000, 100, 5_000_000, 26, network_id);
    context.frozen_key_config =
        henyey_tx::frozen_keys::FrozenKeyConfig::new(vec![frozen_key_bytes], vec![]);

    let mut executor = TransactionExecutor::new(
        &context,
        0,
        SorobanConfig::default(),
        ClassicEventConfig::default(),
    );

    let result = executor
        .execute_transaction(&snapshot, &envelope, 100, None)
        .expect("execute");

    assert_eq!(
        result.failure,
        Some(TransactionResultCode::TxFrozenKeyAccessed),
        "Fee-bump with frozen outer source should fail with TxFrozenKeyAccessed"
    );
    assert!(
        result.fee_bump_outer_failure,
        "Should be marked as fee bump outer failure"
    );
}

/// Fee-bump with frozen outer source is allowed when the tx hash is in the bypass set.
#[test]
fn test_fee_bump_outer_source_frozen_bypass_allowed() {
    // Inner source
    let inner_secret = SecretKey::from_seed(&[40u8; 32]);
    let inner_account_id: AccountId = (&inner_secret.public_key()).into();

    // Fee source (will be frozen, but bypass hash matches)
    let fee_secret = SecretKey::from_seed(&[41u8; 32]);
    let fee_account_id: AccountId = (&fee_secret.public_key()).into();

    let (inner_key, inner_entry) = create_account_entry(inner_account_id.clone(), 1, 10_000_000);
    let (fee_key, fee_entry) = create_account_entry(fee_account_id.clone(), 1, 10_000_000);

    let destination = AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([98u8; 32])));
    let (dest_key, dest_entry) = create_account_entry(destination.clone(), 0, 0);

    let snapshot = SnapshotBuilder::new(1)
        .add_entry(inner_key, inner_entry)
        .add_entry(fee_key, fee_entry)
        .add_entry(dest_key, dest_entry)
        .build_with_default_header();
    let snapshot = SnapshotHandle::new(snapshot);

    let operation = Operation {
        source_account: None,
        body: OperationBody::Payment(stellar_xdr::curr::PaymentOp {
            destination: MuxedAccount::Ed25519(Uint256([98u8; 32])),
            asset: stellar_xdr::curr::Asset::Native,
            amount: 1_000_000,
        }),
    };

    let inner_tx = Transaction {
        source_account: MuxedAccount::Ed25519(Uint256(*inner_secret.public_key().as_bytes())),
        fee: 100,
        seq_num: SequenceNumber(2),
        cond: Preconditions::None,
        memo: Memo::None,
        operations: vec![operation].try_into().unwrap(),
        ext: TransactionExt::V0,
    };

    let inner_env = TransactionV1Envelope {
        tx: inner_tx,
        signatures: VecM::default(),
    };

    let fee_bump = FeeBumpTransaction {
        fee_source: MuxedAccount::Ed25519(Uint256(*fee_secret.public_key().as_bytes())),
        fee: 200,
        inner_tx: FeeBumpTransactionInnerTx::Tx(inner_env),
        ext: stellar_xdr::curr::FeeBumpTransactionExt::V0,
    };

    let mut envelope = TransactionEnvelope::TxFeeBump(FeeBumpTransactionEnvelope {
        tx: fee_bump,
        signatures: VecM::default(),
    });

    let network_id = NetworkId::testnet();

    // Sign inner
    let inner_sig = sign_envelope(
        &{
            let TransactionEnvelope::TxFeeBump(ref fb) = envelope else {
                panic!()
            };
            let FeeBumpTransactionInnerTx::Tx(ref inner) = fb.tx.inner_tx;
            TransactionEnvelope::Tx(inner.clone())
        },
        &inner_secret,
        &network_id,
    );
    if let TransactionEnvelope::TxFeeBump(ref mut fb) = envelope {
        let FeeBumpTransactionInnerTx::Tx(ref mut inner) = fb.tx.inner_tx;
        inner.signatures = vec![inner_sig].try_into().unwrap();
    }

    // Sign outer
    let outer_sig = sign_envelope(&envelope, &fee_secret, &network_id);
    if let TransactionEnvelope::TxFeeBump(ref mut fb) = envelope {
        fb.signatures = vec![outer_sig].try_into().unwrap();
    }

    // Compute the outer tx hash (same as what precondition validation uses)
    let frame = henyey_tx::TransactionFrame::from_owned_with_network(envelope.clone(), network_id);
    let outer_hash = frame.hash(&network_id).expect("hash");

    // Create frozen key config with fee source frozen, but bypass the tx hash
    let fee_account_ledger_key = LedgerKey::Account(LedgerKeyAccount {
        account_id: fee_account_id.clone(),
    });
    let frozen_key_bytes = fee_account_ledger_key
        .to_xdr(stellar_xdr::curr::Limits::none())
        .unwrap();

    let mut context = henyey_tx::LedgerContext::new(1, 1_000, 100, 5_000_000, 26, network_id);
    context.frozen_key_config = henyey_tx::frozen_keys::FrozenKeyConfig::new(
        vec![frozen_key_bytes],
        vec![Hash(outer_hash.0)], // bypass this specific tx hash
    );

    let mut executor = TransactionExecutor::new(
        &context,
        0,
        SorobanConfig::default(),
        ClassicEventConfig::default(),
    );

    let result = executor
        .execute_transaction(&snapshot, &envelope, 100, None)
        .expect("execute");

    assert_ne!(
        result.failure,
        Some(TransactionResultCode::TxFrozenKeyAccessed),
        "Fee-bump with frozen outer source should be allowed when bypass hash matches"
    );
}

/// Inner TX source frozen should fail with TxFrozenKeyAccessed (non-fee-bump case).
#[test]
fn test_inner_source_frozen_key_rejected() {
    let secret = SecretKey::from_seed(&[50u8; 32]);
    let account_id: AccountId = (&secret.public_key()).into();

    let (key, entry) = create_account_entry(account_id.clone(), 1, 10_000_000);
    let snapshot = SnapshotBuilder::new(1)
        .add_entry(key, entry)
        .build_with_default_header();
    let snapshot = SnapshotHandle::new(snapshot);

    let destination = AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([97u8; 32])));
    let operation = Operation {
        source_account: None,
        body: OperationBody::CreateAccount(CreateAccountOp {
            destination,
            starting_balance: 1_000_000,
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

    // Freeze the source account
    let account_ledger_key = LedgerKey::Account(LedgerKeyAccount {
        account_id: account_id.clone(),
    });
    let frozen_key_bytes = account_ledger_key
        .to_xdr(stellar_xdr::curr::Limits::none())
        .unwrap();

    let mut context = henyey_tx::LedgerContext::new(1, 1_000, 100, 5_000_000, 26, network_id);
    context.frozen_key_config =
        henyey_tx::frozen_keys::FrozenKeyConfig::new(vec![frozen_key_bytes], vec![]);

    let mut executor = TransactionExecutor::new(
        &context,
        0,
        SorobanConfig::default(),
        ClassicEventConfig::default(),
    );

    let result = executor
        .execute_transaction(&snapshot, &envelope, 100, None)
        .expect("execute");

    assert_eq!(
        result.failure,
        Some(TransactionResultCode::TxFrozenKeyAccessed),
        "Transaction with frozen source account should fail with TxFrozenKeyAccessed"
    );
}

/// Inner TX source frozen should be allowed when bypass hash matches.
#[test]
fn test_inner_source_frozen_bypass_allowed() {
    let secret = SecretKey::from_seed(&[60u8; 32]);
    let account_id: AccountId = (&secret.public_key()).into();

    let (key, entry) = create_account_entry(account_id.clone(), 1, 10_000_000);

    let destination = AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([96u8; 32])));
    let (dest_key, dest_entry) = create_account_entry(destination.clone(), 0, 0);

    let snapshot = SnapshotBuilder::new(1)
        .add_entry(key, entry)
        .add_entry(dest_key, dest_entry)
        .build_with_default_header();
    let snapshot = SnapshotHandle::new(snapshot);

    let operation = Operation {
        source_account: None,
        body: OperationBody::Payment(stellar_xdr::curr::PaymentOp {
            destination: MuxedAccount::Ed25519(Uint256([96u8; 32])),
            asset: stellar_xdr::curr::Asset::Native,
            amount: 1_000_000,
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

    // Compute the tx hash for the bypass set
    let frame = henyey_tx::TransactionFrame::from_owned_with_network(envelope.clone(), network_id);
    let tx_hash = frame.hash(&network_id).expect("hash");

    // Freeze the source account but add the tx hash to the bypass set
    let account_ledger_key = LedgerKey::Account(LedgerKeyAccount {
        account_id: account_id.clone(),
    });
    let frozen_key_bytes = account_ledger_key
        .to_xdr(stellar_xdr::curr::Limits::none())
        .unwrap();

    let mut context = henyey_tx::LedgerContext::new(1, 1_000, 100, 5_000_000, 26, network_id);
    context.frozen_key_config =
        henyey_tx::frozen_keys::FrozenKeyConfig::new(vec![frozen_key_bytes], vec![Hash(tx_hash.0)]);

    let mut executor = TransactionExecutor::new(
        &context,
        0,
        SorobanConfig::default(),
        ClassicEventConfig::default(),
    );

    let result = executor
        .execute_transaction(&snapshot, &envelope, 100, None)
        .expect("execute");

    assert_ne!(
        result.failure,
        Some(TransactionResultCode::TxFrozenKeyAccessed),
        "Transaction with frozen source should be allowed when bypass hash matches"
    );
}

/// Regression test for AUDIT-180: when current ledger == max_ledger, the result
/// code must be TxTooLate (not TxFailed). stellar-core treats maxLedger as an
/// exclusive upper bound: maxLedger <= ledgerSeq means "too late".
#[test]
fn test_execute_transaction_ledger_bounds_equality_is_too_late() {
    let secret = SecretKey::from_seed(&[42u8; 32]);
    let account_id: AccountId = (&secret.public_key()).into();

    let (key, entry) = create_account_entry(account_id.clone(), 1, 10_000_000);
    let snapshot = SnapshotBuilder::new(1)
        .add_entry(key, entry)
        .build_with_default_header();
    let snapshot = SnapshotHandle::new(snapshot);

    let destination = AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([2u8; 32])));
    let operation = Operation {
        source_account: None,
        body: OperationBody::CreateAccount(CreateAccountOp {
            destination,
            starting_balance: 1_000_000,
        }),
    };

    // Ledger bounds: max_ledger = 100 (exclusive upper bound)
    // Context ledger: 100 (equality case — must be TxTooLate)
    let preconditions = Preconditions::V2(PreconditionsV2 {
        time_bounds: None,
        ledger_bounds: Some(LedgerBounds {
            min_ledger: 0,
            max_ledger: 100,
        }),
        min_seq_num: None,
        min_seq_age: Duration(0),
        min_seq_ledger_gap: 0,
        extra_signers: VecM::default(),
    });

    let tx = Transaction {
        source_account: MuxedAccount::Ed25519(Uint256(*secret.public_key().as_bytes())),
        fee: 100,
        seq_num: SequenceNumber(2),
        cond: preconditions,
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

    let context = henyey_tx::LedgerContext::new(100, 1_000, 100, 5_000_000, 25, network_id);
    let mut executor = TransactionExecutor::new(
        &context,
        0,
        SorobanConfig::default(),
        ClassicEventConfig::default(),
    );

    let result = executor
        .execute_transaction(&snapshot, &envelope, 100, None)
        .expect("execute");

    assert_eq!(
        result.failure,
        Some(ExecutionFailure::TxTooLate),
        "current == max_ledger must yield TxTooLate, not TxFailed"
    );
}

// =============================================================================
// AUDIT-238: Validation ordering regression tests.
//
// These tests verify that when a transaction fails multiple precondition checks
// simultaneously, the first-hit result code matches stellar-core's ordering:
//   Non-fee-bump: structural → time/ledger bounds → fee → account load
//   Fee-bump: outer fee → fee source load → inner time/ledger bounds → inner account load
// =============================================================================

/// AUDIT-238: Time bounds (too early) must be checked before insufficient fee.
/// A tx with minTime in the future AND fee=0 must return TxTooEarly, not TxInsufficientFee.
#[test]
fn test_time_bounds_too_early_checked_before_insufficient_fee() {
    // Use a source that does NOT exist — but time bounds should fail first anyway.
    // However, to isolate time-vs-fee, we DO provide the account so fee would be
    // the next failure if time bounds didn't catch it.
    let secret = SecretKey::from_seed(&[100u8; 32]);
    let account_id: AccountId = (&secret.public_key()).into();

    let (key, entry) = create_account_entry(account_id.clone(), 1, 10_000_000);
    let snapshot = SnapshotBuilder::new(1)
        .add_entry(key, entry)
        .build_with_default_header();
    let snapshot = SnapshotHandle::new(snapshot);

    let operation = Operation {
        source_account: None,
        body: OperationBody::CreateAccount(CreateAccountOp {
            destination: AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([2u8; 32]))),
            starting_balance: 1_000_000,
        }),
    };

    // minTime=5000 (future), fee=50 (insufficient since base_fee=100 * 1 op = 100)
    let tx = Transaction {
        source_account: MuxedAccount::Ed25519(Uint256(*secret.public_key().as_bytes())),
        fee: 50,
        seq_num: SequenceNumber(2),
        cond: Preconditions::Time(TimeBounds {
            min_time: TimePoint(5_000),
            max_time: TimePoint(0),
        }),
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

    // close_time=1000 < minTime=5000 → too early
    let context = henyey_tx::LedgerContext::new(1, 1_000, 100, 5_000_000, 25, network_id);
    let mut executor = TransactionExecutor::new(
        &context,
        0,
        SorobanConfig::default(),
        ClassicEventConfig::default(),
    );

    let result = executor
        .execute_transaction(&snapshot, &envelope, 100, None)
        .expect("execute");

    assert_eq!(
        result.failure,
        Some(ExecutionFailure::TxTooEarly),
        "AUDIT-238: time bounds must be checked before fee"
    );
    assert!(!result.fee_bump_outer_failure);
}

/// AUDIT-238: Time bounds (too late) must be checked before insufficient fee.
#[test]
fn test_time_bounds_too_late_checked_before_insufficient_fee() {
    let secret = SecretKey::from_seed(&[101u8; 32]);
    let account_id: AccountId = (&secret.public_key()).into();

    let (key, entry) = create_account_entry(account_id.clone(), 1, 10_000_000);
    let snapshot = SnapshotBuilder::new(1)
        .add_entry(key, entry)
        .build_with_default_header();
    let snapshot = SnapshotHandle::new(snapshot);

    let operation = Operation {
        source_account: None,
        body: OperationBody::CreateAccount(CreateAccountOp {
            destination: AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([2u8; 32]))),
            starting_balance: 1_000_000,
        }),
    };

    // maxTime=500 (past), fee=50 (insufficient, need 100)
    let tx = Transaction {
        source_account: MuxedAccount::Ed25519(Uint256(*secret.public_key().as_bytes())),
        fee: 50,
        seq_num: SequenceNumber(2),
        cond: Preconditions::Time(TimeBounds {
            min_time: TimePoint(0),
            max_time: TimePoint(500),
        }),
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

    // close_time=1000 > maxTime=500 → too late
    let context = henyey_tx::LedgerContext::new(1, 1_000, 100, 5_000_000, 25, network_id);
    let mut executor = TransactionExecutor::new(
        &context,
        0,
        SorobanConfig::default(),
        ClassicEventConfig::default(),
    );

    let result = executor
        .execute_transaction(&snapshot, &envelope, 100, None)
        .expect("execute");

    assert_eq!(
        result.failure,
        Some(ExecutionFailure::TxTooLate),
        "AUDIT-238: time bounds (too late) must be checked before fee"
    );
    assert!(!result.fee_bump_outer_failure);
}

/// AUDIT-238: Ledger bounds (too early) must be checked before insufficient fee.
#[test]
fn test_ledger_bounds_too_early_checked_before_insufficient_fee() {
    let secret = SecretKey::from_seed(&[102u8; 32]);
    let account_id: AccountId = (&secret.public_key()).into();

    let (key, entry) = create_account_entry(account_id.clone(), 1, 10_000_000);
    let snapshot = SnapshotBuilder::new(1)
        .add_entry(key, entry)
        .build_with_default_header();
    let snapshot = SnapshotHandle::new(snapshot);

    let operation = Operation {
        source_account: None,
        body: OperationBody::CreateAccount(CreateAccountOp {
            destination: AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([2u8; 32]))),
            starting_balance: 1_000_000,
        }),
    };

    // minLedger=1000 (future), fee=50 (insufficient, need 100)
    let preconditions = Preconditions::V2(PreconditionsV2 {
        time_bounds: None,
        ledger_bounds: Some(LedgerBounds {
            min_ledger: 1_000,
            max_ledger: 0,
        }),
        min_seq_num: None,
        min_seq_age: Duration(0),
        min_seq_ledger_gap: 0,
        extra_signers: VecM::default(),
    });

    let tx = Transaction {
        source_account: MuxedAccount::Ed25519(Uint256(*secret.public_key().as_bytes())),
        fee: 50,
        seq_num: SequenceNumber(2),
        cond: preconditions,
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

    // ledger_seq=10 < minLedger=1000 → too early
    let context = henyey_tx::LedgerContext::new(10, 1_000, 100, 5_000_000, 25, network_id);
    let mut executor = TransactionExecutor::new(
        &context,
        0,
        SorobanConfig::default(),
        ClassicEventConfig::default(),
    );

    let result = executor
        .execute_transaction(&snapshot, &envelope, 100, None)
        .expect("execute");

    assert_eq!(
        result.failure,
        Some(ExecutionFailure::TxTooEarly),
        "AUDIT-238: ledger bounds (too early) must be checked before fee"
    );
    assert!(!result.fee_bump_outer_failure);
}

/// AUDIT-238: Ledger bounds (too late) must be checked before insufficient fee.
#[test]
fn test_ledger_bounds_too_late_checked_before_insufficient_fee() {
    let secret = SecretKey::from_seed(&[103u8; 32]);
    let account_id: AccountId = (&secret.public_key()).into();

    let (key, entry) = create_account_entry(account_id.clone(), 1, 10_000_000);
    let snapshot = SnapshotBuilder::new(1)
        .add_entry(key, entry)
        .build_with_default_header();
    let snapshot = SnapshotHandle::new(snapshot);

    let operation = Operation {
        source_account: None,
        body: OperationBody::CreateAccount(CreateAccountOp {
            destination: AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([2u8; 32]))),
            starting_balance: 1_000_000,
        }),
    };

    // maxLedger=5 (past), fee=50 (insufficient, need 100)
    let preconditions = Preconditions::V2(PreconditionsV2 {
        time_bounds: None,
        ledger_bounds: Some(LedgerBounds {
            min_ledger: 0,
            max_ledger: 5,
        }),
        min_seq_num: None,
        min_seq_age: Duration(0),
        min_seq_ledger_gap: 0,
        extra_signers: VecM::default(),
    });

    let tx = Transaction {
        source_account: MuxedAccount::Ed25519(Uint256(*secret.public_key().as_bytes())),
        fee: 50,
        seq_num: SequenceNumber(2),
        cond: preconditions,
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

    // ledger_seq=10 >= maxLedger=5 → too late
    let context = henyey_tx::LedgerContext::new(10, 1_000, 100, 5_000_000, 25, network_id);
    let mut executor = TransactionExecutor::new(
        &context,
        0,
        SorobanConfig::default(),
        ClassicEventConfig::default(),
    );

    let result = executor
        .execute_transaction(&snapshot, &envelope, 100, None)
        .expect("execute");

    assert_eq!(
        result.failure,
        Some(ExecutionFailure::TxTooLate),
        "AUDIT-238: ledger bounds (too late) must be checked before fee"
    );
    assert!(!result.fee_bump_outer_failure);
}

/// AUDIT-238: Time bounds must be checked before account load.
/// A tx with nonexistent source AND expired time bounds must return TxTooEarly, not TxNoAccount.
#[test]
fn test_time_bounds_checked_before_account_load() {
    // Empty snapshot — source account does NOT exist
    let snapshot = SnapshotBuilder::new(1).build_with_default_header();
    let snapshot = SnapshotHandle::new(snapshot);

    let secret = SecretKey::from_seed(&[104u8; 32]);

    let operation = Operation {
        source_account: None,
        body: OperationBody::CreateAccount(CreateAccountOp {
            destination: AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([2u8; 32]))),
            starting_balance: 1_000_000,
        }),
    };

    // minTime=5000 (future), source account doesn't exist
    let tx = Transaction {
        source_account: MuxedAccount::Ed25519(Uint256(*secret.public_key().as_bytes())),
        fee: 100,
        seq_num: SequenceNumber(2),
        cond: Preconditions::Time(TimeBounds {
            min_time: TimePoint(5_000),
            max_time: TimePoint(0),
        }),
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

    // close_time=1000 < minTime=5000 → too early (before account load)
    let context = henyey_tx::LedgerContext::new(1, 1_000, 100, 5_000_000, 25, network_id);
    let mut executor = TransactionExecutor::new(
        &context,
        0,
        SorobanConfig::default(),
        ClassicEventConfig::default(),
    );

    let result = executor
        .execute_transaction(&snapshot, &envelope, 100, None)
        .expect("execute");

    assert_eq!(
        result.failure,
        Some(ExecutionFailure::TxTooEarly),
        "AUDIT-238: time bounds must be checked before account load"
    );
    assert!(!result.fee_bump_outer_failure);
}

/// AUDIT-238: Ledger bounds must be checked before account load.
#[test]
fn test_ledger_bounds_checked_before_account_load() {
    // Empty snapshot — source account does NOT exist
    let snapshot = SnapshotBuilder::new(1).build_with_default_header();
    let snapshot = SnapshotHandle::new(snapshot);

    let secret = SecretKey::from_seed(&[105u8; 32]);

    let operation = Operation {
        source_account: None,
        body: OperationBody::CreateAccount(CreateAccountOp {
            destination: AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([2u8; 32]))),
            starting_balance: 1_000_000,
        }),
    };

    // minLedger=500 (future), source account doesn't exist
    let preconditions = Preconditions::V2(PreconditionsV2 {
        time_bounds: None,
        ledger_bounds: Some(LedgerBounds {
            min_ledger: 500,
            max_ledger: 0,
        }),
        min_seq_num: None,
        min_seq_age: Duration(0),
        min_seq_ledger_gap: 0,
        extra_signers: VecM::default(),
    });

    let tx = Transaction {
        source_account: MuxedAccount::Ed25519(Uint256(*secret.public_key().as_bytes())),
        fee: 100,
        seq_num: SequenceNumber(2),
        cond: preconditions,
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

    // ledger_seq=10 < minLedger=500 → too early (before account load)
    let context = henyey_tx::LedgerContext::new(10, 1_000, 100, 5_000_000, 25, network_id);
    let mut executor = TransactionExecutor::new(
        &context,
        0,
        SorobanConfig::default(),
        ClassicEventConfig::default(),
    );

    let result = executor
        .execute_transaction(&snapshot, &envelope, 100, None)
        .expect("execute");

    assert_eq!(
        result.failure,
        Some(ExecutionFailure::TxTooEarly),
        "AUDIT-238: ledger bounds must be checked before account load"
    );
    assert!(!result.fee_bump_outer_failure);
}

/// AUDIT-238: Fee-bump outer fee must be checked before fee source account load.
/// A fee-bump with insufficient outer fee AND missing fee source must return
/// TxInsufficientFee (outer), not TxNoAccount.
#[test]
fn test_fee_bump_outer_fee_checked_before_fee_source_load() {
    // Empty snapshot — fee source does NOT exist
    let snapshot = SnapshotBuilder::new(1).build_with_default_header();
    let snapshot = SnapshotHandle::new(snapshot);

    let fee_source_secret = SecretKey::from_seed(&[106u8; 32]);
    let inner_secret = SecretKey::from_seed(&[107u8; 32]);

    let operation = Operation {
        source_account: None,
        body: OperationBody::CreateAccount(CreateAccountOp {
            destination: AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([2u8; 32]))),
            starting_balance: 1_000_000,
        }),
    };

    // Inner transaction
    let inner_tx = Transaction {
        source_account: MuxedAccount::Ed25519(Uint256(*inner_secret.public_key().as_bytes())),
        fee: 100,
        seq_num: SequenceNumber(2),
        cond: Preconditions::None,
        memo: Memo::None,
        operations: vec![operation].try_into().unwrap(),
        ext: TransactionExt::V0,
    };

    let inner_envelope = TransactionEnvelope::Tx(TransactionV1Envelope {
        tx: inner_tx,
        signatures: VecM::default(),
    });
    let network_id = NetworkId::testnet();
    let inner_sig = sign_envelope(&inner_envelope, &inner_secret, &network_id);
    let signed_inner = match inner_envelope {
        TransactionEnvelope::Tx(mut env) => {
            env.signatures = vec![inner_sig].try_into().unwrap();
            env
        }
        _ => unreachable!(),
    };

    // Fee-bump with fee=1 (way too low, base_fee=100 * 1 op = 100 minimum)
    let fee_bump_tx = FeeBumpTransaction {
        ext: stellar_xdr::curr::FeeBumpTransactionExt::V0,
        fee_source: MuxedAccount::Ed25519(Uint256(*fee_source_secret.public_key().as_bytes())),
        fee: 1, // Insufficient
        inner_tx: FeeBumpTransactionInnerTx::Tx(signed_inner),
    };

    let mut fee_bump_envelope = TransactionEnvelope::TxFeeBump(FeeBumpTransactionEnvelope {
        tx: fee_bump_tx,
        signatures: VecM::default(),
    });
    let outer_sig = sign_envelope(&fee_bump_envelope, &fee_source_secret, &network_id);
    if let TransactionEnvelope::TxFeeBump(ref mut env) = fee_bump_envelope {
        env.signatures = vec![outer_sig].try_into().unwrap();
    }

    let context = henyey_tx::LedgerContext::new(1, 1_000, 100, 5_000_000, 25, network_id);
    let mut executor = TransactionExecutor::new(
        &context,
        0,
        SorobanConfig::default(),
        ClassicEventConfig::default(),
    );

    let result = executor
        .execute_transaction(&snapshot, &fee_bump_envelope, 100, None)
        .expect("execute");

    assert_eq!(
        result.failure,
        Some(ExecutionFailure::TxInsufficientFee),
        "AUDIT-238: fee-bump outer fee must be checked before fee source account load"
    );
    assert!(
        result.fee_bump_outer_failure,
        "AUDIT-238: fee-bump outer failures must set fee_bump_outer_failure=true"
    );
}

/// AUDIT-238: Fee-bump inner time bounds must be checked before inner source account load.
/// A fee-bump with valid outer fee, existing fee source, but invalid inner time bounds
/// AND missing inner source must return TxTooEarly, not TxNoAccount.
#[test]
fn test_fee_bump_time_bounds_checked_before_inner_source_load() {
    let fee_source_secret = SecretKey::from_seed(&[108u8; 32]);
    let fee_source_id: AccountId = (&fee_source_secret.public_key()).into();
    let inner_secret = SecretKey::from_seed(&[109u8; 32]);

    // Only the fee source exists, inner source does NOT
    let (key, entry) = create_account_entry(fee_source_id.clone(), 1, 100_000_000);
    let snapshot = SnapshotBuilder::new(1)
        .add_entry(key, entry)
        .build_with_default_header();
    let snapshot = SnapshotHandle::new(snapshot);

    let operation = Operation {
        source_account: None,
        body: OperationBody::CreateAccount(CreateAccountOp {
            destination: AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([2u8; 32]))),
            starting_balance: 1_000_000,
        }),
    };

    // Inner tx with minTime=5000 (future) AND source account doesn't exist
    let inner_tx = Transaction {
        source_account: MuxedAccount::Ed25519(Uint256(*inner_secret.public_key().as_bytes())),
        fee: 100,
        seq_num: SequenceNumber(2),
        cond: Preconditions::Time(TimeBounds {
            min_time: TimePoint(5_000),
            max_time: TimePoint(0),
        }),
        memo: Memo::None,
        operations: vec![operation].try_into().unwrap(),
        ext: TransactionExt::V0,
    };

    let inner_envelope = TransactionEnvelope::Tx(TransactionV1Envelope {
        tx: inner_tx,
        signatures: VecM::default(),
    });
    let network_id = NetworkId::testnet();
    let inner_sig = sign_envelope(&inner_envelope, &inner_secret, &network_id);
    let signed_inner = match inner_envelope {
        TransactionEnvelope::Tx(mut env) => {
            env.signatures = vec![inner_sig].try_into().unwrap();
            env
        }
        _ => unreachable!(),
    };

    // Fee-bump with sufficient fee
    let fee_bump_tx = FeeBumpTransaction {
        ext: stellar_xdr::curr::FeeBumpTransactionExt::V0,
        fee_source: MuxedAccount::Ed25519(Uint256(*fee_source_secret.public_key().as_bytes())),
        fee: 200, // Sufficient (inner fee=100, so outer must be >= inner)
        inner_tx: FeeBumpTransactionInnerTx::Tx(signed_inner),
    };

    let mut fee_bump_envelope = TransactionEnvelope::TxFeeBump(FeeBumpTransactionEnvelope {
        tx: fee_bump_tx,
        signatures: VecM::default(),
    });
    let outer_sig = sign_envelope(&fee_bump_envelope, &fee_source_secret, &network_id);
    if let TransactionEnvelope::TxFeeBump(ref mut env) = fee_bump_envelope {
        env.signatures = vec![outer_sig].try_into().unwrap();
    }

    // close_time=1000 < minTime=5000 → too early (before inner account load)
    let context = henyey_tx::LedgerContext::new(1, 1_000, 100, 5_000_000, 25, network_id);
    let mut executor = TransactionExecutor::new(
        &context,
        0,
        SorobanConfig::default(),
        ClassicEventConfig::default(),
    );

    let result = executor
        .execute_transaction(&snapshot, &fee_bump_envelope, 100, None)
        .expect("execute");

    assert_eq!(
        result.failure,
        Some(ExecutionFailure::TxTooEarly),
        "AUDIT-238: fee-bump inner time bounds must be checked before inner source load"
    );
    // Time bounds failure is an inner failure, not outer
    assert!(!result.fee_bump_outer_failure);
}

/// Regression test for AUDIT-252: Soroban transaction with large total fee but
/// insufficient inclusion fee must be rejected with TxInsufficientFee.
///
/// Before the fix, the production path compared frame.fee() (total fee including
/// resource fee) against base_fee * op_count. For Soroban TXs, this allowed
/// transactions to pass the fee check even when their inclusion fee was below
/// the minimum, causing consensus divergence with stellar-core.
#[test]
fn test_soroban_insufficient_inclusion_fee_rejected() {
    // Soroban TX: total_fee=200, resource_fee=195, inclusion_fee=5
    // base_fee=100, op_count=1 → min_inclusion_fee=100
    // Should reject: 5 < 100 → TxInsufficientFee
    let secret = SecretKey::from_seed(&[200u8; 32]);

    let operation = Operation {
        source_account: None,
        body: OperationBody::InvokeHostFunction(InvokeHostFunctionOp {
            host_function: HostFunction::InvokeContract(InvokeContractArgs {
                contract_address: ScAddress::Contract(ContractId(Hash([0u8; 32]))),
                function_name: ScSymbol("test".try_into().unwrap()),
                args: VecM::default(),
            }),
            auth: VecM::default(),
        }),
    };

    let soroban_data = SorobanTransactionData {
        ext: SorobanTransactionDataExt::V0,
        resources: SorobanResources {
            footprint: LedgerFootprint {
                read_only: VecM::default(),
                read_write: VecM::default(),
            },
            instructions: 0,
            disk_read_bytes: 0,
            write_bytes: 0,
        },
        resource_fee: 195,
    };

    let tx = Transaction {
        source_account: MuxedAccount::Ed25519(Uint256(*secret.public_key().as_bytes())),
        fee: 200, // total_fee=200, inclusion_fee = 200 - 195 = 5
        seq_num: SequenceNumber(2),
        cond: Preconditions::None,
        memo: Memo::None,
        operations: vec![operation].try_into().unwrap(),
        ext: TransactionExt::V1(soroban_data),
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

    // No source account in snapshot — if fee check passes, next failure is TxNoAccount
    let snapshot = SnapshotBuilder::new(1).build_with_default_header();
    let snapshot = SnapshotHandle::new(snapshot);

    let context = henyey_tx::LedgerContext::new(1, 1_000, 100, 5_000_000, 25, network_id);
    let mut executor = TransactionExecutor::new(
        &context,
        0,
        SorobanConfig::default(),
        ClassicEventConfig::default(),
    );

    let result = executor
        .execute_transaction(&snapshot, &envelope, 100, None)
        .expect("execute");

    assert_eq!(
        result.failure,
        Some(ExecutionFailure::TxInsufficientFee),
        "AUDIT-252: Soroban TX with inclusion_fee=5 < base_fee=100 must be rejected"
    );
}

/// Positive control for AUDIT-252: Soroban transaction with sufficient inclusion
/// fee passes the fee check and fails on the next gate (TxNoAccount).
#[test]
fn test_soroban_sufficient_inclusion_fee_passes_fee_check() {
    // Soroban TX: total_fee=295, resource_fee=195, inclusion_fee=100
    // base_fee=100, op_count=1 → min_inclusion_fee=100
    // Should pass fee check: 100 >= 100
    let secret = SecretKey::from_seed(&[201u8; 32]);

    let operation = Operation {
        source_account: None,
        body: OperationBody::InvokeHostFunction(InvokeHostFunctionOp {
            host_function: HostFunction::InvokeContract(InvokeContractArgs {
                contract_address: ScAddress::Contract(ContractId(Hash([0u8; 32]))),
                function_name: ScSymbol("test".try_into().unwrap()),
                args: VecM::default(),
            }),
            auth: VecM::default(),
        }),
    };

    let soroban_data = SorobanTransactionData {
        ext: SorobanTransactionDataExt::V0,
        resources: SorobanResources {
            footprint: LedgerFootprint {
                read_only: VecM::default(),
                read_write: VecM::default(),
            },
            instructions: 0,
            disk_read_bytes: 0,
            write_bytes: 0,
        },
        resource_fee: 195,
    };

    let tx = Transaction {
        source_account: MuxedAccount::Ed25519(Uint256(*secret.public_key().as_bytes())),
        fee: 295, // total_fee=295, inclusion_fee = 295 - 195 = 100
        seq_num: SequenceNumber(2),
        cond: Preconditions::None,
        memo: Memo::None,
        operations: vec![operation].try_into().unwrap(),
        ext: TransactionExt::V1(soroban_data),
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

    // No source account in snapshot — fee check passes, then fails on TxNoAccount
    let snapshot = SnapshotBuilder::new(1).build_with_default_header();
    let snapshot = SnapshotHandle::new(snapshot);

    let context = henyey_tx::LedgerContext::new(1, 1_000, 100, 5_000_000, 25, network_id);
    let mut executor = TransactionExecutor::new(
        &context,
        0,
        SorobanConfig::default(),
        ClassicEventConfig::default(),
    );

    let result = executor
        .execute_transaction(&snapshot, &envelope, 100, None)
        .expect("execute");

    assert_eq!(
        result.failure,
        Some(ExecutionFailure::TxNoAccount),
        "AUDIT-252 positive control: Soroban TX with inclusion_fee=100 >= base_fee=100 should pass fee check"
    );
}
