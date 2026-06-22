//! Prefetch key collection for per-ledger batch loading.
//!
//! This module provides functions to statically determine which ledger keys
//! an operation will need during execution, enabling batch prefetch before
//! the transaction loop. This matches stellar-core's `insertLedgerKeysToPrefetch`
//! virtual method pattern.

use std::collections::HashSet;

use stellar_xdr::{
    AccountId, AllowTrustOp, Asset, AssetCode, BeginSponsoringFutureReservesOp, ChangeTrustAsset,
    ChangeTrustOp, ClaimClaimableBalanceOp, ClawbackClaimableBalanceOp, ClawbackOp,
    CreateAccountOp, CreateClaimableBalanceOp, CreatePassiveSellOfferOp, LedgerKey,
    LedgerKeyAccount, LedgerKeyClaimableBalance, LedgerKeyData, LedgerKeyOffer, LedgerKeyTrustLine,
    LiquidityPoolDepositOp, LiquidityPoolWithdrawOp, ManageBuyOfferOp, ManageDataOp,
    ManageSellOfferOp, MuxedAccount, OperationBody, PathPaymentStrictReceiveOp,
    PathPaymentStrictSendOp, PaymentOp, SetTrustLineFlagsOp, TrustLineAsset,
};

use crate::frame::muxed_to_account_id;

// ---------------------------------------------------------------------------
// LedgerKey helper constructors
// ---------------------------------------------------------------------------

fn account_key(id: &AccountId) -> LedgerKey {
    LedgerKey::Account(LedgerKeyAccount {
        account_id: id.clone(),
    })
}

fn trustline_key(id: &AccountId, asset: &TrustLineAsset) -> LedgerKey {
    LedgerKey::Trustline(LedgerKeyTrustLine {
        account_id: id.clone(),
        asset: asset.clone(),
    })
}

fn offer_key(seller: &AccountId, id: i64) -> LedgerKey {
    LedgerKey::Offer(LedgerKeyOffer {
        seller_id: seller.clone(),
        offer_id: id,
    })
}

fn claimable_balance_key(id: &stellar_xdr::ClaimableBalanceId) -> LedgerKey {
    LedgerKey::ClaimableBalance(LedgerKeyClaimableBalance {
        balance_id: id.clone(),
    })
}

fn data_key(account: &AccountId, name: &stellar_xdr::String64) -> LedgerKey {
    LedgerKey::Data(LedgerKeyData {
        account_id: account.clone(),
        data_name: name.clone(),
    })
}

/// Insert trustline key for a non-native asset.
fn insert_asset_trustline(keys: &mut HashSet<LedgerKey>, id: &AccountId, asset: &Asset) {
    if let Some(tl_asset) = henyey_common::asset::non_native_asset_to_trustline_asset(asset) {
        keys.insert(trustline_key(id, &tl_asset));
    }
}

// ---------------------------------------------------------------------------
// Per-operation prefetch key functions
// ---------------------------------------------------------------------------

fn prefetch_keys_create_account(
    op: &CreateAccountOp,
    _source: &AccountId,
    keys: &mut HashSet<LedgerKey>,
) {
    keys.insert(account_key(&op.destination));
}

fn prefetch_keys_payment(op: &PaymentOp, source: &AccountId, keys: &mut HashSet<LedgerKey>) {
    let dest = muxed_to_account_id(&op.destination);
    keys.insert(account_key(&dest));
    insert_asset_trustline(keys, source, &op.asset);
    insert_asset_trustline(keys, &dest, &op.asset);
}

fn prefetch_keys_path_payment_strict_receive(
    op: &PathPaymentStrictReceiveOp,
    source: &AccountId,
    keys: &mut HashSet<LedgerKey>,
) {
    let dest = muxed_to_account_id(&op.destination);
    keys.insert(account_key(&dest));
    insert_asset_trustline(keys, source, &op.send_asset);
    insert_asset_trustline(keys, &dest, &op.dest_asset);
}

