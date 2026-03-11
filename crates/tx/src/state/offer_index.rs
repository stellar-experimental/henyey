//! Order-book offer index for price-ordered offer traversal during DEX exchange.

use super::*;

/// A lightweight descriptor for an offer used in sorting and comparison.
///
/// This struct captures just the information needed to determine offer ordering:
/// the price and the offer ID. Offers are sorted by price (ascending) then
/// offer ID (ascending).
#[derive(Debug, Clone)]
pub struct OfferDescriptor {
    /// The price of the offer (n/d ratio).
    pub price: Price,
    /// The unique identifier of the offer.
    pub offer_id: i64,
}

impl OfferDescriptor {
    /// Create a new offer descriptor.
    pub fn new(price: Price, offer_id: i64) -> Self {
        Self { price, offer_id }
    }

    /// Create a new offer descriptor from an offer entry.
    pub fn from_offer(offer: &OfferEntry) -> Self {
        Self {
            price: offer.price.clone(),
            offer_id: offer.offer_id,
        }
    }

    /// Create an offer descriptor from an offer entry (alias for `from_offer`).
    pub fn from_offer_entry(offer: &OfferEntry) -> Self {
        Self::from_offer(offer)
    }

    /// Create an offer descriptor from a ledger entry containing an offer.
    ///
    /// # Panics
    ///
    /// Panics if the ledger entry does not contain an offer.
    pub fn from_ledger_entry(entry: &LedgerEntry) -> Self {
        match &entry.data {
            LedgerEntryData::Offer(offer) => Self::from_offer(offer),
            _ => panic!("Expected offer entry"),
        }
    }

    /// Calculate the price as a floating point value.
    ///
    /// This is used for comparison purposes only - not for actual calculations
    /// which should use the rational n/d form.
    #[inline]
    pub fn price_as_f64(&self) -> f64 {
        self.price.n as f64 / self.price.d as f64
    }
}

impl PartialEq for OfferDescriptor {
    fn eq(&self, other: &Self) -> bool {
        self.price.n == other.price.n
            && self.price.d == other.price.d
            && self.offer_id == other.offer_id
    }
}

impl Eq for OfferDescriptor {}

/// Comparator for offers: lower price is better, then lower offer ID.
impl Ord for OfferDescriptor {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // Use floating-point comparison to match stellar-core behavior.
        // The stellar-core code uses `double(price.n) / double(price.d)` for ordering.
        let self_price = self.price_as_f64();
        let other_price = other.price_as_f64();

        match self_price.partial_cmp(&other_price) {
            Some(std::cmp::Ordering::Equal) | None => self.offer_id.cmp(&other.offer_id),
            Some(ord) => ord,
        }
    }
}

impl PartialOrd for OfferDescriptor {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl std::hash::Hash for OfferDescriptor {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.price.n.hash(state);
        self.price.d.hash(state);
        self.offer_id.hash(state);
    }
}

/// Key for an offer in the primary offers map.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct OfferKey {
    /// Seller account ID.
    pub seller: AccountId,
    /// Offer ID.
    pub offer_id: i64,
}

impl OfferKey {
    /// Create a new offer key.
    pub fn new(seller: AccountId, offer_id: i64) -> Self {
        Self { seller, offer_id }
    }

    /// Create from an offer entry.
    pub fn from_offer(offer: &OfferEntry) -> Self {
        Self {
            seller: offer.seller_id.clone(),
            offer_id: offer.offer_id,
        }
    }
}

/// Asset pair key for order book lookup.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct AssetPair {
    /// Asset being bought.
    pub buying: TrustLineAsset,
    /// Asset being sold.
    pub selling: TrustLineAsset,
}

impl AssetPair {
    /// Create a new asset pair from XDR assets.
    pub fn new(buying: &Asset, selling: &Asset) -> Self {
        Self {
            buying: asset_to_trustline_asset(buying),
            selling: asset_to_trustline_asset(selling),
        }
    }
}

/// Order book for a single asset pair.
/// Offers are stored in a BTreeMap sorted by (price, offer_id) for O(log n) best offer lookup.
type OrderBook = BTreeMap<OfferDescriptor, OfferKey>;

