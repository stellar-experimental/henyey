use henyey_common::{NetworkId, LIQUIDITY_POOL_FEE_V18};
use henyey_crypto::{sign_hash, SecretKey};
use henyey_ledger::execution::build_tx_result_pair;
use henyey_ledger::execution::{
    ExecutionFailure, TransactionExecutionRequest, TransactionExecutor,
};
use henyey_ledger::LedgerAdvanceParams;
use henyey_ledger::{LedgerSnapshot, SnapshotBuilder, SnapshotHandle};
use henyey_tx::{
    soroban::{PersistentModuleCache, SorobanConfig},
    ClassicEventConfig, OpEventManager,
};
use stellar_xdr::curr::{
    AccountEntry, AccountEntryExt, AccountEntryExtensionV1, AccountEntryExtensionV1Ext,
    AccountEntryExtensionV2, AccountEntryExtensionV2Ext, AccountEntryExtensionV3, AccountId,
    AllowTrustOp, AlphaNum4, Asset, AssetCode, AssetCode4, BytesM, ChangeTrustAsset, ChangeTrustOp,
    ChangeTrustResult, ClaimAtom, ClaimClaimableBalanceOp, ClaimLiquidityAtom, ClaimOfferAtom,
    ClaimPredicate, ClaimableBalanceEntry, ClaimableBalanceEntryExt,
    ClaimableBalanceEntryExtensionV1, ClaimableBalanceEntryExtensionV1Ext, ClaimableBalanceFlags,
    ClaimableBalanceId, Claimant, ClaimantV0, ClawbackClaimableBalanceOp, ClawbackOp,
    ContractCodeEntry, ContractCodeEntryExt, ContractEventBody, ContractId, ContractIdPreimage,
    CreateAccountOp, CreateAccountResult, CreateClaimableBalanceOp, CreateClaimableBalanceResult,
    DecoratedSignature, Duration, ExtendFootprintTtlOp, ExtensionPoint, FeeBumpTransaction,
    FeeBumpTransactionEnvelope, FeeBumpTransactionInnerTx, Hash, HashIdPreimage,
    HashIdPreimageContractId, HostFunction, InnerTransactionResultPair, Int128Parts,
    InvokeContractArgs, InvokeHostFunctionOp, LedgerBounds, LedgerEntry, LedgerEntryChange,
    LedgerEntryChanges, LedgerEntryData, LedgerEntryExt, LedgerFootprint, LedgerKey,
    LedgerKeyAccount, LedgerKeyClaimableBalance, LedgerKeyContractCode, LedgerKeyLiquidityPool,
    LedgerKeyOffer, LedgerKeyTrustLine, LedgerKeyTtl, Liabilities,
    LiquidityPoolConstantProductParameters, LiquidityPoolDepositOp, LiquidityPoolEntry,
    LiquidityPoolEntryBody, LiquidityPoolEntryConstantProduct, LiquidityPoolWithdrawOp,
    ManageSellOfferOp, ManageSellOfferResult, Memo, MuxedAccount, MuxedAccountMed25519, OfferEntry,
    OfferEntryExt, Operation, OperationBody, OperationResult, OperationResultTr,
    PathPaymentStrictReceiveOp, PathPaymentStrictSendOp, PathPaymentStrictSendResult,
    PathPaymentStrictSendResultSuccess, PoolId, Preconditions, PreconditionsV2, Price, PublicKey,
    ScAddress, ScString, ScSymbol, ScVal, SequenceNumber, SetOptionsOp, SetOptionsResult,
    SetTrustLineFlagsOp, Signature as XdrSignature, SignatureHint, Signer, SignerKey,
    SorobanResources, SorobanTransactionData, SorobanTransactionDataExt, SponsorshipDescriptor,
    String32, StringM, Thresholds, TimeBounds, TimePoint, Transaction, TransactionEnvelope,
    TransactionEventStage, TransactionExt, TransactionMeta, TransactionResultCode,
    TransactionResultResult, TransactionV1Envelope, TrustLineAsset, TrustLineEntry,
    TrustLineEntryExt, TrustLineFlags, TtlEntry, Uint256, VecM, WriteXdr,
};

