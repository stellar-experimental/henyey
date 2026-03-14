//! Signature utilities and helpers.
//!
//! This module provides convenience functions for signing and verification,
//! as well as the [`SignedMessage`] type for bundling a message with its
//! signature and signer hint.
//!
//! # Signature Hints
//!
//! Stellar uses "signature hints" to help identify which key created a
//! signature without storing the full public key. A hint is the last 4 bytes
//! of the public key. When verifying, the hint can quickly filter candidate
//! keys before performing the expensive signature verification.

use std::collections::{HashMap, VecDeque};
use std::sync::RwLock;

use once_cell::sync::Lazy;

use crate::error::CryptoError;
use crate::keys::{PublicKey, SecretKey, Signature};
use henyey_common::Hash256;

/// Default capacity for the signature verification cache, matching stellar-core's
/// `gVerifySigCache` (250K entries) in `SecretKey.cpp`.
const SIG_CACHE_CAPACITY: usize = 250_000;

/// Global ed25519 signature verification cache.
///
/// Keyed by SHA-256(pubkey || signature || message_hash). Matches stellar-core's
/// global `gVerifySigCache` which persists across the validator lifetime so that
/// signatures verified during flood/nomination get cache hits during apply.
///
/// Uses RwLock to allow parallel cache lookups from concurrent cluster threads.
/// Only cache inserts require exclusive access.
static SIG_VERIFY_CACHE: Lazy<RwLock<SigVerifyCache>> =
    Lazy::new(|| RwLock::new(SigVerifyCache::new(SIG_CACHE_CAPACITY)));

struct SigVerifyCache {
    map: HashMap<[u8; 32], bool>,
    order: VecDeque<[u8; 32]>,
    capacity: usize,
}

impl SigVerifyCache {
    fn new(capacity: usize) -> Self {
        Self {
            map: HashMap::with_capacity(capacity),
            order: VecDeque::with_capacity(capacity),
            capacity,
        }
    }

    fn get(&self, key: &[u8; 32]) -> Option<bool> {
        self.map.get(key).copied()
    }

    fn insert(&mut self, key: [u8; 32], value: bool) {
        if self.map.contains_key(&key) {
            return;
        }
        if self.map.len() >= self.capacity {
            if let Some(old) = self.order.pop_front() {
                self.map.remove(&old);
            }
        }
        self.map.insert(key, value);
        self.order.push_back(key);
    }
}

fn compute_cache_key(pubkey: &[u8; 32], sig: &[u8; 64], hash: &[u8; 32]) -> [u8; 32] {
    use blake2::{Blake2b, Digest as _};
    type Blake2b256 = Blake2b<blake2::digest::consts::U32>;
    let mut hasher = Blake2b256::new();
    hasher.update(pubkey);
    hasher.update(sig);
    hasher.update(hash);
    hasher.finalize().into()
}

/// Signs arbitrary data with a secret key.
///
/// This is a convenience wrapper around [`SecretKey::sign`].
pub fn sign(secret_key: &SecretKey, data: &[u8]) -> Signature {
    secret_key.sign(data)
}

/// Verifies a signature over data.
///
/// This is a convenience wrapper around [`PublicKey::verify`].
///
/// # Errors
///
/// Returns [`CryptoError::InvalidSignature`] if verification fails.
pub fn verify(
    public_key: &PublicKey,
    data: &[u8],
    signature: &Signature,
) -> Result<(), CryptoError> {
    public_key.verify(data, signature)
}

/// A message bundled with its signature and a hint to identify the signer.
///
/// This type is useful when you need to transmit a signed message along with
/// enough information to verify it. The hint allows recipients to quickly
/// identify which key should be used for verification.
///
/// # Example
///
/// ```
/// use henyey_crypto::{SecretKey, SignedMessage};
///
/// let secret = SecretKey::generate();
/// let public = secret.public_key();
///
/// // Create a signed message
/// let signed = SignedMessage::new(&secret, b"hello".to_vec());
///
/// // Verify with the public key
/// assert!(signed.verify(&public).is_ok());
/// ```
#[derive(Debug, Clone)]
pub struct SignedMessage {
    /// The original message bytes.
    pub message: Vec<u8>,
    /// The Ed25519 signature over the message.
    pub signature: Signature,
    /// The signature hint (last 4 bytes of the signer's public key).
    pub hint: [u8; 4],
}

impl SignedMessage {
    /// Creates a new signed message.
    ///
    /// Signs the message with the provided secret key and extracts the hint
    /// from the corresponding public key.
    pub fn new(secret_key: &SecretKey, message: Vec<u8>) -> Self {
        let signature = secret_key.sign(&message);
        let public_key = secret_key.public_key();
        let hint = signature_hint(&public_key);

        Self {
            message,
            signature,
            hint,
        }
    }

    /// Verifies the signature against a public key.
    ///
    /// First checks that the hint matches the public key, then verifies
    /// the signature. This allows quick rejection of mismatched keys.
    ///
    /// # Errors
    ///
    /// Returns [`CryptoError::InvalidSignature`] if:
    /// - The hint does not match the public key
    /// - The signature verification fails
    pub fn verify(&self, public_key: &PublicKey) -> Result<(), CryptoError> {
        // Check hint matches before doing expensive signature verification
        if self.hint != signature_hint(public_key) {
            return Err(CryptoError::InvalidSignature);
        }

        public_key.verify(&self.message, &self.signature)
    }
}

