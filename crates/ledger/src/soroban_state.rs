//! In-memory Soroban state management for fast contract data access.
//!
//! This module provides an efficient in-memory cache for Soroban contract data
//! and code entries, with co-located TTL information to avoid redundant lookups.
//!
//! # Design Overview
//!
//! The [`InMemorySorobanState`] struct maintains three storage structures:
//!
//! - **Contract data entries**: Maps key hash -> (entry, TTL)
//! - **Contract code entries**: Maps key hash -> (entry, TTL, size)
//! - **Pending TTLs**: Temporary buffer for TTLs that arrive before their entries
//!
//! # TTL Co-location
//!
//! Unlike the bucket list where TTL entries are stored separately, this cache
//! embeds TTL data directly with each entry. This:
//!
//! - Reduces memory footprint (no duplicate key storage)
//! - Enables single-lookup operations (get entry + TTL together)
//! - Simplifies rent calculations during transaction execution
//!
//! # Size Tracking
//!
//! For Protocol 23+ rent calculations, the cache tracks:
//!
//! - **Contract data state size**: Sum of XDR sizes for all data entries
//! - **Contract code state size**: Sum of in-memory compiled module sizes
//!
//! # Initialization
//!
//! The cache is initialized from a bucket list snapshot during catchup or
//! restart. Entries may arrive in any order during this process, so the
//! `pending_ttls` map temporarily holds TTLs until their corresponding
//! data/code entries arrive.
//!
//! # Thread Safety
//!
//! The state is wrapped in `RwLock` at the manager level, allowing:
//!
//! - Concurrent reads during transaction execution
//! - Exclusive writes during ledger close/commit

use parking_lot::RwLock;
use std::collections::HashMap;
use std::sync::Arc;

use henyey_common::protocol::{
    protocol_version_is_before, protocol_version_starts_from, ProtocolVersion,
};
use henyey_common::Hash256;
use henyey_tx::operations::execute::entry_size_for_rent_by_protocol_with_cost_params;
use henyey_tx::soroban::convert::{
    try_convert_cost_params_ws_to_p25, try_convert_ledger_entry_ws_to_p25,
};
use soroban_env_host25::budget::Budget;
use soroban_env_host25::e2e_invoke::entry_size_for_rent as entry_size_for_rent_p25;
use soroban_env_host_p25 as soroban_env_host25;
use stellar_xdr::curr::{
    ConfigSettingId, ContractCostParams, Hash, LedgerEntry, LedgerEntryData, LedgerKey,
    LedgerKeyConfigSetting, LedgerKeyContractCode, LedgerKeyContractData, LedgerKeyTtl, Limits,
    TtlEntry, WriteXdr,
};
use tracing::{debug, trace};

use crate::{LedgerError, Result};

// convert_ledger_entry_to_p25 has been removed after XDR alignment.
// The workspace stellar-xdr 25.0.0 and soroban-env-host P25's stellar-xdr 25.0.0
// are the same crate, so LedgerEntry types are identical.

/// TTL data co-located with contract entries.
///
/// This structure stores the essential TTL information alongside
/// contract data/code entries, avoiding the need to store separate
/// TTL entries.
#[derive(Debug, Clone, Copy, Default)]
pub struct TtlData {
    /// The ledger sequence number when this entry expires.
    pub live_until_ledger_seq: u32,
    /// The ledger sequence number when this entry was last modified.
    pub last_modified_ledger_seq: u32,
}

impl TtlData {
    /// Create new TTL data.
    pub fn new(live_until: u32, last_modified: u32) -> Self {
        Self {
            live_until_ledger_seq: live_until,
            last_modified_ledger_seq: last_modified,
        }
    }

    /// Check if the entry has expired at the given ledger sequence.
    pub fn is_expired(&self, current_ledger_seq: u32) -> bool {
        self.live_until_ledger_seq < current_ledger_seq
    }

    /// Check if TTL data is initialized (non-default).
    pub fn is_initialized(&self) -> bool {
        self.live_until_ledger_seq > 0
    }
}

/// Minimal Soroban config needed for rent size calculations.
#[derive(Debug, Clone)]
pub struct SorobanRentConfig {
    pub cpu_cost_params: ContractCostParams,
    pub mem_cost_params: ContractCostParams,
    pub tx_max_instructions: u64,
    pub tx_max_memory_bytes: u64,
}

impl SorobanRentConfig {
    pub fn has_valid_cost_params(&self) -> bool {
        !self.cpu_cost_params.0.is_empty() && !self.mem_cost_params.0.is_empty()
    }
}

impl Default for SorobanRentConfig {
    fn default() -> Self {
        Self {
            cpu_cost_params: ContractCostParams(vec![].try_into().unwrap_or_default()),
            mem_cost_params: ContractCostParams(vec![].try_into().unwrap_or_default()),
            tx_max_instructions: 0,
            tx_max_memory_bytes: 0,
        }
    }
}

// Local conversion functions removed — use henyey_tx::soroban::convert::try_convert_* instead.

fn build_rent_budget(rent_config: Option<&SorobanRentConfig>) -> Budget {
    let Some(config) = rent_config else {
        return Budget::default();
    };
    if !config.has_valid_cost_params() {
        return Budget::default();
    }

    let instruction_limit = config.tx_max_instructions.saturating_mul(2);
    let memory_limit = config.tx_max_memory_bytes.saturating_mul(2);
    let cpu_params = match try_convert_cost_params_ws_to_p25(&config.cpu_cost_params) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!("build_rent_budget: {e}, using default budget");
            return Budget::default();
        }
    };
    let mem_params = match try_convert_cost_params_ws_to_p25(&config.mem_cost_params) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!("build_rent_budget: {e}, using default budget");
            return Budget::default();
        }
    };
    Budget::try_from_configs(instruction_limit, memory_limit, cpu_params, mem_params)
        .unwrap_or_else(|_| Budget::default())
}

/// A contract data entry with co-located TTL.
#[derive(Debug, Clone)]
pub struct ContractDataMapEntry {
    /// The contract data ledger entry (immutable).
    pub ledger_entry: Arc<LedgerEntry>,
    /// TTL data co-located with the entry.
    pub ttl_data: TtlData,
}

impl ContractDataMapEntry {
    /// Get the XDR size of this entry.
    pub fn xdr_size(&self) -> u32 {
        self.ledger_entry
            .to_xdr(Limits::none())
            .map(|v| v.len() as u32)
            .unwrap_or_else(|e| {
                tracing::error!("ContractDataMapEntry::xdr_size: failed to serialize: {e}");
                0
            })
    }
}

/// A contract code entry with co-located TTL and size tracking.
#[derive(Debug, Clone)]
pub struct ContractCodeMapEntry {
    /// The contract code ledger entry (immutable).
    pub ledger_entry: Arc<LedgerEntry>,
    /// TTL data co-located with the entry.
    pub ttl_data: TtlData,
    /// In-memory size of the compiled module (for rent calculations).
    ///
    /// Protocol 23+ uses this for rent fees, which may differ from XDR size
    /// due to compilation overhead.
    pub size_bytes: u32,
}

impl ContractCodeMapEntry {
    /// Get the XDR size of this entry.
    pub fn xdr_size(&self) -> u32 {
        self.ledger_entry
            .to_xdr(Limits::none())
            .map(|v| v.len() as u32)
            .unwrap_or_else(|e| {
                tracing::error!("ContractCodeMapEntry::xdr_size: failed to serialize: {e}");
                0
            })
    }
}

