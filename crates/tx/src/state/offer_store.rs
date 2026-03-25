//! Unified offer store: canonical data + all indexes + metadata.
//!
//! Owned by `LedgerManager`, shared with `LedgerStateManager` during execution
//! via `Arc<Mutex<OfferStore>>`.
//!
//! This eliminates the ~1 GB offer duplication that previously existed between
//! `LedgerManager::offer_store` + `offer_account_asset_index` and the executor's
//! `LedgerStateManager::offers` + `offer_index` + metadata maps.

use std::collections::{HashMap, HashSet};

use super::offer_index::{OfferIndex, OfferKey};
use super::{asset_to_trustline_asset, TrustlineKey};
use stellar_xdr::curr::{
    AccountId, Asset, LedgerEntry, LedgerEntryData, LedgerEntryExt, LedgerEntryExtensionV1,
    LedgerEntryExtensionV1Ext, OfferEntry, SponsorshipDescriptor,
};

/// All data for a single offer, stored inline to avoid separate metadata maps.
#[derive(Debug, Clone)]
pub struct OfferRecord {
    /// The offer entry itself.
    pub entry: OfferEntry,
    /// Last modified ledger sequence.
    pub last_modified: u32,
    /// Sponsoring account (if sponsored).
    pub sponsor: Option<AccountId>,
    /// Whether the offer has a sponsorship extension (V1 ext).
    pub has_ext: bool,
}

impl OfferRecord {
    /// Create a new OfferRecord from a LedgerEntry.
    ///
    /// Extracts the OfferEntry, last_modified, and sponsorship metadata.
    /// Panics if the entry is not an Offer.
    pub fn from_ledger_entry(entry: &LedgerEntry) -> Self {
        let offer = match &entry.data {
            LedgerEntryData::Offer(offer) => offer.clone(),
            _ => panic!("OfferRecord::from_ledger_entry called with non-offer entry"),
        };
        let (sponsor, has_ext) = match &entry.ext {
            LedgerEntryExt::V0 => (None, false),
            LedgerEntryExt::V1(ext) => (ext.sponsoring_id.0.clone(), true),
        };
        Self {
            entry: offer,
            last_modified: entry.last_modified_ledger_seq,
            sponsor,
            has_ext,
        }
    }

    /// Convert this record to a full LedgerEntry.
    pub fn to_ledger_entry(&self) -> LedgerEntry {
        let ext = if self.has_ext || self.sponsor.is_some() {
            LedgerEntryExt::V1(LedgerEntryExtensionV1 {
                sponsoring_id: SponsorshipDescriptor(self.sponsor.clone()),
                ext: LedgerEntryExtensionV1Ext::V0,
            })
        } else {
            LedgerEntryExt::V0
        };
        LedgerEntry {
            last_modified_ledger_seq: self.last_modified,
            data: LedgerEntryData::Offer(self.entry.clone()),
            ext,
        }
    }

    /// Get the OfferKey for this record.
    pub fn key(&self) -> OfferKey {
        OfferKey::from_offer(&self.entry)
    }
}

/// Unified offer store: canonical data + all indexes + metadata.
///
/// Replaces both `LedgerManager::offer_store` + `offer_account_asset_index` and
/// `LedgerStateManager::offers` + `offer_index` + metadata maps.
///
/// Owned by LedgerManager, shared with executor via `Arc<Mutex<OfferStore>>`.
pub struct OfferStore {
    /// Canonical offer data keyed by (seller, offer_id).
    offers: HashMap<OfferKey, OfferRecord>,
    /// Order book index for best-offer lookups (path payments, manage offer).
    order_book: OfferIndex,
    /// Secondary index: (account, asset) → set of offer_ids.
    /// Each offer is indexed under both (seller, selling_asset) and (seller, buying_asset).
    account_asset_index: HashMap<TrustlineKey, HashSet<i64>>,
    /// By offer_id for LedgerEntry lookups (verify-execution, snapshot closures).
    by_id: HashMap<i64, OfferKey>,
}

