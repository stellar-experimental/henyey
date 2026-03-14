use std::collections::HashMap;

use henyey_common::{Hash256, NetworkId};
use henyey_tx::TransactionFrame;
use stellar_xdr::curr::{
    AccountId, AlphaNum4, Asset, AssetCode4, CreateAccountOp, InvokeContractArgs,
    InvokeHostFunctionOp, LedgerFootprint, ManageSellOfferOp, MuxedAccount, Operation,
    OperationBody, Price, PublicKey, ScAddress, ScSymbol, ScVal, SequenceNumber, SorobanResources,
    SorobanTransactionData, SorobanTransactionDataExt, StringM, Transaction, TransactionEnvelope,
    TransactionExt, TransactionV1Envelope, TxSetComponent, Uint256, VecM, WriteXdr,
};

use henyey_herder::TransactionQueue;

struct TxSetSummary {
    total_fees: i64,
    total_inclusion_fees: i64,
    classic_ops: i64,
    classic_non_dex_txs: usize,
    classic_non_dex_base_fee: i64,
    classic_dex_txs: usize,
    classic_dex_base_fee: i64,
    soroban_ops: i64,
    soroban_base_fee: i64,
    insns: i64,
    disk_read_bytes: i64,
    write_bytes: i64,
    disk_read_entries: i64,
    write_entries: i64,
    tx_size_bytes: i64,
}

fn tx_hash(tx: &TransactionEnvelope) -> Hash256 {
    Hash256::hash_xdr(tx).expect("hash tx")
}

fn make_classic_payment(fee: u32) -> TransactionEnvelope {
    let source = MuxedAccount::Ed25519(Uint256([0u8; 32]));
    let op = Operation {
        source_account: None,
        body: OperationBody::CreateAccount(CreateAccountOp {
            destination: AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([2u8; 32]))),
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

fn make_classic_dex(fee: u32) -> TransactionEnvelope {
    let source = MuxedAccount::Ed25519(Uint256([3u8; 32]));
    let selling = Asset::Native;
    let buying = Asset::CreditAlphanum4(AlphaNum4 {
        asset_code: AssetCode4(*b"USDC"),
        issuer: AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([4u8; 32]))),
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

fn make_soroban_tx(total_fee: u32, resource_fee: i64) -> TransactionEnvelope {
    let source = MuxedAccount::Ed25519(Uint256([5u8; 32]));
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
    let footprint = LedgerFootprint {
        read_only: vec![
            stellar_xdr::curr::LedgerKey::Account(stellar_xdr::curr::LedgerKeyAccount {
                account_id: AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([6u8; 32]))),
            }),
            stellar_xdr::curr::LedgerKey::Account(stellar_xdr::curr::LedgerKeyAccount {
                account_id: AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([7u8; 32]))),
            }),
        ]
        .try_into()
        .unwrap(),
        read_write: vec![stellar_xdr::curr::LedgerKey::Account(
            stellar_xdr::curr::LedgerKeyAccount {
                account_id: AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([8u8; 32]))),
            },
        )]
        .try_into()
        .unwrap(),
    };
    let resources = SorobanResources {
        footprint,
        instructions: 10,
        disk_read_bytes: 5,
        write_bytes: 7,
    };
    let tx = Transaction {
        source_account: source,
        fee: total_fee,
        seq_num: SequenceNumber(1),
        cond: stellar_xdr::curr::Preconditions::None,
        memo: stellar_xdr::curr::Memo::None,
        operations: vec![op].try_into().unwrap(),
        ext: TransactionExt::V1(SorobanTransactionData {
            ext: SorobanTransactionDataExt::V0,
            resources,
            resource_fee,
        }),
    };
    TransactionEnvelope::Tx(TransactionV1Envelope {
        tx,
        signatures: VecM::default(),
    })
}

