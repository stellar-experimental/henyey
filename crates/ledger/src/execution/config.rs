//! Soroban network configuration loading from ledger state.
//!
//! Loads `ConfigSettingEntry` values from the ledger snapshot and assembles
//! them into `SorobanNetworkInfo`, which provides all Soroban-related limits,
//! fee parameters, and cost model data needed during transaction execution.

use super::*;

/// Load a ConfigSettingEntry by ID from any entry reader.
///
/// Returns `Ok(Some(entry))` if the setting exists, `Ok(None)` if it genuinely
/// doesn't exist (e.g., pre-Soroban protocol), and `Err` if an I/O error
/// occurred during lookup. This matches stellar-core's behavior where I/O
/// errors are never silently swallowed.
pub(crate) fn load_config_setting(
    reader: &impl crate::EntryReader,
    id: ConfigSettingId,
) -> Result<Option<ConfigSettingEntry>> {
    let key = LedgerKey::ConfigSetting(LedgerKeyConfigSetting {
        config_setting_id: id,
    });
    match reader.get_entry(&key)? {
        Some(entry) => {
            if let LedgerEntryData::ConfigSetting(config) = entry.data {
                Ok(Some(config))
            } else {
                Ok(None)
            }
        }
        None => Ok(None),
    }
}

/// Load a required config setting and extract a specific variant.
///
/// Reduces the repeated pattern of:
/// 1. `load_config_setting(snapshot, id)?`
/// 2. `require_setting(result, id)?`
/// 3. `if let ConfigSettingEntry::Variant(val) = cs { val } else { return Err(...) }`
macro_rules! load_config {
    ($snapshot:expr, $id:expr, $variant:ident) => {{
        let id = $id;
        let cs = load_config_setting($snapshot, id)?.ok_or_else(|| {
            LedgerError::Internal(format!(
                "load_soroban_config: required config setting {:?} not found in ledger",
                id
            ))
        })?;
        if let ConfigSettingEntry::$variant(val) = cs {
            val
        } else {
            return Err(LedgerError::Internal(format!(
                "load_soroban_config: unexpected variant for {:?}",
                id
            )));
        }
    }};
}

