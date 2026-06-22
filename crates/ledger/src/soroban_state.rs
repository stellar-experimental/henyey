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

use henyey_common::protocol::{protocol_version_is_before, ProtocolVersion};
use henyey_common::Hash256;

// ---------------------------------------------------------------------------
// ShardedCowMap — fixed-size sharded Arc<HashMap> wrapper
// ---------------------------------------------------------------------------

/// Number of shards for the contract data/code maps.
///
/// 64 shards means we use the top 6 bits of the key hash for routing.
/// During ledger close, only the shards containing mutated keys will be
/// deep-cloned by `Arc::make_mut`; untouched shards remain shared with
/// any outstanding snapshots.
const NUM_SHARDS: usize = 64;

/// A sharded copy-on-write map backed by `[Arc<HashMap<Hash, V>>; NUM_SHARDS]`.
///
/// Provides the same logical interface as a single `Arc<HashMap<Hash, V>>` but
/// limits the blast radius of `Arc::make_mut`: when concurrent snapshots hold
/// references, only the shards that are actually mutated incur a deep clone
/// (typically 1–5 per close out of 64), proportional to the per-ledger write
/// set rather than the total state size.
///
/// Shard selection uses the top 6 bits of the `Hash` key (bytes are already
/// uniformly distributed by SHA-256), keeping routing O(1) with no extra
/// hashing.
#[derive(Debug, Clone)]
struct ShardedCowMap<V> {
    shards: [Arc<HashMap<Hash, V>>; NUM_SHARDS],
}

impl<V: Clone> ShardedCowMap<V> {
    /// Create a new empty sharded map.
    fn new() -> Self {
        Self {
            shards: std::array::from_fn(|_| Arc::new(HashMap::new())),
        }
    }

    /// Select the shard index from the top 6 bits of a Hash key.
    #[inline]
    fn shard_index(key: &Hash) -> usize {
        (key.0[0] >> 2) as usize
    }

    /// Get a reference to an entry.
    #[inline]
    fn get(&self, key: &Hash) -> Option<&V> {
        self.shards[Self::shard_index(key)].get(key)
    }

    /// Check if a key exists.
    #[inline]
    fn contains_key(&self, key: &Hash) -> bool {
        self.shards[Self::shard_index(key)].contains_key(key)
    }

    /// Get a mutable reference to the shard's HashMap for the given key.
    ///
    /// This calls `Arc::make_mut` only on the targeted shard, so concurrent
    /// snapshot holders only force a clone of this one shard.
    #[inline]
    fn shard_mut(&mut self, key: &Hash) -> &mut HashMap<Hash, V> {
        Arc::make_mut(&mut self.shards[Self::shard_index(key)])
    }

    /// Insert a key-value pair into the appropriate shard.
    fn insert(&mut self, key: Hash, value: V) {
        self.shard_mut(&key).insert(key, value);
    }

    /// Remove a key from the appropriate shard, returning the value if present.
    fn remove(&mut self, key: &Hash) -> Option<V> {
        self.shard_mut(key).remove(key)
    }

    /// Total number of entries across all shards.
    fn len(&self) -> usize {
        self.shards.iter().map(|s| s.len()).sum()
    }

    /// Check if all shards are empty.
    fn is_empty(&self) -> bool {
        self.shards.iter().all(|s| s.is_empty())
    }

    /// Total capacity across all shards (for memory estimation).
    fn capacity(&self) -> usize {
        self.shards.iter().map(|s| s.capacity()).sum()
    }

    /// Maximum `Arc::strong_count` across all shards.
    ///
    /// A value of 1 means all shards are exclusively owned and `Arc::make_mut`
    /// will mutate in-place. A value >1 means at least one shard is shared
    /// with a snapshot and that shard's next mutation will trigger a clone.
    fn max_arc_strong_count(&self) -> usize {
        self.shards
            .iter()
            .map(|s| Arc::strong_count(s))
            .max()
            .unwrap_or(1)
    }

    /// Clear all shards by replacing with fresh empty `Arc<HashMap>` values.
    ///
    /// This avoids calling `Arc::make_mut` + `.clear()` on shared shards,
    /// which would deep-clone then immediately discard the data.
    fn clear(&mut self) {
        for shard in &mut self.shards {
            *shard = Arc::new(HashMap::new());
        }
    }

    /// Snapshot: clone all shard Arcs (O(NUM_SHARDS), not O(entries)).
    fn snapshot(&self) -> Self {
        Self {
            shards: self.shards.clone(),
        }
    }

