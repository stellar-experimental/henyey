//! Validation parity tests — systematically verify Henyey rejects invalid
//! transactions the same way stellar-core does.
//!
//! Each test constructs a specific invalid transaction variant and asserts the
//! expected rejection. Tests are `#[ignore]`d until the corresponding fix lands.
//!
//! Issue tracker: <https://github.com/stellar-experimental/henyey/issues/1510>

use henyey_tx::validation::check_valid_pre_seq_num;
use henyey_tx::{validate_operation, TransactionFrame};
use stellar_xdr::curr::{
    AccountId, AlphaNum4, Asset, AssetCode4, ClawbackOp, ContractDataDurability, ContractId,
    ContractIdPreimage, CreateContractArgs, ExtendFootprintTtlOp, FeeBumpTransaction,
    FeeBumpTransactionEnvelope, FeeBumpTransactionExt, FeeBumpTransactionInnerTx, Hash,
    HostFunction, InvokeHostFunctionOp, LedgerFootprint, LedgerKey, LedgerKeyAccount,
    LedgerKeyContractCode, LedgerKeyContractData, Memo, MuxedAccount, MuxedAccountMed25519,
    Operation, OperationBody, Preconditions, PublicKey, RestoreFootprintOp, ScAddress, ScVal,
    SequenceNumber, SorobanResources, SorobanTransactionData, SorobanTransactionDataExt,
    Transaction, TransactionEnvelope, TransactionExt, TransactionV1Envelope, Uint256, VecM,
};

const PROTOCOL_VERSION: u32 = 25;

// ============================================================================
// Helpers
// ============================================================================

/// Build a Soroban TransactionFrame with a given operation and footprint.
fn make_soroban_frame(
    op_body: OperationBody,
    footprint: LedgerFootprint,
    resource_fee: i64,
) -> TransactionFrame {
    let source = MuxedAccount::Ed25519(Uint256([1u8; 32]));

    let op = Operation {
        source_account: None,
        body: op_body,
    };

    let tx = Transaction {
        source_account: source,
        fee: (resource_fee.max(0) as u32).saturating_add(1000),
        seq_num: SequenceNumber(1),
        cond: Preconditions::None,
        memo: Memo::None,
        operations: vec![op].try_into().unwrap(),
        ext: TransactionExt::V1(SorobanTransactionData {
            ext: SorobanTransactionDataExt::V0,
            resources: SorobanResources {
                footprint,
                instructions: 100,
                disk_read_bytes: 0,
                write_bytes: 0,
            },
            resource_fee,
        }),
    };

    let envelope = TransactionEnvelope::Tx(TransactionV1Envelope {
        tx,
        signatures: vec![].try_into().unwrap(),
    });

    TransactionFrame::from_owned(envelope)
}

/// Build a minimal InvokeHostFunction operation body (invoke contract no-op).
fn invoke_host_noop() -> OperationBody {
    OperationBody::InvokeHostFunction(InvokeHostFunctionOp {
        host_function: HostFunction::InvokeContract(stellar_xdr::curr::InvokeContractArgs {
            contract_address: ScAddress::Contract(ContractId(Hash([9u8; 32]))),
            function_name: stellar_xdr::curr::ScSymbol(
                stellar_xdr::curr::StringM::<32>::try_from("noop".to_string()).unwrap(),
            ),
            args: VecM::default(),
        }),
        auth: VecM::default(),
    })
}

fn contract_data_key(durability: ContractDataDurability) -> LedgerKey {
    LedgerKey::ContractData(LedgerKeyContractData {
        contract: ScAddress::Contract(ContractId(Hash([10u8; 32]))),
        key: ScVal::Bool(true),
        durability,
    })
}

fn contract_code_key() -> LedgerKey {
    LedgerKey::ContractCode(LedgerKeyContractCode {
        hash: Hash([11u8; 32]),
    })
}

fn account_key() -> LedgerKey {
    LedgerKey::Account(LedgerKeyAccount {
        account_id: AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([99u8; 32]))),
    })
}

fn empty_footprint() -> LedgerFootprint {
    LedgerFootprint {
        read_only: VecM::default(),
        read_write: VecM::default(),
    }
}

// ============================================================================
// #1490 — RestoreFootprint missing footprint structure validation
// Mirrors: stellar-core RestoreFootprintOpFrame::doCheckValidForSoroban()
// ============================================================================

/// readOnly footprint must be empty for RestoreFootprint.
/// stellar-core: RestoreFootprintOpFrame.cpp:429-437
#[test]
#[ignore] // Blocked on #1490
fn test_reject_restore_footprint_nonempty_readonly() {
    let footprint = LedgerFootprint {
        read_only: vec![contract_code_key()].try_into().unwrap(),
        read_write: vec![contract_data_key(ContractDataDurability::Persistent)]
            .try_into()
            .unwrap(),
    };

    let frame = make_soroban_frame(
        OperationBody::RestoreFootprint(RestoreFootprintOp {
            ext: stellar_xdr::curr::ExtensionPoint::V0,
        }),
        footprint,
        50,
    );

    let result = check_valid_pre_seq_num(&frame, PROTOCOL_VERSION, 0);
    assert!(
        result.is_err(),
        "RestoreFootprint with non-empty readOnly should be rejected"
    );
}

