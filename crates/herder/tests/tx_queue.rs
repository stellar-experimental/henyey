use henyey_common::{Hash256, Resource, ResourceType, NUM_SOROBAN_TX_RESOURCES};
use henyey_herder::{TransactionQueue, TxQueueConfig};
use henyey_tx::muxed_to_account_id;
use stellar_xdr::curr::{
    AccountId, AlphaNum4, Asset, AssetCode4, CreateAccountOp, DecoratedSignature, HostFunction,
    InvokeContractArgs, InvokeHostFunctionOp, LedgerFootprint, Limits, ManageSellOfferOp, Memo,
    MuxedAccount, Operation, OperationBody, Preconditions, Price, PublicKey, ScAddress, ScSymbol,
    ScVal, SequenceNumber, Signature as XdrSignature, SignatureHint, SorobanResources,
    SorobanTransactionData, SorobanTransactionDataExt, StringM, Transaction, TransactionEnvelope,
    TransactionExt, TransactionV1Envelope, Uint256, VecM, WriteXdr,
};

fn make_test_envelope(fee: u32, ops: usize) -> TransactionEnvelope {
    let source = MuxedAccount::Ed25519(Uint256([0u8; 32]));
    let operations: Vec<Operation> = (0..ops)
        .map(|_| Operation {
            source_account: None,
            body: OperationBody::CreateAccount(CreateAccountOp {
                destination: AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([1u8; 32]))),
                starting_balance: 1_000_000_000,
            }),
        })
        .collect();

    let tx = Transaction {
        source_account: source,
        fee,
        seq_num: SequenceNumber(1),
        cond: Preconditions::None,
        memo: Memo::None,
        operations: operations.try_into().unwrap(),
        ext: TransactionExt::V0,
    };

    TransactionEnvelope::Tx(TransactionV1Envelope {
        tx,
        signatures: vec![DecoratedSignature {
            hint: SignatureHint([0u8; 4]),
            signature: XdrSignature(vec![0u8; 64].try_into().unwrap()),
        }]
        .try_into()
        .unwrap(),
    })
}

fn make_dex_envelope(fee: u32, ops: usize) -> TransactionEnvelope {
    let source = MuxedAccount::Ed25519(Uint256([10u8; 32]));
    let selling = Asset::Native;
    let buying = Asset::CreditAlphanum4(AlphaNum4 {
        asset_code: AssetCode4(*b"USDC"),
        issuer: AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([11u8; 32]))),
    });
    let operations: Vec<Operation> = (0..ops)
        .map(|_| Operation {
            source_account: None,
            body: OperationBody::ManageSellOffer(ManageSellOfferOp {
                selling: selling.clone(),
                buying: buying.clone(),
                amount: 1,
                price: Price { n: 1, d: 1 },
                offer_id: 0,
            }),
        })
        .collect();

    let tx = Transaction {
        source_account: source,
        fee,
        seq_num: SequenceNumber(1),
        cond: Preconditions::None,
        memo: Memo::None,
        operations: operations.try_into().unwrap(),
        ext: TransactionExt::V0,
    };

    TransactionEnvelope::Tx(TransactionV1Envelope {
        tx,
        signatures: vec![DecoratedSignature {
            hint: SignatureHint([0u8; 4]),
            signature: XdrSignature(vec![0u8; 64].try_into().unwrap()),
        }]
        .try_into()
        .unwrap(),
    })
}

fn make_soroban_envelope_with_resources(fee: u32, instructions: u32) -> TransactionEnvelope {
    let source = MuxedAccount::Ed25519(Uint256([9u8; 32]));
    let function_name = ScSymbol(StringM::<32>::try_from("test".to_string()).expect("symbol"));
    let host_function = HostFunction::InvokeContract(InvokeContractArgs {
        contract_address: ScAddress::default(),
        function_name,
        args: VecM::<ScVal>::default(),
    });

    let op = Operation {
        source_account: None,
        body: OperationBody::InvokeHostFunction(InvokeHostFunctionOp {
            host_function,
            auth: VecM::default(),
        }),
    };

    let mut tx = Transaction {
        source_account: source,
        fee,
        seq_num: SequenceNumber(1),
        cond: Preconditions::None,
        memo: Memo::None,
        operations: vec![op].try_into().unwrap(),
        ext: TransactionExt::V0,
    };

    let resources = SorobanResources {
        footprint: LedgerFootprint {
            read_only: VecM::default(),
            read_write: VecM::default(),
        },
        instructions,
        disk_read_bytes: 0,
        write_bytes: 0,
    };
    tx.ext = TransactionExt::V1(SorobanTransactionData {
        ext: SorobanTransactionDataExt::V0,
        resources,
        resource_fee: 0,
    });

    TransactionEnvelope::Tx(TransactionV1Envelope {
        tx,
        signatures: vec![DecoratedSignature {
            hint: SignatureHint([0u8; 4]),
            signature: XdrSignature(vec![0u8; 64].try_into().unwrap()),
        }]
        .try_into()
        .unwrap(),
    })
}