/// Index of all offers organized by asset pair for efficient best-offer queries.
///
/// This mirrors stellar-core's MultiOrderBook structure. Each asset pair
/// has its own order book (BTreeMap) where offers are sorted by price and offer ID.
///
/// # Performance
///
/// - `best_offer`: O(log n) where n is offers for the asset pair
/// - `add_offer`: O(log n)
/// - `remove_offer`: O(log n)
/// - `update_offer`: O(log n) for same asset pair, O(log n + log m) if assets change
#[derive(Debug, Clone, Default)]
pub struct OfferIndex {
    /// Order books keyed by (buying, selling) asset pair.
    order_books: HashMap<AssetPair, OrderBook>,
    /// Reverse index: offer key -> (asset pair, descriptor) for efficient removal.
    offer_locations: HashMap<OfferKey, (AssetPair, OfferDescriptor)>,
}

impl OfferIndex {
    /// Create a new empty offer index.
    pub fn new() -> Self {
        Self {
            order_books: HashMap::new(),
            offer_locations: HashMap::new(),
        }
    }

    /// Add an offer to the index.
    pub fn add_offer(&mut self, offer: &OfferEntry) {
        let key = OfferKey::from_offer(offer);
        let descriptor = OfferDescriptor::from_offer(offer);
        let asset_pair = AssetPair::new(&offer.buying, &offer.selling);

        // Add to order book
        let order_book = self.order_books.entry(asset_pair.clone()).or_default();
        order_book.insert(descriptor.clone(), key.clone());

        // Add to reverse index
        self.offer_locations.insert(key, (asset_pair, descriptor));
    }

    /// Remove an offer from the index.
    pub fn remove_offer(&mut self, seller: &AccountId, offer_id: i64) {
        let key = OfferKey::new(seller.clone(), offer_id);
        self.remove_by_key(&key);
    }

    /// Remove an offer from the index by its key.
    pub fn remove_by_key(&mut self, key: &OfferKey) {
        // Look up location in reverse index
        if let Some((asset_pair, descriptor)) = self.offer_locations.remove(key) {
            // Remove from order book
            if let Some(order_book) = self.order_books.get_mut(&asset_pair) {
                order_book.remove(&descriptor);
                // Clean up empty order books
                if order_book.is_empty() {
                    self.order_books.remove(&asset_pair);
                }
            }
        }
    }

    /// Update an offer in the index.
    ///
    /// This handles the case where an offer's price or assets might change.
    pub fn update_offer(&mut self, offer: &OfferEntry) {
        // Remove old entry if exists
        self.remove_offer(&offer.seller_id, offer.offer_id);
        // Add with new values
        self.add_offer(offer);
    }

    /// Get the best (lowest price) offer for an asset pair.
    ///
    /// Returns the offer key if one exists.
    pub fn best_offer_key(&self, buying: &Asset, selling: &Asset) -> Option<OfferKey> {
        let asset_pair = AssetPair::new(buying, selling);
        self.order_books
            .get(&asset_pair)
            .and_then(|book| book.first_key_value())
            .map(|(_, key)| key.clone())
    }

    /// Get the best offer for an asset pair, excluding specific offers.
    ///
    /// This is used during offer crossing when we need to skip offers
    /// that have already been processed or belong to the same account.
    pub fn best_offer_key_filtered<F>(
        &self,
        buying: &Asset,
        selling: &Asset,
        mut filter: F,
    ) -> Option<OfferKey>
    where
        F: FnMut(&OfferKey) -> bool,
    {
        let asset_pair = AssetPair::new(buying, selling);
        self.order_books.get(&asset_pair).and_then(|book| {
            book.iter()
                .find(|(_, key)| filter(key))
                .map(|(_, key)| key.clone())
        })
    }

    /// Iterate over all offers for an asset pair in price order.
    pub fn offers_for_pair(
        &self,
        buying: &Asset,
        selling: &Asset,
    ) -> impl Iterator<Item = &OfferKey> {
        let asset_pair = AssetPair::new(buying, selling);
        self.order_books
            .get(&asset_pair)
            .into_iter()
            .flat_map(|book| book.values())
    }