    /// Get a mutable reference to the HashMap of a specific shard by index.
    ///
    /// Used by `recompute_contract_code_sizes` which needs to iterate mutably
    /// over all entries.
    fn shard_mut_by_index(&mut self, idx: usize) -> &mut HashMap<Hash, V> {
        Arc::make_mut(&mut self.shards[idx])
    }
}
use henyey_tx::operations::execute::entry_size_for_rent_by_protocol_with_cost_params;
use henyey_tx::soroban::convert::{
    try_convert_cost_params_ws_to_p25, try_convert_cost_params_ws_to_p26,
    try_convert_ledger_entry_ws_to_p25, try_convert_ledger_entry_ws_to_p26,
};
use soroban_env_host25::budget::Budget as BudgetP25;
use soroban_env_host25::e2e_invoke::entry_size_for_rent as entry_size_for_rent_p25;
use soroban_env_host_p25 as soroban_env_host25;
use soroban_env_host_p26 as soroban_env_host26;
use soroban_env_host_p27 as soroban_env_host27;
use stellar_xdr::{
    ConfigSettingId, ContractCostParams, Hash, LedgerEntry, LedgerEntryData, LedgerKey,
    LedgerKeyConfigSetting, LedgerKeyContractCode, LedgerKeyContractData, LedgerKeyTtl, TtlEntry,
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

/// Build a p25 Budget from on-chain cost parameters (for protocol 25).
fn build_rent_budget_p25(rent_config: Option<&SorobanRentConfig>) -> BudgetP25 {
    let Some(config) = rent_config else {
        return BudgetP25::default();
    };
    if !config.has_valid_cost_params() {
        return BudgetP25::default();
    }

    let instruction_limit = config.tx_max_instructions.saturating_mul(2);
    let memory_limit = config.tx_max_memory_bytes.saturating_mul(2);
    let cpu_params = match try_convert_cost_params_ws_to_p25(&config.cpu_cost_params) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!("build_rent_budget_p25: {e}, using default budget");
            return BudgetP25::default();
        }
    };
    let mem_params = match try_convert_cost_params_ws_to_p25(&config.mem_cost_params) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!("build_rent_budget_p25: {e}, using default budget");
            return BudgetP25::default();
        }
    };
    BudgetP25::try_from_configs(instruction_limit, memory_limit, cpu_params, mem_params)
        .unwrap_or_else(|_| BudgetP25::default())
}

/// Build a p26 Budget from on-chain cost parameters (for protocol 26+).
///
/// P26 host pins stellar-xdr 26.0.0, distinct from the workspace XDR, so the
/// cost params are byte-converted to the P26 type.
fn build_rent_budget_p26(
    rent_config: Option<&SorobanRentConfig>,
) -> soroban_env_host26::budget::Budget {
    let Some(config) = rent_config else {
        return soroban_env_host26::budget::Budget::default();
    };
    if !config.has_valid_cost_params() {
        return soroban_env_host26::budget::Budget::default();
    }

    let instruction_limit = config.tx_max_instructions.saturating_mul(2);
    let memory_limit = config.tx_max_memory_bytes.saturating_mul(2);
    let cpu_params = match try_convert_cost_params_ws_to_p26(&config.cpu_cost_params) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!("build_rent_budget_p26: {e}, using default budget");
            return soroban_env_host26::budget::Budget::default();
        }
    };
    let mem_params = match try_convert_cost_params_ws_to_p26(&config.mem_cost_params) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!("build_rent_budget_p26: {e}, using default budget");
            return soroban_env_host26::budget::Budget::default();
        }
    };
    soroban_env_host26::budget::Budget::try_from_configs(
        instruction_limit,
        memory_limit,
        cpu_params,
        mem_params,
    )
    .unwrap_or_else(|_| soroban_env_host26::budget::Budget::default())
}

/// Build a p27 Budget from on-chain cost parameters (for protocol 27+).
///
/// NATIVE: the P27 host shares stellar-xdr 27.0.0 with the workspace, so the
/// workspace `ContractCostParams` are passed straight through (no conversion).
fn build_rent_budget_p27(
    rent_config: Option<&SorobanRentConfig>,
) -> soroban_env_host27::budget::Budget {
    let Some(config) = rent_config else {
        return soroban_env_host27::budget::Budget::default();
    };
    if !config.has_valid_cost_params() {
        return soroban_env_host27::budget::Budget::default();
    }

    let instruction_limit = config.tx_max_instructions.saturating_mul(2);
    let memory_limit = config.tx_max_memory_bytes.saturating_mul(2);
    soroban_env_host27::budget::Budget::try_from_configs(
        instruction_limit,
        memory_limit,
        config.cpu_cost_params.clone(),
        config.mem_cost_params.clone(),
    )
    .unwrap_or_else(|_| soroban_env_host27::budget::Budget::default())
}

/// Compute the XDR-encoded byte length of a `LedgerEntry` as a `u32`.
///
/// Uses `henyey_common::xdr_encoded_len` (a counting writer with zero heap
/// allocation). Panics if XDR encoding fails or the length exceeds `u32::MAX`
/// — both conditions indicate a bug, not a recoverable error.
fn compute_xdr_size(entry: &LedgerEntry) -> u32 {
    u32::try_from(henyey_common::xdr_encoded_len(entry))
        .expect("XDR encoded length must fit in u32")
}

/// A contract data entry with co-located TTL.
///
/// Fields are private; construction is only through [`ContractDataMapEntry::new`].
#[derive(Debug, Clone)]
pub struct ContractDataMapEntry {
    ledger_entry: Arc<LedgerEntry>,
    ttl_data: TtlData,
}

impl ContractDataMapEntry {
    /// Create a new entry.
    pub(crate) fn new(ledger_entry: Arc<LedgerEntry>, ttl_data: TtlData) -> Self {
        Self {
            ledger_entry,
            ttl_data,
        }
    }

    /// The contract data ledger entry.
    pub fn ledger_entry(&self) -> &LedgerEntry {
        &self.ledger_entry
    }