/// In-memory cache for Soroban contract state.
///
/// This cache provides fast access to contract data and code entries with
/// co-located TTL information. It's used during transaction execution to
/// avoid repeated bucket list lookups.
///
/// # Invariants
///
/// - `pending_ttls` must be empty after initialization and after each update
/// - `last_closed_ledger_seq` must match the ledger manager's tracking
/// - Size counters must match the sum of individual entry sizes
pub struct InMemorySorobanState {
    /// Contract data entries indexed by TTL key hash.
    ///
    /// Uses the TTL key hash as index to enable lookup by either
    /// CONTRACT_DATA key or TTL key without key duplication.
    ///
    /// Wrapped in `Arc` for O(1) snapshot creation via `Arc::clone`.
    /// Mutations use `Arc::make_mut` for copy-on-write semantics.
    contract_data_entries: Arc<HashMap<Hash, ContractDataMapEntry>>,

    /// Contract code entries indexed by TTL key hash.
    ///
    /// Wrapped in `Arc` for O(1) snapshot creation (see `contract_data_entries`).
    contract_code_entries: Arc<HashMap<Hash, ContractCodeMapEntry>>,

    /// ConfigSetting entries indexed by ConfigSettingId.
    ///
    /// These are cached for fast access during ledger close, avoiding
    /// repeated bucket list lookups for Soroban config (cost params, limits, etc.).
    config_settings: HashMap<ConfigSettingId, Arc<LedgerEntry>>,

    /// Pending TTLs waiting for their entries.
    ///
    /// During initialization, TTL entries may arrive before their corresponding
    /// data/code entries. They're stored here temporarily until adopted.
    pending_ttls: HashMap<Hash, TtlData>,

    /// Last closed ledger sequence number.
    last_closed_ledger_seq: u32,

    /// Cumulative XDR size of all contract data entries.
    contract_data_state_size: i64,

    /// Cumulative in-memory size of all contract code entries.
    contract_code_state_size: i64,
}

impl Default for InMemorySorobanState {
    fn default() -> Self {
        Self::new()
    }
}

impl InMemorySorobanState {
    /// Create a new empty in-memory Soroban state.
    pub fn new() -> Self {
        Self {
            contract_data_entries: Arc::new(HashMap::new()),
            contract_code_entries: Arc::new(HashMap::new()),
            config_settings: HashMap::new(),
            pending_ttls: HashMap::new(),
            last_closed_ledger_seq: 0,
            contract_data_state_size: 0,
            contract_code_state_size: 0,
        }
    }

    /// Check if the state is empty.
    pub fn is_empty(&self) -> bool {
        self.contract_data_entries.is_empty()
            && self.contract_code_entries.is_empty()
            && self.config_settings.is_empty()
            && self.pending_ttls.is_empty()
    }

    /// Create a frozen, point-in-time clone of this state for snapshot lookups.
    ///
    /// The clone shares map data with the original via `Arc`, making this O(1)
    /// instead of O(n). The first mutation after a snapshot uses copy-on-write
    /// (`Arc::make_mut`) to detach the live state from the frozen snapshot.
    ///
    /// The snapshot is used by `create_snapshot()` to ensure that Soroban entry
    /// lookups return data consistent with the header captured at the same
    /// instant, preventing a race where a concurrent `commit()` updates entries
    /// before the header is published.
    pub fn snapshot(&self) -> Self {
        Self {
            contract_data_entries: Arc::clone(&self.contract_data_entries),
            contract_code_entries: Arc::clone(&self.contract_code_entries),
            config_settings: self.config_settings.clone(),
            pending_ttls: HashMap::new(),
            last_closed_ledger_seq: self.last_closed_ledger_seq,
            contract_data_state_size: self.contract_data_state_size,
            contract_code_state_size: self.contract_code_state_size,
        }
    }

    /// Get the last closed ledger sequence number.
    pub fn ledger_seq(&self) -> u32 {
        self.last_closed_ledger_seq
    }

    /// Set the last closed ledger sequence number.
    ///
    /// This is used during initialization when populating state from a bucket list
    /// checkpoint without going through `update_state()`.
    pub fn set_last_closed_ledger_seq(&mut self, ledger_seq: u32) {
        self.last_closed_ledger_seq = ledger_seq;
    }

    /// Get the total state size (data + code).
    pub fn total_size(&self) -> u64 {
        (self.contract_data_state_size + self.contract_code_state_size) as u64
    }

    /// Get the number of contract data entries.
    pub fn contract_data_count(&self) -> usize {
        self.contract_data_entries.len()
    }

    /// Get the number of contract code entries.
    pub fn contract_code_count(&self) -> usize {
        self.contract_code_entries.len()
    }

    /// Get the total contract data state size in bytes.
    pub fn contract_data_state_size(&self) -> i64 {
        self.contract_data_state_size
    }

    /// Get the total contract code state size in bytes.
    pub fn contract_code_state_size(&self) -> i64 {
        self.contract_code_state_size
    }

    /// Check if a key type should be stored in memory.
    ///
    /// Matches stellar-core's `isInMemoryType()` which only returns true for
    /// CONTRACT_DATA, CONTRACT_CODE, and TTL. ConfigSetting entries are NOT
    /// stored in the in-memory Soroban state — they are always read from the
    /// database.
    pub fn is_in_memory_type(key: &LedgerKey) -> bool {
        matches!(
            key,
            LedgerKey::ContractData(_) | LedgerKey::ContractCode(_) | LedgerKey::Ttl(_)
        )
    }

    /// Compute the TTL key hash for a CONTRACT_DATA key.
    pub fn contract_data_key_hash(key: &LedgerKeyContractData) -> Hash {
        Hash(*Hash256::hash_xdr(&LedgerKey::ContractData(key.clone())).as_bytes())
    }

    /// Compute the TTL key hash for a CONTRACT_CODE key.
    pub fn contract_code_key_hash(key: &LedgerKeyContractCode) -> Hash {
        Hash(*Hash256::hash_xdr(&LedgerKey::ContractCode(key.clone())).as_bytes())
    }

    /// Compute the map key from a TTL key.
    pub fn ttl_key_to_map_key(key: &LedgerKeyTtl) -> Hash {
        key.key_hash.clone()
    }

    /// Get a contract data entry by key.
    pub fn get_contract_data(&self, key: &LedgerKeyContractData) -> Option<&ContractDataMapEntry> {
        let key_hash = Self::contract_data_key_hash(key);
        self.contract_data_entries.get(&key_hash)
    }

    /// Get a contract code entry by key.
    pub fn get_contract_code(&self, key: &LedgerKeyContractCode) -> Option<&ContractCodeMapEntry> {
        let key_hash = Self::contract_code_key_hash(key);
        self.contract_code_entries.get(&key_hash)
    }

    /// Get a ledger entry by key.
    ///
    /// Returns the entry if found, along with synthesized TTL data.
    pub fn get(&self, key: &LedgerKey) -> Option<Arc<LedgerEntry>> {
        match key {
            LedgerKey::ContractData(cd) => {
                self.get_contract_data(cd).map(|e| e.ledger_entry.clone())
            }
            LedgerKey::ContractCode(cc) => {
                self.get_contract_code(cc).map(|e| e.ledger_entry.clone())
            }
            LedgerKey::Ttl(ttl) => self.get_ttl_entry(ttl),
            LedgerKey::ConfigSetting(cs) => self.get_config_setting(cs),
            _ => None,
        }
    }

    /// Get a ConfigSetting entry by key.
    pub fn get_config_setting(&self, key: &LedgerKeyConfigSetting) -> Option<Arc<LedgerEntry>> {
        self.config_settings.get(&key.config_setting_id).cloned()
    }

    /// Get TTL data for a key.
    pub fn get_ttl(&self, key: &LedgerKey) -> Option<TtlData> {
        match key {
            LedgerKey::ContractData(cd) => self.get_contract_data(cd).map(|e| e.ttl_data),
            LedgerKey::ContractCode(cc) => self.get_contract_code(cc).map(|e| e.ttl_data),
            LedgerKey::Ttl(ttl) => self
                .contract_data_entries
                .get(&ttl.key_hash)
                .map(|e| e.ttl_data)
                .or_else(|| {
                    self.contract_code_entries
                        .get(&ttl.key_hash)
                        .map(|e| e.ttl_data)
                }),
            _ => None,
        }
    }

