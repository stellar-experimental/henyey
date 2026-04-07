//! Frozen ledger keys configuration (CAP-77, Protocol 26).
//!
//! This module provides the `FrozenKeyConfig` struct that holds the set of
//! frozen ledger keys and bypass transaction hashes loaded from the network
//! configuration. Transactions accessing frozen keys are rejected with
//! `txFROZEN_KEY_ACCESSED` unless the transaction hash is in the bypass set.

use std::collections::HashSet;

use stellar_xdr::curr::{Hash, LedgerKey, Limits, WriteXdr};

/// Configuration for frozen ledger keys (Protocol 26+).
///
/// Loaded from CONFIG_SETTING_FROZEN_LEDGER_KEYS and CONFIG_SETTING_FREEZE_BYPASS_TXS
/// at the start of each ledger close.
#[derive(Debug, Clone)]
pub struct FrozenKeyConfig {
    /// Set of frozen ledger keys (stored as XDR-encoded bytes for efficient comparison).
    frozen_keys: HashSet<Vec<u8>>,
    /// Set of transaction hashes that bypass the frozen key check.
    bypass_txs: HashSet<[u8; 32]>,
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
        let bypass_txs: HashSet<[u8; 32]> = bypass_tx_hashes.iter().map(|h| h.0).collect();
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
    pub fn is_freeze_bypass_tx(&self, tx_hash: &[u8; 32]) -> bool {
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

        assert!(config.is_freeze_bypass_tx(&[42u8; 32]));
        assert!(!config.is_freeze_bypass_tx(&[0u8; 32]));
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
}
