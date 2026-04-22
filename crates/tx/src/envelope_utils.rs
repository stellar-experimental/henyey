//! Canonical envelope-level helpers for transaction classification and resource computation.
//!
//! These functions operate directly on `TransactionEnvelope` without requiring
//! construction of a `TransactionFrame`. They serve as the single source of truth
//! for envelope inspection logic — `TransactionFrame` methods delegate to them.
//!
//! # Motivation
//!
//! The surge pricing, tx_queue, and tx_queue_limiter modules only need envelope
//! inspection (classification, resource computation, lane routing). By providing
//! these as free functions on `&TransactionEnvelope`, we eliminate the coupling
//! between `SurgePricingLaneConfig` / `SurgePricingPriorityQueue` and
//! `TransactionFrame`.

use henyey_common::Resource;
use stellar_xdr::curr::{
    FeeBumpTransactionInnerTx, Operation, OperationBody, SorobanResources, SorobanTransactionData,
    TransactionEnvelope, TransactionExt, VecM,
};

/// Check if a transaction envelope is a Soroban transaction.
///
/// A transaction is Soroban if any of its operations is `InvokeHostFunction`,
/// `ExtendFootprintTtl`, or `RestoreFootprint`.
pub fn is_soroban_envelope(env: &TransactionEnvelope) -> bool {
    envelope_operations(env).iter().any(|op| {
        matches!(
            op.body,
            OperationBody::InvokeHostFunction(_)
                | OperationBody::ExtendFootprintTtl(_)
                | OperationBody::RestoreFootprint(_)
        )
    })
}

/// Check if a transaction envelope contains DEX-related operations.
pub fn has_dex_operations_envelope(env: &TransactionEnvelope) -> bool {
    envelope_operations(env).iter().any(|op| {
        matches!(
            op.body,
            OperationBody::ManageSellOffer(_)
                | OperationBody::ManageBuyOffer(_)
                | OperationBody::CreatePassiveSellOffer(_)
                | OperationBody::PathPaymentStrictSend(_)
                | OperationBody::PathPaymentStrictReceive(_)
        )
    })
}

/// Check if a transaction envelope is a fee-bump transaction.
pub fn is_fee_bump_envelope(env: &TransactionEnvelope) -> bool {
    matches!(env, TransactionEnvelope::TxFeeBump(_))
}

/// Extract operations from a transaction envelope.
///
/// For fee-bump transactions, returns the inner transaction's operations.
pub fn envelope_operations(env: &TransactionEnvelope) -> &[Operation] {
    match env {
        TransactionEnvelope::TxV0(e) => &e.tx.operations,
        TransactionEnvelope::Tx(e) => &e.tx.operations,
        TransactionEnvelope::TxFeeBump(e) => match &e.tx.inner_tx {
            FeeBumpTransactionInnerTx::Tx(inner) => &inner.tx.operations,
        },
    }
}

/// Extract `SorobanTransactionData` from a transaction envelope.
///
/// Returns `None` for V0 envelopes and envelopes without V1 ext data.
pub fn envelope_soroban_data(env: &TransactionEnvelope) -> Option<&SorobanTransactionData> {
    let ext = match env {
        TransactionEnvelope::TxV0(_) => return None,
        TransactionEnvelope::Tx(e) => &e.tx.ext,
        TransactionEnvelope::TxFeeBump(e) => match &e.tx.inner_tx {
            FeeBumpTransactionInnerTx::Tx(inner) => &inner.tx.ext,
        },
    };
    match ext {
        TransactionExt::V1(data) => Some(data),
        _ => None,
    }
}

/// Extract `SorobanResources` from a transaction envelope.
///
/// Convenience wrapper over [`envelope_soroban_data`] for callers that only
/// need the resources field.
pub fn envelope_soroban_resources(env: &TransactionEnvelope) -> Option<&SorobanResources> {
    envelope_soroban_data(env).map(|d| &d.resources)
}

/// Returns the operation count for resource/fee accounting.
///
/// For fee-bump transactions, returns `inner_ops + 1`, matching
/// stellar-core's `FeeBumpTransactionFrame::getNumOperations()`.
/// For regular transactions, returns `operations().len()`.
pub fn envelope_operation_count(env: &TransactionEnvelope) -> usize {
    let ops = envelope_operations(env).len();
    if is_fee_bump_envelope(env) {
        ops + 1
    } else {
        ops
    }
}

