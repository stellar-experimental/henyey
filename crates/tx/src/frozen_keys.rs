//! Frozen ledger keys configuration (CAP-77, Protocol 26).
//!
//! This module provides the `FrozenKeyConfig` struct that holds the set of
//! frozen ledger keys and bypass transaction hashes loaded from the network
//! configuration. Transactions accessing frozen keys are rejected with
//! `txFROZEN_KEY_ACCESSED` unless the transaction hash is in the bypass set.

use std::collections::HashSet;

use henyey_common::Hash256;
use stellar_xdr::curr::{
    AccountId, Asset, ChangeTrustAsset, Hash, LedgerFootprint, LedgerKey, Limits, MuxedAccount,
    Operation, OperationBody, RevokeSponsorshipOp, WriteXdr,
};

/// Configuration for frozen ledger keys (Protocol 26+).
///
/// Loaded from CONFIG_SETTING_FROZEN_LEDGER_KEYS and CONFIG_SETTING_FREEZE_BYPASS_TXS
/// at the start of each ledger close.
#[derive(Debug, Clone)]
pub struct FrozenKeyConfig {
    /// Set of frozen ledger keys (stored as XDR-encoded bytes for efficient comparison).
    frozen_keys: HashSet<Vec<u8>>,
    /// Set of transaction hashes that bypass the frozen key check.
    bypass_txs: HashSet<Hash256>,
}

impl FrozenKeyConfig {
    /// Create an empty configuration (no frozen keys, no bypass txs).
    pub fn empty() -> Self {
        Self {
            frozen_keys: HashSet::new(),
            bypass_txs: HashSet::new(),
        }
    }

    /// Create from frozen key bytes and bypass tx hashes.
    pub fn new(frozen_key_bytes: Vec<Vec<u8>>, bypass_tx_hashes: Vec<Hash>) -> Self {
        let frozen_keys: HashSet<Vec<u8>> = frozen_key_bytes.into_iter().collect();
        let bypass_txs: HashSet<Hash256> = bypass_tx_hashes.iter().map(|h| Hash256(h.0)).collect();
        Self {
            frozen_keys,
            bypass_txs,
        }
    }

    /// Returns true if there are any frozen keys configured.
    pub fn has_frozen_keys(&self) -> bool {
        !self.frozen_keys.is_empty()
    }

    /// Returns true if the given ledger key is frozen.
    pub fn is_key_frozen(&self, key: &LedgerKey) -> bool {
        if self.frozen_keys.is_empty() {
            return false;
        }
        // Encode the key to XDR bytes for comparison against the frozen set.
        // This matches stellar-core's approach where frozen keys are stored as
        // opaque XDR-encoded bytes.
        match key.to_xdr(Limits::none()) {
            Ok(bytes) => self.frozen_keys.contains(&bytes),
            Err(_) => false,
        }
    }

    /// Returns true if the given transaction hash is in the freeze bypass set.
    pub fn is_freeze_bypass_tx(&self, tx_hash: &Hash256) -> bool {
        self.bypass_txs.contains(tx_hash)
    }
}

impl Default for FrozenKeyConfig {
    fn default() -> Self {
        Self::empty()
    }
}

/// Helper to construct an account LedgerKey for frozen key checks.
pub fn account_key(account_id: &stellar_xdr::curr::AccountId) -> LedgerKey {
    LedgerKey::Account(stellar_xdr::curr::LedgerKeyAccount {
        account_id: account_id.clone(),
    })
}

/// Helper to construct a trustline LedgerKey for frozen key checks.
pub fn trustline_key(
    account_id: &stellar_xdr::curr::AccountId,
    asset: &stellar_xdr::curr::Asset,
) -> LedgerKey {
    LedgerKey::Trustline(stellar_xdr::curr::LedgerKeyTrustLine {
        account_id: account_id.clone(),
        asset: asset_to_trustline_asset(asset),
    })
}

