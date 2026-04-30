//! Signature verification with caching.
//!
//! This module provides cached signature verification for Ed25519, matching
//! stellar-core's `gVerifySigCache`. Within a TX the same signature is verified
//! 2+N times (N = num ops); across flood→apply each TX signature is verified
//! twice. The cache reduces all repeated verifications to a HashMap lookup.
//!
//! The cache uses random-two-choice eviction matching stellar-core's
//! `RandomEvictionCache` to resist adversarial cache-churn attacks.
//!
//! All verification paths (hash-based and payload-based) route through the same
//! global cache, matching stellar-core where both `verifySig(key, sig, hash)`
//! and `verifyEd25519SignedPayload` use `gVerifySigCache`.

use once_cell::sync::Lazy;
use std::sync::Mutex;

use crate::error::CryptoError;
use crate::keys::{PublicKey, SecretKey, Signature};
use crate::random_eviction_cache::RandomEvictionCache;
use henyey_common::Hash256;

/// Default capacity for the signature verification cache, matching stellar-core's
/// `gVerifySigCache` (250K entries) in `SecretKey.cpp`.
const SIG_CACHE_CAPACITY: usize = 250_000;

/// Cached signature verification outcome.
///
/// Richer than stellar-core's `bool` so that error semantics are stable across
/// cache miss and hit — the same error variant is always returned for the same
/// input regardless of whether the result came from cache.
#[derive(Clone, Copy, PartialEq, Eq)]
enum VerifyOutcome {
    /// Signature is valid.
    Valid,
    /// Public key decompressed successfully but signature check failed.
    InvalidSignature,
    /// Public key bytes are not a valid ed25519 point.
    InvalidPublicKey,
}

/// Global ed25519 signature verification cache.
///
/// Keyed by BLAKE2b-256(pubkey || signature || message). Matches stellar-core's
/// global `gVerifySigCache` which persists across the validator lifetime so that
/// signatures verified during flood/nomination get cache hits during apply.
///
/// Uses Mutex matching stellar-core's `gVerifySigCacheMutex` since every access
/// mutates state (generation counter for LRU tracking).
static SIG_VERIFY_CACHE: Lazy<Mutex<RandomEvictionCache<[u8; 32], VerifyOutcome>>> =
    Lazy::new(|| Mutex::new(RandomEvictionCache::new(SIG_CACHE_CAPACITY)));

fn compute_cache_key(pubkey: &[u8; 32], sig: &[u8; 64], message: &[u8]) -> [u8; 32] {
    use blake2::Digest as _;
    let mut hasher = crate::hash::Blake2b256::new();
    hasher.update(pubkey);
    hasher.update(sig);
    hasher.update(message);
    hasher.finalize().into()
}

/// Signs a hash value.
///
/// This signs the raw 32 bytes of the hash. Use this when signing transaction
/// hashes or other pre-hashed data.
pub fn sign_hash(secret_key: &SecretKey, hash: &Hash256) -> Signature {
    secret_key.sign(hash.as_bytes())
}

/// Cached signature verification against arbitrary message bytes.
///
/// Matches stellar-core's `PubKeyUtils::verifySig(key, sig, bin)` — all
/// verification paths (hash-based and payload-based) route through the same
/// global cache. The cache key is BLAKE2b-256(pubkey || signature || message).
///
/// # Errors
///
/// Returns [`CryptoError::InvalidSignature`] if signature verification fails.
/// Returns [`CryptoError::InvalidPublicKey`] if the raw bytes are not a valid
/// ed25519 public key. Both outcomes are cached for subsequent lookups.
pub fn verify_from_raw_key(
    pubkey_bytes: &[u8; 32],
    message: &[u8],
    signature: &Signature,
) -> Result<(), CryptoError> {
    let cache_key = compute_cache_key(pubkey_bytes, signature.as_bytes(), message);

    // Check cache (mutex held only for lookup, not verification)
    {
        let mut cache = SIG_VERIFY_CACHE
            .lock()
            .expect("signature cache lock poisoned");
        if let Some(&outcome) = cache.get(&cache_key) {
            return match outcome {
                VerifyOutcome::Valid => Ok(()),
                VerifyOutcome::InvalidSignature => Err(CryptoError::InvalidSignature),
                VerifyOutcome::InvalidPublicKey => Err(CryptoError::InvalidPublicKey),
            };
        }
    }

    // Cache miss — decompress public key and verify
    let outcome = match PublicKey::from_bytes(pubkey_bytes) {
        Ok(pk) => {
            if pk.verify(message, signature).is_ok() {
                VerifyOutcome::Valid
            } else {
                VerifyOutcome::InvalidSignature
            }
        }
        Err(_) => VerifyOutcome::InvalidPublicKey,
    };

    // Store result in cache
    {
        let mut cache = SIG_VERIFY_CACHE
            .lock()
            .expect("signature cache lock poisoned");
        cache.put(cache_key, outcome);
    }

    match outcome {
        VerifyOutcome::Valid => Ok(()),
        VerifyOutcome::InvalidSignature => Err(CryptoError::InvalidSignature),
        VerifyOutcome::InvalidPublicKey => Err(CryptoError::InvalidPublicKey),
    }
}

