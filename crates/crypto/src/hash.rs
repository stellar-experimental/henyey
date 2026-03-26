//! SHA-256 and BLAKE2 hashing utilities.
//!
//! This module provides cryptographic hash functions in both single-shot and
//! streaming modes. All functions return [`Hash256`], a 32-byte hash value.
//!
//! # SHA-256
//!
//! ```
//! use henyey_crypto::sha256;
//!
//! let hash = sha256(b"hello world");
//! ```
//!
//! # BLAKE2b
//!
//! ```
//! use henyey_crypto::blake2;
//!
//! let hash = blake2(b"hello world");
//! ```
//!
//! # HMAC-SHA256
//!
//! ```
//! use henyey_crypto::{hmac_sha256, hmac_sha256_verify};
//!
//! let key = [0u8; 32];
//! let mac = hmac_sha256(&key, b"message");
//! assert!(hmac_sha256_verify(&mac, &key, b"message"));
//! ```
//!
//! # HKDF Key Derivation
//!
//! ```
//! use henyey_crypto::{hkdf_extract, hkdf_expand};
//!
//! let ikm = b"input keying material";
//! let prk = hkdf_extract(ikm);
//! let derived = hkdf_expand(&prk, b"context info");
//! ```

use blake2::Blake2b;
use henyey_common::Hash256;
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use stellar_xdr::curr::WriteXdr;

/// Computes the SHA-256 hash of the given data.
///
/// This is a convenience function for single-shot hashing. For streaming
/// hashing of large or chunked data, use [`Sha256Hasher`] instead.
///
/// # Example
///
/// ```
/// use henyey_crypto::sha256;
///
/// let hash = sha256(b"stellar");
/// assert_eq!(hash.as_bytes().len(), 32);
/// ```
pub fn sha256(data: &[u8]) -> Hash256 {
    Hash256::hash(data)
}

/// Computes a sub-seed SHA-256 hash from a seed and counter.
///
/// This is used for per-transaction PRNG sub-seeding in Soroban.
/// Formula: `SHA256(seed || counter_be)` (counter in big-endian / XDR network byte order)
///
/// # Arguments
///
/// * `seed` - The base seed (32 bytes)
/// * `counter` - A counter value
///
/// # Example
///
/// ```
/// use henyey_crypto::sub_sha256;
///
/// let seed = [0u8; 32];
/// let hash = sub_sha256(&seed, 0);
/// ```
pub fn sub_sha256(seed: &[u8], counter: u64) -> Hash256 {
    let mut hasher = Sha256::new();
    hasher.update(seed);
    // XDR encodes uint64 as 8 bytes big-endian (network byte order)
    hasher.update(counter.to_be_bytes());
    Hash256::from(<[u8; 32]>::from(hasher.finalize()))
}

/// A streaming SHA-256 hasher for incremental hash computation.
///
/// Use this when you need to hash data that is not available all at once,
/// such as when reading from a stream or processing data in chunks.
///
/// # Example
///
/// ```
/// use henyey_crypto::Sha256Hasher;
///
/// let mut hasher = Sha256Hasher::new();
/// hasher.update(b"chunk 1");
/// hasher.update(b"chunk 2");
/// let hash = hasher.finalize();
/// ```
pub struct Sha256Hasher {
    inner: Sha256,
}

impl Sha256Hasher {
    /// Creates a new SHA-256 hasher.
    pub fn new() -> Self {
        Self {
            inner: Sha256::new(),
        }
    }

    /// Feeds data into the hasher.
    ///
    /// This method can be called multiple times to incrementally add data.
    pub fn update(&mut self, data: &[u8]) {
        self.inner.update(data);
    }

    /// Consumes the hasher and returns the computed hash.
    ///
    /// After calling this method, the hasher cannot be used again.
    pub fn finalize(self) -> Hash256 {
        Hash256::from(<[u8; 32]>::from(self.inner.finalize()))
    }
}

impl Default for Sha256Hasher {
    fn default() -> Self {
        Self::new()
    }
}

/// Type alias for BLAKE2b with 32-byte output (256 bits).
type Blake2b256 = Blake2b<blake2::digest::consts::U32>;

/// Computes the BLAKE2b-256 hash of the given data.
///
/// This uses BLAKE2b with a 32-byte (256-bit) output, matching the
/// stellar-core implementation.
///
/// # Example
///
/// ```
/// use henyey_crypto::blake2;
///
/// let hash = blake2(b"stellar");
/// assert_eq!(hash.as_bytes().len(), 32);
/// ```
pub fn blake2(data: &[u8]) -> Hash256 {
    let mut hasher = Blake2b256::new();
    hasher.update(data);
    Hash256::from(<[u8; 32]>::from(hasher.finalize()))
}

/// Type alias for HMAC-SHA256.
type HmacSha256 = Hmac<Sha256>;

