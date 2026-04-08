//! Ledger upgrade scheduling and validation.
//!
//! This module provides support for protocol upgrades in Stellar. It matches
//! the stellar-core `Upgrades` class in `Upgrades.h`.
//!
//! # Overview
//!
//! Upgrades allow the network to coordinate changes to protocol parameters:
//! - Protocol version
//! - Base fee
//! - Max transaction set size
//! - Base reserve
//! - Ledger flags
//! - Soroban configuration
//!
//! Validators schedule upgrades for a specific time, and the upgrade is applied
//! when consensus is reached after that time.
//!
//! # Example
//!
//! ```ignore
//! use henyey_herder::upgrades::{UpgradeParameters, Upgrades};
//!
//! // Create upgrade parameters
//! let mut params = UpgradeParameters::default();
//! params.protocol_version = Some(23);
//! params.base_fee = Some(100);
//!
//! // Create an Upgrades instance
//! let upgrades = Upgrades::new(params);
//!
//! // Check if it's time for the upgrade
//! let current_time = 1234567890;
//! if upgrades.time_for_upgrade(current_time) {
//!     // Create upgrade proposals
//!     let proposals = upgrades.create_upgrades_for(ledger_header);
//! }
//! ```

use henyey_common::protocol::{protocol_version_starts_from, ProtocolVersion};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use stellar_xdr::curr::{ConfigUpgradeSetKey, LedgerUpgrade, ReadXdr, UpgradeType};

/// Default expiration time for pending upgrades (12 hours, matching upstream).
pub const DEFAULT_UPGRADE_EXPIRATION_HOURS: u64 = 12;

/// Upgrade validity result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpgradeValidity {
    /// The upgrade is valid and can be applied.
    Valid,
    /// The upgrade XDR could not be deserialized.
    XdrInvalid,
    /// The upgrade is invalid for some other reason.
    Invalid,
}

/// Parameters for scheduled ledger upgrades.
///
/// Contains the scheduled upgrade time and optional values for each
/// upgradeable parameter. Only parameters with `Some` value will be
/// proposed for upgrade.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UpgradeParameters {
    /// The time at which this upgrade should be applied (Unix timestamp).
    #[serde(default)]
    pub upgrade_time: u64,

    /// Target protocol version.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub protocol_version: Option<u32>,

    /// Target base fee (in stroops).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_fee: Option<u32>,

    /// Target max transaction set size (number of operations).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tx_set_size: Option<u32>,

    /// Target base reserve (in stroops).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_reserve: Option<u32>,

    /// Target ledger header flags.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub flags: Option<u32>,

    /// Target max Soroban transaction set size.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_soroban_tx_set_size: Option<u32>,

    /// Key for Soroban config upgrade set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config_upgrade_set_key: Option<ConfigUpgradeSetKeyJson>,

    /// Maximum number of nomination timeouts before stripping upgrade.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nomination_timeout_limit: Option<u32>,

    /// Minutes after scheduled time before upgrade expires.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expiration_minutes: Option<u64>,
}

/// JSON-serializable wrapper for ConfigUpgradeSetKey.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigUpgradeSetKeyJson {
    /// Contract ID (base64 encoded).
    pub contract_id: String,
    /// Content hash (base64 encoded).
    pub content_hash: String,
}

impl ConfigUpgradeSetKeyJson {
    /// Create from XDR ConfigUpgradeSetKey.
    pub fn from_xdr(key: &ConfigUpgradeSetKey) -> Self {
        use base64::{engine::general_purpose::STANDARD, Engine};
        Self {
            // ContractId wraps Hash which wraps [u8; 32]
            contract_id: STANDARD.encode(key.contract_id.0 .0),
            content_hash: STANDARD.encode(key.content_hash.0),
        }
    }

    /// Convert to XDR ConfigUpgradeSetKey.
    pub fn to_xdr(&self) -> std::result::Result<ConfigUpgradeSetKey, String> {
        use base64::{engine::general_purpose::STANDARD, Engine};

        let contract_id = STANDARD
            .decode(&self.contract_id)
            .map_err(|e| format!("Invalid contract_id base64: {}", e))?;
        let content_hash = STANDARD
            .decode(&self.content_hash)
            .map_err(|e| format!("Invalid content_hash base64: {}", e))?;

        if contract_id.len() != 32 {
            return Err(format!(
                "contract_id must be 32 bytes, got {}",
                contract_id.len()
            ));
        }
        if content_hash.len() != 32 {
            return Err(format!(
                "content_hash must be 32 bytes, got {}",
                content_hash.len()
            ));
        }

        let mut cid = [0u8; 32];
        let mut ch = [0u8; 32];
        cid.copy_from_slice(&contract_id);
        ch.copy_from_slice(&content_hash);

        Ok(ConfigUpgradeSetKey {
            contract_id: stellar_xdr::curr::ContractId(stellar_xdr::curr::Hash(cid)),
            content_hash: stellar_xdr::curr::Hash(ch),
        })
    }
}

