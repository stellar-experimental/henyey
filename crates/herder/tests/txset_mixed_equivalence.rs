//! End-to-end mixed classic+Soroban generalized tx-set equivalence test.
//!
//! Regression test for the HashedTx pipeline (#1888). Verifies that
//! `build_generalized_tx_set` produces deterministic, structurally correct,
//! and wire-path-valid output for a mixed classic+Soroban workload.

use std::collections::HashSet;

use henyey_common::{Hash256, NetworkId};
use stellar_xdr::curr::{
    AccountId, AlphaNum4, Asset, AssetCode4, CreateAccountOp, GeneralizedTransactionSet,
    InvokeContractArgs, InvokeHostFunctionOp, ManageSellOfferOp, MuxedAccount, Operation,
    OperationBody, Price, PublicKey, ScAddress, ScSymbol, ScVal, SequenceNumber, SorobanResources,
    SorobanTransactionData, SorobanTransactionDataExt, StringM, Transaction, TransactionEnvelope,
    TransactionExt, TransactionPhase, TransactionV1Envelope, TxSetComponent, Uint256, VecM,
    WriteXdr,
};

use henyey_herder::{TransactionQueue, TxQueueResult};

// ---------------------------------------------------------------------------
// Helper builders (following patterns from txset_summary.rs)
// ---------------------------------------------------------------------------

fn make_classic_payment(fee: u32) -> TransactionEnvelope {
    let source = MuxedAccount::Ed25519(Uint256([0u8; 32]));
    let op = Operation {
        source_account: None,
        body: OperationBody::CreateAccount(CreateAccountOp {
            destination: AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([200u8; 32]))),
            starting_balance: 1_000_000,
        }),
    };
    let tx = Transaction {
        source_account: source,
        fee,
        seq_num: SequenceNumber(1),
        cond: stellar_xdr::curr::Preconditions::None,
        memo: stellar_xdr::curr::Memo::None,
        operations: vec![op].try_into().unwrap(),
        ext: TransactionExt::V0,
    };
    TransactionEnvelope::Tx(TransactionV1Envelope {
        tx,
        signatures: VecM::default(),
    })
}

fn make_dex_tx(fee: u32) -> TransactionEnvelope {
    let source = MuxedAccount::Ed25519(Uint256([0u8; 32]));
    let selling = Asset::Native;
    let buying = Asset::CreditAlphanum4(AlphaNum4 {
        asset_code: AssetCode4(*b"USDC"),
        issuer: AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([201u8; 32]))),
    });
    let op = Operation {
        source_account: None,
        body: OperationBody::ManageSellOffer(ManageSellOfferOp {
            selling,
            buying,
            amount: 1,
            price: Price { n: 1, d: 1 },
            offer_id: 0,
        }),
    };
    let tx = Transaction {
        source_account: source,
        fee,
        seq_num: SequenceNumber(1),
        cond: stellar_xdr::curr::Preconditions::None,
        memo: stellar_xdr::curr::Memo::None,
        operations: vec![op].try_into().unwrap(),
        ext: TransactionExt::V0,
    };
    TransactionEnvelope::Tx(TransactionV1Envelope {
        tx,
        signatures: VecM::default(),
    })
}

fn make_multi_op_classic(fee: u32, ops: usize) -> TransactionEnvelope {
    let source = MuxedAccount::Ed25519(Uint256([0u8; 32]));
    let operations: Vec<Operation> = (0..ops)
        .map(|i| Operation {
            source_account: None,
            body: OperationBody::CreateAccount(CreateAccountOp {
                destination: AccountId(PublicKey::PublicKeyTypeEd25519(Uint256(
                    [202u8.wrapping_add(i as u8); 32],
                ))),
                starting_balance: 1_000_000,
            }),
        })
        .collect();
    let tx = Transaction {
        source_account: source,
        fee,
        seq_num: SequenceNumber(1),
        cond: stellar_xdr::curr::Preconditions::None,
        memo: stellar_xdr::curr::Memo::None,
        operations: operations.try_into().unwrap(),
        ext: TransactionExt::V0,
    };
    TransactionEnvelope::Tx(TransactionV1Envelope {
        tx,
        signatures: VecM::default(),
    })
}

