//! In-memory store of offer seller accounts and trustlines.
//!
//! Holds the full set of account and trustline entries for every account that
//! currently has at least one live offer.  Built at startup from the bucket
//! scan and updated incrementally in `commit_close` alongside the offer store.
//!
//! # Motivation
//!
//! DEX offer crossing is the most expensive classic operation because each
//! crossed offer requires loading the seller's account entry (for balance
//! checks, sub-entry counts) and the relevant trustline entries.  Without this
//! store, every such load triggers an O(log n) bucket list scan.  The store
//! provides O(1) lookups for all ~150K sellers on mainnet.

use std::collections::{HashMap, HashSet};
use stellar_xdr::curr::{
    AccountId, Asset, LedgerEntry, LedgerEntryData, LedgerKey, LedgerKeyAccount,
    LedgerKeyTrustLine, Limits, TrustLineAsset, WriteXdr,
};

/// Per-seller data: account entry + trustline entries for their offer assets.
struct SellerData {
    /// The seller's account entry (None if not yet loaded).
    account: Option<LedgerEntry>,
    /// Trustline entries keyed by XDR-serialized `TrustLineAsset`.
    trustlines: HashMap<Vec<u8>, LedgerEntry>,
    /// Number of live offers for this seller.  When this reaches zero the
    /// seller is evicted from the store.
    offer_count: u32,
}

/// In-memory store of offer-seller accounts and trustlines.
///
/// See [module docs](self) for design rationale.
pub struct OfferSellerStore {
    sellers: HashMap<[u8; 32], SellerData>,
}

impl OfferSellerStore {
    /// Create an empty store.
    pub fn new() -> Self {
        Self {
            sellers: HashMap::new(),
        }
    }

    /// Build the store from an existing offer collection and batch-loaded entries.
    ///
    /// `offers` is the in-memory offer store (offer_id → LedgerEntry).
    /// `entries` are the batch-loaded account and trustline entries for sellers.
    pub fn build(offers: &HashMap<i64, LedgerEntry>, entries: Vec<LedgerEntry>) -> Self {
        // Pass 1: count offers per seller
        let mut seller_counts: HashMap<[u8; 32], u32> = HashMap::new();
        for entry in offers.values() {
            if let LedgerEntryData::Offer(offer) = &entry.data {
                let seller = account_id_bytes(&offer.seller_id);
                *seller_counts.entry(seller).or_insert(0) += 1;
            }
        }

        // Initialise SellerData with counts
        let mut sellers: HashMap<[u8; 32], SellerData> = seller_counts
            .into_iter()
            .map(|(id, count)| {
                (
                    id,
                    SellerData {
                        account: None,
                        trustlines: HashMap::new(),
                        offer_count: count,
                    },
                )
            })
            .collect();

        // Pass 2: install batch-loaded entries
        for entry in entries {
            match &entry.data {
                LedgerEntryData::Account(acct) => {
                    let seller = account_id_bytes(&acct.account_id);
                    if let Some(data) = sellers.get_mut(&seller) {
                        data.account = Some(entry);
                    }
                }
                LedgerEntryData::Trustline(tl) => {
                    let seller = account_id_bytes(&tl.account_id);
                    if let Some(data) = sellers.get_mut(&seller) {
                        if let Ok(key_bytes) = tl.asset.to_xdr(Limits::none()) {
                            data.trustlines.insert(key_bytes, entry);
                        }
                    }
                }
                _ => {}
            }
        }

        Self { sellers }
    }

    /// Number of tracked sellers.
    pub fn seller_count(&self) -> usize {
        self.sellers.len()
    }

    /// Total number of cached entries (accounts + trustlines).
    pub fn entry_count(&self) -> usize {
        self.sellers
            .values()
            .map(|d| d.account.is_some() as usize + d.trustlines.len())
            .sum()
    }

    /// Check whether `account_id` is a known offer seller.
    pub fn is_seller(&self, account_id: &AccountId) -> bool {
        self.sellers.contains_key(&account_id_bytes(account_id))
    }

    /// Look up a seller's account entry.
    pub fn get_account(&self, account_id: &AccountId) -> Option<&LedgerEntry> {
        self.sellers
            .get(&account_id_bytes(account_id))
            .and_then(|d| d.account.as_ref())
    }

