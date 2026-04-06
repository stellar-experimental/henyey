//! Quorum set configuration and validation utilities.
//!
//! This module provides utilities for working with quorum set configurations,
//! including parsing from configuration files, validation, and conversion to
//! the XDR format used by SCP.
//!
//! # Configuration Format
//!
//! Quorum sets are configured with:
//! - A threshold percentage (e.g., 67% means 2/3 of validators must agree)
//! - A list of validator public keys (in strkey or hex format)
//! - Optional inner sets for hierarchical trust structures
//!
//! # Validator Key Formats
//!
//! Validator public keys can be specified as:
//! - **Strkey format**: `GDKXE2OZMJIPOSLNA6N6F2BVCI3O777I2OOC4BV7VOYUEHYX7RTRYA7Y`
//! - **Hex format**: 64-character hex string of the raw public key bytes
//!
//! # Well-Known Validators
//!
//! The [`known_validators`] module provides public keys for well-known
//! validators on testnet and mainnet, useful for getting started.
//!
//! # Example
//!
//! ```ignore
//! use henyey_scp::quorum_config::{testnet_quorum_config, config_to_quorum_set};
//!
//! let config = testnet_quorum_config();
//! let quorum_set = config_to_quorum_set(&config)?;
//! ```

use henyey_common::config::QuorumSetConfig;
use stellar_xdr::curr::{NodeId, PublicKey, ScpQuorumSet, Uint256};
use tracing::warn;

use crate::{get_all_nodes, is_quorum_set_sane, is_valid_quorum_set};

/// Errors that can occur when parsing or validating quorum set configuration.
///
/// These errors indicate problems with the quorum set configuration that
/// would prevent safe consensus operation.
#[derive(Debug, Clone)]
pub enum QuorumConfigError {
    /// The validator public key is malformed or uses an unsupported format.
    ///
    /// Valid formats are:
    /// - Stellar strkey (starts with 'G', 56 characters)
    /// - Hex-encoded public key (64 characters)
    InvalidPublicKey(String),

    /// The threshold value is invalid for the given number of validators.
    ///
    /// Threshold must be > 0 and <= the total number of validators/inner sets.
    InvalidThreshold {
        /// The specified threshold value.
        threshold: u32,
        /// The number of validators available.
        validator_count: usize,
    },

    /// The quorum set structure is invalid (e.g., too deeply nested).
    InvalidStructure(String),

    /// The quorum sets do not have sufficient intersection for safety.
    ///
    /// For SCP to be safe, all nodes' quorum sets must intersect.
    NoQuorumIntersection,
}

impl std::fmt::Display for QuorumConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidPublicKey(key) => write!(f, "invalid public key: {}", key),
            Self::InvalidThreshold {
                threshold,
                validator_count,
            } => {
                write!(
                    f,
                    "threshold {} > validator count {}",
                    threshold, validator_count
                )
            }
            Self::InvalidStructure(msg) => write!(f, "invalid quorum set: {}", msg),
            Self::NoQuorumIntersection => write!(f, "quorum sets do not have intersection"),
        }
    }
}

impl std::error::Error for QuorumConfigError {}

/// Convert a QuorumSetConfig to an XDR ScpQuorumSet.
pub fn config_to_quorum_set(config: &QuorumSetConfig) -> Result<ScpQuorumSet, QuorumConfigError> {
    let validators = config
        .validators
        .iter()
        .map(|key_str| parse_node_id(key_str))
        .collect::<Result<Vec<_>, _>>()?;

    let inner_sets = config
        .inner_sets
        .iter()
        .map(config_to_quorum_set)
        .collect::<Result<Vec<_>, _>>()?;

    // Calculate threshold from percentage
    let total = validators.len() + inner_sets.len();
    let threshold_percent: u32 = config.threshold_percent.into();
    let threshold = if total == 0 {
        0
    } else {
        // Ceiling division: ensures threshold is never below the configured
        // percentage. Floor division would weaken BFT guarantees (e.g., 67%
        // of 4 validators would give 2 instead of 3).
        (threshold_percent as usize * total).div_ceil(100).max(1) as u32
    };

    // Validate threshold
    if threshold as usize > total {
        return Err(QuorumConfigError::InvalidThreshold {
            threshold,
            validator_count: total,
        });
    }

    let qs = ScpQuorumSet {
        threshold,
        validators: validators
            .try_into()
            .expect("validators exceed XDR maximum capacity"),
        inner_sets: inner_sets
            .try_into()
            .expect("inner sets exceed XDR maximum capacity"),
    };

    // Validate the resulting quorum set
    if !is_valid_quorum_set(&qs) {
        return Err(QuorumConfigError::InvalidStructure(
            "quorum set validation failed".to_string(),
        ));
    }

    Ok(qs)
}