    /// Synthesize a TTL entry from stored TTL data.
    fn get_ttl_entry(&self, key: &LedgerKeyTtl) -> Option<Arc<LedgerEntry>> {
        // Look up in both maps
        let ttl_data = self
            .contract_data_entries
            .get(&key.key_hash)
            .map(|e| e.ttl_data)
            .or_else(|| {
                self.contract_code_entries
                    .get(&key.key_hash)
                    .map(|e| e.ttl_data)
            })?;

        // Synthesize the TTL entry
        let entry = LedgerEntry {
            last_modified_ledger_seq: ttl_data.last_modified_ledger_seq,
            data: LedgerEntryData::Ttl(TtlEntry {
                key_hash: key.key_hash.clone(),
                live_until_ledger_seq: ttl_data.live_until_ledger_seq,
            }),
            ext: stellar_xdr::curr::LedgerEntryExt::V0,
        };

        Some(Arc::new(entry))
    }

    /// Check if a TTL exists for a key.
    pub fn has_ttl(&self, key: &LedgerKeyTtl) -> bool {
        self.contract_data_entries.contains_key(&key.key_hash)
            || self.contract_code_entries.contains_key(&key.key_hash)
    }

    /// Create a new contract data entry.
    ///
    /// # Errors
    ///
    /// Returns an error if the entry already exists.
    pub fn create_contract_data(&mut self, entry: LedgerEntry) -> Result<()> {
        let key = match &entry.data {
            LedgerEntryData::ContractData(cd) => LedgerKeyContractData {
                contract: cd.contract.clone(),
                key: cd.key.clone(),
                durability: cd.durability,
            },
            _ => {
                return Err(LedgerError::InvalidEntry(
                    "not a contract data entry".into(),
                ))
            }
        };

        let key_hash = Self::contract_data_key_hash(&key);

        if self.contract_data_entries.contains_key(&key_hash) {
            return Err(LedgerError::InvalidEntry(
                "contract data already exists".into(),
            ));
        }

        // Check for pending TTL
        let ttl_data = self.pending_ttls.remove(&key_hash).unwrap_or_default();

        let map_entry = ContractDataMapEntry {
            ledger_entry: Arc::new(entry),
            ttl_data,
        };

        self.contract_data_state_size += map_entry.xdr_size() as i64;
        Arc::make_mut(&mut self.contract_data_entries).insert(key_hash, map_entry);

        trace!("Created contract data entry");
        Ok(())
    }

    /// Update an existing contract data entry.
    ///
    /// Preserves the existing TTL while updating the data.
    ///
    /// # Errors
    ///
    /// Returns an error if the entry doesn't exist.
    pub fn update_contract_data(&mut self, entry: LedgerEntry) -> Result<()> {
        let key = match &entry.data {
            LedgerEntryData::ContractData(cd) => LedgerKeyContractData {
                contract: cd.contract.clone(),
                key: cd.key.clone(),
                durability: cd.durability,
            },
            _ => {
                return Err(LedgerError::InvalidEntry(
                    "not a contract data entry".into(),
                ))
            }
        };

        let key_hash = Self::contract_data_key_hash(&key);

        let map = Arc::make_mut(&mut self.contract_data_entries);
        let old_entry = map
            .remove(&key_hash)
            .ok_or_else(|| LedgerError::InvalidEntry("contract data does not exist".into()))?;

        // Update size tracking
        let old_size = old_entry.xdr_size();
        let new_entry = ContractDataMapEntry {
            ledger_entry: Arc::new(entry),
            ttl_data: old_entry.ttl_data, // Preserve TTL
        };
        let new_size = new_entry.xdr_size();

        self.contract_data_state_size += (new_size as i64) - (old_size as i64);
        map.insert(key_hash, new_entry);

        trace!("Updated contract data entry");
        Ok(())
    }

    /// Delete a contract data entry.
    ///
    /// # Errors
    ///
    /// Returns an error if the entry doesn't exist.
    pub fn delete_contract_data(&mut self, key: &LedgerKeyContractData) -> Result<()> {
        let key_hash = Self::contract_data_key_hash(key);

        let old_entry = Arc::make_mut(&mut self.contract_data_entries)
            .remove(&key_hash)
            .ok_or_else(|| LedgerError::InvalidEntry("contract data does not exist".into()))?;

        self.contract_data_state_size -= old_entry.xdr_size() as i64;

        trace!("Deleted contract data entry");
        Ok(())
    }

    /// Create a new contract code entry.
    ///
    /// # Arguments
    ///
    /// * `entry` - The contract code ledger entry
    /// * `protocol_version` - Current protocol version for size calculation
    ///
    /// # Errors
    ///
    /// Returns an error if the entry already exists.
    pub fn create_contract_code(
        &mut self,
        entry: LedgerEntry,
        protocol_version: u32,
        rent_config: Option<&SorobanRentConfig>,
    ) -> Result<()> {
        let key = match &entry.data {
            LedgerEntryData::ContractCode(cc) => LedgerKeyContractCode {
                hash: cc.hash.clone(),
            },
            _ => {
                return Err(LedgerError::InvalidEntry(
                    "not a contract code entry".into(),
                ))
            }
        };

        let key_hash = Self::contract_code_key_hash(&key);

        if self.contract_code_entries.contains_key(&key_hash) {
            return Err(LedgerError::InvalidEntry(
                "contract code already exists".into(),
            ));
        }

        // Check for pending TTL
        let ttl_data = self.pending_ttls.remove(&key_hash).unwrap_or_default();

        // Calculate size for rent
        let size_bytes = self.calculate_code_size(&entry, protocol_version, rent_config);

        let map_entry = ContractCodeMapEntry {
            ledger_entry: Arc::new(entry),
            ttl_data,
            size_bytes,
        };

        self.contract_code_state_size += map_entry.size_bytes as i64;
        Arc::make_mut(&mut self.contract_code_entries).insert(key_hash, map_entry);

        trace!("Created contract code entry");
        Ok(())
    }

    /// Update an existing contract code entry.
    ///
    /// Preserves the existing TTL while updating the code.
    ///
    /// # Errors
    ///
    /// Returns an error if the entry doesn't exist.
    pub fn update_contract_code(
        &mut self,
        entry: LedgerEntry,
        protocol_version: u32,
        rent_config: Option<&SorobanRentConfig>,
    ) -> Result<()> {
        let key = match &entry.data {
            LedgerEntryData::ContractCode(cc) => LedgerKeyContractCode {
                hash: cc.hash.clone(),
            },
            _ => {
                return Err(LedgerError::InvalidEntry(
                    "not a contract code entry".into(),
                ))
            }
        };

        let key_hash = Self::contract_code_key_hash(&key);

        // Calculate new size before taking mutable borrow on the map.
        let new_size = self.calculate_code_size(&entry, protocol_version, rent_config);

        let map = Arc::make_mut(&mut self.contract_code_entries);
        let old_entry = map
            .remove(&key_hash)
            .ok_or_else(|| LedgerError::InvalidEntry("contract code does not exist".into()))?;

        let new_entry = ContractCodeMapEntry {
            ledger_entry: Arc::new(entry),
            ttl_data: old_entry.ttl_data, // Preserve TTL
            size_bytes: new_size,
        };

        // Update size tracking
        self.contract_code_state_size += (new_size as i64) - (old_entry.size_bytes as i64);
        map.insert(key_hash, new_entry);

        trace!("Updated contract code entry");
        Ok(())
    }