    /// Look up a seller's trustline entry for a given asset.
    pub fn get_trustline(
        &self,
        account_id: &AccountId,
        asset: &TrustLineAsset,
    ) -> Option<&LedgerEntry> {
        let seller = account_id_bytes(account_id);
        let data = self.sellers.get(&seller)?;
        let key_bytes = asset.to_xdr(Limits::none()).ok()?;
        data.trustlines.get(&key_bytes)
    }

    // ------------------------------------------------------------------
    // Incremental update (called from commit_close)
    // ------------------------------------------------------------------

    /// Register new offers, updating seller membership and offer counts.
    pub fn add_offers(&mut self, offers: &[&LedgerEntry]) {
        for entry in offers {
            if let LedgerEntryData::Offer(offer) = &entry.data {
                let seller = account_id_bytes(&offer.seller_id);
                self.sellers
                    .entry(seller)
                    .or_insert_with(|| SellerData {
                        account: None,
                        trustlines: HashMap::new(),
                        offer_count: 0,
                    })
                    .offer_count += 1;
            }
        }
    }

    /// Remove deleted offers.  Evicts the seller entirely when their last
    /// offer is deleted.
    pub fn remove_offers(&mut self, offer_entries: &[&LedgerEntry]) {
        for entry in offer_entries {
            if let LedgerEntryData::Offer(offer) = &entry.data {
                let seller = account_id_bytes(&offer.seller_id);
                let mut remove = false;
                if let Some(data) = self.sellers.get_mut(&seller) {
                    data.offer_count = data.offer_count.saturating_sub(1);
                    if data.offer_count == 0 {
                        remove = true;
                    }
                }
                if remove {
                    self.sellers.remove(&seller);
                }
            }
        }
    }

    /// Update an account entry if the account is a known seller.
    pub fn update_account(&mut self, account_id: &AccountId, entry: &LedgerEntry) {
        let seller = account_id_bytes(account_id);
        if let Some(data) = self.sellers.get_mut(&seller) {
            data.account = Some(entry.clone());
        }
    }

    /// Remove a deleted account entry from the store.
    pub fn remove_account(&mut self, account_id: &AccountId) {
        let seller = account_id_bytes(account_id);
        if let Some(data) = self.sellers.get_mut(&seller) {
            data.account = None;
        }
    }

    /// Update a trustline entry if the account is a known seller.
    pub fn update_trustline(
        &mut self,
        account_id: &AccountId,
        asset: &TrustLineAsset,
        entry: &LedgerEntry,
    ) {
        let seller = account_id_bytes(account_id);
        if let Some(data) = self.sellers.get_mut(&seller) {
            if let Ok(key_bytes) = asset.to_xdr(Limits::none()) {
                data.trustlines.insert(key_bytes, entry.clone());
            }
        }
    }

    /// Remove a deleted trustline entry from the store.
    pub fn remove_trustline(&mut self, account_id: &AccountId, asset: &TrustLineAsset) {
        let seller = account_id_bytes(account_id);
        if let Some(data) = self.sellers.get_mut(&seller) {
            if let Ok(key_bytes) = asset.to_xdr(Limits::none()) {
                data.trustlines.remove(&key_bytes);
            }
        }
    }
}

impl Default for OfferSellerStore {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for OfferSellerStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OfferSellerStore")
            .field("sellers", &self.seller_count())
            .field("entries", &self.entry_count())
            .finish()
    }
}

// ------------------------------------------------------------------
// Helpers
// ------------------------------------------------------------------