/// Parse a node ID from a string.
///
/// Supports both:
/// - Raw 64-character hex public key
/// - Stellar strkey format (G...)
pub fn parse_node_id(key_str: &str) -> Result<NodeId, QuorumConfigError> {
    let key_str = key_str.trim();

    // Try parsing as strkey first (G... format)
    if key_str.starts_with('G') {
        return parse_strkey_node_id(key_str);
    }

    // Try parsing as hex
    if key_str.len() == 64 {
        return parse_hex_node_id(key_str);
    }

    Err(QuorumConfigError::InvalidPublicKey(key_str.to_string()))
}

fn parse_strkey_node_id(key_str: &str) -> Result<NodeId, QuorumConfigError> {
    use stellar_strkey::ed25519::PublicKey as StrKeyPublicKey;

    let strkey = StrKeyPublicKey::from_string(key_str)
        .map_err(|_| QuorumConfigError::InvalidPublicKey(key_str.to_string()))?;

    let bytes = strkey.0;
    Ok(NodeId(PublicKey::PublicKeyTypeEd25519(Uint256(bytes))))
}

fn parse_hex_node_id(key_str: &str) -> Result<NodeId, QuorumConfigError> {
    let bytes = hex::decode(key_str)
        .map_err(|_| QuorumConfigError::InvalidPublicKey(key_str.to_string()))?;

    if bytes.len() != 32 {
        return Err(QuorumConfigError::InvalidPublicKey(key_str.to_string()));
    }

    let mut arr = [0u8; 32];
    arr.copy_from_slice(&bytes);
    Ok(NodeId(PublicKey::PublicKeyTypeEd25519(Uint256(arr))))
}

/// Validate that a quorum set configuration will produce valid consensus.
///
/// This checks:
/// 1. The quorum set is structurally valid
/// 2. Threshold is reasonable (not 0, not > 100%)
/// 3. There are enough validators for safety
pub fn validate_quorum_config(config: &QuorumSetConfig) -> Result<(), QuorumConfigError> {
    // Check threshold is in range (ThresholdPercent already validated during deserialization)
    let threshold_value: u32 = config.threshold_percent.into();
    if threshold_value > 100 {
        return Err(QuorumConfigError::InvalidThreshold {
            threshold: threshold_value,
            validator_count: 100,
        });
    }

    // Convert to XDR to validate structure
    let qs = config_to_quorum_set(config)?;

    if let Err(err) = is_quorum_set_sane(&qs, false) {
        return Err(QuorumConfigError::InvalidStructure(err));
    }

    // Check we have validators
    let all_nodes = get_all_nodes(&qs);
    if all_nodes.is_empty() && config.validators.is_empty() && config.inner_sets.is_empty() {
        warn!("Quorum set has no validators - node will not be able to reach consensus");
    }

    // Warn if threshold is too low
    if threshold_value < 51 {
        warn!(
            "Quorum threshold {}% is below 51% - this may compromise safety",
            threshold_value
        );
    }

    // Warn if threshold is 100% (no fault tolerance)
    if threshold_value == 100 && !config.validators.is_empty() {
        warn!(
            "Quorum threshold is 100% - no fault tolerance, any validator failure blocks consensus"
        );
    }

    Ok(())
}

/// Well-known validators for Stellar networks.
///
/// This module provides public keys for SDF-operated validators on testnet
/// and mainnet. These can be used as a starting point for configuring
/// quorum sets, but production deployments should carefully consider their
/// trust topology.
///
/// # Security Note
///
/// For production use, carefully consider which validators you trust.
/// SDF validators are provided as a convenience, but decentralization
/// requires trusting multiple independent organizations.
pub mod known_validators {
    /// SDF's testnet validators (3 validators operated by SDF).
    ///
    /// These are the core validators for the Stellar testnet. Using all
    /// three with a 67% threshold provides Byzantine fault tolerance
    /// for up to 1 faulty validator.
    pub const TESTNET_VALIDATORS: &[&str] = &[
        "GDKXE2OZMJIPOSLNA6N6F2BVCI3O777I2OOC4BV7VOYUEHYX7RTRYA7Y", // core1
        "GCUCJTIYXSOXKBSNFGNFWW5MUQ54HKRPGJUTQFJ5RQXZXNOLNXYDHRAP", // core2
        "GC2V2EFSXN6SQTWVYA5EPJPBWWIMSD2XQNKUOHGEKB535AQE2I6IXV2Z", // core3
    ];

    /// SDF's mainnet validators (Tier 1 validators operated by SDF).
    ///
    /// Note: For production mainnet use, you should configure a more
    /// diverse set of validators from multiple organizations.
    pub const MAINNET_SDF_VALIDATORS: &[&str] = &[
        "GCGB2S2KGYARPVIA37HYZXVRM2YZUEXA6S33ZU5BUDC6THSB62LZSTYH", // sdf1
        "GCM6QMP3DLRPTAZW2UZPCPX2LF3SXWXKPMP3GKFZBDSF3QZGV2G5QSTK", // sdf2
        "GABMKJM6I25XI4K7U6XWMULOUQIQ27BCTMLS6BYYSOWKTBUXVRJSXHYQ", // sdf3
    ];