fn create_account_entry_with_last_modified(
    account_id: AccountId,
    seq_num: i64,
    balance: i64,
    last_modified_ledger_seq: u32,
) -> (LedgerKey, LedgerEntry) {
    let key = LedgerKey::Account(stellar_xdr::curr::LedgerKeyAccount {
        account_id: account_id.clone(),
    });

    let entry = LedgerEntry {
        last_modified_ledger_seq,
        data: LedgerEntryData::Account(AccountEntry {
            account_id,
            balance,
            seq_num: SequenceNumber(seq_num),
            num_sub_entries: 0,
            inflation_dest: None,
            flags: 0,
            home_domain: String32::default(),
            thresholds: Thresholds([1, 0, 0, 0]),
            signers: VecM::default(),
            ext: AccountEntryExt::V0,
        }),
        ext: LedgerEntryExt::V0,
    };

    (key, entry)
}

/// Create an account entry with seq_time and seq_ledger set (for min_seq_age/min_seq_ledger_gap tests).
fn create_account_entry_with_seq_info(
    account_id: AccountId,
    seq_num: i64,
    balance: i64,
    seq_ledger: u32,
    seq_time: u64,
) -> (LedgerKey, LedgerEntry) {
    let key = LedgerKey::Account(stellar_xdr::curr::LedgerKeyAccount {
        account_id: account_id.clone(),
    });

    let entry = LedgerEntry {
        last_modified_ledger_seq: seq_ledger,
        data: LedgerEntryData::Account(AccountEntry {
            account_id,
            balance,
            seq_num: SequenceNumber(seq_num),
            num_sub_entries: 0,
            inflation_dest: None,
            flags: 0,
            home_domain: String32::default(),
            thresholds: Thresholds([1, 0, 0, 0]),
            signers: VecM::default(),
            ext: AccountEntryExt::V1(AccountEntryExtensionV1 {
                liabilities: Liabilities {
                    buying: 0,
                    selling: 0,
                },
                ext: AccountEntryExtensionV1Ext::V2(AccountEntryExtensionV2 {
                    num_sponsored: 0,
                    num_sponsoring: 0,
                    signer_sponsoring_i_ds: vec![].try_into().unwrap(),
                    ext: AccountEntryExtensionV2Ext::V3(AccountEntryExtensionV3 {
                        ext: ExtensionPoint::V0,
                        seq_ledger,
                        seq_time: TimePoint(seq_time),
                    }),
                }),
            }),
        }),
        ext: LedgerEntryExt::V0,
    };

    (key, entry)
}

fn create_account_entry(
    account_id: AccountId,
    seq_num: i64,
    balance: i64,
) -> (LedgerKey, LedgerEntry) {
    create_account_entry_with_last_modified(account_id, seq_num, balance, 1)
}

fn create_account_entry_with_flags(
    account_id: AccountId,
    seq_num: i64,
    balance: i64,
    flags: u32,
) -> (LedgerKey, LedgerEntry) {
    let key = LedgerKey::Account(stellar_xdr::curr::LedgerKeyAccount {
        account_id: account_id.clone(),
    });

    let entry = LedgerEntry {
        last_modified_ledger_seq: 1,
        data: LedgerEntryData::Account(AccountEntry {
            account_id,
            balance,
            seq_num: SequenceNumber(seq_num),
            num_sub_entries: 0,
            inflation_dest: None,
            flags,
            home_domain: String32::default(),
            thresholds: Thresholds([1, 0, 0, 0]),
            signers: VecM::default(),
            ext: AccountEntryExt::V0,
        }),
        ext: LedgerEntryExt::V0,
    };

    (key, entry)
}