fn summary_for_set(
    txs: &[TransactionEnvelope],
    gen: &stellar_xdr::curr::GeneralizedTransactionSet,
) -> TxSetSummary {
    let mut base_fee_map: HashMap<Hash256, Option<i64>> = HashMap::new();
    let mut soroban_base_fee = None;

    let stellar_xdr::curr::GeneralizedTransactionSet::V1(tx_set) = gen;
    if let stellar_xdr::curr::TransactionPhase::V0(components) = &tx_set.phases[0] {
        for component in components.iter() {
            let TxSetComponent::TxsetCompTxsMaybeDiscountedFee(comp) = component;
            for tx in comp.txs.iter() {
                base_fee_map.insert(tx_hash(tx), comp.base_fee);
            }
        }
    }
    match &tx_set.phases[1] {
        stellar_xdr::curr::TransactionPhase::V1(parallel) => {
            soroban_base_fee = parallel.base_fee;
            for stage in parallel.execution_stages.iter() {
                for cluster in stage.iter() {
                    for tx in cluster.0.iter() {
                        base_fee_map.insert(tx_hash(tx), parallel.base_fee);
                    }
                }
            }
        }
        stellar_xdr::curr::TransactionPhase::V0(components) => {
            if let Some(component) = components.first() {
                let TxSetComponent::TxsetCompTxsMaybeDiscountedFee(comp) = component;
                soroban_base_fee = comp.base_fee;
            }
            for component in components.iter() {
                let TxSetComponent::TxsetCompTxsMaybeDiscountedFee(comp) = component;
                for tx in comp.txs.iter() {
                    base_fee_map.insert(tx_hash(tx), comp.base_fee);
                }
            }
        }
    }

    let mut total_fees = 0i64;
    let mut total_inclusion_fees = 0i64;
    let mut classic_ops = 0i64;
    let mut soroban_ops = 0i64;
    let mut insns = 0i64;
    let mut disk_read_bytes = 0i64;
    let mut write_bytes = 0i64;
    let mut disk_read_entries = 0i64;
    let mut write_entries = 0i64;
    let mut tx_size_bytes = 0i64;

    let network_id = NetworkId::testnet();
    let mut classic_non_dex_txs = 0usize;
    let mut classic_dex_txs = 0usize;
    let mut classic_non_dex_base_fee: Option<i64> = None;
    let mut classic_dex_base_fee: Option<i64> = None;
    for tx in txs {
        let frame = TransactionFrame::from_owned_with_network(tx.clone(), network_id);
        let hash = tx_hash(tx);
        let base_fee = base_fee_map.get(&hash).copied().flatten();

        if let Some(base) = base_fee {
            let adjusted = base.saturating_mul(frame.operation_count().max(1) as i64);
            let resource_fee = if frame.is_soroban() {
                frame.declared_soroban_resource_fee()
            } else {
                0
            };
            total_fees += resource_fee + frame.inclusion_fee().min(adjusted);
        } else {
            total_fees += frame.total_fee();
        }

        total_inclusion_fees += frame.inclusion_fee();

        if frame.is_soroban() {
            soroban_ops += frame.operation_count() as i64;
            let resources = frame.resources(false, 25);
            insns += resources.get_val(henyey_common::ResourceType::Instructions);
            disk_read_bytes += resources.get_val(henyey_common::ResourceType::DiskReadBytes);
            write_bytes += resources.get_val(henyey_common::ResourceType::WriteBytes);
            disk_read_entries +=
                resources.get_val(henyey_common::ResourceType::ReadLedgerEntries);
            write_entries +=
                resources.get_val(henyey_common::ResourceType::WriteLedgerEntries);
            tx_size_bytes += tx
                .to_xdr(stellar_xdr::curr::Limits::none())
                .map(|b| b.len() as i64)
                .unwrap_or(0);
        } else {
            classic_ops += frame.operation_count() as i64;
            if frame.has_dex_operations() {
                classic_dex_txs += 1;
                if let Some(fee) = base_fee {
                    classic_dex_base_fee = Some(match classic_dex_base_fee {
                        Some(curr) => curr.min(fee),
                        None => fee,
                    });
                }
            } else {
                classic_non_dex_txs += 1;
                if let Some(fee) = base_fee {
                    classic_non_dex_base_fee = Some(match classic_non_dex_base_fee {
                        Some(curr) => curr.min(fee),
                        None => fee,
                    });
                }
            }
        }
    }

    TxSetSummary {
        total_fees,
        total_inclusion_fees,
        classic_ops,
        classic_non_dex_txs,
        classic_non_dex_base_fee: classic_non_dex_base_fee.unwrap_or(100),
        classic_dex_txs,
        classic_dex_base_fee: classic_dex_base_fee.unwrap_or(0),
        soroban_ops,
        soroban_base_fee: soroban_base_fee.unwrap_or(100),
        insns,
        disk_read_bytes,
        write_bytes,
        disk_read_entries,
        write_entries,
        tx_size_bytes,
    }
}

#[test]
fn test_txset_summary_smoke() {
    let queue = TransactionQueue::with_defaults();
    let classic = make_classic_payment(200);
    let dex = make_classic_dex(400);
    let soroban = make_soroban_tx(1000, 200);

    queue.try_add(classic.clone());
    queue.try_add(dex.clone());
    queue.try_add(soroban.clone());

    let (tx_set, gen) = queue.build_generalized_tx_set(Hash256::ZERO, 100);
    let summary = summary_for_set(&tx_set.transactions, &gen);

    assert_eq!(summary.classic_ops, 2);
    assert_eq!(summary.soroban_ops, 1);
    assert_eq!(summary.classic_non_dex_txs, 1);
    assert_eq!(summary.classic_dex_txs, 1);
    assert_eq!(summary.classic_non_dex_base_fee, 100);
    assert_eq!(summary.classic_dex_base_fee, 100);
    assert_eq!(summary.soroban_base_fee, 100);
    assert_eq!(summary.total_inclusion_fees, 1400);
    assert_eq!(summary.total_fees, 500);
    assert_eq!(summary.insns, 10);
    assert_eq!(summary.disk_read_bytes, 5);
    assert_eq!(summary.write_bytes, 7);
    assert_eq!(summary.disk_read_entries, 3);
    assert_eq!(summary.write_entries, 1);
    assert!(summary.tx_size_bytes > 0);
}

#[test]
fn test_txset_summary_string() {
    let queue = TransactionQueue::with_defaults();
    let classic = make_classic_payment(200);
    let dex = make_classic_dex(400);
    let soroban = make_soroban_tx(1000, 200);

    queue.try_add(classic.clone());
    queue.try_add(dex.clone());
    queue.try_add(soroban.clone());

    let (tx_set, _gen) = queue.build_generalized_tx_set(Hash256::ZERO, 100);
    let summary = tx_set.summary();

    assert!(summary.contains("classic phase"));
    assert!(summary.contains("soroban phase"));
}

#[test]
fn test_txset_summary_classic() {
    let classic = make_classic_payment(200);
    let dex = make_classic_dex(400);
    let tx_set = henyey_herder::TransactionSet::new(Hash256::ZERO, vec![classic, dex]);

    let summary = tx_set.summary();
    assert_eq!(summary, "txs:2, ops:2, base_fee:200");
}
