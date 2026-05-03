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

/// Resource limits for Soroban execution.
#[derive(Debug, Clone, Copy)]
pub struct ResourceLimits {
    /// Maximum CPU instructions allowed.
    pub cpu_instructions: u64,
    /// Maximum memory bytes allowed.
    pub memory_bytes: u64,
    /// Maximum read bytes allowed.
    pub read_bytes: u64,
    /// Maximum write bytes allowed.
    pub write_bytes: u64,
    /// Maximum read entries allowed.
    pub read_entries: u32,
    /// Maximum write entries allowed.
    pub write_entries: u32,
}

// SECURITY: dead code; production budget comes from Budget::try_from_configs(), not Default
impl Default for ResourceLimits {
    fn default() -> Self {
        Self {
            cpu_instructions: 100_000_000,  // 100M instructions
            memory_bytes: 64 * 1024 * 1024, // 64 MB
            read_bytes: 200 * 1024,         // 200 KB
            write_bytes: 65 * 1024,         // 65 KB
            read_entries: 40,
            write_entries: 25,
        }
    }
}

impl ResourceLimits {
    /// Create limits from SorobanResources XDR.
    pub fn from_soroban_resources(resources: &stellar_xdr::curr::SorobanResources) -> Self {
        Self {
            cpu_instructions: resources.instructions as u64,
            memory_bytes: 64 * 1024 * 1024, // Fixed memory limit
            read_bytes: resources.disk_read_bytes as u64,
            write_bytes: resources.write_bytes as u64,
            read_entries: resources.footprint.read_only.len() as u32,
            write_entries: resources.footprint.read_write.len() as u32,
        }
    }
}

/// Budget tracker for Soroban execution.
#[derive(Debug, Clone)]
pub struct SorobanBudget {
    /// Current CPU instructions used.
    pub cpu_used: u64,
    /// Current memory bytes used.
    pub mem_used: u64,
    /// Current read bytes used.
    pub read_bytes_used: u64,
    /// Current write bytes used.
    pub write_bytes_used: u64,
    /// Resource limits.
    pub limits: ResourceLimits,
}

// SECURITY: entry-count limits enforced by Soroban host runtime during contract execution
impl SorobanBudget {
    /// Create a new budget with the given limits.
    pub fn new(limits: ResourceLimits) -> Self {
        Self {
            cpu_used: 0,
            mem_used: 0,
            read_bytes_used: 0,
            write_bytes_used: 0,
            limits,
        }
    }

    /// Charge CPU instructions.
    pub fn charge_cpu(&mut self, instructions: u64) -> Result<(), BudgetError> {
        self.cpu_used = self.cpu_used.saturating_add(instructions);
        if self.cpu_used > self.limits.cpu_instructions {
            Err(BudgetError::Cpu)
        } else {
            Ok(())
        }
    }

    /// Charge memory allocation.
    pub fn charge_mem(&mut self, bytes: u64) -> Result<(), BudgetError> {
        self.mem_used = self.mem_used.saturating_add(bytes);
        if self.mem_used > self.limits.memory_bytes {
            Err(BudgetError::Memory)
        } else {
            Ok(())
        }
    }

    /// Charge read bytes.
    pub fn charge_read(&mut self, bytes: u64) -> Result<(), BudgetError> {
        self.read_bytes_used = self.read_bytes_used.saturating_add(bytes);
        if self.read_bytes_used > self.limits.read_bytes {
            Err(BudgetError::Read)
        } else {
            Ok(())
        }
    }

    /// Charge write bytes.
    pub fn charge_write(&mut self, bytes: u64) -> Result<(), BudgetError> {
        self.write_bytes_used = self.write_bytes_used.saturating_add(bytes);
        if self.write_bytes_used > self.limits.write_bytes {
            Err(BudgetError::Write)
        } else {
            Ok(())
        }
    }

    /// Check if the budget is exhausted.
    pub fn is_exhausted(&self) -> bool {
        self.cpu_used > self.limits.cpu_instructions
            || self.mem_used > self.limits.memory_bytes
            || self.read_bytes_used > self.limits.read_bytes
            || self.write_bytes_used > self.limits.write_bytes
    }

    /// Get remaining CPU instructions.
    pub fn remaining_cpu(&self) -> u64 {
        self.limits.cpu_instructions.saturating_sub(self.cpu_used)
    }

    /// Get remaining memory bytes.
    pub fn remaining_mem(&self) -> u64 {
        self.limits.memory_bytes.saturating_sub(self.mem_used)
    }

    /// Reset the budget.
    pub fn reset(&mut self) {
        self.cpu_used = 0;
        self.mem_used = 0;
        self.read_bytes_used = 0;
        self.write_bytes_used = 0;
    }
}

/// Budget tracking error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BudgetError {
    /// CPU instruction limit exceeded.
    Cpu,
    /// Memory limit exceeded.
    Memory,
    /// Read bytes limit exceeded.
    Read,
    /// Write bytes limit exceeded.
    Write,
}

