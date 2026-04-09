//! Soroban configuration upgrade handling.
//!
//! This module implements the ConfigUpgradeSetFrame from stellar-core.
//! It handles loading, validating, and applying Soroban network configuration
//! upgrades that are stored in CONTRACT_DATA ledger entries.
//!
//! # Overview
//!
//! Configuration upgrades allow the network to change Soroban parameters
//! (like compute limits, fees, etc.) through consensus. The upgrade data is
//! stored in a temporary CONTRACT_DATA entry, and validators propose upgrading
//! to a specific configuration by referencing its hash.
//!
//! # Protocol
//!
//! 1. A ConfigUpgradeSet is uploaded to the network as CONTRACT_DATA
//! 2. Validators schedule an upgrade by specifying the ConfigUpgradeSetKey
//! 3. When the upgrade time arrives, the ConfigUpgradeSet is loaded and validated
//! 4. If valid, the CONFIG_SETTING entries are updated in the ledger

use henyey_common::protocol::{
    protocol_version_is_before, protocol_version_starts_from, ProtocolVersion,
    PARALLEL_SOROBAN_PHASE_PROTOCOL_VERSION,
};
use henyey_common::Hash256;
use sha2::{Digest, Sha256};
use std::sync::Arc;
use stellar_xdr::curr::{
    ConfigSettingEntry, ConfigSettingId, ConfigUpgradeSet, ConfigUpgradeSetKey,
    ContractDataDurability, EncodedLedgerKey, FreezeBypassTxs, FrozenLedgerKeys,
    FrozenLedgerKeysDelta, Hash, LedgerEntry, LedgerEntryData, LedgerEntryExt, LedgerKey,
    LedgerKeyContractData, Limits, ReadXdr, ScAddress, ScVal, TrustLineAsset, WriteXdr,
};
use tracing::{debug, info, warn};

use crate::error::LedgerError;
use crate::ltx::LedgerTxn;

/// Validity of a configuration upgrade.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigUpgradeValidity {
    /// The upgrade is valid and can be applied.
    Valid,
    /// The upgrade XDR is invalid (bad hash, unsorted, duplicates).
    XdrInvalid,
    /// The upgrade is invalid (violates constraints or non-upgradeable).
    Invalid,
}

/// Minimum values for Soroban network configuration.
///
/// These match the stellar-core MinimumSorobanNetworkConfig values from NetworkConfig.h.
/// These are intentionally low floor values that allow the network flexibility
/// to configure settings via upgrades. They are NOT the initial/production
/// values.
pub mod min_config {
    /// Minimum max contract size.
    pub const MAX_CONTRACT_SIZE: u32 = 2_000;
    /// Minimum max contract data key size.
    pub const MAX_CONTRACT_DATA_KEY_SIZE_BYTES: u32 = 200;
    /// Minimum max contract data entry size.
    pub const MAX_CONTRACT_DATA_ENTRY_SIZE_BYTES: u32 = 2_000;
    /// Minimum tx max size bytes.
    pub const TX_MAX_SIZE_BYTES: u32 = 10_000;
    /// Minimum tx max instructions.
    pub const TX_MAX_INSTRUCTIONS: u32 = 2_500_000;
    /// Minimum memory limit.
    pub const MEMORY_LIMIT: u32 = 2_000_000;
    /// Minimum tx max read ledger entries.
    pub const TX_MAX_READ_LEDGER_ENTRIES: u32 = 3;
    /// Minimum tx max read bytes.
    pub const TX_MAX_READ_BYTES: u32 = 3_200;
    /// Minimum tx max write ledger entries.
    pub const TX_MAX_WRITE_LEDGER_ENTRIES: u32 = 2;
    /// Minimum tx max write bytes.
    pub const TX_MAX_WRITE_BYTES: u32 = 3_200;
    /// Minimum tx max contract events size.
    pub const TX_MAX_CONTRACT_EVENTS_SIZE_BYTES: u32 = 200;
    /// Minimum maximum entry TTL.
    pub const MAXIMUM_ENTRY_LIFETIME: u32 = 1_054_080;
    /// Minimum temporary entry TTL.
    pub const MINIMUM_TEMP_ENTRY_LIFETIME: u32 = 16;
    /// Minimum persistent entry TTL.
    pub const MINIMUM_PERSISTENT_ENTRY_LIFETIME: u32 = 10;
    /// Minimum max entries to archive.
    pub const MAX_ENTRIES_TO_ARCHIVE: u32 = 0;
    /// Minimum bucket list size window sample size.
    pub const BUCKETLIST_SIZE_WINDOW_SAMPLE_SIZE: u32 = 1;
    /// Minimum eviction scan size.
    pub const EVICTION_SCAN_SIZE: u32 = 0;
    /// Minimum starting eviction level.
    pub const STARTING_EVICTION_LEVEL: u32 = 1;
    /// Minimum bucket list window sample period.
    pub const BUCKETLIST_WINDOW_SAMPLE_PERIOD: u32 = 1;
    /// Minimum ledger target close time.
    pub const LEDGER_TARGET_CLOSE_TIME_MILLISECONDS: u32 = 4_000;
    /// Minimum nomination timeout initial.
    pub const NOMINATION_TIMEOUT_INITIAL_MILLISECONDS: u32 = 750;
    /// Minimum nomination timeout increment.
    pub const NOMINATION_TIMEOUT_INCREMENT_MILLISECONDS: u32 = 750;
    /// Minimum ballot timeout initial.
    pub const BALLOT_TIMEOUT_INITIAL_MILLISECONDS: u32 = 750;
    /// Minimum ballot timeout increment.
    pub const BALLOT_TIMEOUT_INCREMENT_MILLISECONDS: u32 = 750;
}

/// Maximum values for Soroban network configuration.
///
/// These match the stellar-core MaximumSorobanNetworkConfig values from NetworkConfig.h.
pub mod max_config {
    /// Maximum ledger target close time.
    pub const LEDGER_TARGET_CLOSE_TIME_MILLISECONDS: u32 = 5_000;
    /// Maximum nomination timeout initial.
    pub const NOMINATION_TIMEOUT_INITIAL_MILLISECONDS: u32 = 2_500;
    /// Maximum nomination timeout increment.
    pub const NOMINATION_TIMEOUT_INCREMENT_MILLISECONDS: u32 = 2_000;
    /// Maximum ballot timeout initial.
    pub const BALLOT_TIMEOUT_INITIAL_MILLISECONDS: u32 = 2_500;
    /// Maximum ballot timeout increment.
    pub const BALLOT_TIMEOUT_INCREMENT_MILLISECONDS: u32 = 2_000;
}

/// A frame for a ConfigUpgradeSet stored in the ledger.
///
/// This struct wraps a ConfigUpgradeSet and provides methods for validation
/// and application. It's loaded from a CONTRACT_DATA entry using a
/// ConfigUpgradeSetKey.
#[derive(Debug, Clone)]
pub struct ConfigUpgradeSetFrame {
    /// The upgrade set data.
    config_upgrade_set: ConfigUpgradeSet,
    /// Whether the XDR is valid (hash matches, sorted, no duplicates).
    valid_xdr: bool,
    /// The ledger version when this was loaded.
    ledger_version: u32,
}

impl ConfigUpgradeSetFrame {
    /// Returns the protocol version this frame was constructed with.
    pub fn ledger_version(&self) -> u32 {
        self.ledger_version
    }