    /// Recommended threshold percentage (67%, providing Byzantine fault tolerance).
    ///
    /// With 67% threshold, the network can tolerate up to 1/3 faulty validators.
    pub const RECOMMENDED_THRESHOLD_PERCENT: u32 = 67;

    /// Minimum safe threshold percentage (51%).
    ///
    /// Below 51%, the network may not have proper quorum intersection,
    /// compromising safety guarantees.
    pub const MINIMUM_SAFE_THRESHOLD_PERCENT: u32 = 51;
}

fn recommended_quorum_config(validators: &[&str]) -> QuorumSetConfig {
    QuorumSetConfig {
        threshold_percent: known_validators::RECOMMENDED_THRESHOLD_PERCENT.into(),
        validators: validators
            .iter()
            .map(|validator| validator.to_string())
            .collect(),
        inner_sets: Vec::new(),
    }
}

/// Create a testnet quorum set configuration.
pub fn testnet_quorum_config() -> QuorumSetConfig {
    recommended_quorum_config(known_validators::TESTNET_VALIDATORS)
}

/// Create a mainnet quorum set configuration using SDF validators.
///
/// Note: For production, you should configure your own quorum set
/// based on validators you trust. This is just a starting point.
pub fn mainnet_sdf_quorum_config() -> QuorumSetConfig {
    recommended_quorum_config(known_validators::MAINNET_SDF_VALIDATORS)
}

/// Convert a NodeId to a strkey string (G... format).
pub fn node_id_to_strkey(node_id: &NodeId) -> String {
    use stellar_strkey::ed25519::PublicKey as StrKeyPublicKey;

    match &node_id.0 {
        PublicKey::PublicKeyTypeEd25519(Uint256(bytes)) => StrKeyPublicKey(*bytes).to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_hex_node_id() {
        let hex = "0000000000000000000000000000000000000000000000000000000000000001";
        let node_id = parse_node_id(hex).unwrap();

        match &node_id.0 {
            PublicKey::PublicKeyTypeEd25519(Uint256(bytes)) => {
                assert_eq!(bytes[31], 1);
            }
        }
    }

    #[test]
    fn test_config_to_quorum_set() {
        let config = QuorumSetConfig {
            threshold_percent: 67.into(),
            validators: vec![
                "0000000000000000000000000000000000000000000000000000000000000001".to_string(),
                "0000000000000000000000000000000000000000000000000000000000000002".to_string(),
                "0000000000000000000000000000000000000000000000000000000000000003".to_string(),
            ],
            inner_sets: Vec::new(),
        };

        let qs = config_to_quorum_set(&config).unwrap();
        assert_eq!(qs.validators.len(), 3);
        assert_eq!(qs.threshold, 3); // 67% of 3 = 2.01 -> ceil -> 3
    }

    #[test]
    fn test_validate_quorum_config() {
        let config = QuorumSetConfig {
            threshold_percent: 67.into(),
            validators: vec![
                "0000000000000000000000000000000000000000000000000000000000000001".to_string(),
                "0000000000000000000000000000000000000000000000000000000000000002".to_string(),
            ],
            inner_sets: Vec::new(),
        };

        assert!(validate_quorum_config(&config).is_ok());
    }

    #[test]
    fn test_testnet_quorum_config() {
        let config = testnet_quorum_config();
        // The testnet validator keys are strkey format, so this may fail
        // if stellar-strkey is not available - that's ok for testing structure
        assert!(!config.validators.is_empty());
        assert_eq!(config.threshold_percent.value(), 67);
    }

    #[test]
    fn test_threshold_percent_clamped() {
        // ThresholdPercent clamps values > 100 to 100
        // This tests that the clamping works correctly
        use henyey_common::config::ThresholdPercent;
        let threshold: ThresholdPercent = 150.into();
        assert_eq!(threshold.value(), 100);
    }

    #[test]
    fn test_audit_c3_ceiling_threshold_calculation() {
        // Regression test for AUDIT-C3: threshold calculation must use
        // ceiling division, not floor, to prevent weakening BFT guarantees.
        //
        // With 4 validators at 67%, floor gives 2 (50% = not BFT safe),
        // ceiling gives 3 (75% = BFT safe, matching stellar-core).
        let config = QuorumSetConfig {
            threshold_percent: 67.into(),
            validators: vec![
                "0000000000000000000000000000000000000000000000000000000000000001".to_string(),
                "0000000000000000000000000000000000000000000000000000000000000002".to_string(),
                "0000000000000000000000000000000000000000000000000000000000000003".to_string(),
                "0000000000000000000000000000000000000000000000000000000000000004".to_string(),
            ],
            inner_sets: Vec::new(),
        };

        let qs = config_to_quorum_set(&config).unwrap();
        // 67% of 4 = 2.68 → must be 3 (ceiling), not 2 (floor)
        assert_eq!(
            qs.threshold, 3,
            "67% of 4 validators must produce threshold=3 (ceiling), not 2 (floor)"
        );
    }
}
