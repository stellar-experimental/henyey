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
/// occurred during lookup or if the entry exists but contains a non-ConfigSetting
/// data variant (data corruption). This matches stellar-core's behavior where
/// data invariant violations are never silently swallowed.
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
                Err(LedgerError::Internal(format!(
                    "load_config_setting: expected ConfigSetting data for {:?}, got wrong LedgerEntryData variant",
                    id
                )))
            }
        }
        None => Ok(None),
    }
}

/// Load a required config setting and extract its inner value via `extract`.
///
/// Errors if:
/// - The entry doesn't exist (required in Soroban-active context)
/// - The `LedgerEntryData` is not `ConfigSetting` (checked by `load_config_setting`)
/// - The `ConfigSettingEntry` subtype doesn't match (`extract` returns `None`)
///
/// This is the function equivalent of the `load_config!` macro, usable from
/// any module in the crate (macros are module-local).
pub(crate) fn require_config<T>(
    reader: &impl crate::EntryReader,
    id: ConfigSettingId,
    extract: impl FnOnce(ConfigSettingEntry) -> Option<T>,
    ctx: &str,
) -> Result<T> {
    let cs = load_config_setting(reader, id)?.ok_or_else(|| {
        LedgerError::Internal(format!("{ctx}: required config setting {id:?} not found"))
    })?;
    extract(cs).ok_or_else(|| {
        LedgerError::Internal(format!(
            "{ctx}: unexpected ConfigSettingEntry variant for {id:?}"
        ))
    })
}

/// Load a required config setting and extract a specific variant.
///
/// Returns the inner value of the requested `ConfigSettingEntry` variant,
/// or errors if the setting is missing or has a wrong `ConfigSettingEntry`
/// subtype. The `$ctx` parameter identifies the calling function for
/// diagnostic messages.
macro_rules! load_config {
    ($snapshot:expr, $id:expr, $variant:ident, $ctx:expr) => {{
        let id = $id;
        let cs = load_config_setting($snapshot, id)?.ok_or_else(|| {
            LedgerError::Internal(format!(
                "{}: required config setting {:?} not found in ledger",
                $ctx, id
            ))
        })?;
        if let ConfigSettingEntry::$variant(val) = cs {
            val
        } else {
            return Err(LedgerError::Internal(format!(
                "{}: unexpected variant for {:?}",
                $ctx, id
            )));
        }
    }};
}