    /// Load a ConfigUpgradeSet from the ledger.
    ///
    /// Returns None if:
    /// - The CONTRACT_DATA entry doesn't exist
    /// - The entry's TTL has expired
    /// - The entry is not TEMPORARY durability
    /// - The value is not SCV_BYTES
    /// - The XDR cannot be decoded
    pub fn make_from_key(
        ltx: &LedgerTxn,
        key: &ConfigUpgradeSetKey,
        closing_ledger_seq: u32,
        protocol_version: u32,
    ) -> Option<Arc<Self>> {
        let lk = Self::get_ledger_key(key);

        // Load the CONTRACT_DATA entry
        let entry = ltx.get_entry(&lk).ok()??;

        // Check TTL (entry must be live)
        let ttl_key = Self::get_ttl_key(&lk);
        let ttl_entry = ltx.get_entry(&ttl_key).ok()??;
        if !Self::is_live(&ttl_entry, closing_ledger_seq) {
            debug!(
                hash = format!("{:02x?}", &key.content_hash.0[..8]),
                "ConfigUpgradeSet TTL expired"
            );
            return None;
        }

        // Extract CONTRACT_DATA
        let contract_data = match &entry.data {
            stellar_xdr::curr::LedgerEntryData::ContractData(cd) => cd,
            _ => return None,
        };

        // Must be TEMPORARY durability
        if contract_data.durability != ContractDataDurability::Temporary {
            debug!("ConfigUpgradeSet must have TEMPORARY durability");
            return None;
        }

        // Value must be SCV_BYTES
        let bytes = match &contract_data.val {
            ScVal::Bytes(b) => b.0.as_slice(),
            _ => {
                debug!("ConfigUpgradeSet value must be SCV_BYTES");
                return None;
            }
        };

        // Decode the ConfigUpgradeSet
        let upgrade_set = match ConfigUpgradeSet::from_xdr(bytes, Limits::none()) {
            Ok(set) => set,
            Err(e) => {
                debug!(
                    hash = format!("{:02x?}", &key.content_hash.0[..8]),
                    error = %e,
                    "Failed to decode ConfigUpgradeSet"
                );
                return None;
            }
        };

        // Use the post-upgrade protocol version passed by the caller,
        // not snapshot.header().ledger_version which may be stale when
        // Version(N) and Config upgrades occur in the same ledger (#1088).
        let frame = Self {
            valid_xdr: Self::is_valid_xdr_static(&upgrade_set, key),
            config_upgrade_set: upgrade_set,
            ledger_version: protocol_version,
        };

        Some(Arc::new(frame))
    }

    /// Construct the LedgerKey for a ConfigUpgradeSet.
    ///
    /// The upgrade set is stored in a CONTRACT_DATA entry with:
    /// - Contract address: the contract_id from the key
    /// - Key: SCV_BYTES containing the content_hash
    /// - Durability: TEMPORARY
    pub fn get_ledger_key(upgrade_key: &ConfigUpgradeSetKey) -> LedgerKey {
        let key_val = ScVal::Bytes(
            upgrade_key
                .content_hash
                .0
                .to_vec()
                .try_into()
                .expect("content hash is 32 bytes"),
        );

        LedgerKey::ContractData(LedgerKeyContractData {
            contract: ScAddress::Contract(upgrade_key.contract_id.clone()),
            key: key_val,
            durability: ContractDataDurability::Temporary,
        })
    }

    /// Get the TTL key for a CONTRACT_DATA entry.
    fn get_ttl_key(data_key: &LedgerKey) -> LedgerKey {
        LedgerKey::Ttl(stellar_xdr::curr::LedgerKeyTtl {
            key_hash: Hash256::hash_xdr(data_key)
                .map(|h| Hash(h.0))
                .unwrap_or(Hash([0u8; 32])),
        })
    }

    /// Check if a TTL entry indicates the data is live.
    fn is_live(ttl_entry: &LedgerEntry, current_ledger: u32) -> bool {
        match &ttl_entry.data {
            stellar_xdr::curr::LedgerEntryData::Ttl(ttl) => {
                ttl.live_until_ledger_seq >= current_ledger
            }
            _ => false,
        }
    }

    /// Get the ConfigUpgradeSet XDR.
    pub fn to_xdr(&self) -> &ConfigUpgradeSet {
        &self.config_upgrade_set
    }

    /// Validate the upgrade for application.
    ///
    /// Returns Valid if:
    /// - XDR is valid (hash matches, sorted, no duplicates)
    /// - All entries pass validation constraints
    /// - No non-upgradeable entries are included
    pub fn is_valid_for_apply(&self) -> ConfigUpgradeValidity {
        if !self.valid_xdr {
            return ConfigUpgradeValidity::XdrInvalid;
        }

        for entry in self.config_upgrade_set.updated_entry.iter() {
            if !Self::is_valid_config_setting_entry(entry, self.ledger_version) {
                warn!(
                    id = ?entry.discriminant(),
                    "Config setting entry fails validation"
                );
                return ConfigUpgradeValidity::Invalid;
            }

            if Self::is_non_upgradeable(entry.discriminant()) {
                warn!(
                    id = ?entry.discriminant(),
                    "Config setting entry is not upgradeable"
                );
                return ConfigUpgradeValidity::Invalid;
            }
        }

        ConfigUpgradeValidity::Valid
    }

    /// Apply the configuration upgrades to the ledger.
    ///
    /// Updates all CONFIG_SETTING entries in the upgrade set. Also handles
    /// secondary effects that stellar-core performs during config upgrade:
    /// - Resizing the LiveSorobanStateSizeWindow when sample size changes
    ///   (parity: Upgrades.cpp:1443 `maybeUpdateSorobanStateSizeWindowSize`)
    ///
    /// Returns a tuple of:
    /// - `state_archival_changed`: Whether StateArchival settings were modified
    /// - `memory_cost_params_changed`: Whether ContractCostParamsMemoryBytes was upgraded
    ///
    /// These flags are used by the caller to trigger special handling:
    /// - StateArchival changes may affect the state size window
    /// - Memory cost params changes require recomputing in-memory state sizes
    ///   and overwriting all window entries
    ///   (parity: Upgrades.cpp:1449 `handleUpgradeAffectingSorobanInMemoryStateSize`)
    // SECURITY: config entries validated during upgrade proposal phase before reaching apply
    pub fn apply_to(
        &self,
        ltx: &mut LedgerTxn,
    ) -> Result<(bool, bool, stellar_xdr::curr::LedgerEntryChanges), LedgerError> {
        use stellar_xdr::curr::LedgerEntryChange;

        let mut state_archival_changed = false;
        let mut memory_cost_params_changed = false;
        let mut window_sample_size_changed = false;
        let mut changes: Vec<LedgerEntryChange> = Vec::new();

        for new_entry in self.config_upgrade_set.updated_entry.iter() {
            let setting_id = new_entry.discriminant();

            // Delta entries modify existing base entries instead of replacing them.
            // They must be handled before we try to load a config setting by ID,
            // since delta IDs don't have their own stored config setting entries.
            // Parity: Upgrades.cpp:1458-1517
            if setting_id == ConfigSettingId::FrozenLedgerKeysDelta {
                self.apply_frozen_keys_delta(new_entry, ltx, &mut changes)?;
                continue;
            }
            if setting_id == ConfigSettingId::FreezeBypassTxsDelta {
                self.apply_freeze_bypass_delta(new_entry, ltx, &mut changes)?;
                continue;
            }

            // Construct the ledger key for this config setting
            let key = LedgerKey::ConfigSetting(stellar_xdr::curr::LedgerKeyConfigSetting {
                config_setting_id: setting_id,
            });

            // Load the current entry from the ledger (reads through the full
            // LedgerTxn chain: current → committed → snapshot)
            let current_entry = ltx.get_entry(&key).map_err(|e| {
                LedgerError::Internal(format!(
                    "Failed to load config setting {:?}: {}",
                    setting_id, e
                ))
            })?;

            let previous = match current_entry {
                Some(entry) => entry,
                None => {
                    // Entry doesn't exist - this shouldn't happen for valid upgrades
                    // as all config settings should exist after protocol 20
                    warn!(
                        setting_id = ?setting_id,
                        "Config setting entry not found during upgrade"
                    );
                    continue;
                }
            };

            // Track special changes BEFORE recording the update
            // Parity: Upgrades.cpp:1426-1437
            if matches!(setting_id, ConfigSettingId::StateArchival) {
                state_archival_changed = true;
                // Check if liveSorobanStateSizeWindowSampleSize changed
                if let (
                    LedgerEntryData::ConfigSetting(ConfigSettingEntry::StateArchival(old)),
                    ConfigSettingEntry::StateArchival(new),
                ) = (&previous.data, new_entry)
                {
                    if old.live_soroban_state_size_window_sample_size
                        != new.live_soroban_state_size_window_sample_size
                    {
                        window_sample_size_changed = true;
                    }
                }
            }

            if matches!(setting_id, ConfigSettingId::ContractCostParamsMemoryBytes) {
                memory_cost_params_changed = true;
            }

            // Create the new entry
            let new_ledger_entry = LedgerEntry {
                last_modified_ledger_seq: ltx.ledger_seq(),
                data: LedgerEntryData::ConfigSetting(new_entry.clone()),
                ext: LedgerEntryExt::V0,
            };

            // Capture before/after for upgrade meta
            changes.push(LedgerEntryChange::State(previous.clone()));
            changes.push(LedgerEntryChange::Updated(new_ledger_entry.clone()));

            // Record the update
            ltx.record_update(previous.clone(), new_ledger_entry)?;

            info!(
                setting_id = ?setting_id,
                "Applied config upgrade"
            );
        }

        // Parity: Upgrades.cpp:1443-1446
        // If the state size window sample size changed, resize the window.
        // This must happen AFTER all config settings are applied but BEFORE
        // entries are extracted for the bucket list.
        if window_sample_size_changed {
            self.maybe_update_state_size_window(ltx)?;
        }

        let entry_changes = stellar_xdr::curr::LedgerEntryChanges(
            changes
                .try_into()
                .expect("config upgrade entry changes must fit XDR bounds"),
        );

        Ok((
            state_archival_changed,
            memory_cost_params_changed,
            entry_changes,
        ))
    }

