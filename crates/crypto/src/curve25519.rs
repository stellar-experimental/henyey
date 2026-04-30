//! Curve25519 ECDH key exchange for P2P overlay authentication.
//!
//! This module provides ECDH (Elliptic-curve Diffie-Hellman) key exchange using
//! Curve25519, as used in the Stellar P2P overlay network for peer authentication.
//!
//! # Important Security Note
//!
//! These keys should **not** be mixed with Ed25519 signing keys. While there is
//! a mathematical equivalence between Curve25519 and Ed25519, using the same key
//! material for both ECDH and signing can complicate security analysis.
//!
//! Stellar's design uses:
//! - **Ed25519**: For transaction signatures and SCP message signing
//! - **Curve25519**: For P2P session key agreement (ephemeral, per-session)
//!
//! During P2P handshake, peers generate random Curve25519 keys and sign them
//! with their long-lived Ed25519 keys.
//!
//! # Example
//!
//! ```
//! use henyey_crypto::{Curve25519Secret, Curve25519Public, KeyOrdering};
//!
//! // Generate random session keys for two peers
//! let alice_secret = Curve25519Secret::random();
//! let alice_public = alice_secret.derive_public();
//!
//! let bob_secret = Curve25519Secret::random();
//! let bob_public = bob_secret.derive_public();
//!
//! // Derive shared key from each side - both should match
//! let alice_shared = Curve25519Secret::derive_shared_key(
//!     &alice_secret, &alice_public, &bob_public, KeyOrdering::LocalFirst).unwrap();
//! let bob_shared = Curve25519Secret::derive_shared_key(
//!     &bob_secret, &bob_public, &alice_public, KeyOrdering::RemoteFirst).unwrap();
//!
//! assert_eq!(alice_shared, bob_shared);
//! ```

use crate::{hkdf_extract, random_bytes, CryptoError};

/// Ordering of public keys when deriving a shared key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyOrdering {
    /// Local public key comes first in concatenation.
    LocalFirst,
    /// Remote public key comes first in concatenation.
    RemoteFirst,
}
use std::hash::{Hash, Hasher};
use x25519_dalek::{PublicKey, StaticSecret};
use zeroize::ZeroizeOnDrop;

/// A secret Curve25519 scalar for ECDH key exchange.
///
/// This is a 32-byte random scalar used as the private key in ECDH.
/// The key material is zeroized on drop for security.
#[derive(Clone, ZeroizeOnDrop)]
pub struct Curve25519Secret {
    inner: StaticSecret,
}

/// A public Curve25519 point for ECDH key exchange.
///
/// This is a 32-byte public key derived from a [`Curve25519Secret`].
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Curve25519Public {
    inner: PublicKey,
}

impl Curve25519Secret {
    /// Generates a random Curve25519 secret key.
    ///
    /// Uses the system's cryptographically secure random number generator.
    pub fn random() -> Self {
        Self {
            inner: StaticSecret::from(random_bytes()),
        }
    }

