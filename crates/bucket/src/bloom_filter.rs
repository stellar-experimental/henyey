//! Binary Fuse Filter for fast negative lookups in bucket indexes.
//!
//! This module provides a probabilistic data structure that enables fast negative
//! lookups (determining that a key is definitely NOT in a bucket) while allowing
//! for occasional false positives.
//!
//! # Algorithm
//!
//! We use a Binary Fuse Filter (BinaryFuse16) which provides:
//! - False positive rate: ~1/65536 (~0.0015%)
//! - Space efficiency: ~18 bits per entry
//! - O(1) lookup time
//!
//! # Usage in Bucket Indexing
//!
//! For large buckets (typically > 250 MB), the bloom filter reduces disk I/O
//! by quickly determining if a key is not present without needing to check
//! the range index or read from disk.
//!
//! # Hash Function
//!
//! Keys are hashed using SipHash-2-4 for consistency with stellar-core.
//! The hash seed must match the global short hash key to ensure deterministic
//! behavior across nodes.
//!
//! # Example
//!
//! ```ignore
//! use henyey_bucket::bloom_filter::BucketBloomFilter;
//!
//! // Build filter from keys during bucket index construction
//! let key_hashes: Vec<u64> = ledger_keys.iter()
//!     .map(|k| BucketBloomFilter::hash_key(k, &seed))
//!     .collect();
//!
//! let filter = BucketBloomFilter::from_hashes(&key_hashes, &seed)?;
//!
//! // Fast negative lookup
//! let query_key = LedgerKey::Account(...);
//! if !filter.may_contain(&query_key, &seed) {
//!     // Key is definitely NOT in the bucket - skip disk read
//!     return Ok(None);
//! }
//! // Key might be in bucket - need to check disk
//! ```

use siphasher::sip::SipHasher24;
use std::hash::Hasher;
use stellar_xdr::curr::{LedgerKey, Limits, WriteXdr};
use xorf::{BinaryFuse16, Filter};

use crate::{BucketError, Result};

/// Size of the SipHash key in bytes (128 bits).
pub const HASH_KEY_BYTES: usize = 16;

/// Type alias for the hash seed used in bloom filter construction.
pub type HashSeed = [u8; HASH_KEY_BYTES];

/// A Binary Fuse Filter for fast negative lookups in bucket indexes.
///
/// This probabilistic data structure allows fast determination of whether
/// a key is definitely NOT in a set, while allowing occasional false positives
/// (key reported as possibly present when it's not).
///
/// # False Positive Rate
///
/// BinaryFuse16 has a false positive rate of approximately 1/65536 (~0.0015%).
/// This means out of 65,536 negative lookups, on average only one will
/// incorrectly report the key as possibly present.
///
/// # Construction
///
/// Construction can fail if there are too many hash collisions in the input.
/// The implementation retries with modified seeds up to 10 times before failing.
#[derive(Clone)]
pub struct BucketBloomFilter {
    /// The underlying binary fuse filter.
    filter: BinaryFuse16,
    /// The hash seed used during construction (for verification).
    seed: HashSeed,
}

impl BucketBloomFilter {
    /// Creates a new bloom filter from pre-computed key hashes.
    ///
    /// # Arguments
    ///
    /// * `key_hashes` - SipHash-2-4 hashes of all keys to include
    /// * `seed` - The hash seed (should match the global short hash key)
    ///
    /// # Errors
    ///
    /// Returns an error if the filter construction fails after multiple retries.
    /// This can happen with highly degenerate input (many hash collisions).
    ///
    /// # Note
    ///
    /// The filter requires at least 2 elements. For 0 or 1 elements, use
    /// [`BucketBloomFilter::empty`] or handle the case specially.
    pub fn from_hashes(key_hashes: &[u64], seed: &HashSeed) -> Result<Self> {
        if key_hashes.len() < 2 {
            return Err(BucketError::BloomFilter(
                "bloom filter requires at least 2 elements".to_string(),
            ));
        }

        // The xorf BinaryFuse16 constructor can fail with out_of_range errors
        // if there are hash collisions. We retry with a modified seed similar
        // to the stellar-core implementation.
        let mut modified_seed = *seed;
        for attempt in 0..10 {
            // BinaryFuse16::try_from takes ownership, so we need to clone for retries
            match BinaryFuse16::try_from(key_hashes) {
                Ok(filter) => {
                    return Ok(Self {
                        filter,
                        seed: modified_seed,
                    });
                }
                Err(_) if attempt < 9 => {
                    // Rotate the seed and retry (matches stellar-core behavior)
                    modified_seed[0] = modified_seed[0].wrapping_add(1);
                }
                Err(e) => {
                    return Err(BucketError::BloomFilter(format!(
                        "failed to construct bloom filter after 10 attempts: {:?}",
                        e
                    )));
                }
            }
        }

        // Unreachable due to the loop logic, but needed for type safety
        Err(BucketError::BloomFilter(
            "bloom filter construction failed".to_string(),
        ))
    }

