//! Cryptographic error types.
//!
//! This module defines the error types used throughout the crypto crate.
//! All cryptographic operations that can fail return [`CryptoError`].

use thiserror::Error;

/// Errors that can occur during cryptographic operations.
///
/// This enum covers all failure modes in the crypto crate, including:
/// - Key parsing and validation errors
/// - Signature verification failures
/// - Encoding/decoding errors (StrKey, hex)
/// - Encryption/decryption failures
/// - XDR serialization errors
#[derive(Error, Debug)]
pub enum CryptoError {
    /// The provided bytes do not represent a valid Ed25519 public key.
    ///
    /// This can occur when the point is not on the Ed25519 curve.
    #[error("invalid public key")]
    InvalidPublicKey,

    /// The provided bytes do not represent a valid Ed25519 secret key.
    #[error("invalid secret key")]
    InvalidSecretKey,

    /// Signature verification failed.
    ///
    /// This indicates the signature does not match the message and public key,
    /// or the signature bytes are malformed.
    #[error("invalid signature")]
    InvalidSignature,

    /// StrKey encoding or decoding failed.
    ///
    /// The contained string provides details about the failure (e.g., invalid
    /// base32, wrong version byte, checksum mismatch).
    #[error("invalid strkey encoding: {0}")]
    InvalidStrKey(String),

    /// Hexadecimal decoding failed.
    #[error("invalid hex encoding")]
    InvalidHex,

    /// Data length does not match the expected size.
    #[error("invalid length: expected {expected}, got {got}")]
    InvalidLength {
        /// The expected number of bytes.
        expected: usize,
        /// The actual number of bytes received.
        got: usize,
    },

    /// Sealed box encryption failed.
    #[error("encryption failed")]
    EncryptionFailed,

    /// Sealed box decryption failed.
    ///
    /// This can indicate the ciphertext was tampered with, the wrong key
    /// was used, or the ciphertext is malformed.
    #[error("decryption failed")]
    DecryptionFailed,

    /// XDR serialization or deserialization failed.
    #[error("XDR error: {0}")]
    Xdr(#[from] stellar_xdr::curr::Error),

    /// Attempted to reseed the short hash with a different value after hashing.
    ///
    /// The short hash key can only be seeded once per process. Once hashing
    /// has begun, the seed cannot be changed to ensure deterministic behavior.
    #[error("short hash already seeded with {existing}, cannot reseed with {requested}")]
    ShortHashSeedConflict {
        /// The seed value already in use.
        existing: u32,
        /// The new seed value that was rejected.
        requested: u32,
    },

    /// ECDH produced an all-zeros shared secret, indicating the remote public
    /// key is a small-order point. stellar-core rejects these via libsodium's
    /// `crypto_scalarmult()` return-code check.
    #[error("small-order public key produced all-zeros shared secret")]
    SmallOrderPublicKey,
}