    /// TTL data co-located with the entry.
    pub fn ttl_data(&self) -> TtlData {
        self.ttl_data
    }

    /// XDR-encoded byte length, computed on demand via [`compute_xdr_size`]
    /// (zero-alloc counting writer).
    ///
    /// Cost is O(entry_size) per call. This is acceptable for the internal
    /// call sites (create/update/delete accounting) which invoke it at most
    /// once per mutation. External callers should avoid calling this in tight
    /// loops over large entry sets.
    pub fn xdr_size(&self) -> u32 {
        compute_xdr_size(&self.ledger_entry)
    }
}

/// A contract code entry with co-located TTL and size tracking.
///
/// All fields are private to enforce the cache invariant: `cached_xdr_size`
/// always equals the XDR-encoded length of `ledger_entry`. Construction is
/// only through [`ContractCodeMapEntry::new`].
#[derive(Debug, Clone)]
pub struct ContractCodeMapEntry {
    ledger_entry: Arc<LedgerEntry>,
    ttl_data: TtlData,
    /// In-memory size of the compiled module (for rent calculations).
    /// Protocol 23+ uses this for rent fees, which may differ from XDR size
    /// due to compilation overhead.
    size_bytes: u32,
    cached_xdr_size: u32,
}

impl ContractCodeMapEntry {
    /// Create a new entry with a pre-computed XDR size.
    ///
    /// The caller computes `xdr_size` via [`compute_xdr_size`] and passes it
    /// both to `calculate_code_size` and here, avoiding double serialization.
    fn with_xdr_size(
        ledger_entry: Arc<LedgerEntry>,
        ttl_data: TtlData,
        size_bytes: u32,
        cached_xdr_size: u32,
    ) -> Self {
        Self {
            ledger_entry,
            ttl_data,
            size_bytes,
            cached_xdr_size,
        }
    }

    /// Create a new entry, computing and caching the XDR size.
    #[cfg(test)]
    pub(crate) fn new(ledger_entry: Arc<LedgerEntry>, ttl_data: TtlData, size_bytes: u32) -> Self {
        let cached_xdr_size = compute_xdr_size(&ledger_entry);
        Self::with_xdr_size(ledger_entry, ttl_data, size_bytes, cached_xdr_size)
    }

    /// The contract code ledger entry.
    pub fn ledger_entry(&self) -> &LedgerEntry {
        &self.ledger_entry
    }

    /// TTL data co-located with the entry.
    pub fn ttl_data(&self) -> TtlData {
        self.ttl_data
    }

    /// In-memory size of the compiled module (for rent calculations).
    pub fn size_bytes(&self) -> u32 {
        self.size_bytes
    }