    /// Get the top-N offer keys (cheapest first) for an asset pair.
    pub fn top_n_offer_keys(&self, buying: &Asset, selling: &Asset, n: usize) -> Vec<OfferKey> {
        let pair = AssetPair::new(buying, selling);
        self.order_books
            .get(&pair)
            .into_iter()
            .flat_map(|book| book.values().take(n).cloned())
            .collect()
    }

    /// Check if the index contains any offers for an asset pair.
    pub fn has_offers(&self, buying: &Asset, selling: &Asset) -> bool {
        let asset_pair = AssetPair::new(buying, selling);
        self.order_books
            .get(&asset_pair)
            .is_some_and(|book| !book.is_empty())
    }

    /// Get the total number of offers in the index.
    pub fn len(&self) -> usize {
        self.offer_locations.len()
    }

    /// Check if the index is empty.
    pub fn is_empty(&self) -> bool {
        self.offer_locations.is_empty()
    }

    /// Clear all offers from the index.
    pub fn clear(&mut self) {
        self.order_books.clear();
        self.offer_locations.clear();
    }

    /// Get the number of asset pairs with offers.
    pub fn num_asset_pairs(&self) -> usize {
        self.order_books.len()
    }

    /// Capacity of the order books map (for memory estimation).
    pub fn order_book_capacity(&self) -> usize {
        self.order_books.capacity()
    }

    /// Capacity of the offer locations map (for memory estimation).
    pub fn location_capacity(&self) -> usize {
        self.offer_locations.capacity()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use stellar_xdr::curr::{
        AccountId, AlphaNum4, Asset, AssetCode4, OfferEntry, OfferEntryExt, Price, PublicKey,
        Uint256,
    };

    fn make_account_id(seed: u8) -> AccountId {
        let mut bytes = [0u8; 32];
        bytes[0] = seed;
        AccountId(PublicKey::PublicKeyTypeEd25519(Uint256(bytes)))
    }

    fn usd(issuer_seed: u8) -> Asset {
        Asset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4([b'U', b'S', b'D', 0]),
            issuer: make_account_id(issuer_seed),
        })
    }

    fn eur(issuer_seed: u8) -> Asset {
        Asset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4([b'E', b'U', b'R', 0]),
            issuer: make_account_id(issuer_seed),
        })
    }

    fn make_offer(
        seller_seed: u8,
        offer_id: i64,
        selling: Asset,
        buying: Asset,
        price_n: i32,
        price_d: i32,
    ) -> OfferEntry {
        OfferEntry {
            seller_id: make_account_id(seller_seed),
            offer_id,
            selling,
            buying,
            amount: 1000,
            price: Price {
                n: price_n,
                d: price_d,
            },
            flags: 0,
            ext: OfferEntryExt::V0,
        }
    }

    /// top_n_offer_keys returns top N offers by price (cheapest first), bounded by N.
    #[test]
    fn test_top_n_offer_keys() {
        let issuer = 99u8;
        let buying = usd(issuer);
        let selling = Asset::Native;

        let mut index = OfferIndex::new();
        // Three offers at increasing prices: 1/1, 2/1, 3/1
        let o1 = make_offer(1, 1, selling.clone(), buying.clone(), 1, 1);
        let o2 = make_offer(2, 2, selling.clone(), buying.clone(), 2, 1);
        let o3 = make_offer(3, 3, selling.clone(), buying.clone(), 3, 1);
        index.add_offer(&o1);
        index.add_offer(&o2);
        index.add_offer(&o3);

        // top-2 should return offers 1 and 2 (cheapest first)
        let top2 = index.top_n_offer_keys(&buying, &selling, 2);
        assert_eq!(top2.len(), 2);
        assert_eq!(top2[0].offer_id, 1);
        assert_eq!(top2[1].offer_id, 2);

        // top-10 with only 3 offers should return all 3
        let top10 = index.top_n_offer_keys(&buying, &selling, 10);
        assert_eq!(top10.len(), 3);

        // Unknown pair returns empty
        let empty = index.top_n_offer_keys(&eur(issuer), &selling, 10);
        assert!(empty.is_empty());
    }
}