impl UpgradeParameters {
    /// Create new upgrade parameters with the given upgrade time.
    pub fn new(upgrade_time: u64) -> Self {
        Self {
            upgrade_time,
            ..Default::default()
        }
    }

    /// Create upgrade parameters from current system time plus offset.
    pub fn from_now(offset: Duration) -> Self {
        let upgrade_time = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before UNIX epoch")
            .as_secs()
            + offset.as_secs();
        Self::new(upgrade_time)
    }

    /// Check if any upgrade parameter is set.
    pub fn has_any_upgrade(&self) -> bool {
        self.protocol_version.is_some()
            || self.base_fee.is_some()
            || self.max_tx_set_size.is_some()
            || self.base_reserve.is_some()
            || self.flags.is_some()
            || self.max_soroban_tx_set_size.is_some()
            || self.config_upgrade_set_key.is_some()
    }

    /// Get the expiration time in seconds.
    pub fn expiration_seconds(&self) -> u64 {
        self.expiration_minutes
            .map(|m| m * 60)
            .unwrap_or(DEFAULT_UPGRADE_EXPIRATION_HOURS * 3600)
    }

    /// Check if the upgrade has expired at the given time.
    ///
    /// `upgrade_time == 0` means "apply immediately" (from HTTP
    /// `upgradetime=1970-01-01T00:00:00Z`). These never expire because
    /// in an async runtime, `remove_upgrades` (called on every ledger
    /// close) can race with the upgrade being proposed — clearing the
    /// params before they're ever included in an SCP value.
    pub fn is_expired(&self, current_time: u64) -> bool {
        if self.upgrade_time == 0 {
            return false;
        }
        current_time > self.upgrade_time + self.expiration_seconds()
    }

    /// Serialize to JSON string.
    pub fn to_json(&self) -> std::result::Result<String, serde_json::Error> {
        serde_json::to_string(self)
    }

    /// Deserialize from JSON string.
    pub fn from_json(s: &str) -> std::result::Result<Self, serde_json::Error> {
        serde_json::from_str(s)
    }
}

impl fmt::Display for UpgradeParameters {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut parts = Vec::new();

        // Format upgrade time in ISO format
        let datetime = UNIX_EPOCH + Duration::from_secs(self.upgrade_time);
        if let Ok(duration) = datetime.duration_since(UNIX_EPOCH) {
            parts.push(format!("upgradetime={}", duration.as_secs()));
        }

        if let Some(v) = self.protocol_version {
            parts.push(format!("protocolversion={}", v));
        }
        if let Some(v) = self.base_fee {
            parts.push(format!("basefee={}", v));
        }
        if let Some(v) = self.base_reserve {
            parts.push(format!("basereserve={}", v));
        }
        if let Some(v) = self.max_tx_set_size {
            parts.push(format!("maxtxsetsize={}", v));
        }
        if let Some(v) = self.max_soroban_tx_set_size {
            parts.push(format!("maxsorobantxsetsize={}", v));
        }
        if let Some(v) = self.flags {
            parts.push(format!("flags={}", v));
        }
        if self.config_upgrade_set_key.is_some() {
            parts.push("configupgradesetkey=<set>".to_string());
        }

        write!(f, "{}", parts.join(", "))
    }
}

/// Current ledger state for upgrade proposal generation.
///
/// Groups the ledger parameters that `create_upgrades_for` compares against
/// scheduled upgrade targets.
#[derive(Debug, Clone, Default)]
pub struct CurrentLedgerState {
    /// Current ledger close time.
    pub close_time: u64,
    /// Current protocol version.
    pub protocol_version: u32,
    /// Current base fee (in stroops).
    pub base_fee: u32,
    /// Current max transaction set size (number of operations).
    pub max_tx_set_size: u32,
    /// Current base reserve (in stroops).
    pub base_reserve: u32,
    /// Current ledger header flags.
    pub flags: u32,
    /// Current max Soroban transaction set size (if Soroban enabled).
    pub max_soroban_tx_set_size: Option<u32>,
}