/// Load SorobanConfig from the ledger's ConfigSettingEntry entries.
///
/// This loads the cost parameters and limits from the ledger state,
/// which are required for accurate Soroban transaction execution.
/// Returns an error if any required settings are missing or if an I/O
/// error occurs during lookup. This matches stellar-core's behavior
/// where missing config settings trigger `releaseAssertOrThrow` (fatal).
///
/// The `protocol_version` parameter is used to determine which fee to use
/// for `fee_per_write_1kb` in the FeeConfiguration:
/// - For protocol >= 23: uses `fee_write1_kb` from ContractLedgerCostExtV0
/// - For protocol < 23: uses the computed `fee_per_rent_1kb` (state-size based)
///
/// This matches stellar-core's `rustBridgeFeeConfiguration()` behavior.
pub fn load_soroban_config(
    reader: &impl crate::EntryReader,
    protocol_version: u32,
) -> Result<SorobanConfig> {
    // Load CPU cost params
    let cpu_cost_params = load_config!(
        reader,
        ConfigSettingId::ContractCostParamsCpuInstructions,
        ContractCostParamsCpuInstructions
    );

    // Load memory cost params
    let mem_cost_params = load_config!(
        reader,
        ConfigSettingId::ContractCostParamsMemoryBytes,
        ContractCostParamsMemoryBytes
    );

    // Load compute limits and fee rate per instructions
    let compute = load_config!(
        reader,
        ConfigSettingId::ContractComputeV0,
        ContractComputeV0
    );
    let (tx_max_instructions, tx_max_memory_bytes, fee_per_instruction_increment) = (
        compute.tx_max_instructions as u64,
        compute.tx_memory_limit as u64,
        compute.fee_rate_per_instructions_increment,
    );

    // Load ledger cost settings
    let cost = load_config!(
        reader,
        ConfigSettingId::ContractLedgerCostV0,
        ContractLedgerCostV0
    );
    let (
        fee_disk_read_ledger_entry,
        fee_write_ledger_entry,
        fee_disk_read_1kb,
        soroban_state_target_size_bytes,
        rent_fee_1kb_state_size_low,
        rent_fee_1kb_state_size_high,
        soroban_state_rent_fee_growth_factor,
    ) = (
        cost.fee_disk_read_ledger_entry,
        cost.fee_write_ledger_entry,
        cost.fee_disk_read1_kb,
        cost.soroban_state_target_size_bytes,
        cost.rent_fee1_kb_soroban_state_size_low,
        cost.rent_fee1_kb_soroban_state_size_high,
        cost.soroban_state_rent_fee_growth_factor,
    );

    // Load fee_write_1kb from extended cost settings (Protocol 23+).
    // For protocol < 23, this setting may not exist, so we use 0 as the default.
    let fee_write_1kb = {
        let id = ConfigSettingId::ContractLedgerCostExtV0;
        match load_config_setting(reader, id)? {
            Some(ConfigSettingEntry::ContractLedgerCostExtV0(ext)) => ext.fee_write1_kb,
            Some(_) => {
                return Err(LedgerError::Internal(format!(
                    "load_soroban_config: unexpected variant for {:?}",
                    id
                )));
            }
            None => 0, // Not available pre-Protocol 23
        }
    };

    let fee_historical_1kb = {
        let hist = load_config!(
            reader,
            ConfigSettingId::ContractHistoricalDataV0,
            ContractHistoricalDataV0
        );
        hist.fee_historical1_kb
    };

    let (tx_max_contract_events_size_bytes, fee_contract_events_1kb) = {
        let events = load_config!(reader, ConfigSettingId::ContractEventsV0, ContractEventsV0);
        (
            events.tx_max_contract_events_size_bytes,
            events.fee_contract_events1_kb,
        )
    };

    let fee_tx_size_1kb = {
        let bandwidth = load_config!(
            reader,
            ConfigSettingId::ContractBandwidthV0,
            ContractBandwidthV0
        );
        bandwidth.fee_tx_size1_kb
    };

    // Load contract size limits for entry validation (validateContractLedgerEntry)
    let max_contract_size_bytes = load_config!(
        reader,
        ConfigSettingId::ContractMaxSizeBytes,
        ContractMaxSizeBytes
    );

    let max_contract_data_entry_size_bytes = load_config!(
        reader,
        ConfigSettingId::ContractDataEntrySizeBytes,
        ContractDataEntrySizeBytes
    );

    // Load state archival TTL settings
    let archival = load_config!(reader, ConfigSettingId::StateArchival, StateArchival);
    tracing::debug!(
        min_temp_ttl = archival.min_temporary_ttl,
        min_persistent_ttl = archival.min_persistent_ttl,
        max_entry_ttl = archival.max_entry_ttl,
        persistent_rent_rate_denominator = archival.persistent_rent_rate_denominator,
        temp_rent_rate_denominator = archival.temp_rent_rate_denominator,
        "load_soroban_config: StateArchival settings from ledger"
    );
    let (
        min_temp_entry_ttl,
        min_persistent_entry_ttl,
        max_entry_ttl,
        persistent_rent_rate_denominator,
        temp_rent_rate_denominator,
    ) = (
        archival.min_temporary_ttl,
        archival.min_persistent_ttl,
        archival.max_entry_ttl,
        archival.persistent_rent_rate_denominator,
        archival.temp_rent_rate_denominator,
    );

    let average_soroban_state_size = {
        let window = load_config!(
            reader,
            ConfigSettingId::LiveSorobanStateSizeWindow,
            LiveSorobanStateSizeWindow
        );
        if window.is_empty() {
            0i64
        } else {
            let mut sum: u64 = 0;
            for size in window.iter() {
                sum = sum.saturating_add(*size);
            }
            (sum / window.len() as u64) as i64
        }
    };

    let rent_write_config = RentWriteFeeConfiguration {
        state_target_size_bytes: soroban_state_target_size_bytes,
        rent_fee_1kb_state_size_low,
        rent_fee_1kb_state_size_high,
        state_size_rent_fee_growth_factor: soroban_state_rent_fee_growth_factor,
    };
    let fee_per_rent_1kb =
        compute_rent_write_fee_per_1kb(average_soroban_state_size, &rent_write_config);

    tracing::debug!(
        fee_per_rent_1kb,
        average_soroban_state_size,
        state_target_size_bytes = soroban_state_target_size_bytes,
        rent_fee_1kb_state_size_low,
        rent_fee_1kb_state_size_high,
        soroban_state_rent_fee_growth_factor,
        "load_soroban_config: computed fee_per_rent_1kb"
    );

    // Protocol version-dependent fee selection matching stellar-core rustBridgeFeeConfiguration():
    // - For protocol >= 23: use fee_write_1kb (flat rate from ContractLedgerCostExtV0)
    // - For protocol < 23: use fee_per_rent_1kb (computed from state size)
    let fee_per_write_1kb_for_config =
        if protocol_version_starts_from(protocol_version, ProtocolVersion::V23) {
            fee_write_1kb
        } else {
            fee_per_rent_1kb
        };

    let fee_config = FeeConfiguration {
        fee_per_instruction_increment,
        fee_per_disk_read_entry: fee_disk_read_ledger_entry,
        fee_per_write_entry: fee_write_ledger_entry,
        fee_per_disk_read_1kb: fee_disk_read_1kb,
        fee_per_write_1kb: fee_per_write_1kb_for_config,
        fee_per_historical_1kb: fee_historical_1kb,
        fee_per_contract_event_1kb: fee_contract_events_1kb,
        fee_per_transaction_size_1kb: fee_tx_size_1kb,
    };

    // RentFeeConfiguration.fee_per_write_1kb must be feeFlatRateWrite1KB() to match stellar-core
    // rustBridgeRentFeeConfiguration(). This is 0 for protocol < 23 (the setting doesn't exist),
    // which is correct because the TTL entry write fee component was introduced in protocol 23.
    // This is DIFFERENT from FeeConfiguration.fee_per_write_1kb which uses fee_per_rent_1kb
    // for protocol < 23.
    let rent_fee_config = RentFeeConfiguration {
        fee_per_write_1kb: fee_write_1kb,
        fee_per_rent_1kb,
        fee_per_write_entry: fee_write_ledger_entry,
        persistent_rent_rate_denominator,
        temporary_rent_rate_denominator: temp_rent_rate_denominator,
    };

    let config = SorobanConfig {
        cpu_cost_params,
        mem_cost_params,
        tx_max_instructions,
        tx_max_memory_bytes,
        min_temp_entry_ttl,
        min_persistent_entry_ttl,
        max_entry_ttl,
        fee_config,
        rent_fee_config,
        tx_max_contract_events_size_bytes,
        max_contract_size_bytes,
        max_contract_data_entry_size_bytes,
    };

    // Log whether we found valid cost params
    if config.has_valid_cost_params() {
        debug!(
            cpu_cost_params_count = config.cpu_cost_params.0.len(),
            mem_cost_params_count = config.mem_cost_params.0.len(),
            tx_max_instructions = config.tx_max_instructions,
            fee_per_instruction = config.fee_config.fee_per_instruction_increment,
            fee_per_event_1kb = config.fee_config.fee_per_contract_event_1kb,
            "Loaded Soroban config from ledger"
        );
    } else {
        warn!(
            "No Soroban cost parameters found in ledger - using defaults. \
             Soroban transaction results may not match network."
        );
    }

    Ok(config)
}