impl OfferStore {
    /// Create a new empty OfferStore.
    pub fn new() -> Self {
        Self {
            offers: HashMap::new(),
            order_book: OfferIndex::new(),
            account_asset_index: HashMap::new(),
            by_id: HashMap::new(),
        }
    }

    /// Populate the store from bucket list entries.
    ///
    /// Accepts an iterator of (offer_id, LedgerEntry) pairs.
    pub fn from_bucket_list_entries(entries: HashMap<i64, LedgerEntry>) -> Self {
        let mut store = Self {
            offers: HashMap::with_capacity(entries.len()),
            order_book: OfferIndex::new(),
            account_asset_index: HashMap::new(),
            by_id: HashMap::with_capacity(entries.len()),
        };
        for (offer_id, entry) in entries {
            let record = OfferRecord::from_ledger_entry(&entry);
            let key = record.key();
            store.order_book.add_offer(&record.entry);
            aa_index_insert(&mut store.account_asset_index, &record.entry);
            store.by_id.insert(offer_id, key.clone());
            store.offers.insert(key, record);
        }
        store
    }

    // ==================== Read Operations ====================

    /// Get an offer record by key.
    pub fn get(&self, key: &OfferKey) -> Option<&OfferRecord> {
        self.offers.get(key)
    }

    /// Get a mutable offer record by key.
    pub fn get_mut(&mut self, key: &OfferKey) -> Option<&mut OfferRecord> {
        self.offers.get_mut(key)
    }

    /// Get an offer entry by key (convenience).
    pub fn get_offer(&self, key: &OfferKey) -> Option<&OfferEntry> {
        self.offers.get(key).map(|r| &r.entry)
    }

    /// Get an offer by seller and offer_id.
    pub fn get_by_seller(&self, seller_id: &AccountId, offer_id: i64) -> Option<&OfferRecord> {
        self.offers.get(&OfferKey::new(seller_id.clone(), offer_id))
    }

    /// Get a mutable offer by seller and offer_id.
    pub fn get_by_seller_mut(
        &mut self,
        seller_id: &AccountId,
        offer_id: i64,
    ) -> Option<&mut OfferRecord> {
        self.offers
            .get_mut(&OfferKey::new(seller_id.clone(), offer_id))
    }

    /// Get an offer record by offer_id (for verify-execution).
    pub fn get_by_id(&self, offer_id: i64) -> Option<&OfferRecord> {
        self.by_id
            .get(&offer_id)
            .and_then(|key| self.offers.get(key))
    }

    /// Get a LedgerEntry by offer_id (for verify-execution / snapshot closures).
    pub fn get_ledger_entry_by_id(&self, offer_id: i64) -> Option<LedgerEntry> {
        self.get_by_id(offer_id).map(|r| r.to_ledger_entry())
    }

    /// Check if an offer exists.
    pub fn contains_key(&self, key: &OfferKey) -> bool {
        self.offers.contains_key(key)
    }

    /// Number of offers.
    pub fn len(&self) -> usize {
        self.offers.len()
    }

    /// Check if empty.
    pub fn is_empty(&self) -> bool {
        self.offers.is_empty()
    }

    // ==================== Index Access ====================

    /// Get the order book index (read-only).
    pub fn order_book(&self) -> &OfferIndex {
        &self.order_book
    }

    /// Get the order book index (mutable).
    pub fn order_book_mut(&mut self) -> &mut OfferIndex {
        &mut self.order_book
    }

    /// Get the (account, asset) secondary index (read-only).
    pub fn account_asset_index(&self) -> &HashMap<TrustlineKey, HashSet<i64>> {
        &self.account_asset_index
    }

    // ==================== Write Operations ====================

    /// Insert or update an offer from a LedgerEntry.
    ///
    /// Maintains all indexes.
    pub fn insert_from_ledger_entry(&mut self, entry: &LedgerEntry) {
        let record = OfferRecord::from_ledger_entry(entry);
        self.insert_record(record);
    }

