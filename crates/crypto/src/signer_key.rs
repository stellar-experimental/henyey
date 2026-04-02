//! SignerKey utilities for transaction authorization.
//!
//! This module provides utilities for creating and working with [`SignerKey`] types
//! used in Stellar account authorization. SignerKeys allow accounts to authorize
//! transactions via:
//!
//! - **Ed25519 public keys**: Standard signature-based authorization
//! - **Pre-authorized transactions**: Hash of a transaction authorized in advance
//! - **Hash(X)**: SHA-256 preimage authorization (reveal X to authorize)
//! - **Ed25519 signed payloads**: Ed25519 key + additional payload for advanced authorization
//!
//! # Example
//!
//! ```
//! use henyey_crypto::{pre_auth_tx_key, hash_x_key, ed25519_payload_key};
//! use stellar_xdr::curr::SignerKey;
//!
//! // Create a hash(x) signer - anyone with the preimage can authorize
//! let preimage = b"my secret preimage";
//! let signer: SignerKey = hash_x_key(preimage);
//!
//! // Create an Ed25519 payload signer
//! let pubkey = [0u8; 32]; // Your Ed25519 public key
//! let payload = b"additional data";
//! let signer: SignerKey = ed25519_payload_key(&pubkey, payload);
//! ```

use crate::{sha256, Hash256};
use stellar_xdr::curr::{BytesM, SignerKey, SignerKeyEd25519SignedPayload, Uint256};

fn uint256_signer_key(key: fn(Uint256) -> SignerKey, bytes: [u8; 32]) -> SignerKey {
    key(Uint256(bytes))
}

/// Creates a pre-authorized transaction signer key from a transaction hash.
///
/// This signer allows a transaction to be authorized if its hash matches the
/// stored value. The transaction hash should be computed using the network
/// passphrase and transaction envelope.
///
/// # Arguments
///
/// * `tx_hash` - The SHA-256 hash of the transaction contents
///
/// # Returns
///
/// A [`SignerKey`] of type `PreAuthTx` containing the transaction hash.
///
/// # Example
///
/// ```
/// use henyey_crypto::{pre_auth_tx_key, sha256};
/// use stellar_xdr::curr::SignerKey;
///
/// // Compute transaction hash (normally from TransactionFrame::contents_hash())
/// let tx_hash = sha256(b"transaction contents");
/// let signer: SignerKey = pre_auth_tx_key(&tx_hash);
/// ```
pub fn pre_auth_tx_key(tx_hash: &Hash256) -> SignerKey {
    uint256_signer_key(SignerKey::PreAuthTx, tx_hash.0)
}

/// Creates a hash(x) signer key from a preimage.
///
/// This signer allows authorization by anyone who can provide the preimage
/// that hashes to the stored value. The preimage is first hashed with SHA-256,
/// and authorization requires revealing the original preimage.
///
/// **Security note**: The preimage should be kept secret until authorization
/// is needed. Anyone with the preimage can authorize operations.
///
/// # Arguments
///
/// * `preimage` - The secret data that will be hashed
///
/// # Returns
///
/// A [`SignerKey`] of type `HashX` containing SHA-256(preimage).
///
/// # Example
///
/// ```
/// use henyey_crypto::hash_x_key;
/// use stellar_xdr::curr::SignerKey;
///
/// let secret = b"my_secret_preimage_123";
/// let signer: SignerKey = hash_x_key(secret);
///
/// // Later, reveal the preimage to authorize operations
/// ```
pub fn hash_x_key(preimage: &[u8]) -> SignerKey {
    hash_x_key_from_hash(&sha256(preimage))
}

/// Creates a hash(x) signer key from an already-computed hash.
///
/// Unlike [`hash_x_key`], this function takes the hash directly rather than
/// computing it from a preimage. Use this when you already have the hash
/// and don't have access to the preimage.
///
/// # Arguments
///
/// * `hash` - The SHA-256 hash that the preimage must match
///
/// # Returns
///
/// A [`SignerKey`] of type `HashX` containing the provided hash.
pub fn hash_x_key_from_hash(hash: &Hash256) -> SignerKey {
    uint256_signer_key(SignerKey::HashX, hash.0)
}

/// Creates an Ed25519 signed payload signer key.
///
/// This signer requires both an Ed25519 signature from the specified public key
/// AND that the transaction includes the specified payload. This enables
/// advanced authorization schemes where additional context is required.
///
/// # Arguments
///
/// * `ed25519_pubkey` - The Ed25519 public key bytes (32 bytes)
/// * `payload` - Additional payload data (up to 64 bytes)
///
/// # Returns
///
/// A [`SignerKey`] of type `Ed25519SignedPayload`.
///
/// # Panics
///
/// Panics if the payload is longer than 64 bytes.
///
/// # Example
///
/// ```
/// use henyey_crypto::ed25519_payload_key;
/// use stellar_xdr::curr::SignerKey;
///
/// let pubkey = [1u8; 32]; // Ed25519 public key
/// let payload = b"memo:1234";
/// let signer: SignerKey = ed25519_payload_key(&pubkey, payload);
/// ```
pub fn ed25519_payload_key(ed25519_pubkey: &[u8; 32], payload: &[u8]) -> SignerKey {
    SignerKey::Ed25519SignedPayload(SignerKeyEd25519SignedPayload {
        ed25519: Uint256(*ed25519_pubkey),
        payload: BytesM::try_from(payload.to_vec()).expect("payload must be <= 64 bytes"),
    })
}

