//! Protocol version utilities.
//!
//! This module provides utilities for handling Stellar protocol versions and
//! gating features based on the current ledger protocol version.
//!
//! # Protocol Versioning in Stellar
//!
//! Stellar uses protocol versions to manage network upgrades. Each version
//! may introduce new features, transaction types, or behavioral changes.
//! The network coordinates upgrades through validator voting.
//!
//! # Feature Gating
//!
//! Use the helper functions in this module to conditionally enable features
//! based on the current protocol version:
//!
//! ```rust
//! use henyey_common::protocol::{
//!     protocol_version_starts_from, soroban_supported, ProtocolVersion
//! };
//!
//! let current_version = 22;
//!
//! // Check if Soroban smart contracts are supported
//! if soroban_supported(current_version) {
//!     // Execute smart contract logic
//! }
//!
//! // Check if a specific version feature is available
//! if protocol_version_starts_from(current_version, ProtocolVersion::V21) {
//!     // Use V21+ features
//! }
//! ```
//!
//! # Key Protocol Versions
//!
//! - **V20**: Soroban smart contracts introduced
//! - **V23**: Parallel Soroban execution, auto-restore, reusable module cache

/// Protocol version enumeration for type-safe version comparisons.
///
/// This enum represents all known Stellar protocol versions from V0 to V25.
/// It is used with the version-checking functions to enable compile-time
/// verification of version comparisons.
///
/// # Representation
///
/// The enum uses `#[repr(u32)]` to ensure the discriminant values match
/// the actual protocol version numbers used on-chain.
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ProtocolVersion {
    V0 = 0,
    V1 = 1,
    V2 = 2,
    V3 = 3,
    V4 = 4,
    V5 = 5,
    V6 = 6,
    V7 = 7,
    V8 = 8,
    V9 = 9,
    V10 = 10,
    V11 = 11,
    V12 = 12,
    V13 = 13,
    V14 = 14,
    V15 = 15,
    V16 = 16,
    V17 = 17,
    V18 = 18,
    V19 = 19,
    V20 = 20,
    V21 = 21,
    V22 = 22,
    V23 = 23,
    V24 = 24,
    V25 = 25,
}

impl ProtocolVersion {
    /// Convert to the underlying `u32` value.
    pub const fn as_u32(self) -> u32 {
        self as u32
    }
}

// =============================================================================
// Protocol Version Constants
// =============================================================================

/// The protocol version when Soroban smart contracts were first introduced.
///
/// Soroban is Stellar's smart contract platform, enabling developers to write
/// and deploy WebAssembly-based contracts on the network.
pub const SOROBAN_PROTOCOL_VERSION: ProtocolVersion = ProtocolVersion::V20;

/// The protocol version when parallel Soroban execution was introduced.
///
/// This optimization allows independent smart contract invocations to be
/// executed concurrently, improving throughput.
pub const PARALLEL_SOROBAN_PHASE_PROTOCOL_VERSION: ProtocolVersion = ProtocolVersion::V23;

/// The protocol version when automatic TTL restoration was introduced.
///
/// Auto-restore allows expired contract data to be automatically restored
/// when accessed, simplifying contract state management.
pub const AUTO_RESTORE_PROTOCOL_VERSION: ProtocolVersion = ProtocolVersion::V23;

/// The protocol version when reusable Soroban module cache was introduced.
///
/// This optimization caches compiled WASM modules across transactions,
/// reducing redundant compilation overhead.
pub const REUSABLE_SOROBAN_MODULE_CACHE_PROTOCOL_VERSION: ProtocolVersion = ProtocolVersion::V23;

/// The minimum supported ledger protocol version.
///
/// This implementation only supports protocol versions 24 and above.
/// Ledgers with lower versions will be rejected.
pub const MIN_LEDGER_PROTOCOL_VERSION: u32 = 24;

/// The current maximum supported ledger protocol version.
///
/// This represents the highest protocol version that this implementation
/// can process. Ledgers with higher versions will be rejected.
pub const CURRENT_LEDGER_PROTOCOL_VERSION: u32 = 25;

/// The minimum supported ledger protocol version for Soroban execution.
///
/// Attempting to execute Soroban transactions on ledgers before this version
/// will fail.
pub const MIN_SOROBAN_PROTOCOL_VERSION: u32 = 20;

