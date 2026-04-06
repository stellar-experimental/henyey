//! Common types for rs-stellar-core.
//!
//! This module provides fundamental types used throughout the codebase,
//! particularly the [`Hash256`] type for cryptographic hashes.

use sha2::{Digest, Sha256};
use std::fmt;

/// A 32-byte SHA-256 hash.
///
/// This is the canonical hash type used throughout Stellar for ledger hashes,
/// transaction hashes, network IDs, and other cryptographic identifiers.
///
/// # Examples
///
/// ```rust
/// use henyey_common::Hash256;
///
/// // Hash some data
/// let hash = Hash256::hash(b"hello world");
/// assert!(!hash.is_zero());
///
/// // Convert to/from hex
/// let hex_str = hash.to_hex();
/// let parsed = Hash256::from_hex(&hex_str).unwrap();
/// assert_eq!(hash, parsed);
///
/// // Create from raw bytes
/// let zeros = Hash256::from_bytes([0u8; 32]);
/// assert!(zeros.is_zero());
/// ```
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Hash256(pub [u8; 32]);

impl Hash256 {
    /// The zero hash (all bytes are 0x00).
    ///
    /// This is commonly used as a sentinel value or placeholder.
    pub const ZERO: Self = Self([0u8; 32]);

    /// Compute the SHA-256 hash of arbitrary data.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use henyey_common::Hash256;
    ///
    /// let hash = Hash256::hash(b"test data");
    /// assert_eq!(hash.as_bytes().len(), 32);
    /// ```
    pub fn hash(data: &[u8]) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(data);
        let result = hasher.finalize();
        let mut bytes = [0u8; 32];
        bytes.copy_from_slice(&result);
        Self(bytes)
    }

    /// Compute the SHA-256 hash of XDR-encoded data.
    ///
    /// This is a convenience method that first serializes the value to XDR format
    /// and then computes its hash. This is the standard way to hash Stellar
    /// protocol objects.
    ///
    /// # Errors
    ///
    /// Returns an error if XDR serialization fails.
    pub fn hash_xdr<T: stellar_xdr::curr::WriteXdr>(
        value: &T,
    ) -> Result<Self, stellar_xdr::curr::Error> {
        Self::hash_xdr_streaming(value)
    }

    /// Compute the SHA-256 hash of XDR-encoded data using streaming serialization.
    ///
    /// Writes XDR directly into the SHA-256 hasher without allocating
    /// an intermediate buffer. This is significantly faster for large
    /// structures (e.g. transaction sets with thousands of entries).
    pub fn hash_xdr_streaming<T: stellar_xdr::curr::WriteXdr>(
        value: &T,
    ) -> Result<Self, stellar_xdr::curr::Error> {
        use sha2::{Digest, Sha256};
        use stellar_xdr::curr::Limited;

        struct Sha256Writer(Sha256);
        impl std::io::Write for Sha256Writer {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.update(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let mut writer = Sha256Writer(Sha256::new());
        let mut limited = Limited::new(&mut writer, stellar_xdr::curr::Limits::none());
        value.write_xdr(&mut limited)?;
        let result = writer.0.finalize();
        let mut bytes = [0u8; 32];
        bytes.copy_from_slice(&result);
        Ok(Self(bytes))
    }

    /// Returns a reference to the underlying 32-byte array.
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Creates a `Hash256` from a 32-byte array.
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Creates a `Hash256` from a hexadecimal string.
    ///
    /// The string must be exactly 64 hex characters (representing 32 bytes).
    ///
    /// # Errors
    ///
    /// Returns an error if the string is not valid hex or not exactly 64 characters.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use henyey_common::Hash256;
    ///
    /// let hash = Hash256::from_hex(
    ///     "0000000000000000000000000000000000000000000000000000000000000000"
    /// ).unwrap();
    /// assert!(hash.is_zero());
    /// ```
    pub fn from_hex(s: &str) -> Result<Self, hex::FromHexError> {
        let bytes = hex::decode(s)?;
        if bytes.len() != 32 {
            return Err(hex::FromHexError::InvalidStringLength);
        }
        let mut arr = [0u8; 32];
        arr.copy_from_slice(&bytes);
        Ok(Self(arr))
    }

    /// Converts the hash to a lowercase hexadecimal string.
    ///
    /// The resulting string is always 64 characters long.
    pub fn to_hex(&self) -> String {
        hex::encode(self.0)
    }

    /// Returns `true` if this is the zero hash.
    ///
    /// This is useful for checking sentinel values or uninitialized hashes.
    pub fn is_zero(&self) -> bool {
        self.0 == [0u8; 32]
    }
}

impl fmt::Debug for Hash256 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Hash256({})", self.to_hex())
    }
}

impl fmt::Display for Hash256 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.to_hex())
    }
}

impl From<stellar_xdr::curr::Hash> for Hash256 {
    fn from(hash: stellar_xdr::curr::Hash) -> Self {
        Self(hash.0)
    }
}

impl From<Hash256> for stellar_xdr::curr::Hash {
    fn from(hash: Hash256) -> Self {
        stellar_xdr::curr::Hash(hash.0)
    }
}

impl From<[u8; 32]> for Hash256 {
    fn from(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }
}

impl AsRef<[u8]> for Hash256 {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

// ============================================================================
// Hash XOR Operations
// ============================================================================

/// XOR two hashes together, modifying the first in place.
impl std::ops::BitXorAssign for Hash256 {
    fn bitxor_assign(&mut self, rhs: Self) {
        for (a, b) in self.0.iter_mut().zip(rhs.0.iter()) {
            *a ^= b;
        }
    }
}

impl std::ops::BitXor for Hash256 {
    type Output = Self;

