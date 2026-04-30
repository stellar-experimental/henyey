//! Sealed box encryption for anonymous encrypted payloads.
//!
//! This module provides "sealed box" encryption, which allows encrypting a
//! message to a recipient's public key without revealing the sender's identity.
//! The primary use case in Stellar is encrypting survey response payloads.
//!
//! # Protocol
//!
//! Implements libsodium's `crypto_box_seal` / `crypto_box_seal_open` protocol:
//!
//! 1. An ephemeral X25519 keypair is generated for each encryption
//! 2. X25519 Diffie-Hellman derives a raw shared secret
//! 3. HSalsa20 derives a symmetric key from the shared secret
//! 4. Blake2b-24 derives a nonce from `ephemeral_pk || recipient_pk`
//! 5. XSalsa20-Poly1305 encrypts and authenticates the message
//! 6. The ephemeral public key is prepended to the ciphertext
//!
//! This provides confidentiality and authenticity, but not sender authentication
//! (the sender is anonymous).
//!
//! # Wire Format
//!
//! `[ephemeral_pk: 32 bytes][ciphertext + poly1305 tag: plaintext.len() + 16 bytes]`
//!
//! Compatible with libsodium and stellar-core's `crypto_box_seal`.
//!
//! # Ed25519 to Curve25519 Conversion
//!
//! Stellar uses Ed25519 keys, but sealed boxes require Curve25519 keys. This
//! module handles the conversion automatically when using Ed25519 keys, or
//! you can provide Curve25519 keys directly.
//!
//! # Example
//!
//! ```ignore
//! use henyey_crypto::{SecretKey, seal_to_public_key, open_from_secret_key};
//!
//! let recipient_secret = SecretKey::generate();
//! let recipient_public = recipient_secret.public_key();
//!
//! // Encrypt a message
//! let plaintext = b"secret survey response";
//! let ciphertext = seal_to_public_key(&recipient_public, plaintext).unwrap();
//!
//! // Decrypt the message
//! let decrypted = open_from_secret_key(&recipient_secret, &ciphertext).unwrap();
//! assert_eq!(decrypted, plaintext);
//! ```

use blake2::digest::consts::U24;
use blake2::{Blake2b, Digest};
use crypto_secretbox::aead::Aead;
use crypto_secretbox::{KeyInit, XSalsa20Poly1305};
use rand::rngs::OsRng;
use salsa20::cipher::consts::U10;
use salsa20::hsalsa;
use x25519_dalek::{PublicKey, StaticSecret};
use zeroize::Zeroize;

use crate::CryptoError;
#[cfg(test)]
use crate::{PublicKey as EdPublicKey, SecretKey as EdSecretKey};

/// Overhead added to plaintext: 32-byte ephemeral public key + 16-byte Poly1305 tag.
pub const SEALED_BOX_OVERHEAD: usize = 32 + 16;

/// Performs X25519 DH, checks the result is contributory, and derives the
/// XSalsa20 symmetric key via HSalsa20 (standard NaCl crypto_box key derivation).
fn derive_shared_key(
    our_secret: &StaticSecret,
    their_public: &PublicKey,
) -> Result<[u8; 32], CryptoError> {
    let shared = our_secret.diffie_hellman(their_public);
    if !shared.was_contributory() {
        return Err(CryptoError::SmallOrderPublicKey);
    }
    let mut raw = shared.to_bytes();
    let key: [u8; 32] = hsalsa::<U10>(&raw.into(), &[0u8; 16].into()).into();
    raw.zeroize();
    Ok(key)
}

/// Derives the sealed-box nonce: Blake2b-24(ephemeral_pk || recipient_pk).
fn derive_seal_nonce(ephemeral_pk: &[u8; 32], recipient_pk: &[u8; 32]) -> [u8; 24] {
    let mut hasher = Blake2b::<U24>::new();
    hasher.update(ephemeral_pk);
    hasher.update(recipient_pk);
    hasher.finalize().into()
}