fn new_hmac(key: &[u8]) -> HmacSha256 {
    HmacSha256::new_from_slice(key).expect("HMAC accepts any key length")
}

fn finalize_hmac(mac: HmacSha256) -> [u8; 32] {
    mac.finalize().into_bytes().into()
}

fn hmac_sha256_chunks(key: &[u8], chunks: &[&[u8]]) -> [u8; 32] {
    let mut mac = new_hmac(key);
    for chunk in chunks {
        mac.update(chunk);
    }
    finalize_hmac(mac)
}

/// Computes the HMAC-SHA256 of data with a given key.
///
/// # Arguments
///
/// * `key` - The 32-byte HMAC key
/// * `data` - The data to authenticate
///
/// # Returns
///
/// A 32-byte message authentication code.
///
/// # Example
///
/// ```
/// use henyey_crypto::hmac_sha256;
///
/// let key = [0u8; 32];
/// let mac = hmac_sha256(&key, b"message");
/// ```
pub fn hmac_sha256(key: &[u8; 32], data: &[u8]) -> [u8; 32] {
    hmac_sha256_chunks(key, &[data])
}

/// Verifies an HMAC-SHA256 in constant time.
///
/// This function performs a timing-safe comparison to prevent timing attacks.
/// Always use this instead of `==` for HMAC verification.
///
/// # Arguments
///
/// * `mac` - The MAC to verify
/// * `key` - The 32-byte HMAC key
/// * `data` - The original data
///
/// # Returns
///
/// `true` if the MAC is valid, `false` otherwise.
///
/// # Example
///
/// ```
/// use henyey_crypto::{hmac_sha256, hmac_sha256_verify};
///
/// let key = [0u8; 32];
/// let mac = hmac_sha256(&key, b"message");
/// assert!(hmac_sha256_verify(&mac, &key, b"message"));
/// assert!(!hmac_sha256_verify(&mac, &key, b"tampered"));
/// ```
pub fn hmac_sha256_verify(mac: &[u8; 32], key: &[u8; 32], data: &[u8]) -> bool {
    let mut verifier = new_hmac(key);
    verifier.update(data);
    verifier.verify_slice(mac).is_ok()
}

/// Performs HKDF-Extract with an all-zero salt.
///
/// This implements the Extract phase of RFC 5869 HKDF:
/// `PRK = HMAC-SHA256(salt=all_zeros, IKM=input)`
///
/// # Arguments
///
/// * `ikm` - Input Keying Material
///
/// # Returns
///
/// A 32-byte Pseudo-Random Key (PRK).
///
/// # Example
///
/// ```
/// use henyey_crypto::hkdf_extract;
///
/// let ikm = b"some input keying material";
/// let prk = hkdf_extract(ikm);
/// ```
pub fn hkdf_extract(ikm: &[u8]) -> [u8; 32] {
    let zero_salt = [0u8; 32];
    hmac_sha256(&zero_salt, ikm)
}

/// Performs single-step HKDF-Expand.
///
/// This implements a single round of the Expand phase of RFC 5869 HKDF:
/// `OKM = HMAC-SHA256(PRK, info || 0x01)`
///
/// For deriving multiple keys, use different info values.
///
/// # Arguments
///
/// * `prk` - Pseudo-Random Key from HKDF-Extract
/// * `info` - Context/application-specific info
///
/// # Returns
///
/// A 32-byte derived key.
///
/// # Example
///
/// ```
/// use henyey_crypto::{hkdf_extract, hkdf_expand};
///
/// let prk = hkdf_extract(b"ikm");
/// let key = hkdf_expand(&prk, b"context");
/// ```
pub fn hkdf_expand(prk: &[u8; 32], info: &[u8]) -> [u8; 32] {
    let counter = [0x01];
    hmac_sha256_chunks(prk, &[info, &counter])
}

/// Full HKDF key derivation (extract + expand).
///
/// Combines HKDF-Extract and HKDF-Expand in a single call.
///
/// # Arguments
///
/// * `ikm` - Input Keying Material
/// * `info` - Context/application-specific info
///
/// # Returns
///
/// A 32-byte derived key.
///
/// # Example
///
/// ```
/// use henyey_crypto::hkdf;
///
/// let derived = hkdf(b"input keying material", b"context");
/// ```
pub fn hkdf(ikm: &[u8], info: &[u8]) -> [u8; 32] {
    let prk = hkdf_extract(ikm);
    hkdf_expand(&prk, info)
}