    /// Apply a FrozenLedgerKeysDelta to the base FrozenLedgerKeys config setting.
    ///
    /// Loads the current frozen keys, adds keys_to_freeze, removes keys_to_unfreeze,
    /// and writes back the modified entry. Uses BTreeSet for deterministic ordering
    /// matching stellar-core's std::set.
    ///
    /// Parity: Upgrades.cpp:1458-1486
    fn apply_frozen_keys_delta(
        &self,
        delta_entry: &ConfigSettingEntry,
        ltx: &mut LedgerTxn,
        changes: &mut Vec<stellar_xdr::curr::LedgerEntryChange>,
    ) -> Result<(), LedgerError> {
        use stellar_xdr::curr::LedgerEntryChange;

        // Load the base CONFIG_SETTING_FROZEN_LEDGER_KEYS entry
        let key = LedgerKey::ConfigSetting(stellar_xdr::curr::LedgerKeyConfigSetting {
            config_setting_id: ConfigSettingId::FrozenLedgerKeys,
        });
        let previous = ltx
            .get_entry(&key)
            .map_err(|e| {
                LedgerError::Internal(format!(
                    "Failed to load FrozenLedgerKeys config setting: {}",
                    e
                ))
            })?
            .ok_or_else(|| {
                LedgerError::Internal("FrozenLedgerKeys config setting not found".into())
            })?;

        // Extract the current keys
        let current_keys = match &previous.data {
            LedgerEntryData::ConfigSetting(ConfigSettingEntry::FrozenLedgerKeys(fk)) => &fk.keys,
            _ => {
                return Err(LedgerError::Internal(
                    "Unexpected entry type for FrozenLedgerKeys".into(),
                ))
            }
        };

        // Extract the delta
        let delta_data = match delta_entry {
            ConfigSettingEntry::FrozenLedgerKeysDelta(d) => d,
            _ => {
                return Err(LedgerError::Internal(
                    "Expected FrozenLedgerKeysDelta entry".into(),
                ))
            }
        };

        // Apply: use BTreeSet for deterministic ordering (matching stellar-core std::set)
        let mut existing: std::collections::BTreeSet<Vec<u8>> =
            current_keys.iter().map(|k| k.0.to_vec()).collect();
        for k in delta_data.keys_to_freeze.iter() {
            existing.insert(k.0.to_vec());
        }
        for k in delta_data.keys_to_unfreeze.iter() {
            existing.remove(k.0.as_slice());
        }

        // Convert back to XDR types
        let new_keys: Vec<EncodedLedgerKey> = existing
            .into_iter()
            .map(|v| EncodedLedgerKey(v.try_into().expect("key bytes must fit BytesM")))
            .collect();
        let frozen_keys = FrozenLedgerKeys {
            keys: new_keys
                .try_into()
                .expect("frozen keys must fit XDR bounds"),
        };

        let new_ledger_entry = LedgerEntry {
            last_modified_ledger_seq: ltx.ledger_seq(),
            data: LedgerEntryData::ConfigSetting(ConfigSettingEntry::FrozenLedgerKeys(frozen_keys)),
            ext: LedgerEntryExt::V0,
        };

        // Capture before/after for upgrade meta
        changes.push(LedgerEntryChange::State(previous.clone()));
        changes.push(LedgerEntryChange::Updated(new_ledger_entry.clone()));

        ltx.record_update(previous, new_ledger_entry)?;

        info!("Applied frozen ledger keys delta");
        Ok(())
    }

    /// Apply a FreezeBypassTxsDelta to the base FreezeBypassTxs config setting.
    ///
    /// Loads the current bypass tx hashes, adds add_txs, removes remove_txs,
    /// and writes back the modified entry. Uses BTreeSet for deterministic ordering
    /// matching stellar-core's std::set.
    ///
    /// Parity: Upgrades.cpp:1488-1517
    fn apply_freeze_bypass_delta(
        &self,
        delta_entry: &ConfigSettingEntry,
        ltx: &mut LedgerTxn,
        changes: &mut Vec<stellar_xdr::curr::LedgerEntryChange>,
    ) -> Result<(), LedgerError> {
        use stellar_xdr::curr::LedgerEntryChange;

        // Load the base CONFIG_SETTING_FREEZE_BYPASS_TXS entry
        let key = LedgerKey::ConfigSetting(stellar_xdr::curr::LedgerKeyConfigSetting {
            config_setting_id: ConfigSettingId::FreezeBypassTxs,
        });
        let previous = ltx
            .get_entry(&key)
            .map_err(|e| {
                LedgerError::Internal(format!(
                    "Failed to load FreezeBypassTxs config setting: {}",
                    e
                ))
            })?
            .ok_or_else(|| {
                LedgerError::Internal("FreezeBypassTxs config setting not found".into())
            })?;

        // Extract the current tx hashes
        let current_hashes = match &previous.data {
            LedgerEntryData::ConfigSetting(ConfigSettingEntry::FreezeBypassTxs(bt)) => {
                &bt.tx_hashes
            }
            _ => {
                return Err(LedgerError::Internal(
                    "Unexpected entry type for FreezeBypassTxs".into(),
                ))
            }
        };

        // Extract the delta
        let delta_data = match delta_entry {
            ConfigSettingEntry::FreezeBypassTxsDelta(d) => d,
            _ => {
                return Err(LedgerError::Internal(
                    "Expected FreezeBypassTxsDelta entry".into(),
                ))
            }
        };

        // Apply: use BTreeSet for deterministic ordering (matching stellar-core std::set<Hash>)
        let mut existing: std::collections::BTreeSet<Hash> =
            current_hashes.iter().cloned().collect();
        for h in delta_data.add_txs.iter() {
            existing.insert(h.clone());
        }
        for h in delta_data.remove_txs.iter() {
            existing.remove(h);
        }

        // Convert back to XDR types
        let new_hashes: Vec<Hash> = existing.into_iter().collect();
        let bypass_txs = FreezeBypassTxs {
            tx_hashes: new_hashes
                .try_into()
                .expect("bypass tx hashes must fit XDR bounds"),
        };

        let new_ledger_entry = LedgerEntry {
            last_modified_ledger_seq: ltx.ledger_seq(),
            data: LedgerEntryData::ConfigSetting(ConfigSettingEntry::FreezeBypassTxs(bypass_txs)),
            ext: LedgerEntryExt::V0,
        };

        // Capture before/after for upgrade meta
        changes.push(LedgerEntryChange::State(previous.clone()));
        changes.push(LedgerEntryChange::Updated(new_ledger_entry.clone()));

        ltx.record_update(previous, new_ledger_entry)?;

        info!("Applied freeze bypass txs delta");
        Ok(())
    }

