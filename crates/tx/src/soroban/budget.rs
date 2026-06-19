//! Soroban resource budget tracking.
//!
//! Tracks CPU instructions and memory usage for contract execution.

pub use soroban_env_host_p25::fees::{FeeConfiguration, RentFeeConfiguration};
use stellar_xdr::curr::ContractCostParams;

/// Soroban network configuration for contract execution.
///
/// This contains the cost parameters and limits loaded from the network's
/// ConfigSettingEntry entries. These must match the network to produce
/// correct transaction results and ledger hashes.
#[derive(Debug)]
pub struct SorobanConfig {
    /// CPU cost model parameters from ConfigSettingId::ContractCostParamsCpuInstructions.
    pub cpu_cost_params: ContractCostParams,
    /// Memory cost model parameters from ConfigSettingId::ContractCostParamsMemoryBytes.
    pub mem_cost_params: ContractCostParams,
    /// Maximum CPU instructions per transaction from ConfigSettingId::ContractComputeV0.
    pub tx_max_instructions: u64,
    /// Maximum memory bytes per transaction.
    pub tx_max_memory_bytes: u64,
    /// Minimum TTL for temporary entries.
    pub min_temp_entry_ttl: u32,
    /// Minimum TTL for persistent entries.
    pub min_persistent_entry_ttl: u32,
    /// Maximum TTL for any entry.
    pub max_entry_ttl: u32,
    /// Fee configuration for Soroban resource fees.
    pub fee_config: FeeConfiguration,
    /// Rent fee configuration for Soroban storage.
    pub rent_fee_config: RentFeeConfiguration,
    /// Maximum size of contract events + return value per tx.
    pub tx_max_contract_events_size_bytes: u32,
    /// Maximum CONTRACT_CODE entry size in bytes (from ConfigSettingId::ContractMaxSizeBytes).
    pub max_contract_size_bytes: u32,
    /// Maximum CONTRACT_DATA entry size in bytes (from ConfigSettingId::ContractDataEntrySizeBytes).
    pub max_contract_data_entry_size_bytes: u32,
}

impl Clone for SorobanConfig {
    fn clone(&self) -> Self {
        Self {
            cpu_cost_params: self.cpu_cost_params.clone(),
            mem_cost_params: self.mem_cost_params.clone(),
            tx_max_instructions: self.tx_max_instructions,
            tx_max_memory_bytes: self.tx_max_memory_bytes,
            min_temp_entry_ttl: self.min_temp_entry_ttl,
            min_persistent_entry_ttl: self.min_persistent_entry_ttl,
            max_entry_ttl: self.max_entry_ttl,
            fee_config: FeeConfiguration {
                fee_per_instruction_increment: self.fee_config.fee_per_instruction_increment,
                fee_per_disk_read_entry: self.fee_config.fee_per_disk_read_entry,
                fee_per_write_entry: self.fee_config.fee_per_write_entry,
                fee_per_disk_read_1kb: self.fee_config.fee_per_disk_read_1kb,
                fee_per_write_1kb: self.fee_config.fee_per_write_1kb,
                fee_per_historical_1kb: self.fee_config.fee_per_historical_1kb,
                fee_per_contract_event_1kb: self.fee_config.fee_per_contract_event_1kb,
                fee_per_transaction_size_1kb: self.fee_config.fee_per_transaction_size_1kb,
            },
            rent_fee_config: RentFeeConfiguration {
                fee_per_write_1kb: self.rent_fee_config.fee_per_write_1kb,
                fee_per_rent_1kb: self.rent_fee_config.fee_per_rent_1kb,
                fee_per_write_entry: self.rent_fee_config.fee_per_write_entry,
                persistent_rent_rate_denominator: self
                    .rent_fee_config
                    .persistent_rent_rate_denominator,
                temporary_rent_rate_denominator: self
                    .rent_fee_config
                    .temporary_rent_rate_denominator,
            },
            tx_max_contract_events_size_bytes: self.tx_max_contract_events_size_bytes,
            max_contract_size_bytes: self.max_contract_size_bytes,
            max_contract_data_entry_size_bytes: self.max_contract_data_entry_size_bytes,
        }
    }
}

/// Synthetic placeholder with incomplete/zero values.
///
/// **Only valid for pre-protocol-20 struct initialization and test fixtures
/// where Soroban operations are not executed.** Production Soroban execution
/// MUST use config loaded from ledger entries via `require_soroban_config()`.
///
/// Notable dangerous defaults: `tx_max_contract_events_size_bytes: 0` and
/// empty cost params. These act as a canary — any Soroban execution using
/// this default will produce incorrect results immediately.
impl Default for SorobanConfig {
    fn default() -> Self {
        Self {
            cpu_cost_params: ContractCostParams(vec![].try_into().unwrap_or_default()),
            mem_cost_params: ContractCostParams(vec![].try_into().unwrap_or_default()),
            tx_max_instructions: 100_000_000, // 100M instructions
            tx_max_memory_bytes: 40 * 1024 * 1024, // 40 MB
            min_temp_entry_ttl: 16,
            min_persistent_entry_ttl: 120960, // ~7 days at 5s ledger close
            max_entry_ttl: 6312000,           // ~1 year
            fee_config: FeeConfiguration::default(),
            rent_fee_config: RentFeeConfiguration::default(),
            tx_max_contract_events_size_bytes: 0,
            max_contract_size_bytes: 64 * 1024, // 64 KB default
            max_contract_data_entry_size_bytes: 64 * 1024, // 64 KB default
        }
    }
}

impl SorobanConfig {
    /// Check if this config has valid cost parameters.
    ///
    /// Returns false if the cost params are empty (default/placeholder values).
    pub fn has_valid_cost_params(&self) -> bool {
        !self.cpu_cost_params.0.is_empty() && !self.mem_cost_params.0.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test SorobanConfig default values.
    #[test]
    fn test_soroban_config_default() {
        let config = SorobanConfig::default();

        assert_eq!(config.tx_max_instructions, 100_000_000);
        assert_eq!(config.tx_max_memory_bytes, 40 * 1024 * 1024);
        assert_eq!(config.min_temp_entry_ttl, 16);
        assert_eq!(config.min_persistent_entry_ttl, 120960);
        assert_eq!(config.max_entry_ttl, 6312000);
        // Default has empty cost params
        assert!(!config.has_valid_cost_params());
    }
}
