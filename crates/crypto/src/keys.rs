//! Ed25519 key types and management.
//!
//! This module provides the core key types for Ed25519 cryptography in Stellar:
//!
//! - [`PublicKey`]: A 32-byte Ed25519 public key (verifying key)
//! - [`SecretKey`]: A 32-byte Ed25519 secret key (signing key), zeroized on drop
//! - [`Signature`]: A 64-byte Ed25519 signature
//!
//! All keys can be encoded to and decoded from Stellar's StrKey format:
//! - Public keys encode to account IDs starting with 'G'
//! - Secret keys encode to seeds starting with 'S'
//!
//! # Example
//!
//! ```
//! use henyey_crypto::{SecretKey, PublicKey};
//!
//! // Generate a new keypair
//! let secret = SecretKey::generate();
//! let public = secret.public_key();
//!
//! // Convert to StrKey format
//! let account_id = public.to_strkey(); // G...
//! let seed = secret.to_strkey();       // S...
//!
//! // Parse from StrKey
//! let public2 = PublicKey::from_strkey(&account_id).unwrap();
//! assert_eq!(public, public2);
//! ```

use crate::error::CryptoError;
use ed25519_dalek::{SigningKey, VerifyingKey};
use std::fmt;

/// An Ed25519 public key (verifying key).
///
/// This is a 32-byte key used to verify signatures created by the corresponding
/// [`SecretKey`]. In Stellar, public keys are encoded as account IDs starting
/// with the letter 'G'.
///
/// # Display
///
/// The `Debug` and `Display` implementations show the StrKey encoding (G...),
/// making log output more readable and consistent with Stellar conventions.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct PublicKey(VerifyingKey);

impl PublicKey {
    /// Creates a public key from raw 32-byte Ed25519 key material.
    ///
    /// # Errors
    ///
    /// Returns [`CryptoError::InvalidPublicKey`] if the bytes do not represent
    /// a valid point on the Ed25519 curve.
    pub fn from_bytes(bytes: &[u8; 32]) -> Result<Self, CryptoError> {
        let key = VerifyingKey::from_bytes(bytes).map_err(|_| CryptoError::InvalidPublicKey)?;
        Ok(Self(key))
    }

    /// Returns the raw 32-byte key material.
    pub fn as_bytes(&self) -> &[u8; 32] {
        self.0.as_bytes()
    }

    /// Verifies an Ed25519 signature over a message.
    ///
    /// Uses `verify_strict` to reject small-order and mixed-order public key
    /// points, matching stellar-core's libsodium / dalek behavior
    /// (see stellar-core/src/rust/src/ed25519_verify.rs:39).
    ///
    /// # Errors
    ///
    /// Returns [`CryptoError::InvalidSignature`] if verification fails.
    pub fn verify(&self, message: &[u8], signature: &Signature) -> Result<(), CryptoError> {
        let sig = ed25519_dalek::Signature::from_bytes(&signature.0);
        self.0
            .verify_strict(message, &sig)
            .map_err(|_| CryptoError::InvalidSignature)
    }

    /// Encodes the public key as a Stellar account ID (G...).
    ///
    /// This uses Stellar's StrKey format: base32 encoding with a version byte
    /// and CRC16 checksum.
    pub fn to_strkey(&self) -> String {
        stellar_strkey::ed25519::PublicKey(*self.as_bytes()).to_string()
    }

    /// Parses a public key from a Stellar account ID (G...).
    ///
    /// # Errors
    ///
    /// Returns [`CryptoError::InvalidStrKey`] if the string is not a valid
    /// account ID, or [`CryptoError::InvalidPublicKey`] if the decoded bytes
    /// are not a valid Ed25519 public key.
    pub fn from_strkey(s: &str) -> Result<Self, CryptoError> {
        let pk: stellar_strkey::ed25519::PublicKey = s
            .parse()
            .map_err(|e: stellar_strkey::DecodeError| CryptoError::InvalidStrKey(e.to_string()))?;
        Self::from_bytes(&pk.0)
    }

    /// Converts to Curve25519 (Montgomery form) public key bytes.
    ///
    /// This is used for sealed box encryption, which uses X25519 key exchange.
    /// The Ed25519 key is converted to its Curve25519 equivalent.
    pub fn to_curve25519_bytes(&self) -> [u8; 32] {
        self.0.to_montgomery().to_bytes()
    }
}

impl fmt::Debug for PublicKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "PublicKey({})", self.to_strkey())
    }
}

