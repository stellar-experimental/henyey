//! Unified offer store: canonical data + all indexes + metadata.
//!
//! Owned by `LedgerManager`, shared with `LedgerStateManager` during execution
//! via `Arc<Mutex<OfferStore>>`.
//!
//! This eliminates the ~1 GB offer duplication that previously existed between
//! `LedgerManager::offer_store` + `offer_account_asset_index` and the executor's
//! `LedgerStateManager::offers` + `offer_index` + metadata maps.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use super::offer_index::{OfferIndex, OfferKey};
use super::{asset_to_trustline_asset, TrustlineKey};
use stellar_xdr::{
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
#[derive(Default)]
struct OfferStoreData {
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

/// Immutable offer data shared by isolated simulation stores.
pub struct ImmutableOfferStoreBase(OfferStoreData);

/// Canonical owned offer store, or a private mutation overlay over an immutable base.
pub struct OfferStore {
    data: OfferStoreData,
    base: Option<Arc<ImmutableOfferStoreBase>>,
    /// Base keys hidden by updates and deletes in this overlay.
    shadowed: HashSet<OfferKey>,
}

impl OfferStore {
    /// Create a new empty OfferStore.
    pub fn new() -> Self {
        Self {
            data: OfferStoreData::default(),
            base: None,
            shadowed: HashSet::new(),
        }
    }

    /// Populate the store from bucket list entries.
    ///
    /// Accepts an iterator of (offer_id, LedgerEntry) pairs.
    pub fn from_bucket_list_entries(entries: HashMap<i64, LedgerEntry>) -> Self {
        let mut store = Self {
            data: OfferStoreData {
                offers: HashMap::with_capacity(entries.len()),
                order_book: OfferIndex::new(),
                account_asset_index: HashMap::new(),
                by_id: HashMap::with_capacity(entries.len()),
            },
            base: None,
            shadowed: HashSet::new(),
        };
        for (offer_id, entry) in entries {
            let record = OfferRecord::from_ledger_entry(&entry);
            let key = record.key();
            store.data.order_book.add_offer(&record.entry);
            aa_index_insert(&mut store.data.account_asset_index, &record.entry);
            store.data.by_id.insert(offer_id, key.clone());
            store.data.offers.insert(key, record);
        }
        store
    }

    /// Freeze this owned store for sharing by cheap private simulation overlays.
    pub fn into_immutable_base(self) -> Arc<ImmutableOfferStoreBase> {
        assert!(self.base.is_none(), "cannot freeze an offer overlay");
        Arc::new(ImmutableOfferStoreBase(self.data))
    }

    /// Create an empty private mutation overlay over `base`.
    pub fn with_immutable_base(base: Arc<ImmutableOfferStoreBase>) -> Self {
        Self {
            data: OfferStoreData::default(),
            base: Some(base),
            shadowed: HashSet::new(),
        }
    }

    /// Number of records privately inserted, updated, or deleted by this overlay.
    pub fn overlay_len(&self) -> usize {
        if self.base.is_some() {
            self.data.offers.len()
                + self
                    .shadowed
                    .iter()
                    .filter(|key| !self.data.offers.contains_key(*key))
                    .count()
        } else {
            0
        }
    }

    // ==================== Read Operations ====================

    /// Get an offer record by key.
    pub fn get(&self, key: &OfferKey) -> Option<&OfferRecord> {
        self.data.offers.get(key).or_else(|| {
            (!self.shadowed.contains(key))
                .then(|| self.base.as_ref()?.0.offers.get(key))
                .flatten()
        })
    }

    fn metadata_mut(&mut self, key: &OfferKey) -> Option<&mut OfferRecord> {
        if self.base.is_none() {
            return self.data.offers.get_mut(key);
        }
        if !self.data.offers.contains_key(key) {
            let record = self.get(key)?.clone();
            self.insert_record(record);
        }
        self.data.offers.get_mut(key)
    }

    /// Get an offer entry by key (convenience).
    pub fn get_offer(&self, key: &OfferKey) -> Option<&OfferEntry> {
        self.get(key).map(|r| &r.entry)
    }

    /// Get an offer by seller and offer_id.
    pub fn get_by_seller(&self, seller_id: &AccountId, offer_id: i64) -> Option<&OfferRecord> {
        self.get(&OfferKey::new(seller_id.clone(), offer_id))
    }

    /// Replace metadata without exposing indexed offer identity, pair, or descriptor fields.
    pub fn set_metadata(
        &mut self,
        key: &OfferKey,
        last_modified: u32,
        sponsor: Option<AccountId>,
        has_ext: bool,
    ) -> bool {
        let Some(record) = self.metadata_mut(key) else {
            return false;
        };
        record.last_modified = last_modified;
        record.sponsor = sponsor;
        record.has_ext = has_ext;
        true
    }

    pub fn set_last_modified(&mut self, key: &OfferKey, last_modified: u32) -> bool {
        let Some(record) = self.metadata_mut(key) else {
            return false;
        };
        record.last_modified = last_modified;
        true
    }

    pub fn set_sponsorship(
        &mut self,
        key: &OfferKey,
        sponsor: Option<AccountId>,
        has_ext: bool,
    ) -> bool {
        let Some(record) = self.metadata_mut(key) else {
            return false;
        };
        record.sponsor = sponsor;
        record.has_ext = has_ext;
        true
    }

    pub fn take_sponsorship(&mut self, key: &OfferKey) -> Option<Option<AccountId>> {
        self.metadata_mut(key).map(|record| record.sponsor.take())
    }

    pub fn set_has_ext(&mut self, key: &OfferKey, has_ext: bool) -> bool {
        let Some(record) = self.metadata_mut(key) else {
            return false;
        };
        record.has_ext = has_ext;
        true
    }

    pub fn clear_has_ext(&mut self, key: &OfferKey) -> Option<bool> {
        self.metadata_mut(key).map(|record| {
            let previous = record.has_ext;
            record.has_ext = false;
            previous
        })
    }

    /// Get an offer record by offer_id (for verify-execution).
    pub fn get_by_id(&self, offer_id: i64) -> Option<&OfferRecord> {
        self.data
            .by_id
            .get(&offer_id)
            .and_then(|key| self.data.offers.get(key))
            .or_else(|| {
                let key = self.base.as_ref()?.0.by_id.get(&offer_id)?;
                (!self.shadowed.contains(key))
                    .then(|| self.base.as_ref()?.0.offers.get(key))
                    .flatten()
            })
    }

    /// Get a LedgerEntry by offer_id (for verify-execution / snapshot closures).
    pub fn get_ledger_entry_by_id(&self, offer_id: i64) -> Option<LedgerEntry> {
        self.get_by_id(offer_id).map(|r| r.to_ledger_entry())
    }

    /// Check if an offer exists.
    pub fn contains_key(&self, key: &OfferKey) -> bool {
        self.get(key).is_some()
    }

    /// Number of offers.
    pub fn len(&self) -> usize {
        self.base.as_ref().map_or(self.data.offers.len(), |base| {
            base.0.offers.len()
                - self
                    .shadowed
                    .iter()
                    .filter(|key| base.0.offers.contains_key(*key))
                    .count()
                + self.data.offers.len()
        })
    }

    /// Check if empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    // ==================== Index Access ====================

    /// Get the order book index (read-only).
    pub fn order_book(&self) -> &OfferIndex {
        assert!(
            self.base.is_none(),
            "order_book is unavailable on an offer overlay"
        );
        &self.data.order_book
    }

    /// Get the order book index (mutable).
    pub fn order_book_mut(&mut self) -> &mut OfferIndex {
        assert!(
            self.base.is_none(),
            "order_book_mut is unavailable on an offer overlay"
        );
        &mut self.data.order_book
    }

    /// Get the (account, asset) secondary index (read-only).
    pub fn account_asset_index(&self) -> &HashMap<TrustlineKey, HashSet<i64>> {
        assert!(
            self.base.is_none(),
            "account_asset_index is unavailable on an offer overlay"
        );
        &self.data.account_asset_index
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
        if let Some(old) = self.data.offers.get(&key) {
            aa_index_remove(&mut self.data.account_asset_index, &old.entry);
            self.data.order_book.remove_with_data(&old.entry);
        }

        if self.base.is_some() {
            self.shadowed.insert(key.clone());
        }

        // Add to indexes
        self.data.order_book.add_offer(&record.entry);
        aa_index_insert(&mut self.data.account_asset_index, &record.entry);
        self.data.by_id.insert(offer_id, key.clone());
        self.data.offers.insert(key, record);
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
        if self.base.is_none() {
            let old_entry = self
                .data
                .offers
                .get(&key)
                .map(|record| record.entry.clone());
            if let Some(old) = &old_entry {
                aa_index_remove(&mut self.data.account_asset_index, old);
                self.data.order_book.update_offer(old, &entry);
            }
            aa_index_insert(&mut self.data.account_asset_index, &entry);
            if let Some(record) = self.data.offers.get_mut(&key) {
                record.entry = entry;
            }
            return;
        }
        if let Some(mut record) = self.get(&key).cloned() {
            record.entry = entry;
            self.insert_record(record);
        }
    }

    /// Remove an offer by key.
    ///
    /// Returns the removed record if it existed.
    pub fn remove(&mut self, key: &OfferKey) -> Option<OfferRecord> {
        if self.base.is_none() {
            let record = self.data.offers.remove(key)?;
            self.data.order_book.remove_with_data(&record.entry);
            aa_index_remove(&mut self.data.account_asset_index, &record.entry);
            self.data.by_id.remove(&record.entry.offer_id);
            return Some(record);
        }
        let record = self.get(key).cloned()?;
        if let Some(local) = self.data.offers.remove(key) {
            self.data.order_book.remove_with_data(&local.entry);
            aa_index_remove(&mut self.data.account_asset_index, &local.entry);
            self.data.by_id.remove(&local.entry.offer_id);
        }
        if self.base.is_some() {
            self.shadowed.insert(key.clone());
        }
        Some(record)
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
        if self.base.is_none() {
            return self
                .data
                .order_book
                .best_offer_key(buying, selling)
                .and_then(|key| self.data.offers.get(&key).map(|record| &record.entry));
        }
        let local = self
            .data
            .order_book
            .offers_for_pair(buying, selling)
            .find_map(|key| self.data.offers.get(key));
        let base = self.base.as_ref().and_then(|base| {
            base.0
                .order_book
                .offers_for_pair(buying, selling)
                .find(|key| !self.shadowed.contains(*key))
                .and_then(|key| base.0.offers.get(key))
        });
        match (local, base) {
            (Some(local), Some(base)) => {
                if super::offer_index::OfferDescriptor::from_offer(&local.entry)
                    <= super::offer_index::OfferDescriptor::from_offer(&base.entry)
                {
                    Some(&local.entry)
                } else {
                    Some(&base.entry)
                }
            }
            (Some(local), None) => Some(&local.entry),
            (None, Some(base)) => Some(&base.entry),
            (None, None) => None,
        }
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
        if self.base.is_none() {
            for key in self.data.order_book.offers_for_pair(buying, selling) {
                if let Some(record) = self.data.offers.get(key) {
                    if keep(&record.entry) {
                        return Some(record.entry.clone());
                    }
                }
            }
            return None;
        }
        self.offers_for_asset_pair(buying, selling)
            .into_iter()
            .find(|offer| keep(offer))
    }

    /// Top N offer keys for an asset pair.
    pub fn top_n_offer_keys(&self, buying: &Asset, selling: &Asset, n: usize) -> Vec<OfferKey> {
        if self.base.is_none() {
            return self.data.order_book.top_n_offer_keys(buying, selling, n);
        }
        self.offers_for_asset_pair(buying, selling)
            .into_iter()
            .take(n)
            .map(|offer| OfferKey::from_offer(&offer))
            .collect()
    }

    /// Check if offers exist for a pair.
    pub fn has_offers_for_pair(&self, buying: &Asset, selling: &Asset) -> bool {
        self.best_offer(buying, selling).is_some()
    }

    /// Get all offers for an asset pair in price order.
    pub fn offers_for_asset_pair(&self, buying: &Asset, selling: &Asset) -> Vec<OfferEntry> {
        if self.base.is_none() {
            return self
                .data
                .order_book
                .offers_for_pair(buying, selling)
                .filter_map(|key| self.data.offers.get(key).map(|record| record.entry.clone()))
                .collect();
        }
        let mut offers: Vec<OfferEntry> = self
            .base
            .as_ref()
            .into_iter()
            .flat_map(|base| {
                base.0
                    .order_book
                    .offers_for_pair(buying, selling)
                    .map(move |key| (base, key))
            })
            .filter(|(_, key)| !self.shadowed.contains(*key))
            .filter_map(|(base, key)| base.0.offers.get(key).map(|record| record.entry.clone()))
            .chain(
                self.data
                    .order_book
                    .offers_for_pair(buying, selling)
                    .filter_map(|key| self.data.offers.get(key).map(|record| record.entry.clone())),
            )
            .collect::<Vec<_>>();
        offers.sort_by_key(super::offer_index::OfferDescriptor::from_offer);
        offers
    }

    // ==================== Account+Asset Queries ====================

    /// Get all offers by account and asset (from secondary index).
    pub fn get_offers_by_account_and_asset(
        &self,
        account_id: &AccountId,
        asset: &Asset,
    ) -> Vec<OfferEntry> {
        let asset_key = asset_to_trustline_asset(asset);
        let mut offers: Vec<OfferEntry> = self
            .data
            .account_asset_index
            .get(&(account_id.clone(), asset_key))
            .map(|ids| {
                ids.iter()
                    .filter_map(|&id| {
                        self.data
                            .offers
                            .get(&OfferKey::new(account_id.clone(), id))
                            .map(|r| r.entry.clone())
                    })
                    .collect()
            })
            .unwrap_or_default();
        if let Some(base) = &self.base {
            let asset_key = asset_to_trustline_asset(asset);
            offers.extend(
                base.0
                    .account_asset_index
                    .get(&(account_id.clone(), asset_key))
                    .into_iter()
                    .flatten()
                    .filter_map(|id| {
                        let key = OfferKey::new(account_id.clone(), *id);
                        (!self.shadowed.contains(&key))
                            .then(|| base.0.offers.get(&key).map(|record| record.entry.clone()))
                            .flatten()
                    }),
            );
        }
        offers
    }

    // ==================== Bulk Operations ====================

    /// Get all offers as LedgerEntry values (for snapshot closures).
    pub fn all_ledger_entries(&self) -> Vec<LedgerEntry> {
        if self.base.is_none() {
            return self
                .data
                .offers
                .values()
                .map(OfferRecord::to_ledger_entry)
                .collect();
        }
        let mut entries = self
            .base
            .as_ref()
            .into_iter()
            .flat_map(|base| base.0.offers.iter())
            .filter(|(key, _)| !self.shadowed.contains(*key))
            .map(|(_, record)| record.to_ledger_entry())
            .collect::<Vec<_>>();
        entries.extend(self.data.offers.values().map(OfferRecord::to_ledger_entry));
        entries
    }

    /// Get offers by account and asset as LedgerEntry values (for snapshot closures).
    pub fn offers_by_account_and_asset_as_entries(
        &self,
        account_id: &AccountId,
        asset: &Asset,
    ) -> Vec<LedgerEntry> {
        if self.base.is_none() {
            let asset_key = asset_to_trustline_asset(asset);
            return self
                .data
                .account_asset_index
                .get(&(account_id.clone(), asset_key))
                .map(|ids| {
                    ids.iter()
                        .filter_map(|id| {
                            self.data
                                .offers
                                .get(&OfferKey::new(account_id.clone(), *id))
                                .map(OfferRecord::to_ledger_entry)
                        })
                        .collect()
                })
                .unwrap_or_default();
        }
        self.get_offers_by_account_and_asset(account_id, asset)
            .into_iter()
            .filter_map(|offer| {
                self.get(&OfferKey::from_offer(&offer))
                    .map(OfferRecord::to_ledger_entry)
            })
            .collect()
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
        let offers = hashmap_heap_bytes(
            self.data.offers.capacity(),
            offer_key_size,
            offer_record_size,
        );

        // Order book index
        let order_books = hashmap_heap_bytes(
            self.data.order_book.order_book_capacity(),
            asset_pair_size,
            200,
        );

        // Account-asset secondary index
        let aa_index = hashmap_heap_bytes(
            self.data.account_asset_index.capacity(),
            trustline_key_size,
            64,
        );

        // by_id index
        let by_id = hashmap_heap_bytes(
            self.data.by_id.capacity(),
            std::mem::size_of::<i64>(),
            offer_key_size,
        );

        offers + order_books + aa_index + by_id
    }

    /// Number of unique asset pairs with offers.
    pub fn num_asset_pairs(&self) -> usize {
        if self.base.is_some() {
            self.all_ledger_entries()
                .into_iter()
                .filter_map(|entry| match entry.data {
                    LedgerEntryData::Offer(offer) => Some((offer.buying, offer.selling)),
                    _ => None,
                })
                .collect::<HashSet<_>>()
                .len()
        } else {
            self.data.order_book.num_asset_pairs()
        }
    }

    /// Offer index size.
    pub fn offer_index_size(&self) -> usize {
        self.len()
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

#[cfg(test)]
mod tests {
    use super::*;
    use stellar_xdr::{AlphaNum4, AssetCode4, OfferEntryExt, Price, PublicKey, Uint256};

    fn account(seed: u8) -> AccountId {
        AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([seed; 32])))
    }

    fn usd() -> Asset {
        Asset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4(*b"USD\0"),
            issuer: account(9),
        })
    }

    fn record(id: i64, price: i32, amount: i64) -> OfferRecord {
        OfferRecord {
            entry: OfferEntry {
                seller_id: account(id as u8),
                offer_id: id,
                selling: usd(),
                buying: Asset::Native,
                amount,
                price: Price { n: price, d: 1 },
                flags: 0,
                ext: OfferEntryExt::V0,
            },
            last_modified: 41 + id as u32,
            sponsor: (id == 2).then(|| account(8)),
            has_ext: id == 2,
        }
    }

    fn base() -> Arc<ImmutableOfferStoreBase> {
        let mut store = OfferStore::new();
        store.insert_record(record(1, 1, 100));
        store.insert_record(record(2, 2, 200));
        store.into_immutable_base()
    }

    #[test]
    fn overlay_merges_updates_deletes_and_inserts_in_descriptor_order() {
        let base = base();
        let mut overlay = OfferStore::with_immutable_base(Arc::clone(&base));
        assert_eq!(
            overlay.best_offer(&Asset::Native, &usd()).unwrap().offer_id,
            1
        );

        let mut updated = overlay.get_by_id(1).unwrap().clone();
        updated.entry.price = Price { n: 3, d: 1 };
        overlay.insert_record(updated);
        overlay.insert_record(record(3, 1, 50));
        overlay.remove_by_seller(&account(2), 2).unwrap();

        assert_eq!(
            overlay
                .offers_for_asset_pair(&Asset::Native, &usd())
                .iter()
                .map(|offer| offer.offer_id)
                .collect::<Vec<_>>(),
            vec![3, 1]
        );
        assert_eq!(overlay.len(), 2);
        assert_eq!(overlay.overlay_len(), 3);

        let pristine = OfferStore::with_immutable_base(base);
        assert_eq!(
            pristine
                .offers_for_asset_pair(&Asset::Native, &usd())
                .iter()
                .map(|offer| offer.offer_id)
                .collect::<Vec<_>>(),
            vec![1, 2]
        );
    }

    #[test]
    fn overlay_mutable_metadata_is_private_and_preserved_as_ledger_entry() {
        let base = base();
        let mut first = OfferStore::with_immutable_base(Arc::clone(&base));
        assert!(first.set_metadata(&OfferKey::new(account(2), 2), 99, None, true));

        let entry = first.get_ledger_entry_by_id(2).unwrap();
        assert_eq!(entry.last_modified_ledger_seq, 99);
        assert!(matches!(entry.ext, LedgerEntryExt::V1(_)));
        assert_eq!(first.overlay_len(), 1);

        let second = OfferStore::with_immutable_base(base);
        let record = second.get_by_id(2).unwrap();
        assert_eq!(record.last_modified, 43);
        assert_eq!(record.sponsor, Some(account(8)));
        assert_eq!(second.overlay_len(), 0);
    }

    #[test]
    fn overlay_metadata_api_cannot_corrupt_offer_indexes() {
        let mut overlay = OfferStore::with_immutable_base(base());
        let before = overlay
            .offers_for_asset_pair(&Asset::Native, &usd())
            .into_iter()
            .map(|offer| offer.offer_id)
            .collect::<Vec<_>>();
        assert!(overlay.set_last_modified(&OfferKey::new(account(1), 1), 99));
        assert!(overlay.set_sponsorship(&OfferKey::new(account(1), 1), Some(account(7)), true));
        assert_eq!(
            overlay
                .offers_for_asset_pair(&Asset::Native, &usd())
                .into_iter()
                .map(|offer| offer.offer_id)
                .collect::<Vec<_>>(),
            before
        );
        assert_eq!(overlay.get_by_id(1).unwrap().entry, record(1, 1, 100).entry);
    }

    #[test]
    fn best_offer_equal_price_tie_prefers_lower_id_when_base_offer_is_older() {
        // Base holds id 1, overlay inserts id 3 at the same price:
        // the base offer (lower id) must win the tie.
        let mut store = OfferStore::new();
        store.insert_record(record(1, 1, 100));
        let mut overlay = OfferStore::with_immutable_base(store.into_immutable_base());
        overlay.insert_record(record(3, 1, 50));
        assert_eq!(
            overlay.best_offer(&Asset::Native, &usd()).unwrap().offer_id,
            1
        );
    }

    #[test]
    fn best_offer_equal_price_tie_prefers_lower_id_when_local_offer_is_older() {
        // Base holds id 3, overlay inserts id 1 at the same price:
        // the overlay-local offer (lower id) must win the tie.
        let mut store = OfferStore::new();
        store.insert_record(record(3, 1, 100));
        let mut overlay = OfferStore::with_immutable_base(store.into_immutable_base());
        overlay.insert_record(record(1, 1, 50));
        assert_eq!(
            overlay.best_offer(&Asset::Native, &usd()).unwrap().offer_id,
            1
        );
    }

    #[test]
    fn filtered_lookup_skips_shadowed_base_offer_and_uses_local_offer() {
        let base = base();
        let mut overlay = OfferStore::with_immutable_base(base);
        overlay.remove_by_seller(&account(1), 1).unwrap();
        let mut local = record(4, 1, 10);
        local.entry.seller_id = account(1);
        overlay.insert_record(local);

        assert_eq!(
            overlay
                .best_offer_filtered(&Asset::Native, &usd(), |offer| offer.seller_id
                    == account(1))
                .unwrap()
                .offer_id,
            4
        );
    }
}