// =============================================================================
// Version Comparison Functions
// =============================================================================

/// Returns `true` if `version` is strictly before the target version.
///
/// # Example
///
/// ```rust
/// use henyey_common::protocol::{protocol_version_is_before, ProtocolVersion};
///
/// assert!(protocol_version_is_before(19, ProtocolVersion::V20));
/// assert!(!protocol_version_is_before(20, ProtocolVersion::V20));
/// ```
#[inline]
pub fn protocol_version_is_before(version: u32, before: ProtocolVersion) -> bool {
    version < before.as_u32()
}

/// Returns `true` if `version` is at or after the target version.
///
/// This is the most commonly used version check for feature gating.
///
/// # Example
///
/// ```rust
/// use henyey_common::protocol::{protocol_version_starts_from, ProtocolVersion};
///
/// assert!(protocol_version_starts_from(20, ProtocolVersion::V20));
/// assert!(protocol_version_starts_from(21, ProtocolVersion::V20));
/// assert!(!protocol_version_starts_from(19, ProtocolVersion::V20));
/// ```
#[inline]
pub fn protocol_version_starts_from(version: u32, from: ProtocolVersion) -> bool {
    version >= from.as_u32()
}

/// Returns `true` if an upgrade to the target version occurred between `prev_version` and `new_version`.
///
/// This is useful for detecting when a protocol upgrade has just happened and
/// special migration logic needs to run.
///
/// # Example
///
/// ```rust
/// use henyey_common::protocol::{needs_upgrade_to_version, ProtocolVersion};
///
/// // Upgrading from 19 to 20 crosses the V20 boundary
/// assert!(needs_upgrade_to_version(ProtocolVersion::V20, 19, 20));
///
/// // Already at 20, no upgrade crossing
/// assert!(!needs_upgrade_to_version(ProtocolVersion::V20, 20, 20));
///
/// // Upgrading from 19 to 21 still crosses V20
/// assert!(needs_upgrade_to_version(ProtocolVersion::V20, 19, 21));
/// ```
#[inline]
pub fn needs_upgrade_to_version(
    target: ProtocolVersion,
    prev_version: u32,
    new_version: u32,
) -> bool {
    protocol_version_is_before(prev_version, target)
        && protocol_version_starts_from(new_version, target)
}

/// Returns `true` if Soroban smart contracts are supported at the given protocol version.
///
/// # Example
///
/// ```rust
/// use henyey_common::protocol::soroban_supported;
///
/// assert!(!soroban_supported(19));
/// assert!(soroban_supported(20));
/// assert!(soroban_supported(25));
/// ```
#[inline]
pub fn soroban_supported(protocol_version: u32) -> bool {
    protocol_version_starts_from(protocol_version, SOROBAN_PROTOCOL_VERSION)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_protocol_version_is_before() {
        assert!(protocol_version_is_before(19, ProtocolVersion::V20));
        assert!(!protocol_version_is_before(20, ProtocolVersion::V20));
        assert!(!protocol_version_is_before(21, ProtocolVersion::V20));
    }

    #[test]
    fn test_protocol_version_starts_from() {
        assert!(!protocol_version_starts_from(19, ProtocolVersion::V20));
        assert!(protocol_version_starts_from(20, ProtocolVersion::V20));
        assert!(protocol_version_starts_from(21, ProtocolVersion::V20));
    }

    #[test]
    fn test_needs_upgrade_to_version() {
        // Upgrading from 19 to 20 needs upgrade to V20
        assert!(needs_upgrade_to_version(ProtocolVersion::V20, 19, 20));
        // Already at 20, no upgrade needed
        assert!(!needs_upgrade_to_version(ProtocolVersion::V20, 20, 20));
        // Upgrading from 20 to 21 doesn't need upgrade to V20
        assert!(!needs_upgrade_to_version(ProtocolVersion::V20, 20, 21));
        // Upgrading from 19 to 21 needs upgrade to V20
        assert!(needs_upgrade_to_version(ProtocolVersion::V20, 19, 21));
    }

    #[test]
    fn test_soroban_supported() {
        assert!(!soroban_supported(19));
        assert!(soroban_supported(20));
        assert!(soroban_supported(24));
        assert!(soroban_supported(25));
    }
}