fn prefetch_keys_path_payment_strict_send(
    op: &PathPaymentStrictSendOp,
    source: &AccountId,
    keys: &mut HashSet<LedgerKey>,
) {
    let dest = muxed_to_account_id(&op.destination);
    keys.insert(account_key(&dest));
    insert_asset_trustline(keys, source, &op.send_asset);
    insert_asset_trustline(keys, &dest, &op.dest_asset);
}

fn prefetch_keys_manage_sell_offer(
    op: &ManageSellOfferOp,
    source: &AccountId,
    keys: &mut HashSet<LedgerKey>,
) {
    if op.offer_id != 0 {
        keys.insert(offer_key(source, op.offer_id));
    }
    insert_asset_trustline(keys, source, &op.selling);
    insert_asset_trustline(keys, source, &op.buying);
}

fn prefetch_keys_manage_buy_offer(
    op: &ManageBuyOfferOp,
    source: &AccountId,
    keys: &mut HashSet<LedgerKey>,
) {
    if op.offer_id != 0 {
        keys.insert(offer_key(source, op.offer_id));
    }
    insert_asset_trustline(keys, source, &op.selling);
    insert_asset_trustline(keys, source, &op.buying);
}

fn prefetch_keys_create_passive_sell_offer(
    op: &CreatePassiveSellOfferOp,
    source: &AccountId,
    keys: &mut HashSet<LedgerKey>,
) {
    insert_asset_trustline(keys, source, &op.selling);
    insert_asset_trustline(keys, source, &op.buying);
}

fn prefetch_keys_change_trust(
    op: &ChangeTrustOp,
    source: &AccountId,
    keys: &mut HashSet<LedgerKey>,
) {
    // Insert trustline key for the asset being trusted
    match &op.line {
        ChangeTrustAsset::Native => {}
        ChangeTrustAsset::CreditAlphanum4(a) => {
            keys.insert(trustline_key(
                source,
                &TrustLineAsset::CreditAlphanum4(a.clone()),
            ));
        }
        ChangeTrustAsset::CreditAlphanum12(a) => {
            keys.insert(trustline_key(
                source,
                &TrustLineAsset::CreditAlphanum12(a.clone()),
            ));
        }
        ChangeTrustAsset::PoolShare(_) => {
            // Pool share trustline key requires computing the pool ID hash.
            // This is handled by load_operation_accounts which has the SHA-256
            // computation. We skip it here for simplicity since pool shares
            // are less common and the computation cost outweighs the benefit
            // for a prefetch hint.
        }
    }
}

fn prefetch_keys_allow_trust(op: &AllowTrustOp, source: &AccountId, keys: &mut HashSet<LedgerKey>) {
    keys.insert(account_key(&op.trustor));
    // Build the trustline asset from the AllowTrust asset code + source (issuer)
    let tl_asset = match &op.asset {
        AssetCode::CreditAlphanum4(code) => {
            TrustLineAsset::CreditAlphanum4(stellar_xdr::AlphaNum4 {
                asset_code: code.clone(),
                issuer: source.clone(),
            })
        }
        AssetCode::CreditAlphanum12(code) => {
            TrustLineAsset::CreditAlphanum12(stellar_xdr::AlphaNum12 {
                asset_code: code.clone(),
                issuer: source.clone(),
            })
        }
    };
    keys.insert(trustline_key(&op.trustor, &tl_asset));
}

fn prefetch_keys_set_trust_line_flags(
    op: &SetTrustLineFlagsOp,
    _source: &AccountId,
    keys: &mut HashSet<LedgerKey>,
) {
    keys.insert(account_key(&op.trustor));
    insert_asset_trustline(keys, &op.trustor, &op.asset);
}

fn prefetch_keys_account_merge(
    dest: &MuxedAccount,
    _source: &AccountId,
    keys: &mut HashSet<LedgerKey>,
) {
    let dest_id = muxed_to_account_id(dest);
    keys.insert(account_key(&dest_id));
}

