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
/// This enum represents all known Stellar protocol versions from V0 to V27.
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
    V26 = 26,
    V27 = 27,
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

/// The protocol version when frozen ledger keys (CAP-77) were introduced.
pub const FROZEN_LEDGER_KEYS_PROTOCOL_VERSION: ProtocolVersion = ProtocolVersion::V26;

/// The protocol version when hot archive bucket list was introduced.
///
/// From this version onward, the HAS includes hot archive bucket hashes and
/// the combined bucket list hash incorporates the hot archive hash.
pub const HOT_ARCHIVE_PROTOCOL_VERSION: ProtocolVersion = ProtocolVersion::V23;

/// The minimum supported ledger protocol version.
///
/// This implementation only supports protocol versions 24 and above.
/// Ledgers with lower versions will be rejected.
pub const MIN_LEDGER_PROTOCOL_VERSION: u32 = 24;

/// The current maximum supported ledger protocol version.
///
/// This represents the highest protocol version that this implementation
/// can process. Ledgers with higher versions will be rejected.
pub const CURRENT_LEDGER_PROTOCOL_VERSION: u32 = 27;

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

/// Returns `true` if the hot archive bucket list is active at the given protocol version.
///
/// # Example
///
/// ```rust
/// use henyey_common::protocol::hot_archive_supported;
///
/// assert!(!hot_archive_supported(22));
/// assert!(hot_archive_supported(23));
/// assert!(hot_archive_supported(25));
/// ```
#[inline]
pub fn hot_archive_supported(protocol_version: u32) -> bool {
    protocol_version_starts_from(protocol_version, HOT_ARCHIVE_PROTOCOL_VERSION)
}

// =============================================================================
// Apply-time upgrade validity (non-Config)
// =============================================================================

/// Bitmask of valid ledger-header flags (`MASK_LEDGER_HEADER_FLAGS`).
///
/// Mirrors stellar-core's `MASK_LEDGER_HEADER_FLAGS = 0x7` (bits 0-2). A
/// `LedgerUpgrade::Flags` upgrade is only valid if no bits outside this mask
/// are set.
pub const MASK_LEDGER_HEADER_FLAGS: u32 = 0x7;

/// Re-validate a non-`Config` ledger upgrade for application, mirroring
/// stellar-core `Upgrades::isValidForApply` (Upgrades.cpp:565-637) for the
/// scalar (non-Config) arms.
///
/// This is the single shared source of truth for the apply-time re-check
/// performed by the ledger close path (LEDGER_SPEC §7.3.4 step 2). It lives in
/// `henyey-common` because `henyey-ledger` cannot depend on `henyey-herder`
/// (herder depends on ledger — a cycle), and herder's nomination-time validity
/// rules cannot be reused directly from the ledger apply path.
///
/// The `Config` variant is deliberately out of this helper's remit: Config
/// validity requires ledger-state lookups (the config upgrade set must be
/// loadable and structurally valid), which is performed by the ledger-side
/// `apply_config_upgrades`. Callers must branch on `Config` themselves; this
/// function returns `true` for `Config` so a caller that forwards every upgrade
/// here does not incorrectly skip a Config upgrade that the ledger-side path
/// will validate.
///
/// # Arguments
///
/// * `upgrade` - the upgrade to validate
/// * `current_version` - the protocol version in effect *before* this upgrade
///   (callers thread an `effective_version` that advances as valid `Version`
///   upgrades are accepted, mirroring core re-reading the header per upgrade)
/// * `max_protocol_version` - the maximum protocol version this node supports
///
/// # Parity
///
/// Matches the non-Config arms of stellar-core `Upgrades::isValidForApply`:
/// - `Version`: `new > current && new <= max`
/// - `BaseFee`: `fee != 0`
/// - `MaxTxSetSize`: always valid
/// - `BaseReserve`: `reserve != 0`
/// - `Flags`: protocol >= V18 and no bits outside `MASK_LEDGER_HEADER_FLAGS`
/// - `MaxSorobanTxSetSize`: protocol >= V20
pub fn upgrade_valid_for_apply_non_config(
    upgrade: &stellar_xdr::LedgerUpgrade,
    current_version: u32,
    max_protocol_version: u32,
) -> bool {
    use stellar_xdr::LedgerUpgrade;
    match upgrade {
        LedgerUpgrade::Version(new_version) => {
            *new_version <= max_protocol_version && *new_version > current_version
        }
        LedgerUpgrade::BaseFee(fee) => *fee != 0,
        LedgerUpgrade::MaxTxSetSize(_) => true,
        LedgerUpgrade::BaseReserve(reserve) => *reserve != 0,
        LedgerUpgrade::Flags(flags) => {
            protocol_version_starts_from(current_version, ProtocolVersion::V18)
                && (*flags & !MASK_LEDGER_HEADER_FLAGS) == 0
        }
        LedgerUpgrade::MaxSorobanTxSetSize(_) => {
            protocol_version_starts_from(current_version, ProtocolVersion::V20)
        }
        // Config validity is delegated to the ledger-side apply path (it
        // requires ledger-state lookups). Treat as valid here so a caller that
        // forwards every upgrade does not skip a Config upgrade.
        LedgerUpgrade::Config(_) => true,
    }
}