    /// Resize the LiveSorobanStateSizeWindow when liveSorobanStateSizeWindowSampleSize
    /// changes via a config upgrade.
    ///
    /// Parity: NetworkConfig.cpp:2080 `maybeUpdateSorobanStateSizeWindowSize`
    // SECURITY: config entries validated during upgrade proposal phase before reaching apply
    fn maybe_update_state_size_window(&self, ltx: &mut LedgerTxn) -> Result<(), LedgerError> {
        // Get the new sample size from the upgrade set
        let new_sample_size = self
            .config_upgrade_set
            .updated_entry
            .iter()
            .find_map(|entry| {
                if let ConfigSettingEntry::StateArchival(archival) = entry {
                    Some(archival.live_soroban_state_size_window_sample_size as usize)
                } else {
                    None
                }
            });

        let new_sample_size = match new_sample_size {
            Some(size) => size,
            None => return Ok(()),
        };

        // Load the current window from the LedgerTxn (sees prior config upgrades)
        let window_key = LedgerKey::ConfigSetting(stellar_xdr::curr::LedgerKeyConfigSetting {
            config_setting_id: ConfigSettingId::LiveSorobanStateSizeWindow,
        });
        let window_entry = ltx.get_entry(&window_key).map_err(|e| {
            LedgerError::Internal(format!("Failed to load LiveSorobanStateSizeWindow: {}", e))
        })?;

        let window_entry = match window_entry {
            Some(entry) => entry,
            None => return Ok(()),
        };

        let window = match &window_entry.data {
            LedgerEntryData::ConfigSetting(ConfigSettingEntry::LiveSorobanStateSizeWindow(w)) => w,
            _ => return Ok(()),
        };

        let mut window_vec: Vec<u64> = window.iter().copied().collect();
        let curr_size = window_vec.len();

        if new_sample_size == curr_size {
            return Ok(());
        }

        if new_sample_size < curr_size {
            // Shrink: remove oldest entries from front
            window_vec.drain(0..(curr_size - new_sample_size));
        } else {
            // Grow: backfill with oldest value at front
            let oldest = window_vec.first().copied().unwrap_or(0);
            let insert_count = new_sample_size - curr_size;
            for _ in 0..insert_count {
                window_vec.insert(0, oldest);
            }
        }

        info!(
            old_size = curr_size,
            new_size = new_sample_size,
            "Resized LiveSorobanStateSizeWindow due to config upgrade"
        );

        let new_window: stellar_xdr::curr::VecM<u64> = window_vec
            .try_into()
            .map_err(|_| LedgerError::Internal("Failed to convert window vec".to_string()))?;

        let new_window_entry = LedgerEntry {
            last_modified_ledger_seq: ltx.ledger_seq(),
            data: LedgerEntryData::ConfigSetting(ConfigSettingEntry::LiveSorobanStateSizeWindow(
                new_window,
            )),
            ext: LedgerEntryExt::V0,
        };

        ltx.record_update(window_entry.clone(), new_window_entry)?;

        Ok(())
    }

    // --- Validation helpers ---

    /// Validate XDR structure.
    fn is_valid_xdr_static(upgrade_set: &ConfigUpgradeSet, key: &ConfigUpgradeSetKey) -> bool {
        // Check hash matches
        let bytes = upgrade_set
            .to_xdr(Limits::none())
            .expect("ConfigUpgradeSet XDR serialization must not fail");
        let computed_hash = Sha256::digest(&bytes);
        if &computed_hash[..] != key.content_hash.0 {
            debug!(
                expected = format!("{:02x?}", &key.content_hash.0[..8]),
                computed = format!("{:02x?}", &computed_hash[..8]),
                "ConfigUpgradeSet hash mismatch"
            );
            return false;
        }

        // Check not empty
        if upgrade_set.updated_entry.is_empty() {
            debug!("ConfigUpgradeSet has no entries");
            return false;
        }

        // Check sorted by config setting ID
        let entries: Vec<_> = upgrade_set.updated_entry.iter().collect();
        for i in 1..entries.len() {
            if entries[i].discriminant() <= entries[i - 1].discriminant() {
                debug!("ConfigUpgradeSet entries not sorted or have duplicates");
                return false;
            }
        }

        true
    }

    /// Check if a config setting ID is non-upgradeable.
    pub fn is_non_upgradeable(id: ConfigSettingId) -> bool {
        // While the BucketList size window and eviction iterator are stored in a
        // ConfigSetting entry, the BucketList defines these values, they should
        // never be changed via upgrade. Frozen ledger keys and freeze bypass txs
        // are also non-upgradeable (they use delta-based upgrades instead).
        matches!(
            id,
            ConfigSettingId::LiveSorobanStateSizeWindow
                | ConfigSettingId::EvictionIterator
                | ConfigSettingId::FrozenLedgerKeys
                | ConfigSettingId::FreezeBypassTxs
        )
    }

    /// Validate cost parameters by count and sign.
    ///
    /// Matches stellar-core `SorobanNetworkConfig::isValidCostParams` from
    /// NetworkConfig.cpp:2480-2520. Checks that:
    /// 1. The parameter count matches the expected number for the protocol version
    /// 2. All constTerm and linearTerm values are non-negative
    ///
    /// Expected counts by protocol version (from ContractCostType enum):
    /// - V20 (before V21): ChaCha20DrawBytes(22) + 1 = 23
    /// - V21 (before V22): VerifyEcdsaSecp256r1Sig(44) + 1 = 45
    /// - V22-V24 (before V25): Bls12381FrInv(69) + 1 = 70
    /// - V25+: Bn254FrInv(84) + 1 = 85
    fn is_valid_cost_params(
        params: &stellar_xdr::curr::ContractCostParams,
        ledger_version: u32,
    ) -> bool {
        // Determine expected number of cost types by protocol version.
        // These values come from the ContractCostType XDR enum.
        let expected_count: usize =
            if protocol_version_is_before(ledger_version, ProtocolVersion::V21) {
                23 // ChaCha20DrawBytes(22) + 1
            } else if protocol_version_is_before(ledger_version, ProtocolVersion::V22) {
                45 // VerifyEcdsaSecp256r1Sig(44) + 1
            } else if protocol_version_is_before(ledger_version, ProtocolVersion::V25) {
                70 // Bls12381FrInv(69) + 1
            } else if protocol_version_is_before(ledger_version, ProtocolVersion::V26) {
                85 // Bn254FrInv(84) + 1
            } else {
                86 // Bn254G1Msm(85) + 1
            };

        if params.0.len() != expected_count {
            return false;
        }

        for param in params.0.iter() {
            if param.const_term < 0 || param.linear_term < 0 {
                return false;
            }
        }

        true
    }