/// Ledger upgrade scheduling and validation.
///
/// Manages scheduled upgrades and validates upgrade proposals during
/// consensus.
#[derive(Debug, Clone, Default)]
pub struct Upgrades {
    /// Current upgrade parameters.
    params: UpgradeParameters,
}

impl Upgrades {
    /// Create a new Upgrades instance with the given parameters.
    pub fn new(params: UpgradeParameters) -> Self {
        Self { params }
    }

    /// Get the current upgrade parameters.
    pub fn parameters(&self) -> &UpgradeParameters {
        &self.params
    }

    /// Set new upgrade parameters.
    ///
    /// # Arguments
    ///
    /// * `params` - New upgrade parameters
    /// * `max_protocol_version` - Maximum supported protocol version
    ///
    /// # Errors
    ///
    /// Returns an error if the requested protocol version exceeds the maximum.
    pub fn set_parameters(
        &mut self,
        params: UpgradeParameters,
        max_protocol_version: u32,
    ) -> std::result::Result<(), String> {
        if let Some(version) = params.protocol_version {
            if version > max_protocol_version {
                return Err(format!(
                    "Protocol version error: supported is up to {}, passed is {}",
                    max_protocol_version, version
                ));
            }
        }
        self.params = params;
        Ok(())
    }

    /// Check if it's time for the upgrade.
    ///
    /// Returns true if the current time is at or past the scheduled upgrade time.
    pub fn time_for_upgrade(&self, current_time: u64) -> bool {
        current_time >= self.params.upgrade_time
    }

    /// Check if an upgrade is valid for nomination.
    ///
    /// During nomination, upgrades must additionally:
    /// 1. Pass the timing gate (current time >= upgrade time)
    /// 2. Match the node's configured upgrade parameters exactly
    ///
    /// Parity: stellar-core `Upgrades::isValidForNomination` (Upgrades.cpp:639-683)
    pub fn is_valid_for_nomination(&self, upgrade: &LedgerUpgrade, close_time: u64) -> bool {
        if !self.time_for_upgrade(close_time) {
            return false;
        }

        match upgrade {
            LedgerUpgrade::Version(new_version) => self
                .params
                .protocol_version
                .map_or(false, |v| v == *new_version),
            LedgerUpgrade::BaseFee(fee) => self.params.base_fee.map_or(false, |f| f == *fee),
            LedgerUpgrade::MaxTxSetSize(size) => {
                self.params.max_tx_set_size.map_or(false, |s| s == *size)
            }
            LedgerUpgrade::BaseReserve(reserve) => {
                self.params.base_reserve.map_or(false, |r| r == *reserve)
            }
            LedgerUpgrade::Flags(flags) => self.params.flags.map_or(false, |f| f == *flags),
            LedgerUpgrade::Config(_key) => {
                // Config upgrades require the node to have a config upgrade set key
                // configured. Full consistency check (isConsistentWith) requires ledger
                // state access which we don't have here. For now, just verify the node
                // has a config upgrade key configured.
                self.params.config_upgrade_set_key.is_some()
            }
            LedgerUpgrade::MaxSorobanTxSetSize(size) => self
                .params
                .max_soroban_tx_set_size
                .map_or(false, |s| s == *size),
        }
    }

    /// Create upgrade proposals for the given ledger state.
    ///
    /// Returns a list of LedgerUpgrade XDR objects for parameters that
    /// differ from the current values and should be upgraded.
    pub fn create_upgrades_for(&self, state: &CurrentLedgerState) -> Vec<LedgerUpgrade> {
        let close_time = state.close_time;
        let current_version = state.protocol_version;
        let current_base_fee = state.base_fee;
        let current_max_tx_set_size = state.max_tx_set_size;
        let current_base_reserve = state.base_reserve;
        let current_flags = state.flags;
        let current_max_soroban_tx_set_size = state.max_soroban_tx_set_size;
        let mut result = Vec::new();

        if !self.time_for_upgrade(close_time) {
            return result;
        }

        if let Some(version) = self.params.protocol_version {
            if current_version != version {
                result.push(LedgerUpgrade::Version(version));
            }
        }

        if let Some(fee) = self.params.base_fee {
            if current_base_fee != fee {
                result.push(LedgerUpgrade::BaseFee(fee));
            }
        }

        if let Some(size) = self.params.max_tx_set_size {
            if current_max_tx_set_size != size {
                result.push(LedgerUpgrade::MaxTxSetSize(size));
            }
        }

        if let Some(reserve) = self.params.base_reserve {
            if current_base_reserve != reserve {
                result.push(LedgerUpgrade::BaseReserve(reserve));
            }
        }

        if let Some(flags) = self.params.flags {
            if current_flags != flags {
                result.push(LedgerUpgrade::Flags(flags));
            }
        }

        // Only propose Soroban upgrades if we have Soroban enabled (protocol 20+)
        if let (Some(target_size), Some(current_size)) = (
            self.params.max_soroban_tx_set_size,
            current_max_soroban_tx_set_size,
        ) {
            if current_size != target_size {
                result.push(LedgerUpgrade::MaxSorobanTxSetSize(target_size));
            }
        }

        // Parity: config upgrade proposal. In upstream, this validates via
        // ConfigUpgradeSetFrame::makeFromKey() against ledger state. Here we
        // emit the upgrade if the key is configured, and validation happens
        // when the upgrade is applied.
        if let Some(ref key_json) = self.params.config_upgrade_set_key {
            if let Ok(key) = key_json.to_xdr() {
                result.push(LedgerUpgrade::Config(key));
            }
        }

        result
    }