impl std::fmt::Display for BudgetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cpu => write!(f, "CPU instruction limit exceeded"),
            Self::Memory => write!(f, "memory limit exceeded"),
            Self::Read => write!(f, "read bytes limit exceeded"),
            Self::Write => write!(f, "write bytes limit exceeded"),
        }
    }
}

impl std::error::Error for BudgetError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_budget_cpu_tracking() {
        let mut budget = SorobanBudget::new(ResourceLimits {
            cpu_instructions: 1000,
            ..Default::default()
        });

        assert!(budget.charge_cpu(500).is_ok());
        assert_eq!(budget.cpu_used, 500);
        assert_eq!(budget.remaining_cpu(), 500);

        assert!(budget.charge_cpu(500).is_ok());
        assert_eq!(budget.cpu_used, 1000);

        assert!(budget.charge_cpu(1).is_err());
    }

    #[test]
    fn test_budget_exhausted() {
        let mut budget = SorobanBudget::new(ResourceLimits {
            cpu_instructions: 100,
            ..Default::default()
        });

        assert!(!budget.is_exhausted());
        let _ = budget.charge_cpu(200);
        assert!(budget.is_exhausted());
    }

    /// Test memory budget tracking.
    #[test]
    fn test_budget_memory_tracking() {
        let mut budget = SorobanBudget::new(ResourceLimits {
            memory_bytes: 1024,
            ..Default::default()
        });

        assert!(budget.charge_mem(512).is_ok());
        assert_eq!(budget.mem_used, 512);
        assert_eq!(budget.remaining_mem(), 512);

        assert!(budget.charge_mem(512).is_ok());
        assert_eq!(budget.mem_used, 1024);
        assert_eq!(budget.remaining_mem(), 0);

        // Exceeds limit
        let result = budget.charge_mem(1);
        assert!(matches!(result, Err(BudgetError::Memory)));
    }

    /// Test read bytes budget tracking.
    #[test]
    fn test_budget_read_tracking() {
        let mut budget = SorobanBudget::new(ResourceLimits {
            read_bytes: 200,
            ..Default::default()
        });

        assert!(budget.charge_read(100).is_ok());
        assert_eq!(budget.read_bytes_used, 100);

        assert!(budget.charge_read(100).is_ok());
        assert_eq!(budget.read_bytes_used, 200);

        let result = budget.charge_read(1);
        assert!(matches!(result, Err(BudgetError::Read)));
    }

    /// Test write bytes budget tracking.
    #[test]
    fn test_budget_write_tracking() {
        let mut budget = SorobanBudget::new(ResourceLimits {
            write_bytes: 150,
            ..Default::default()
        });

        assert!(budget.charge_write(75).is_ok());
        assert_eq!(budget.write_bytes_used, 75);

        assert!(budget.charge_write(75).is_ok());
        assert_eq!(budget.write_bytes_used, 150);

        let result = budget.charge_write(1);
        assert!(matches!(result, Err(BudgetError::Write)));
    }

    /// Test budget reset.
    #[test]
    fn test_budget_reset() {
        let mut budget = SorobanBudget::new(ResourceLimits::default());

        budget.cpu_used = 1000;
        budget.mem_used = 2000;
        budget.read_bytes_used = 3000;
        budget.write_bytes_used = 4000;

        budget.reset();

        assert_eq!(budget.cpu_used, 0);
        assert_eq!(budget.mem_used, 0);
        assert_eq!(budget.read_bytes_used, 0);
        assert_eq!(budget.write_bytes_used, 0);
    }

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

    /// Test ResourceLimits default values.
    #[test]
    fn test_resource_limits_default() {
        let limits = ResourceLimits::default();

        assert_eq!(limits.cpu_instructions, 100_000_000);
        assert_eq!(limits.memory_bytes, 64 * 1024 * 1024);
        assert_eq!(limits.read_bytes, 200 * 1024);
        assert_eq!(limits.write_bytes, 65 * 1024);
        assert_eq!(limits.read_entries, 40);
        assert_eq!(limits.write_entries, 25);
    }

    /// Test is_exhausted with multiple resource types.
    #[test]
    fn test_budget_exhausted_multiple_resources() {
        let limits = ResourceLimits {
            cpu_instructions: 100,
            memory_bytes: 100,
            read_bytes: 100,
            write_bytes: 100,
            read_entries: 10,
            write_entries: 10,
        };
        let mut budget = SorobanBudget::new(limits);

        assert!(!budget.is_exhausted());

        // Memory exhausted
        budget.mem_used = 200;
        assert!(budget.is_exhausted());
        budget.mem_used = 0;

        // Read exhausted
        budget.read_bytes_used = 200;
        assert!(budget.is_exhausted());
        budget.read_bytes_used = 0;

        // Write exhausted
        budget.write_bytes_used = 200;
        assert!(budget.is_exhausted());
    }
}
