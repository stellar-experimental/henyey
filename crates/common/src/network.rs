//! Network identity types.
//!
//! This module provides the [`NetworkId`] type, which represents a unique
//! identifier for a Stellar network derived from its passphrase.
//!
//! # Network Passphrases
//!
//! Each Stellar network is identified by a unique passphrase. The network ID
//! is computed as the SHA-256 hash of this passphrase. This ensures that
//! transactions signed for one network cannot be replayed on another.
//!
//! # Standard Networks
//!
//! | Network | Passphrase |
//! |---------|------------|
//! | Mainnet | `"Public Global Stellar Network ; September 2015"` |
//! | Testnet | `"Test SDF Network ; September 2015"` |
//!
//! # Example
//!
//! ```rust
//! use henyey_common::NetworkId;
//!
//! // Use a standard network
//! let testnet = NetworkId::testnet();
//! let mainnet = NetworkId::mainnet();
//!
//! // They have different IDs
//! assert_ne!(testnet.as_bytes(), mainnet.as_bytes());
//!
//! // Create a custom network ID
//! let custom = NetworkId::from_passphrase("My Private Network ; 2024");
//! ```

use crate::types::Hash256;

/// A unique identifier for a Stellar network.
///
/// The network ID is the SHA-256 hash of the network passphrase. It is used
/// throughout the protocol to ensure that signatures and transactions are
/// bound to a specific network, preventing cross-network replay attacks.
///
/// # Usage
///
/// The network ID is included in the hash preimage for transaction signatures,
/// ensuring that a signature valid on testnet cannot be used on mainnet.
///
/// # Example
///
/// ```rust
/// use henyey_common::NetworkId;
///
/// let network = NetworkId::testnet();
/// println!("Network ID bytes: {:?}", network.as_bytes());
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct NetworkId(pub Hash256);

impl NetworkId {
    /// Creates a network ID from a passphrase string.
    ///
    /// The network ID is computed as `SHA256(passphrase)`.
    ///
    /// # Example
    ///
    /// ```rust
    /// use henyey_common::NetworkId;
    ///
    /// let id = NetworkId::from_passphrase("My Custom Network");
    /// assert!(!id.0.is_zero());
    /// ```
    pub fn from_passphrase(passphrase: &str) -> Self {
        Self(Hash256::hash(passphrase.as_bytes()))
    }

    /// Returns a reference to the underlying 32-byte hash.
    pub fn as_bytes(&self) -> &[u8; 32] {
        self.0.as_bytes()
    }

    /// Returns the network ID for the Stellar public testnet.
    ///
    /// Passphrase: `"Test SDF Network ; September 2015"`
    pub fn testnet() -> Self {
        Self::from_passphrase("Test SDF Network ; September 2015")
    }

    /// Returns the network ID for the Stellar public mainnet.
    ///
    /// Passphrase: `"Public Global Stellar Network ; September 2015"`
    pub fn mainnet() -> Self {
        Self::from_passphrase("Public Global Stellar Network ; September 2015")
    }

    /// Returns `true` if this network ID matches the Stellar public mainnet.
    ///
    /// This is used for mainnet-only corrections (e.g., V24 fee pool fix).
    pub fn is_mainnet(&self) -> bool {
        *self == Self::mainnet()
    }
}

impl From<NetworkId> for stellar_xdr::Hash {
    fn from(id: NetworkId) -> Self {
        stellar_xdr::Hash(id.0 .0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_network_id_testnet() {
        let id = NetworkId::testnet();
        // Known testnet network ID hash
        assert!(!id.0.is_zero());
    }

    #[test]
    fn test_network_id_mainnet() {
        let id = NetworkId::mainnet();
        // Known mainnet network ID hash
        assert!(!id.0.is_zero());
    }

    #[test]
    fn test_is_mainnet() {
        assert!(NetworkId::mainnet().is_mainnet());
        assert!(!NetworkId::testnet().is_mainnet());
        assert!(!NetworkId::from_passphrase("Custom Network").is_mainnet());
    }
}