    /// Remove upgrades that have been applied.
    ///
    /// Given a list of applied upgrades, removes matching parameters from
    /// the scheduled upgrades. Also removes all upgrades if they have expired.
    ///
    /// # Returns
    ///
    /// A tuple of (updated parameters, whether any changes were made).
    pub fn remove_upgrades(
        &self,
        applied_upgrades: &[UpgradeType],
        close_time: u64,
    ) -> (UpgradeParameters, bool) {
        let mut result = self.params.clone();
        let mut updated = false;

        // If upgrades have expired, remove all
        if result.is_expired(close_time) {
            let had_any = result.has_any_upgrade();
            result.protocol_version = None;
            result.base_fee = None;
            result.max_tx_set_size = None;
            result.base_reserve = None;
            result.flags = None;
            result.max_soroban_tx_set_size = None;
            result.config_upgrade_set_key = None;
            result.nomination_timeout_limit = None;
            result.expiration_minutes = None;
            return (result, had_any);
        }

        // Remove individual applied upgrades
        for upgrade_bytes in applied_upgrades {
            let Ok(upgrade) =
                LedgerUpgrade::from_xdr(&upgrade_bytes.0, stellar_xdr::curr::Limits::none())
            else {
                continue;
            };

            match upgrade {
                LedgerUpgrade::Version(v) => {
                    if result.protocol_version == Some(v) {
                        result.protocol_version = None;
                        updated = true;
                    }
                }
                LedgerUpgrade::BaseFee(v) => {
                    if result.base_fee == Some(v) {
                        result.base_fee = None;
                        updated = true;
                    }
                }
                LedgerUpgrade::MaxTxSetSize(v) => {
                    if result.max_tx_set_size == Some(v) {
                        result.max_tx_set_size = None;
                        updated = true;
                    }
                }
                LedgerUpgrade::BaseReserve(v) => {
                    if result.base_reserve == Some(v) {
                        result.base_reserve = None;
                        updated = true;
                    }
                }
                LedgerUpgrade::Flags(v) => {
                    if result.flags == Some(v) {
                        result.flags = None;
                        updated = true;
                    }
                }
                LedgerUpgrade::MaxSorobanTxSetSize(v) => {
                    if result.max_soroban_tx_set_size == Some(v) {
                        result.max_soroban_tx_set_size = None;
                        updated = true;
                    }
                }
                LedgerUpgrade::Config(key) => {
                    if let Some(ref our_key) = result.config_upgrade_set_key {
                        if let Ok(our_xdr_key) = our_key.to_xdr() {
                            if our_xdr_key == key {
                                result.config_upgrade_set_key = None;
                                updated = true;
                            }
                        }
                    }
                }
            }
        }

        (result, updated)
    }
}

impl fmt::Display for Upgrades {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.params)
    }
}