/// Convert an Asset to a TrustLineAsset (for trustline key construction).
fn asset_to_trustline_asset(asset: &stellar_xdr::curr::Asset) -> stellar_xdr::curr::TrustLineAsset {
    match asset {
        stellar_xdr::curr::Asset::Native => {
            // Native assets don't have trustlines — this shouldn't be called for native.
            // Return a dummy value; caller should guard against native.
            stellar_xdr::curr::TrustLineAsset::Native
        }
        stellar_xdr::curr::Asset::CreditAlphanum4(a4) => {
            stellar_xdr::curr::TrustLineAsset::CreditAlphanum4(a4.clone())
        }
        stellar_xdr::curr::Asset::CreditAlphanum12(a12) => {
            stellar_xdr::curr::TrustLineAsset::CreditAlphanum12(a12.clone())
        }
    }
}

/// Check if a transaction accesses any frozen ledger key (CAP-77).
///
/// Parity: stellar-core TransactionFrame::accessesFrozenKey
/// Checks: TX source account, Soroban footprint keys, and per-operation keys.
pub fn accesses_frozen_key(
    source_account_id: &AccountId,
    operations: &[Operation],
    soroban_footprint: Option<&LedgerFootprint>,
    config: &FrozenKeyConfig,
) -> bool {
    if !config.has_frozen_keys() {
        return false;
    }

    // Check TX source account
    if config.is_key_frozen(&account_key(source_account_id)) {
        return true;
    }

    // If Soroban TX: check all footprint keys (read_only + read_write)
    if let Some(footprint) = soroban_footprint {
        for key in footprint
            .read_only
            .iter()
            .chain(footprint.read_write.iter())
        {
            if config.is_key_frozen(key) {
                return true;
            }
        }
    }

    // Check each operation
    for op in operations {
        if operation_accesses_frozen_key(op, source_account_id, config) {
            return true;
        }
    }

    false
}

/// Check if a single operation accesses a frozen key (CAP-77).
///
/// Parity: stellar-core OperationFrame::accessesFrozenKey (base class checks
/// op source account) + per-op doesAccessFrozenKey virtual dispatch.
pub fn operation_accesses_frozen_key(
    op: &Operation,
    tx_source: &AccountId,
    config: &FrozenKeyConfig,
) -> bool {
    // Check op-level source account override (if present)
    if let Some(ref op_source) = op.source_account {
        let op_source_id = muxed_to_account_id(op_source);
        if config.is_key_frozen(&account_key(&op_source_id)) {
            return true;
        }
    }

    // Resolve the effective source for this operation
    let effective_source = op
        .source_account
        .as_ref()
        .map(muxed_to_account_id)
        .unwrap_or_else(|| tx_source.clone());

    // Per-operation frozen key checks (parity: doesAccessFrozenKey virtual dispatch)
    match &op.body {
        OperationBody::CreateAccount(create) => {
            config.is_key_frozen(&account_key(&create.destination))
        }
        OperationBody::Payment(payment) => {
            if matches!(payment.asset, Asset::Native) {
                // Native payment: check dest account key
                config.is_key_frozen(&account_key(&muxed_to_account_id(&payment.destination)))
            } else {
                // Non-native: check source trustline and dest trustline
                if config.is_key_frozen(&trustline_key(&effective_source, &payment.asset)) {
                    return true;
                }
                config.is_key_frozen(&trustline_key(
                    &muxed_to_account_id(&payment.destination),
                    &payment.asset,
                ))
            }
        }
        OperationBody::PathPaymentStrictReceive(pp) => path_payment_accesses_frozen_key(
            &effective_source,
            &pp.send_asset,
            &muxed_to_account_id(&pp.destination),
            &pp.dest_asset,
            config,
        ),
        OperationBody::PathPaymentStrictSend(pp) => path_payment_accesses_frozen_key(
            &effective_source,
            &pp.send_asset,
            &muxed_to_account_id(&pp.destination),
            &pp.dest_asset,
            config,
        ),
        OperationBody::ManageSellOffer(offer) => manage_offer_accesses_frozen_key(
            &effective_source,
            &offer.selling,
            &offer.buying,
            config,
        ),
        OperationBody::ManageBuyOffer(offer) => manage_offer_accesses_frozen_key(
            &effective_source,
            &offer.selling,
            &offer.buying,
            config,
        ),
        OperationBody::CreatePassiveSellOffer(offer) => manage_offer_accesses_frozen_key(
            &effective_source,
            &offer.selling,
            &offer.buying,
            config,
        ),
        OperationBody::ChangeTrust(ct) => {
            // Only check alphanum4/alphanum12 trustlines, not pool shares
            // Parity: ChangeTrustOpFrame::doesAccessFrozenKey
            match &ct.line {
                ChangeTrustAsset::CreditAlphanum4(a4) => {
                    let asset = Asset::CreditAlphanum4(a4.clone());
                    config.is_key_frozen(&trustline_key(&effective_source, &asset))
                }
                ChangeTrustAsset::CreditAlphanum12(a12) => {
                    let asset = Asset::CreditAlphanum12(a12.clone());
                    config.is_key_frozen(&trustline_key(&effective_source, &asset))
                }
                ChangeTrustAsset::Native | ChangeTrustAsset::PoolShare(_) => false,
            }
        }
        OperationBody::AllowTrust(at) => {
            // Construct asset from asset_code + source as issuer
            let asset = allow_trust_asset(&effective_source, &at.asset);
            config.is_key_frozen(&trustline_key(&at.trustor, &asset))
        }
        OperationBody::SetTrustLineFlags(stf) => {
            config.is_key_frozen(&trustline_key(&stf.trustor, &stf.asset))
        }
        OperationBody::AccountMerge(dest) => {
            config.is_key_frozen(&account_key(&muxed_to_account_id(dest)))
        }
        OperationBody::Clawback(cb) => {
            config.is_key_frozen(&trustline_key(&muxed_to_account_id(&cb.from), &cb.asset))
        }
        OperationBody::CreateClaimableBalance(ccb) => {
            if matches!(ccb.asset, Asset::Native) {
                false
            } else {
                config.is_key_frozen(&trustline_key(&effective_source, &ccb.asset))
            }
        }
        OperationBody::RevokeSponsorship(rs) => match rs {
            RevokeSponsorshipOp::LedgerEntry(key) => config.is_key_frozen(key),
            RevokeSponsorshipOp::Signer(signer) => {
                config.is_key_frozen(&account_key(&signer.account_id))
            }
        },
        // These operations return false from doesAccessFrozenKey:
        // - BumpSequence, ManageData, SetOptions, Inflation
        // - Begin/EndSponsoringFutureReserves
        // - ClawbackClaimableBalance, ClaimClaimableBalance (apply-time check)
        // - LiquidityPoolDeposit/Withdraw (apply-time check)
        // - InvokeHostFunction, RestoreFootprint, ExtendFootprintTtl (footprint checked at TX level)
        _ => false,
    }
}