fn prefetch_keys_manage_data(op: &ManageDataOp, source: &AccountId, keys: &mut HashSet<LedgerKey>) {
    keys.insert(data_key(source, &op.data_name));
}

fn prefetch_keys_claim_claimable_balance(
    op: &ClaimClaimableBalanceOp,
    _source: &AccountId,
    keys: &mut HashSet<LedgerKey>,
) {
    keys.insert(claimable_balance_key(&op.balance_id));
}

fn prefetch_keys_create_claimable_balance(
    op: &CreateClaimableBalanceOp,
    source: &AccountId,
    keys: &mut HashSet<LedgerKey>,
) {
    insert_asset_trustline(keys, source, &op.asset);
}

fn prefetch_keys_clawback(op: &ClawbackOp, _source: &AccountId, keys: &mut HashSet<LedgerKey>) {
    let from = muxed_to_account_id(&op.from);
    insert_asset_trustline(keys, &from, &op.asset);
}

fn prefetch_keys_clawback_claimable_balance(
    op: &ClawbackClaimableBalanceOp,
    _source: &AccountId,
    keys: &mut HashSet<LedgerKey>,
) {
    keys.insert(claimable_balance_key(&op.balance_id));
}

fn prefetch_keys_liquidity_pool_deposit(
    op: &LiquidityPoolDepositOp,
    _source: &AccountId,
    keys: &mut HashSet<LedgerKey>,
) {
    keys.insert(LedgerKey::LiquidityPool(
        stellar_xdr::LedgerKeyLiquidityPool {
            liquidity_pool_id: op.liquidity_pool_id.clone(),
        },
    ));
}

fn prefetch_keys_liquidity_pool_withdraw(
    op: &LiquidityPoolWithdrawOp,
    _source: &AccountId,
    keys: &mut HashSet<LedgerKey>,
) {
    keys.insert(LedgerKey::LiquidityPool(
        stellar_xdr::LedgerKeyLiquidityPool {
            liquidity_pool_id: op.liquidity_pool_id.clone(),
        },
    ));
}

fn prefetch_keys_begin_sponsoring(
    op: &BeginSponsoringFutureReservesOp,
    _source: &AccountId,
    keys: &mut HashSet<LedgerKey>,
) {
    keys.insert(account_key(&op.sponsored_id));
}

// ---------------------------------------------------------------------------
// Central dispatcher
// ---------------------------------------------------------------------------