/// Validate an upgrade for application (without ledger state).
///
/// Checks if the upgrade XDR is valid and the upgrade can be safely applied
/// based on protocol version constraints alone.
///
/// **Note**: For `LedgerUpgrade::Config`, this function only checks that the
/// protocol version is >= 20. It does NOT validate the ConfigUpgradeSet
/// against ledger state (existence, XDR validity, non-upgradeable entries).
/// For full Config upgrade validation matching stellar-core's
/// `isValidForApply`, use `ScpDriver::is_valid_upgrade_for_apply` which
/// performs ledger state lookups via `LedgerManager::get_config_upgrade_set`.
///
/// # Arguments
///
/// * `upgrade_bytes` - The serialized upgrade
/// * `current_version` - Current protocol version
/// * `max_protocol_version` - Maximum supported protocol version
///
/// # Returns
///
/// A tuple of (validity, deserialized upgrade if valid XDR).
pub fn is_valid_for_apply(
    upgrade_bytes: &UpgradeType,
    current_version: u32,
    max_protocol_version: u32,
) -> (UpgradeValidity, Option<LedgerUpgrade>) {
    let upgrade = match LedgerUpgrade::from_xdr(&upgrade_bytes.0, stellar_xdr::curr::Limits::none())
    {
        Ok(u) => u,
        Err(_) => return (UpgradeValidity::XdrInvalid, None),
    };

    let valid = match &upgrade {
        LedgerUpgrade::Version(new_version) => {
            // Only allow upgrades to supported versions, must be strictly increasing
            *new_version <= max_protocol_version && *new_version > current_version
        }
        LedgerUpgrade::BaseFee(fee) => *fee != 0,
        LedgerUpgrade::MaxTxSetSize(_) => true, // Any size allowed
        LedgerUpgrade::BaseReserve(reserve) => *reserve != 0,
        LedgerUpgrade::Flags(flags) => {
            // Flags upgrade requires protocol 18+
            // MASK_LEDGER_HEADER_FLAGS = 0x7 (bits 0-2)
            const MASK_LEDGER_HEADER_FLAGS: u32 = 0x7;
            protocol_version_starts_from(current_version, ProtocolVersion::V18)
                && (*flags & !MASK_LEDGER_HEADER_FLAGS) == 0
        }
        LedgerUpgrade::Config(_) => {
            // Config upgrade requires Soroban (protocol 20+)
            protocol_version_starts_from(current_version, ProtocolVersion::V20)
        }
        LedgerUpgrade::MaxSorobanTxSetSize(_) => {
            // Soroban tx set size requires protocol 20+
            protocol_version_starts_from(current_version, ProtocolVersion::V20)
        }
    };

    if valid {
        (UpgradeValidity::Valid, Some(upgrade))
    } else {
        (UpgradeValidity::Invalid, Some(upgrade))
    }
}

/// Format a LedgerUpgrade as a human-readable string.
pub fn upgrade_to_string(upgrade: &LedgerUpgrade) -> String {
    match upgrade {
        LedgerUpgrade::Version(v) => format!("protocolversion={}", v),
        LedgerUpgrade::BaseFee(v) => format!("basefee={}", v),
        LedgerUpgrade::MaxTxSetSize(v) => format!("maxtxsetsize={}", v),
        LedgerUpgrade::BaseReserve(v) => format!("basereserve={}", v),
        LedgerUpgrade::Flags(v) => format!("flags={}", v),
        LedgerUpgrade::Config(key) => {
            format!("configupgradesetkey={}", hex::encode(key.content_hash.0))
        }
        LedgerUpgrade::MaxSorobanTxSetSize(v) => {
            format!("maxsorobantxsetsize={}", v)
        }
    }
}