    fn bitxor(mut self, rhs: Self) -> Self::Output {
        self ^= rhs;
        self
    }
}

/// Extract the [`LedgerKey`] from a [`LedgerEntry`].
///
/// This is the canonical, infallible conversion from a ledger entry to its
/// corresponding key. The match is exhaustive over all `LedgerEntryData`
/// variants, so it always succeeds.
pub fn entry_to_key(entry: &stellar_xdr::curr::LedgerEntry) -> stellar_xdr::curr::LedgerKey {
    use stellar_xdr::curr::*;

    match &entry.data {
        LedgerEntryData::Account(a) => LedgerKey::Account(LedgerKeyAccount {
            account_id: a.account_id.clone(),
        }),
        LedgerEntryData::Trustline(t) => LedgerKey::Trustline(LedgerKeyTrustLine {
            account_id: t.account_id.clone(),
            asset: t.asset.clone(),
        }),
        LedgerEntryData::Offer(o) => LedgerKey::Offer(LedgerKeyOffer {
            seller_id: o.seller_id.clone(),
            offer_id: o.offer_id,
        }),
        LedgerEntryData::Data(d) => LedgerKey::Data(LedgerKeyData {
            account_id: d.account_id.clone(),
            data_name: d.data_name.clone(),
        }),
        LedgerEntryData::ClaimableBalance(c) => {
            LedgerKey::ClaimableBalance(LedgerKeyClaimableBalance {
                balance_id: c.balance_id.clone(),
            })
        }
        LedgerEntryData::LiquidityPool(l) => LedgerKey::LiquidityPool(LedgerKeyLiquidityPool {
            liquidity_pool_id: l.liquidity_pool_id.clone(),
        }),
        LedgerEntryData::ContractData(c) => LedgerKey::ContractData(LedgerKeyContractData {
            contract: c.contract.clone(),
            key: c.key.clone(),
            durability: c.durability,
        }),
        LedgerEntryData::ContractCode(c) => LedgerKey::ContractCode(LedgerKeyContractCode {
            hash: c.hash.clone(),
        }),
        LedgerEntryData::ConfigSetting(c) => LedgerKey::ConfigSetting(LedgerKeyConfigSetting {
            config_setting_id: c.discriminant(),
        }),
        LedgerEntryData::Ttl(t) => LedgerKey::Ttl(LedgerKeyTtl {
            key_hash: t.key_hash.clone(),
        }),
    }
}

/// Type alias: `ThresholdLevel` is now `ThresholdIndexes` from the XDR crate.
/// Variants: `MasterWeight = 0`, `Low = 1`, `Med = 2`, `High = 3`.
pub type ThresholdLevel = stellar_xdr::curr::ThresholdIndexes;

/// Deterministic seed derivation matching stellar-core `txtest::getAccount()`.
///
/// The name is right-padded with `'.'` to fill 32 bytes, then used as an
/// Ed25519 seed. Names longer than 32 bytes are truncated.
///
/// This is used for test account key derivation (`/testacc`, `GENESIS_TEST_ACCOUNT_COUNT`,
/// and load generation).
pub fn deterministic_seed(name: &str) -> [u8; 32] {
    let mut seed = [b'.'; 32];
    let len = name.len().min(32);
    seed[..len].copy_from_slice(&name.as_bytes()[..len]);
    seed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hash256_hash() {
        let hash = Hash256::hash(b"hello");
        assert!(!hash.is_zero());

        // Same input should produce same hash
        let hash2 = Hash256::hash(b"hello");
        assert_eq!(hash, hash2);

        // Different input should produce different hash
        let hash3 = Hash256::hash(b"world");
        assert_ne!(hash, hash3);
    }

    #[test]
    fn test_hash256_hex() {
        let hash = Hash256::hash(b"test");
        let hex = hash.to_hex();
        let parsed = Hash256::from_hex(&hex).unwrap();
        assert_eq!(hash, parsed);
    }

    #[test]
    fn test_hash256_zero() {
        assert!(Hash256::ZERO.is_zero());
        assert!(!Hash256::hash(b"test").is_zero());
    }

    #[test]
    fn test_threshold_level_index() {
        assert_eq!(ThresholdLevel::Low as usize, 1);
        assert_eq!(ThresholdLevel::Med as usize, 2);
        assert_eq!(ThresholdLevel::High as usize, 3);
    }

    #[test]
    fn test_hash_xor() {
        let mut h1 = Hash256::from_bytes([0xff; 32]);
        let h2 = Hash256::from_bytes([0xff; 32]);
        h1 ^= h2;
        assert!(h1.is_zero());

        let h3 = Hash256::from_bytes([0x0f; 32]);
        let h4 = Hash256::from_bytes([0xf0; 32]);
        let result = h3 ^ h4;
        assert_eq!(result.0, [0xff; 32]);
    }

    #[test]
    fn test_deterministic_seed_padding() {
        let seed = deterministic_seed("root");
        assert_eq!(&seed[..4], b"root");
        assert!(seed[4..].iter().all(|&b| b == b'.'));
    }

    #[test]
    fn test_deterministic_seed_full_length() {
        let name = "a]".repeat(16); // 32 bytes
        let seed = deterministic_seed(&name);
        assert_eq!(&seed[..], name.as_bytes());
    }

    #[test]
    fn test_deterministic_seed_empty() {
        let seed = deterministic_seed("");
        assert!(seed.iter().all(|&b| b == b'.'));
    }
}