/// Extract the selling keys needed for a batch bucket-list lookup.
///
/// Returns `(account_keys, trustline_keys)`.  Native-asset trustlines are
/// omitted because they don't exist.
pub fn seller_keys_from_offers(
    offers: &HashMap<i64, LedgerEntry>,
) -> (Vec<LedgerKey>, Vec<LedgerKey>) {
    let mut seller_ids: HashSet<[u8; 32]> = HashSet::new();
    let mut trustline_keys_set: HashSet<Vec<u8>> = HashSet::new();
    let mut account_keys = Vec::new();
    let mut trustline_keys = Vec::new();

    for entry in offers.values() {
        if let LedgerEntryData::Offer(offer) = &entry.data {
            let seller = account_id_bytes(&offer.seller_id);
            if seller_ids.insert(seller) {
                account_keys.push(LedgerKey::Account(LedgerKeyAccount {
                    account_id: offer.seller_id.clone(),
                }));
            }
            // Trustline for selling asset (if non-native)
            if let Some(tl_asset) = asset_to_trustline_asset(&offer.selling) {
                let tl_key = LedgerKey::Trustline(LedgerKeyTrustLine {
                    account_id: offer.seller_id.clone(),
                    asset: tl_asset,
                });
                if let Ok(bytes) = tl_key.to_xdr(Limits::none()) {
                    if trustline_keys_set.insert(bytes) {
                        trustline_keys.push(tl_key);
                    }
                }
            }
            // Trustline for buying asset (if non-native)
            if let Some(tl_asset) = asset_to_trustline_asset(&offer.buying) {
                let tl_key = LedgerKey::Trustline(LedgerKeyTrustLine {
                    account_id: offer.seller_id.clone(),
                    asset: tl_asset,
                });
                if let Ok(bytes) = tl_key.to_xdr(Limits::none()) {
                    if trustline_keys_set.insert(bytes) {
                        trustline_keys.push(tl_key);
                    }
                }
            }
        }
    }

    (account_keys, trustline_keys)
}

/// Convert an `Asset` to the corresponding `TrustLineAsset`.
/// Returns `None` for native (XLM) since native has no trustline.
fn asset_to_trustline_asset(asset: &Asset) -> Option<TrustLineAsset> {
    match asset {
        Asset::Native => None,
        Asset::CreditAlphanum4(a) => Some(TrustLineAsset::CreditAlphanum4(a.clone())),
        Asset::CreditAlphanum12(a) => Some(TrustLineAsset::CreditAlphanum12(a.clone())),
    }
}

