//! Offer sorting and comparison utilities.
//!
//! This module provides utilities for working with DEX offers in Stellar:
//!
//! - [`OfferDescriptor`]: A lightweight reference to an offer's price and ID
//! - [`AssetPair`]: A trading pair of buying and selling assets
//! - [`is_better_offer`]: Determine which offer has a better price
//!
//! # Offer Ordering
//!
//! Offers are ordered by:
//! 1. **Price** (ascending) - Lower price is better (seller wants less of buying asset per selling asset)
//! 2. **Offer ID** (ascending) - For equal prices, older offers (lower ID) have priority
//!
//! This ordering ensures fairness: better-priced offers are matched first, and among
//! equal prices, the oldest offer is filled first (FIFO).
//!
//! # Example
//!
//! ```
//! use henyey_ledger::offer::{OfferDescriptor, is_better_offer};
//! use stellar_xdr::curr::Price;
//!
//! let offer1 = OfferDescriptor {
//!     price: Price { n: 1, d: 2 },  // 0.5
//!     offer_id: 100,
//! };
//!
//! let offer2 = OfferDescriptor {
//!     price: Price { n: 2, d: 3 },  // 0.67
//!     offer_id: 50,
//! };
//!
//! // offer1 is better because it has a lower price
//! assert!(is_better_offer(&offer1, &offer2));
//! ```

use std::hash::{Hash, Hasher};

use stellar_xdr::curr::{Asset, LedgerEntry, OfferEntry, Price, WriteXdr};

/// A lightweight descriptor for an offer used in sorting and comparison.
///
/// This struct captures just the information needed to determine offer ordering:
/// the price and the offer ID.
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

    /// Create an offer descriptor from an offer entry.
    pub fn from_offer_entry(offer: &OfferEntry) -> Self {
        Self {
            price: offer.price.clone(),
            offer_id: offer.offer_id,
        }
    }

    /// Create an offer descriptor from a ledger entry containing an offer.
    ///
    /// # Panics
    ///
    /// Panics if the ledger entry does not contain an offer.
    pub fn from_ledger_entry(entry: &LedgerEntry) -> Self {
        match &entry.data {
            stellar_xdr::curr::LedgerEntryData::Offer(offer) => Self::from_offer_entry(offer),
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

impl PartialOrd for OfferDescriptor {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for OfferDescriptor {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // Compare by price first (lower is better)
        let self_price = self.price_as_f64();
        let other_price = other.price_as_f64();

        match self_price.partial_cmp(&other_price) {
            Some(std::cmp::Ordering::Equal) | None => {
                // For equal prices, lower offer ID is better (older offers first)
                self.offer_id.cmp(&other.offer_id)
            }
            Some(ordering) => ordering,
        }
    }
}

impl Hash for OfferDescriptor {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.price.n.hash(state);
        self.price.d.hash(state);
        self.offer_id.hash(state);
    }
}

/// Check if the left offer is better than the right offer.
///
/// An offer is considered "better" if:
/// 1. It has a lower price (n/d ratio), or
/// 2. For equal prices, it has a lower offer ID (older offers have priority)
///
/// # Arguments
///
/// * `lhs` - The left-hand offer descriptor
/// * `rhs` - The right-hand offer descriptor
///
/// # Returns
///
/// `true` if `lhs` is a better offer than `rhs`.
///
/// # Example
///
/// ```
/// use henyey_ledger::offer::{OfferDescriptor, is_better_offer};
/// use stellar_xdr::curr::Price;
///
/// let cheaper = OfferDescriptor::new(Price { n: 1, d: 2 }, 100);
/// let expensive = OfferDescriptor::new(Price { n: 2, d: 3 }, 50);
///
/// assert!(is_better_offer(&cheaper, &expensive));
/// assert!(!is_better_offer(&expensive, &cheaper));
/// ```
pub fn is_better_offer(lhs: &OfferDescriptor, rhs: &OfferDescriptor) -> bool {
    lhs < rhs
}

/// A trading pair of assets.
///
/// This struct represents a pair of assets being traded in the DEX:
/// what asset is being bought and what asset is being sold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssetPair {
    /// The asset being bought.
    pub buying: Asset,
    /// The asset being sold.
    pub selling: Asset,
}

impl AssetPair {
    /// Create a new asset pair.
    pub fn new(buying: Asset, selling: Asset) -> Self {
        Self { buying, selling }
    }

    /// Create an asset pair from an offer entry.
    pub fn from_offer_entry(offer: &OfferEntry) -> Self {
        Self {
            buying: offer.buying.clone(),
            selling: offer.selling.clone(),
        }
    }
}

impl Hash for AssetPair {
    fn hash<H: Hasher>(&self, state: &mut H) {
        // Hash the XDR bytes of both assets
        if let Ok(bytes) = self.buying.to_xdr(stellar_xdr::curr::Limits::none()) {
            bytes.hash(state);
        }
        if let Ok(bytes) = self.selling.to_xdr(stellar_xdr::curr::Limits::none()) {
            bytes.hash(state);
        }
    }
}

/// A comparator for sorting offers by price then ID.
///
/// This can be used with sorted collections to maintain offers
/// in their proper order.
#[derive(Debug, Clone, Copy, Default)]
pub struct IsBetterOfferComparator;