fn make_soroban_tx(fee: u32, instructions: u32) -> TransactionEnvelope {
    let source = MuxedAccount::Ed25519(Uint256([0u8; 32]));
    let op = Operation {
        source_account: None,
        body: OperationBody::InvokeHostFunction(InvokeHostFunctionOp {
            host_function: stellar_xdr::curr::HostFunction::InvokeContract(InvokeContractArgs {
                contract_address: ScAddress::default(),
                function_name: ScSymbol(
                    StringM::<32>::try_from("test".to_string()).expect("symbol"),
                ),
                args: VecM::<ScVal>::default(),
            }),
            auth: VecM::default(),
        }),
    };
    let resources = SorobanResources {
        footprint: stellar_xdr::curr::LedgerFootprint {
            read_only: VecM::default(),
            read_write: VecM::default(),
        },
        instructions,
        disk_read_bytes: 0,
        write_bytes: 0,
    };
    let tx = Transaction {
        source_account: source,
        fee,
        seq_num: SequenceNumber(1),
        cond: stellar_xdr::curr::Preconditions::None,
        memo: stellar_xdr::curr::Memo::None,
        operations: vec![op].try_into().unwrap(),
        ext: TransactionExt::V1(SorobanTransactionData {
            ext: SorobanTransactionDataExt::V0,
            resources,
            resource_fee: 0,
        }),
    };
    TransactionEnvelope::Tx(TransactionV1Envelope {
        tx,
        signatures: VecM::default(),
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

fn tx_hash(tx: &TransactionEnvelope) -> Hash256 {
    Hash256::hash_xdr(tx)
}

/// Flatten a GeneralizedTransactionSet into all contained envelopes.
fn flatten_gen_tx_set(gen: &GeneralizedTransactionSet) -> Vec<TransactionEnvelope> {
    let GeneralizedTransactionSet::V1(v1) = gen;
    let mut all = Vec::new();
    for phase in v1.phases.iter() {
        match phase {
            TransactionPhase::V0(components) => {
                for comp in components.iter() {
                    let TxSetComponent::TxsetCompTxsMaybeDiscountedFee(c) = comp;
                    all.extend(c.txs.iter().cloned());
                }
            }
            TransactionPhase::V1(parallel) => {
                for stage in parallel.execution_stages.iter() {
                    for cluster in stage.0.iter() {
                        all.extend(cluster.0.iter().cloned());
                    }
                }
            }
        }
    }
    all
}

fn xdr_bytes(gen: &GeneralizedTransactionSet) -> Vec<u8> {
    gen.to_xdr(stellar_xdr::curr::Limits::none())
        .expect("XDR encode")
}

// ---------------------------------------------------------------------------
// Test
// ---------------------------------------------------------------------------

#[test]
fn test_mixed_classic_soroban_txset_deterministic() {
    // Build 5 transactions: 3 classic + 2 soroban, each with a distinct source.
    let mut payment = make_classic_payment(200);
    set_source(&mut payment, 1);

    let mut dex = make_dex_tx(400);
    set_source(&mut dex, 2);

    let mut multi_op = make_multi_op_classic(300, 2);
    set_source(&mut multi_op, 3);

    let mut soroban1 = make_soroban_tx(500, 10);
    set_source(&mut soroban1, 4);

    let mut soroban2 = make_soroban_tx(600, 20);
    set_source(&mut soroban2, 5);

    let input_txs = vec![
        payment.clone(),
        dex.clone(),
        multi_op.clone(),
        soroban1.clone(),
        soroban2.clone(),
    ];
    let input_hashes: HashSet<Hash256> = input_txs.iter().map(tx_hash).collect();
    assert_eq!(input_hashes.len(), 5, "all 5 txs must have distinct hashes");

    // --- Build forward-order set ---
    let queue1 = TransactionQueue::with_defaults();
    for tx in &input_txs {
        assert_eq!(
            queue1.try_add(tx.clone()),
            TxQueueResult::Added,
            "forward-order try_add must succeed"
        );
    }
    let (tx_set1, gen1) = queue1.build_generalized_tx_set(Hash256::ZERO, 100);

    // --- Determinism: build reverse-order set ---
    let queue2 = TransactionQueue::with_defaults();
    for tx in input_txs.iter().rev() {
        assert_eq!(
            queue2.try_add(tx.clone()),
            TxQueueResult::Added,
            "reverse-order try_add must succeed"
        );
    }
    let (tx_set2, gen2) = queue2.build_generalized_tx_set(Hash256::ZERO, 100);

    let gen1_bytes = xdr_bytes(&gen1);
    let gen2_bytes = xdr_bytes(&gen2);
    assert_eq!(
        gen1_bytes, gen2_bytes,
        "GeneralizedTransactionSet XDR must be identical regardless of insertion order"
    );
    assert_eq!(
        tx_set1.hash, tx_set2.hash,
        "TransactionSet hashes must match"
    );

    // --- Structural assertions ---
    let GeneralizedTransactionSet::V1(ref v1) = gen1;
    assert_eq!(v1.phases.len(), 2, "must have exactly 2 phases");

    // Phase 0: classic (V0)
    let classic_count = match &v1.phases[0] {
        TransactionPhase::V0(components) => {
            let mut count = 0;
            for comp in components.iter() {
                let TxSetComponent::TxsetCompTxsMaybeDiscountedFee(c) = comp;
                count += c.txs.len();
            }
            count
        }
        other => panic!("phase 0 should be V0 (classic), got {:?}", other),
    };
    assert_eq!(classic_count, 3, "classic phase must contain 3 txs");

    // Phase 1: soroban (V1)
    let soroban_count = match &v1.phases[1] {
        TransactionPhase::V1(parallel) => {
            let mut count = 0;
            for stage in parallel.execution_stages.iter() {
                for cluster in stage.0.iter() {
                    count += cluster.0.len();
                }
            }
            count
        }
        other => panic!("phase 1 should be V1 (soroban), got {:?}", other),
    };
    assert_eq!(soroban_count, 2, "soroban phase must contain 2 txs");

    // --- Three-way envelope consistency ---
    assert_eq!(
        tx_set1.transactions.len(),
        5,
        "tx_set.transactions must contain all 5 txs"
    );

    let flattened = flatten_gen_tx_set(&gen1);
    assert_eq!(
        flattened.len(),
        5,
        "flattened GeneralizedTransactionSet must contain exactly 5 envelopes"
    );

    // Compare flattened hashes vs tx_set.transactions hashes (ordered).
    let flattened_hashes: Vec<Hash256> = flattened.iter().map(tx_hash).collect();
    let tx_set_hashes: Vec<Hash256> = tx_set1.transactions.iter().map(tx_hash).collect();
    assert_eq!(
        flattened_hashes, tx_set_hashes,
        "flattened gen-set envelope hashes must match tx_set.transactions hashes in order"
    );

    // Compare against input hashes as a set (order-independent).
    let flattened_hash_set: HashSet<Hash256> = flattened_hashes.iter().copied().collect();
    assert_eq!(
        flattened_hash_set, input_hashes,
        "flattened envelope hashes must equal input tx hashes"
    );

    // --- generalized_tx_set field ---
    assert!(
        tx_set1.generalized_tx_set.is_some(),
        "generalized_tx_set must be Some"
    );
    let embedded_gen_bytes = xdr_bytes(tx_set1.generalized_tx_set.as_ref().unwrap());
    assert_eq!(
        embedded_gen_bytes, gen1_bytes,
        "embedded generalized_tx_set XDR must equal separately returned one"
    );

    // --- Hash consistency ---
    let recomputed_hash = Hash256::hash_xdr(&gen1);
    assert_eq!(
        tx_set1.hash, recomputed_hash,
        "tx_set.hash must equal SHA-256 of GeneralizedTransactionSet XDR"
    );

    // --- Wire-path round-trip ---
    let stored = tx_set1.to_xdr_stored_set();
    let round_tripped =
        henyey_herder::TransactionSet::from_xdr_stored_set(&stored).expect("round-trip decode");
    assert_eq!(
        round_tripped.hash, tx_set1.hash,
        "round-tripped hash must match original"
    );
    let rt_gen_bytes = xdr_bytes(round_tripped.generalized_tx_set.as_ref().unwrap());
    assert_eq!(
        rt_gen_bytes, gen1_bytes,
        "round-tripped generalized_tx_set XDR must equal original"
    );

    let prepared = round_tripped
        .prepare_for_apply(NetworkId::testnet())
        .expect("prepare_for_apply must succeed on a well-formed tx set");
    assert_eq!(
        prepared.hash, tx_set1.hash,
        "prepared set hash must match original"
    );
}