fn seal(recipient_pk: &[u8; 32], plaintext: &[u8]) -> Result<Vec<u8>, CryptoError> {
    let ephemeral_secret = StaticSecret::random_from_rng(OsRng);
    let recipient_public = PublicKey::from(*recipient_pk);
    let ephemeral_public = PublicKey::from(&ephemeral_secret);
    let ephemeral_pk_bytes = ephemeral_public.to_bytes();

    let mut sym_key = derive_shared_key(&ephemeral_secret, &recipient_public)?;
    let nonce = derive_seal_nonce(&ephemeral_pk_bytes, recipient_pk);

    let cipher = XSalsa20Poly1305::new((&sym_key).into());
    sym_key.zeroize();

    let encrypted = cipher
        .encrypt((&nonce).into(), plaintext)
        .map_err(|_| CryptoError::EncryptionFailed)?;

    let mut out = Vec::with_capacity(32 + encrypted.len());
    out.extend_from_slice(&ephemeral_pk_bytes);
    out.extend_from_slice(&encrypted);
    Ok(out)
}

fn open(recipient_secret: &[u8; 32], ciphertext: &[u8]) -> Result<Vec<u8>, CryptoError> {
    // Minimum: 32-byte ephemeral_pk + 16-byte poly1305 tag
    if ciphertext.len() < SEALED_BOX_OVERHEAD {
        return Err(CryptoError::DecryptionFailed);
    }

    let ephemeral_pk: [u8; 32] = ciphertext[..32].try_into().unwrap();
    let ephemeral_public = PublicKey::from(ephemeral_pk);

    let secret = StaticSecret::from(*recipient_secret);
    let recipient_public = PublicKey::from(&secret);
    let recipient_pk_bytes = recipient_public.to_bytes();

    let mut sym_key =
        derive_shared_key(&secret, &ephemeral_public).map_err(|_| CryptoError::DecryptionFailed)?;
    let nonce = derive_seal_nonce(&ephemeral_pk, &recipient_pk_bytes);

    let cipher = XSalsa20Poly1305::new((&sym_key).into());
    sym_key.zeroize();

    cipher
        .decrypt((&nonce).into(), &ciphertext[32..])
        .map_err(|_| CryptoError::DecryptionFailed)
}

/// Encrypts a payload to a recipient's Ed25519 public key.
///
/// The Ed25519 public key is converted to Curve25519 internally. The returned
/// ciphertext includes the ephemeral public key and authentication tag.
///
/// # Errors
///
/// Returns [`CryptoError::EncryptionFailed`] if encryption fails (rare, typically
/// only on RNG failure).
#[cfg(test)]
fn seal_to_public_key(recipient: &EdPublicKey, plaintext: &[u8]) -> Result<Vec<u8>, CryptoError> {
    // Ed25519-to-Curve25519 conversion always produces contributory points
    let curve_pk = recipient.to_curve25519_bytes();
    seal(&curve_pk, plaintext)
}

/// Encrypts a payload to a Curve25519 public key.
///
/// Use this when you already have a Curve25519 key rather than an Ed25519 key.
///
/// # Errors
///
/// Returns [`CryptoError::SmallOrderPublicKey`] if the recipient key is small-order.
/// Returns [`CryptoError::EncryptionFailed`] if encryption fails.
pub fn seal_to_curve25519_public_key(
    recipient: &[u8; 32],
    plaintext: &[u8],
) -> Result<Vec<u8>, CryptoError> {
    seal(recipient, plaintext).map_err(|e| match e {
        CryptoError::SmallOrderPublicKey => CryptoError::SmallOrderPublicKey,
        _ => CryptoError::EncryptionFailed,
    })
}