    /// Delete a contract code entry.
    ///
    /// # Errors
    ///
    /// Returns an error if the entry doesn't exist.
    pub fn delete_contract_code(&mut self, key: &LedgerKeyContractCode) -> Result<()> {
        let key_hash = Self::contract_code_key_hash(key);

        let old_entry = Arc::make_mut(&mut self.contract_code_entries)
            .remove(&key_hash)
            .ok_or_else(|| LedgerError::InvalidEntry("contract code does not exist".into()))?;

        self.contract_code_state_size -= old_entry.size_bytes as i64;

        trace!("Deleted contract code entry");
        Ok(())
    }

    /// Create TTL for an entry.
    ///
    /// If the corresponding data/code entry exists and has no TTL yet, stores
    /// the TTL inline. Otherwise, stores in pending_ttls to be adopted later.
    pub fn create_ttl(&mut self, key: &LedgerKeyTtl, ttl_data: TtlData) -> Result<()> {
        // Try to update inline in contract data
        if let Some(entry) = Arc::make_mut(&mut self.contract_data_entries).get_mut(&key.key_hash) {
            if entry.ttl_data.is_initialized() {
                return Err(LedgerError::InvalidEntry(
                    "contract data TTL already initialized".into(),
                ));
            }
            entry.ttl_data = ttl_data;
            trace!("Updated TTL inline for contract data");
            return Ok(());
        }

        // Try to update inline in contract code
        if let Some(entry) = Arc::make_mut(&mut self.contract_code_entries).get_mut(&key.key_hash) {
            if entry.ttl_data.is_initialized() {
                return Err(LedgerError::InvalidEntry(
                    "contract code TTL already initialized".into(),
                ));
            }
            entry.ttl_data = ttl_data;
            trace!("Updated TTL inline for contract code");
            return Ok(());
        }

        // No entry found, store as pending
        if self.pending_ttls.contains_key(&key.key_hash) {
            return Err(LedgerError::InvalidEntry(
                "pending TTL already exists".into(),
            ));
        }
        self.pending_ttls.insert(key.key_hash.clone(), ttl_data);
        trace!("Stored pending TTL");
        Ok(())
    }

    /// Update TTL for an existing entry.
    ///
    /// Returns an error if the corresponding data/code entry does not exist.
    pub fn update_ttl(&mut self, key: &LedgerKeyTtl, ttl_data: TtlData) -> Result<()> {
        if let Some(entry) = Arc::make_mut(&mut self.contract_data_entries).get_mut(&key.key_hash) {
            entry.ttl_data = ttl_data;
            trace!("Updated TTL inline for contract data");
            return Ok(());
        }

        if let Some(entry) = Arc::make_mut(&mut self.contract_code_entries).get_mut(&key.key_hash) {
            entry.ttl_data = ttl_data;
            trace!("Updated TTL inline for contract code");
            return Ok(());
        }

        Err(LedgerError::InvalidEntry(
            "TTL update missing contract data/code entry".into(),
        ))
    }

    fn ttl_from_entry(&self, entry: &LedgerEntry) -> Result<(LedgerKeyTtl, TtlData)> {
        match &entry.data {
            LedgerEntryData::Ttl(ttl) => {
                let key = LedgerKeyTtl {
                    key_hash: ttl.key_hash.clone(),
                };
                let ttl_data =
                    TtlData::new(ttl.live_until_ledger_seq, entry.last_modified_ledger_seq);
                Ok((key, ttl_data))
            }
            _ => Err(LedgerError::InvalidEntry("not a TTL entry".into())),
        }
    }

    /// Process a TTL entry for initialization (create semantics).
    pub fn process_ttl_entry_create(&mut self, entry: &LedgerEntry) -> Result<()> {
        let (key, ttl_data) = self.ttl_from_entry(entry)?;
        self.create_ttl(&key, ttl_data)
    }

    /// Process a TTL entry for updates (update semantics).
    pub fn process_ttl_entry_update(&mut self, entry: &LedgerEntry) -> Result<()> {
        let (key, ttl_data) = self.ttl_from_entry(entry)?;
        self.update_ttl(&key, ttl_data)
    }

    /// Calculate the in-memory size for a contract code entry.
    fn calculate_code_size(
        &self,
        entry: &LedgerEntry,
        protocol_version: u32,
        rent_config: Option<&SorobanRentConfig>,
    ) -> u32 {
        let xdr_size = entry
            .to_xdr(Limits::none())
            .map(|v| v.len() as u32)
            .unwrap_or_else(|e| {
                tracing::error!("calculate_code_size: failed to serialize LedgerEntry: {e}");
                0
            });
        if protocol_version_is_before(protocol_version, ProtocolVersion::V25) {
            let cost_params = rent_config.map(|rc| (&rc.cpu_cost_params, &rc.mem_cost_params));
            return entry_size_for_rent_by_protocol_with_cost_params(
                protocol_version,
                entry,
                xdr_size,
                cost_params,
            );
        }
        let budget = build_rent_budget(rent_config);
        match try_convert_ledger_entry_ws_to_p25(entry) {
            Ok(p25_entry) => {
                entry_size_for_rent_p25(&budget, &p25_entry, xdr_size).unwrap_or(xdr_size)
            }
            Err(e) => {
                tracing::warn!("calculate_code_size: {e}, falling back to XDR size");
                xdr_size
            }
        }
    }

    /// Update state with new entries from a ledger close.
    ///
    /// # Arguments
    ///
    /// * `ledger_seq` - The new ledger sequence number
    /// * `init_entries` - Newly created entries
    /// * `live_entries` - Updated entries
    /// * `dead_entries` - Deleted entry keys
    /// * `protocol_version` - Current protocol version
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The ledger sequence is not exactly one more than the current
    /// - Entry operations fail
    pub fn update_state(
        &mut self,
        ledger_seq: u32,
        init_entries: &[LedgerEntry],
        live_entries: &[LedgerEntry],
        dead_entries: &[LedgerKey],
        protocol_version: u32,
        rent_config: Option<&SorobanRentConfig>,
    ) -> Result<()> {
        // Validate sequence progression
        if self.last_closed_ledger_seq > 0 && ledger_seq != self.last_closed_ledger_seq + 1 {
            return Err(LedgerError::InvalidSequence {
                expected: self.last_closed_ledger_seq + 1,
                actual: ledger_seq,
            });
        }

        // Process init entries (creates)
        for entry in init_entries {
            self.process_entry_create(entry, protocol_version, rent_config)?;
        }

        // Process live entries (updates)
        for entry in live_entries {
            self.process_entry_update(entry, protocol_version, rent_config)?;
        }

        // Process dead entries (deletes)
        for key in dead_entries {
            self.process_entry_delete(key)?;
        }

        self.last_closed_ledger_seq = ledger_seq;

        // Check invariant: pending_ttls should be empty after each update
        if !self.pending_ttls.is_empty() {
            // Log which pending TTLs remain for debugging
            for (key_hash, ttl_data) in &self.pending_ttls {
                tracing::error!(
                    key_hash = %format!("{:02x?}", &key_hash.0[..8]),
                    live_until = ttl_data.live_until_ledger_seq,
                    "Remaining pending TTL"
                );
            }
            return Err(LedgerError::InvalidEntry(format!(
                "pending TTLs not empty after update: {} remaining",
                self.pending_ttls.len()
            )));
        }

        debug!(
            "Updated Soroban state to ledger {}: {} data, {} code entries",
            ledger_seq,
            self.contract_data_entries.len(),
            self.contract_code_entries.len()
        );

        Ok(())
    }

