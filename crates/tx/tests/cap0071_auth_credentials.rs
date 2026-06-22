//! CAP-0071 Soroban authorization delegation — protocol-boundary tests (#3527).
//!
//! Protocol 27 (CAP-0071) adds two new `SorobanCredentials` variants —
//! `AddressV2` (discriminant 2) and `AddressWithDelegates` (discriminant 3,
//! carrying a recursive `SorobanDelegateSignature` tree) — plus a new
//! `ENVELOPE_TYPE_SOROBAN_AUTHORIZATION_WITH_ADDRESS` signature preimage.
//!
//! henyey-tx's ONLY obligation is the protocol boundary: it passes `op.auth`
//! opaquely to the protocol-routed Soroban host (`encode_invocation_inputs`),
//! which decodes and verifies it with its own bundled XDR. ALL CAP-0071
//! verification (credential routing, recursive delegate signatures, the new
//! preimage + metered hashing) lives in `soroban-env-host-p27` — the same
//! crate stellar-core v27 uses. henyey-tx reimplements none of it, because
//! that would fork consensus auth.
//!
//! Pre-V27 rejection is *host-routing*, not a validation-time gate: stellar-core
//! v27's `InvokeHostFunctionOpFrame::doCheckValidForSoroban` performs NO
//! credential-type validation, and routes by ledger protocol version to the
//! matching host. A pre-V27 ledger runs the P26 host (stellar-xdr 26.0.0), which
//! cannot decode discriminants 2/3 → host error → `INVOKE_HOST_FUNCTION_TRAPPED`
//! (a failed invocation), NOT a synthesized `txMALFORMED`. henyey mirrors this
//! exactly via `PersistentModuleCache::new_for_protocol` (V27-first routing).
//!
//! The byte-level decode behavior of each host (P26 rejects, P27 accepts) is
//! pinned by the unit tests in `crates/tx/src/soroban/host.rs`, which have the
//! host crates in scope. These integration tests pin the henyey public-API
//! boundary: version routing and that fee-bump inner-tx auth ops are reached.

use henyey_tx::soroban::PersistentModuleCache;
use henyey_tx::TransactionFrame;
use stellar_xdr::{
    AccountId, ContractId, FeeBumpTransaction, FeeBumpTransactionEnvelope, FeeBumpTransactionExt,
    FeeBumpTransactionInnerTx, Hash, HostFunction, InvokeContractArgs, InvokeHostFunctionOp, Memo,
    MuxedAccount, Operation, OperationBody, Preconditions, PublicKey, ScAddress, ScSymbol, ScVal,
    SequenceNumber, SorobanAddressCredentials, SorobanAddressCredentialsWithDelegates,
    SorobanAuthorizationEntry, SorobanAuthorizedFunction, SorobanAuthorizedInvocation,
    SorobanCredentials, SorobanCredentialsType, SorobanDelegateSignature, Transaction,
    TransactionEnvelope, TransactionExt, TransactionV1Envelope, Uint256, VecM,
};

// ============================================================================
// Helpers
// ============================================================================

fn account_id(b: u8) -> AccountId {
    AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([b; 32])))
}

fn contract_addr(b: u8) -> ScAddress {
    ScAddress::Contract(ContractId(Hash([b; 32])))
}

fn root_invocation() -> SorobanAuthorizedInvocation {
    SorobanAuthorizedInvocation {
        function: SorobanAuthorizedFunction::ContractFn(InvokeContractArgs {
            contract_address: contract_addr(8),
            function_name: ScSymbol("f".try_into().unwrap()),
            args: VecM::default(),
        }),
        sub_invocations: VecM::default(),
    }
}

fn address_v2_entry() -> SorobanAuthorizationEntry {
    SorobanAuthorizationEntry {
        credentials: SorobanCredentials::AddressV2(SorobanAddressCredentials {
            address: contract_addr(7),
            nonce: 42,
            signature_expiration_ledger: 1000,
            signature: ScVal::Void,
        }),
        root_invocation: root_invocation(),
    }
}

/// `AddressWithDelegates` with a recursive `SorobanDelegateSignature` tree
/// (a nested delegate under a top-level delegate).
fn address_with_delegates_entry() -> SorobanAuthorizationEntry {
    let nested = SorobanDelegateSignature {
        address: ScAddress::Account(account_id(3)),
        signature: ScVal::Void,
        nested_delegates: VecM::default(),
    };
    let top = SorobanDelegateSignature {
        address: ScAddress::Account(account_id(4)),
        signature: ScVal::Void,
        nested_delegates: vec![nested].try_into().unwrap(),
    };
    SorobanAuthorizationEntry {
        credentials: SorobanCredentials::AddressWithDelegates(
            SorobanAddressCredentialsWithDelegates {
                address_credentials: SorobanAddressCredentials {
                    address: contract_addr(7),
                    nonce: 42,
                    signature_expiration_ledger: 1000,
                    signature: ScVal::Void,
                },
                delegates: vec![top].try_into().unwrap(),
            },
        ),
        root_invocation: root_invocation(),
    }
}

fn invoke_op_with_auth(auth: Vec<SorobanAuthorizationEntry>) -> Operation {
    Operation {
        source_account: None,
        body: OperationBody::InvokeHostFunction(InvokeHostFunctionOp {
            host_function: HostFunction::InvokeContract(InvokeContractArgs {
                contract_address: contract_addr(9),
                function_name: ScSymbol("noop".try_into().unwrap()),
                args: VecM::default(),
            }),
            auth: auth.try_into().unwrap(),
        }),
    }
}