/// Creates an Ed25519 signer key from a public key.
///
/// This is the most common signer type, requiring a valid Ed25519 signature
/// from the specified public key.
///
/// # Arguments
///
/// * `ed25519_pubkey` - The Ed25519 public key bytes (32 bytes)
///
/// # Returns
///
/// A [`SignerKey`] of type `Ed25519`.
///
/// # Example
///
/// ```
/// use henyey_crypto::ed25519_key;
/// use stellar_xdr::curr::SignerKey;
///
/// let pubkey = [1u8; 32]; // Ed25519 public key
/// let signer: SignerKey = ed25519_key(&pubkey);
/// ```
pub fn ed25519_key(ed25519_pubkey: &[u8; 32]) -> SignerKey {
    uint256_signer_key(SignerKey::Ed25519, *ed25519_pubkey)
}

/// Extracts the Ed25519 public key from a SignerKey, if applicable.
///
/// Returns the 32-byte public key for Ed25519 and Ed25519SignedPayload signer
/// types. Returns `None` for PreAuthTx and HashX types which don't contain
/// an Ed25519 key.
///
/// # Arguments
///
/// * `signer_key` - The signer key to extract from
///
/// # Returns
///
/// `Some([u8; 32])` containing the Ed25519 public key, or `None`.
pub fn get_ed25519_from_signer_key(signer_key: &SignerKey) -> Option<[u8; 32]> {
    match signer_key {
        SignerKey::Ed25519(key) => Some(key.0),
        SignerKey::Ed25519SignedPayload(payload) => Some(payload.ed25519.0),
        SignerKey::PreAuthTx(_) | SignerKey::HashX(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pre_auth_tx_key() {
        let hash = Hash256([0xab; 32]);
        let signer = pre_auth_tx_key(&hash);

        match signer {
            SignerKey::PreAuthTx(h) => assert_eq!(h.0, [0xab; 32]),
            _ => panic!("expected PreAuthTx"),
        }
    }

    #[test]
    fn test_hash_x_key() {
        let preimage = b"test preimage";
        let signer = hash_x_key(preimage);

        let expected_hash = sha256(preimage);
        match signer {
            SignerKey::HashX(h) => assert_eq!(h.0, expected_hash.0),
            _ => panic!("expected HashX"),
        }
    }

    #[test]
    fn test_hash_x_key_from_hash() {
        let hash = Hash256([0xcd; 32]);
        let signer = hash_x_key_from_hash(&hash);

        match signer {
            SignerKey::HashX(h) => assert_eq!(h.0, [0xcd; 32]),
            _ => panic!("expected HashX"),
        }
    }

    #[test]
    fn test_ed25519_payload_key() {
        let pubkey = [0x01; 32];
        let payload = b"test payload";
        let signer = ed25519_payload_key(&pubkey, payload);

        match signer {
            SignerKey::Ed25519SignedPayload(p) => {
                assert_eq!(p.ed25519.0, pubkey);
                assert_eq!(p.payload.as_slice(), payload);
            }
            _ => panic!("expected Ed25519SignedPayload"),
        }
    }

    #[test]
    fn test_ed25519_key() {
        let pubkey = [0x02; 32];
        let signer = ed25519_key(&pubkey);

        match signer {
            SignerKey::Ed25519(k) => assert_eq!(k.0, pubkey),
            _ => panic!("expected Ed25519"),
        }
    }

    #[test]
    fn test_get_ed25519_from_signer_key() {
        // Ed25519 type
        let pubkey = [0x03; 32];
        let signer = ed25519_key(&pubkey);
        assert_eq!(get_ed25519_from_signer_key(&signer), Some(pubkey));

        // Ed25519SignedPayload type
        let signer = ed25519_payload_key(&pubkey, b"payload");
        assert_eq!(get_ed25519_from_signer_key(&signer), Some(pubkey));

        // PreAuthTx type - no Ed25519 key
        let signer = pre_auth_tx_key(&Hash256([0; 32]));
        assert_eq!(get_ed25519_from_signer_key(&signer), None);

        // HashX type - no Ed25519 key
        let signer = hash_x_key(b"secret");
        assert_eq!(get_ed25519_from_signer_key(&signer), None);
    }

    #[test]
    fn test_empty_payload() {
        let pubkey = [0x04; 32];
        let signer = ed25519_payload_key(&pubkey, b"");

        match signer {
            SignerKey::Ed25519SignedPayload(p) => {
                assert!(p.payload.is_empty());
            }
            _ => panic!("expected Ed25519SignedPayload"),
        }
    }
}