impl fmt::Display for PublicKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.to_strkey())
    }
}

impl TryFrom<&stellar_xdr::curr::PublicKey> for PublicKey {
    type Error = CryptoError;

    fn try_from(xdr: &stellar_xdr::curr::PublicKey) -> Result<Self, Self::Error> {
        match xdr {
            stellar_xdr::curr::PublicKey::PublicKeyTypeEd25519(stellar_xdr::curr::Uint256(
                bytes,
            )) => Self::from_bytes(bytes),
        }
    }
}

impl From<&PublicKey> for stellar_xdr::curr::PublicKey {
    fn from(pk: &PublicKey) -> Self {
        stellar_xdr::curr::PublicKey::PublicKeyTypeEd25519(stellar_xdr::curr::Uint256(
            *pk.as_bytes(),
        ))
    }
}

impl From<&PublicKey> for stellar_xdr::curr::AccountId {
    fn from(pk: &PublicKey) -> Self {
        stellar_xdr::curr::AccountId(pk.into())
    }
}

/// An Ed25519 secret key (signing key).
///
/// This is a 32-byte seed used to generate signatures. The corresponding
/// [`PublicKey`] can be derived from it. In Stellar, secret keys are encoded
/// as seeds starting with the letter 'S'.
///
/// # Security
///
/// - The underlying key material is zeroized when this struct is dropped.
/// - The `Debug` implementation does not reveal the key material.
/// - Clone is implemented but should be used sparingly to minimize key copies.
///
/// # Example
///
/// ```
/// use henyey_crypto::SecretKey;
///
/// let secret = SecretKey::generate();
/// let signature = secret.sign(b"message");
/// ```
pub struct SecretKey {
    // SigningKey implements Zeroize internally when dropped,
    // ensuring sensitive key material is cleared from memory.
    inner: SigningKey,
}

impl SecretKey {
    /// Generates a new random secret key using the OS random number generator.
    ///
    /// This uses [`OsRng`](rand::rngs::OsRng), which provides cryptographically
    /// secure random bytes from the operating system.
    pub fn generate() -> Self {
        let mut csprng = rand::rngs::OsRng;
        let signing_key = SigningKey::generate(&mut csprng);
        Self { inner: signing_key }
    }

    /// Creates a secret key from a 32-byte seed.
    ///
    /// The seed is the raw Ed25519 secret key material. This is deterministic:
    /// the same seed will always produce the same key.
    pub fn from_seed(seed: &[u8; 32]) -> Self {
        let signing_key = SigningKey::from_bytes(seed);
        Self { inner: signing_key }
    }

    /// Signs a message, producing a 64-byte Ed25519 signature.
    ///
    /// The signature can be verified using the corresponding [`PublicKey`].
    pub fn sign(&self, message: &[u8]) -> Signature {
        use ed25519_dalek::Signer;
        let signature = self.inner.sign(message);
        Signature(signature.to_bytes())
    }

    /// Derives the corresponding public key.
    ///
    /// This is a deterministic operation: a secret key always produces the
    /// same public key.
    pub fn public_key(&self) -> PublicKey {
        PublicKey(self.inner.verifying_key())
    }

    /// Encodes the secret key as a Stellar seed (S...).
    ///
    /// This uses Stellar's StrKey format: base32 encoding with a version byte
    /// and CRC16 checksum.
    ///
    /// # Security
    ///
    /// The returned string contains sensitive key material. Handle with care
    /// and avoid logging or displaying it unnecessarily.
    pub fn to_strkey(&self) -> String {
        stellar_strkey::ed25519::PrivateKey(*self.inner.as_bytes()).to_string()
    }

    /// Parses a secret key from a Stellar seed (S...).
    ///
    /// # Errors
    ///
    /// Returns [`CryptoError::InvalidStrKey`] if the string is not a valid seed.
    pub fn from_strkey(s: &str) -> Result<Self, CryptoError> {
        let pk: stellar_strkey::ed25519::PrivateKey = s
            .parse()
            .map_err(|e: stellar_strkey::DecodeError| CryptoError::InvalidStrKey(e.to_string()))?;
        Ok(Self::from_seed(&pk.0))
    }

    /// Converts to Curve25519 scalar bytes for sealed box decryption.
    ///
    /// This is used for sealed box operations, which use X25519 key exchange.
    /// The Ed25519 secret key is converted to its Curve25519 equivalent.
    pub fn to_curve25519_bytes(&self) -> [u8; 32] {
        self.inner.to_scalar_bytes()
    }