/// Load SorobanNetworkInfo from the ledger's ConfigSettingEntry entries.
///
/// This loads all the configuration settings needed for the /sorobaninfo endpoint,
/// matching the "basic" format from stellar-core.
pub(crate) fn load_soroban_network_info(
    snapshot: &SnapshotHandle,
) -> Result<Option<SorobanNetworkInfo>> {
    // Check if we have any Soroban config (indicates protocol 20+)
    let compute_v0 = match load_config_setting(snapshot, ConfigSettingId::ContractComputeV0)? {
        Some(cs) => cs,
        None => return Ok(None),
    };

    let mut info = SorobanNetworkInfo::default();

    // Load contract size limits
    if let Some(ConfigSettingEntry::ContractDataKeySizeBytes(size)) =
        load_config_setting(snapshot, ConfigSettingId::ContractDataKeySizeBytes)?
    {
        info.max_contract_data_key_size = size;
    }
    if let Some(ConfigSettingEntry::ContractDataEntrySizeBytes(size)) =
        load_config_setting(snapshot, ConfigSettingId::ContractDataEntrySizeBytes)?
    {
        info.max_contract_data_entry_size = size;
    }
    if let Some(ConfigSettingEntry::ContractMaxSizeBytes(size)) =
        load_config_setting(snapshot, ConfigSettingId::ContractMaxSizeBytes)?
    {
        info.max_contract_size = size;
    }

    // Load compute settings
    if let ConfigSettingEntry::ContractComputeV0(compute) = compute_v0 {
        info.tx_max_instructions = compute.tx_max_instructions;
        info.ledger_max_instructions = compute.ledger_max_instructions;
        info.fee_rate_per_instructions_increment = compute.fee_rate_per_instructions_increment;
        info.tx_memory_limit = compute.tx_memory_limit;
    }

    // Load ledger access settings
    if let Some(ConfigSettingEntry::ContractLedgerCostV0(cost)) =
        load_config_setting(snapshot, ConfigSettingId::ContractLedgerCostV0)?
    {
        info.ledger_max_read_ledger_entries = cost.ledger_max_disk_read_entries;
        info.ledger_max_read_bytes = cost.ledger_max_disk_read_bytes;
        info.ledger_max_write_ledger_entries = cost.ledger_max_write_ledger_entries;
        info.ledger_max_write_bytes = cost.ledger_max_write_bytes;
        info.tx_max_read_ledger_entries = cost.tx_max_disk_read_entries;
        info.tx_max_read_bytes = cost.tx_max_disk_read_bytes;
        info.tx_max_write_ledger_entries = cost.tx_max_write_ledger_entries;
        info.tx_max_write_bytes = cost.tx_max_write_bytes;
        info.fee_read_ledger_entry = cost.fee_disk_read_ledger_entry;
        info.fee_write_ledger_entry = cost.fee_write_ledger_entry;
        info.fee_read_1kb = cost.fee_disk_read1_kb;
        info.state_target_size_bytes = cost.soroban_state_target_size_bytes;
        info.rent_fee_1kb_state_size_low = cost.rent_fee1_kb_soroban_state_size_low;
        info.rent_fee_1kb_state_size_high = cost.rent_fee1_kb_soroban_state_size_high;
        info.state_size_rent_fee_growth_factor = cost.soroban_state_rent_fee_growth_factor;
    }

    // Load fee_write_1kb from extended cost settings
    if let Some(ConfigSettingEntry::ContractLedgerCostExtV0(ext)) =
        load_config_setting(snapshot, ConfigSettingId::ContractLedgerCostExtV0)?
    {
        info.fee_write_1kb = ext.fee_write1_kb;
    }

    // Load historical data settings
    if let Some(ConfigSettingEntry::ContractHistoricalDataV0(hist)) =
        load_config_setting(snapshot, ConfigSettingId::ContractHistoricalDataV0)?
    {
        info.fee_historical_1kb = hist.fee_historical1_kb;
    }

    // Load contract events settings
    if let Some(ConfigSettingEntry::ContractEventsV0(events)) =
        load_config_setting(snapshot, ConfigSettingId::ContractEventsV0)?
    {
        info.tx_max_contract_events_size_bytes = events.tx_max_contract_events_size_bytes;
        info.fee_contract_events_size_1kb = events.fee_contract_events1_kb;
    }

    // Load bandwidth settings
    if let Some(ConfigSettingEntry::ContractBandwidthV0(bandwidth)) =
        load_config_setting(snapshot, ConfigSettingId::ContractBandwidthV0)?
    {
        info.ledger_max_tx_size_bytes = bandwidth.ledger_max_txs_size_bytes;
        info.tx_max_size_bytes = bandwidth.tx_max_size_bytes;
        info.fee_transaction_size_1kb = bandwidth.fee_tx_size1_kb;
    }

    // Load execution lanes settings for ledger tx count
    if let Some(ConfigSettingEntry::ContractExecutionLanes(lanes)) =
        load_config_setting(snapshot, ConfigSettingId::ContractExecutionLanes)?
    {
        info.ledger_max_tx_count = lanes.ledger_max_tx_count;
    }

    // Load state archival settings
    if let Some(ConfigSettingEntry::StateArchival(archival)) =
        load_config_setting(snapshot, ConfigSettingId::StateArchival)?
    {
        info.max_entry_ttl = archival.max_entry_ttl;
        info.min_temporary_ttl = archival.min_temporary_ttl;
        info.min_persistent_ttl = archival.min_persistent_ttl;
        info.persistent_rent_rate_denominator = archival.persistent_rent_rate_denominator;
        info.temp_rent_rate_denominator = archival.temp_rent_rate_denominator;
        info.max_entries_to_archive = archival.max_entries_to_archive;
        info.bucketlist_size_window_sample_size =
            archival.live_soroban_state_size_window_sample_size;
        info.eviction_scan_size = archival.eviction_scan_size as i64;
        info.starting_eviction_scan_level = archival.starting_eviction_scan_level;
    }

    // Load average bucket list size from live window
    if let Some(ConfigSettingEntry::LiveSorobanStateSizeWindow(window)) =
        load_config_setting(snapshot, ConfigSettingId::LiveSorobanStateSizeWindow)?
    {
        if !window.is_empty() {
            let mut sum: u64 = 0;
            for size in window.iter() {
                sum = sum.saturating_add(*size);
            }
            info.average_bucket_list_size = sum / window.len() as u64;
        }
    }

    // Load SCP timing settings (Protocol 23+)
    if let Some(ConfigSettingEntry::ScpTiming(timing)) =
        load_config_setting(snapshot, ConfigSettingId::ScpTiming)?
    {
        info.nomination_timeout_initial_ms = timing.nomination_timeout_initial_milliseconds;
        info.nomination_timeout_increment_ms = timing.nomination_timeout_increment_milliseconds;
        info.ballot_timeout_initial_ms = timing.ballot_timeout_initial_milliseconds;
        info.ballot_timeout_increment_ms = timing.ballot_timeout_increment_milliseconds;
        info.ledger_target_close_time_ms = timing.ledger_target_close_time_milliseconds;
    }

    // Load parallel compute settings (Protocol 23+)
    if let Some(ConfigSettingEntry::ContractParallelComputeV0(parallel)) =
        load_config_setting(snapshot, ConfigSettingId::ContractParallelComputeV0)?
    {
        info.ledger_max_dependent_tx_clusters = parallel.ledger_max_dependent_tx_clusters;
    }

    Ok(Some(info))
}