    /// Process a single entry creation.
    pub fn process_entry_create(
        &mut self,
        entry: &LedgerEntry,
        protocol_version: u32,
        rent_config: Option<&SorobanRentConfig>,
    ) -> Result<()> {
        match &entry.data {
            LedgerEntryData::ContractData(_) => self.create_contract_data(entry.clone()),
            LedgerEntryData::ContractCode(_) => {
                self.create_contract_code(entry.clone(), protocol_version, rent_config)
            }
            LedgerEntryData::Ttl(_) => self.process_ttl_entry_create(entry),
            LedgerEntryData::ConfigSetting(cs) => {
                self.config_settings
                    .insert(cs.discriminant(), Arc::new(entry.clone()));
                Ok(())
            }
            _ => Ok(()), // Ignore non-Soroban entries
        }
    }

    /// Process a single entry update.
    ///
    /// Parity: stellar-core's updateContractData / updateContractCode assert
    /// that the entry already exists (`releaseAssertOrThrow`). We mirror that
    /// by returning an error when the entry is missing instead of silently
    /// creating it.
    pub fn process_entry_update(
        &mut self,
        entry: &LedgerEntry,
        protocol_version: u32,
        rent_config: Option<&SorobanRentConfig>,
    ) -> Result<()> {
        match &entry.data {
            LedgerEntryData::ContractData(_) => self.update_contract_data(entry.clone()),
            LedgerEntryData::ContractCode(_) => {
                self.update_contract_code(entry.clone(), protocol_version, rent_config)
            }
            LedgerEntryData::Ttl(_) => self.process_ttl_entry_update(entry),
            LedgerEntryData::ConfigSetting(cs) => {
                self.config_settings
                    .insert(cs.discriminant(), Arc::new(entry.clone()));
                Ok(())
            }
            _ => Ok(()), // Ignore non-Soroban entries
        }
    }

    /// Process a single entry deletion.
    ///
    /// Parity: stellar-core's deleteContractData / deleteContractCode assert
    /// that the entry exists (`releaseAssertOrThrow`). We propagate the error
    /// rather than silently ignoring missing entries.
    pub fn process_entry_delete(&mut self, key: &LedgerKey) -> Result<()> {
        match key {
            LedgerKey::ContractData(cd) => self.delete_contract_data(cd),
            LedgerKey::ContractCode(cc) => self.delete_contract_code(cc),
            LedgerKey::Ttl(ttl) => {
                // TTL deletion is handled implicitly when data/code is deleted
                self.pending_ttls.remove(&ttl.key_hash);
                Ok(())
            }
            _ => Ok(()), // Ignore non-Soroban entries
        }
    }

    /// Recompute all contract code sizes.
    ///
    /// Called after a protocol upgrade or config change that affects
    /// compiled module sizing (e.g. ContractCostParamsMemoryBytes upgrade).
    ///
    /// Parity: InMemorySorobanState.cpp:562 recomputeContractCodeSize
    pub fn recompute_contract_code_sizes(
        &mut self,
        protocol_version: u32,
        rent_config: Option<&SorobanRentConfig>,
    ) {
        let mut total_size: i64 = 0;

        // Build the budget once outside the loop for efficiency
        let budget = build_rent_budget(rent_config);

        for entry in Arc::make_mut(&mut self.contract_code_entries).values_mut() {
            let xdr_size = entry
                .ledger_entry
                .to_xdr(Limits::none())
                .map(|v| v.len() as u32)
                .unwrap_or_else(|e| {
                    tracing::error!("recompute_contract_code_sizes: failed to serialize: {e}");
                    0
                });

            // Use the same logic as calculate_code_size
            let new_size = if protocol_version_starts_from(protocol_version, ProtocolVersion::V25) {
                match try_convert_ledger_entry_ws_to_p25(&entry.ledger_entry) {
                    Ok(p25_entry) => {
                        entry_size_for_rent_p25(&budget, &p25_entry, xdr_size).unwrap_or(xdr_size)
                    }
                    Err(e) => {
                        tracing::warn!(
                            "recompute_contract_code_sizes: {e}, falling back to XDR size"
                        );
                        xdr_size
                    }
                }
            } else {
                let cost_params = rent_config.map(|rc| (&rc.cpu_cost_params, &rc.mem_cost_params));
                entry_size_for_rent_by_protocol_with_cost_params(
                    protocol_version,
                    &entry.ledger_entry,
                    xdr_size,
                    cost_params,
                )
            };

            entry.size_bytes = new_size;
            total_size += new_size as i64;
        }

        self.contract_code_state_size = total_size;

        debug!(
            "Recomputed contract code sizes for {} entries: {} bytes",
            self.contract_code_entries.len(),
            total_size
        );
    }

    /// Clear all state.
    pub fn clear(&mut self) {
        Arc::make_mut(&mut self.contract_data_entries).clear();
        Arc::make_mut(&mut self.contract_code_entries).clear();
        self.pending_ttls.clear();
        self.last_closed_ledger_seq = 0;
        self.contract_data_state_size = 0;
        self.contract_code_state_size = 0;
    }

    /// Get the number of config setting entries.
    pub fn config_settings_count(&self) -> usize {
        self.config_settings.len()
    }

    /// Get statistics about the current state.
    pub fn stats(&self) -> SorobanStateStats {
        SorobanStateStats {
            ledger_seq: self.last_closed_ledger_seq,
            contract_data_count: self.contract_data_entries.len(),
            contract_code_count: self.contract_code_entries.len(),
            config_settings_count: self.config_settings.len(),
            contract_data_size: self.contract_data_state_size,
            contract_code_size: self.contract_code_state_size,
            pending_ttl_count: self.pending_ttls.len(),
        }
    }

    /// Estimate heap bytes for contract data entries.
    ///
    /// Uses `contract_data_state_size` as a proxy for Arc<LedgerEntry> payload
    /// sizes plus HashMap overhead for the key-to-entry mapping.
    pub fn estimate_contract_data_heap_bytes(&self) -> usize {
        use henyey_common::memory::hashmap_heap_bytes;
        // HashMap<[u8;32], ContractDataMapEntry>
        // ContractDataMapEntry: Arc<LedgerEntry>(8) + TtlData(8) = 16 inline
        let map_bytes = hashmap_heap_bytes(
            self.contract_data_entries.capacity(),
            32,
            std::mem::size_of::<ContractDataMapEntry>(),
        );
        // Arc payload sizes tracked by contract_data_state_size
        let payload_bytes = self.contract_data_state_size.max(0) as usize;
        map_bytes + payload_bytes
    }

    /// Estimate heap bytes for contract code entries.
    pub fn estimate_contract_code_heap_bytes(&self) -> usize {
        use henyey_common::memory::hashmap_heap_bytes;
        let map_bytes = hashmap_heap_bytes(
            self.contract_code_entries.capacity(),
            32,
            std::mem::size_of::<ContractCodeMapEntry>(),
        );
        let payload_bytes = self.contract_code_state_size.max(0) as usize;
        map_bytes + payload_bytes
    }

    /// Estimate total heap bytes for all Soroban state.
    pub fn estimate_heap_bytes(&self) -> usize {
        use henyey_common::memory::hashmap_heap_bytes;
        let data = self.estimate_contract_data_heap_bytes();
        let code = self.estimate_contract_code_heap_bytes();
        // config_settings: HashMap<i32, Arc<LedgerEntry>>
        let config = hashmap_heap_bytes(
            self.config_settings.capacity(),
            std::mem::size_of::<i32>(),
            std::mem::size_of::<Arc<LedgerEntry>>(),
        );
        data + code + config
    }
}

/// Statistics about the Soroban state cache.
#[derive(Debug, Clone, Default)]
pub struct SorobanStateStats {
    /// Current ledger sequence.
    pub ledger_seq: u32,
    /// Number of contract data entries.
    pub contract_data_count: usize,
    /// Number of contract code entries.
    pub contract_code_count: usize,
    /// Number of config setting entries.
    pub config_settings_count: usize,
    /// Total contract data size in bytes.
    pub contract_data_size: i64,
    /// Total contract code size in bytes.
    pub contract_code_size: i64,
    /// Number of pending TTL entries (should be 0 after init).
    pub pending_ttl_count: usize,
}