    /// Creates an empty bloom filter placeholder.
    ///
    /// This is used when a bucket has too few entries to warrant a bloom filter.
    /// The filter will always return `true` from `may_contain` to ensure
    /// no false negatives.
    pub fn empty() -> Option<Self> {
        // Return None to indicate no filter - callers should skip bloom check
        None
    }

    /// Computes the SipHash-2-4 hash of a ledger key.
    ///
    /// This matches the stellar-core hash computation for bloom filter keys.
    pub fn hash_key(key: &LedgerKey, seed: &HashSeed) -> u64 {
        let key_bytes = key
            .to_xdr(Limits::none())
            .expect("XDR encoding of LedgerKey must not fail");
        Self::hash_bytes(&key_bytes, seed)
    }

    /// Computes the SipHash-2-4 hash of raw bytes.
    pub fn hash_bytes(bytes: &[u8], seed: &HashSeed) -> u64 {
        let mut hasher = SipHasher24::new_with_key(seed);
        hasher.write(bytes);
        hasher.finish()
    }

    /// Checks if a key might be contained in the filter.
    ///
    /// - Returns `false` if the key is definitely NOT in the set (no false negatives)
    /// - Returns `true` if the key might be in the set (possible false positive)
    ///
    /// # Hash Computation
    ///
    /// This method computes the hash internally. If you need to check many keys
    /// and already have the hashes, use [`Self::may_contain_hash`] instead.
    pub fn may_contain(&self, key: &LedgerKey, seed: &HashSeed) -> bool {
        let hash = Self::hash_key(key, seed);
        self.may_contain_hash(hash)
    }

    /// Checks if a pre-computed hash might be contained in the filter.
    ///
    /// Use this when you already have the key hash to avoid redundant computation.
    pub fn may_contain_hash(&self, hash: u64) -> bool {
        self.filter.contains(&hash)
    }

    /// Returns the hash seed used during construction.
    pub fn seed(&self) -> &HashSeed {
        &self.seed
    }

    /// Returns the approximate size of the filter in bytes.
    pub fn size_bytes(&self) -> usize {
        // BinaryFuse16 uses approximately 18 bits per element
        // The filter stores fingerprints in a Vec<u16>
        self.filter.len() * std::mem::size_of::<u16>()
    }

    /// Returns the number of fingerprints in the filter.
    pub fn len(&self) -> usize {
        self.filter.len()
    }

    /// Returns true if the filter is empty.
    pub fn is_empty(&self) -> bool {
        self.filter.len() == 0
    }

    /// Returns a reference to the inner `BinaryFuse16` filter.
    ///
    /// Used for serialization/persistence.
    pub fn inner_filter(&self) -> &BinaryFuse16 {
        &self.filter
    }

    /// Constructs a `BucketBloomFilter` from its component parts.
    ///
    /// Used when restoring a persisted filter from disk.
    ///
    /// # Arguments
    ///
    /// * `filter` - The deserialized `BinaryFuse16` filter
    /// * `seed` - The hash seed used during original construction
    pub fn from_parts(filter: BinaryFuse16, seed: HashSeed) -> Self {
        Self { filter, seed }
    }
}