    /// Insert an OfferRecord, maintaining all indexes.
    pub fn insert_record(&mut self, record: OfferRecord) {
        let key = record.key();
        let offer_id = record.entry.offer_id;

        // Remove old index entries if updating
        if let Some(old) = self.offers.get(&key) {
            aa_index_remove(&mut self.account_asset_index, &old.entry);
            self.order_book.remove_with_data(&old.entry);
        }

        // Add to indexes
        self.order_book.add_offer(&record.entry);
        aa_index_insert(&mut self.account_asset_index, &record.entry);
        self.by_id.insert(offer_id, key.clone());
        self.offers.insert(key, record);
    }

    /// Insert an offer entry with metadata.
    pub fn insert(
        &mut self,
        entry: OfferEntry,
        last_modified: u32,
        sponsor: Option<AccountId>,
        has_ext: bool,
    ) {
        self.insert_record(OfferRecord {
            entry,
            last_modified,
            sponsor,
            has_ext,
        });
    }

    /// Update an existing offer entry in place.
    ///
    /// Updates the offer data and all indexes. The metadata (last_modified, sponsor, has_ext)
    /// is NOT changed — the caller should update those separately if needed.
    pub fn update_offer_entry(&mut self, entry: OfferEntry) {
        let key = OfferKey::from_offer(&entry);
        let old_entry = self.offers.get(&key).map(|r| r.entry.clone());

        if let Some(ref old) = old_entry {
            aa_index_remove(&mut self.account_asset_index, old);
            self.order_book.update_offer(old, &entry);
        }
        aa_index_insert(&mut self.account_asset_index, &entry);

        if let Some(record) = self.offers.get_mut(&key) {
            record.entry = entry;
        }
    }

    /// Remove an offer by key.
    ///
    /// Returns the removed record if it existed.
    pub fn remove(&mut self, key: &OfferKey) -> Option<OfferRecord> {
        if let Some(record) = self.offers.remove(key) {
            self.order_book.remove_with_data(&record.entry);
            aa_index_remove(&mut self.account_asset_index, &record.entry);
            self.by_id.remove(&record.entry.offer_id);
            Some(record)
        } else {
            None
        }
    }

    /// Remove an offer by seller and offer_id.
    pub fn remove_by_seller(
        &mut self,
        seller_id: &AccountId,
        offer_id: i64,
    ) -> Option<OfferRecord> {
        let key = OfferKey::new(seller_id.clone(), offer_id);
        self.remove(&key)
    }

    // ==================== Order Book Queries ====================

    /// Best offer for an asset pair.
    pub fn best_offer(&self, buying: &Asset, selling: &Asset) -> Option<&OfferEntry> {
        self.order_book
            .best_offer_key(buying, selling)
            .and_then(|key| self.get_offer(&key))
    }

    /// Best offer with a filter predicate.
    pub fn best_offer_filtered<F>(
        &self,
        buying: &Asset,
        selling: &Asset,
        mut keep: F,
    ) -> Option<OfferEntry>
    where
        F: FnMut(&OfferEntry) -> bool,
    {
        for offer_key in self.order_book.offers_for_pair(buying, selling) {
            if let Some(record) = self.offers.get(offer_key) {
                if keep(&record.entry) {
                    return Some(record.entry.clone());
                }
            }
        }
        None
    }

    /// Top N offer keys for an asset pair.
    pub fn top_n_offer_keys(&self, buying: &Asset, selling: &Asset, n: usize) -> Vec<OfferKey> {
        self.order_book.top_n_offer_keys(buying, selling, n)
    }

    /// Check if offers exist for a pair.
    pub fn has_offers_for_pair(&self, buying: &Asset, selling: &Asset) -> bool {
        self.order_book.has_offers(buying, selling)
    }

    /// Get all offers for an asset pair in price order.
    pub fn offers_for_asset_pair(&self, buying: &Asset, selling: &Asset) -> Vec<OfferEntry> {
        self.order_book
            .offers_for_pair(buying, selling)
            .filter_map(|key| self.get_offer(key).cloned())
            .collect()
    }

    // ==================== Account+Asset Queries ====================