pub(crate) fn compute_soroban_resource_fee(
    frame: &TransactionFrame,
    protocol_version: u32,
    config: &SorobanConfig,
    event_size_bytes: u32,
) -> Option<(i64, i64)> {
    let resources = frame.soroban_transaction_resources(protocol_version, event_size_bytes)?;
    Some(compute_transaction_resource_fee(
        &resources,
        &config.fee_config,
    ))
}

/// Load frozen key configuration from ledger state (Protocol 26+, CAP-77).
///
/// Returns a `FrozenKeyConfig` loaded from CONFIG_SETTING_FROZEN_LEDGER_KEYS and
/// CONFIG_SETTING_FREEZE_BYPASS_TXS. Returns empty config if settings don't exist
/// (pre-P26 or not yet initialized).
pub fn load_frozen_key_config(
    reader: &impl crate::EntryReader,
    protocol_version: u32,
) -> Result<henyey_tx::frozen_keys::FrozenKeyConfig> {
    use henyey_common::protocol::{protocol_version_starts_from, ProtocolVersion};

    if !protocol_version_starts_from(protocol_version, ProtocolVersion::V26) {
        return Ok(henyey_tx::frozen_keys::FrozenKeyConfig::empty());
    }

    // Load frozen ledger keys
    let frozen_key_bytes = match load_config_setting(reader, ConfigSettingId::FrozenLedgerKeys)? {
        Some(ConfigSettingEntry::FrozenLedgerKeys(fk)) => {
            fk.keys.iter().map(|k| k.0.to_vec()).collect()
        }
        _ => Vec::new(),
    };

    // Load freeze bypass tx hashes
    let bypass_tx_hashes = match load_config_setting(reader, ConfigSettingId::FreezeBypassTxs)? {
        Some(ConfigSettingEntry::FreezeBypassTxs(bt)) => bt.tx_hashes.to_vec(),
        _ => Vec::new(),
    };

    Ok(henyey_tx::frozen_keys::FrozenKeyConfig::new(
        frozen_key_bytes,
        bypass_tx_hashes,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot::LedgerSnapshot;

    /// Regression test for AUDIT-C11: load_config_setting must propagate I/O errors,
    /// not silently swallow them and return None.
    #[test]
    fn test_audit_c11_load_config_setting_propagates_io_errors() {
        // Create a snapshot with a lookup function that always returns an error
        // (simulating a bucket list I/O failure).
        let error_lookup: crate::EntryLookupFn = std::sync::Arc::new(|_key: &LedgerKey| {
            Err(LedgerError::Internal("simulated I/O error".to_string()))
        });
        let snapshot = SnapshotHandle::with_lookup(LedgerSnapshot::empty(100), error_lookup);

        // Before the fix, this would return Ok(None) — silently swallowing the error.
        // After the fix, it must return Err.
        let result = load_config_setting(&snapshot, ConfigSettingId::ContractComputeV0);
        assert!(
            result.is_err(),
            "load_config_setting should propagate I/O errors, not swallow them"
        );
        let err_msg = format!("{}", result.unwrap_err());
        assert!(
            err_msg.contains("simulated I/O error"),
            "Error should contain the original I/O error message, got: {}",
            err_msg
        );
    }

    /// Regression test for AUDIT-C11: load_soroban_config must return Err when
    /// required config settings are missing, not silently fall back to defaults.
    #[test]
    fn test_audit_c11_load_soroban_config_errors_on_missing_settings() {
        // Create an empty snapshot with no config settings.
        // The lookup returns Ok(None) for everything (entry not found).
        let empty_lookup: crate::EntryLookupFn = std::sync::Arc::new(|_key: &LedgerKey| Ok(None));
        let snapshot = SnapshotHandle::with_lookup(LedgerSnapshot::empty(100), empty_lookup);

        // Before the fix, this would return a SorobanConfig with hardcoded defaults.
        // After the fix, it must return Err because required settings are missing.
        let result = load_soroban_config(&snapshot, 21);
        assert!(
            result.is_err(),
            "load_soroban_config should error when required settings are missing, \
             not silently fall back to defaults"
        );
        let err_msg = format!("{}", result.unwrap_err());
        assert!(
            err_msg.contains("required config setting"),
            "Error should mention missing required setting, got: {}",
            err_msg
        );
    }

    /// Regression test for AUDIT-C11: load_soroban_config must propagate I/O errors
    /// from underlying load_config_setting calls.
    #[test]
    fn test_audit_c11_load_soroban_config_propagates_io_errors() {
        // Create a snapshot where lookups fail with I/O errors.
        let error_lookup: crate::EntryLookupFn = std::sync::Arc::new(|_key: &LedgerKey| {
            Err(LedgerError::Internal("disk read failed".to_string()))
        });
        let snapshot = SnapshotHandle::with_lookup(LedgerSnapshot::empty(100), error_lookup);

        let result = load_soroban_config(&snapshot, 21);
        assert!(
            result.is_err(),
            "load_soroban_config should propagate I/O errors from load_config_setting"
        );
        let err_msg = format!("{}", result.unwrap_err());
        assert!(
            err_msg.contains("disk read failed"),
            "Error should contain the original I/O error, got: {}",
            err_msg
        );
    }
}