/// Decrypts a sealed payload using the recipient's Ed25519 secret key.
///
/// The Ed25519 secret key is converted to Curve25519 internally.
///
/// # Errors
///
/// Returns [`CryptoError::DecryptionFailed`] if:
/// - The ciphertext was tampered with
/// - The wrong key was used
/// - The ciphertext is malformed
#[cfg(test)]
fn open_from_secret_key(
    recipient: &EdSecretKey,
    ciphertext: &[u8],
) -> Result<Vec<u8>, CryptoError> {
    let curve_sk = recipient.to_curve25519_bytes();
    open(&curve_sk, ciphertext)
}

/// Decrypts a sealed payload using a Curve25519 secret key.
///
/// Use this when you already have a Curve25519 key rather than an Ed25519 key.
///
/// # Errors
///
/// Returns [`CryptoError::DecryptionFailed`] if decryption fails.
pub fn open_from_curve25519_secret_key(
    recipient: &[u8; 32],
    ciphertext: &[u8],
) -> Result<Vec<u8>, CryptoError> {
    open(recipient, ciphertext)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- G3: Survey crypto roundtrip tests ----

    #[test]
    fn test_seal_open_ed25519_roundtrip_g3() {
        // Encrypt with Ed25519 public key, decrypt with Ed25519 secret key.
        let secret = EdSecretKey::generate();
        let public = secret.public_key();
        let plaintext = b"survey response payload";

        let ciphertext = seal_to_public_key(&public, plaintext).unwrap();

        // Ciphertext should be longer than plaintext (ephemeral key + tag overhead)
        assert!(ciphertext.len() > plaintext.len());

        let decrypted = open_from_secret_key(&secret, &ciphertext).unwrap();
        assert_eq!(
            decrypted, plaintext,
            "decrypted should match original plaintext"
        );
    }

    #[test]
    fn test_seal_open_curve25519_roundtrip_g3() {
        // Encrypt/decrypt using raw Curve25519 keys (the path used by survey_impl.rs).
        // This matches: seal_to_curve25519_public_key + open_from_curve25519_secret_key.
        let curve_secret = x25519_dalek::StaticSecret::random_from_rng(rand::rngs::OsRng);
        let curve_public = x25519_dalek::PublicKey::from(&curve_secret);

        let secret_bytes: [u8; 32] = curve_secret.to_bytes();
        let public_bytes: [u8; 32] = *curve_public.as_bytes();

        let plaintext = b"encrypted survey topology data";
        let ciphertext = seal_to_curve25519_public_key(&public_bytes, plaintext).unwrap();

        assert!(ciphertext.len() > plaintext.len());

        let decrypted = open_from_curve25519_secret_key(&secret_bytes, &ciphertext).unwrap();
        assert_eq!(decrypted, plaintext);
    }

    #[test]
    fn test_decryption_with_wrong_key_fails_g3() {
        // Encrypting to one key and decrypting with another must fail.
        let secret_a = EdSecretKey::generate();
        let public_a = secret_a.public_key();
        let secret_b = EdSecretKey::generate();

        let plaintext = b"secret data";
        let ciphertext = seal_to_public_key(&public_a, plaintext).unwrap();

        let result = open_from_secret_key(&secret_b, &ciphertext);
        assert!(result.is_err(), "decryption with wrong key should fail");
        assert!(matches!(result, Err(CryptoError::DecryptionFailed)));
    }

    #[test]
    fn test_tampered_ciphertext_fails_g3() {
        // Authenticated encryption should reject tampered ciphertext.
        let secret = EdSecretKey::generate();
        let public = secret.public_key();
        let plaintext = b"integrity-protected data";

        let mut ciphertext = seal_to_public_key(&public, plaintext).unwrap();

        // Tamper with the last byte (inside the encrypted payload, past the ephemeral key)
        let last = ciphertext.len() - 1;
        ciphertext[last] ^= 0xff;

        let result = open_from_secret_key(&secret, &ciphertext);
        assert!(
            result.is_err(),
            "tampered ciphertext should fail decryption"
        );
    }

    #[test]
    fn test_empty_plaintext_roundtrip_g3() {
        // Edge case: empty plaintext should still work.
        let secret = EdSecretKey::generate();
        let public = secret.public_key();
        let plaintext = b"";

        let ciphertext = seal_to_public_key(&public, plaintext).unwrap();
        let decrypted = open_from_secret_key(&secret, &ciphertext).unwrap();
        assert_eq!(decrypted, plaintext.as_slice());
    }

    #[test]
    fn test_large_plaintext_roundtrip_g3() {
        // Survey responses can be large. Test with a realistic payload size.
        let secret = EdSecretKey::generate();
        let public = secret.public_key();
        let plaintext = vec![0xABu8; 4096]; // 4 KB payload

        let ciphertext = seal_to_public_key(&public, &plaintext).unwrap();
        let decrypted = open_from_secret_key(&secret, &ciphertext).unwrap();
        assert_eq!(decrypted, plaintext);
    }

    #[test]
    fn test_each_encryption_produces_different_ciphertext_g3() {
        // Sealed boxes use ephemeral keys, so encrypting the same plaintext twice
        // should produce different ciphertexts (non-deterministic).
        let secret = EdSecretKey::generate();
        let public = secret.public_key();
        let plaintext = b"same payload";

        let ct1 = seal_to_public_key(&public, plaintext).unwrap();
        let ct2 = seal_to_public_key(&public, plaintext).unwrap();

        assert_ne!(
            ct1, ct2,
            "sealed box encryption should be non-deterministic"
        );

        // Both should decrypt to the same plaintext
        let pt1 = open_from_secret_key(&secret, &ct1).unwrap();
        let pt2 = open_from_secret_key(&secret, &ct2).unwrap();
        assert_eq!(pt1, plaintext.as_slice());
        assert_eq!(pt2, plaintext.as_slice());
    }

    #[test]
    fn test_curve25519_wrong_key_fails_g3() {
        // Same wrong-key test but for the Curve25519 path used by surveys.
        let sk_a = x25519_dalek::StaticSecret::random_from_rng(rand::rngs::OsRng);
        let pk_a = x25519_dalek::PublicKey::from(&sk_a);
        let sk_b = x25519_dalek::StaticSecret::random_from_rng(rand::rngs::OsRng);

        let plaintext = b"survey data";
        let ciphertext = seal_to_curve25519_public_key(pk_a.as_bytes(), plaintext).unwrap();

        let result = open_from_curve25519_secret_key(&sk_b.to_bytes(), &ciphertext);
        assert!(result.is_err(), "wrong Curve25519 key should fail");
    }

    #[test]
    fn test_seal_rejects_zero_public_key() {
        let result = seal_to_curve25519_public_key(&[0u8; 32], b"plaintext");
        assert!(result.is_err(), "seal with all-zeros recipient should fail");
        assert!(matches!(result, Err(CryptoError::SmallOrderPublicKey)));
    }

    #[test]
    fn test_open_rejects_small_order_ephemeral_key() {
        let secret = x25519_dalek::StaticSecret::random_from_rng(rand::rngs::OsRng);
        let mut ciphertext = vec![0u8; 48];
        ciphertext[..32].copy_from_slice(&[0u8; 32]);

        let result = open_from_curve25519_secret_key(&secret.to_bytes(), &ciphertext);
        assert!(
            result.is_err(),
            "open with small-order ephemeral key should fail"
        );
    }

    // ---- New tests ----

    #[test]
    fn test_deterministic_vector_wire_compatibility() {
        // This vector was captured from the crypto_box 0.9.x implementation
        // to prove wire-format compatibility with libsodium's crypto_box_seal.
        let recipient_secret: [u8; 32] = [
            0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e,
            0x0f, 0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x1b, 0x1c,
            0x1d, 0x1e, 0x1f, 0x20,
        ];

        let sealed_hex = "64b101b1d0be5a8704bd078f9895001fc03e8e9f9522f188dd128d9846d4846673c0fedc538f220290c003c4b9e4d2688b427b23374e4a5d00068691c238293e5bc754ff6824";
        let sealed = hex::decode(sealed_hex).unwrap();
        let plaintext = b"test sealed box vector";

        let decrypted = open_from_curve25519_secret_key(&recipient_secret, &sealed).unwrap();
        assert_eq!(decrypted, plaintext.as_slice());
    }

    #[test]
    fn test_nonce_derivation() {
        // Verify nonce derivation matches expected output for known inputs
        let ephemeral_pk: [u8; 32] = [
            0x64, 0xb1, 0x01, 0xb1, 0xd0, 0xbe, 0x5a, 0x87, 0x04, 0xbd, 0x07, 0x8f, 0x98, 0x95,
            0x00, 0x1f, 0xc0, 0x3e, 0x8e, 0x9f, 0x95, 0x22, 0xf1, 0x88, 0xdd, 0x12, 0x8d, 0x98,
            0x46, 0xd4, 0x84, 0x66,
        ];
        let recipient_pk: [u8; 32] = [
            0x07, 0xa3, 0x7c, 0xbc, 0x14, 0x20, 0x93, 0xc8, 0xb7, 0x55, 0xdc, 0x1b, 0x10, 0xe8,
            0x6c, 0xb4, 0x26, 0x37, 0x4a, 0xd1, 0x6a, 0xa8, 0x53, 0xed, 0x0b, 0xdf, 0xc0, 0xb2,
            0xb8, 0x6d, 0x1c, 0x7c,
        ];
        let expected_nonce: [u8; 24] = [
            0x8f, 0x16, 0xb4, 0x75, 0x15, 0xc4, 0xd2, 0xd7, 0x20, 0x91, 0xfe, 0x20, 0x5e, 0x3a,
            0x60, 0x1f, 0x02, 0x91, 0xc7, 0xe2, 0x9b, 0x2f, 0xbd, 0x2c,
        ];

        let nonce = derive_seal_nonce(&ephemeral_pk, &recipient_pk);
        assert_eq!(nonce, expected_nonce);
    }

    #[test]
    fn test_malformed_ciphertext_lengths() {
        let secret = x25519_dalek::StaticSecret::random_from_rng(rand::rngs::OsRng);
        let sk_bytes = secret.to_bytes();

        // All lengths below SEALED_BOX_OVERHEAD (48) must fail
        for len in [0, 1, 16, 31, 32, 33, 47] {
            let ciphertext = vec![0x42u8; len];
            let result = open_from_curve25519_secret_key(&sk_bytes, &ciphertext);
            assert!(result.is_err(), "ciphertext of length {} should fail", len);
            assert!(
                matches!(result, Err(CryptoError::DecryptionFailed)),
                "ciphertext of length {} should return DecryptionFailed",
                len
            );
        }
    }

    #[test]
    fn test_overhead_constant() {
        let secret = EdSecretKey::generate();
        let public = secret.public_key();
        let plaintext = b"measure overhead";

        let ciphertext = seal_to_public_key(&public, plaintext).unwrap();
        assert_eq!(ciphertext.len(), plaintext.len() + SEALED_BOX_OVERHEAD);
    }

    /// Tests that the Ed25519 open path also correctly rejects malformed
    /// ciphertexts of various lengths, mirroring test_malformed_ciphertext_lengths
    /// for the Curve25519 path.
    #[test]
    fn test_malformed_ciphertext_lengths_ed25519_path() {
        let secret = EdSecretKey::generate();

        // All lengths below SEALED_BOX_OVERHEAD (48) must fail
        for len in [0, 1, 16, 31, 32, 33, 47] {
            let ciphertext = vec![0x42u8; len];
            let result = open_from_secret_key(&secret, &ciphertext);
            assert!(result.is_err(), "ciphertext of length {} should fail", len);
            assert!(
                matches!(result, Err(CryptoError::DecryptionFailed)),
                "ciphertext of length {} should return DecryptionFailed",
                len
            );
        }
    }

    /// Tests that a ciphertext at the minimum overhead size (48 bytes) with a
    /// valid-looking ephemeral key but invalid auth tag is rejected.
    #[test]
    fn test_minimum_length_ciphertext_auth_failure() {
        let secret = x25519_dalek::StaticSecret::random_from_rng(rand::rngs::OsRng);
        let sk_bytes = secret.to_bytes();

        // Use a valid (non-small-order) ephemeral public key so we pass the
        // contributory check, then append a bogus 16-byte tag.
        let ephemeral = x25519_dalek::StaticSecret::random_from_rng(rand::rngs::OsRng);
        let ephemeral_pk = x25519_dalek::PublicKey::from(&ephemeral);

        let mut ciphertext = Vec::with_capacity(48);
        ciphertext.extend_from_slice(ephemeral_pk.as_bytes());
        ciphertext.extend_from_slice(&[0xAA; 16]); // bogus poly1305 tag

        let result = open_from_curve25519_secret_key(&sk_bytes, &ciphertext);
        assert!(result.is_err());
        assert!(matches!(result, Err(CryptoError::DecryptionFailed)));
    }
}