/// Return the inner transaction envelope size for resource accounting.
///
/// For fee-bump transactions, returns the XDR-encoded size of the inner
/// V1 envelope (not the outer fee-bump), matching stellar-core's delegation
/// pattern where `FeeBumpTransactionFrame::getResources()` inherits the
/// inner tx's size. For regular transactions, returns the full envelope size.
pub fn envelope_tx_size_bytes(env: &TransactionEnvelope) -> u32 {
    match env {
        TransactionEnvelope::TxFeeBump(e) => {
            let inner = match &e.tx.inner_tx {
                FeeBumpTransactionInnerTx::Tx(inner) => inner,
            };
            henyey_common::xdr_encoded_len_u32(&TransactionEnvelope::Tx(inner.clone()))
        }
        _ => henyey_common::xdr_encoded_len_u32(env),
    }
}

/// Return the resource footprint used for surge pricing and limits.
///
/// Mirrors stellar-core's `TransactionFrame::getResources()` and
/// `FeeBumpTransactionFrame::getResources()`. Branches on operation
/// classification (`is_soroban_envelope`), not on `TransactionExt::V1` presence.
pub fn resources_from_envelope(
    env: &TransactionEnvelope,
    use_byte_limit_in_classic: bool,
    ledger_version: u32,
) -> Resource {
    let tx_size = envelope_tx_size_bytes(env) as i64;

    if is_soroban_envelope(env) {
        let data = envelope_soroban_data(env);
        let fallback_resources = SorobanResources {
            footprint: stellar_xdr::curr::LedgerFootprint {
                read_only: VecM::default(),
                read_write: VecM::default(),
            },
            instructions: 0,
            disk_read_bytes: 0,
            write_bytes: 0,
        };
        let resources = data.map(|d| &d.resources).unwrap_or(&fallback_resources);

        // stellar-core: TransactionFrame::getResources() hardcodes opCount = 1,
        // FeeBumpTransactionFrame::getResources() overrides with getNumOperations()
        let op_count = if is_fee_bump_envelope(env) {
            envelope_operation_count(env) as i64
        } else {
            1i64
        };

        let is_restore = envelope_operations(env)
            .iter()
            .any(|op| matches!(op.body, OperationBody::RestoreFootprint(_)));

        let disk_read_entries = crate::frame::soroban_disk_read_entries(
            resources,
            data.map(|d| &d.ext),
            is_restore,
            ledger_version,
        );
        let write_entries = resources.footprint.read_write.len() as i64;

        return Resource::new(vec![
            op_count,
            resources.instructions as i64,
            tx_size,
            resources.disk_read_bytes as i64,
            resources.write_bytes as i64,
            disk_read_entries,
            write_entries,
        ]);
    }

    if use_byte_limit_in_classic {
        Resource::new(vec![envelope_operation_count(env) as i64, tx_size])
    } else {
        Resource::new(vec![envelope_operation_count(env) as i64])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use stellar_xdr::curr::*;

    fn make_classic_v1(ops: Vec<Operation>) -> TransactionEnvelope {
        TransactionEnvelope::Tx(TransactionV1Envelope {
            tx: Transaction {
                source_account: MuxedAccount::Ed25519(Uint256([0u8; 32])),
                fee: 100,
                seq_num: SequenceNumber(1),
                cond: Preconditions::None,
                memo: Memo::None,
                operations: ops.try_into().unwrap(),
                ext: TransactionExt::V0,
            },
            signatures: vec![].try_into().unwrap(),
        })
    }

    fn payment_op() -> Operation {
        Operation {
            source_account: None,
            body: OperationBody::Payment(PaymentOp {
                destination: MuxedAccount::Ed25519(Uint256([1u8; 32])),
                asset: Asset::Native,
                amount: 1000,
            }),
        }
    }

    fn manage_sell_offer_op() -> Operation {
        Operation {
            source_account: None,
            body: OperationBody::ManageSellOffer(ManageSellOfferOp {
                selling: Asset::Native,
                buying: Asset::Native,
                amount: 100,
                price: Price { n: 1, d: 1 },
                offer_id: 0,
            }),
        }
    }

    fn invoke_host_fn_op() -> Operation {
        Operation {
            source_account: None,
            body: OperationBody::InvokeHostFunction(InvokeHostFunctionOp {
                host_function: HostFunction::InvokeContract(InvokeContractArgs {
                    contract_address: ScAddress::Contract(ContractId(Hash([0u8; 32]))),
                    function_name: ScSymbol("test".try_into().unwrap()),
                    args: VecM::default(),
                }),
                auth: VecM::default(),
            }),
        }
    }

    fn restore_footprint_op() -> Operation {
        Operation {
            source_account: None,
            body: OperationBody::RestoreFootprint(RestoreFootprintOp {
                ext: ExtensionPoint::V0,
            }),
        }
    }

    fn wrap_fee_bump(inner: TransactionEnvelope) -> TransactionEnvelope {
        let inner_env = match inner {
            TransactionEnvelope::Tx(env) => env,
            _ => panic!("expected V1 for fee bump wrapping"),
        };
        TransactionEnvelope::TxFeeBump(FeeBumpTransactionEnvelope {
            tx: FeeBumpTransaction {
                fee_source: MuxedAccount::Ed25519(Uint256([2u8; 32])),
                fee: 200,
                inner_tx: FeeBumpTransactionInnerTx::Tx(inner_env),
                ext: FeeBumpTransactionExt::V0,
            },
            signatures: vec![].try_into().unwrap(),
        })
    }

    fn make_soroban_v1(op: Operation) -> TransactionEnvelope {
        TransactionEnvelope::Tx(TransactionV1Envelope {
            tx: Transaction {
                source_account: MuxedAccount::Ed25519(Uint256([0u8; 32])),
                fee: 100,
                seq_num: SequenceNumber(1),
                cond: Preconditions::None,
                memo: Memo::None,
                operations: vec![op].try_into().unwrap(),
                ext: TransactionExt::V1(SorobanTransactionData {
                    ext: SorobanTransactionDataExt::V0,
                    resources: SorobanResources {
                        footprint: LedgerFootprint {
                            read_only: vec![LedgerKey::Account(LedgerKeyAccount {
                                account_id: AccountId(PublicKey::PublicKeyTypeEd25519(Uint256(
                                    [0u8; 32],
                                ))),
                            })]
                            .try_into()
                            .unwrap(),
                            read_write: vec![LedgerKey::ContractData(LedgerKeyContractData {
                                contract: ScAddress::Contract(ContractId(Hash([0u8; 32]))),
                                key: ScVal::Bool(true),
                                durability: ContractDataDurability::Persistent,
                            })]
                            .try_into()
                            .unwrap(),
                        },
                        instructions: 5000,
                        disk_read_bytes: 1024,
                        write_bytes: 512,
                    },
                    resource_fee: 50,
                }),
            },
            signatures: vec![].try_into().unwrap(),
        })
    }

    // --- Classification tests ---

    #[test]
    fn test_is_soroban_envelope_classic() {
        let env = make_classic_v1(vec![payment_op()]);
        assert!(!is_soroban_envelope(&env));
    }

    #[test]
    fn test_is_soroban_envelope_soroban() {
        let env = make_soroban_v1(invoke_host_fn_op());
        assert!(is_soroban_envelope(&env));
    }

    #[test]
    fn test_is_soroban_envelope_restore() {
        let env = make_soroban_v1(restore_footprint_op());
        assert!(is_soroban_envelope(&env));
    }

    #[test]
    fn test_has_dex_operations_envelope() {
        let no_dex = make_classic_v1(vec![payment_op()]);
        assert!(!has_dex_operations_envelope(&no_dex));

        let with_dex = make_classic_v1(vec![payment_op(), manage_sell_offer_op()]);
        assert!(has_dex_operations_envelope(&with_dex));
    }

    #[test]
    fn test_is_fee_bump_envelope() {
        let regular = make_classic_v1(vec![payment_op()]);
        assert!(!is_fee_bump_envelope(&regular));

        let bumped = wrap_fee_bump(regular);
        assert!(is_fee_bump_envelope(&bumped));
    }

    // --- Operation count tests ---

    #[test]
    fn test_envelope_operation_count_regular() {
        let env = make_classic_v1(vec![payment_op(), payment_op(), payment_op()]);
        assert_eq!(envelope_operation_count(&env), 3);
    }

    #[test]
    fn test_envelope_operation_count_fee_bump() {
        let inner = make_classic_v1(vec![payment_op(), payment_op(), payment_op()]);
        let bumped = wrap_fee_bump(inner);
        // inner ops (3) + 1 for fee-bump wrapper
        assert_eq!(envelope_operation_count(&bumped), 4);
    }

    #[test]
    fn test_envelope_operation_count_fee_bump_single_op() {
        let inner = make_classic_v1(vec![payment_op()]);
        let bumped = wrap_fee_bump(inner);
        assert_eq!(envelope_operation_count(&bumped), 2);
    }

    // --- Soroban data extraction tests ---

    #[test]
    fn test_envelope_soroban_data_classic() {
        let env = make_classic_v1(vec![payment_op()]);
        assert!(envelope_soroban_data(&env).is_none());
    }

    #[test]
    fn test_envelope_soroban_data_soroban() {
        let env = make_soroban_v1(invoke_host_fn_op());
        let data = envelope_soroban_data(&env).unwrap();
        assert_eq!(data.resources.instructions, 5000);
    }

    #[test]
    fn test_envelope_soroban_resources_present() {
        let env = make_soroban_v1(invoke_host_fn_op());
        let resources = envelope_soroban_resources(&env).unwrap();
        assert_eq!(resources.instructions, 5000);
        assert_eq!(resources.disk_read_bytes, 1024);
    }

    #[test]
    fn test_envelope_soroban_resources_absent() {
        let env = make_classic_v1(vec![payment_op()]);
        assert!(envelope_soroban_resources(&env).is_none());
    }

    // --- TX size tests ---

    #[test]
    fn test_envelope_tx_size_bytes_regular() {
        let env = make_classic_v1(vec![payment_op()]);
        let size = envelope_tx_size_bytes(&env);
        assert!(size > 0);
        // Should match full envelope XDR size
        assert_eq!(size, henyey_common::xdr_encoded_len_u32(&env));
    }

    #[test]
    fn test_envelope_tx_size_bytes_fee_bump_uses_inner() {
        let inner = make_classic_v1(vec![payment_op()]);
        let inner_size = henyey_common::xdr_encoded_len_u32(&inner);
        let bumped = wrap_fee_bump(inner);
        let bumped_full_size = henyey_common::xdr_encoded_len_u32(&bumped);
        let resource_size = envelope_tx_size_bytes(&bumped);

        // Resource size should equal inner size, not outer
        assert_eq!(resource_size, inner_size);
        assert!(resource_size < bumped_full_size);
    }

    // --- Resource computation tests ---

    #[test]
    fn test_resources_classic_ops_only() {
        let env = make_classic_v1(vec![payment_op(), payment_op()]);
        let res = resources_from_envelope(&env, false, 25);
        assert_eq!(res.size(), 1); // [ops]
        assert_eq!(res.get_val(henyey_common::ResourceType::Operations), 2);
    }

    #[test]
    fn test_resources_classic_with_bytes() {
        let env = make_classic_v1(vec![payment_op(), payment_op()]);
        let res = resources_from_envelope(&env, true, 25);
        assert_eq!(res.size(), 2); // [ops, bytes]
        assert_eq!(res.get_val(henyey_common::ResourceType::Operations), 2);
        assert!(res.get_val(henyey_common::ResourceType::Instructions) > 0); // tx bytes in 2nd slot
    }

    #[test]
    fn test_resources_classic_fee_bump() {
        let inner = make_classic_v1(vec![payment_op(), payment_op()]);
        let bumped = wrap_fee_bump(inner);
        let res = resources_from_envelope(&bumped, false, 25);
        // ops = inner ops (2) + 1 = 3
        assert_eq!(res.get_val(henyey_common::ResourceType::Operations), 3);
    }

    #[test]
    fn test_resources_soroban() {
        let env = make_soroban_v1(invoke_host_fn_op());
        let res = resources_from_envelope(&env, false, 25);
        assert_eq!(res.size(), 7); // [ops, instructions, tx_size, disk_read, write, read_entries, write_entries]
                                   // Non-fee-bump soroban: op count = 1
        assert_eq!(res.get_val(henyey_common::ResourceType::Operations), 1);
    }

    #[test]
    fn test_resources_soroban_fee_bump() {
        let inner = make_soroban_v1(invoke_host_fn_op());
        let bumped = wrap_fee_bump(inner);
        let res = resources_from_envelope(&bumped, false, 25);
        // Fee-bump soroban: op count = resource_operation_count = inner ops (1) + 1 = 2
        assert_eq!(res.get_val(henyey_common::ResourceType::Operations), 2);
    }

    #[test]
    fn test_resources_soroban_absent_data_fallback() {
        // Classic envelope (no SorobanTransactionData) but with a soroban op
        // This exercises the fallback path
        let env = TransactionEnvelope::Tx(TransactionV1Envelope {
            tx: Transaction {
                source_account: MuxedAccount::Ed25519(Uint256([0u8; 32])),
                fee: 100,
                seq_num: SequenceNumber(1),
                cond: Preconditions::None,
                memo: Memo::None,
                operations: vec![invoke_host_fn_op()].try_into().unwrap(),
                ext: TransactionExt::V0, // No V1 ext data
            },
            signatures: vec![].try_into().unwrap(),
        });
        let res = resources_from_envelope(&env, false, 25);
        assert_eq!(res.size(), 7);
        // Fallback: all resource values should be zero except op_count (1) and tx_size
        assert_eq!(res.get_val(henyey_common::ResourceType::Operations), 1);
    }

    #[test]
    fn test_resources_restore_footprint_disk_read_entries() {
        let env = make_soroban_v1(restore_footprint_op());
        let res = resources_from_envelope(&env, false, 25);
        // RestoreFootprint: disk_read_entries = read_write.len()
        // Our test envelope has 1 read_write entry
        assert_eq!(
            res.get_val(henyey_common::ResourceType::ReadLedgerEntries),
            1
        );
    }
}