    /// Cached XDR-encoded byte length (computed once at construction).
    pub fn xdr_size(&self) -> u32 {
        self.cached_xdr_size
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
    /// Sharded across 64 `Arc<HashMap>` slots for bounded copy-on-write:
    /// concurrent snapshots only force a clone of the shards touched by a
    /// given close, not the entire map.
    contract_data_entries: ShardedCowMap<ContractDataMapEntry>,

    /// Contract code entries indexed by TTL key hash.
    ///
    /// Sharded across 64 `Arc<HashMap>` slots (see `contract_data_entries`).
    contract_code_entries: ShardedCowMap<ContractCodeMapEntry>,

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
            contract_data_entries: ShardedCowMap::new(),
            contract_code_entries: ShardedCowMap::new(),
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
    /// The clone shares map data with the original via per-shard `Arc`, making
    /// this O(NUM_SHARDS) instead of O(n). The first mutation after a snapshot
    /// uses copy-on-write (`Arc::make_mut`) to detach only the affected shard
    /// from the frozen snapshot.
    ///
    /// The snapshot is used by `create_snapshot()` to ensure that Soroban entry
    /// lookups return data consistent with the header captured at the same
    /// instant, preventing a race where a concurrent `commit()` updates entries
    /// before the header is published.
    pub fn snapshot(&self) -> Self {
        Self {
            contract_data_entries: self.contract_data_entries.snapshot(),
            contract_code_entries: self.contract_code_entries.snapshot(),
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

    /// Get the maximum `Arc::strong_count` across shards for data and code maps.
    ///
    /// Returns `(max_data_shard_count, max_code_shard_count)`. A count of 1
    /// means all shards are exclusively owned and `Arc::make_mut` can mutate
    /// in-place; >1 means at least one shard is shared with a snapshot and
    /// that shard's next mutation will trigger a clone.
    pub fn arc_strong_counts(&self) -> (usize, usize) {
        (
            self.contract_data_entries.max_arc_strong_count(),
            self.contract_code_entries.max_arc_strong_count(),
        )
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
            ext: stellar_xdr::LedgerEntryExt::V0,
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

        let map_entry = ContractDataMapEntry::new(Arc::new(entry), ttl_data);

        self.contract_data_state_size += map_entry.xdr_size() as i64;
        self.contract_data_entries.insert(key_hash, map_entry);

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

        let old_entry = self
            .contract_data_entries
            .remove(&key_hash)
            .ok_or_else(|| LedgerError::InvalidEntry("contract data does not exist".into()))?;

        // Update size tracking
        let old_size = old_entry.xdr_size();
        let new_entry = ContractDataMapEntry::new(Arc::new(entry), old_entry.ttl_data);
        let new_size = new_entry.xdr_size();

        self.contract_data_state_size += (new_size as i64) - (old_size as i64);
        self.contract_data_entries.insert(key_hash, new_entry);

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

        let old_entry = self
            .contract_data_entries
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

        let arc_entry = Arc::new(entry);
        let xdr_size = compute_xdr_size(&arc_entry);
        let size_bytes =
            Self::calculate_code_size(&arc_entry, xdr_size, protocol_version, rent_config);

        let map_entry =
            ContractCodeMapEntry::with_xdr_size(arc_entry, ttl_data, size_bytes, xdr_size);

        self.contract_code_state_size += map_entry.size_bytes as i64;
        self.contract_code_entries.insert(key_hash, map_entry);

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
        let arc_entry = Arc::new(entry);
        let xdr_size = compute_xdr_size(&arc_entry);
        let new_size =
            Self::calculate_code_size(&arc_entry, xdr_size, protocol_version, rent_config);

        let map = self.contract_code_entries.shard_mut(&key_hash);
        let old_entry = map
            .remove(&key_hash)
            .ok_or_else(|| LedgerError::InvalidEntry("contract code does not exist".into()))?;

        let new_entry =
            ContractCodeMapEntry::with_xdr_size(arc_entry, old_entry.ttl_data, new_size, xdr_size);

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

        let old_entry = self
            .contract_code_entries
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
        // Check which map contains the key before taking a mutable reference,
        // to avoid an unnecessary COW clone on the wrong shard.
        if self.contract_data_entries.contains_key(&key.key_hash) {
            let entry = self
                .contract_data_entries
                .shard_mut(&key.key_hash)
                .get_mut(&key.key_hash)
                .unwrap();
            if entry.ttl_data.is_initialized() {
                return Err(LedgerError::InvalidEntry(
                    "contract data TTL already initialized".into(),
                ));
            }
            entry.ttl_data = ttl_data;
            trace!("Updated TTL inline for contract data");
            return Ok(());
        }

        if self.contract_code_entries.contains_key(&key.key_hash) {
            let entry = self
                .contract_code_entries
                .shard_mut(&key.key_hash)
                .get_mut(&key.key_hash)
                .unwrap();
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
        // Check which map contains the key before taking a mutable reference,
        // to avoid an unnecessary COW clone on the wrong shard.
        if self.contract_data_entries.contains_key(&key.key_hash) {
            self.contract_data_entries
                .shard_mut(&key.key_hash)
                .get_mut(&key.key_hash)
                .unwrap()
                .ttl_data = ttl_data;
            trace!("Updated TTL inline for contract data");
            return Ok(());
        }

        if self.contract_code_entries.contains_key(&key.key_hash) {
            self.contract_code_entries
                .shard_mut(&key.key_hash)
                .get_mut(&key.key_hash)
                .unwrap()
                .ttl_data = ttl_data;
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
    ///
    /// Accepts a pre-computed XDR size to avoid redundant serialization.
    /// Dispatches to the appropriate soroban host version based on protocol:
    /// - Protocol < 25: uses p24 host (via entry_size_for_rent_by_protocol_with_cost_params)
    /// - Protocol 25: uses p25 host
    /// - Protocol >= 26: uses p26 host (handles 86 cost type entries)
    fn calculate_code_size(
        entry: &LedgerEntry,
        xdr_size: u32,
        protocol_version: u32,
        rent_config: Option<&SorobanRentConfig>,
    ) -> u32 {
        if protocol_version_is_before(protocol_version, ProtocolVersion::V25) {
            let cost_params = rent_config.map(|rc| (&rc.cpu_cost_params, &rc.mem_cost_params));
            return entry_size_for_rent_by_protocol_with_cost_params(
                protocol_version,
                entry,
                xdr_size,
                cost_params,
            );
        }
        if protocol_version_is_before(protocol_version, ProtocolVersion::V26) {
            // Protocol 25: use p25 host (85 cost types)
            let budget = build_rent_budget_p25(rent_config);
            return match try_convert_ledger_entry_ws_to_p25(entry) {
                Ok(p25_entry) => {
                    entry_size_for_rent_p25(&budget, &p25_entry, xdr_size).unwrap_or(xdr_size)
                }
                Err(e) => {
                    tracing::warn!("calculate_code_size: {e}, falling back to XDR size");
                    xdr_size
                }
            };
        }
        if protocol_version_is_before(protocol_version, ProtocolVersion::V27) {
            // Protocol 26: use p26 host (86 cost types).
            // P26 host pins stellar-xdr 26.0.0, distinct from the workspace XDR, so
            // the entry is byte-converted to the P26 type.
            let budget = build_rent_budget_p26(rent_config);
            return match try_convert_ledger_entry_ws_to_p26(entry) {
                Ok(p26_entry) => soroban_env_host26::e2e_invoke::entry_size_for_rent(
                    &budget, &p26_entry, xdr_size,
                )
                .unwrap_or(xdr_size),
                Err(e) => {
                    tracing::warn!("calculate_code_size: {e}, falling back to XDR size");
                    xdr_size
                }
            };
        }
        // Protocol >= 27: use p27 host.
        // P27 host shares stellar-xdr 27.0.0 with the workspace, so the entry is
        // passed NATIVELY (no byte conversion).
        let budget = build_rent_budget_p27(rent_config);
        soroban_env_host27::e2e_invoke::entry_size_for_rent(&budget, entry, xdr_size)
            .unwrap_or(xdr_size)
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

        for shard_idx in 0..NUM_SHARDS {
            let shard = self.contract_code_entries.shard_mut_by_index(shard_idx);
            for entry in shard.values_mut() {
                let new_size = Self::calculate_code_size(
                    &entry.ledger_entry,
                    entry.cached_xdr_size,
                    protocol_version,
                    rent_config,
                );

                entry.size_bytes = new_size;
                total_size += new_size as i64;
            }
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
        self.contract_data_entries.clear();
        self.contract_code_entries.clear();
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
    /// sizes plus HashMap overhead for the key-to-entry mapping across all shards.
    pub fn estimate_contract_data_heap_bytes(&self) -> usize {
        use henyey_common::memory::hashmap_heap_bytes;
        let map_bytes = hashmap_heap_bytes(
            self.contract_data_entries.capacity(),
            32,
            std::mem::size_of::<ContractDataMapEntry>(),
        );
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
    use stellar_xdr::{
        ContractCodeEntry, ContractCodeEntryExt, ContractDataDurability, ContractDataEntry,
        ContractId, ExtensionPoint, Hash, LedgerEntryExt, LedgerKeyContractCode,
        LedgerKeyContractData, ScAddress, ScVal,
    };

    fn make_contract_address() -> ScAddress {
        ScAddress::Contract(ContractId(Hash([1u8; 32])))
    }

    /// Compute the shard index a contract data entry with the given key_bytes
    /// would land in. Uses the same hash path as production code.
    fn contract_data_shard_index(key_bytes: [u8; 32]) -> usize {
        let ledger_key = LedgerKey::ContractData(LedgerKeyContractData {
            contract: make_contract_address(),
            key: ScVal::Bytes(stellar_xdr::ScBytes(key_bytes.to_vec().try_into().unwrap())),
            durability: ContractDataDurability::Persistent,
        });
        let key_hash = Hash256::hash_xdr(&ledger_key);
        (key_hash.as_bytes()[0] >> 2) as usize
    }

    /// Compute the shard index a contract code entry with the given hash would
    /// land in.
    fn contract_code_shard_index(hash_bytes: [u8; 32]) -> usize {
        let ledger_key = LedgerKey::ContractCode(LedgerKeyContractCode {
            hash: Hash(hash_bytes),
        });
        let key_hash = Hash256::hash_xdr(&ledger_key);
        (key_hash.as_bytes()[0] >> 2) as usize
    }

    /// Find two byte arrays whose contract data entries land in different shards.
    fn find_two_data_keys_in_different_shards() -> ([u8; 32], usize, [u8; 32], usize) {
        let key_a = [0x00; 32];
        let shard_a = contract_data_shard_index(key_a);
        for i in 1u8..=255 {
            let key_b = [i; 32];
            let shard_b = contract_data_shard_index(key_b);
            if shard_b != shard_a {
                return (key_a, shard_a, key_b, shard_b);
            }
        }
        panic!("could not find two keys in different shards");
    }

    /// Find two byte arrays whose contract code entries land in different shards.
    fn find_two_code_keys_in_different_shards() -> ([u8; 32], usize, [u8; 32], usize) {
        let key_a = [0x00; 32];
        let shard_a = contract_code_shard_index(key_a);
        for i in 1u8..=255 {
            let key_b = [i; 32];
            let shard_b = contract_code_shard_index(key_b);
            if shard_b != shard_a {
                return (key_a, shard_a, key_b, shard_b);
            }
        }
        panic!("could not find two keys in different shards");
    }

    fn make_contract_data_entry(key_bytes: [u8; 32]) -> LedgerEntry {
        LedgerEntry {
            last_modified_ledger_seq: 100,
            data: LedgerEntryData::ContractData(ContractDataEntry {
                ext: ExtensionPoint::V0,
                contract: make_contract_address(),
                key: ScVal::Bytes(stellar_xdr::ScBytes(key_bytes.to_vec().try_into().unwrap())),
                durability: ContractDataDurability::Persistent,
                val: ScVal::I32(42),
            }),
            ext: stellar_xdr::LedgerEntryExt::V0,
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
            ext: stellar_xdr::LedgerEntryExt::V0,
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
            key: ScVal::Bytes(stellar_xdr::ScBytes([1u8; 32].to_vec().try_into().unwrap())),
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
            key: ScVal::Bytes(stellar_xdr::ScBytes([1u8; 32].to_vec().try_into().unwrap())),
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
            key: ScVal::Bytes(stellar_xdr::ScBytes([1u8; 32].to_vec().try_into().unwrap())),
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
            key: ScVal::Bytes(stellar_xdr::ScBytes([1u8; 32].to_vec().try_into().unwrap())),
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
            key: ScVal::Bytes(stellar_xdr::ScBytes(
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
                key: ScVal::Bytes(stellar_xdr::ScBytes(
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
        use stellar_xdr::{ConfigSettingId, LedgerKeyConfigSetting};

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
        let account_key = LedgerKey::Account(stellar_xdr::LedgerKeyAccount {
            account_id: stellar_xdr::AccountId(stellar_xdr::PublicKey::PublicKeyTypeEd25519(
                stellar_xdr::Uint256([0u8; 32]),
            )),
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

    #[test]
    fn test_xdr_size_matches_serialization() {
        use stellar_xdr::{Limits, WriteXdr};

        // Contract data entry
        let entry = make_contract_data_entry([1u8; 32]);
        let expected_size = entry.to_xdr(Limits::none()).unwrap().len() as u32;
        let map_entry = ContractDataMapEntry::new(Arc::new(entry), TtlData::default());
        assert_eq!(map_entry.xdr_size(), expected_size);
        assert!(expected_size > 0);

        // Contract code entry
        let code_entry = make_contract_code_entry([2u8; 32]);
        let expected_code_size = code_entry.to_xdr(Limits::none()).unwrap().len() as u32;
        let code_map_entry =
            ContractCodeMapEntry::new(Arc::new(code_entry), TtlData::default(), 100);
        assert_eq!(code_map_entry.xdr_size(), expected_code_size);
        assert!(expected_code_size > 0);
    }

    #[test]
    fn test_contract_data_size_tracking_exact_deltas() {
        use stellar_xdr::{Limits, WriteXdr};

        let mut state = InMemorySorobanState::new();

        // Create entry and verify exact size
        let entry = make_contract_data_entry([1u8; 32]);
        let entry_xdr_size = entry.to_xdr(Limits::none()).unwrap().len() as i64;
        state.create_contract_data(entry).unwrap();
        assert_eq!(state.contract_data_state_size, entry_xdr_size);

        // Update with a different-sized value (larger)
        let mut updated_entry = make_contract_data_entry([1u8; 32]);
        if let LedgerEntryData::ContractData(cd) = &mut updated_entry.data {
            // Use a much larger value to ensure different XDR size
            cd.val = ScVal::Bytes(stellar_xdr::ScBytes(vec![0u8; 200].try_into().unwrap()));
        }
        let updated_xdr_size = updated_entry.to_xdr(Limits::none()).unwrap().len() as i64;
        state.update_contract_data(updated_entry).unwrap();
        assert_eq!(state.contract_data_state_size, updated_xdr_size);
        assert_ne!(
            entry_xdr_size, updated_xdr_size,
            "updated entry should have different size"
        );

        // Delete and verify size returns to 0
        let key = LedgerKeyContractData {
            contract: make_contract_address(),
            key: ScVal::Bytes(stellar_xdr::ScBytes([1u8; 32].to_vec().try_into().unwrap())),
            durability: ContractDataDurability::Persistent,
        };
        state.delete_contract_data(&key).unwrap();
        assert_eq!(state.contract_data_state_size, 0);
    }

    #[test]
    fn test_contract_code_size_tracking_exact_deltas() {
        let mut state = InMemorySorobanState::new();

        // Create code entry (protocol 25, no rent config = uses XDR size)
        let entry = make_contract_code_entry([2u8; 32]);
        state.create_contract_code(entry, 25, None).unwrap();

        let code_size = state.contract_code_state_size;
        assert!(code_size > 0);

        // Delete and verify returns to 0
        let key = LedgerKeyContractCode {
            hash: Hash([2u8; 32]),
        };
        state.delete_contract_code(&key).unwrap();
        assert_eq!(state.contract_code_state_size, 0);
    }

    #[test]
    fn test_recompute_contract_code_sizes() {
        let mut state = InMemorySorobanState::new();

        let entry1 = make_contract_code_entry([1u8; 32]);
        let entry2 = make_contract_code_entry([2u8; 32]);
        state.create_contract_code(entry1, 25, None).unwrap();
        state.create_contract_code(entry2, 25, None).unwrap();

        let size_before = state.contract_code_state_size;
        assert!(size_before > 0);

        // Recompute with same config — size should remain the same
        state.recompute_contract_code_sizes(25, None);
        assert_eq!(state.contract_code_state_size, size_before);
    }

    #[test]
    fn test_xdr_size_consistent_across_clone() {
        // Data entry: xdr_size() is computed on demand from the shared Arc<LedgerEntry>,
        // so clones produce the same result.
        let entry = make_contract_data_entry([1u8; 32]);
        let map_entry = ContractDataMapEntry::new(Arc::new(entry), TtlData::default());
        let cloned = map_entry.clone();
        assert_eq!(cloned.xdr_size(), map_entry.xdr_size());
        assert!(map_entry.xdr_size() > 0);

        // Code entry: xdr_size() is cached at construction, preserved on clone.
        let code_entry = make_contract_code_entry([2u8; 32]);
        let code_map_entry =
            ContractCodeMapEntry::new(Arc::new(code_entry), TtlData::default(), 100);
        let code_cloned = code_map_entry.clone();
        assert_eq!(code_cloned.xdr_size(), code_map_entry.xdr_size());
    }

    #[test]
    fn test_snapshot_preserves_sizes() {
        use stellar_xdr::{Limits, WriteXdr};

        let mut state = InMemorySorobanState::new();

        let data1 = make_contract_data_entry([1u8; 32]);
        let data1_xdr_size = data1.to_xdr(Limits::none()).unwrap().len() as i64;
        state.create_contract_data(data1.clone()).unwrap();

        let code1 = make_contract_code_entry([3u8; 32]);
        state.create_contract_code(code1, 25, None).unwrap();

        // Take snapshot
        let snap = state.snapshot();
        let snap_data_size = snap.contract_data_state_size();
        let snap_code_size = snap.contract_code_state_size();
        assert_eq!(snap_data_size, data1_xdr_size);
        assert!(snap_code_size > 0);

        // Mutate live state — create new entry
        let data2 = make_contract_data_entry([2u8; 32]);
        state.create_contract_data(data2).unwrap();
        assert!(state.contract_data_state_size > snap_data_size);

        // Snapshot sizes must be unchanged
        assert_eq!(snap.contract_data_state_size(), snap_data_size);
        assert_eq!(snap.contract_code_state_size(), snap_code_size);

        // Delete from live
        let key = LedgerKeyContractData {
            contract: make_contract_address(),
            key: ScVal::Bytes(stellar_xdr::ScBytes([1u8; 32].to_vec().try_into().unwrap())),
            durability: ContractDataDurability::Persistent,
        };
        state.delete_contract_data(&key).unwrap();

        // Snapshot still unchanged
        assert_eq!(snap.contract_data_state_size(), snap_data_size);
    }

    #[test]
    fn test_arc_strong_counts_tracks_snapshot_sharing() {
        let mut state = InMemorySorobanState::new();
        state.set_last_closed_ledger_seq(100);

        // Fresh state: sole owner of both Arcs.
        let (data_count, code_count) = state.arc_strong_counts();
        assert_eq!(data_count, 1, "fresh state data arc should have count 1");
        assert_eq!(code_count, 1, "fresh state code arc should have count 1");

        // Insert some data so the maps are non-trivial.
        let data1 = make_contract_data_entry([10u8; 32]);
        let code1 = make_contract_code_entry([20u8; 32]);
        state.process_entry_create(&data1, 25, None).unwrap();
        state.process_entry_create(&code1, 25, None).unwrap();

        // Still sole owner.
        let (data_count, code_count) = state.arc_strong_counts();
        assert_eq!(data_count, 1);
        assert_eq!(code_count, 1);

        // Take a snapshot — now both Arcs are shared.
        let _snap = state.snapshot();
        let (data_count, code_count) = state.arc_strong_counts();
        assert_eq!(data_count, 2, "snapshot should increment data arc count");
        assert_eq!(code_count, 2, "snapshot should increment code arc count");

        // Take another snapshot — triple sharing.
        let _snap2 = state.snapshot();
        let (data_count, code_count) = state.arc_strong_counts();
        assert_eq!(data_count, 3);
        assert_eq!(code_count, 3);

        // Drop one snapshot — back to double.
        drop(_snap);
        let (data_count, code_count) = state.arc_strong_counts();
        assert_eq!(data_count, 2);
        assert_eq!(code_count, 2);

        // Drop the last snapshot — sole owner again.
        drop(_snap2);
        let (data_count, code_count) = state.arc_strong_counts();
        assert_eq!(data_count, 1);
        assert_eq!(code_count, 1);
    }

    /// Test that snapshot mutation only detaches the touched contract data shard.
    ///
    /// Creates two entries that land in different shards (verified via the actual
    /// hash-based shard routing), takes a snapshot, mutates one key, and asserts
    /// only the touched shard detaches while the untouched shard remains shared.
    #[test]
    fn test_snapshot_mutation_detaches_only_touched_contract_data_shard() {
        let mut state = InMemorySorobanState::new();

        // Find two keys that provably land in different shards.
        let (key_a_bytes, shard_a, key_b_bytes, shard_b) = find_two_data_keys_in_different_shards();
        assert_ne!(shard_a, shard_b, "precondition: keys in different shards");

        let entry_a = make_contract_data_entry(key_a_bytes);
        let entry_b = make_contract_data_entry(key_b_bytes);

        state.create_contract_data(entry_a.clone()).unwrap();
        state.create_contract_data(entry_b.clone()).unwrap();
        assert_eq!(state.contract_data_count(), 2);

        // Take a snapshot — all shards are now shared (strong_count=2 for
        // the two occupied shards).
        let snap = state.snapshot();

        // Record shard Arc pointers before mutation.
        let ptr_shard_b_before = Arc::as_ptr(&state.contract_data_entries.shards[shard_b]);

        // Mutate entry in shard_a only.
        let mut updated = make_contract_data_entry(key_a_bytes);
        if let LedgerEntryData::ContractData(cd) = &mut updated.data {
            cd.val = ScVal::I32(999);
        }
        state.update_contract_data(updated).unwrap();

        // The shard containing key_b should still be the same Arc (not cloned).
        let ptr_shard_b_after = Arc::as_ptr(&state.contract_data_entries.shards[shard_b]);
        assert_eq!(
            ptr_shard_b_before, ptr_shard_b_after,
            "untouched shard must remain shared (same Arc pointer)"
        );

        // The touched shard (shard_a) should have detached (new Arc).
        assert_eq!(
            Arc::strong_count(&state.contract_data_entries.shards[shard_a]),
            1,
            "mutated shard must be solely owned after make_mut"
        );

        // Verify snapshot isolation: snapshot still sees original values.
        let snap_key_a = LedgerKey::ContractData(LedgerKeyContractData {
            contract: make_contract_address(),
            key: ScVal::Bytes(stellar_xdr::ScBytes(
                key_a_bytes.to_vec().try_into().unwrap(),
            )),
            durability: ContractDataDurability::Persistent,
        });
        let snap_entry = snap.get(&snap_key_a).unwrap();
        if let LedgerEntryData::ContractData(cd) = &snap_entry.data {
            assert_eq!(cd.val, ScVal::I32(42), "snapshot must see original value");
        }

        // Live state should see updated value
        let live_entry = state.get(&snap_key_a).unwrap();
        if let LedgerEntryData::ContractData(cd) = &live_entry.data {
            assert_eq!(cd.val, ScVal::I32(999), "live state must see updated value");
        }
    }

    /// Test that snapshot mutation only detaches the touched contract code shard.
    #[test]
    fn test_snapshot_mutation_detaches_only_touched_contract_code_shard() {
        let mut state = InMemorySorobanState::new();

        // Find two code keys that land in different shards.
        let (key_a_bytes, shard_a, key_b_bytes, shard_b) = find_two_code_keys_in_different_shards();
        assert_ne!(shard_a, shard_b, "precondition: keys in different shards");

        let entry_a = make_contract_code_entry(key_a_bytes);
        let entry_b = make_contract_code_entry(key_b_bytes);

        state
            .create_contract_code(entry_a.clone(), 25, None)
            .unwrap();
        state
            .create_contract_code(entry_b.clone(), 25, None)
            .unwrap();
        assert_eq!(state.contract_code_count(), 2);

        let snap = state.snapshot();
        let snap_code_size = snap.contract_code_state_size();

        // Record the untouched shard's Arc pointer before mutation.
        let ptr_shard_b_before = Arc::as_ptr(&state.contract_code_entries.shards[shard_b]);

        // Update code in shard_a only.
        let mut updated = make_contract_code_entry(key_a_bytes);
        if let LedgerEntryData::ContractCode(cc) = &mut updated.data {
            cc.code = vec![1u8; 200].try_into().unwrap();
        }
        state.update_contract_code(updated, 25, None).unwrap();

        // The untouched shard must still share the same Arc (not cloned).
        let ptr_shard_b_after = Arc::as_ptr(&state.contract_code_entries.shards[shard_b]);
        assert_eq!(
            ptr_shard_b_before, ptr_shard_b_after,
            "untouched code shard must remain shared (same Arc pointer)"
        );

        // The touched shard must have been detached.
        assert_eq!(
            Arc::strong_count(&state.contract_code_entries.shards[shard_a]),
            1,
            "mutated code shard must be solely owned"
        );

        // Snapshot sizes must be unchanged (snapshot isolation).
        assert_eq!(snap.contract_code_state_size(), snap_code_size);
        assert_eq!(snap.contract_code_count(), 2);

        // Snapshot should still be able to look up the shard_b entry.
        let key_b = LedgerKeyContractCode {
            hash: Hash(key_b_bytes),
        };
        assert!(
            snap.get_contract_code(&key_b).is_some(),
            "snapshot must still see untouched shard entry"
        );
    }

    /// Test that arc_strong_counts reports the maximum shared per-shard refcount.
    #[test]
    fn test_arc_strong_counts_reports_max_shared_shard_refcount() {
        let mut state = InMemorySorobanState::new();

        // Insert entries so at least one shard is occupied.
        let data1 = make_contract_data_entry([10u8; 32]);
        state.create_contract_data(data1).unwrap();

        // Fresh state: sole owner.
        let (d, c) = state.arc_strong_counts();
        assert_eq!(d, 1);
        assert_eq!(c, 1);

        // Hold two snapshots — the occupied data shard should have count 3.
        let _snap1 = state.snapshot();
        let _snap2 = state.snapshot();
        let (d, _c) = state.arc_strong_counts();
        assert_eq!(d, 3, "two snapshots + original = 3 for the occupied shard");

        // Mutate the entry — this detaches the shard from the snapshots.
        let mut updated = make_contract_data_entry([10u8; 32]);
        if let LedgerEntryData::ContractData(cd) = &mut updated.data {
            cd.val = ScVal::I32(999);
        }
        state.update_contract_data(updated).unwrap();

        // After mutation, the live shard for the mutated key now has a fresh Arc
        // (count=1 for that shard). But other shards that were snapshot'd still
        // have count 3 (the empty shards are still shared).
        // max_arc_strong_count reports the max across all shards.
        let (d, _c) = state.arc_strong_counts();
        // The mutated shard detached (count=1), but all other shards (including
        // the empty ones) are still shared with both snapshots (count=3).
        assert_eq!(
            d, 3,
            "max count should be 3 from the untouched shared shards"
        );

        // Drop snapshots — all counts return to 1.
        drop(_snap1);
        drop(_snap2);
        let (d, c) = state.arc_strong_counts();
        assert_eq!(d, 1);
        assert_eq!(c, 1);
    }
}