/// Collect all statically-known ledger keys needed for an operation.
///
/// This is the central dispatcher that routes to per-operation prefetch functions.
/// Keys collected here are used for batch prefetch before the transaction loop,
/// matching stellar-core's `insertLedgerKeysToPrefetch` pattern.
///
/// Only statically-determinable keys are included; keys that depend on loaded
/// state (e.g., sponsor accounts, offer dependencies after crossing) are handled
/// separately in `load_operation_accounts`.
pub fn collect_prefetch_keys(
    op: &OperationBody,
    source: &AccountId,
    keys: &mut HashSet<LedgerKey>,
) {
    match op {
        OperationBody::CreateAccount(data) => {
            prefetch_keys_create_account(data, source, keys);
        }
        OperationBody::Payment(data) => {
            prefetch_keys_payment(data, source, keys);
        }
        OperationBody::PathPaymentStrictReceive(data) => {
            prefetch_keys_path_payment_strict_receive(data, source, keys);
        }
        OperationBody::PathPaymentStrictSend(data) => {
            prefetch_keys_path_payment_strict_send(data, source, keys);
        }
        OperationBody::ManageSellOffer(data) => {
            prefetch_keys_manage_sell_offer(data, source, keys);
        }
        OperationBody::ManageBuyOffer(data) => {
            prefetch_keys_manage_buy_offer(data, source, keys);
        }
        OperationBody::CreatePassiveSellOffer(data) => {
            prefetch_keys_create_passive_sell_offer(data, source, keys);
        }
        OperationBody::ChangeTrust(data) => {
            prefetch_keys_change_trust(data, source, keys);
        }
        OperationBody::AllowTrust(data) => {
            prefetch_keys_allow_trust(data, source, keys);
        }
        OperationBody::SetTrustLineFlags(data) => {
            prefetch_keys_set_trust_line_flags(data, source, keys);
        }
        OperationBody::AccountMerge(dest) => {
            prefetch_keys_account_merge(dest, source, keys);
        }
        OperationBody::ManageData(data) => {
            prefetch_keys_manage_data(data, source, keys);
        }
        OperationBody::ClaimClaimableBalance(data) => {
            prefetch_keys_claim_claimable_balance(data, source, keys);
        }
        OperationBody::CreateClaimableBalance(data) => {
            prefetch_keys_create_claimable_balance(data, source, keys);
        }
        OperationBody::Clawback(data) => {
            prefetch_keys_clawback(data, source, keys);
        }
        OperationBody::ClawbackClaimableBalance(data) => {
            prefetch_keys_clawback_claimable_balance(data, source, keys);
        }
        OperationBody::LiquidityPoolDeposit(data) => {
            prefetch_keys_liquidity_pool_deposit(data, source, keys);
        }
        OperationBody::LiquidityPoolWithdraw(data) => {
            prefetch_keys_liquidity_pool_withdraw(data, source, keys);
        }
        OperationBody::BeginSponsoringFutureReserves(data) => {
            prefetch_keys_begin_sponsoring(data, source, keys);
        }
        // Soroban operations: empty implementation matching stellar-core.
        // All three Soroban op frames (InvokeHostFunction, ExtendFootprintTtl,
        // RestoreFootprint) have empty insertLedgerKeysToPrefetch in core.
        // Soroban entries use InMemorySorobanState; classic entries in Soroban
        // footprints are handled by load_soroban_footprint's own batch loading.
        OperationBody::InvokeHostFunction(_)
        | OperationBody::ExtendFootprintTtl(_)
        | OperationBody::RestoreFootprint(_) => {}
        // BumpSequence, Inflation, SetOptions, EndSponsoring, RevokeSponsorship:
        // no statically-known keys to prefetch.
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use stellar_xdr::*;

    fn test_account_id(seed: u8) -> AccountId {
        AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([seed; 32])))
    }

    fn test_credit_asset(code: &[u8; 4], issuer_seed: u8) -> Asset {
        Asset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4(*code),
            issuer: test_account_id(issuer_seed),
        })
    }

    #[test]
    fn test_prefetch_create_account() {
        let mut keys = HashSet::new();
        let source = test_account_id(1);
        let op = CreateAccountOp {
            destination: test_account_id(2),
            starting_balance: 10_000_000,
        };
        prefetch_keys_create_account(&op, &source, &mut keys);
        assert_eq!(keys.len(), 1);
        assert!(keys.contains(&account_key(&test_account_id(2))));
    }

    #[test]
    fn test_prefetch_payment_native() {
        let mut keys = HashSet::new();
        let source = test_account_id(1);
        let op = PaymentOp {
            destination: MuxedAccount::Ed25519(Uint256([2; 32])),
            asset: Asset::Native,
            amount: 1000,
        };
        prefetch_keys_payment(&op, &source, &mut keys);
        // Only destination account key for native
        assert_eq!(keys.len(), 1);
        assert!(keys.contains(&account_key(&test_account_id(2))));
    }

    #[test]
    fn test_prefetch_payment_credit() {
        let mut keys = HashSet::new();
        let source = test_account_id(1);
        let asset = test_credit_asset(b"USD\0", 3);
        let op = PaymentOp {
            destination: MuxedAccount::Ed25519(Uint256([2; 32])),
            asset: asset.clone(),
            amount: 1000,
        };
        prefetch_keys_payment(&op, &source, &mut keys);
        // dest account + source trustline + dest trustline = 3
        assert_eq!(keys.len(), 3);
    }

    #[test]
    fn test_prefetch_manage_sell_offer_with_id() {
        let mut keys = HashSet::new();
        let source = test_account_id(1);
        let selling = test_credit_asset(b"USD\0", 3);
        let buying = test_credit_asset(b"EUR\0", 4);
        let op = ManageSellOfferOp {
            selling: selling.clone(),
            buying: buying.clone(),
            amount: 1000,
            price: Price { n: 1, d: 1 },
            offer_id: 42,
        };
        prefetch_keys_manage_sell_offer(&op, &source, &mut keys);
        // offer key + 2 trustlines = 3
        assert_eq!(keys.len(), 3);
        assert!(keys.contains(&offer_key(&source, 42)));
    }

    #[test]
    fn test_prefetch_manage_sell_offer_new() {
        let mut keys = HashSet::new();
        let source = test_account_id(1);
        let op = ManageSellOfferOp {
            selling: Asset::Native,
            buying: test_credit_asset(b"USD\0", 3),
            amount: 1000,
            price: Price { n: 1, d: 1 },
            offer_id: 0,
        };
        prefetch_keys_manage_sell_offer(&op, &source, &mut keys);
        // no offer key + buying trustline = 1
        assert_eq!(keys.len(), 1);
    }

    #[test]
    fn test_prefetch_account_merge() {
        let mut keys = HashSet::new();
        let source = test_account_id(1);
        let dest = MuxedAccount::Ed25519(Uint256([2; 32]));
        prefetch_keys_account_merge(&dest, &source, &mut keys);
        assert_eq!(keys.len(), 1);
        assert!(keys.contains(&account_key(&test_account_id(2))));
    }

    #[test]
    fn test_prefetch_manage_data() {
        let mut keys = HashSet::new();
        let source = test_account_id(1);
        let op = ManageDataOp {
            data_name: String64::try_from(b"mykey".to_vec()).unwrap(),
            data_value: Some(b"val".to_vec().try_into().unwrap()),
        };
        prefetch_keys_manage_data(&op, &source, &mut keys);
        assert_eq!(keys.len(), 1);
    }

    #[test]
    fn test_prefetch_claim_claimable_balance() {
        let mut keys = HashSet::new();
        let source = test_account_id(1);
        let op = ClaimClaimableBalanceOp {
            balance_id: ClaimableBalanceId::ClaimableBalanceIdTypeV0(Hash([5; 32])),
        };
        prefetch_keys_claim_claimable_balance(&op, &source, &mut keys);
        assert_eq!(keys.len(), 1);
    }

    #[test]
    fn test_collect_prefetch_keys_dispatcher() {
        let mut keys = HashSet::new();
        let source = test_account_id(1);
        let body = OperationBody::CreateAccount(CreateAccountOp {
            destination: test_account_id(2),
            starting_balance: 10_000_000,
        });
        collect_prefetch_keys(&body, &source, &mut keys);
        assert_eq!(keys.len(), 1);
    }

    #[test]
    fn test_collect_prefetch_keys_soroban_noop() {
        let mut keys = HashSet::new();
        let source = test_account_id(1);
        // Soroban ops should not insert any keys
        let body = OperationBody::ExtendFootprintTtl(ExtendFootprintTtlOp {
            ext: ExtensionPoint::V0,
            extend_to: 100,
        });
        collect_prefetch_keys(&body, &source, &mut keys);
        assert!(keys.is_empty());
    }

    #[test]
    fn test_collect_prefetch_keys_inflation_noop() {
        let mut keys = HashSet::new();
        let source = test_account_id(1);
        collect_prefetch_keys(&OperationBody::Inflation, &source, &mut keys);
        assert!(keys.is_empty());
    }
}