    /// Validate a config setting entry against constraints.
    ///
    /// Matches stellar-core `SorobanNetworkConfig::isValidConfigSettingEntry` from
    /// NetworkConfig.cpp.
    fn is_valid_config_setting_entry(entry: &ConfigSettingEntry, ledger_version: u32) -> bool {
        match entry {
            ConfigSettingEntry::ContractMaxSizeBytes(v) => *v >= min_config::MAX_CONTRACT_SIZE,
            ConfigSettingEntry::ContractCostParamsCpuInstructions(params) => {
                Self::is_valid_cost_params(params, ledger_version)
            }
            ConfigSettingEntry::ContractCostParamsMemoryBytes(params) => {
                Self::is_valid_cost_params(params, ledger_version)
            }
            ConfigSettingEntry::ContractDataKeySizeBytes(v) => {
                *v >= min_config::MAX_CONTRACT_DATA_KEY_SIZE_BYTES
            }
            ConfigSettingEntry::ContractDataEntrySizeBytes(v) => {
                *v >= min_config::MAX_CONTRACT_DATA_ENTRY_SIZE_BYTES
            }
            ConfigSettingEntry::ContractExecutionLanes(_) => true,
            ConfigSettingEntry::ContractBandwidthV0(bw) => {
                bw.fee_tx_size1_kb >= 0
                    && bw.tx_max_size_bytes >= min_config::TX_MAX_SIZE_BYTES
                    && bw.ledger_max_txs_size_bytes >= bw.tx_max_size_bytes
            }
            ConfigSettingEntry::ContractComputeV0(compute) => {
                compute.fee_rate_per_instructions_increment >= 0
                    && compute.tx_max_instructions >= i64::from(min_config::TX_MAX_INSTRUCTIONS)
                    && compute.ledger_max_instructions >= compute.tx_max_instructions
                    && compute.tx_memory_limit >= min_config::MEMORY_LIMIT
            }
            ConfigSettingEntry::ContractHistoricalDataV0(hist) => hist.fee_historical1_kb >= 0,
            ConfigSettingEntry::ContractLedgerCostV0(cost) => {
                cost.tx_max_disk_read_entries >= min_config::TX_MAX_READ_LEDGER_ENTRIES
                    && cost.ledger_max_disk_read_entries >= cost.tx_max_disk_read_entries
                    && cost.tx_max_disk_read_bytes >= min_config::TX_MAX_READ_BYTES
                    && cost.ledger_max_disk_read_bytes >= cost.tx_max_disk_read_bytes
                    && cost.tx_max_write_ledger_entries >= min_config::TX_MAX_WRITE_LEDGER_ENTRIES
                    && cost.ledger_max_write_ledger_entries >= cost.tx_max_write_ledger_entries
                    && cost.tx_max_write_bytes >= min_config::TX_MAX_WRITE_BYTES
                    && cost.ledger_max_write_bytes >= cost.tx_max_write_bytes
                    && cost.fee_disk_read_ledger_entry >= 0
                    && cost.fee_write_ledger_entry >= 0
                    && cost.fee_disk_read1_kb >= 0
                    && cost.soroban_state_target_size_bytes > 0
                    && cost.rent_fee1_kb_soroban_state_size_high >= 0
                // Note: stellar-core also checks sorobanStateRentFeeGrowthFactor >= 0,
                // but the field is u32 in Rust so it's always >= 0.
            }
            ConfigSettingEntry::ContractEventsV0(events) => {
                events.tx_max_contract_events_size_bytes
                    >= min_config::TX_MAX_CONTRACT_EVENTS_SIZE_BYTES
                    && events.fee_contract_events1_kb >= 0
            }
            ConfigSettingEntry::StateArchival(archival) => {
                archival.max_entry_ttl >= min_config::MAXIMUM_ENTRY_LIFETIME
                    && archival.min_temporary_ttl >= min_config::MINIMUM_TEMP_ENTRY_LIFETIME
                    && archival.min_persistent_ttl
                        >= min_config::MINIMUM_PERSISTENT_ENTRY_LIFETIME
                    && archival.persistent_rent_rate_denominator > 0
                    && archival.temp_rent_rate_denominator > 0
                    && archival.live_soroban_state_size_window_sample_size
                        >= min_config::BUCKETLIST_SIZE_WINDOW_SAMPLE_SIZE
                    && archival.starting_eviction_scan_level
                        >= min_config::STARTING_EVICTION_LEVEL
                    && archival.starting_eviction_scan_level < 12 // kNumLevels
                    && archival.live_soroban_state_size_window_sample_period
                        >= min_config::BUCKETLIST_WINDOW_SAMPLE_PERIOD
                    && archival.max_entry_ttl > archival.min_persistent_ttl
                    && archival.max_entry_ttl > archival.min_temporary_ttl
            }
            ConfigSettingEntry::LiveSorobanStateSizeWindow(_) => true,
            ConfigSettingEntry::EvictionIterator(_) => true,
            ConfigSettingEntry::ContractParallelComputeV0(parallel) => {
                protocol_version_starts_from(
                    ledger_version,
                    PARALLEL_SOROBAN_PHASE_PROTOCOL_VERSION,
                ) && parallel.ledger_max_dependent_tx_clusters > 0
                    && parallel.ledger_max_dependent_tx_clusters < 128
            }
            ConfigSettingEntry::ContractLedgerCostExtV0(ext) => {
                protocol_version_starts_from(ledger_version, ProtocolVersion::V23)
                    && ext.tx_max_footprint_entries >= min_config::TX_MAX_READ_LEDGER_ENTRIES
                    && ext.fee_write1_kb >= 0
            }
            // CAP-77 frozen key config settings (Protocol 26+).
            // Parity: NetworkConfig.cpp:1486-1572
            ConfigSettingEntry::FrozenLedgerKeys(_) | ConfigSettingEntry::FreezeBypassTxs(_) => {
                // The base entries are always structurally valid but require V26.
                // (They cannot be directly upgraded — is_non_upgradeable blocks them.)
                protocol_version_starts_from(ledger_version, ProtocolVersion::V26)
            }
            ConfigSettingEntry::FrozenLedgerKeysDelta(delta) => {
                protocol_version_starts_from(ledger_version, ProtocolVersion::V26)
                    && Self::is_valid_frozen_keys_delta(delta)
            }
            ConfigSettingEntry::FreezeBypassTxsDelta(_) => {
                // Bypass tx delta is structurally validated only, matching upstream.
                protocol_version_starts_from(ledger_version, ProtocolVersion::V26)
            }
            ConfigSettingEntry::ScpTiming(timing) => {
                protocol_version_starts_from(ledger_version, ProtocolVersion::V23)
                    && timing.ledger_target_close_time_milliseconds
                        >= min_config::LEDGER_TARGET_CLOSE_TIME_MILLISECONDS
                    && timing.ledger_target_close_time_milliseconds
                        <= max_config::LEDGER_TARGET_CLOSE_TIME_MILLISECONDS
                    && timing.nomination_timeout_initial_milliseconds
                        >= min_config::NOMINATION_TIMEOUT_INITIAL_MILLISECONDS
                    && timing.nomination_timeout_initial_milliseconds
                        <= max_config::NOMINATION_TIMEOUT_INITIAL_MILLISECONDS
                    && timing.nomination_timeout_increment_milliseconds
                        >= min_config::NOMINATION_TIMEOUT_INCREMENT_MILLISECONDS
                    && timing.nomination_timeout_increment_milliseconds
                        <= max_config::NOMINATION_TIMEOUT_INCREMENT_MILLISECONDS
                    && timing.ballot_timeout_initial_milliseconds
                        >= min_config::BALLOT_TIMEOUT_INITIAL_MILLISECONDS
                    && timing.ballot_timeout_initial_milliseconds
                        <= max_config::BALLOT_TIMEOUT_INITIAL_MILLISECONDS
                    && timing.ballot_timeout_increment_milliseconds
                        >= min_config::BALLOT_TIMEOUT_INCREMENT_MILLISECONDS
                    && timing.ballot_timeout_increment_milliseconds
                        <= max_config::BALLOT_TIMEOUT_INCREMENT_MILLISECONDS
            }
        }
    }