    /// Returns the raw 32-byte seed.
    ///
    /// # Security
    ///
    /// This exposes sensitive key material. Use with caution.
    pub fn as_bytes(&self) -> &[u8; 32] {
        self.inner.as_bytes()
    }
}

impl fmt::Debug for SecretKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SecretKey([REDACTED])")
    }
}

impl Clone for SecretKey {
    fn clone(&self) -> Self {
        Self {
            inner: SigningKey::from_bytes(self.inner.as_bytes()),
        }
    }
}

/// Converts an XDR AccountId to its strkey representation (G...).
///
/// This is a convenience function for converting the XDR AccountId type
/// directly to a human-readable strkey format, suitable for logging.
pub fn account_id_to_strkey(account_id: &stellar_xdr::curr::AccountId) -> String {
    match &account_id.0 {
        stellar_xdr::curr::PublicKey::PublicKeyTypeEd25519(stellar_xdr::curr::Uint256(bytes)) => {
            stellar_strkey::ed25519::PublicKey(*bytes).to_string()
        }
    }
}

/// A 64-byte Ed25519 signature.
///
/// Signatures are created by [`SecretKey::sign`] and verified by [`PublicKey::verify`].
/// They are deterministic: signing the same message with the same key always
/// produces the same signature.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Signature(pub [u8; 64]);

impl Signature {
    /// Returns the raw 64-byte signature.
    pub fn as_bytes(&self) -> &[u8; 64] {
        &self.0
    }

    /// Creates a signature from raw bytes.
    ///
    /// No validation is performed; use [`PublicKey::verify`] to check validity.
    pub fn from_bytes(bytes: [u8; 64]) -> Self {
        Self(bytes)
    }
}

impl fmt::Debug for Signature {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Signature({})", hex::encode(&self.0[..8]))
    }
}

impl From<Signature> for stellar_xdr::curr::Signature {
    fn from(sig: Signature) -> Self {
        // Safety: Signature is always exactly 64 bytes, which is within
        // the XDR Signature's BytesM::<64> limit.
        stellar_xdr::curr::Signature(
            sig.0
                .try_into()
                .expect("64-byte signature always fits in BytesM::<64>"),
        )
    }
}

impl TryFrom<&stellar_xdr::curr::Signature> for Signature {
    type Error = CryptoError;

    fn try_from(xdr: &stellar_xdr::curr::Signature) -> Result<Self, Self::Error> {
        let bytes: [u8; 64] =
            xdr.0
                .as_slice()
                .try_into()
                .map_err(|_| CryptoError::InvalidLength {
                    expected: 64,
                    got: xdr.0.len(),
                })?;
        Ok(Self(bytes))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_key_generation() {
        let secret = SecretKey::generate();
        let public = secret.public_key();

        // Should produce valid strkeys
        let secret_strkey = secret.to_strkey();
        let public_strkey = public.to_strkey();

        assert!(secret_strkey.starts_with('S'));
        assert!(public_strkey.starts_with('G'));

        // Should round-trip
        let secret2 = SecretKey::from_strkey(&secret_strkey).unwrap();
        let public2 = PublicKey::from_strkey(&public_strkey).unwrap();

        assert_eq!(secret.as_bytes(), secret2.as_bytes());
        assert_eq!(public.as_bytes(), public2.as_bytes());
    }

    #[test]
    fn test_signing() {
        let secret = SecretKey::generate();
        let public = secret.public_key();

        let message = b"hello world";
        let signature = secret.sign(message);

        // Should verify
        assert!(public.verify(message, &signature).is_ok());

        // Should not verify with wrong message
        assert!(public.verify(b"wrong message", &signature).is_err());
    }

    #[test]
    fn test_known_key() {
        // Generate a key and verify round-trip
        let secret = SecretKey::generate();
        let secret_strkey = secret.to_strkey();

        // Verify starts with S (secret seed prefix)
        assert!(secret_strkey.starts_with('S'));

        // Round-trip the secret key
        let secret2 = SecretKey::from_strkey(&secret_strkey).unwrap();
        assert_eq!(secret.as_bytes(), secret2.as_bytes());

        // Derive and verify public key
        let public = secret.public_key();
        let public_strkey = public.to_strkey();

        // Verify starts with G (account ID prefix)
        assert!(public_strkey.starts_with('G'));

        // Verify public key from original and loaded secrets match
        let public2 = secret2.public_key();
        assert_eq!(public.as_bytes(), public2.as_bytes());
    }
}