fn set_source(envelope: &mut TransactionEnvelope, seed: u8) {
    let source = MuxedAccount::Ed25519(Uint256([seed; 32]));
    match envelope {
        TransactionEnvelope::TxV0(env) => {
            env.tx.source_account_ed25519 = Uint256([seed; 32]);
        }
        TransactionEnvelope::Tx(env) => {
            env.tx.source_account = source;
        }
        TransactionEnvelope::TxFeeBump(env) => match &mut env.tx.inner_tx {
            stellar_xdr::curr::FeeBumpTransactionInnerTx::Tx(inner) => {
                inner.tx.source_account = source;
            }
        },
    }
}

fn set_seq(envelope: &mut TransactionEnvelope, seq: i64) {
    match envelope {
        TransactionEnvelope::TxV0(env) => {
            env.tx.seq_num = SequenceNumber(seq);
        }
        TransactionEnvelope::Tx(env) => {
            env.tx.seq_num = SequenceNumber(seq);
        }
        TransactionEnvelope::TxFeeBump(env) => match &mut env.tx.inner_tx {
            stellar_xdr::curr::FeeBumpTransactionInnerTx::Tx(inner) => {
                inner.tx.seq_num = SequenceNumber(seq);
            }
        },
    }
}

fn account_key_from_envelope(envelope: &TransactionEnvelope) -> Vec<u8> {
    let source = match envelope {
        TransactionEnvelope::TxV0(env) => {
            stellar_xdr::curr::MuxedAccount::Ed25519(env.tx.source_account_ed25519.clone())
        }
        TransactionEnvelope::Tx(env) => env.tx.source_account.clone(),
        TransactionEnvelope::TxFeeBump(env) => match &env.tx.inner_tx {
            stellar_xdr::curr::FeeBumpTransactionInnerTx::Tx(inner) => {
                inner.tx.source_account.clone()
            }
        },
    };
    let account_id = muxed_to_account_id(&source);
    henyey_common::xdr_stream::xdr_to_bytes(&account_id)
}

fn full_hash(envelope: &TransactionEnvelope) -> Hash256 {
    Hash256::hash_xdr(envelope)
}

#[test]
fn test_dex_lane_limit_deterministic_selection() {
    let config = TxQueueConfig {
        max_dex_ops: Some(1),
        ..Default::default()
    };

    let mut dex_a = make_dex_envelope(200, 1);
    let mut dex_b = make_dex_envelope(200, 1);
    let mut classic = make_test_envelope(200, 1);
    set_source(&mut dex_a, 201);
    set_source(&mut dex_b, 202);
    set_source(&mut classic, 203);

    let queue = TransactionQueue::new(config);
    queue.try_add(dex_a.clone());
    queue.try_add(dex_b.clone());
    queue.try_add(classic.clone());

    let set = queue.get_transaction_set(Hash256::ZERO, 10);
    assert_eq!(set.len(), 2);

    let hash_dex_a = full_hash(&dex_a);
    let hash_dex_b = full_hash(&dex_b);
    let hash_classic = full_hash(&classic);
    let hashes: Vec<_> = set.transactions.iter().map(full_hash).collect();
    assert!(hashes.contains(&hash_classic));
    assert!(hashes.contains(&hash_dex_a) || hashes.contains(&hash_dex_b));
}

#[test]
fn test_classic_queue_limit_eviction() {
    let config = TxQueueConfig {
        max_queue_ops: Some(1),
        max_size: 10,
        min_fee_per_op: 0,
        ..Default::default()
    };
    let queue = TransactionQueue::new(config);

    let mut low = make_test_envelope(200, 1);
    let mut high = make_test_envelope(400, 1);
    set_source(&mut low, 21);
    set_source(&mut high, 22);

    let low_hash = full_hash(&low);
    let high_hash = full_hash(&high);

    assert_eq!(queue.try_add(low), henyey_herder::TxQueueResult::Added);
    assert_eq!(queue.try_add(high), henyey_herder::TxQueueResult::Added);
    assert!(!queue.contains(&low_hash));
    assert!(queue.contains(&high_hash));
}

#[test]
fn test_dex_queue_limit_eviction() {
    let config = TxQueueConfig {
        max_queue_dex_ops: Some(1),
        max_size: 10,
        min_fee_per_op: 0,
        ..Default::default()
    };
    let queue = TransactionQueue::new(config);

    let mut dex_low = make_dex_envelope(200, 1);
    let mut dex_high = make_dex_envelope(400, 1);
    set_source(&mut dex_low, 31);
    set_source(&mut dex_high, 32);

    let low_hash = full_hash(&dex_low);
    let high_hash = full_hash(&dex_high);

    assert_eq!(queue.try_add(dex_low), henyey_herder::TxQueueResult::Added);
    assert_eq!(queue.try_add(dex_high), henyey_herder::TxQueueResult::Added);
    assert!(!queue.contains(&low_hash));
    assert!(queue.contains(&high_hash));
}