/// readWrite entries must be persistent ContractData/ContractCode only.
/// stellar-core: RestoreFootprintOpFrame.cpp:439-449
#[test]
#[ignore] // Blocked on #1490
fn test_reject_restore_footprint_non_persistent_readwrite() {
    let footprint = LedgerFootprint {
        read_only: VecM::default(),
        read_write: vec![contract_data_key(ContractDataDurability::Temporary)]
            .try_into()
            .unwrap(),
    };

    let frame = make_soroban_frame(
        OperationBody::RestoreFootprint(RestoreFootprintOp {
            ext: stellar_xdr::curr::ExtensionPoint::V0,
        }),
        footprint,
        50,
    );

    let result = check_valid_pre_seq_num(&frame, PROTOCOL_VERSION, 0);
    assert!(
        result.is_err(),
        "RestoreFootprint with temporary ContractData in readWrite should be rejected"
    );
}

// ============================================================================
// #1488 — ExtendFootprintTTL missing footprint structure validation
// Mirrors: stellar-core ExtendFootprintTTLOpFrame::doCheckValidForSoroban()
// ============================================================================

/// readWrite footprint must be empty for ExtendFootprintTTL.
/// stellar-core: ExtendFootprintTTLOpFrame.cpp:327-336
#[test]
#[ignore] // Blocked on #1488
fn test_reject_extend_footprint_ttl_nonempty_readwrite() {
    let footprint = LedgerFootprint {
        read_only: vec![contract_code_key()].try_into().unwrap(),
        read_write: vec![contract_data_key(ContractDataDurability::Persistent)]
            .try_into()
            .unwrap(),
    };

    let frame = make_soroban_frame(
        OperationBody::ExtendFootprintTtl(ExtendFootprintTtlOp {
            ext: stellar_xdr::curr::ExtensionPoint::V0,
            extend_to: 1000,
        }),
        footprint,
        50,
    );

    let result = check_valid_pre_seq_num(&frame, PROTOCOL_VERSION, 0);
    assert!(
        result.is_err(),
        "ExtendFootprintTTL with non-empty readWrite should be rejected"
    );
}

/// readOnly keys must be Soroban entries (ContractData/ContractCode).
/// stellar-core: ExtendFootprintTTLOpFrame.cpp:338-350
#[test]
#[ignore] // Blocked on #1488
fn test_reject_extend_footprint_ttl_non_soroban_key() {
    let footprint = LedgerFootprint {
        read_only: vec![account_key()].try_into().unwrap(),
        read_write: VecM::default(),
    };

    let frame = make_soroban_frame(
        OperationBody::ExtendFootprintTtl(ExtendFootprintTtlOp {
            ext: stellar_xdr::curr::ExtensionPoint::V0,
            extend_to: 1000,
        }),
        footprint,
        50,
    );

    let result = check_valid_pre_seq_num(&frame, PROTOCOL_VERSION, 0);
    assert!(
        result.is_err(),
        "ExtendFootprintTTL with non-Soroban key in readOnly should be rejected"
    );
}

// ============================================================================
// #1492 — Fee-bump inner Soroban resource_fee overflow
// Mirrors: stellar-core FeeBumpTransactionFrame resource fee validation
// ============================================================================

/// Regression test for #1492 — Fee-bump Soroban resource_fee overflow.
#[test]
fn test_reject_fee_bump_soroban_resource_fee_overflow() {
    let source = MuxedAccount::Ed25519(Uint256([1u8; 32]));

    // Inner Soroban tx with very large resource_fee
    let inner_op = Operation {
        source_account: None,
        body: invoke_host_noop(),
    };
    let inner_tx = Transaction {
        source_account: source.clone(),
        fee: 1000,
        seq_num: SequenceNumber(1),
        cond: Preconditions::None,
        memo: Memo::None,
        operations: vec![inner_op].try_into().unwrap(),
        ext: TransactionExt::V1(SorobanTransactionData {
            ext: SorobanTransactionDataExt::V0,
            resources: SorobanResources {
                footprint: empty_footprint(),
                instructions: 100,
                disk_read_bytes: 0,
                write_bytes: 0,
            },
            // resource_fee exceeds a reasonable max — makes inclusion fee negative
            resource_fee: i64::MAX / 2,
        }),
    };

    let inner_env = TransactionV1Envelope {
        tx: inner_tx,
        signatures: vec![].try_into().unwrap(),
    };

    let fee_bump_env = TransactionEnvelope::TxFeeBump(FeeBumpTransactionEnvelope {
        tx: FeeBumpTransaction {
            fee_source: source,
            fee: 2000, // outer fee < resource_fee → negative inclusion
            inner_tx: FeeBumpTransactionInnerTx::Tx(inner_env),
            ext: FeeBumpTransactionExt::V0,
        },
        signatures: vec![].try_into().unwrap(),
    });

    let frame = TransactionFrame::from_owned(fee_bump_env);
    let result = check_valid_pre_seq_num(&frame, PROTOCOL_VERSION, 0);
    assert!(
        result.is_err(),
        "Fee-bump with resource_fee overflow should be rejected"
    );
}