    /// Creates a Curve25519 secret from raw bytes.
    ///
    /// # Arguments
    ///
    /// * `bytes` - A 32-byte array representing the secret scalar
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self {
            inner: StaticSecret::from(bytes),
        }
    }

    /// Returns the raw bytes of this secret key.
    ///
    /// # Security
    ///
    /// Handle the returned bytes carefully - they are sensitive key material.
    pub fn to_bytes(&self) -> [u8; 32] {
        self.inner.to_bytes()
    }

    /// Derives the public key from this secret.
    pub fn derive_public(&self) -> Curve25519Public {
        Curve25519Public {
            inner: PublicKey::from(&self.inner),
        }
    }

    /// Performs ECDH to derive a shared secret.
    ///
    /// Computes `localSecret * remotePublic` (scalar multiplication).
    /// Returns an error if the result is not contributory (small-order public key),
    /// matching stellar-core's crypto_scalarmult() return-code check.
    pub fn diffie_hellman(
        &self,
        remote_public: &Curve25519Public,
    ) -> Result<[u8; 32], CryptoError> {
        let shared = self.inner.diffie_hellman(&remote_public.inner);
        if !shared.was_contributory() {
            return Err(CryptoError::SmallOrderPublicKey);
        }
        Ok(shared.to_bytes())
    }

    /// Derives a shared HMAC-SHA256 key for authenticated encryption.
    ///
    /// This performs ECDH and then applies HKDF-extract to derive a key
    /// suitable for use with HMAC-SHA256. The derivation includes both
    /// public keys to bind the key to the specific session.
    ///
    /// Formula: `HKDF_extract(localSecret * remotePublic || publicA || publicB)`
    ///
    /// Where:
    /// - `publicA = localFirst ? localPublic : remotePublic`
    /// - `publicB = localFirst ? remotePublic : localPublic`
    ///
    /// # Arguments
    ///
    /// * `local_secret` - The local Curve25519 secret key
    /// * `local_public` - The local Curve25519 public key
    /// * `remote_public` - The remote peer's public key
    /// * `key_ordering` - Whether the local key comes first in concatenation
    ///
    /// # Returns
    ///
    /// A 32-byte key suitable for HMAC-SHA256 operations.
    pub fn derive_shared_key(
        local_secret: &Curve25519Secret,
        local_public: &Curve25519Public,
        remote_public: &Curve25519Public,
        key_ordering: KeyOrdering,
    ) -> Result<[u8; 32], CryptoError> {
        // Perform ECDH (returns Err for small-order public keys)
        let shared_secret = local_secret.diffie_hellman(remote_public)?;

        // Determine key ordering based on key_ordering
        let (public_a, public_b) = if matches!(key_ordering, KeyOrdering::LocalFirst) {
            (local_public, remote_public)
        } else {
            (remote_public, local_public)
        };

        // Concatenate: shared_secret || publicA || publicB
        let mut buf = [0u8; 96];
        buf[..32].copy_from_slice(&shared_secret);
        buf[32..64].copy_from_slice(&public_a.to_bytes());
        buf[64..96].copy_from_slice(&public_b.to_bytes());

        // Apply HKDF-extract
        Ok(hkdf_extract(&buf))
    }
}

impl std::fmt::Debug for Curve25519Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Curve25519Secret")
            .field("inner", &"[REDACTED]")
            .finish()
    }
}

impl Curve25519Public {
    /// Creates a public key from raw bytes.
    ///
    /// # Arguments
    ///
    /// * `bytes` - A 32-byte array representing the public point
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self {
            inner: PublicKey::from(bytes),
        }
    }

    /// Returns the raw bytes of this public key.
    pub fn to_bytes(&self) -> [u8; 32] {
        self.inner.to_bytes()
    }

    /// Returns a reference to the raw bytes.
    pub fn as_bytes(&self) -> &[u8; 32] {
        self.inner.as_bytes()
    }
}

impl std::fmt::Debug for Curve25519Public {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Curve25519Public")
            .field("key", &hex::encode(self.to_bytes()))
            .finish()
    }
}

impl Hash for Curve25519Public {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.to_bytes().hash(state);
    }
}

impl From<stellar_xdr::curr::Curve25519Public> for Curve25519Public {
    fn from(xdr: stellar_xdr::curr::Curve25519Public) -> Self {
        Self::from_bytes(xdr.key)
    }
}

impl From<Curve25519Public> for stellar_xdr::curr::Curve25519Public {
    fn from(key: Curve25519Public) -> Self {
        stellar_xdr::curr::Curve25519Public {
            key: key.to_bytes(),
        }
    }
}

impl From<stellar_xdr::curr::Curve25519Secret> for Curve25519Secret {
    fn from(xdr: stellar_xdr::curr::Curve25519Secret) -> Self {
        Self::from_bytes(xdr.key)
    }
}