/// Helper for path payment frozen key checks.
/// Parity: PathPaymentOpFrameBase::doesAccessFrozenKey
fn path_payment_accesses_frozen_key(
    source: &AccountId,
    send_asset: &Asset,
    dest_id: &AccountId,
    dest_asset: &Asset,
    config: &FrozenKeyConfig,
) -> bool {
    // Check source trustline for send asset (if non-native)
    if !matches!(send_asset, Asset::Native)
        && config.is_key_frozen(&trustline_key(source, send_asset))
    {
        return true;
    }
    // Check dest: trustline if non-native, account if native
    if !matches!(dest_asset, Asset::Native) {
        config.is_key_frozen(&trustline_key(dest_id, dest_asset))
    } else {
        config.is_key_frozen(&account_key(dest_id))
    }
}

/// Helper for manage offer frozen key checks.
/// Parity: ManageOfferOpFrameBase::doesAccessFrozenKey
fn manage_offer_accesses_frozen_key(
    source: &AccountId,
    selling: &Asset,
    buying: &Asset,
    config: &FrozenKeyConfig,
) -> bool {
    if !matches!(selling, Asset::Native) && config.is_key_frozen(&trustline_key(source, selling)) {
        return true;
    }
    if !matches!(buying, Asset::Native) && config.is_key_frozen(&trustline_key(source, buying)) {
        return true;
    }
    false
}

/// Construct an Asset from AllowTrust asset code + issuer.
fn allow_trust_asset(issuer: &AccountId, asset_code: &stellar_xdr::curr::AssetCode) -> Asset {
    match asset_code {
        stellar_xdr::curr::AssetCode::CreditAlphanum4(code) => {
            Asset::CreditAlphanum4(stellar_xdr::curr::AlphaNum4 {
                asset_code: code.clone(),
                issuer: issuer.clone(),
            })
        }
        stellar_xdr::curr::AssetCode::CreditAlphanum12(code) => {
            Asset::CreditAlphanum12(stellar_xdr::curr::AlphaNum12 {
                asset_code: code.clone(),
                issuer: issuer.clone(),
            })
        }
    }
}