// =============================================================================
// LCL Context
// =============================================================================

/// Context from the Last Closed Ledger needed for tx-set format selection.
///
/// Bundles the LCL hash and protocol version so they cannot be mixed from
/// different ledgers. Mirrors stellar-core's `TxSetXDRFrame::makeEmpty(lclHeader)`
/// which takes a single `lclHeader` argument providing both hash and protocol.
///
/// Private fields ensure construction only through approved constructors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LclContext {
    /// Hash of the LCL (becomes `previous_ledger_hash` in synthesized tx sets).
    lcl_hash: stellar_xdr::Hash,
    /// The LCL's protocol version (determines Classic vs Generalized format).
    protocol_version: u32,
}

impl LclContext {
    /// Construct from the LCL's protocol version and hash.
    ///
    /// Both values must come from the same ledger header. For the live-node path,
    /// prefer `From<&HeaderSnapshot>` (implemented in `henyey-ledger`) which
    /// provides this guarantee structurally.
    pub fn new(protocol_version: u32, lcl_hash: crate::Hash256) -> Self {
        Self {
            lcl_hash: stellar_xdr::Hash(lcl_hash.0),
            protocol_version,
        }
    }

    /// Construct for the pre-genesis case (ledger 0, before any close).
    ///
    /// At genesis, there is no LCL — the "previous ledger hash" is all zeros
    /// and the protocol version is 0 (Classic format).
    pub fn pre_genesis() -> Self {
        Self {
            lcl_hash: stellar_xdr::Hash([0u8; 32]),
            protocol_version: 0,
        }
    }

    /// The LCL hash (used as `previous_ledger_hash` in synthesized tx sets).
    pub fn lcl_hash(&self) -> &stellar_xdr::Hash {
        &self.lcl_hash
    }

    /// The LCL's protocol version.
    pub fn protocol_version(&self) -> u32 {
        self.protocol_version
    }
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
    fn test_protocol_version_v27_value() {
        assert_eq!(ProtocolVersion::V27 as u32, 27);
        assert_eq!(ProtocolVersion::V27.as_u32(), 27);
    }

    #[test]
    fn test_current_ledger_protocol_version_is_27() {
        assert_eq!(CURRENT_LEDGER_PROTOCOL_VERSION, 27);
    }

    #[test]
    fn test_ledger_header_version_27_xdr_roundtrip() {
        // A protocol-27 LedgerHeader must round-trip through the workspace XDR
        // (stellar-xdr 27.0.0), confirming the toolchain recognizes ledger_version 27.
        use stellar_xdr::{Limits, ReadXdr, WriteXdr};
        let header = stellar_xdr::LedgerHeader {
            ledger_version: 27,
            previous_ledger_hash: stellar_xdr::Hash([0u8; 32]),
            scp_value: stellar_xdr::StellarValue {
                tx_set_hash: stellar_xdr::Hash([0u8; 32]),
                close_time: stellar_xdr::TimePoint(0),
                upgrades: vec![].try_into().unwrap(),
                ext: stellar_xdr::StellarValueExt::Basic,
            },
            tx_set_result_hash: stellar_xdr::Hash([0u8; 32]),
            bucket_list_hash: stellar_xdr::Hash([0u8; 32]),
            ledger_seq: 1,
            total_coins: 0,
            fee_pool: 0,
            inflation_seq: 0,
            id_pool: 0,
            base_fee: 100,
            base_reserve: 5_000_000,
            max_tx_set_size: 1000,
            skip_list: [
                stellar_xdr::Hash([0u8; 32]),
                stellar_xdr::Hash([0u8; 32]),
                stellar_xdr::Hash([0u8; 32]),
                stellar_xdr::Hash([0u8; 32]),
            ],
            ext: stellar_xdr::LedgerHeaderExt::V0,
        };
        let bytes = header.to_xdr(Limits::none()).expect("encode v27 header");
        let decoded =
            stellar_xdr::LedgerHeader::from_xdr(&bytes, Limits::none()).expect("decode v27 header");
        assert_eq!(decoded.ledger_version, 27);

        // ScVal round-trip (smoke).
        let sv = stellar_xdr::ScVal::U32(27);
        let sv_bytes = sv.to_xdr(Limits::none()).expect("encode scval");
        let sv_decoded =
            stellar_xdr::ScVal::from_xdr(&sv_bytes, Limits::none()).expect("decode scval");
        assert_eq!(sv_decoded, stellar_xdr::ScVal::U32(27));
    }

    #[test]
    fn test_protocol_version_starts_from_v27() {
        // A v27 ledger is at or after V27; a v26 ledger is before V27.
        assert!(protocol_version_starts_from(27, ProtocolVersion::V27));
        assert!(!protocol_version_starts_from(26, ProtocolVersion::V27));
        assert!(protocol_version_is_before(26, ProtocolVersion::V27));
        assert!(!protocol_version_is_before(27, ProtocolVersion::V27));
        // V27 is strictly after V26.
        assert!(ProtocolVersion::V27 > ProtocolVersion::V26);
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
        assert!(soroban_supported(26));
    }