#[cfg(test)]
mod deterministic_seal_tests {
    use super::*;

    /// Seal with a fixed ephemeral secret to produce deterministic output.
    /// This proves the seal side produces the exact wire format.
    fn seal_deterministic(
        ephemeral_secret_bytes: &[u8; 32],
        recipient_pk: &[u8; 32],
        plaintext: &[u8],
    ) -> Result<Vec<u8>, CryptoError> {
        let ephemeral_secret = StaticSecret::from(*ephemeral_secret_bytes);
        let recipient_public = PublicKey::from(*recipient_pk);
        let ephemeral_public = PublicKey::from(&ephemeral_secret);
        let ephemeral_pk_bytes = ephemeral_public.to_bytes();

        let mut sym_key = derive_shared_key(&ephemeral_secret, &recipient_public)?;
        let nonce = derive_seal_nonce(&ephemeral_pk_bytes, recipient_pk);

        let cipher = XSalsa20Poly1305::new((&sym_key).into());
        sym_key.zeroize();

        let encrypted = cipher
            .encrypt((&nonce).into(), plaintext)
            .map_err(|_| CryptoError::EncryptionFailed)?;

        let mut out = Vec::with_capacity(32 + encrypted.len());
        out.extend_from_slice(&ephemeral_pk_bytes);
        out.extend_from_slice(&encrypted);
        Ok(out)
    }