/// Thread-safe wrapper for in-memory Soroban state.
///
/// Provides concurrent read access during transaction execution
/// and exclusive write access during ledger close.
pub struct SharedSorobanState {
    inner: RwLock<InMemorySorobanState>,
}

impl Default for SharedSorobanState {
    fn default() -> Self {
        Self::new()
    }
}

impl SharedSorobanState {
    /// Create a new shared Soroban state.
    pub fn new() -> Self {
        Self {
            inner: RwLock::new(InMemorySorobanState::new()),
        }
    }

    /// Get a read lock for concurrent access.
    pub fn read(&self) -> parking_lot::RwLockReadGuard<'_, InMemorySorobanState> {
        self.inner.read()
    }

    /// Get a write lock for exclusive access.
    pub fn write(&self) -> parking_lot::RwLockWriteGuard<'_, InMemorySorobanState> {
        self.inner.write()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use stellar_xdr::curr::{
        ContractCodeEntry, ContractCodeEntryExt, ContractDataDurability, ContractDataEntry,
        ContractId, ExtensionPoint, Hash, LedgerEntryExt, ScAddress, ScVal,
    };

    fn make_contract_address() -> ScAddress {
        ScAddress::Contract(ContractId(Hash([1u8; 32])))
    }

    fn make_contract_data_entry(key_bytes: [u8; 32]) -> LedgerEntry {
        LedgerEntry {
            last_modified_ledger_seq: 100,
            data: LedgerEntryData::ContractData(ContractDataEntry {
                ext: ExtensionPoint::V0,
                contract: make_contract_address(),
                key: ScVal::Bytes(stellar_xdr::curr::ScBytes(
                    key_bytes.to_vec().try_into().unwrap(),
                )),
                durability: ContractDataDurability::Persistent,
                val: ScVal::I32(42),
            }),
            ext: stellar_xdr::curr::LedgerEntryExt::V0,
        }
    }

    fn make_contract_code_entry(hash: [u8; 32]) -> LedgerEntry {
        LedgerEntry {
            last_modified_ledger_seq: 100,
            data: LedgerEntryData::ContractCode(ContractCodeEntry {
                ext: ContractCodeEntryExt::V0,
                hash: Hash(hash),
                code: vec![0u8; 100].try_into().unwrap(),
            }),
            ext: stellar_xdr::curr::LedgerEntryExt::V0,
        }
    }

    #[test]
    fn test_empty_state() {
        let state = InMemorySorobanState::new();
        assert!(state.is_empty());
        assert_eq!(state.ledger_seq(), 0);
        assert_eq!(state.total_size(), 0);
    }

    #[test]
    fn test_create_contract_data() {
        let mut state = InMemorySorobanState::new();
        let entry = make_contract_data_entry([1u8; 32]);

        state.create_contract_data(entry.clone()).unwrap();

        assert_eq!(state.contract_data_count(), 1);
        assert!(!state.is_empty());
        assert!(state.contract_data_state_size > 0);
    }

    #[test]
    fn test_update_contract_data() {
        let mut state = InMemorySorobanState::new();
        let entry = make_contract_data_entry([1u8; 32]);

        state.create_contract_data(entry).unwrap();
        let _initial_size = state.contract_data_state_size;

        let mut updated_entry = make_contract_data_entry([1u8; 32]);
        if let LedgerEntryData::ContractData(cd) = &mut updated_entry.data {
            cd.val = ScVal::I32(99);
        }

        state.update_contract_data(updated_entry).unwrap();

        // Size should be similar (small change in value)
        assert!(state.contract_data_state_size > 0);
        assert_eq!(state.contract_data_count(), 1);
    }

    #[test]
    fn test_delete_contract_data() {
        let mut state = InMemorySorobanState::new();
        let entry = make_contract_data_entry([1u8; 32]);

        state.create_contract_data(entry.clone()).unwrap();
        assert_eq!(state.contract_data_count(), 1);

        let key = LedgerKeyContractData {
            contract: make_contract_address(),
            key: ScVal::Bytes(stellar_xdr::curr::ScBytes(
                [1u8; 32].to_vec().try_into().unwrap(),
            )),
            durability: ContractDataDurability::Persistent,
        };

        state.delete_contract_data(&key).unwrap();

        assert_eq!(state.contract_data_count(), 0);
        assert!(state.is_empty());
        assert_eq!(state.contract_data_state_size, 0);
    }

    #[test]
    fn test_create_contract_code() {
        let mut state = InMemorySorobanState::new();
        let entry = make_contract_code_entry([2u8; 32]);

        state.create_contract_code(entry, 25, None).unwrap();

        assert_eq!(state.contract_code_count(), 1);
        assert!(state.contract_code_state_size > 0);
    }

    #[test]
    fn test_update_state() {
        let mut state = InMemorySorobanState::new();

        let data_entry = make_contract_data_entry([1u8; 32]);
        let code_entry = make_contract_code_entry([2u8; 32]);

        state
            .update_state(
                1,
                &[data_entry.clone(), code_entry.clone()],
                &[],
                &[],
                25,
                None,
            )
            .unwrap();

        assert_eq!(state.ledger_seq(), 1);
        assert_eq!(state.contract_data_count(), 1);
        assert_eq!(state.contract_code_count(), 1);
    }

    #[test]
    fn test_ttl_co_location() {
        let mut state = InMemorySorobanState::new();
        let entry = make_contract_data_entry([1u8; 32]);

        // Create entry (will have default TTL)
        state.create_contract_data(entry.clone()).unwrap();

        // Get the key hash
        let key = LedgerKeyContractData {
            contract: make_contract_address(),
            key: ScVal::Bytes(stellar_xdr::curr::ScBytes(
                [1u8; 32].to_vec().try_into().unwrap(),
            )),
            durability: ContractDataDurability::Persistent,
        };
        let key_hash = InMemorySorobanState::contract_data_key_hash(&key);

        // Update TTL
        let ttl_key = LedgerKeyTtl { key_hash };
        state.update_ttl(&ttl_key, TtlData::new(1000, 100)).unwrap();

        // Verify TTL is co-located
        let map_entry = state.get_contract_data(&key).unwrap();
        assert_eq!(map_entry.ttl_data.live_until_ledger_seq, 1000);
        assert_eq!(map_entry.ttl_data.last_modified_ledger_seq, 100);
    }

    #[test]
    fn test_pending_ttl_adoption() {
        let mut state = InMemorySorobanState::new();

        // Create TTL before the entry exists
        let key = LedgerKeyContractData {
            contract: make_contract_address(),
            key: ScVal::Bytes(stellar_xdr::curr::ScBytes(
                [1u8; 32].to_vec().try_into().unwrap(),
            )),
            durability: ContractDataDurability::Persistent,
        };
        let key_hash = InMemorySorobanState::contract_data_key_hash(&key);
        let ttl_key = LedgerKeyTtl { key_hash };

        state.create_ttl(&ttl_key, TtlData::new(2000, 200)).unwrap();
        assert_eq!(state.pending_ttls.len(), 1);

        // Now create the entry
        let entry = make_contract_data_entry([1u8; 32]);
        state.create_contract_data(entry).unwrap();

        // Pending TTL should be adopted
        assert!(state.pending_ttls.is_empty());

        // Verify TTL was adopted
        let map_entry = state.get_contract_data(&key).unwrap();
        assert_eq!(map_entry.ttl_data.live_until_ledger_seq, 2000);
    }

    #[test]
    fn test_synthesize_ttl_entry() {
        let mut state = InMemorySorobanState::new();
        let entry = make_contract_data_entry([1u8; 32]);

        state.create_contract_data(entry).unwrap();

        // Set TTL
        let key = LedgerKeyContractData {
            contract: make_contract_address(),
            key: ScVal::Bytes(stellar_xdr::curr::ScBytes(
                [1u8; 32].to_vec().try_into().unwrap(),
            )),
            durability: ContractDataDurability::Persistent,
        };
        let key_hash = InMemorySorobanState::contract_data_key_hash(&key);
        let ttl_key = LedgerKeyTtl { key_hash };

        state.update_ttl(&ttl_key, TtlData::new(3000, 300)).unwrap();

        // Get synthesized TTL entry
        let ttl_entry = state.get_ttl_entry(&ttl_key).unwrap();
        if let LedgerEntryData::Ttl(ttl) = &ttl_entry.data {
            assert_eq!(ttl.live_until_ledger_seq, 3000);
            assert_eq!(ttl_entry.last_modified_ledger_seq, 300);
        } else {
            panic!("Expected TTL entry");
        }
    }

    #[test]
    fn test_stats() {
        let mut state = InMemorySorobanState::new();

        let data_entry = make_contract_data_entry([1u8; 32]);
        let code_entry = make_contract_code_entry([2u8; 32]);

        state
            .update_state(10, &[data_entry, code_entry], &[], &[], 25, None)
            .unwrap();

        let stats = state.stats();
        assert_eq!(stats.ledger_seq, 10);
        assert_eq!(stats.contract_data_count, 1);
        assert_eq!(stats.contract_code_count, 1);
        assert!(stats.contract_data_size > 0);
        assert!(stats.contract_code_size > 0);
        assert_eq!(stats.pending_ttl_count, 0);
    }

    #[test]
    fn test_shared_state() {
        let shared = SharedSorobanState::new();

        {
            let mut write = shared.write();
            let entry = make_contract_data_entry([1u8; 32]);
            write.create_contract_data(entry).unwrap();
        }

        {
            let read = shared.read();
            assert_eq!(read.contract_data_count(), 1);
        }
    }

    /// Test that update_state correctly pairs TTL entries with data entries
    /// when data entries are added to init_entries (simulates RestoreFootprint
    /// from hot archive where data entries are prefetched and added to init).
    ///
    /// This is a regression test for the bug fixed in ledger 327974 TX 3 where
    /// RestoreFootprint from hot archive was failing with "pending TTLs not empty".
    #[test]
    fn test_restore_footprint_hot_archive_ttl_pairing() {
        let mut state = InMemorySorobanState::new();

        // Simulate initial state with some existing entries
        let existing_entry = make_contract_data_entry([0u8; 32]);
        state.create_contract_data(existing_entry).unwrap();

        // Create a TTL entry for the data that will be "restored" from hot archive
        let restored_key = LedgerKeyContractData {
            contract: make_contract_address(),
            key: ScVal::Bytes(stellar_xdr::curr::ScBytes(
                [42u8; 32].to_vec().try_into().unwrap(),
            )),
            durability: ContractDataDurability::Persistent,
        };
        let key_hash = InMemorySorobanState::contract_data_key_hash(&restored_key);

        // Create the TTL entry that would come from RestoreFootprint execution
        let ttl_entry = LedgerEntry {
            last_modified_ledger_seq: 100,
            data: LedgerEntryData::Ttl(TtlEntry {
                key_hash,
                live_until_ledger_seq: 500000, // Extended TTL
            }),
            ext: LedgerEntryExt::V0,
        };

        // Create the data entry that would come from hot archive prefetch
        let data_entry = LedgerEntry {
            last_modified_ledger_seq: 100,
            data: LedgerEntryData::ContractData(ContractDataEntry {
                ext: ExtensionPoint::V0,
                contract: make_contract_address(),
                key: ScVal::Bytes(stellar_xdr::curr::ScBytes(
                    [42u8; 32].to_vec().try_into().unwrap(),
                )),
                durability: ContractDataDurability::Persistent,
                val: ScVal::I32(12345),
            }),
            ext: LedgerEntryExt::V0,
        };

        // Simulate update_state with BOTH the TTL and data entry in init_entries.
        // The TTL entry may be processed before the data entry, so it should go
        // to pending_ttls and then be adopted when the data entry is created.
        // Order matters here - TTL first, then data (worst case for the bug).
        let init_entries = vec![ttl_entry, data_entry];

        state
            .update_state(100, &init_entries, &[], &[], 25, None)
            .expect("update_state should succeed - TTL should pair with data entry");

        // Verify the entry was created with correct TTL
        let map_entry = state.get_contract_data(&restored_key).unwrap();
        assert_eq!(map_entry.ttl_data.live_until_ledger_seq, 500000);
        assert_eq!(state.pending_ttls.len(), 0);
    }

    /// Regression test: Creating a ContractCode that already exists should fail.
    ///
    /// This tests the scenario from ledger 306338 where InvokeHostFunction restores
    /// ContractCode from hot archive, but the same WASM code (same hash) is already
    /// in soroban_state because another contract uses it. The caller must check for
    /// existence and use update_contract_code instead of create_contract_code.
    #[test]
    fn test_create_duplicate_contract_code_fails() {
        let mut state = InMemorySorobanState::new();

        // Create a contract code entry
        let code_hash = [42u8; 32];
        let entry = make_contract_code_entry(code_hash);
        state.create_contract_code(entry.clone(), 25, None).unwrap();

        // Attempting to create the same code again should fail
        let result = state.create_contract_code(entry, 25, None);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("contract code already exists"));
    }

    /// Test that is_in_memory_type returns false for ConfigSetting keys.
    ///
    /// stellar-core's isInMemoryType() only returns true for CONTRACT_DATA,
    /// CONTRACT_CODE, and TTL. ConfigSetting entries should NOT be routed through
    /// the in-memory Soroban state cache — they should always go to the database.
    #[test]
    fn test_config_setting_not_in_memory_type() {
        use stellar_xdr::curr::{ConfigSettingId, LedgerKeyConfigSetting};

        // ConfigSetting should NOT be an in-memory type
        let config_key = LedgerKey::ConfigSetting(LedgerKeyConfigSetting {
            config_setting_id: ConfigSettingId::ContractMaxSizeBytes,
        });
        assert!(
            !InMemorySorobanState::is_in_memory_type(&config_key),
            "ConfigSetting should NOT be an in-memory type (C++ isInMemoryType excludes it)"
        );

        // Verify the types that SHOULD be in-memory
        let contract_data_key = LedgerKey::ContractData(LedgerKeyContractData {
            contract: make_contract_address(),
            key: ScVal::I32(0),
            durability: ContractDataDurability::Persistent,
        });
        assert!(InMemorySorobanState::is_in_memory_type(&contract_data_key));

        let contract_code_key = LedgerKey::ContractCode(LedgerKeyContractCode {
            hash: Hash([0u8; 32]),
        });
        assert!(InMemorySorobanState::is_in_memory_type(&contract_code_key));

        let ttl_key = LedgerKey::Ttl(LedgerKeyTtl {
            key_hash: Hash([0u8; 32]),
        });
        assert!(InMemorySorobanState::is_in_memory_type(&ttl_key));

        // Account should also not be an in-memory type
        let account_key = LedgerKey::Account(stellar_xdr::curr::LedgerKeyAccount {
            account_id: stellar_xdr::curr::AccountId(
                stellar_xdr::curr::PublicKey::PublicKeyTypeEd25519(stellar_xdr::curr::Uint256(
                    [0u8; 32],
                )),
            ),
        });
        assert!(!InMemorySorobanState::is_in_memory_type(&account_key));
    }

    /// Regression test: Creating ContractData that already exists should fail.
    ///
    /// Similar to ContractCode, ContractData entries restored from hot archive
    /// might already exist in soroban_state. The caller must check for existence.
    #[test]
    fn test_create_duplicate_contract_data_fails() {
        let mut state = InMemorySorobanState::new();

        // Create a contract data entry
        let entry = make_contract_data_entry([42u8; 32]);
        state.create_contract_data(entry.clone()).unwrap();

        // Attempting to create the same data again should fail
        let result = state.create_contract_data(entry);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("contract data already exists"));
    }

    /// Test that process_entry_update returns an error when entry doesn't exist.
    ///
    /// Parity: stellar-core asserts entry exists in updateContractData /
    /// updateContractCode (releaseAssertOrThrow). We mirror that by returning
    /// an error rather than silently creating.
    #[test]
    fn test_process_entry_update_errors_if_not_exists() {
        let mut state = InMemorySorobanState::new();

        // ContractCode update when entry doesn't exist should error
        let code_entry = make_contract_code_entry([42u8; 32]);
        let result = state.process_entry_update(&code_entry, 25, None);
        assert!(result.is_err(), "update of missing code entry should fail");
        assert_eq!(state.contract_code_count(), 0);

        // ContractData update when entry doesn't exist should error
        let data_entry = make_contract_data_entry([43u8; 32]);
        let result = state.process_entry_update(&data_entry, 25, None);
        assert!(result.is_err(), "update of missing data entry should fail");
        assert_eq!(state.contract_data_count(), 0);
    }

    #[test]
    fn test_snapshot_is_frozen_after_mutation() {
        // Verify that a snapshot is a frozen point-in-time copy: mutations to the
        // original state after snapshot() must NOT be visible through the snapshot.
        let mut state = InMemorySorobanState::new();
        state.set_last_closed_ledger_seq(100);

        // Insert a contract data entry.
        let entry1 = make_contract_data_entry([10u8; 32]);
        state.process_entry_create(&entry1, 25, None).unwrap();
        assert_eq!(state.contract_data_count(), 1);

        // Take a snapshot.
        let snap = state.snapshot();
        assert_eq!(snap.contract_data_count(), 1);
        assert_eq!(snap.ledger_seq(), 100);

        // Mutate the original: add another entry and bump ledger seq.
        let entry2 = make_contract_data_entry([20u8; 32]);
        state.process_entry_create(&entry2, 25, None).unwrap();
        state.set_last_closed_ledger_seq(101);
        assert_eq!(state.contract_data_count(), 2);

        // Snapshot must still see the old state.
        assert_eq!(snap.contract_data_count(), 1);
        assert_eq!(snap.ledger_seq(), 100);

        // Verify the snapshot can look up the original entry but NOT the new one.
        let make_key = |entry: &LedgerEntry| -> LedgerKey {
            if let LedgerEntryData::ContractData(cd) = &entry.data {
                LedgerKey::ContractData(LedgerKeyContractData {
                    contract: cd.contract.clone(),
                    key: cd.key.clone(),
                    durability: cd.durability,
                })
            } else {
                panic!("expected ContractData");
            }
        };
        let key1 = make_key(&entry1);
        let key2 = make_key(&entry2);
        assert!(snap.get(&key1).is_some(), "snapshot should contain entry1");
        assert!(
            snap.get(&key2).is_none(),
            "snapshot must NOT contain entry2"
        );
    }

    /// Test Arc-based COW snapshot isolation across all mutation types.
    ///
    /// Verifies that after taking a snapshot, mutations via Arc::make_mut
    /// (create, update, delete, TTL update, clear) only affect the live
    /// state while the snapshot remains frozen.
    #[test]
    fn test_arc_cow_snapshot_isolation() {
        let mut state = InMemorySorobanState::new();
        state.set_last_closed_ledger_seq(100);

        // Populate initial state: 2 data entries + 1 code entry.
        let data1 = make_contract_data_entry([1u8; 32]);
        let data2 = make_contract_data_entry([2u8; 32]);
        let code1 = make_contract_code_entry([3u8; 32]);
        state.process_entry_create(&data1, 25, None).unwrap();
        state.process_entry_create(&data2, 25, None).unwrap();
        state.process_entry_create(&code1, 25, None).unwrap();
        assert_eq!(state.contract_data_count(), 2);
        assert_eq!(state.contract_code_count(), 1);
        let pre_snap_data_size = state.contract_data_state_size();
        let pre_snap_code_size = state.contract_code_state_size();

        // --- Take snapshot ---
        let snap = state.snapshot();
        assert_eq!(snap.contract_data_count(), 2);
        assert_eq!(snap.contract_code_count(), 1);

        // 1) CREATE a new data entry on live state.
        let data3 = make_contract_data_entry([4u8; 32]);
        state.process_entry_create(&data3, 25, None).unwrap();
        assert_eq!(state.contract_data_count(), 3);
        assert_eq!(
            snap.contract_data_count(),
            2,
            "snapshot must not see new create"
        );

        // 2) UPDATE data2 on live state.
        let data2_updated = {
            let mut e = data2.clone();
            if let LedgerEntryData::ContractData(ref mut cd) = e.data {
                cd.val = ScVal::I32(999);
            }
            e
        };
        state.update_contract_data(data2_updated).unwrap();
        // Snapshot should still see original data2 value.
        let snap_data2 = snap.get(&make_data_key(&data2)).unwrap();
        if let LedgerEntryData::ContractData(cd) = &snap_data2.data {
            assert_eq!(cd.val, ScVal::I32(42), "snapshot must see original value");
        }

        // 3) DELETE data1 on live state.
        let data1_key = match &data1.data {
            LedgerEntryData::ContractData(cd) => LedgerKeyContractData {
                contract: cd.contract.clone(),
                key: cd.key.clone(),
                durability: cd.durability,
            },
            _ => unreachable!(),
        };
        state.delete_contract_data(&data1_key).unwrap();
        assert_eq!(state.contract_data_count(), 2); // data2 + data3
        assert_eq!(
            snap.contract_data_count(),
            2,
            "snapshot must still have data1"
        );
        assert!(snap.get(&make_data_key(&data1)).is_some());

        // 4) UPDATE TTL on code1 in live state.
        let code1_key_hash = InMemorySorobanState::contract_code_key_hash(&LedgerKeyContractCode {
            hash: Hash([3u8; 32]),
        });
        let ttl_key = LedgerKeyTtl {
            key_hash: code1_key_hash,
        };
        // First set a TTL so we can update it.
        state.create_ttl(&ttl_key, TtlData::new(200, 100)).unwrap();
        state.update_ttl(&ttl_key, TtlData::new(999, 100)).unwrap();
        // Snapshot code entry should have default TTL (uninitialized).
        let snap_code = snap
            .get_contract_code(&LedgerKeyContractCode {
                hash: Hash([3u8; 32]),
            })
            .unwrap();
        assert!(
            !snap_code.ttl_data.is_initialized(),
            "snapshot TTL must remain uninitialized"
        );

        // 5) CLEAR live state.
        state.clear();
        assert_eq!(state.contract_data_count(), 0);
        assert_eq!(state.contract_code_count(), 0);
        // Snapshot must be entirely unaffected.
        assert_eq!(snap.contract_data_count(), 2);
        assert_eq!(snap.contract_code_count(), 1);
        assert_eq!(snap.contract_data_state_size(), pre_snap_data_size);
        assert_eq!(snap.contract_code_state_size(), pre_snap_code_size);
    }

    /// Helper to make a LedgerKey from a contract data LedgerEntry.
    fn make_data_key(entry: &LedgerEntry) -> LedgerKey {
        if let LedgerEntryData::ContractData(cd) = &entry.data {
            LedgerKey::ContractData(LedgerKeyContractData {
                contract: cd.contract.clone(),
                key: cd.key.clone(),
                durability: cd.durability,
            })
        } else {
            panic!("expected ContractData");
        }
    }
}