/// Computes the SHA-256 hash of an XDR-encoded value.
///
/// This serializes the value to XDR format and hashes the result.
/// Note: This allocates a temporary buffer for the XDR encoding.
///
/// # Arguments
///
/// * `value` - Any value that implements `WriteXdr`
///
/// # Returns
///
/// The SHA-256 hash of the XDR-encoded value, or an error if encoding fails.
///
/// # Example
///
/// ```ignore
/// use henyey_crypto::xdr_sha256;
/// use stellar_xdr::curr::LedgerHeader;
///
/// let header: LedgerHeader = /* ... */;
/// let hash = xdr_sha256(&header)?;
/// ```
pub fn xdr_sha256<T: WriteXdr>(value: &T) -> Result<Hash256, stellar_xdr::curr::Error> {
    let bytes = value.to_xdr(stellar_xdr::curr::Limits::none())?;
    Ok(sha256(&bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sha256() {
        // Test vector from NIST
        let hash = sha256(b"abc");
        assert_eq!(
            hash.to_hex(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn test_sha256_empty() {
        let hash = sha256(b"");
        assert_eq!(
            hash.to_hex(),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn test_streaming_sha256_hasher() {
        let mut hasher = Sha256Hasher::new();
        hasher.update(b"hello");
        hasher.update(b"world");
        let hash = hasher.finalize();

        assert_eq!(hash, sha256(b"helloworld"));
    }

    #[test]
    fn test_sub_sha256() {
        let seed = [0u8; 32];
        let hash0 = sub_sha256(&seed, 0);
        let hash1 = sub_sha256(&seed, 1);

        // Different counters should produce different hashes
        assert_ne!(hash0, hash1);

        // Same inputs should produce same outputs
        assert_eq!(hash0, sub_sha256(&seed, 0));
    }

    #[test]
    fn test_blake2() {
        // Test that BLAKE2 produces 32-byte output
        let hash = blake2(b"stellar");
        assert_eq!(hash.as_bytes().len(), 32);
    }

    #[test]
    fn test_blake2_empty() {
        // BLAKE2b-256 empty input test vector
        let hash = blake2(b"");
        assert_eq!(hash.as_bytes().len(), 32);
    }

    #[test]
    fn test_blake2_differs_from_sha256() {
        // BLAKE2 and SHA-256 should produce different results
        let data = b"test data";
        assert_ne!(blake2(data), sha256(data));
    }

    #[test]
    fn test_hmac_sha256() {
        let key = [0u8; 32];
        let mac = hmac_sha256(&key, b"message");
        assert_eq!(mac.len(), 32);
    }

    #[test]
    fn test_hmac_sha256_verify() {
        let key = [0u8; 32];
        let mac = hmac_sha256(&key, b"message");

        // Valid verification
        assert!(hmac_sha256_verify(&mac, &key, b"message"));

        // Invalid data
        assert!(!hmac_sha256_verify(&mac, &key, b"tampered"));

        // Invalid key
        let wrong_key = [1u8; 32];
        assert!(!hmac_sha256_verify(&mac, &wrong_key, b"message"));

        // Invalid MAC
        let wrong_mac = [0u8; 32];
        assert!(!hmac_sha256_verify(&wrong_mac, &key, b"message"));
    }

    #[test]
    fn test_hmac_different_keys_different_macs() {
        let key1 = [0u8; 32];
        let key2 = [1u8; 32];
        let data = b"same message";

        let mac1 = hmac_sha256(&key1, data);
        let mac2 = hmac_sha256(&key2, data);

        assert_ne!(mac1, mac2);
    }

    #[test]
    fn test_hkdf_extract() {
        let ikm = b"input keying material";
        let prk = hkdf_extract(ikm);
        assert_eq!(prk.len(), 32);
    }

    #[test]
    fn test_hkdf_expand() {
        let prk = hkdf_extract(b"ikm");
        let key1 = hkdf_expand(&prk, b"context1");
        let key2 = hkdf_expand(&prk, b"context2");

        // Different contexts should produce different keys
        assert_ne!(key1, key2);

        // Same context should produce same key
        assert_eq!(key1, hkdf_expand(&prk, b"context1"));
    }

    #[test]
    fn test_hkdf_full() {
        let key = hkdf(b"ikm", b"info");
        assert_eq!(key.len(), 32);
    }

    #[test]
    fn test_xdr_sha256() {
        use stellar_xdr::curr::Uint256;

        let value = Uint256([0u8; 32]);
        let hash = xdr_sha256(&value).unwrap();

        // Should produce a valid 32-byte hash
        assert_eq!(hash.as_bytes().len(), 32);
    }

    #[test]
    fn test_xdr_sha256_matches_direct_hash() {
        use stellar_xdr::curr::Uint256;

        let value = Uint256([42u8; 32]);
        let xdr_bytes = value.to_xdr(stellar_xdr::curr::Limits::none()).unwrap();

        let sha256_hash = xdr_sha256(&value).unwrap();
        let direct_sha256 = sha256(&xdr_bytes);
        assert_eq!(sha256_hash, direct_sha256);
    }
}