    #[test]
    fn test_seal_deterministic_vector() {
        // Same keys used in the decrypt-side compatibility test.
        let recipient_secret: [u8; 32] = [
            0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e,
            0x0f, 0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x1b, 0x1c,
            0x1d, 0x1e, 0x1f, 0x20,
        ];
        let ephemeral_secret: [u8; 32] = [
            0x41, 0x42, 0x43, 0x44, 0x45, 0x46, 0x47, 0x48, 0x49, 0x4a, 0x4b, 0x4c, 0x4d, 0x4e,
            0x4f, 0x50, 0x51, 0x52, 0x53, 0x54, 0x55, 0x56, 0x57, 0x58, 0x59, 0x5a, 0x5b, 0x5c,
            0x5d, 0x5e, 0x5f, 0x60,
        ];
        let recipient_sk = StaticSecret::from(recipient_secret);
        let recipient_pk = PublicKey::from(&recipient_sk);
        let plaintext = b"test sealed box vector";

        // Expected output captured from crypto_box 0.9.x
        let expected_hex = "64b101b1d0be5a8704bd078f9895001fc03e8e9f9522f188dd128d9846d4846673c0fedc538f220290c003c4b9e4d2688b427b23374e4a5d00068691c238293e5bc754ff6824";

        let sealed =
            seal_deterministic(&ephemeral_secret, recipient_pk.as_bytes(), plaintext).unwrap();
        assert_eq!(hex::encode(&sealed), expected_hex);

        // Verify roundtrip
        let decrypted = open(&recipient_secret, &sealed).unwrap();
        assert_eq!(decrypted, plaintext);
    }
}