/// Load an optional config setting and extract a specific variant.
///
/// Returns `Some(inner)` if the setting exists with the correct variant,
/// `None` if the setting is genuinely absent, or errors if the setting
/// exists but has an unexpected `ConfigSettingEntry` subtype (data corruption).
macro_rules! load_config_optional {
    ($snapshot:expr, $id:expr, $variant:ident, $ctx:expr) => {{
        let id = $id;
        match load_config_setting($snapshot, id)? {
            Some(ConfigSettingEntry::$variant(val)) => Some(val),
            Some(_) => {
                return Err(LedgerError::Internal(format!(
                    "{}: unexpected variant for {:?}",
                    $ctx, id
                )));
            }
            None => None,
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
        ContractCostParamsCpuInstructions,
        "load_soroban_config"
    );

    // Load memory cost params
    let mem_cost_params = load_config!(
        reader,
        ConfigSettingId::ContractCostParamsMemoryBytes,
        ContractCostParamsMemoryBytes,
        "load_soroban_config"
    );

    // Load compute limits and fee rate per instructions
    let compute = load_config!(
        reader,
        ConfigSettingId::ContractComputeV0,
        ContractComputeV0,
        "load_soroban_config"
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
        ContractLedgerCostV0,
        "load_soroban_config"
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
            ContractHistoricalDataV0,
            "load_soroban_config"
        );
        hist.fee_historical1_kb
    };

    let (tx_max_contract_events_size_bytes, fee_contract_events_1kb) = {
        let events = load_config!(
            reader,
            ConfigSettingId::ContractEventsV0,
            ContractEventsV0,
            "load_soroban_config"
        );
        (
            events.tx_max_contract_events_size_bytes,
            events.fee_contract_events1_kb,
        )
    };

    let fee_tx_size_1kb = {
        let bandwidth = load_config!(
            reader,
            ConfigSettingId::ContractBandwidthV0,
            ContractBandwidthV0,
            "load_soroban_config"
        );
        bandwidth.fee_tx_size1_kb
    };

    // Load contract size limits for entry validation (validateContractLedgerEntry)
    let max_contract_size_bytes = load_config!(
        reader,
        ConfigSettingId::ContractMaxSizeBytes,
        ContractMaxSizeBytes,
        "load_soroban_config"
    );

    let max_contract_data_entry_size_bytes = load_config!(
        reader,
        ConfigSettingId::ContractDataEntrySizeBytes,
        ContractDataEntrySizeBytes,
        "load_soroban_config"
    );

    // Load state archival TTL settings
    let archival = load_config!(
        reader,
        ConfigSettingId::StateArchival,
        StateArchival,
        "load_soroban_config"
    );
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
            LiveSorobanStateSizeWindow,
            "load_soroban_config"
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
/// matching the "basic" format from stellar-core's `SorobanNetworkConfig::loadFromLedger`.
///
/// Returns `Ok(None)` if Soroban is not active (no `ContractComputeV0` setting exists).
/// Returns `Err` if any required setting is missing or has a wrong variant (data corruption).
/// V23+ settings (`ContractLedgerCostExtV0`, `ScpTiming`, `ContractParallelComputeV0`) are
/// loaded optionally since the snapshot header may be stale during protocol-upgrade ledgers.
pub(crate) fn load_soroban_network_info(
    snapshot: &SnapshotHandle,
) -> Result<Option<SorobanNetworkInfo>> {
    const CTX: &str = "load_soroban_network_info";

    // Probe: if ContractComputeV0 doesn't exist, Soroban is not active (pre-protocol 20)
    if load_config_setting(snapshot, ConfigSettingId::ContractComputeV0)?.is_none() {
        return Ok(None);
    }

    let mut info = SorobanNetworkInfo::default();

    // --- Base required settings (must exist once Soroban is active) ---

    let compute = load_config!(
        snapshot,
        ConfigSettingId::ContractComputeV0,
        ContractComputeV0,
        CTX
    );
    info.tx_max_instructions = compute.tx_max_instructions;
    info.ledger_max_instructions = compute.ledger_max_instructions;
    info.fee_rate_per_instructions_increment = compute.fee_rate_per_instructions_increment;
    info.tx_memory_limit = compute.tx_memory_limit;

    info.max_contract_data_key_size = load_config!(
        snapshot,
        ConfigSettingId::ContractDataKeySizeBytes,
        ContractDataKeySizeBytes,
        CTX
    );
    info.max_contract_data_entry_size = load_config!(
        snapshot,
        ConfigSettingId::ContractDataEntrySizeBytes,
        ContractDataEntrySizeBytes,
        CTX
    );
    info.max_contract_size = load_config!(
        snapshot,
        ConfigSettingId::ContractMaxSizeBytes,
        ContractMaxSizeBytes,
        CTX
    );

    let cost = load_config!(
        snapshot,
        ConfigSettingId::ContractLedgerCostV0,
        ContractLedgerCostV0,
        CTX
    );
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

    let hist = load_config!(
        snapshot,
        ConfigSettingId::ContractHistoricalDataV0,
        ContractHistoricalDataV0,
        CTX
    );
    info.fee_historical_1kb = hist.fee_historical1_kb;

    let events = load_config!(
        snapshot,
        ConfigSettingId::ContractEventsV0,
        ContractEventsV0,
        CTX
    );
    info.tx_max_contract_events_size_bytes = events.tx_max_contract_events_size_bytes;
    info.fee_contract_events_size_1kb = events.fee_contract_events1_kb;

    let bandwidth = load_config!(
        snapshot,
        ConfigSettingId::ContractBandwidthV0,
        ContractBandwidthV0,
        CTX
    );
    info.ledger_max_tx_size_bytes = bandwidth.ledger_max_txs_size_bytes;
    info.tx_max_size_bytes = bandwidth.tx_max_size_bytes;
    info.fee_transaction_size_1kb = bandwidth.fee_tx_size1_kb;

    let lanes = load_config!(
        snapshot,
        ConfigSettingId::ContractExecutionLanes,
        ContractExecutionLanes,
        CTX
    );
    info.ledger_max_tx_count = lanes.ledger_max_tx_count;

    let archival = load_config!(snapshot, ConfigSettingId::StateArchival, StateArchival, CTX);
    info.max_entry_ttl = archival.max_entry_ttl;
    info.min_temporary_ttl = archival.min_temporary_ttl;
    info.min_persistent_ttl = archival.min_persistent_ttl;
    info.persistent_rent_rate_denominator = archival.persistent_rent_rate_denominator;
    info.temp_rent_rate_denominator = archival.temp_rent_rate_denominator;
    info.max_entries_to_archive = archival.max_entries_to_archive;
    info.bucketlist_size_window_sample_size = archival.live_soroban_state_size_window_sample_size;
    info.eviction_scan_size = archival.eviction_scan_size as i64;
    info.starting_eviction_scan_level = archival.starting_eviction_scan_level;

    let window = load_config!(
        snapshot,
        ConfigSettingId::LiveSorobanStateSizeWindow,
        LiveSorobanStateSizeWindow,
        CTX
    );
    if !window.is_empty() {
        let mut sum: u64 = 0;
        for size in window.iter() {
            sum = sum.saturating_add(*size);
        }
        info.average_bucket_list_size = sum / window.len() as u64;
    }

    // --- V23+ settings (optional — may be absent during protocol upgrade ledgers) ---

    if let Some(ext) = load_config_optional!(
        snapshot,
        ConfigSettingId::ContractLedgerCostExtV0,
        ContractLedgerCostExtV0,
        CTX
    ) {
        info.fee_write_1kb = ext.fee_write1_kb;
        info.tx_max_footprint_entries = ext.tx_max_footprint_entries;
    }

    if let Some(timing) =
        load_config_optional!(snapshot, ConfigSettingId::ScpTiming, ScpTiming, CTX)
    {
        info.nomination_timeout_initial_ms = timing.nomination_timeout_initial_milliseconds;
        info.nomination_timeout_increment_ms = timing.nomination_timeout_increment_milliseconds;
        info.ballot_timeout_initial_ms = timing.ballot_timeout_initial_milliseconds;
        info.ballot_timeout_increment_ms = timing.ballot_timeout_increment_milliseconds;
        info.ledger_target_close_time_ms = timing.ledger_target_close_time_milliseconds;
    }

    if let Some(parallel) = load_config_optional!(
        snapshot,
        ConfigSettingId::ContractParallelComputeV0,
        ContractParallelComputeV0,
        CTX
    ) {
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
/// CONFIG_SETTING_FREEZE_BYPASS_TXS. Returns empty config for pre-P26 protocols.
/// Returns an error if settings are missing or have wrong variants for P26+,
/// since both are created during V26 ledger initialization.
pub fn load_frozen_key_config(
    reader: &impl crate::EntryReader,
    protocol_version: u32,
) -> Result<henyey_tx::frozen_keys::FrozenKeyConfig> {
    use henyey_common::protocol::{protocol_version_starts_from, ProtocolVersion};

    if !protocol_version_starts_from(protocol_version, ProtocolVersion::V26) {
        return Ok(henyey_tx::frozen_keys::FrozenKeyConfig::empty());
    }

    // Load frozen ledger keys (required for V26+)
    let fk = load_config!(
        reader,
        ConfigSettingId::FrozenLedgerKeys,
        FrozenLedgerKeys,
        "load_frozen_key_config"
    );
    let frozen_key_bytes = fk.keys.iter().map(|k| k.0.to_vec()).collect();

    // Load freeze bypass tx hashes (required for V26+)
    let bt = load_config!(
        reader,
        ConfigSettingId::FreezeBypassTxs,
        FreezeBypassTxs,
        "load_frozen_key_config"
    );
    let bypass_tx_hashes = bt.tx_hashes.to_vec();

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

    // --- Tests for load_config_setting wrong data variant (Layer 1) ---

    /// load_config_setting must error when an entry exists but contains a
    /// non-ConfigSetting LedgerEntryData variant (data corruption).
    #[test]
    fn test_load_config_setting_wrong_data_variant() {
        use stellar_xdr::curr::{
            AccountEntry, AccountId, LedgerEntryExt, PublicKey, SequenceNumber, Thresholds, Uint256,
        };

        let corrupt_lookup: crate::EntryLookupFn = std::sync::Arc::new(|_key: &LedgerKey| {
            // Return a LedgerEntry with Account data for a ConfigSetting key
            Ok(Some(LedgerEntry {
                last_modified_ledger_seq: 1,
                data: LedgerEntryData::Account(AccountEntry {
                    account_id: AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([0; 32]))),
                    balance: 0,
                    seq_num: SequenceNumber(0),
                    num_sub_entries: 0,
                    inflation_dest: None,
                    flags: 0,
                    home_domain: Default::default(),
                    thresholds: Thresholds([0; 4]),
                    signers: Default::default(),
                    ext: Default::default(),
                }),
                ext: LedgerEntryExt::V0,
            }))
        });
        let snapshot = SnapshotHandle::with_lookup(LedgerSnapshot::empty(100), corrupt_lookup);

        let result = load_config_setting(&snapshot, ConfigSettingId::ContractComputeV0);
        assert!(
            result.is_err(),
            "load_config_setting should error on wrong LedgerEntryData variant, not return Ok(None)"
        );
        let err_msg = format!("{}", result.unwrap_err());
        assert!(
            err_msg.contains("expected ConfigSetting data")
                && err_msg.contains("wrong LedgerEntryData variant"),
            "Error should describe the data variant mismatch, got: {}",
            err_msg
        );
    }

    // --- Tests for load_frozen_key_config ---

    /// Helper: create a LedgerEntry wrapping a ConfigSettingEntry.
    fn make_config_entry(setting: ConfigSettingEntry) -> LedgerEntry {
        use stellar_xdr::curr::LedgerEntryExt;
        LedgerEntry {
            last_modified_ledger_seq: 1,
            data: LedgerEntryData::ConfigSetting(setting),
            ext: LedgerEntryExt::V0,
        }
    }

    /// Pre-V26 protocol returns empty FrozenKeyConfig without loading any settings.
    #[test]
    fn test_load_frozen_key_config_pre_v26_returns_empty() {
        // Use an error lookup to prove no settings are accessed
        let error_lookup: crate::EntryLookupFn = std::sync::Arc::new(|_key: &LedgerKey| {
            Err(LedgerError::Internal("should not be called".to_string()))
        });
        let snapshot = SnapshotHandle::with_lookup(LedgerSnapshot::empty(100), error_lookup);

        let result = load_frozen_key_config(&snapshot, 25);
        assert!(result.is_ok(), "Pre-V26 should return Ok");
        let config = result.unwrap();
        assert!(
            !config.has_frozen_keys(),
            "Pre-V26 should return empty config"
        );
    }

    /// V26+ with both settings present and populated returns correct config.
    #[test]
    fn test_load_frozen_key_config_v26_happy_path() {
        use stellar_xdr::curr::{EncodedLedgerKey, FreezeBypassTxs, FrozenLedgerKeys, Hash};

        let lookup: crate::EntryLookupFn = std::sync::Arc::new(move |key: &LedgerKey| {
            if let LedgerKey::ConfigSetting(cs) = key {
                match cs.config_setting_id {
                    ConfigSettingId::FrozenLedgerKeys => Ok(Some(make_config_entry(
                        ConfigSettingEntry::FrozenLedgerKeys(FrozenLedgerKeys {
                            keys: vec![EncodedLedgerKey(vec![1u8; 32].try_into().unwrap())]
                                .try_into()
                                .unwrap(),
                        }),
                    ))),
                    ConfigSettingId::FreezeBypassTxs => Ok(Some(make_config_entry(
                        ConfigSettingEntry::FreezeBypassTxs(FreezeBypassTxs {
                            tx_hashes: vec![Hash([2u8; 32])].try_into().unwrap(),
                        }),
                    ))),
                    _ => Ok(None),
                }
            } else {
                Ok(None)
            }
        });
        let snapshot = SnapshotHandle::with_lookup(LedgerSnapshot::empty(100), lookup);

        let result = load_frozen_key_config(&snapshot, 26);
        assert!(result.is_ok(), "V26 with correct settings should succeed");
        let config = result.unwrap();
        assert!(config.has_frozen_keys(), "Config should have frozen keys");
    }

    /// V26+ with both settings present but empty (VecM::default) succeeds with empty config.
    #[test]
    fn test_load_frozen_key_config_v26_empty_settings() {
        use stellar_xdr::curr::{FreezeBypassTxs, FrozenLedgerKeys};

        let lookup: crate::EntryLookupFn = std::sync::Arc::new(|key: &LedgerKey| {
            if let LedgerKey::ConfigSetting(cs) = key {
                match cs.config_setting_id {
                    ConfigSettingId::FrozenLedgerKeys => Ok(Some(make_config_entry(
                        ConfigSettingEntry::FrozenLedgerKeys(FrozenLedgerKeys {
                            keys: Default::default(),
                        }),
                    ))),
                    ConfigSettingId::FreezeBypassTxs => Ok(Some(make_config_entry(
                        ConfigSettingEntry::FreezeBypassTxs(FreezeBypassTxs {
                            tx_hashes: Default::default(),
                        }),
                    ))),
                    _ => Ok(None),
                }
            } else {
                Ok(None)
            }
        });
        let snapshot = SnapshotHandle::with_lookup(LedgerSnapshot::empty(100), lookup);

        let result = load_frozen_key_config(&snapshot, 26);
        assert!(result.is_ok(), "V26 with empty settings should succeed");
        let config = result.unwrap();
        assert!(
            !config.has_frozen_keys(),
            "Config with empty settings should have no frozen keys"
        );
    }

    /// V26+ with missing FrozenLedgerKeys setting must error.
    #[test]
    fn test_load_frozen_key_config_missing_frozen_keys() {
        let lookup: crate::EntryLookupFn = std::sync::Arc::new(|_key: &LedgerKey| Ok(None));
        let snapshot = SnapshotHandle::with_lookup(LedgerSnapshot::empty(100), lookup);

        let result = load_frozen_key_config(&snapshot, 26);
        assert!(
            result.is_err(),
            "Missing FrozenLedgerKeys should error for V26+"
        );
        let err_msg = format!("{}", result.unwrap_err());
        assert!(
            err_msg.contains("required config setting") && err_msg.contains("FrozenLedgerKeys"),
            "Error should mention missing FrozenLedgerKeys, got: {}",
            err_msg
        );
    }

    /// V26+ with missing FreezeBypassTxs setting must error.
    #[test]
    fn test_load_frozen_key_config_missing_bypass_txs() {
        use stellar_xdr::curr::FrozenLedgerKeys;

        let lookup: crate::EntryLookupFn = std::sync::Arc::new(|key: &LedgerKey| {
            if let LedgerKey::ConfigSetting(cs) = key {
                match cs.config_setting_id {
                    ConfigSettingId::FrozenLedgerKeys => Ok(Some(make_config_entry(
                        ConfigSettingEntry::FrozenLedgerKeys(FrozenLedgerKeys {
                            keys: Default::default(),
                        }),
                    ))),
                    _ => Ok(None),
                }
            } else {
                Ok(None)
            }
        });
        let snapshot = SnapshotHandle::with_lookup(LedgerSnapshot::empty(100), lookup);

        let result = load_frozen_key_config(&snapshot, 26);
        assert!(
            result.is_err(),
            "Missing FreezeBypassTxs should error for V26+"
        );
        let err_msg = format!("{}", result.unwrap_err());
        assert!(
            err_msg.contains("required config setting") && err_msg.contains("FreezeBypassTxs"),
            "Error should mention missing FreezeBypassTxs, got: {}",
            err_msg
        );
    }

    /// V26+ with wrong ConfigSettingEntry subtype for FrozenLedgerKeys must error.
    #[test]
    fn test_load_frozen_key_config_wrong_subtype_frozen_keys() {
        // Return ContractComputeV0 when FrozenLedgerKeys is requested
        let lookup: crate::EntryLookupFn = std::sync::Arc::new(|key: &LedgerKey| {
            if let LedgerKey::ConfigSetting(_cs) = key {
                Ok(Some(make_config_entry(
                    ConfigSettingEntry::ContractComputeV0(Default::default()),
                )))
            } else {
                Ok(None)
            }
        });
        let snapshot = SnapshotHandle::with_lookup(LedgerSnapshot::empty(100), lookup);

        let result = load_frozen_key_config(&snapshot, 26);
        assert!(
            result.is_err(),
            "Wrong subtype for FrozenLedgerKeys should error"
        );
        let err_msg = format!("{}", result.unwrap_err());
        assert!(
            err_msg.contains("unexpected variant"),
            "Error should mention unexpected variant, got: {}",
            err_msg
        );
    }

    /// V26+ with wrong ConfigSettingEntry subtype for FreezeBypassTxs must error.
    #[test]
    fn test_load_frozen_key_config_wrong_subtype_bypass_txs() {
        use stellar_xdr::curr::FrozenLedgerKeys;

        // Return correct FrozenLedgerKeys, but wrong subtype for FreezeBypassTxs
        let lookup: crate::EntryLookupFn = std::sync::Arc::new(|key: &LedgerKey| {
            if let LedgerKey::ConfigSetting(cs) = key {
                match cs.config_setting_id {
                    ConfigSettingId::FrozenLedgerKeys => Ok(Some(make_config_entry(
                        ConfigSettingEntry::FrozenLedgerKeys(FrozenLedgerKeys {
                            keys: Default::default(),
                        }),
                    ))),
                    _ => {
                        // Return wrong subtype for FreezeBypassTxs
                        Ok(Some(make_config_entry(
                            ConfigSettingEntry::ContractComputeV0(Default::default()),
                        )))
                    }
                }
            } else {
                Ok(None)
            }
        });
        let snapshot = SnapshotHandle::with_lookup(LedgerSnapshot::empty(100), lookup);

        let result = load_frozen_key_config(&snapshot, 26);
        assert!(
            result.is_err(),
            "Wrong subtype for FreezeBypassTxs should error"
        );
        let err_msg = format!("{}", result.unwrap_err());
        assert!(
            err_msg.contains("unexpected variant"),
            "Error should mention unexpected variant, got: {}",
            err_msg
        );
    }

    // --- Tests for load_soroban_network_info variant masking fix ---

    /// Helper: create a lookup function that returns specific ConfigSettingEntry values
    /// based on the requested ConfigSettingId. Used to construct test snapshots for
    /// load_soroban_network_info.
    fn soroban_info_lookup(
        overrides: std::collections::HashMap<ConfigSettingId, Option<ConfigSettingEntry>>,
    ) -> crate::EntryLookupFn {
        use stellar_xdr::curr::LedgerEntryExt;
        std::sync::Arc::new(move |key: &LedgerKey| {
            if let LedgerKey::ConfigSetting(cs) = key {
                let id = cs.config_setting_id;
                if let Some(override_val) = overrides.get(&id) {
                    return match override_val {
                        Some(entry) => Ok(Some(LedgerEntry {
                            last_modified_ledger_seq: 1,
                            data: LedgerEntryData::ConfigSetting(entry.clone()),
                            ext: LedgerEntryExt::V0,
                        })),
                        None => Ok(None),
                    };
                }
                // Default: return correct default entries for all base settings
                let entry = match id {
                    ConfigSettingId::ContractComputeV0 => {
                        Some(ConfigSettingEntry::ContractComputeV0(Default::default()))
                    }
                    ConfigSettingId::ContractDataKeySizeBytes => {
                        Some(ConfigSettingEntry::ContractDataKeySizeBytes(64))
                    }
                    ConfigSettingId::ContractDataEntrySizeBytes => {
                        Some(ConfigSettingEntry::ContractDataEntrySizeBytes(65536))
                    }
                    ConfigSettingId::ContractMaxSizeBytes => {
                        Some(ConfigSettingEntry::ContractMaxSizeBytes(65536))
                    }
                    ConfigSettingId::ContractLedgerCostV0 => {
                        Some(ConfigSettingEntry::ContractLedgerCostV0(Default::default()))
                    }
                    ConfigSettingId::ContractHistoricalDataV0 => Some(
                        ConfigSettingEntry::ContractHistoricalDataV0(Default::default()),
                    ),
                    ConfigSettingId::ContractEventsV0 => {
                        Some(ConfigSettingEntry::ContractEventsV0(Default::default()))
                    }
                    ConfigSettingId::ContractBandwidthV0 => {
                        Some(ConfigSettingEntry::ContractBandwidthV0(Default::default()))
                    }
                    ConfigSettingId::ContractExecutionLanes => Some(
                        ConfigSettingEntry::ContractExecutionLanes(Default::default()),
                    ),
                    ConfigSettingId::StateArchival => {
                        Some(ConfigSettingEntry::StateArchival(Default::default()))
                    }
                    ConfigSettingId::LiveSorobanStateSizeWindow => Some(
                        ConfigSettingEntry::LiveSorobanStateSizeWindow(Default::default()),
                    ),
                    _ => None,
                };
                match entry {
                    Some(e) => Ok(Some(LedgerEntry {
                        last_modified_ledger_seq: 1,
                        data: LedgerEntryData::ConfigSetting(e),
                        ext: LedgerEntryExt::V0,
                    })),
                    None => Ok(None),
                }
            } else {
                Ok(None)
            }
        })
    }

    /// Pre-Soroban: no ContractComputeV0 entry returns Ok(None).
    #[test]
    fn test_load_soroban_network_info_pre_soroban_returns_none() {
        let mut overrides = std::collections::HashMap::new();
        overrides.insert(ConfigSettingId::ContractComputeV0, None);
        let lookup = soroban_info_lookup(overrides);
        let snapshot = SnapshotHandle::with_lookup(LedgerSnapshot::empty(100), lookup);

        let result = load_soroban_network_info(&snapshot);
        assert!(result.is_ok());
        assert!(
            result.unwrap().is_none(),
            "Should return None when Soroban is not active"
        );
    }

    /// All base settings correct returns Ok(Some(info)) with populated fields.
    #[test]
    fn test_load_soroban_network_info_all_correct() {
        let overrides = std::collections::HashMap::new();
        let lookup = soroban_info_lookup(overrides);
        let snapshot = SnapshotHandle::with_lookup(LedgerSnapshot::empty(100), lookup);

        let result = load_soroban_network_info(&snapshot);
        assert!(result.is_ok(), "Should succeed with all correct settings");
        assert!(result.unwrap().is_some(), "Should return Some(info)");
    }

    /// Missing required base setting (StateArchival) after Soroban probe passes → error.
    #[test]
    fn test_load_soroban_network_info_missing_required_setting() {
        let mut overrides = std::collections::HashMap::new();
        overrides.insert(ConfigSettingId::StateArchival, None);
        let lookup = soroban_info_lookup(overrides);
        let snapshot = SnapshotHandle::with_lookup(LedgerSnapshot::empty(100), lookup);

        let result = load_soroban_network_info(&snapshot);
        assert!(
            result.is_err(),
            "Should error when required setting is missing"
        );
        let err_msg = format!("{}", result.unwrap_err());
        assert!(
            err_msg.contains("required config setting") && err_msg.contains("StateArchival"),
            "Error should mention missing required setting, got: {}",
            err_msg
        );
    }

    /// Wrong variant for required base setting (ContractComputeV0) → error.
    #[test]
    fn test_load_soroban_network_info_wrong_variant_required() {
        let mut overrides = std::collections::HashMap::new();
        // Return a ContractBandwidthV0 value for the ContractComputeV0 key
        overrides.insert(
            ConfigSettingId::ContractComputeV0,
            Some(ConfigSettingEntry::ContractBandwidthV0(Default::default())),
        );
        let lookup = soroban_info_lookup(overrides);
        let snapshot = SnapshotHandle::with_lookup(LedgerSnapshot::empty(100), lookup);

        let result = load_soroban_network_info(&snapshot);
        assert!(
            result.is_err(),
            "Should error when required setting has wrong variant"
        );
        let err_msg = format!("{}", result.unwrap_err());
        assert!(
            err_msg.contains("unexpected variant") && err_msg.contains("ContractComputeV0"),
            "Error should mention unexpected variant for ContractComputeV0, got: {}",
            err_msg
        );
    }

    /// Wrong variant for optional V23 setting (ContractLedgerCostExtV0) → error.
    #[test]
    fn test_load_soroban_network_info_wrong_variant_optional_v23() {
        let mut overrides = std::collections::HashMap::new();
        // Return a wrong variant for the V23 optional setting
        overrides.insert(
            ConfigSettingId::ContractLedgerCostExtV0,
            Some(ConfigSettingEntry::ContractComputeV0(Default::default())),
        );
        let lookup = soroban_info_lookup(overrides);
        let snapshot = SnapshotHandle::with_lookup(LedgerSnapshot::empty(100), lookup);

        let result = load_soroban_network_info(&snapshot);
        assert!(
            result.is_err(),
            "Should error when V23 optional setting has wrong variant"
        );
        let err_msg = format!("{}", result.unwrap_err());
        assert!(
            err_msg.contains("unexpected variant") && err_msg.contains("ContractLedgerCostExtV0"),
            "Error should mention unexpected variant for V23 setting, got: {}",
            err_msg
        );
    }

    /// V23 settings absent (None) is acceptable — returns Ok(Some(info)) with defaults.
    #[test]
    fn test_load_soroban_network_info_v23_settings_absent_ok() {
        // Default lookup doesn't include V23 settings, so they're absent (None)
        let overrides = std::collections::HashMap::new();
        let lookup = soroban_info_lookup(overrides);
        let snapshot = SnapshotHandle::with_lookup(LedgerSnapshot::empty(100), lookup);

        let result = load_soroban_network_info(&snapshot);
        assert!(
            result.is_ok(),
            "Should succeed when V23 settings are absent"
        );
        let info = result.unwrap().unwrap();
        // V23 fields should be at default (zero)
        assert_eq!(
            info.fee_write_1kb, 0,
            "fee_write_1kb should be default (0) when V23 absent"
        );
        assert_eq!(
            info.ledger_max_dependent_tx_clusters, 0,
            "ledger_max_dependent_tx_clusters should be default (0) when V23 absent"
        );
    }

    /// V23 settings present and correct are loaded properly.
    #[test]
    fn test_load_soroban_network_info_v23_settings_present() {
        use stellar_xdr::curr::{
            ConfigSettingContractLedgerCostExtV0, ConfigSettingContractParallelComputeV0,
            ConfigSettingScpTiming,
        };

        let mut overrides = std::collections::HashMap::new();
        overrides.insert(
            ConfigSettingId::ContractLedgerCostExtV0,
            Some(ConfigSettingEntry::ContractLedgerCostExtV0(
                ConfigSettingContractLedgerCostExtV0 {
                    fee_write1_kb: 42,
                    tx_max_footprint_entries: 10,
                },
            )),
        );
        overrides.insert(
            ConfigSettingId::ScpTiming,
            Some(ConfigSettingEntry::ScpTiming(ConfigSettingScpTiming {
                nomination_timeout_initial_milliseconds: 1000,
                nomination_timeout_increment_milliseconds: 500,
                ballot_timeout_initial_milliseconds: 2000,
                ballot_timeout_increment_milliseconds: 300,
                ledger_target_close_time_milliseconds: 5000,
            })),
        );
        overrides.insert(
            ConfigSettingId::ContractParallelComputeV0,
            Some(ConfigSettingEntry::ContractParallelComputeV0(
                ConfigSettingContractParallelComputeV0 {
                    ledger_max_dependent_tx_clusters: 7,
                },
            )),
        );
        let lookup = soroban_info_lookup(overrides);
        let snapshot = SnapshotHandle::with_lookup(LedgerSnapshot::empty(100), lookup);

        let result = load_soroban_network_info(&snapshot);
        assert!(
            result.is_ok(),
            "Should succeed with all V23 settings present"
        );
        let info = result.unwrap().unwrap();
        assert_eq!(info.fee_write_1kb, 42);
        assert_eq!(info.tx_max_footprint_entries, 10);
        assert_eq!(info.nomination_timeout_initial_ms, 1000);
        assert_eq!(info.nomination_timeout_increment_ms, 500);
        assert_eq!(info.ballot_timeout_initial_ms, 2000);
        assert_eq!(info.ballot_timeout_increment_ms, 300);
        assert_eq!(info.ledger_target_close_time_ms, 5000);
        assert_eq!(info.ledger_max_dependent_tx_clusters, 7);
    }

    /// Wrong variant for another base required setting (ContractLedgerCostV0) → error.
    #[test]
    fn test_load_soroban_network_info_wrong_variant_ledger_cost() {
        let mut overrides = std::collections::HashMap::new();
        overrides.insert(
            ConfigSettingId::ContractLedgerCostV0,
            Some(ConfigSettingEntry::ContractEventsV0(Default::default())),
        );
        let lookup = soroban_info_lookup(overrides);
        let snapshot = SnapshotHandle::with_lookup(LedgerSnapshot::empty(100), lookup);

        let result = load_soroban_network_info(&snapshot);
        assert!(
            result.is_err(),
            "Should error when ContractLedgerCostV0 has wrong variant"
        );
        let err_msg = format!("{}", result.unwrap_err());
        assert!(
            err_msg.contains("unexpected variant") && err_msg.contains("ContractLedgerCostV0"),
            "Error should mention unexpected variant, got: {}",
            err_msg
        );
    }

    // --- Tests for require_config ---

    /// require_config succeeds when the entry exists and the extract closure matches.
    #[test]
    fn test_require_config_happy_path() {
        use stellar_xdr::curr::ConfigSettingContractComputeV0;

        let lookup: crate::EntryLookupFn = std::sync::Arc::new(|key: &LedgerKey| {
            if let LedgerKey::ConfigSetting(cs) = key {
                if cs.config_setting_id == ConfigSettingId::ContractComputeV0 {
                    return Ok(Some(make_config_entry(
                        ConfigSettingEntry::ContractComputeV0(ConfigSettingContractComputeV0 {
                            ledger_max_instructions: 100,
                            tx_max_instructions: 50,
                            fee_rate_per_instructions_increment: 10,
                            tx_memory_limit: 1000,
                        }),
                    )));
                }
            }
            Ok(None)
        });
        let snapshot = SnapshotHandle::with_lookup(LedgerSnapshot::empty(100), lookup);

        let result = require_config(
            &snapshot,
            ConfigSettingId::ContractComputeV0,
            |cs| {
                if let ConfigSettingEntry::ContractComputeV0(v) = cs {
                    Some(v)
                } else {
                    None
                }
            },
            "test_happy_path",
        );
        assert!(result.is_ok(), "require_config should succeed");
        assert_eq!(result.unwrap().ledger_max_instructions, 100);
    }

    /// require_config errors when the entry is missing entirely.
    #[test]
    fn test_require_config_missing_entry() {
        let lookup: crate::EntryLookupFn = std::sync::Arc::new(|_key: &LedgerKey| Ok(None));
        let snapshot = SnapshotHandle::with_lookup(LedgerSnapshot::empty(100), lookup);

        let result = require_config(
            &snapshot,
            ConfigSettingId::ContractComputeV0,
            |cs| {
                if let ConfigSettingEntry::ContractComputeV0(v) = cs {
                    Some(v)
                } else {
                    None
                }
            },
            "test_missing",
        );
        assert!(
            result.is_err(),
            "require_config should error on missing entry"
        );
        let err_msg = format!("{}", result.unwrap_err());
        assert!(
            err_msg.contains("required config setting") && err_msg.contains("not found"),
            "Error should say config not found, got: {}",
            err_msg
        );
    }

    /// require_config errors when the ConfigSettingEntry variant doesn't match the extractor.
    #[test]
    fn test_require_config_wrong_variant() {
        use stellar_xdr::curr::ConfigSettingContractComputeV0;

        // Store a ContractComputeV0 but try to extract StateArchivalSettings
        let lookup: crate::EntryLookupFn = std::sync::Arc::new(|key: &LedgerKey| {
            if let LedgerKey::ConfigSetting(cs) = key {
                if cs.config_setting_id == ConfigSettingId::StateArchival {
                    return Ok(Some(make_config_entry(
                        ConfigSettingEntry::ContractComputeV0(ConfigSettingContractComputeV0 {
                            ledger_max_instructions: 100,
                            tx_max_instructions: 50,
                            fee_rate_per_instructions_increment: 10,
                            tx_memory_limit: 1000,
                        }),
                    )));
                }
            }
            Ok(None)
        });
        let snapshot = SnapshotHandle::with_lookup(LedgerSnapshot::empty(100), lookup);

        let result = require_config(
            &snapshot,
            ConfigSettingId::StateArchival,
            |cs| {
                if let ConfigSettingEntry::StateArchival(v) = cs {
                    Some(v)
                } else {
                    None
                }
            },
            "test_wrong_variant",
        );
        assert!(
            result.is_err(),
            "require_config should error on wrong variant"
        );
        let err_msg = format!("{}", result.unwrap_err());
        assert!(
            err_msg.contains("unexpected ConfigSettingEntry variant"),
            "Error should mention unexpected variant, got: {}",
            err_msg
        );
    }

    /// require_config propagates I/O errors from the reader.
    #[test]
    fn test_require_config_propagates_io_error() {
        let error_lookup: crate::EntryLookupFn = std::sync::Arc::new(|_key: &LedgerKey| {
            Err(LedgerError::Internal("disk failure".to_string()))
        });
        let snapshot = SnapshotHandle::with_lookup(LedgerSnapshot::empty(100), error_lookup);

        let result = require_config(
            &snapshot,
            ConfigSettingId::ContractComputeV0,
            |cs| {
                if let ConfigSettingEntry::ContractComputeV0(v) = cs {
                    Some(v)
                } else {
                    None
                }
            },
            "test_io_error",
        );
        assert!(
            result.is_err(),
            "require_config should propagate I/O errors"
        );
        let err_msg = format!("{}", result.unwrap_err());
        assert!(
            err_msg.contains("disk failure"),
            "Error should contain original I/O error, got: {}",
            err_msg
        );
    }
}