// ============================================================================
// #1495 — validate_clawback strips MuxedAccount discriminant
// stellar-core: ClawbackOpFrame.cpp:67 compares full MuxedAccount via
// `mClawback.from == toMuxedAccount(getSourceID())` — different discriminants
// (Ed25519 vs MuxedEd25519) are NOT equal.
// Henyey: strips to AccountId via muxed_to_account_id → false rejection.
// ============================================================================

/// Regression test for #1495 — MuxedEd25519 from is not self-clawback.
#[test]
fn test_reject_clawback_muxed_from_not_self_clawback() {
    let issuer_key = Uint256([6u8; 32]);
    let issuer = AccountId(PublicKey::PublicKeyTypeEd25519(issuer_key.clone()));
    let asset = Asset::CreditAlphanum4(AlphaNum4 {
        asset_code: AssetCode4([b'U', b'S', b'D', 0]),
        issuer: issuer.clone(),
    });

    // Test: source == from's underlying key but different MuxedAccount type.
    // stellar-core: toMuxedAccount(getSourceID()) returns Ed25519(issuer_key)
    // op.from = MuxedEd25519(issuer_key, 42) — different discriminant → NOT equal
    // So stellar-core allows this. Henyey strips both → rejects as self-clawback.
    let op_same_key = Operation {
        source_account: None,
        body: OperationBody::Clawback(ClawbackOp {
            asset,
            from: MuxedAccount::MuxedEd25519(MuxedAccountMed25519 {
                id: 42,
                ed25519: issuer_key,
            }),
            amount: 100,
        }),
    };

    let result = validate_operation(&op_same_key, PROTOCOL_VERSION, 0, Some(&issuer));
    assert!(
        result.is_ok(),
        "Clawback from MuxedEd25519(same_key, id) should NOT be rejected as self-clawback \
         when source is Ed25519(same_key) — stellar-core compares full MuxedAccount"
    );
}

// ============================================================================
// #1486 — CreateContract fromAsset invalid asset code
// Mirrors: stellar-core InvokeHostFunctionOpFrame::doCheckValidForSoroban()
// lines 1300-1310
// ============================================================================

/// Regression test for #1486 — CreateContract fromAsset with invalid asset code.
#[test]
fn test_reject_create_contract_from_invalid_asset() {
    use stellar_xdr::curr::ContractExecutable;

    // All-zero asset code is invalid per isAssetValid
    let invalid_asset = Asset::CreditAlphanum4(AlphaNum4 {
        asset_code: AssetCode4([0u8; 4]),
        issuer: AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([1u8; 32]))),
    });

    let op_body = OperationBody::InvokeHostFunction(InvokeHostFunctionOp {
        host_function: HostFunction::CreateContract(CreateContractArgs {
            contract_id_preimage: ContractIdPreimage::Asset(invalid_asset),
            executable: ContractExecutable::StellarAsset,
        }),
        auth: VecM::default(),
    });

    let frame = make_soroban_frame(op_body, empty_footprint(), 50);
    let result = check_valid_pre_seq_num(&frame, PROTOCOL_VERSION, 0);
    assert!(
        result.is_err(),
        "CreateContract FROM_ASSET with invalid asset code should be rejected"
    );
}

// ============================================================================
// #1481 — oversized UploadContractWasm skips pre-host size gate
// Mirrors: stellar-core InvokeHostFunctionOpFrame::doCheckValidForSoroban()
// lines 1290-1299
// ============================================================================

/// UploadContractWasm exceeding max contract size should be rejected before host.
/// NOTE: The fix needs to thread SorobanNetworkConfig into the validation path.
/// This test documents the expected post-fix behavior.
#[test]
#[ignore] // Blocked on #1481
fn test_reject_upload_wasm_oversized() {
    // Typical maxContractSizeBytes is 256KB
    let oversized_wasm = vec![0u8; 512 * 1024];

    let op_body = OperationBody::InvokeHostFunction(InvokeHostFunctionOp {
        host_function: HostFunction::UploadContractWasm(oversized_wasm.try_into().unwrap()),
        auth: VecM::default(),
    });

    let frame = make_soroban_frame(op_body, empty_footprint(), 50);

    // This currently passes because check_valid_pre_seq_num has no network config.
    // After #1481 fix, it should reject oversized wasm.
    let result = check_valid_pre_seq_num(&frame, PROTOCOL_VERSION, 0);
    assert!(
        result.is_err(),
        "UploadContractWasm exceeding max contract size should be rejected"
    );
}