/// Verifies a signature over a hash value from raw public key bytes.
///
/// Convenience wrapper around [`verify_from_raw_key`] for hash-based
/// verification. Accepts raw 32-byte public key bytes to avoid ed25519 point
/// decompression (~35μs) on cache hits.
///
/// # Errors
///
/// Returns [`CryptoError::InvalidSignature`] if verification fails.
/// Returns [`CryptoError::InvalidPublicKey`] if the raw bytes are not a valid
/// ed25519 public key.
pub fn verify_hash_from_raw_key(
    pubkey_bytes: &[u8; 32],
    hash: &Hash256,
    signature: &Signature,
) -> Result<(), CryptoError> {
    verify_from_raw_key(pubkey_bytes, hash.as_bytes(), signature)
}

/// Verify a single decorated signature against an Ed25519 signed-payload
/// signer key per CAP-0040.
///
/// Checks the XOR hint (pubkey hint ⊕ payload hint) against `sig.hint`,
/// parses the signature bytes, and delegates to cached verification via
/// [`verify_from_raw_key`].
///
/// Returns `true` if and only if the hint matches AND the signature is
/// cryptographically valid against the payload.
///
/// # Preconditions
///
/// This function does NOT enforce CAP-0040 validity constraints (e.g.,
/// non-empty payload). Higher layers (tx validation, set-options) are
/// responsible for rejecting invalid signed-payload signer keys before
/// reaching verification.
///
/// # Parity
///
/// Mirrors stellar-core's `SignatureUtils::verifyEd25519SignedPayload`
/// and `SignatureUtils::getSignedPayloadHint` in
/// `src/transactions/SignatureUtils.cpp`.
pub fn verify_ed25519_signed_payload(
    sig: &stellar_xdr::curr::DecoratedSignature,
    signed_payload: &stellar_xdr::curr::SignerKeyEd25519SignedPayload,
) -> bool {
    // Compute expected XOR hint per stellar-core getSignedPayloadHint.
    let pubkey_hint = [
        signed_payload.ed25519.0[28],
        signed_payload.ed25519.0[29],
        signed_payload.ed25519.0[30],
        signed_payload.ed25519.0[31],
    ];
    let payload_hint = if signed_payload.payload.len() >= 4 {
        let len = signed_payload.payload.len();
        [
            signed_payload.payload[len - 4],
            signed_payload.payload[len - 3],
            signed_payload.payload[len - 2],
            signed_payload.payload[len - 1],
        ]
    } else {
        // For shorter payloads, copy bytes left-aligned, remainder zero.
        let mut hint = [0u8; 4];
        for (i, &byte) in signed_payload.payload.iter().enumerate() {
            hint[i] = byte;
        }
        hint
    };
    let expected_hint = [
        pubkey_hint[0] ^ payload_hint[0],
        pubkey_hint[1] ^ payload_hint[1],
        pubkey_hint[2] ^ payload_hint[2],
        pubkey_hint[3] ^ payload_hint[3],
    ];

    if sig.hint.0 != expected_hint {
        return false;
    }

    let Ok(ed_sig) = Signature::try_from(&sig.signature) else {
        return false;
    };

    verify_from_raw_key(&signed_payload.ed25519.0, &signed_payload.payload, &ed_sig).is_ok()
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

    #[test]
    fn test_invalid_pubkey_cached_consistently() {
        // y=2 is not on the ed25519 curve (u/v is not a quadratic residue).
        let mut invalid_key = [0u8; 32];
        invalid_key[0] = 2;
        assert!(
            PublicKey::from_bytes(&invalid_key).is_err(),
            "test setup: expected invalid key for y=2"
        );
        let sig = Signature::from_bytes([0u8; 64]);
        let hash = Hash256::hash(b"test invalid key caching");

        // First call (cache miss) — should return InvalidPublicKey
        let err1 = verify_hash_from_raw_key(&invalid_key, &hash, &sig).unwrap_err();
        assert!(
            matches!(err1, CryptoError::InvalidPublicKey),
            "expected InvalidPublicKey on miss, got: {err1:?}"
        );

        // Second call (cache hit) — should return the same error
        let err2 = verify_hash_from_raw_key(&invalid_key, &hash, &sig).unwrap_err();
        assert!(
            matches!(err2, CryptoError::InvalidPublicKey),
            "expected InvalidPublicKey on hit, got: {err2:?}"
        );
    }

    #[test]
    fn test_invalid_signature_cached_consistently() {
        let secret = SecretKey::generate();
        let public = secret.public_key();
        let hash = Hash256::hash(b"test invalid sig caching");
        // Wrong signature bytes
        let bad_sig = Signature::from_bytes([0xAB; 64]);

        // First call (cache miss) — should return InvalidSignature
        let err1 = verify_hash_from_raw_key(public.as_bytes(), &hash, &bad_sig).unwrap_err();
        assert!(
            matches!(err1, CryptoError::InvalidSignature),
            "expected InvalidSignature on miss, got: {err1:?}"
        );

        // Second call (cache hit) — should return the same error
        let err2 = verify_hash_from_raw_key(public.as_bytes(), &hash, &bad_sig).unwrap_err();
        assert!(
            matches!(err2, CryptoError::InvalidSignature),
            "expected InvalidSignature on hit, got: {err2:?}"
        );
    }

    #[test]
    fn test_verify_from_raw_key_with_payload() {
        let secret = SecretKey::generate();
        let public = secret.public_key();

        // Sign arbitrary payload (not a hash)
        let payload = b"CAP-0040 signed payload data of arbitrary length";
        let sig = secret.sign(payload);

        // Verification with arbitrary message bytes
        assert!(verify_from_raw_key(public.as_bytes(), payload, &sig).is_ok());

        // Wrong payload should fail
        assert!(verify_from_raw_key(public.as_bytes(), b"wrong payload", &sig).is_err());
    }

    #[test]
    fn test_cache_independence() {
        let secret = SecretKey::generate();
        let public = secret.public_key();

        let msg1 = b"message one";
        let msg2 = b"message two";
        let sig1 = secret.sign(msg1);
        let sig2 = secret.sign(msg2);

        // Both should verify correctly (different cache entries)
        assert!(verify_from_raw_key(public.as_bytes(), msg1, &sig1).is_ok());
        assert!(verify_from_raw_key(public.as_bytes(), msg2, &sig2).is_ok());

        // Cross-verification should fail
        assert!(verify_from_raw_key(public.as_bytes(), msg1, &sig2).is_err());
        assert!(verify_from_raw_key(public.as_bytes(), msg2, &sig1).is_err());
    }

    /// Helper to build a DecoratedSignature + SignerKeyEd25519SignedPayload for tests.
    fn make_signed_payload_fixture(
        payload: &[u8],
    ) -> (
        SecretKey,
        stellar_xdr::curr::DecoratedSignature,
        stellar_xdr::curr::SignerKeyEd25519SignedPayload,
    ) {
        use stellar_xdr::curr::{
            DecoratedSignature, SignatureHint, SignerKeyEd25519SignedPayload, Uint256,
        };

        let secret = SecretKey::generate();
        let pubkey_bytes = *secret.public_key().as_bytes();

        let signed_payload = SignerKeyEd25519SignedPayload {
            ed25519: Uint256(pubkey_bytes),
            payload: payload.try_into().expect("payload too long for VecM"),
        };

        // Sign the payload
        let sig = secret.sign(payload);

        // Compute the XOR hint
        let pubkey_hint = [
            pubkey_bytes[28],
            pubkey_bytes[29],
            pubkey_bytes[30],
            pubkey_bytes[31],
        ];
        let payload_hint = if payload.len() >= 4 {
            let len = payload.len();
            [
                payload[len - 4],
                payload[len - 3],
                payload[len - 2],
                payload[len - 1],
            ]
        } else {
            let mut hint = [0u8; 4];
            for (i, &byte) in payload.iter().enumerate() {
                hint[i] = byte;
            }
            hint
        };
        let xor_hint = [
            pubkey_hint[0] ^ payload_hint[0],
            pubkey_hint[1] ^ payload_hint[1],
            pubkey_hint[2] ^ payload_hint[2],
            pubkey_hint[3] ^ payload_hint[3],
        ];

        let decorated = DecoratedSignature {
            hint: SignatureHint(xor_hint),
            signature: stellar_xdr::curr::Signature(sig.as_bytes().try_into().unwrap()),
        };

        (secret, decorated, signed_payload)
    }

    #[test]
    fn test_verify_ed25519_signed_payload_valid() {
        let payload = b"CAP-0040 test payload with enough bytes";
        let (_secret, sig, signed_payload) = make_signed_payload_fixture(payload);
        assert!(verify_ed25519_signed_payload(&sig, &signed_payload));
    }

    #[test]
    fn test_verify_ed25519_signed_payload_hint_mismatch() {
        use stellar_xdr::curr::{DecoratedSignature, SignatureHint};

        let payload = b"CAP-0040 test payload";
        let (_secret, sig, signed_payload) = make_signed_payload_fixture(payload);

        // Corrupt the hint
        let bad_sig = DecoratedSignature {
            hint: SignatureHint([0xFF, 0xFF, 0xFF, 0xFF]),
            signature: sig.signature.clone(),
        };
        assert!(!verify_ed25519_signed_payload(&bad_sig, &signed_payload));
    }

    #[test]
    fn test_verify_ed25519_signed_payload_short_payload() {
        // 1-byte payload
        let (_secret, sig, sp) = make_signed_payload_fixture(&[0xAB]);
        assert!(verify_ed25519_signed_payload(&sig, &sp));

        // 2-byte payload
        let (_secret, sig, sp) = make_signed_payload_fixture(&[0xAB, 0xCD]);
        assert!(verify_ed25519_signed_payload(&sig, &sp));

        // 3-byte payload
        let (_secret, sig, sp) = make_signed_payload_fixture(&[0xAB, 0xCD, 0xEF]);
        assert!(verify_ed25519_signed_payload(&sig, &sp));
    }

    #[test]
    fn test_verify_ed25519_signed_payload_empty_payload() {
        let (_secret, sig, sp) = make_signed_payload_fixture(&[]);
        assert!(verify_ed25519_signed_payload(&sig, &sp));
    }

    #[test]
    fn test_verify_ed25519_signed_payload_invalid_signature() {
        use stellar_xdr::curr::{DecoratedSignature, SignatureHint, Uint256};

        let secret = SecretKey::generate();
        let pubkey_bytes = *secret.public_key().as_bytes();
        let payload = b"test payload data";

        let signed_payload = stellar_xdr::curr::SignerKeyEd25519SignedPayload {
            ed25519: Uint256(pubkey_bytes),
            payload: payload.as_slice().try_into().unwrap(),
        };

        // Compute correct hint but use garbage signature
        let pubkey_hint = [
            pubkey_bytes[28],
            pubkey_bytes[29],
            pubkey_bytes[30],
            pubkey_bytes[31],
        ];
        let len = payload.len();
        let payload_hint = [
            payload[len - 4],
            payload[len - 3],
            payload[len - 2],
            payload[len - 1],
        ];
        let xor_hint = [
            pubkey_hint[0] ^ payload_hint[0],
            pubkey_hint[1] ^ payload_hint[1],
            pubkey_hint[2] ^ payload_hint[2],
            pubkey_hint[3] ^ payload_hint[3],
        ];

        let bad_sig = DecoratedSignature {
            hint: SignatureHint(xor_hint),
            signature: stellar_xdr::curr::Signature(vec![0xDE; 64].try_into().unwrap()),
        };
        assert!(!verify_ed25519_signed_payload(&bad_sig, &signed_payload));
    }

    #[test]
    fn test_verify_ed25519_signed_payload_invalid_key() {
        use stellar_xdr::curr::{DecoratedSignature, SignatureHint, Uint256};

        // Use y=2 which is not on curve
        let mut invalid_key = [0u8; 32];
        invalid_key[0] = 2;
        let payload = b"test payload";

        let signed_payload = stellar_xdr::curr::SignerKeyEd25519SignedPayload {
            ed25519: Uint256(invalid_key),
            payload: payload.as_slice().try_into().unwrap(),
        };

        // Compute hint with the invalid key
        let pubkey_hint = [
            invalid_key[28],
            invalid_key[29],
            invalid_key[30],
            invalid_key[31],
        ];
        let len = payload.len();
        let payload_hint = [
            payload[len - 4],
            payload[len - 3],
            payload[len - 2],
            payload[len - 1],
        ];
        let xor_hint = [
            pubkey_hint[0] ^ payload_hint[0],
            pubkey_hint[1] ^ payload_hint[1],
            pubkey_hint[2] ^ payload_hint[2],
            pubkey_hint[3] ^ payload_hint[3],
        ];

        let sig = DecoratedSignature {
            hint: SignatureHint(xor_hint),
            signature: stellar_xdr::curr::Signature(vec![0xAB; 64].try_into().unwrap()),
        };
        assert!(!verify_ed25519_signed_payload(&sig, &signed_payload));
    }
}