fn create_trustline_entry(
    account_id: AccountId,
    asset: TrustLineAsset,
    balance: i64,
    limit: i64,
    flags: u32,
) -> (LedgerKey, LedgerEntry) {
    let key = LedgerKey::Trustline(LedgerKeyTrustLine {
        account_id: account_id.clone(),
        asset: asset.clone(),
    });

    let entry = LedgerEntry {
        last_modified_ledger_seq: 1,
        data: LedgerEntryData::Trustline(TrustLineEntry {
            account_id,
            asset,
            balance,
            limit,
            flags,
            ext: TrustLineEntryExt::V0,
        }),
        ext: LedgerEntryExt::V0,
    };

    (key, entry)
}

fn set_account_liabilities(entry: &mut LedgerEntry, selling: i64, buying: i64) {
    let LedgerEntryData::Account(account) = &mut entry.data else {
        return;
    };
    account.ext = AccountEntryExt::V1(stellar_xdr::curr::AccountEntryExtensionV1 {
        liabilities: stellar_xdr::curr::Liabilities { selling, buying },
        ext: stellar_xdr::curr::AccountEntryExtensionV1Ext::V0,
    });
}

fn set_trustline_liabilities(entry: &mut LedgerEntry, selling: i64, buying: i64) {
    let LedgerEntryData::Trustline(trustline) = &mut entry.data else {
        return;
    };
    trustline.ext = TrustLineEntryExt::V1(stellar_xdr::curr::TrustLineEntryV1 {
        liabilities: stellar_xdr::curr::Liabilities { selling, buying },
        ext: stellar_xdr::curr::TrustLineEntryV1Ext::V0,
    });
}

fn create_liquidity_pool_entry(
    pool_id: PoolId,
    asset_a: Asset,
    asset_b: Asset,
    reserve_a: i64,
    reserve_b: i64,
    total_shares: i64,
    share_trustline_count: i64,
) -> (LedgerKey, LedgerEntry) {
    let key = LedgerKey::LiquidityPool(LedgerKeyLiquidityPool {
        liquidity_pool_id: pool_id.clone(),
    });

    let entry = LedgerEntry {
        last_modified_ledger_seq: 1,
        data: LedgerEntryData::LiquidityPool(LiquidityPoolEntry {
            liquidity_pool_id: pool_id,
            body: LiquidityPoolEntryBody::LiquidityPoolConstantProduct(
                LiquidityPoolEntryConstantProduct {
                    params: LiquidityPoolConstantProductParameters {
                        asset_a,
                        asset_b,
                        fee: LIQUIDITY_POOL_FEE_V18,
                    },
                    reserve_a,
                    reserve_b,
                    total_pool_shares: total_shares,
                    pool_shares_trust_line_count: share_trustline_count,
                },
            ),
        }),
        ext: LedgerEntryExt::V0,
    };

    (key, entry)
}

fn create_offer_entry(
    seller_id: AccountId,
    offer_id: i64,
    selling: Asset,
    buying: Asset,
    amount: i64,
    price: Price,
) -> (LedgerKey, LedgerEntry) {
    let key = LedgerKey::Offer(LedgerKeyOffer {
        seller_id: seller_id.clone(),
        offer_id,
    });
    let entry = LedgerEntry {
        last_modified_ledger_seq: 1,
        data: LedgerEntryData::Offer(OfferEntry {
            seller_id,
            offer_id,
            selling,
            buying,
            amount,
            price,
            flags: 0,
            ext: OfferEntryExt::V0,
        }),
        ext: LedgerEntryExt::V0,
    };
    (key, entry)
}

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

fn i128_parts(value: i128) -> Int128Parts {
    Int128Parts {
        hi: (value >> 64) as i64,
        lo: value as u64,
    }
}

fn scval_symbol(sym: &str) -> ScVal {
    ScVal::Symbol(ScSymbol(StringM::try_from(sym).unwrap()))
}