/// Get the deserialized upgrade from an UpgradeType XDR.
///
/// Returns None if the XDR cannot be deserialized.
pub fn parse_upgrade(upgrade_bytes: &UpgradeType) -> Option<LedgerUpgrade> {
    LedgerUpgrade::from_xdr(&upgrade_bytes.0, stellar_xdr::curr::Limits::none()).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_upgrade_parameters_default() {
        let params = UpgradeParameters::default();
        assert_eq!(params.upgrade_time, 0);
        assert!(params.protocol_version.is_none());
        assert!(params.base_fee.is_none());
        assert!(!params.has_any_upgrade());
    }

    #[test]
    fn test_upgrade_parameters_has_any() {
        let mut params = UpgradeParameters::default();
        assert!(!params.has_any_upgrade());

        params.protocol_version = Some(23);
        assert!(params.has_any_upgrade());
    }

    #[test]
    fn test_upgrade_parameters_expiration() {
        let mut params = UpgradeParameters::new(1000);
        assert!(!params.is_expired(1000));
        assert!(!params.is_expired(1000 + 12 * 3600 - 1)); // Just before 12h expiration
        assert!(params.is_expired(1000 + 12 * 3600 + 1)); // After 12h expiration

        // Custom expiration (in minutes) — shorter than default
        params.expiration_minutes = Some(30);
        assert!(!params.is_expired(1000 + 30 * 60 - 1)); // Just before custom expiration
        assert!(params.is_expired(1000 + 30 * 60 + 1)); // Expires with custom

        // Custom expiration — longer than default
        params.expiration_minutes = Some(60 * 24); // 24 hours in minutes
        assert!(!params.is_expired(1000 + 12 * 3600 + 1)); // Would have expired with default
        assert!(params.is_expired(1000 + 24 * 3600 + 1)); // Expires with custom
    }

    #[test]
    fn test_upgrade_parameters_json() {
        let mut params = UpgradeParameters::new(1234567890);
        params.protocol_version = Some(23);
        params.base_fee = Some(100);

        let json = params.to_json().unwrap();
        let parsed = UpgradeParameters::from_json(&json).unwrap();

        assert_eq!(parsed.upgrade_time, 1234567890);
        assert_eq!(parsed.protocol_version, Some(23));
        assert_eq!(parsed.base_fee, Some(100));
    }

    #[test]
    fn test_upgrades_time_for_upgrade() {
        let params = UpgradeParameters::new(1000);
        let upgrades = Upgrades::new(params);

        assert!(!upgrades.time_for_upgrade(999));
        assert!(upgrades.time_for_upgrade(1000));
        assert!(upgrades.time_for_upgrade(1001));
    }

    #[test]
    fn test_upgrades_set_parameters_validates_version() {
        let mut upgrades = Upgrades::default();

        let mut params = UpgradeParameters::default();
        params.protocol_version = Some(100);

        let result = upgrades.set_parameters(params, 25);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Protocol version error"));
    }

    #[test]
    fn test_create_upgrades_for() {
        let mut params = UpgradeParameters::new(1000);
        params.protocol_version = Some(24);
        params.base_fee = Some(200);
        params.max_tx_set_size = Some(500);

        let upgrades = Upgrades::new(params);

        // Before upgrade time
        let proposals = upgrades.create_upgrades_for(&CurrentLedgerState {
            close_time: 999,
            protocol_version: 23,
            base_fee: 100,
            max_tx_set_size: 1000,
            base_reserve: 10000000,
            flags: 0,
            max_soroban_tx_set_size: None,
        });
        assert!(proposals.is_empty());

        // At upgrade time, with differences
        let proposals = upgrades.create_upgrades_for(&CurrentLedgerState {
            close_time: 1000,
            protocol_version: 23,
            base_fee: 100,
            max_tx_set_size: 1000,
            base_reserve: 10000000,
            flags: 0,
            max_soroban_tx_set_size: None,
        });
        assert_eq!(proposals.len(), 3);

        // At upgrade time, with no differences
        let proposals = upgrades.create_upgrades_for(&CurrentLedgerState {
            close_time: 1000,
            protocol_version: 24,
            base_fee: 200,
            max_tx_set_size: 500,
            base_reserve: 10000000,
            flags: 0,
            max_soroban_tx_set_size: None,
        });
        assert!(proposals.is_empty());
    }

    #[test]
    fn test_is_valid_for_apply_version() {
        use stellar_xdr::curr::WriteXdr;

        let upgrade = LedgerUpgrade::Version(24);
        let bytes = UpgradeType(
            upgrade
                .to_xdr(stellar_xdr::curr::Limits::none())
                .unwrap()
                .try_into()
                .unwrap(),
        );

        // Valid: upgrading from 23 to 24, max supported is 25
        let (validity, _) = is_valid_for_apply(&bytes, 23, 25);
        assert_eq!(validity, UpgradeValidity::Valid);

        // Invalid: downgrade
        let (validity, _) = is_valid_for_apply(&bytes, 24, 25);
        assert_eq!(validity, UpgradeValidity::Invalid);

        // Invalid: exceeds max
        let (validity, _) = is_valid_for_apply(&bytes, 23, 23);
        assert_eq!(validity, UpgradeValidity::Invalid);
    }

    #[test]
    fn test_is_valid_for_apply_base_fee() {
        use stellar_xdr::curr::WriteXdr;

        // Valid fee
        let upgrade = LedgerUpgrade::BaseFee(100);
        let bytes = UpgradeType(
            upgrade
                .to_xdr(stellar_xdr::curr::Limits::none())
                .unwrap()
                .try_into()
                .unwrap(),
        );
        let (validity, _) = is_valid_for_apply(&bytes, 23, 25);
        assert_eq!(validity, UpgradeValidity::Valid);

        // Invalid: zero fee
        let upgrade = LedgerUpgrade::BaseFee(0);
        let bytes = UpgradeType(
            upgrade
                .to_xdr(stellar_xdr::curr::Limits::none())
                .unwrap()
                .try_into()
                .unwrap(),
        );
        let (validity, _) = is_valid_for_apply(&bytes, 23, 25);
        assert_eq!(validity, UpgradeValidity::Invalid);
    }

    #[test]
    fn test_is_valid_for_apply_flags() {
        use stellar_xdr::curr::WriteXdr;

        // Valid flags on protocol 18+
        let upgrade = LedgerUpgrade::Flags(0x3);
        let bytes = UpgradeType(
            upgrade
                .to_xdr(stellar_xdr::curr::Limits::none())
                .unwrap()
                .try_into()
                .unwrap(),
        );
        let (validity, _) = is_valid_for_apply(&bytes, 18, 25);
        assert_eq!(validity, UpgradeValidity::Valid);

        // Invalid: protocol too old
        let (validity, _) = is_valid_for_apply(&bytes, 17, 25);
        assert_eq!(validity, UpgradeValidity::Invalid);

        // Invalid: flags out of range
        let upgrade = LedgerUpgrade::Flags(0x100);
        let bytes = UpgradeType(
            upgrade
                .to_xdr(stellar_xdr::curr::Limits::none())
                .unwrap()
                .try_into()
                .unwrap(),
        );
        let (validity, _) = is_valid_for_apply(&bytes, 18, 25);
        assert_eq!(validity, UpgradeValidity::Invalid);
    }

    #[test]
    fn test_is_valid_for_apply_soroban() {
        use stellar_xdr::curr::WriteXdr;

        let upgrade = LedgerUpgrade::MaxSorobanTxSetSize(100);
        let bytes = UpgradeType(
            upgrade
                .to_xdr(stellar_xdr::curr::Limits::none())
                .unwrap()
                .try_into()
                .unwrap(),
        );

        // Valid on protocol 20+
        let (validity, _) = is_valid_for_apply(&bytes, 20, 25);
        assert_eq!(validity, UpgradeValidity::Valid);

        // Invalid before Soroban
        let (validity, _) = is_valid_for_apply(&bytes, 19, 25);
        assert_eq!(validity, UpgradeValidity::Invalid);
    }

    #[test]
    fn test_upgrade_to_string() {
        assert_eq!(
            upgrade_to_string(&LedgerUpgrade::Version(24)),
            "protocolversion=24"
        );
        assert_eq!(
            upgrade_to_string(&LedgerUpgrade::BaseFee(100)),
            "basefee=100"
        );
        assert_eq!(
            upgrade_to_string(&LedgerUpgrade::MaxTxSetSize(500)),
            "maxtxsetsize=500"
        );
    }

    #[test]
    fn test_remove_upgrades_expiration() {
        let mut params = UpgradeParameters::new(1000);
        params.protocol_version = Some(24);
        params.base_fee = Some(200);

        let upgrades = Upgrades::new(params);

        // After expiration (12 hours = 43200 seconds)
        let (new_params, updated) = upgrades.remove_upgrades(&[], 1000 + 12 * 3600 + 1);
        assert!(updated);
        assert!(!new_params.has_any_upgrade());
    }

    #[test]
    fn test_remove_upgrades_applied() {
        use stellar_xdr::curr::WriteXdr;

        let mut params = UpgradeParameters::new(1000);
        params.protocol_version = Some(24);
        params.base_fee = Some(200);

        let upgrades = Upgrades::new(params);

        // Apply protocol version upgrade
        let applied_upgrade = LedgerUpgrade::Version(24);
        let applied_bytes = UpgradeType(
            applied_upgrade
                .to_xdr(stellar_xdr::curr::Limits::none())
                .unwrap()
                .try_into()
                .unwrap(),
        );

        let (new_params, updated) = upgrades.remove_upgrades(&[applied_bytes], 1001);
        assert!(updated);
        assert!(new_params.protocol_version.is_none());
        assert_eq!(new_params.base_fee, Some(200)); // Still pending
    }

    #[test]
    fn test_parse_upgrade() {
        use stellar_xdr::curr::WriteXdr;

        let upgrade = LedgerUpgrade::Version(24);
        let bytes = UpgradeType(
            upgrade
                .to_xdr(stellar_xdr::curr::Limits::none())
                .unwrap()
                .try_into()
                .unwrap(),
        );

        let parsed = parse_upgrade(&bytes);
        assert!(matches!(parsed, Some(LedgerUpgrade::Version(24))));
    }

    #[test]
    fn test_config_upgrade_set_key_json() {
        let key = ConfigUpgradeSetKey {
            contract_id: stellar_xdr::curr::ContractId(stellar_xdr::curr::Hash([1u8; 32])),
            content_hash: stellar_xdr::curr::Hash([2u8; 32]),
        };

        let json = ConfigUpgradeSetKeyJson::from_xdr(&key);
        let roundtrip = json.to_xdr().unwrap();

        assert_eq!(key.contract_id, roundtrip.contract_id);
        assert_eq!(key.content_hash, roundtrip.content_hash);
    }

    // =========================================================================
    // Phase 3B: Config upgrade proposal
    // =========================================================================

    #[test]
    fn test_create_upgrades_for_config_upgrade() {
        use base64::{engine::general_purpose::STANDARD, Engine};

        let contract_id = [1u8; 32];
        let content_hash = [2u8; 32];

        let mut params = UpgradeParameters::new(1000);
        params.config_upgrade_set_key = Some(ConfigUpgradeSetKeyJson {
            contract_id: STANDARD.encode(contract_id),
            content_hash: STANDARD.encode(content_hash),
        });

        let upgrades = Upgrades::new(params);

        // At upgrade time, should emit a Config upgrade
        let proposals = upgrades.create_upgrades_for(&CurrentLedgerState {
            close_time: 1000,
            protocol_version: 24,
            base_fee: 100,
            max_tx_set_size: 1000,
            base_reserve: 10000000,
            flags: 0,
            max_soroban_tx_set_size: None,
        });
        assert_eq!(proposals.len(), 1);
        match &proposals[0] {
            LedgerUpgrade::Config(key) => {
                assert_eq!(key.contract_id.0 .0, contract_id);
                assert_eq!(key.content_hash.0, content_hash);
            }
            other => panic!("Expected Config upgrade, got {:?}", other),
        }
    }

    #[test]
    fn test_create_upgrades_for_config_upgrade_with_other_upgrades() {
        use base64::{engine::general_purpose::STANDARD, Engine};

        let mut params = UpgradeParameters::new(1000);
        params.protocol_version = Some(25);
        params.base_fee = Some(200);
        params.config_upgrade_set_key = Some(ConfigUpgradeSetKeyJson {
            contract_id: STANDARD.encode([3u8; 32]),
            content_hash: STANDARD.encode([4u8; 32]),
        });

        let upgrades = Upgrades::new(params);

        let proposals = upgrades.create_upgrades_for(&CurrentLedgerState {
            close_time: 1000,
            protocol_version: 24,
            base_fee: 100,
            max_tx_set_size: 1000,
            base_reserve: 10000000,
            flags: 0,
            max_soroban_tx_set_size: None,
        });
        // Should have: Version, BaseFee, Config = 3 upgrades
        assert_eq!(proposals.len(), 3);
        assert!(matches!(proposals[0], LedgerUpgrade::Version(25)));
        assert!(matches!(proposals[1], LedgerUpgrade::BaseFee(200)));
        assert!(matches!(proposals[2], LedgerUpgrade::Config(_)));
    }

    #[test]
    fn test_create_upgrades_for_config_upgrade_bad_key() {
        // Invalid base64 key should be silently skipped
        let mut params = UpgradeParameters::new(1000);
        params.config_upgrade_set_key = Some(ConfigUpgradeSetKeyJson {
            contract_id: "not-valid-base64!!!".to_string(),
            content_hash: "also-bad".to_string(),
        });

        let upgrades = Upgrades::new(params);
        let proposals = upgrades.create_upgrades_for(&CurrentLedgerState {
            close_time: 1000,
            protocol_version: 24,
            base_fee: 100,
            max_tx_set_size: 1000,
            base_reserve: 10000000,
            flags: 0,
            max_soroban_tx_set_size: None,
        });
        assert!(
            proposals.is_empty(),
            "Bad config key should produce no upgrade"
        );
    }

    #[test]
    fn test_create_upgrades_for_config_upgrade_before_time() {
        use base64::{engine::general_purpose::STANDARD, Engine};

        let mut params = UpgradeParameters::new(2000);
        params.config_upgrade_set_key = Some(ConfigUpgradeSetKeyJson {
            contract_id: STANDARD.encode([1u8; 32]),
            content_hash: STANDARD.encode([2u8; 32]),
        });

        let upgrades = Upgrades::new(params);

        // Before upgrade time, should emit nothing
        let proposals = upgrades.create_upgrades_for(&CurrentLedgerState {
            close_time: 1000,
            protocol_version: 24,
            base_fee: 100,
            max_tx_set_size: 1000,
            base_reserve: 10000000,
            flags: 0,
            max_soroban_tx_set_size: None,
        });
        assert!(proposals.is_empty());
    }
}