/// Computes the signature hint for a public key.
///
/// The hint is the last 4 bytes of the 32-byte public key. It is used in
/// Stellar to help identify signers without including the full public key.
pub fn signature_hint(public_key: &PublicKey) -> [u8; 4] {
    let key_bytes = public_key.as_bytes();
    key_bytes[28..32].try_into().expect("slice is exactly 4 bytes")
}

/// Signs a hash value.
///
/// This signs the raw 32 bytes of the hash. Use this when signing transaction
/// hashes or other pre-hashed data.
pub fn sign_hash(secret_key: &SecretKey, hash: &Hash256) -> Signature {
    secret_key.sign(hash.as_bytes())
}

/// Verifies a signature over a hash value, using a global cache to avoid
/// redundant ed25519 verification.
///
/// The cache matches stellar-core's `gVerifySigCache` in `SecretKey.cpp`.
/// Within a TX, the same signature is verified 2+N times (N = num ops);
/// across flood→apply, each TX signature is verified twice. The cache
/// reduces all repeated verifications to a HashMap lookup.
///
/// # Errors
///
/// Returns [`CryptoError::InvalidSignature`] if verification fails.
pub fn verify_hash(
    public_key: &PublicKey,
    hash: &Hash256,
    signature: &Signature,
) -> Result<(), CryptoError> {
    let cache_key = compute_cache_key(public_key.as_bytes(), signature.as_bytes(), hash.as_bytes());

    // Check cache (read lock allows parallel lookups from cluster threads)
    {
        let cache = SIG_VERIFY_CACHE.read().unwrap();
        if let Some(result) = cache.get(&cache_key) {
            return if result {
                Ok(())
            } else {
                Err(CryptoError::InvalidSignature)
            };
        }
    }

    // Cache miss — perform actual ed25519 verification
    let result = public_key.verify(hash.as_bytes(), signature);

    // Store result in cache (write lock only for inserts)
    {
        let mut cache = SIG_VERIFY_CACHE.write().unwrap();
        cache.insert(cache_key, result.is_ok());
    }

    result
}

/// Verifies a signature over a hash value from raw public key bytes.
///
/// Like [`verify_hash`], but accepts raw 32-byte public key bytes instead of a
/// [`PublicKey`]. This avoids ed25519 point decompression (~35μs) on cache hits.
/// Matches stellar-core's `PubKeyUtils::verifySig` which checks the signature
/// cache using raw bytes before touching the crypto library.
///
/// # Errors
///
/// Returns [`CryptoError::InvalidSignature`] if verification fails.
/// Returns [`CryptoError::InvalidPublicKey`] if the raw bytes are not a valid
/// ed25519 public key (only checked on cache miss).
pub fn verify_hash_from_raw_key(
    pubkey_bytes: &[u8; 32],
    hash: &Hash256,
    signature: &Signature,
) -> Result<(), CryptoError> {
    let cache_key = compute_cache_key(pubkey_bytes, signature.as_bytes(), hash.as_bytes());

    // Check cache — no decompression needed (read lock for parallel access)
    {
        let cache = SIG_VERIFY_CACHE.read().unwrap();
        if let Some(result) = cache.get(&cache_key) {
            return if result {
                Ok(())
            } else {
                Err(CryptoError::InvalidSignature)
            };
        }
    }

    // Cache miss — decompress public key and verify
    let public_key = PublicKey::from_bytes(pubkey_bytes)?;
    let result = public_key.verify(hash.as_bytes(), signature);

    // Store result in cache (write lock only for inserts)
    {
        let mut cache = SIG_VERIFY_CACHE.write().unwrap();
        cache.insert(cache_key, result.is_ok());
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sign_verify() {
        let secret = SecretKey::generate();
        let public = secret.public_key();

        let message = b"test message";
        let sig = sign(&secret, message);

        assert!(verify(&public, message, &sig).is_ok());
        assert!(verify(&public, b"wrong", &sig).is_err());
    }

    #[test]
    fn test_signed_message() {
        let secret = SecretKey::generate();
        let public = secret.public_key();

        let signed = SignedMessage::new(&secret, b"hello".to_vec());
        assert!(signed.verify(&public).is_ok());

        // Wrong key should fail
        let other_secret = SecretKey::generate();
        let other_public = other_secret.public_key();
        assert!(signed.verify(&other_public).is_err());
    }

    #[test]
    fn test_signature_hint() {
        let secret = SecretKey::generate();
        let public = secret.public_key();

        let hint = signature_hint(&public);
        let key_bytes = public.as_bytes();

        assert_eq!(
            hint,
            [key_bytes[28], key_bytes[29], key_bytes[30], key_bytes[31]]
        );
    }

    #[test]
    fn test_sign_hash() {
        let secret = SecretKey::generate();
        let public = secret.public_key();

        let hash = Hash256::hash(b"test data");
        let sig = sign_hash(&secret, &hash);

        assert!(verify_hash(&public, &hash, &sig).is_ok());
    }
}