fn account_id_to_strkey(account_id: &AccountId) -> String {
    henyey_crypto::account_id_to_strkey(account_id)
}

fn asset_code_to_string(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}

fn asset_string_scval(asset: &Asset) -> ScVal {
    let asset_str = match asset {
        Asset::Native => "native".to_string(),
        Asset::CreditAlphanum4(a) => format!(
            "{}:{}",
            asset_code_to_string(&a.asset_code.0),
            account_id_to_strkey(&a.issuer)
        ),
        Asset::CreditAlphanum12(a) => format!(
            "{}:{}",
            asset_code_to_string(&a.asset_code.0),
            account_id_to_strkey(&a.issuer)
        ),
    };
    ScVal::String(ScString(StringM::try_from(asset_str).unwrap()))
}

fn assert_transfer_event(
    event: &stellar_xdr::curr::ContractEvent,
    from: &ScAddress,
    to: &ScAddress,
    asset: &Asset,
    amount: i64,
) {
    let ContractEventBody::V0(body) = &event.body;
    let topics: &[ScVal] = body.topics.as_ref();
    assert_eq!(topics.len(), 4);
    assert_eq!(topics[0], scval_symbol("transfer"));
    assert_eq!(topics[1], ScVal::Address(from.clone()));
    assert_eq!(topics[2], ScVal::Address(to.clone()));
    assert_eq!(topics[3], asset_string_scval(asset));
    assert_eq!(body.data, ScVal::I128(i128_parts(amount.into())));
}

fn assert_claim_atom_events(
    events: &[stellar_xdr::curr::ContractEvent],
    claim: &ClaimAtom,
    source_id: &AccountId,
    start: usize,
) -> usize {
    let source_address = ScAddress::Account(source_id.clone());
    match claim {
        ClaimAtom::OrderBook(ClaimOfferAtom {
            seller_id,
            asset_sold,
            amount_sold,
            asset_bought,
            amount_bought,
            ..
        }) => {
            let seller = ScAddress::Account(seller_id.clone());
            assert_transfer_event(
                &events[start],
                &source_address,
                &seller,
                asset_bought,
                *amount_bought,
            );
            assert_transfer_event(
                &events[start + 1],
                &seller,
                &source_address,
                asset_sold,
                *amount_sold,
            );
            start + 2
        }
        ClaimAtom::LiquidityPool(ClaimLiquidityAtom {
            liquidity_pool_id,
            asset_sold,
            amount_sold,
            asset_bought,
            amount_bought,
            ..
        }) => {
            let pool = ScAddress::LiquidityPool(liquidity_pool_id.clone());
            assert_transfer_event(
                &events[start],
                &source_address,
                &pool,
                asset_bought,
                *amount_bought,
            );
            assert_transfer_event(
                &events[start + 1],
                &pool,
                &source_address,
                asset_sold,
                *amount_sold,
            );
            start + 2
        }
        ClaimAtom::V0(claim) => {
            let seller = ScAddress::Account(AccountId(PublicKey::PublicKeyTypeEd25519(
                claim.seller_ed25519.clone(),
            )));
            assert_transfer_event(
                &events[start],
                &source_address,
                &seller,
                &claim.asset_bought,
                claim.amount_bought,
            );
            assert_transfer_event(
                &events[start + 1],
                &seller,
                &source_address,
                &claim.asset_sold,
                claim.amount_sold,
            );
            start + 2
        }
    }
}

fn native_asset_contract_id(network_id: &NetworkId) -> ContractId {
    let preimage = HashIdPreimage::ContractId(HashIdPreimageContractId {
        network_id: Hash::from(network_id.0),
        contract_id_preimage: ContractIdPreimage::Asset(stellar_xdr::curr::Asset::Native),
    });
    let hash = henyey_common::Hash256::hash_xdr(&preimage);
    ContractId(Hash::from(hash))
}

mod classic_events;
mod preconditions;
mod regression;