/// Extract 32-byte public key from an AccountId.
fn account_id_bytes(account_id: &AccountId) -> [u8; 32] {
    match &account_id.0 {
        stellar_xdr::curr::PublicKey::PublicKeyTypeEd25519(k) => k.0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use stellar_xdr::curr::*;

    fn make_account_id(byte: u8) -> AccountId {
        AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([byte; 32])))
    }

    fn make_account_entry(byte: u8) -> LedgerEntry {
        LedgerEntry {
            last_modified_ledger_seq: 1,
            data: LedgerEntryData::Account(AccountEntry {
                account_id: make_account_id(byte),
                balance: 100,
                seq_num: SequenceNumber(1),
                num_sub_entries: 0,
                inflation_dest: None,
                flags: 0,
                home_domain: String32::default(),
                thresholds: Thresholds([1, 0, 0, 0]),
                signers: vec![].try_into().unwrap(),
                ext: AccountEntryExt::V0,
            }),
            ext: LedgerEntryExt::V0,
        }
    }

    fn make_trustline_entry(account_byte: u8, code: &[u8; 4]) -> LedgerEntry {
        LedgerEntry {
            last_modified_ledger_seq: 1,
            data: LedgerEntryData::Trustline(TrustLineEntry {
                account_id: make_account_id(account_byte),
                asset: TrustLineAsset::CreditAlphanum4(AlphaNum4 {
                    asset_code: AssetCode4(*code),
                    issuer: make_account_id(0),
                }),
                balance: 1000,
                limit: 10000,
                flags: TrustLineFlags::AuthorizedFlag as u32,
                ext: TrustLineEntryExt::V0,
            }),
            ext: LedgerEntryExt::V0,
        }
    }

    fn make_offer_entry(seller_byte: u8, offer_id: i64) -> LedgerEntry {
        LedgerEntry {
            last_modified_ledger_seq: 1,
            data: LedgerEntryData::Offer(OfferEntry {
                seller_id: make_account_id(seller_byte),
                offer_id,
                selling: Asset::CreditAlphanum4(AlphaNum4 {
                    asset_code: AssetCode4(*b"USD\0"),
                    issuer: make_account_id(0),
                }),
                buying: Asset::Native,
                amount: 100,
                price: Price { n: 1, d: 1 },
                flags: 0,
                ext: OfferEntryExt::V0,
            }),
            ext: LedgerEntryExt::V0,
        }
    }

    #[test]
    fn test_build_from_offers_and_entries() {
        let mut offers = HashMap::new();
        offers.insert(1, make_offer_entry(1, 1));
        offers.insert(2, make_offer_entry(2, 2));
        offers.insert(3, make_offer_entry(1, 3)); // same seller as offer 1

        let entries = vec![
            make_account_entry(1),
            make_account_entry(2),
            make_trustline_entry(1, b"USD\0"),
            make_trustline_entry(2, b"USD\0"),
        ];

        let store = OfferSellerStore::build(&offers, entries);
        assert_eq!(store.seller_count(), 2);

        // Seller 1: has account + trustline
        let id1 = make_account_id(1);
        assert!(store.is_seller(&id1));
        assert!(store.get_account(&id1).is_some());
        let asset = TrustLineAsset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4(*b"USD\0"),
            issuer: make_account_id(0),
        });
        assert!(store.get_trustline(&id1, &asset).is_some());

        // Non-seller: not found
        let id3 = make_account_id(3);
        assert!(!store.is_seller(&id3));
        assert!(store.get_account(&id3).is_none());
    }

    #[test]
    fn test_add_and_remove_offers() {
        let mut offers = HashMap::new();
        offers.insert(1, make_offer_entry(1, 1));
        let store_entries = vec![make_account_entry(1)];
        let mut store = OfferSellerStore::build(&offers, store_entries);
        assert_eq!(store.seller_count(), 1);

        // Add a second offer for the same seller
        let new_offer = make_offer_entry(1, 2);
        store.add_offers(&[&new_offer]);
        assert_eq!(store.seller_count(), 1); // still 1 seller

        // Remove both offers — seller should be evicted
        let offer1 = make_offer_entry(1, 1);
        let offer2 = make_offer_entry(1, 2);
        store.remove_offers(&[&offer1, &offer2]);
        assert_eq!(store.seller_count(), 0);
    }

    #[test]
    fn test_update_account_and_trustline() {
        let mut offers = HashMap::new();
        offers.insert(1, make_offer_entry(1, 1));
        let entries = vec![make_account_entry(1)];
        let mut store = OfferSellerStore::build(&offers, entries);

        let id1 = make_account_id(1);

        // Update account with new balance
        let mut updated = make_account_entry(1);
        if let LedgerEntryData::Account(ref mut acct) = updated.data {
            acct.balance = 999;
        }
        store.update_account(&id1, &updated);
        let stored = store.get_account(&id1).unwrap();
        if let LedgerEntryData::Account(acct) = &stored.data {
            assert_eq!(acct.balance, 999);
        }

        // Update trustline
        let asset = TrustLineAsset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4(*b"USD\0"),
            issuer: make_account_id(0),
        });
        let tl = make_trustline_entry(1, b"USD\0");
        store.update_trustline(&id1, &asset, &tl);
        assert!(store.get_trustline(&id1, &asset).is_some());
    }

    #[test]
    fn test_seller_keys_from_offers() {
        let mut offers = HashMap::new();
        offers.insert(1, make_offer_entry(1, 1));
        offers.insert(2, make_offer_entry(2, 2));
        offers.insert(3, make_offer_entry(1, 3)); // same seller

        let (acct_keys, tl_keys) = seller_keys_from_offers(&offers);
        assert_eq!(acct_keys.len(), 2); // 2 unique sellers
        // Selling asset is USD (non-native) → trustline keys exist
        // Buying asset is Native → no trustline key
        assert_eq!(tl_keys.len(), 2); // 1 trustline per seller
    }

    #[test]
    fn test_nonexistent_seller_lookups_return_none() {
        let store = OfferSellerStore::new();
        let id = make_account_id(99);
        assert!(!store.is_seller(&id));
        assert!(store.get_account(&id).is_none());
        let asset = TrustLineAsset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4(*b"ETH\0"),
            issuer: make_account_id(0),
        });
        assert!(store.get_trustline(&id, &asset).is_none());
    }
}