    /// Validate a FrozenLedgerKeysDelta entry.
    ///
    /// Each encoded key in keys_to_freeze and keys_to_unfreeze must:
    /// - Decode as valid XDR
    /// - Be one of ACCOUNT, TRUSTLINE, CONTRACT_DATA, CONTRACT_CODE
    /// - For TRUSTLINE: not be a pool-share trustline, and not be an issuer trustline
    ///
    /// Parity: NetworkConfig.cpp:1492-1562
    fn is_valid_frozen_keys_delta(delta: &FrozenLedgerKeysDelta) -> bool {
        let validate_encoded_keys =
            |encoded_keys: &[EncodedLedgerKey], key_set_name: &str| -> bool {
                for encoded_key in encoded_keys {
                    match LedgerKey::from_xdr(encoded_key.0.as_slice(), Limits::none()) {
                        Ok(lk) => match &lk {
                            LedgerKey::Account(_)
                            | LedgerKey::ContractData(_)
                            | LedgerKey::ContractCode(_) => {}
                            LedgerKey::Trustline(tl) => {
                                // Pool-share trustlines cannot be frozen.
                                if matches!(tl.asset, TrustLineAsset::PoolShare(_)) {
                                    warn!(
                                        key_set = key_set_name,
                                        "Rejecting frozen key delta: pool-share trustline"
                                    );
                                    return false;
                                }
                                // Issuer trustlines cannot be frozen.
                                if henyey_common::asset::is_trustline_asset_issuer(
                                    &tl.account_id,
                                    &tl.asset,
                                ) {
                                    warn!(
                                        key_set = key_set_name,
                                        "Rejecting frozen key delta: issuer trustline"
                                    );
                                    return false;
                                }
                            }
                            _ => {
                                warn!(
                                    key_set = key_set_name,
                                    key_type = ?lk.discriminant(),
                                    "Rejecting frozen key delta: unsupported key type"
                                );
                                return false;
                            }
                        },
                        Err(e) => {
                            warn!(
                                key_set = key_set_name,
                                error = %e,
                                "Rejecting frozen key delta: failed to decode encoded ledger key"
                            );
                            return false;
                        }
                    }
                }
                true
            };

        validate_encoded_keys(&delta.keys_to_freeze, "keysToFreeze")
            && validate_encoded_keys(&delta.keys_to_unfreeze, "keysToUnfreeze")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use stellar_xdr::curr::ContractId;

    fn make_test_key() -> ConfigUpgradeSetKey {
        ConfigUpgradeSetKey {
            contract_id: ContractId(Hash([1u8; 32])),
            content_hash: Hash([2u8; 32]),
        }
    }

    #[test]
    fn test_get_ledger_key() {
        let key = make_test_key();
        let lk = ConfigUpgradeSetFrame::get_ledger_key(&key);

        match lk {
            LedgerKey::ContractData(cd) => {
                assert_eq!(cd.durability, ContractDataDurability::Temporary);
                match cd.contract {
                    ScAddress::Contract(cid) => {
                        assert_eq!(cid.0, key.contract_id.0);
                    }
                    _ => panic!("Expected Contract address"),
                }
                match cd.key {
                    ScVal::Bytes(b) => {
                        assert_eq!(b.0.as_slice(), &key.content_hash.0);
                    }
                    _ => panic!("Expected Bytes key"),
                }
            }
            _ => panic!("Expected ContractData key"),
        }
    }

    #[test]
    fn test_is_non_upgradeable() {
        // These 4 settings should be non-upgradeable
        assert!(ConfigUpgradeSetFrame::is_non_upgradeable(
            ConfigSettingId::LiveSorobanStateSizeWindow
        ));
        assert!(ConfigUpgradeSetFrame::is_non_upgradeable(
            ConfigSettingId::EvictionIterator
        ));
        assert!(ConfigUpgradeSetFrame::is_non_upgradeable(
            ConfigSettingId::FrozenLedgerKeys
        ));
        assert!(ConfigUpgradeSetFrame::is_non_upgradeable(
            ConfigSettingId::FreezeBypassTxs
        ));

        // Delta settings for frozen keys ARE upgradeable
        assert!(!ConfigUpgradeSetFrame::is_non_upgradeable(
            ConfigSettingId::FrozenLedgerKeysDelta
        ));
        assert!(!ConfigUpgradeSetFrame::is_non_upgradeable(
            ConfigSettingId::FreezeBypassTxsDelta
        ));

        // Other settings are upgradeable
        assert!(!ConfigUpgradeSetFrame::is_non_upgradeable(
            ConfigSettingId::ContractMaxSizeBytes
        ));
        assert!(!ConfigUpgradeSetFrame::is_non_upgradeable(
            ConfigSettingId::StateArchival
        ));
    }

    #[test]
    fn test_valid_xdr_empty_entries() {
        let key = make_test_key();
        let upgrade_set = ConfigUpgradeSet {
            updated_entry: vec![].try_into().unwrap(),
        };
        assert!(!ConfigUpgradeSetFrame::is_valid_xdr_static(
            &upgrade_set,
            &key
        ));
    }

    /// Regression test: config upgrade validation must accept values at the
    /// stellar-core MinimumSorobanNetworkConfig floor, not the higher initial/production
    /// values. Testnet ledger 427 has a ContractLedgerCostV0 upgrade with values
    /// below the (incorrect) old Rust minimums but above the real stellar-core minimums.
    #[test]
    fn test_contract_ledger_cost_v0_accepts_stellar_core_minimum_values() {
        use stellar_xdr::curr::ConfigSettingContractLedgerCostV0;

        // Values at the stellar-core minimum floor - must be accepted
        let cost_at_minimum =
            ConfigSettingEntry::ContractLedgerCostV0(ConfigSettingContractLedgerCostV0 {
                tx_max_disk_read_entries: 3, // stellar-core min: 3
                ledger_max_disk_read_entries: 3,
                tx_max_disk_read_bytes: 3200, // stellar-core min: 3200
                ledger_max_disk_read_bytes: 3200,
                tx_max_write_ledger_entries: 2, // stellar-core min: 2
                ledger_max_write_ledger_entries: 2,
                tx_max_write_bytes: 3200, // stellar-core min: 3200
                ledger_max_write_bytes: 3200,
                fee_disk_read_ledger_entry: 0,
                fee_write_ledger_entry: 0,
                fee_disk_read1_kb: 0,
                soroban_state_target_size_bytes: 1,
                rent_fee1_kb_soroban_state_size_low: 0,
                rent_fee1_kb_soroban_state_size_high: 0,
                soroban_state_rent_fee_growth_factor: 0,
            });
        assert!(
            ConfigUpgradeSetFrame::is_valid_config_setting_entry(&cost_at_minimum, 25),
            "ContractLedgerCostV0 at stellar-core minimum values must be accepted"
        );

        // Values below the stellar-core minimum - must be rejected
        let cost_below_minimum =
            ConfigSettingEntry::ContractLedgerCostV0(ConfigSettingContractLedgerCostV0 {
                tx_max_disk_read_entries: 2, // Below stellar-core min of 3
                ledger_max_disk_read_entries: 2,
                tx_max_disk_read_bytes: 3200,
                ledger_max_disk_read_bytes: 3200,
                tx_max_write_ledger_entries: 2,
                ledger_max_write_ledger_entries: 2,
                tx_max_write_bytes: 3200,
                ledger_max_write_bytes: 3200,
                fee_disk_read_ledger_entry: 0,
                fee_write_ledger_entry: 0,
                fee_disk_read1_kb: 0,
                soroban_state_target_size_bytes: 1,
                rent_fee1_kb_soroban_state_size_low: 0,
                rent_fee1_kb_soroban_state_size_high: 0,
                soroban_state_rent_fee_growth_factor: 0,
            });
        assert!(
            !ConfigUpgradeSetFrame::is_valid_config_setting_entry(&cost_below_minimum, 25),
            "ContractLedgerCostV0 below stellar-core minimum values must be rejected"
        );
    }

    /// Regression test: all config setting types must use the correct stellar-core
    /// MinimumSorobanNetworkConfig floor values, not the higher initial values.
    #[test]
    fn test_all_config_settings_accept_stellar_core_minimum_values() {
        use stellar_xdr::curr::{
            ConfigSettingContractBandwidthV0, ConfigSettingContractComputeV0,
            ConfigSettingContractEventsV0, StateArchivalSettings,
        };

        // ContractMaxSizeBytes at stellar-core min of 2000
        let entry = ConfigSettingEntry::ContractMaxSizeBytes(2000);
        assert!(
            ConfigUpgradeSetFrame::is_valid_config_setting_entry(&entry, 25),
            "ContractMaxSizeBytes=2000 must be accepted (stellar-core min)"
        );
        let entry = ConfigSettingEntry::ContractMaxSizeBytes(1999);
        assert!(
            !ConfigUpgradeSetFrame::is_valid_config_setting_entry(&entry, 25),
            "ContractMaxSizeBytes=1999 must be rejected"
        );

        // ContractDataKeySizeBytes at stellar-core min of 200
        let entry = ConfigSettingEntry::ContractDataKeySizeBytes(200);
        assert!(
            ConfigUpgradeSetFrame::is_valid_config_setting_entry(&entry, 25),
            "ContractDataKeySizeBytes=200 must be accepted (stellar-core min)"
        );

        // ContractDataEntrySizeBytes at stellar-core min of 2000
        let entry = ConfigSettingEntry::ContractDataEntrySizeBytes(2000);
        assert!(
            ConfigUpgradeSetFrame::is_valid_config_setting_entry(&entry, 25),
            "ContractDataEntrySizeBytes=2000 must be accepted (stellar-core min)"
        );

        // ContractBandwidthV0 at stellar-core min
        let entry = ConfigSettingEntry::ContractBandwidthV0(ConfigSettingContractBandwidthV0 {
            fee_tx_size1_kb: 0,
            tx_max_size_bytes: 10_000, // stellar-core min
            ledger_max_txs_size_bytes: 10_000,
        });
        assert!(
            ConfigUpgradeSetFrame::is_valid_config_setting_entry(&entry, 25),
            "ContractBandwidthV0 at stellar-core min must be accepted"
        );

        // ContractComputeV0 at stellar-core min
        let entry = ConfigSettingEntry::ContractComputeV0(ConfigSettingContractComputeV0 {
            fee_rate_per_instructions_increment: 0,
            tx_max_instructions: 2_500_000, // stellar-core min
            ledger_max_instructions: 2_500_000,
            tx_memory_limit: 2_000_000, // stellar-core min
        });
        assert!(
            ConfigUpgradeSetFrame::is_valid_config_setting_entry(&entry, 25),
            "ContractComputeV0 at stellar-core min must be accepted"
        );

        // ContractEventsV0 at stellar-core min
        let entry = ConfigSettingEntry::ContractEventsV0(ConfigSettingContractEventsV0 {
            tx_max_contract_events_size_bytes: 200, // stellar-core min
            fee_contract_events1_kb: 0,
        });
        assert!(
            ConfigUpgradeSetFrame::is_valid_config_setting_entry(&entry, 25),
            "ContractEventsV0 at stellar-core min must be accepted"
        );

        // StateArchival at stellar-core min
        let entry = ConfigSettingEntry::StateArchival(StateArchivalSettings {
            max_entry_ttl: 1_054_080, // stellar-core min
            min_temporary_ttl: 16,    // stellar-core min
            min_persistent_ttl: 10,   // stellar-core min
            persistent_rent_rate_denominator: 1,
            temp_rent_rate_denominator: 1,
            max_entries_to_archive: 0, // stellar-core min
            live_soroban_state_size_window_sample_size: 1, // stellar-core min
            eviction_scan_size: 0,     // stellar-core min (was 1000)
            starting_eviction_scan_level: 1, // stellar-core min (was 7)
            live_soroban_state_size_window_sample_period: 1,
        });
        assert!(
            ConfigUpgradeSetFrame::is_valid_config_setting_entry(&entry, 25),
            "StateArchival at stellar-core min must be accepted"
        );
    }

    /// Helper to create valid cost params with the given count.
    fn make_cost_params(count: usize) -> stellar_xdr::curr::ContractCostParams {
        use stellar_xdr::curr::{ContractCostParamEntry, ExtensionPoint};

        let entries: Vec<ContractCostParamEntry> = (0..count)
            .map(|_| ContractCostParamEntry {
                ext: ExtensionPoint::V0,
                const_term: 100,
                linear_term: 10,
            })
            .collect();
        stellar_xdr::curr::ContractCostParams(entries.try_into().unwrap())
    }

    #[test]
    fn test_cost_params_validation_v25_correct_count() {
        // V25 expects 85 params (Bn254FrInv=84 + 1)
        let params = make_cost_params(85);
        assert!(ConfigUpgradeSetFrame::is_valid_cost_params(&params, 25));
    }

    #[test]
    fn test_cost_params_validation_v25_wrong_count() {
        // V25 expects 85, not 70
        let params = make_cost_params(70);
        assert!(!ConfigUpgradeSetFrame::is_valid_cost_params(&params, 25));

        // Not 23 either
        let params = make_cost_params(23);
        assert!(!ConfigUpgradeSetFrame::is_valid_cost_params(&params, 25));
    }

    #[test]
    fn test_cost_params_validation_v20_correct_count() {
        // V20 (before V21) expects 23 params (ChaCha20DrawBytes=22 + 1)
        let params = make_cost_params(23);
        assert!(ConfigUpgradeSetFrame::is_valid_cost_params(&params, 20));
    }

    #[test]
    fn test_cost_params_validation_v21_correct_count() {
        // V21 (before V22) expects 45 params (VerifyEcdsaSecp256r1Sig=44 + 1)
        let params = make_cost_params(45);
        assert!(ConfigUpgradeSetFrame::is_valid_cost_params(&params, 21));
    }

    #[test]
    fn test_cost_params_validation_v22_correct_count() {
        // V22-V24 (before V25) expects 70 params (Bls12381FrInv=69 + 1)
        let params = make_cost_params(70);
        assert!(ConfigUpgradeSetFrame::is_valid_cost_params(&params, 22));
        assert!(ConfigUpgradeSetFrame::is_valid_cost_params(&params, 23));
        assert!(ConfigUpgradeSetFrame::is_valid_cost_params(&params, 24));
    }

    #[test]
    fn test_cost_params_validation_negative_values_rejected() {
        use stellar_xdr::curr::{ContractCostParamEntry, ExtensionPoint};

        // Create 85 valid params for V25, then make one have negative constTerm
        let mut entries: Vec<ContractCostParamEntry> = (0..85)
            .map(|_| ContractCostParamEntry {
                ext: ExtensionPoint::V0,
                const_term: 100,
                linear_term: 10,
            })
            .collect();
        entries[42].const_term = -1;
        let params = stellar_xdr::curr::ContractCostParams(entries.try_into().unwrap());
        assert!(
            !ConfigUpgradeSetFrame::is_valid_cost_params(&params, 25),
            "Negative constTerm should be rejected"
        );

        // Same but negative linearTerm
        let mut entries: Vec<ContractCostParamEntry> = (0..85)
            .map(|_| ContractCostParamEntry {
                ext: ExtensionPoint::V0,
                const_term: 100,
                linear_term: 10,
            })
            .collect();
        entries[10].linear_term = -5;
        let params = stellar_xdr::curr::ContractCostParams(entries.try_into().unwrap());
        assert!(
            !ConfigUpgradeSetFrame::is_valid_cost_params(&params, 25),
            "Negative linearTerm should be rejected"
        );
    }

    #[test]
    fn test_cost_params_validation_via_config_setting_entry() {
        // Verify it works when called through is_valid_config_setting_entry
        let valid_cpu_params = make_cost_params(85);
        let entry = ConfigSettingEntry::ContractCostParamsCpuInstructions(valid_cpu_params.clone());
        assert!(
            ConfigUpgradeSetFrame::is_valid_config_setting_entry(&entry, 25),
            "Valid CPU cost params should be accepted"
        );

        let entry = ConfigSettingEntry::ContractCostParamsMemoryBytes(valid_cpu_params);
        assert!(
            ConfigUpgradeSetFrame::is_valid_config_setting_entry(&entry, 25),
            "Valid memory cost params should be accepted"
        );

        // Wrong count should be rejected
        let invalid_params = make_cost_params(70); // V22 count, not V25
        let entry = ConfigSettingEntry::ContractCostParamsCpuInstructions(invalid_params);
        assert!(
            !ConfigUpgradeSetFrame::is_valid_config_setting_entry(&entry, 25),
            "Wrong count CPU cost params should be rejected"
        );
    }

    // ========================================================================
    // CAP-77: Frozen ledger key delta validation tests
    // ========================================================================

    /// Encode a LedgerKey to an EncodedLedgerKey for test construction.
    fn encode_ledger_key(key: &LedgerKey) -> EncodedLedgerKey {
        let bytes = key.to_xdr(Limits::none()).expect("XDR encode");
        EncodedLedgerKey(bytes.try_into().expect("fits BytesM"))
    }

    /// Build a FrozenLedgerKeysDelta with the given keys_to_freeze and empty keys_to_unfreeze.
    fn make_freeze_delta(keys_to_freeze: Vec<EncodedLedgerKey>) -> FrozenLedgerKeysDelta {
        FrozenLedgerKeysDelta {
            keys_to_freeze: keys_to_freeze.try_into().unwrap(),
            keys_to_unfreeze: vec![].try_into().unwrap(),
        }
    }

    /// Build a FrozenLedgerKeysDelta with the given keys_to_unfreeze and empty keys_to_freeze.
    fn make_unfreeze_delta(keys_to_unfreeze: Vec<EncodedLedgerKey>) -> FrozenLedgerKeysDelta {
        FrozenLedgerKeysDelta {
            keys_to_freeze: vec![].try_into().unwrap(),
            keys_to_unfreeze: keys_to_unfreeze.try_into().unwrap(),
        }
    }

    fn test_account_id() -> stellar_xdr::curr::AccountId {
        stellar_xdr::curr::AccountId(stellar_xdr::curr::PublicKey::PublicKeyTypeEd25519(
            stellar_xdr::curr::Uint256([42u8; 32]),
        ))
    }

    fn test_issuer_id() -> stellar_xdr::curr::AccountId {
        stellar_xdr::curr::AccountId(stellar_xdr::curr::PublicKey::PublicKeyTypeEd25519(
            stellar_xdr::curr::Uint256([99u8; 32]),
        ))
    }

    #[test]
    fn test_frozen_keys_delta_valid_account_key() {
        let key = LedgerKey::Account(stellar_xdr::curr::LedgerKeyAccount {
            account_id: test_account_id(),
        });
        let delta = make_freeze_delta(vec![encode_ledger_key(&key)]);
        let entry = ConfigSettingEntry::FrozenLedgerKeysDelta(delta);
        assert!(
            ConfigUpgradeSetFrame::is_valid_config_setting_entry(&entry, 26),
            "Account key should be accepted"
        );
    }

    #[test]
    fn test_frozen_keys_delta_valid_contract_data_key() {
        let key = LedgerKey::ContractData(LedgerKeyContractData {
            contract: ScAddress::Contract(ContractId(Hash([10u8; 32]))),
            key: ScVal::U32(1),
            durability: ContractDataDurability::Persistent,
        });
        let delta = make_freeze_delta(vec![encode_ledger_key(&key)]);
        let entry = ConfigSettingEntry::FrozenLedgerKeysDelta(delta);
        assert!(
            ConfigUpgradeSetFrame::is_valid_config_setting_entry(&entry, 26),
            "ContractData key should be accepted"
        );
    }

    #[test]
    fn test_frozen_keys_delta_valid_contract_code_key() {
        let key = LedgerKey::ContractCode(stellar_xdr::curr::LedgerKeyContractCode {
            hash: Hash([20u8; 32]),
        });
        let delta = make_freeze_delta(vec![encode_ledger_key(&key)]);
        let entry = ConfigSettingEntry::FrozenLedgerKeysDelta(delta);
        assert!(
            ConfigUpgradeSetFrame::is_valid_config_setting_entry(&entry, 26),
            "ContractCode key should be accepted"
        );
    }

    #[test]
    fn test_frozen_keys_delta_valid_trustline_key() {
        use stellar_xdr::curr::{AlphaNum4, AssetCode4, LedgerKeyTrustLine};
        let key = LedgerKey::Trustline(LedgerKeyTrustLine {
            account_id: test_account_id(),
            asset: TrustLineAsset::CreditAlphanum4(AlphaNum4 {
                asset_code: AssetCode4([b'U', b'S', b'D', 0]),
                issuer: test_issuer_id(),
            }),
        });
        let delta = make_freeze_delta(vec![encode_ledger_key(&key)]);
        let entry = ConfigSettingEntry::FrozenLedgerKeysDelta(delta);
        assert!(
            ConfigUpgradeSetFrame::is_valid_config_setting_entry(&entry, 26),
            "Non-issuer, non-pool-share trustline key should be accepted"
        );
    }

    #[test]
    fn test_frozen_keys_delta_rejects_malformed_xdr() {
        let bad_encoded = EncodedLedgerKey(vec![0xDE, 0xAD, 0xBE, 0xEF].try_into().unwrap());
        let delta = make_freeze_delta(vec![bad_encoded]);
        let entry = ConfigSettingEntry::FrozenLedgerKeysDelta(delta);
        assert!(
            !ConfigUpgradeSetFrame::is_valid_config_setting_entry(&entry, 26),
            "Malformed XDR should be rejected"
        );
    }

    #[test]
    fn test_frozen_keys_delta_rejects_unsupported_key_type_offer() {
        let key = LedgerKey::Offer(stellar_xdr::curr::LedgerKeyOffer {
            seller_id: test_account_id(),
            offer_id: 42,
        });
        let delta = make_freeze_delta(vec![encode_ledger_key(&key)]);
        let entry = ConfigSettingEntry::FrozenLedgerKeysDelta(delta);
        assert!(
            !ConfigUpgradeSetFrame::is_valid_config_setting_entry(&entry, 26),
            "Offer key type should be rejected"
        );
    }

    #[test]
    fn test_frozen_keys_delta_rejects_unsupported_key_type_data() {
        let key = LedgerKey::Data(stellar_xdr::curr::LedgerKeyData {
            account_id: test_account_id(),
            data_name: stellar_xdr::curr::String64(
                stellar_xdr::curr::StringM::<64>::try_from("test").unwrap(),
            ),
        });
        let delta = make_freeze_delta(vec![encode_ledger_key(&key)]);
        let entry = ConfigSettingEntry::FrozenLedgerKeysDelta(delta);
        assert!(
            !ConfigUpgradeSetFrame::is_valid_config_setting_entry(&entry, 26),
            "Data key type should be rejected"
        );
    }

    #[test]
    fn test_freeze_bypass_delta_rejected_before_v26() {
        let entry =
            ConfigSettingEntry::FreezeBypassTxsDelta(stellar_xdr::curr::FreezeBypassTxsDelta {
                add_txs: vec![].try_into().unwrap(),
                remove_txs: vec![].try_into().unwrap(),
            });
        assert!(
            !ConfigUpgradeSetFrame::is_valid_config_setting_entry(&entry, 25),
            "FreezeBypassTxsDelta should be rejected before protocol 26"
        );
    }

    #[test]
    fn test_freeze_bypass_base_entry_rejected_before_v26() {
        let entry = ConfigSettingEntry::FreezeBypassTxs(FreezeBypassTxs {
            tx_hashes: vec![].try_into().unwrap(),
        });
        assert!(
            !ConfigUpgradeSetFrame::is_valid_config_setting_entry(&entry, 25),
            "FreezeBypassTxs should be rejected before protocol 26"
        );
    }

    #[test]
    fn test_frozen_keys_delta_rejects_pool_share_trustline() {
        use stellar_xdr::curr::LedgerKeyTrustLine;
        let key = LedgerKey::Trustline(LedgerKeyTrustLine {
            account_id: test_account_id(),
            asset: TrustLineAsset::PoolShare(stellar_xdr::curr::PoolId(Hash([50u8; 32]))),
        });
        let delta = make_freeze_delta(vec![encode_ledger_key(&key)]);
        let entry = ConfigSettingEntry::FrozenLedgerKeysDelta(delta);
        assert!(
            !ConfigUpgradeSetFrame::is_valid_config_setting_entry(&entry, 26),
            "Pool-share trustline should be rejected"
        );
    }

    #[test]
    fn test_frozen_keys_delta_rejects_issuer_trustline() {
        use stellar_xdr::curr::{AlphaNum4, AssetCode4, LedgerKeyTrustLine};
        // Create a trustline where the account is the issuer of the asset
        let issuer = test_issuer_id();
        let key = LedgerKey::Trustline(LedgerKeyTrustLine {
            account_id: issuer.clone(),
            asset: TrustLineAsset::CreditAlphanum4(AlphaNum4 {
                asset_code: AssetCode4([b'U', b'S', b'D', 0]),
                issuer,
            }),
        });
        let delta = make_freeze_delta(vec![encode_ledger_key(&key)]);
        let entry = ConfigSettingEntry::FrozenLedgerKeysDelta(delta);
        assert!(
            !ConfigUpgradeSetFrame::is_valid_config_setting_entry(&entry, 26),
            "Issuer trustline should be rejected"
        );
    }

    #[test]
    fn test_frozen_keys_delta_rejects_issuer_trustline_alphanum12() {
        use stellar_xdr::curr::{AlphaNum12, AssetCode12, LedgerKeyTrustLine};
        let issuer = test_issuer_id();
        let key = LedgerKey::Trustline(LedgerKeyTrustLine {
            account_id: issuer.clone(),
            asset: TrustLineAsset::CreditAlphanum12(AlphaNum12 {
                asset_code: AssetCode12(*b"LONGASSET\0\0\0"),
                issuer,
            }),
        });
        let delta = make_freeze_delta(vec![encode_ledger_key(&key)]);
        let entry = ConfigSettingEntry::FrozenLedgerKeysDelta(delta);
        assert!(
            !ConfigUpgradeSetFrame::is_valid_config_setting_entry(&entry, 26),
            "Issuer trustline (alphanum12) should be rejected"
        );
    }

    #[test]
    fn test_frozen_keys_delta_validates_keys_to_unfreeze_too() {
        // Bad key in keys_to_unfreeze should also be rejected
        let bad_encoded = EncodedLedgerKey(vec![0xBA, 0xD0].try_into().unwrap());
        let delta = make_unfreeze_delta(vec![bad_encoded]);
        let entry = ConfigSettingEntry::FrozenLedgerKeysDelta(delta);
        assert!(
            !ConfigUpgradeSetFrame::is_valid_config_setting_entry(&entry, 26),
            "Malformed key in keys_to_unfreeze should be rejected"
        );
    }

    #[test]
    fn test_frozen_keys_delta_mixed_valid_and_invalid() {
        use stellar_xdr::curr::LedgerKeyTrustLine;
        // First key is valid (account), second is invalid (pool-share trustline)
        let valid_key = LedgerKey::Account(stellar_xdr::curr::LedgerKeyAccount {
            account_id: test_account_id(),
        });
        let invalid_key = LedgerKey::Trustline(LedgerKeyTrustLine {
            account_id: test_account_id(),
            asset: TrustLineAsset::PoolShare(stellar_xdr::curr::PoolId(Hash([50u8; 32]))),
        });
        let delta = make_freeze_delta(vec![
            encode_ledger_key(&valid_key),
            encode_ledger_key(&invalid_key),
        ]);
        let entry = ConfigSettingEntry::FrozenLedgerKeysDelta(delta);
        assert!(
            !ConfigUpgradeSetFrame::is_valid_config_setting_entry(&entry, 26),
            "Delta with any invalid key should be rejected"
        );
    }

    #[test]
    fn test_frozen_keys_delta_rejected_before_v26() {
        let key = LedgerKey::Account(stellar_xdr::curr::LedgerKeyAccount {
            account_id: test_account_id(),
        });
        let delta = make_freeze_delta(vec![encode_ledger_key(&key)]);
        let entry = ConfigSettingEntry::FrozenLedgerKeysDelta(delta);
        assert!(
            !ConfigUpgradeSetFrame::is_valid_config_setting_entry(&entry, 25),
            "FrozenLedgerKeysDelta should be rejected before protocol 26"
        );
    }

    #[test]
    fn test_frozen_keys_base_entry_rejected_before_v26() {
        let entry = ConfigSettingEntry::FrozenLedgerKeys(FrozenLedgerKeys {
            keys: vec![].try_into().unwrap(),
        });
        assert!(
            !ConfigUpgradeSetFrame::is_valid_config_setting_entry(&entry, 25),
            "FrozenLedgerKeys should be rejected before protocol 26"
        );
    }
}
