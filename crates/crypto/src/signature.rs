//! Signature verification with caching.
//!
//! This module provides cached signature verification for Ed25519, matching
//! stellar-core's `gVerifySigCache`. Within a TX the same signature is verified
//! 2+N times (N = num ops); across flood→apply each TX signature is verified
//! twice. The cache reduces all repeated verifications to a HashMap lookup.

use once_cell::sync::Lazy;
use std::collections::{HashMap, VecDeque};
use std::sync::RwLock;

use crate::error::CryptoError;
use crate::keys::{PublicKey, SecretKey, Signature};
use henyey_common::Hash256;

/// Default capacity for the signature verification cache, matching stellar-core's
/// `gVerifySigCache` (250K entries) in `SecretKey.cpp`.
const SIG_CACHE_CAPACITY: usize = 250_000;

/// Global ed25519 signature verification cache.
///
/// Keyed by BLAKE2b-256(pubkey || signature || message_hash). Matches stellar-core's
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
    use blake2::Digest as _;
    let mut hasher = crate::hash::Blake2b256::new();
    hasher.update(pubkey);
    hasher.update(sig);
    hasher.update(hash);
    hasher.finalize().into()
}

/// Signs a hash value.
///
/// This signs the raw 32 bytes of the hash. Use this when signing transaction
/// hashes or other pre-hashed data.
pub fn sign_hash(secret_key: &SecretKey, hash: &Hash256) -> Signature {
    secret_key.sign(hash.as_bytes())
}

/// Verifies a signature over a hash value from raw public key bytes.
///
/// Accepts raw 32-byte public key bytes to avoid ed25519 point decompression
/// (~35μs) on cache hits. Matches stellar-core's `PubKeyUtils::verifySig`
/// which checks the signature cache using raw bytes before touching the
/// crypto library.
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
        let cache = SIG_VERIFY_CACHE
            .read()
            .expect("signature cache lock poisoned");
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
        let mut cache = SIG_VERIFY_CACHE
            .write()
            .expect("signature cache lock poisoned");
        cache.insert(cache_key, result.is_ok());
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sign_and_verify_hash() {
        let secret = SecretKey::generate();
        let public = secret.public_key();

        let hash = Hash256::hash(b"test data");
        let sig = sign_hash(&secret, &hash);

        assert!(verify_hash_from_raw_key(public.as_bytes(), &hash, &sig).is_ok());

        // Wrong key should fail
        let other = SecretKey::generate();
        assert!(verify_hash_from_raw_key(other.public_key().as_bytes(), &hash, &sig).is_err());
    }

    #[test]
    fn test_cache_hit_returns_same_result() {
        let secret = SecretKey::generate();
        let public = secret.public_key();

        let hash = Hash256::hash(b"cached data");
        let sig = sign_hash(&secret, &hash);

        // First call populates cache
        assert!(verify_hash_from_raw_key(public.as_bytes(), &hash, &sig).is_ok());
        // Second call should hit cache and return the same result
        assert!(verify_hash_from_raw_key(public.as_bytes(), &hash, &sig).is_ok());
    }
}