    #[test]
    fn test_lcl_context_pre_genesis() {
        let lcl = LclContext::pre_genesis();
        assert_eq!(lcl.protocol_version(), 0);
        assert_eq!(lcl.lcl_hash(), &stellar_xdr::Hash([0u8; 32]));
    }

    #[test]
    fn test_lcl_context_new() {
        let hash = crate::Hash256([42u8; 32]);
        let lcl = LclContext::new(23, hash);
        assert_eq!(lcl.protocol_version(), 23);
        assert_eq!(lcl.lcl_hash(), &stellar_xdr::Hash([42u8; 32]));
    }

    // ---- upgrade_valid_for_apply_non_config -------------------------------

    #[test]
    fn test_upgrade_valid_version_monotonic_and_range() {
        use stellar_xdr::LedgerUpgrade;
        // Strictly increasing and within max → valid.
        assert!(upgrade_valid_for_apply_non_config(
            &LedgerUpgrade::Version(26),
            25,
            26
        ));
        // Regression (new <= current) → invalid.
        assert!(!upgrade_valid_for_apply_non_config(
            &LedgerUpgrade::Version(25),
            26,
            26
        ));
        // Equal to current → invalid (not strictly increasing).
        assert!(!upgrade_valid_for_apply_non_config(
            &LedgerUpgrade::Version(25),
            25,
            26
        ));
        // Above max supported → invalid.
        assert!(!upgrade_valid_for_apply_non_config(
            &LedgerUpgrade::Version(27),
            25,
            26
        ));
    }

    #[test]
    fn test_upgrade_valid_base_fee_nonzero() {
        use stellar_xdr::LedgerUpgrade;
        assert!(upgrade_valid_for_apply_non_config(
            &LedgerUpgrade::BaseFee(100),
            25,
            26
        ));
        assert!(!upgrade_valid_for_apply_non_config(
            &LedgerUpgrade::BaseFee(0),
            25,
            26
        ));
    }

    #[test]
    fn test_upgrade_valid_base_reserve_nonzero() {
        use stellar_xdr::LedgerUpgrade;
        assert!(upgrade_valid_for_apply_non_config(
            &LedgerUpgrade::BaseReserve(5_000_000),
            25,
            26
        ));
        assert!(!upgrade_valid_for_apply_non_config(
            &LedgerUpgrade::BaseReserve(0),
            25,
            26
        ));
    }

    #[test]
    fn test_upgrade_valid_max_tx_set_size_always_valid() {
        use stellar_xdr::LedgerUpgrade;
        assert!(upgrade_valid_for_apply_non_config(
            &LedgerUpgrade::MaxTxSetSize(0),
            25,
            26
        ));
        assert!(upgrade_valid_for_apply_non_config(
            &LedgerUpgrade::MaxTxSetSize(1000),
            25,
            26
        ));
    }

    #[test]
    fn test_upgrade_valid_flags_mask_and_protocol() {
        use stellar_xdr::LedgerUpgrade;
        // Valid: within mask, protocol >= V18.
        assert!(upgrade_valid_for_apply_non_config(
            &LedgerUpgrade::Flags(MASK_LEDGER_HEADER_FLAGS),
            25,
            26
        ));
        // Invalid: bit outside the mask.
        assert!(!upgrade_valid_for_apply_non_config(
            &LedgerUpgrade::Flags(MASK_LEDGER_HEADER_FLAGS | 0x8),
            25,
            26
        ));
        // Invalid: protocol below V18 (not reachable in henyey's 24+ floor,
        // but the rule must still hold).
        assert!(!upgrade_valid_for_apply_non_config(
            &LedgerUpgrade::Flags(0x1),
            17,
            26
        ));
    }

    #[test]
    fn test_upgrade_valid_max_soroban_tx_set_size_protocol_gate() {
        use stellar_xdr::LedgerUpgrade;
        assert!(upgrade_valid_for_apply_non_config(
            &LedgerUpgrade::MaxSorobanTxSetSize(100),
            20,
            26
        ));
        assert!(!upgrade_valid_for_apply_non_config(
            &LedgerUpgrade::MaxSorobanTxSetSize(100),
            19,
            26
        ));
    }

    #[test]
    fn test_upgrade_valid_config_delegated_returns_true() {
        use stellar_xdr::{ConfigUpgradeSetKey, ContractId, Hash, LedgerUpgrade};
        let key = ConfigUpgradeSetKey {
            contract_id: ContractId(Hash([0u8; 32])),
            content_hash: Hash([0u8; 32]),
        };
        // Config is delegated to ledger-side validation → helper returns true.
        assert!(upgrade_valid_for_apply_non_config(
            &LedgerUpgrade::Config(key),
            25,
            26
        ));
    }
}