#[test]
fn test_soroban_queue_limit_eviction() {
    let mut limit = Resource::new(vec![i64::MAX; NUM_SOROBAN_TX_RESOURCES]);
    limit.set_val(ResourceType::Instructions, 100);
    let config = TxQueueConfig {
        max_queue_soroban_resources: Some(limit),
        max_size: 10,
        min_fee_per_op: 0,
        ..Default::default()
    };
    let queue = TransactionQueue::new(config);

    let mut low_fee = make_soroban_envelope_with_resources(4000, 80);
    let mut high_fee = make_soroban_envelope_with_resources(8000, 80);
    set_source(&mut low_fee, 71);
    set_source(&mut high_fee, 72);

    let low_hash = full_hash(&low_fee);
    let high_hash = full_hash(&high_fee);

    assert_eq!(queue.try_add(low_fee), henyey_herder::TxQueueResult::Added);
    assert_eq!(queue.try_add(high_fee), henyey_herder::TxQueueResult::Added);
    assert!(!queue.contains(&low_hash));
    assert!(queue.contains(&high_hash));
}

#[test]
fn test_sequence_gap_blocks_following() {
    let queue = TransactionQueue::with_defaults();

    let mut tx_a = make_test_envelope(200, 1);
    let mut tx_b = make_test_envelope(200, 1);
    set_seq(&mut tx_a, 1);
    set_seq(&mut tx_b, 3);

    queue.try_add(tx_a);
    queue.try_add(tx_b);

    let set = queue.get_transaction_set(Hash256::ZERO, 10);
    let seqs: Vec<i64> = set
        .transactions
        .iter()
        .map(|tx| match tx {
            TransactionEnvelope::Tx(env) => env.tx.seq_num.0,
            TransactionEnvelope::TxV0(env) => env.tx.seq_num.0,
            TransactionEnvelope::TxFeeBump(env) => match &env.tx.inner_tx {
                stellar_xdr::curr::FeeBumpTransactionInnerTx::Tx(inner) => inner.tx.seq_num.0,
            },
        })
        .collect();
    assert_eq!(seqs, vec![1]);
}

#[test]
fn test_duplicate_sequence_prefers_higher_fee() {
    // With one-tx-per-account limit, use different accounts
    let queue = TransactionQueue::with_defaults();

    let mut low = make_test_envelope(200, 1);
    let mut high = make_test_envelope(400, 1);
    set_source(&mut low, 77);
    set_source(&mut high, 78); // Different account
    set_seq(&mut low, 5);
    set_seq(&mut high, 5);

    let low_hash = full_hash(&low);
    let high_hash = full_hash(&high);

    queue.try_add(low);
    queue.try_add(high);

    // Both transactions should be in the set now (different accounts)
    let set = queue.get_transaction_set(Hash256::ZERO, 10);
    let hashes: Vec<_> = set.transactions.iter().map(full_hash).collect();
    assert!(hashes.contains(&low_hash));
    assert!(hashes.contains(&high_hash));
}

#[test]
fn test_starting_sequence_excludes_prior() {
    // With one-tx-per-account limit, use different accounts
    let queue = TransactionQueue::with_defaults();

    let mut tx_a = make_test_envelope(200, 1);
    let mut tx_b = make_test_envelope(200, 1);
    set_source(&mut tx_a, 1);
    set_source(&mut tx_b, 2); // Different account
    set_seq(&mut tx_a, 5);
    set_seq(&mut tx_b, 6);

    queue.try_add(tx_a.clone());
    queue.try_add(tx_b);

    // Set starting sequence for account 1 to exclude tx_a
    let mut starting = std::collections::HashMap::new();
    starting.insert(account_key_from_envelope(&tx_a), 5);

    let set = queue.get_transaction_set_with_starting_seq(Hash256::ZERO, 10, Some(&starting));
    let seqs: Vec<i64> = set
        .transactions
        .iter()
        .map(|tx| match tx {
            TransactionEnvelope::Tx(env) => env.tx.seq_num.0,
            TransactionEnvelope::TxV0(env) => env.tx.seq_num.0,
            TransactionEnvelope::TxFeeBump(env) => match &env.tx.inner_tx {
                stellar_xdr::curr::FeeBumpTransactionInnerTx::Tx(inner) => inner.tx.seq_num.0,
            },
        })
        .collect();
    // tx_a with seq 5 is excluded (starting_seq >= 5), only tx_b with seq 6 remains
    assert_eq!(seqs, vec![6]);
}