/// Convert a MuxedAccount to AccountId.
fn muxed_to_account_id(muxed: &MuxedAccount) -> AccountId {
    match muxed {
        MuxedAccount::Ed25519(key) => AccountId(
            stellar_xdr::curr::PublicKey::PublicKeyTypeEd25519(key.clone()),
        ),
        MuxedAccount::MuxedEd25519(m) => AccountId(
            stellar_xdr::curr::PublicKey::PublicKeyTypeEd25519(m.ed25519.clone()),
        ),
    }
}

/// Check if an offer accesses a frozen key (CAP-77).
///
/// Used during DEX crossing to skip/delete frozen offers. An offer accesses a
/// frozen key if:
/// - The seller's account is frozen and at least one side of the offer is native
/// - The selling asset's trustline is frozen (non-native only)
/// - The buying asset's trustline is frozen (non-native only)
pub fn offer_accesses_frozen_key(
    offer: &stellar_xdr::curr::OfferEntry,
    config: &FrozenKeyConfig,
) -> bool {
    if !config.has_frozen_keys() {
        return false;
    }
    // Frozen seller account only matters when at least one side is native
    if (matches!(offer.selling, stellar_xdr::curr::Asset::Native)
        || matches!(offer.buying, stellar_xdr::curr::Asset::Native))
        && config.is_key_frozen(&account_key(&offer.seller_id))
    {
        return true;
    }
    // Check selling asset trustline (if non-native)
    if !matches!(offer.selling, stellar_xdr::curr::Asset::Native)
        && config.is_key_frozen(&trustline_key(&offer.seller_id, &offer.selling))
    {
        return true;
    }
    // Check buying asset trustline (if non-native)
    if !matches!(offer.buying, stellar_xdr::curr::Asset::Native)
        && config.is_key_frozen(&trustline_key(&offer.seller_id, &offer.buying))
    {
        return true;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use stellar_xdr::curr::*;

    fn make_account_id(seed: u8) -> AccountId {
        AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([seed; 32])))
    }

    fn make_account_key(seed: u8) -> LedgerKey {
        account_key(&make_account_id(seed))
    }

    #[test]
    fn test_empty_config_no_frozen_keys() {
        let config = FrozenKeyConfig::empty();
        assert!(!config.has_frozen_keys());
        assert!(!config.is_key_frozen(&make_account_key(1)));
    }

    #[test]
    fn test_frozen_key_detection() {
        let key = make_account_key(1);
        let key_bytes = key.to_xdr(Limits::none()).unwrap();
        let config = FrozenKeyConfig::new(vec![key_bytes], vec![]);

        assert!(config.has_frozen_keys());
        assert!(config.is_key_frozen(&make_account_key(1)));
        assert!(!config.is_key_frozen(&make_account_key(2)));
    }

    #[test]
    fn test_bypass_tx_hash() {
        let config = FrozenKeyConfig::new(vec![], vec![Hash([42u8; 32])]);

        assert!(config.is_freeze_bypass_tx(&Hash256([42u8; 32])));
        assert!(!config.is_freeze_bypass_tx(&Hash256([0u8; 32])));
    }

    fn make_credit_asset(code: &[u8; 4], issuer_seed: u8) -> Asset {
        Asset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4(*code),
            issuer: make_account_id(issuer_seed),
        })
    }

    fn make_offer(seller_seed: u8, selling: Asset, buying: Asset) -> OfferEntry {
        OfferEntry {
            seller_id: make_account_id(seller_seed),
            offer_id: 1,
            selling,
            buying,
            amount: 1000,
            price: Price { n: 1, d: 1 },
            flags: 0,
            ext: OfferEntryExt::V0,
        }
    }

    #[test]
    fn test_offer_accesses_frozen_key_empty_config() {
        let offer = make_offer(1, Asset::Native, make_credit_asset(b"USD\0", 2));
        let config = FrozenKeyConfig::empty();
        assert!(!offer_accesses_frozen_key(&offer, &config));
    }

    #[test]
    fn test_offer_frozen_seller_account_native_selling() {
        let seller_id = make_account_id(1);
        let acct_key = account_key(&seller_id);
        let key_bytes = acct_key.to_xdr(Limits::none()).unwrap();
        let config = FrozenKeyConfig::new(vec![key_bytes], vec![]);

        // Selling native, buying credit: seller account frozen -> true
        let offer = make_offer(1, Asset::Native, make_credit_asset(b"USD\0", 2));
        assert!(offer_accesses_frozen_key(&offer, &config));
    }

    #[test]
    fn test_offer_frozen_seller_account_native_buying() {
        let seller_id = make_account_id(1);
        let acct_key = account_key(&seller_id);
        let key_bytes = acct_key.to_xdr(Limits::none()).unwrap();
        let config = FrozenKeyConfig::new(vec![key_bytes], vec![]);

        // Selling credit, buying native: seller account frozen -> true
        let offer = make_offer(1, make_credit_asset(b"USD\0", 2), Asset::Native);
        assert!(offer_accesses_frozen_key(&offer, &config));
    }

    #[test]
    fn test_offer_frozen_seller_account_no_native() {
        let seller_id = make_account_id(1);
        let acct_key = account_key(&seller_id);
        let key_bytes = acct_key.to_xdr(Limits::none()).unwrap();
        let config = FrozenKeyConfig::new(vec![key_bytes], vec![]);

        // Both sides credit: seller account frozen but no native side -> false
        // (unless the trustlines are also frozen)
        let offer = make_offer(
            1,
            make_credit_asset(b"USD\0", 2),
            make_credit_asset(b"EUR\0", 3),
        );
        assert!(!offer_accesses_frozen_key(&offer, &config));
    }

    #[test]
    fn test_offer_frozen_selling_trustline() {
        let seller_id = make_account_id(1);
        let selling = make_credit_asset(b"USD\0", 2);
        let tl_key = trustline_key(&seller_id, &selling);
        let key_bytes = tl_key.to_xdr(Limits::none()).unwrap();
        let config = FrozenKeyConfig::new(vec![key_bytes], vec![]);

        let offer = make_offer(1, selling, make_credit_asset(b"EUR\0", 3));
        assert!(offer_accesses_frozen_key(&offer, &config));
    }

    #[test]
    fn test_offer_frozen_buying_trustline() {
        let seller_id = make_account_id(1);
        let buying = make_credit_asset(b"EUR\0", 3);
        let tl_key = trustline_key(&seller_id, &buying);
        let key_bytes = tl_key.to_xdr(Limits::none()).unwrap();
        let config = FrozenKeyConfig::new(vec![key_bytes], vec![]);

        let offer = make_offer(1, make_credit_asset(b"USD\0", 2), buying);
        assert!(offer_accesses_frozen_key(&offer, &config));
    }

    #[test]
    fn test_offer_no_frozen_keys_match() {
        // Freeze a different account's trustline
        let other_id = make_account_id(99);
        let asset = make_credit_asset(b"USD\0", 2);
        let tl_key = trustline_key(&other_id, &asset);
        let key_bytes = tl_key.to_xdr(Limits::none()).unwrap();
        let config = FrozenKeyConfig::new(vec![key_bytes], vec![]);

        let offer = make_offer(1, asset, Asset::Native);
        assert!(!offer_accesses_frozen_key(&offer, &config));
    }

    // --- accesses_frozen_key / operation_accesses_frozen_key tests ---

    fn make_op(body: OperationBody) -> Operation {
        Operation {
            source_account: None,
            body,
        }
    }

    fn make_op_with_source(body: OperationBody, source_seed: u8) -> Operation {
        Operation {
            source_account: Some(MuxedAccount::Ed25519(Uint256([source_seed; 32]))),
            body,
        }
    }

    fn freeze_account(seed: u8) -> FrozenKeyConfig {
        let key = account_key(&make_account_id(seed));
        let key_bytes = key.to_xdr(Limits::none()).unwrap();
        FrozenKeyConfig::new(vec![key_bytes], vec![])
    }

    fn freeze_trustline(account_seed: u8, asset: &Asset) -> FrozenKeyConfig {
        let key = trustline_key(&make_account_id(account_seed), asset);
        let key_bytes = key.to_xdr(Limits::none()).unwrap();
        FrozenKeyConfig::new(vec![key_bytes], vec![])
    }

    #[test]
    fn test_accesses_frozen_key_empty_config() {
        let source = make_account_id(1);
        let ops = vec![make_op(OperationBody::Inflation)];
        assert!(!accesses_frozen_key(
            &source,
            &ops,
            None,
            &FrozenKeyConfig::empty()
        ));
    }

    #[test]
    fn test_accesses_frozen_key_source_frozen() {
        let source = make_account_id(1);
        let config = freeze_account(1);
        let ops = vec![make_op(OperationBody::Inflation)];
        assert!(accesses_frozen_key(&source, &ops, None, &config));
    }

    #[test]
    fn test_accesses_frozen_key_source_not_frozen() {
        let source = make_account_id(1);
        let config = freeze_account(2); // freeze a different account
        let ops = vec![make_op(OperationBody::Inflation)];
        assert!(!accesses_frozen_key(&source, &ops, None, &config));
    }

    #[test]
    fn test_accesses_frozen_key_soroban_footprint() {
        let source = make_account_id(1);
        let frozen_key = LedgerKey::ContractData(stellar_xdr::curr::LedgerKeyContractData {
            contract: stellar_xdr::curr::ScAddress::Contract(stellar_xdr::curr::ContractId(Hash(
                [42u8; 32],
            ))),
            key: stellar_xdr::curr::ScVal::Void,
            durability: stellar_xdr::curr::ContractDataDurability::Persistent,
        });
        let key_bytes = frozen_key.to_xdr(Limits::none()).unwrap();
        let config = FrozenKeyConfig::new(vec![key_bytes], vec![]);

        let footprint = LedgerFootprint {
            read_only: vec![frozen_key].try_into().unwrap(),
            read_write: vec![].try_into().unwrap(),
        };
        let ops = vec![];
        assert!(accesses_frozen_key(
            &source,
            &ops,
            Some(&footprint),
            &config
        ));
    }

    #[test]
    fn test_op_create_account_frozen_dest() {
        let tx_source = make_account_id(1);
        let config = freeze_account(2);
        let op = make_op(OperationBody::CreateAccount(
            stellar_xdr::curr::CreateAccountOp {
                destination: make_account_id(2),
                starting_balance: 1000,
            },
        ));
        assert!(operation_accesses_frozen_key(&op, &tx_source, &config));
    }

    #[test]
    fn test_op_payment_native_frozen_dest() {
        let tx_source = make_account_id(1);
        let config = freeze_account(2);
        let op = make_op(OperationBody::Payment(stellar_xdr::curr::PaymentOp {
            destination: MuxedAccount::Ed25519(Uint256([2u8; 32])),
            asset: Asset::Native,
            amount: 100,
        }));
        assert!(operation_accesses_frozen_key(&op, &tx_source, &config));
    }

    #[test]
    fn test_op_payment_credit_frozen_source_trustline() {
        let tx_source = make_account_id(1);
        let asset = make_credit_asset(b"USD\0", 3);
        let config = freeze_trustline(1, &asset);
        let op = make_op(OperationBody::Payment(stellar_xdr::curr::PaymentOp {
            destination: MuxedAccount::Ed25519(Uint256([2u8; 32])),
            asset: asset.clone(),
            amount: 100,
        }));
        assert!(operation_accesses_frozen_key(&op, &tx_source, &config));
    }

    #[test]
    fn test_op_payment_credit_frozen_dest_trustline() {
        let tx_source = make_account_id(1);
        let asset = make_credit_asset(b"USD\0", 3);
        let config = freeze_trustline(2, &asset);
        let op = make_op(OperationBody::Payment(stellar_xdr::curr::PaymentOp {
            destination: MuxedAccount::Ed25519(Uint256([2u8; 32])),
            asset: asset.clone(),
            amount: 100,
        }));
        assert!(operation_accesses_frozen_key(&op, &tx_source, &config));
    }

    #[test]
    fn test_op_manage_sell_offer_frozen_selling_trustline() {
        let tx_source = make_account_id(1);
        let selling = make_credit_asset(b"USD\0", 3);
        let config = freeze_trustline(1, &selling);
        let op = make_op(OperationBody::ManageSellOffer(
            stellar_xdr::curr::ManageSellOfferOp {
                selling: selling.clone(),
                buying: Asset::Native,
                amount: 100,
                price: Price { n: 1, d: 1 },
                offer_id: 0,
            },
        ));
        assert!(operation_accesses_frozen_key(&op, &tx_source, &config));
    }

    #[test]
    fn test_op_manage_sell_offer_native_no_frozen() {
        let tx_source = make_account_id(1);
        let config = freeze_account(99); // freeze unrelated account
        let op = make_op(OperationBody::ManageSellOffer(
            stellar_xdr::curr::ManageSellOfferOp {
                selling: Asset::Native,
                buying: make_credit_asset(b"USD\0", 3),
                amount: 100,
                price: Price { n: 1, d: 1 },
                offer_id: 0,
            },
        ));
        assert!(!operation_accesses_frozen_key(&op, &tx_source, &config));
    }

    #[test]
    fn test_op_change_trust_frozen_trustline() {
        let tx_source = make_account_id(1);
        let asset = make_credit_asset(b"USD\0", 3);
        let config = freeze_trustline(1, &asset);
        let op = make_op(OperationBody::ChangeTrust(
            stellar_xdr::curr::ChangeTrustOp {
                line: ChangeTrustAsset::CreditAlphanum4(stellar_xdr::curr::AlphaNum4 {
                    asset_code: AssetCode4(*b"USD\0"),
                    issuer: make_account_id(3),
                }),
                limit: 1000,
            },
        ));
        assert!(operation_accesses_frozen_key(&op, &tx_source, &config));
    }

    #[test]
    fn test_op_account_merge_frozen_dest() {
        let tx_source = make_account_id(1);
        let config = freeze_account(2);
        let op = make_op(OperationBody::AccountMerge(MuxedAccount::Ed25519(Uint256(
            [2u8; 32],
        ))));
        assert!(operation_accesses_frozen_key(&op, &tx_source, &config));
    }

    #[test]
    fn test_op_clawback_frozen_trustline() {
        let tx_source = make_account_id(1);
        let asset = make_credit_asset(b"USD\0", 1);
        let config = freeze_trustline(2, &asset);
        let op = make_op(OperationBody::Clawback(stellar_xdr::curr::ClawbackOp {
            asset: asset.clone(),
            from: MuxedAccount::Ed25519(Uint256([2u8; 32])),
            amount: 50,
        }));
        assert!(operation_accesses_frozen_key(&op, &tx_source, &config));
    }

    #[test]
    fn test_op_create_claimable_balance_frozen_trustline() {
        let tx_source = make_account_id(1);
        let asset = make_credit_asset(b"USD\0", 3);
        let config = freeze_trustline(1, &asset);
        let op = make_op(OperationBody::CreateClaimableBalance(
            stellar_xdr::curr::CreateClaimableBalanceOp {
                asset: asset.clone(),
                amount: 100,
                claimants: vec![].try_into().unwrap(),
            },
        ));
        assert!(operation_accesses_frozen_key(&op, &tx_source, &config));
    }

    #[test]
    fn test_op_create_claimable_balance_native_not_frozen() {
        let tx_source = make_account_id(1);
        let config = freeze_account(99);
        let op = make_op(OperationBody::CreateClaimableBalance(
            stellar_xdr::curr::CreateClaimableBalanceOp {
                asset: Asset::Native,
                amount: 100,
                claimants: vec![].try_into().unwrap(),
            },
        ));
        assert!(!operation_accesses_frozen_key(&op, &tx_source, &config));
    }

    #[test]
    fn test_op_set_trustline_flags_frozen() {
        let tx_source = make_account_id(1);
        let asset = make_credit_asset(b"USD\0", 1);
        let config = freeze_trustline(2, &asset);
        let op = make_op(OperationBody::SetTrustLineFlags(
            stellar_xdr::curr::SetTrustLineFlagsOp {
                trustor: make_account_id(2),
                asset: asset.clone(),
                clear_flags: 0,
                set_flags: 0,
            },
        ));
        assert!(operation_accesses_frozen_key(&op, &tx_source, &config));
    }

    #[test]
    fn test_op_revoke_sponsorship_ledger_entry() {
        let frozen_key = account_key(&make_account_id(5));
        let key_bytes = frozen_key.to_xdr(Limits::none()).unwrap();
        let config = FrozenKeyConfig::new(vec![key_bytes], vec![]);

        let tx_source = make_account_id(1);
        let op = make_op(OperationBody::RevokeSponsorship(
            RevokeSponsorshipOp::LedgerEntry(account_key(&make_account_id(5))),
        ));
        assert!(operation_accesses_frozen_key(&op, &tx_source, &config));
    }

    #[test]
    fn test_op_revoke_sponsorship_signer() {
        let config = freeze_account(5);
        let tx_source = make_account_id(1);
        let op = make_op(OperationBody::RevokeSponsorship(
            RevokeSponsorshipOp::Signer(stellar_xdr::curr::RevokeSponsorshipOpSigner {
                account_id: make_account_id(5),
                signer_key: stellar_xdr::curr::SignerKey::Ed25519(Uint256([0u8; 32])),
            }),
        ));
        assert!(operation_accesses_frozen_key(&op, &tx_source, &config));
    }

    #[test]
    fn test_op_source_account_override_frozen() {
        // Op has its own source account that is frozen
        let tx_source = make_account_id(1);
        let config = freeze_account(5);
        let op = make_op_with_source(OperationBody::Inflation, 5);
        assert!(operation_accesses_frozen_key(&op, &tx_source, &config));
    }

    #[test]
    fn test_op_noop_ops_not_frozen() {
        let tx_source = make_account_id(1);
        let config = freeze_account(99);

        // These ops return false from doesAccessFrozenKey
        for body in [
            OperationBody::Inflation,
            OperationBody::BumpSequence(stellar_xdr::curr::BumpSequenceOp {
                bump_to: stellar_xdr::curr::SequenceNumber(100),
            }),
        ] {
            assert!(
                !operation_accesses_frozen_key(&make_op(body.clone()), &tx_source, &config),
                "Expected false for {:?}",
                body
            );
        }
    }

    #[test]
    fn test_path_payment_frozen_source_trustline() {
        let tx_source = make_account_id(1);
        let send_asset = make_credit_asset(b"USD\0", 3);
        let config = freeze_trustline(1, &send_asset);
        let op = make_op(OperationBody::PathPaymentStrictReceive(
            stellar_xdr::curr::PathPaymentStrictReceiveOp {
                send_asset: send_asset.clone(),
                send_max: 100,
                destination: MuxedAccount::Ed25519(Uint256([2u8; 32])),
                dest_asset: Asset::Native,
                dest_amount: 50,
                path: vec![].try_into().unwrap(),
            },
        ));
        assert!(operation_accesses_frozen_key(&op, &tx_source, &config));
    }

    #[test]
    fn test_path_payment_native_dest_frozen_account() {
        let tx_source = make_account_id(1);
        let config = freeze_account(2);
        let op = make_op(OperationBody::PathPaymentStrictReceive(
            stellar_xdr::curr::PathPaymentStrictReceiveOp {
                send_asset: make_credit_asset(b"USD\0", 3),
                send_max: 100,
                destination: MuxedAccount::Ed25519(Uint256([2u8; 32])),
                dest_asset: Asset::Native,
                dest_amount: 50,
                path: vec![].try_into().unwrap(),
            },
        ));
        assert!(operation_accesses_frozen_key(&op, &tx_source, &config));
    }

    #[test]
    fn test_path_payment_non_native_dest_frozen_trustline() {
        let tx_source = make_account_id(1);
        let dest_asset = make_credit_asset(b"EUR\0", 4);
        let config = freeze_trustline(2, &dest_asset);
        let op = make_op(OperationBody::PathPaymentStrictSend(
            stellar_xdr::curr::PathPaymentStrictSendOp {
                send_asset: Asset::Native,
                send_amount: 100,
                destination: MuxedAccount::Ed25519(Uint256([2u8; 32])),
                dest_asset: dest_asset.clone(),
                dest_min: 50,
                path: vec![].try_into().unwrap(),
            },
        ));
        assert!(operation_accesses_frozen_key(&op, &tx_source, &config));
    }
}