impl IsBetterOfferComparator {
    /// Compare two offer descriptors.
    ///
    /// Returns `true` if `lhs` should come before `rhs` in sorted order.
    pub fn compare(&self, lhs: &OfferDescriptor, rhs: &OfferDescriptor) -> bool {
        is_better_offer(lhs, rhs)
    }
}

/// Sort offer descriptors by price and ID.
pub fn sort_offer_descriptors(offers: &mut [OfferDescriptor]) {
    offers.sort();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_price(n: i32, d: i32) -> Price {
        Price { n, d }
    }

    fn make_descriptor(n: i32, d: i32, offer_id: i64) -> OfferDescriptor {
        OfferDescriptor::new(make_price(n, d), offer_id)
    }

    #[test]
    fn test_offer_descriptor_equality() {
        let d1 = make_descriptor(1, 2, 100);
        let d2 = make_descriptor(1, 2, 100);
        let d3 = make_descriptor(1, 2, 101);
        let d4 = make_descriptor(2, 4, 100); // Same price ratio but different n/d

        assert_eq!(d1, d2);
        assert_ne!(d1, d3);
        assert_ne!(d1, d4); // Different n/d even though ratio is same
    }

    #[test]
    fn test_is_better_offer_by_price() {
        let cheaper = make_descriptor(1, 2, 100); // price = 0.5
        let expensive = make_descriptor(2, 3, 50); // price = 0.67

        assert!(is_better_offer(&cheaper, &expensive));
        assert!(!is_better_offer(&expensive, &cheaper));
    }

    #[test]
    fn test_is_better_offer_by_id() {
        let older = make_descriptor(1, 2, 100);
        let newer = make_descriptor(1, 2, 200);

        // Same price, older (lower ID) is better
        assert!(is_better_offer(&older, &newer));
        assert!(!is_better_offer(&newer, &older));
    }

    #[test]
    fn test_is_better_offer_equal() {
        let d1 = make_descriptor(1, 2, 100);
        let d2 = make_descriptor(1, 2, 100);

        // Neither is better than equal
        assert!(!is_better_offer(&d1, &d2));
        assert!(!is_better_offer(&d2, &d1));
    }

    #[test]
    fn test_offer_descriptor_ordering() {
        let mut offers = vec![
            make_descriptor(3, 4, 300), // price = 0.75
            make_descriptor(1, 2, 100), // price = 0.5
            make_descriptor(1, 2, 200), // price = 0.5, higher ID
            make_descriptor(2, 3, 50),  // price = 0.67
        ];

        sort_offer_descriptors(&mut offers);

        // Expected order: 0.5 (ID 100), 0.5 (ID 200), 0.67, 0.75
        assert_eq!(offers[0], make_descriptor(1, 2, 100));
        assert_eq!(offers[1], make_descriptor(1, 2, 200));
        assert_eq!(offers[2], make_descriptor(2, 3, 50));
        assert_eq!(offers[3], make_descriptor(3, 4, 300));
    }

    #[test]
    fn test_asset_pair() {
        let native = Asset::Native;
        let credit = Asset::CreditAlphanum4(stellar_xdr::curr::AlphaNum4 {
            asset_code: stellar_xdr::curr::AssetCode4([b'U', b'S', b'D', 0]),
            issuer: stellar_xdr::curr::AccountId(
                stellar_xdr::curr::PublicKey::PublicKeyTypeEd25519(stellar_xdr::curr::Uint256(
                    [0; 32],
                )),
            ),
        });

        let pair1 = AssetPair::new(native.clone(), credit.clone());
        let pair2 = AssetPair::new(native.clone(), credit.clone());
        let pair3 = AssetPair::new(credit.clone(), native.clone());

        assert_eq!(pair1, pair2);
        assert_ne!(pair1, pair3); // Different order
    }

    #[test]
    fn test_asset_pair_hash() {
        use std::collections::HashMap;

        let native = Asset::Native;
        let credit = Asset::CreditAlphanum4(stellar_xdr::curr::AlphaNum4 {
            asset_code: stellar_xdr::curr::AssetCode4([b'U', b'S', b'D', 0]),
            issuer: stellar_xdr::curr::AccountId(
                stellar_xdr::curr::PublicKey::PublicKeyTypeEd25519(stellar_xdr::curr::Uint256(
                    [0; 32],
                )),
            ),
        });

        let pair = AssetPair::new(native.clone(), credit.clone());

        let mut map: HashMap<AssetPair, i32> = HashMap::new();
        map.insert(pair.clone(), 42);

        assert_eq!(map.get(&pair), Some(&42));

        // Different pair should not match
        let different = AssetPair::new(credit.clone(), native.clone());
        assert_eq!(map.get(&different), None);
    }

    #[test]
    fn test_is_better_offer_comparator() {
        let comparator = IsBetterOfferComparator;

        let better = make_descriptor(1, 2, 100);
        let worse = make_descriptor(2, 3, 50);

        assert!(comparator.compare(&better, &worse));
        assert!(!comparator.compare(&worse, &better));
    }

    #[test]
    fn test_price_as_f64() {
        let d1 = make_descriptor(1, 2, 100);
        assert!((d1.price_as_f64() - 0.5).abs() < f64::EPSILON);

        let d2 = make_descriptor(1, 3, 100);
        assert!((d2.price_as_f64() - 1.0 / 3.0).abs() < f64::EPSILON);
    }
}