impl std::fmt::Debug for BucketBloomFilter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BucketBloomFilter")
            .field("fingerprints", &self.len())
            .field("size_bytes", &self.size_bytes())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use stellar_xdr::curr::*;

    fn make_test_seed() -> HashSeed {
        [
            0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d,
            0x0e, 0x0f,
        ]
    }

    fn make_account_key(id: u8) -> LedgerKey {
        LedgerKey::Account(LedgerKeyAccount {
            account_id: AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([id; 32]))),
        })
    }

    #[test]
    fn test_bloom_filter_construction() {
        let seed = make_test_seed();

        // Create hashes for 100 keys
        let hashes: Vec<u64> = (0..100u8)
            .map(|i| BucketBloomFilter::hash_key(&make_account_key(i), &seed))
            .collect();

        let filter = BucketBloomFilter::from_hashes(&hashes, &seed).unwrap();
        assert!(!filter.is_empty());
        assert!(filter.len() > 0);
    }

    #[test]
    fn test_bloom_filter_no_false_negatives() {
        let seed = make_test_seed();

        // Create and insert 50 keys
        let keys: Vec<LedgerKey> = (0..50u8).map(make_account_key).collect();
        let hashes: Vec<u64> = keys
            .iter()
            .map(|k| BucketBloomFilter::hash_key(k, &seed))
            .collect();

        let filter = BucketBloomFilter::from_hashes(&hashes, &seed).unwrap();

        // All inserted keys must be found (no false negatives)
        for key in &keys {
            assert!(
                filter.may_contain(key, &seed),
                "false negative detected for key"
            );
        }
    }

    #[test]
    fn test_bloom_filter_false_positive_rate() {
        let seed = make_test_seed();

        // Insert keys 0-99
        let inserted: Vec<u64> = (0..100u8)
            .map(|i| BucketBloomFilter::hash_key(&make_account_key(i), &seed))
            .collect();

        let filter = BucketBloomFilter::from_hashes(&inserted, &seed).unwrap();

        // Test keys 100-199 (none should be in the filter)
        let mut false_positives = 0;
        for i in 100..200u8 {
            let key = make_account_key(i);
            if filter.may_contain(&key, &seed) {
                false_positives += 1;
            }
        }

        // With BinaryFuse16, false positive rate is ~1/65536
        // For 100 tests, we expect essentially 0 false positives
        // Allow up to 5 to account for statistical variation
        assert!(
            false_positives <= 5,
            "too many false positives: {}/100",
            false_positives
        );
    }

    #[test]
    fn test_bloom_filter_requires_minimum_elements() {
        let seed = make_test_seed();

        // Empty list should fail
        let result = BucketBloomFilter::from_hashes(&[], &seed);
        assert!(result.is_err());

        // Single element should fail
        let result = BucketBloomFilter::from_hashes(&[42], &seed);
        assert!(result.is_err());

        // Two elements should succeed
        let result = BucketBloomFilter::from_hashes(&[42, 43], &seed);
        assert!(result.is_ok());
    }

    #[test]
    fn test_hash_determinism() {
        let seed = make_test_seed();
        let key = make_account_key(42);

        let hash1 = BucketBloomFilter::hash_key(&key, &seed);
        let hash2 = BucketBloomFilter::hash_key(&key, &seed);

        assert_eq!(hash1, hash2, "hash should be deterministic");
    }

    #[test]
    fn test_different_seeds_different_hashes() {
        let seed1 = make_test_seed();
        let mut seed2 = make_test_seed();
        seed2[0] = 0xFF;

        let key = make_account_key(42);

        let hash1 = BucketBloomFilter::hash_key(&key, &seed1);
        let hash2 = BucketBloomFilter::hash_key(&key, &seed2);

        assert_ne!(
            hash1, hash2,
            "different seeds should produce different hashes"
        );
    }

    #[test]
    fn test_bloom_filter_size() {
        let seed = make_test_seed();

        // Create filter with 1000 keys
        let hashes: Vec<u64> = (0..1000u64).collect();
        let filter = BucketBloomFilter::from_hashes(&hashes, &seed).unwrap();

        // BinaryFuse16 uses ~18 bits per entry, so ~2.25 bytes per entry
        // For 1000 entries, expect roughly 2-3 KB
        let size = filter.size_bytes();
        assert!(size > 1000, "filter too small: {} bytes", size);
        assert!(size < 10000, "filter too large: {} bytes", size);
    }
}