    /// Get all offers by account and asset (from secondary index).
    pub fn get_offers_by_account_and_asset(
        &self,
        account_id: &AccountId,
        asset: &Asset,
    ) -> Vec<OfferEntry> {
        let asset_key = asset_to_trustline_asset(asset);
        self.account_asset_index
            .get(&(account_id.clone(), asset_key))
            .map(|ids| {
                ids.iter()
                    .filter_map(|&id| {
                        self.offers
                            .get(&OfferKey::new(account_id.clone(), id))
                            .map(|r| r.entry.clone())
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    // ==================== Bulk Operations ====================

    /// Get all offers as LedgerEntry values (for snapshot closures).
    pub fn all_ledger_entries(&self) -> Vec<LedgerEntry> {
        self.offers.values().map(|r| r.to_ledger_entry()).collect()
    }

    /// Get offers by account and asset as LedgerEntry values (for snapshot closures).
    pub fn offers_by_account_and_asset_as_entries(
        &self,
        account_id: &AccountId,
        asset: &Asset,
    ) -> Vec<LedgerEntry> {
        let asset_key = asset_to_trustline_asset(asset);
        self.account_asset_index
            .get(&(account_id.clone(), asset_key))
            .map(|ids| {
                ids.iter()
                    .filter_map(|&id| {
                        self.offers
                            .get(&OfferKey::new(account_id.clone(), id))
                            .map(|r| r.to_ledger_entry())
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    // ==================== Memory Estimation ====================

    /// Estimate total heap bytes used by this OfferStore.
    pub fn estimate_heap_bytes(&self) -> usize {
        use henyey_common::memory::hashmap_heap_bytes;

        let offer_key_size = 44; // (AccountId, i64)
        let offer_record_size = 280; // OfferEntry + metadata
        let asset_pair_size = 120;
        let trustline_key_size = 100;

        // Main offers map
        let offers = hashmap_heap_bytes(self.offers.capacity(), offer_key_size, offer_record_size);

        // Order book index
        let order_books =
            hashmap_heap_bytes(self.order_book.order_book_capacity(), asset_pair_size, 200);

        // Account-asset secondary index
        let aa_index =
            hashmap_heap_bytes(self.account_asset_index.capacity(), trustline_key_size, 64);

        // by_id index
        let by_id = hashmap_heap_bytes(
            self.by_id.capacity(),
            std::mem::size_of::<i64>(),
            offer_key_size,
        );

        offers + order_books + aa_index + by_id
    }

    /// Number of unique asset pairs with offers.
    pub fn num_asset_pairs(&self) -> usize {
        self.order_book.num_asset_pairs()
    }

    /// Offer index size.
    pub fn offer_index_size(&self) -> usize {
        self.order_book.len()
    }
}

impl Default for OfferStore {
    fn default() -> Self {
        Self::new()
    }
}

// ==================== Index Helper Functions ====================

/// Insert an offer into the (account, asset) secondary index.
fn aa_index_insert(index: &mut HashMap<TrustlineKey, HashSet<i64>>, offer: &OfferEntry) {
    let seller = offer.seller_id.clone();
    let selling_key = asset_to_trustline_asset(&offer.selling);
    let buying_key = asset_to_trustline_asset(&offer.buying);
    index
        .entry((seller.clone(), selling_key))
        .or_default()
        .insert(offer.offer_id);
    index
        .entry((seller, buying_key))
        .or_default()
        .insert(offer.offer_id);
}

/// Remove an offer from the (account, asset) secondary index.
fn aa_index_remove(index: &mut HashMap<TrustlineKey, HashSet<i64>>, offer: &OfferEntry) {
    let seller = offer.seller_id.clone();
    let selling_key = asset_to_trustline_asset(&offer.selling);
    let buying_key = asset_to_trustline_asset(&offer.buying);
    if let Some(set) = index.get_mut(&(seller.clone(), selling_key)) {
        set.remove(&offer.offer_id);
    }
    if let Some(set) = index.get_mut(&(seller, buying_key)) {
        set.remove(&offer.offer_id);
    }
}