impl From<Curve25519Secret> for stellar_xdr::curr::Curve25519Secret {
    fn from(key: Curve25519Secret) -> Self {
        stellar_xdr::curr::Curve25519Secret {
            key: key.to_bytes(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_key_generation() {
        let secret = Curve25519Secret::random();
        let public = secret.derive_public();

        // Public key should not be all zeros
        assert_ne!(public.to_bytes(), [0u8; 32]);
    }

    #[test]
    fn test_diffie_hellman_agreement() {
        // Two parties generate keypairs
        let alice_secret = Curve25519Secret::random();
        let alice_public = alice_secret.derive_public();

        let bob_secret = Curve25519Secret::random();
        let bob_public = bob_secret.derive_public();

        // Both sides compute the same shared secret
        let alice_shared = alice_secret.diffie_hellman(&bob_public).unwrap();
        let bob_shared = bob_secret.diffie_hellman(&alice_public).unwrap();

        assert_eq!(alice_shared, bob_shared);
    }

    #[test]
    fn test_derive_shared_key() {
        let alice_secret = Curve25519Secret::random();
        let alice_public = alice_secret.derive_public();

        let bob_secret = Curve25519Secret::random();
        let bob_public = bob_secret.derive_public();

        // Derive shared key from both sides
        let alice_key = Curve25519Secret::derive_shared_key(
            &alice_secret,
            &alice_public,
            &bob_public,
            KeyOrdering::LocalFirst, // Alice first
        )
        .unwrap();

        let bob_key = Curve25519Secret::derive_shared_key(
            &bob_secret,
            &bob_public,
            &alice_public,
            KeyOrdering::RemoteFirst, // Alice first (so Bob is not first)
        )
        .unwrap();

        // Both should derive the same key
        assert_eq!(alice_key, bob_key);
    }

    #[test]
    fn test_derive_shared_key_ordering_matters() {
        let alice_secret = Curve25519Secret::random();
        let alice_public = alice_secret.derive_public();

        let bob_secret = Curve25519Secret::random();
        let bob_public = bob_secret.derive_public();

        // Same local_first value should produce different keys for different parties
        let alice_key_first = Curve25519Secret::derive_shared_key(
            &alice_secret,
            &alice_public,
            &bob_public,
            KeyOrdering::LocalFirst,
        )
        .unwrap();

        let alice_key_second = Curve25519Secret::derive_shared_key(
            &alice_secret,
            &alice_public,
            &bob_public,
            KeyOrdering::RemoteFirst,
        )
        .unwrap();

        // Different ordering should produce different keys
        assert_ne!(alice_key_first, alice_key_second);
    }

    #[test]
    fn test_from_bytes_roundtrip() {
        let secret = Curve25519Secret::random();
        let bytes = secret.to_bytes();
        let restored = Curve25519Secret::from_bytes(bytes);

        assert_eq!(
            secret.derive_public().to_bytes(),
            restored.derive_public().to_bytes()
        );
    }

    #[test]
    fn test_public_key_hash() {
        use std::collections::HashSet;

        let key1 = Curve25519Secret::random().derive_public();
        let key2 = Curve25519Secret::random().derive_public();

        let mut set = HashSet::new();
        set.insert(key1);
        set.insert(key2);

        assert_eq!(set.len(), 2);
    }

    #[test]
    fn test_xdr_conversion() {
        let secret = Curve25519Secret::random();
        let public = secret.derive_public();

        // Convert to XDR and back
        let xdr_public: stellar_xdr::curr::Curve25519Public = public.into();
        let restored_public: Curve25519Public = xdr_public.into();

        assert_eq!(public.to_bytes(), restored_public.to_bytes());
    }

    #[test]
    fn test_debug_redacts_secret() {
        let secret = Curve25519Secret::random();
        let debug_str = format!("{:?}", secret);

        // Should not contain the actual key bytes
        assert!(debug_str.contains("REDACTED"));
    }

    /// Regression test for AUDIT-018 (#1091): small-order Curve25519 public keys
    /// must be rejected. stellar-core checks crypto_scalarmult() return code.
    #[test]
    fn test_audit_018_diffie_hellman_rejects_small_order_keys() {
        let secret = Curve25519Secret::random();

        // The identity point (all zeros) is a small-order point
        let zero_key = Curve25519Public::from_bytes([0u8; 32]);
        let result = secret.diffie_hellman(&zero_key);
        assert!(result.is_err(), "DH with all-zeros public key should fail");
        assert!(matches!(result, Err(CryptoError::SmallOrderPublicKey)));
    }
}