fn inner_tx_envelope(op: Operation) -> TransactionEnvelope {
    let tx = Transaction {
        source_account: MuxedAccount::Ed25519(Uint256([1u8; 32])),
        fee: 1000,
        seq_num: SequenceNumber(1),
        cond: Preconditions::None,
        memo: Memo::None,
        operations: vec![op].try_into().unwrap(),
        ext: TransactionExt::V0,
    };
    TransactionEnvelope::Tx(TransactionV1Envelope {
        tx,
        signatures: vec![].try_into().unwrap(),
    })
}

// ============================================================================
// XDR type surface (confirms the workspace pins stellar-xdr 27.0.0 with CAP-0071)
// ============================================================================

/// The CAP-0071 credential discriminants are exactly 2 and 3, matching the XDR
/// `SOROBAN_CREDENTIALS_ADDRESS_V2 = 2` / `SOROBAN_CREDENTIALS_ADDRESS_WITH_DELEGATES = 3`.
#[test]
fn test_cap0071_credential_discriminants() {
    assert_eq!(SorobanCredentialsType::AddressV2 as i32, 2);
    assert_eq!(SorobanCredentialsType::AddressWithDelegates as i32, 3);
    assert_eq!(
        SorobanCredentials::AddressV2(SorobanAddressCredentials {
            address: contract_addr(7),
            nonce: 0,
            signature_expiration_ledger: 0,
            signature: ScVal::Void,
        })
        .discriminant(),
        SorobanCredentialsType::AddressV2
    );
    assert_eq!(
        address_with_delegates_entry().credentials.discriminant(),
        SorobanCredentialsType::AddressWithDelegates
    );
}

// ============================================================================
// Version routing — the pre-V27 reject mechanism is host-routing
// ============================================================================

/// V_27 routes to the P27 host (which verifies CAP-0071 credentials natively);
/// pre-V27 routes to the P26 host (which cannot decode them). This is the
/// version gate — no explicit credential validation gate exists, matching
/// stellar-core v27 (`doCheckValidForSoroban` does no credential validation).
#[test]
fn test_version_routing_gates_cap0071_credentials() {
    let cache27 =
        PersistentModuleCache::new_for_protocol(27).expect("P27 module cache should be available");
    assert!(
        cache27.as_p27().is_some(),
        "protocol 27 must route to the P27 host (native CAP-0071 verification)"
    );
    assert!(
        cache27.as_p26().is_none(),
        "protocol 27 must NOT route to the P26 host"
    );

    let cache26 =
        PersistentModuleCache::new_for_protocol(26).expect("P26 module cache should be available");
    assert!(
        cache26.as_p26().is_some(),
        "pre-V27 protocol 26 must route to the P26 host (which rejects discriminants 2/3)"
    );
    assert!(
        cache26.as_p27().is_none(),
        "protocol 26 must NOT route to the P27 host"
    );
}

// ============================================================================
// Fee-bump inner-tx ops are reached
// ============================================================================

/// `invoke_host_function_ops()` unwraps the fee-bump inner transaction, so the
/// new credential auth entries on an inner Soroban op are reached by the host
/// boundary even when wrapped in a fee-bump envelope (pre-V27, that inner op
/// then fails the host decode → trapped invocation).
#[test]
fn test_fee_bump_inner_invoke_host_function_ops_reached() {
    let inner = inner_tx_envelope(invoke_op_with_auth(vec![address_with_delegates_entry()]));
    let fee_bump_env = TransactionEnvelope::TxFeeBump(FeeBumpTransactionEnvelope {
        tx: FeeBumpTransaction {
            fee_source: MuxedAccount::Ed25519(Uint256([2u8; 32])),
            fee: 2000,
            inner_tx: FeeBumpTransactionInnerTx::Tx(match inner {
                TransactionEnvelope::Tx(e) => e,
                _ => unreachable!(),
            }),
            ext: FeeBumpTransactionExt::V0,
        },
        signatures: vec![].try_into().unwrap(),
    });
    let frame = TransactionFrame::from_owned(fee_bump_env);

    let ihf_ops = frame.invoke_host_function_ops();
    assert_eq!(
        ihf_ops.len(),
        1,
        "fee-bump inner InvokeHostFunction op must be reached via invoke_host_function_ops()"
    );
    let auth = &ihf_ops[0].auth;
    assert_eq!(auth.len(), 1, "the inner op's auth entry must be present");
    assert_eq!(
        auth[0].credentials.discriminant(),
        SorobanCredentialsType::AddressWithDelegates,
        "the AddressWithDelegates credential must survive fee-bump unwrapping intact"
    );
}

/// Sanity: a plain (non-fee-bump) Soroban tx carrying an `AddressV2` auth entry
/// is reached the same way, with the credential intact for the host boundary.
#[test]
fn test_plain_invoke_host_function_ops_reached() {
    let env = inner_tx_envelope(invoke_op_with_auth(vec![address_v2_entry()]));
    let frame = TransactionFrame::from_owned(env);
    let ihf_ops = frame.invoke_host_function_ops();
    assert_eq!(ihf_ops.len(), 1);
    assert_eq!(
        ihf_ops[0].auth[0].credentials.discriminant(),
        SorobanCredentialsType::AddressV2
    );
}
