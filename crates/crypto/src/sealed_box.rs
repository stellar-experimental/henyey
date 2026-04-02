//! Sealed box encryption for anonymous encrypted payloads.
//!
//! This module provides "sealed box" encryption, which allows encrypting a
//! message to a recipient's public key without revealing the sender's identity.
//! The primary use case in Stellar is encrypting survey response payloads.
//!
//! # How It Works
//!
//! Sealed boxes use X25519 key exchange combined with XSalsa20-Poly1305
//! authenticated encryption:
//!
//! 1. An ephemeral keypair is generated for each encryption
//! 2. X25519 key exchange derives a shared secret
//! 3. XSalsa20-Poly1305 encrypts and authenticates the message
//! 4. The ephemeral public key is prepended to the ciphertext
//!
//! This provides confidentiality and authenticity, but not sender authentication
//! (the sender is anonymous).
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

use crypto_box::{PublicKey as CurvePublicKey, SecretKey as CurveSecretKey};
use rand::rngs::OsRng;

use crate::CryptoError;
#[cfg(test)]
use crate::{PublicKey, SecretKey};

fn seal(curve_pk: CurvePublicKey, plaintext: &[u8]) -> Result<Vec<u8>, CryptoError> {
    let mut rng = OsRng;
    curve_pk
        .seal(&mut rng, plaintext)
        .map_err(|_| CryptoError::EncryptionFailed)
}

fn open(curve_sk: CurveSecretKey, ciphertext: &[u8]) -> Result<Vec<u8>, CryptoError> {
    curve_sk
        .unseal(ciphertext)
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
fn seal_to_public_key(recipient: &PublicKey, plaintext: &[u8]) -> Result<Vec<u8>, CryptoError> {
    seal(
        CurvePublicKey::from(recipient.to_curve25519_bytes()),
        plaintext,
    )
}

/// Encrypts a payload to a Curve25519 public key.
///
/// Use this when you already have a Curve25519 key rather than an Ed25519 key.
///
/// # Errors
///
/// Returns [`CryptoError::EncryptionFailed`] if encryption fails.
pub fn seal_to_curve25519_public_key(
    recipient: &[u8; 32],
    plaintext: &[u8],
) -> Result<Vec<u8>, CryptoError> {
    seal(CurvePublicKey::from(*recipient), plaintext)
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
fn open_from_secret_key(recipient: &SecretKey, ciphertext: &[u8]) -> Result<Vec<u8>, CryptoError> {
    open(
        CurveSecretKey::from(recipient.to_curve25519_bytes()),
        ciphertext,
    )
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
    open(CurveSecretKey::from(*recipient), ciphertext)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- G3: Survey crypto roundtrip tests ----

    #[test]
    fn test_seal_open_ed25519_roundtrip_g3() {
        // Encrypt with Ed25519 public key, decrypt with Ed25519 secret key.
        let secret = SecretKey::generate();
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
        let secret_a = SecretKey::generate();
        let public_a = secret_a.public_key();
        let secret_b = SecretKey::generate();

        let plaintext = b"secret data";
        let ciphertext = seal_to_public_key(&public_a, plaintext).unwrap();

        let result = open_from_secret_key(&secret_b, &ciphertext);
        assert!(result.is_err(), "decryption with wrong key should fail");
        assert!(matches!(result, Err(CryptoError::DecryptionFailed)));
    }

    #[test]
    fn test_tampered_ciphertext_fails_g3() {
        // Authenticated encryption should reject tampered ciphertext.
        let secret = SecretKey::generate();
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
        let secret = SecretKey::generate();
        let public = secret.public_key();
        let plaintext = b"";

        let ciphertext = seal_to_public_key(&public, plaintext).unwrap();
        let decrypted = open_from_secret_key(&secret, &ciphertext).unwrap();
        assert_eq!(decrypted, plaintext.as_slice());
    }

    #[test]
    fn test_large_plaintext_roundtrip_g3() {
        // Survey responses can be large. Test with a realistic payload size.
        let secret = SecretKey::generate();
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
        let secret = SecretKey::generate();
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
}
