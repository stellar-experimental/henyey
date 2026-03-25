//! Soroban Host execution integration.
//!
//! This module provides the integration between our ledger state and the
//! soroban-env-host crate for executing Soroban smart contracts.

use std::rc::Rc;

use henyey_common::protocol::{protocol_version_starts_from, ProtocolVersion};
use sha2::{Digest, Sha256};

// Use soroban-env-host types for Host interaction
use soroban_env_host24::xdr::ReadXdr as ReadXdrP24;
use soroban_env_host24::{
    budget::{AsBudget, Budget},
    fees::compute_rent_fee,
    storage::{EntryWithLiveUntil, SnapshotSource},
    vm::VersionedContractCodeCostInputs,
    CompilationContext, ErrorHandler, HostError as HostErrorP24, LedgerInfo as LedgerInfoP24,
    ModuleCache,
};
use soroban_env_host25::HostError as HostErrorP25;
use soroban_env_host_p24 as soroban_env_host24;
use soroban_env_host_p25 as soroban_env_host25;

// P25 module cache types
use soroban_env_host25::{
    budget::AsBudget as AsBudgetP25,
    vm::VersionedContractCodeCostInputs as VersionedContractCodeCostInputsP25,
    CompilationContext as CompilationContextP25, ErrorHandler as ErrorHandlerP25,
    ModuleCache as ModuleCacheP25,
};

// After XDR alignment: our workspace stellar-xdr 25.0.0 is the same crate as
// soroban-env-host P25's transitive stellar-xdr 25.0.0, so the Rust types are
// identical and no conversion is needed for the P25 path.
use stellar_xdr::curr::{
    AccountId, DiagnosticEvent, HostFunction, LedgerEntry, LedgerKey, Limits, ReadXdr, ScVal,
    SorobanAuthorizationEntry, SorobanTransactionData, SorobanTransactionDataExt, WriteXdr,
};

use super::error::convert_host_error_p24_to_p25;
use super::SorobanConfig;
use crate::state::LedgerStateManager;
use crate::validation::LedgerContext;

use henyey_common::xdr_encoded_len as xdr_encoded_len_usize;

/// Compute the XDR-encoded byte length as u32 (convenience wrapper).
fn xdr_encoded_len(val: &impl WriteXdr) -> u32 {
    xdr_encoded_len_usize(val) as u32
}

/// Return type for `get_archived_with_restore_info` (p24 version).
/// Contains: (entry, live_until, live_bl_restore_info)
type ArchivedWithRestoreInfoP24 = Option<(
    Rc<soroban_env_host24::xdr::LedgerEntry>,
    Option<u32>,
    Option<super::protocol::LiveBucketListRestore>,
)>;

/// Return type for `get_archived_with_restore_info` (p25 version).
/// Contains: (entry, live_until, live_bl_restore_info)
type ArchivedWithRestoreInfoP25 = Option<(
    Rc<LedgerEntry>,
    Option<u32>,
    Option<super::protocol::LiveBucketListRestore>,
)>;

/// A ledger entry paired with its optional TTL (live_until ledger sequence).
pub type EntryWithTtl = (Rc<LedgerEntry>, Option<u32>);

/// Macro to generate `extract_rent_changes_from_typed` for both P24 and P25 host crates.
/// The logic is identical — only the qualifying crate path differs.
macro_rules! define_extract_rent_changes {
    ($fn_name:ident, $host_crate:ident) => {
        fn $fn_name(
            ledger_changes: &[$host_crate::e2e_invoke::TypedLedgerEntryChange],
        ) -> Vec<$host_crate::fees::LedgerEntryRentChange> {
            use $host_crate::e2e_invoke::TypedLedgerEntryChange;
            use $host_crate::fees::LedgerEntryRentChange;

            ledger_changes
                .iter()
                .filter_map(|entry_change: &TypedLedgerEntryChange| {
                    if let Some(ttl_change) = &entry_change.ttl_change {
                        let new_size_bytes_for_rent = if entry_change.new_entry.is_some() {
                            entry_change.new_entry_size_bytes_for_rent
                        } else {
                            entry_change.old_entry_size_bytes_for_rent
                        };

                        if ttl_change.old_live_until_ledger >= ttl_change.new_live_until_ledger
                            && entry_change.old_entry_size_bytes_for_rent >= new_size_bytes_for_rent
                        {
                            return None;
                        }
                        Some(LedgerEntryRentChange {
                            is_persistent: ttl_change.durability
                                == $host_crate::xdr::ContractDataDurability::Persistent,
                            is_code_entry: ttl_change.entry_type
                                == $host_crate::xdr::LedgerEntryType::ContractCode,
                            old_size_bytes: entry_change.old_entry_size_bytes_for_rent,
                            new_size_bytes: new_size_bytes_for_rent,
                            old_live_until_ledger: ttl_change.old_live_until_ledger,
                            new_live_until_ledger: ttl_change.new_live_until_ledger,
                        })
                    } else {
                        None
                    }
                })
                .collect()
        }
    };
}

define_extract_rent_changes!(extract_rent_changes_from_typed, soroban_env_host25);
define_extract_rent_changes!(extract_rent_changes_from_typed_p24, soroban_env_host24);

/// Derive a fallback PRNG seed from ledger context when no explicit seed is provided.
fn derive_fallback_prng_seed(context: &LedgerContext) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(context.network_id.0 .0);
    hasher.update(context.sequence.to_le_bytes());
    hasher.update(context.close_time.to_le_bytes());
    hasher.finalize().into()
}

/// Extract archived entry restoration indices from SorobanTransactionData.
fn extract_restored_indices(
    soroban_data: &SorobanTransactionData,
) -> (Vec<u32>, std::collections::HashSet<u32>) {
    let indices: Vec<u32> = match &soroban_data.ext {
        SorobanTransactionDataExt::V1(ext) => {
            ext.archived_soroban_entries.iter().copied().collect()
        }
        SorobanTransactionDataExt::V0 => Vec::new(),
    };
    let set: std::collections::HashSet<u32> = indices.iter().copied().collect();
    (indices, set)
}

/// Result of Soroban host function execution.
pub struct SorobanExecutionResult {
    /// The return value of the function.
    pub return_value: ScVal,
    /// Storage changes made during execution.
    pub storage_changes: Vec<StorageChange>,
    /// Contract and system events emitted during execution.
    pub contract_events: Vec<stellar_xdr::curr::ContractEvent>,
    /// Diagnostic events emitted during execution.
    pub diagnostic_events: Vec<DiagnosticEvent>,
    /// CPU instructions consumed.
    pub cpu_insns: u64,
    /// Memory bytes consumed.
    pub mem_bytes: u64,
    /// Contract events + return value size in bytes.
    pub contract_events_and_return_value_size: u32,
    /// Rent fee charged for storage changes.
    pub rent_fee: i64,
    /// Entries restored from the live BucketList (expired TTL but not yet evicted).
    pub live_bucket_list_restores: Vec<super::protocol::LiveBucketListRestore>,
    /// Indices of entries ACTUALLY restored from hot archive in THIS transaction.
    /// This is a subset of the transaction envelope's archived_soroban_entries,
    /// excluding entries that were already restored by a previous transaction
    /// in the same ledger. Used to determine whether to emit INIT vs LIVE changes.
    pub actual_restored_indices: Vec<u32>,
}

/// Error from Soroban execution that includes consumed resources.
/// This is needed to properly determine TRAPPED vs RESOURCE_LIMIT_EXCEEDED
/// based on whether actual consumption exceeded specified limits.
pub struct SorobanExecutionError {
    /// The underlying host error.
    pub host_error: HostErrorP25,
    /// CPU instructions consumed before failure.
    pub cpu_insns_consumed: u64,
    /// Memory bytes consumed before failure.
    pub mem_bytes_consumed: u64,
}

/// A single storage change from Soroban execution.
pub struct StorageChange {
    /// The ledger key.
    pub key: LedgerKey,
    /// The new entry (None if deleted or read-only).
    pub new_entry: Option<LedgerEntry>,
    /// The new live_until ledger (for TTL).
    pub live_until: Option<u32>,
    /// Whether the TTL was extended (new > old).
    pub ttl_extended: bool,
    /// Whether the entry was included due to rent calculations (old_entry_size_bytes_for_rent > 0).
    /// Rent-related read-only entries should still emit TTL updates.
    pub is_rent_related: bool,
    /// Whether this is a read-only entry with only a TTL change (no data modification).
    /// Such changes should be applied to state (bucket list) but NOT included in transaction meta.
    /// This matches stellar-core's behavior per CAP-0063: read-only TTL bumps are accumulated
    /// separately and flushed at write barriers, not in individual transaction meta.
    pub is_read_only_ttl_bump: bool,
}

/// Persistent module cache that can be reused across transactions.
///
/// This enum wraps either a P24 or P25 module cache, allowing it to be
/// passed through the execution layer without knowing the protocol version
/// upfront. The cache is populated once at ledger start and reused for all
/// transactions in that ledger, matching stellar-core's SharedModuleCacheCompiler.
#[derive(Clone)]
pub enum PersistentModuleCache {
    /// Protocol 24 module cache
    P24(ModuleCache),
    /// Protocol 25+ module cache
    P25(ModuleCacheP25),
}

impl PersistentModuleCache {
    /// Create a new empty P24 cache.
    pub fn new_p24() -> Option<Self> {
        let ctx = WasmCompilationContext::new();
        ModuleCache::new(&ctx).ok().map(PersistentModuleCache::P24)
    }

    /// Create a new empty P25 cache.
    pub fn new_p25() -> Option<Self> {
        let ctx = WasmCompilationContextP25::new();
        ModuleCacheP25::new(&ctx)
            .ok()
            .map(PersistentModuleCache::P25)
    }

    /// Create a new cache for the given protocol version.
    pub fn new_for_protocol(protocol_version: u32) -> Option<Self> {
        if protocol_version_starts_from(protocol_version, ProtocolVersion::V25) {
            Self::new_p25()
        } else {
            Self::new_p24()
        }
    }

    /// Add a contract code entry to the cache.
    /// Returns true if compilation succeeded, false otherwise.
    pub fn add_contract(&self, code: &[u8], protocol_version: u32) -> bool {
        match self {
            PersistentModuleCache::P24(cache) => {
                let ctx = WasmCompilationContext::new();
                let contract_id =
                    soroban_env_host24::xdr::Hash(<Sha256 as Digest>::digest(code).into());
                let cost_inputs = VersionedContractCodeCostInputs::V0 {
                    wasm_bytes: code.len(),
                };
                cache
                    .parse_and_cache_module(&ctx, protocol_version, &contract_id, code, cost_inputs)
                    .is_ok()
            }
            PersistentModuleCache::P25(cache) => {
                let ctx = WasmCompilationContextP25::new();
                let contract_id =
                    soroban_env_host25::xdr::Hash(<Sha256 as Digest>::digest(code).into());
                let cost_inputs = VersionedContractCodeCostInputsP25::V0 {
                    wasm_bytes: code.len(),
                };
                cache
                    .parse_and_cache_module(&ctx, protocol_version, &contract_id, code, cost_inputs)
                    .is_ok()
            }
        }
    }

    /// Remove a contract code entry from the cache by its hash.
    ///
    /// This should be called when contract code is evicted (TTL expired) to
    /// prevent unbounded growth of the module cache.
    ///
    /// Returns true if the module was found and removed, false otherwise.
    pub fn remove_contract(&self, hash: &stellar_xdr::curr::Hash) -> bool {
        match self {
            PersistentModuleCache::P24(cache) => {
                let contract_id = soroban_env_host24::xdr::Hash(hash.0);
                cache.remove_module(&contract_id).ok().flatten().is_some()
            }
            PersistentModuleCache::P25(cache) => {
                let contract_id = soroban_env_host25::xdr::Hash(hash.0);
                cache.remove_module(&contract_id).ok().flatten().is_some()
            }
        }
    }

    /// Get the P24 cache if this is a P24 cache.
    pub fn as_p24(&self) -> Option<&ModuleCache> {
        match self {
            PersistentModuleCache::P24(cache) => Some(cache),
            PersistentModuleCache::P25(_) => None,
        }
    }

    /// Get the P25 cache if this is a P25 cache.
    pub fn as_p25(&self) -> Option<&ModuleCacheP25> {
        match self {
            PersistentModuleCache::P24(_) => None,
            PersistentModuleCache::P25(cache) => Some(cache),
        }
    }
}

/// Adapter that provides snapshot access to our ledger state for Soroban (P24).
struct LedgerSnapshotAdapter<'a> {
    state: &'a LedgerStateManager,
    current_ledger: u32,
    hot_archive: Option<&'a dyn super::HotArchiveLookup>,
    ttl_key_cache: Option<&'a super::TtlKeyCache>,
}

impl<'a> LedgerSnapshotAdapter<'a> {
    /// Create a new snapshot adapter with hot archive and TTL key cache.
    pub fn with_hot_archive(
        state: &'a LedgerStateManager,
        current_ledger: u32,
        hot_archive: Option<&'a dyn super::HotArchiveLookup>,
        ttl_key_cache: Option<&'a super::TtlKeyCache>,
    ) -> Self {
        Self {
            state,
            current_ledger,
            hot_archive,
            ttl_key_cache,
        }
    }

    /// Get an archived entry without checking TTL.
    /// Used for entries that are being restored from the hot archive.
    ///
    /// This method first checks the live state (for entries with expired TTL but not yet evicted),
    /// then falls back to the hot archive bucket list (for truly evicted entries).
    pub fn get_archived(
        &self,
        key: &Rc<soroban_env_host24::xdr::LedgerKey>,
    ) -> Result<Option<EntryWithLiveUntil>, HostErrorP24> {
        let current_key = convert_ledger_key_from_p24(key.as_ref()).ok_or_else(|| {
            HostErrorP24::from(soroban_env_host24::Error::from_type_and_code(
                soroban_env_host24::xdr::ScErrorType::Context,
                soroban_env_host24::xdr::ScErrorCode::InternalError,
            ))
        })?;

        // Get TTL but don't check if it's expired - this is for archived entries
        let live_until = get_entry_ttl_with_cache(
            self.state,
            &current_key,
            self.current_ledger,
            self.ttl_key_cache,
        );

        // Use get_entry() to reconstruct the full LedgerEntry with correct
        // last_modified_ledger_seq and ext (sponsorship) metadata.
        // No TTL check - entry might be archived with expired TTL.
        let entry = self.state.get_entry(&current_key);

        // If entry found in live state, return it
        if let Some(e) = entry {
            let entry = convert_ledger_entry_to_p24(&e).ok_or_else(|| {
                HostErrorP24::from(soroban_env_host24::Error::from_type_and_code(
                    soroban_env_host24::xdr::ScErrorType::Context,
                    soroban_env_host24::xdr::ScErrorCode::InternalError,
                ))
            })?;
            return Ok(Some((Rc::new(entry), live_until)));
        }

        // Entry not found in live state - try the hot archive bucket list.
        // This handles the case where the entry was evicted from the live bucket list
        // and is now in the hot archive, waiting to be restored.
        if let Some(hot_archive) = self.hot_archive {
            if let Some(archived_entry) = hot_archive.get(&current_key) {
                tracing::debug!(
                    key_type = ?std::mem::discriminant(&current_key),
                    "P24: Found archived entry in hot archive bucket list"
                );
                // Convert to P24 format
                let entry = convert_ledger_entry_to_p24(&archived_entry).ok_or_else(|| {
                    HostErrorP24::from(soroban_env_host24::Error::from_type_and_code(
                        soroban_env_host24::xdr::ScErrorType::Context,
                        soroban_env_host24::xdr::ScErrorCode::InternalError,
                    ))
                })?;
                // Hot archive entries have no TTL (they are archived/expired)
                return Ok(Some((Rc::new(entry), None)));
            }
        }

        Ok(None)
    }

    /// Get an archived entry in curr (stellar_xdr) format, without P24 conversion.
    ///
    /// This is used to capture the entry data for hot archive restorations so
    /// that read-only restorations (where the host doesn't modify the entry) can
    /// Get an archived entry and check if it's a live BL restore.
    /// Returns (p24 entry, live_until, live_bl_restore info if applicable).
    pub fn get_archived_with_restore_info(
        &self,
        key: &Rc<soroban_env_host24::xdr::LedgerKey>,
        current_key: &LedgerKey,
    ) -> Result<ArchivedWithRestoreInfoP24, HostErrorP24> {
        let result = self.get_archived(key)?;

        match result {
            Some((entry, live_until)) => {
                // Check if this is a live BL restore: entry exists AND TTL is expired
                let live_bl_restore = if let Some(lu) = live_until {
                    if lu < self.current_ledger {
                        // Get the entry with correct metadata
                        let current_entry = self.state.get_entry(current_key);

                        if let Some(e) = current_entry {
                            // Get the TTL entry for the restore info
                            let key_hash =
                                super::get_or_compute_key_hash(self.ttl_key_cache, current_key);
                            let ttl_key =
                                LedgerKey::Ttl(stellar_xdr::curr::LedgerKeyTtl { key_hash });
                            let ttl_entry = self.state.get_entry(&ttl_key);

                            ttl_entry.map(|te| super::protocol::LiveBucketListRestore {
                                key: current_key.clone(),
                                entry: e,
                                ttl_key,
                                ttl_entry: te,
                            })
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                } else {
                    None
                };

                Ok(Some((entry, live_until, live_bl_restore)))
            }
            None => Ok(None),
        }
    }
}

impl<'a> SnapshotSource for LedgerSnapshotAdapter<'a> {
    fn get(
        &self,
        key: &Rc<soroban_env_host24::xdr::LedgerKey>,
    ) -> Result<Option<EntryWithLiveUntil>, HostErrorP24> {
        let current_key = convert_ledger_key_from_p24(key.as_ref()).ok_or_else(|| {
            HostErrorP24::from(soroban_env_host24::Error::from_type_and_code(
                soroban_env_host24::xdr::ScErrorType::Context,
                soroban_env_host24::xdr::ScErrorCode::InternalError,
            ))
        })?;

        // For ContractData and ContractCode, check TTL first.
        // If TTL has expired, the entry is considered to be in the hot archive
        // and not accessible. This mimics stellar-core behavior.
        let live_until = get_entry_ttl_with_cache(
            self.state,
            &current_key,
            self.current_ledger,
            self.ttl_key_cache,
        );

        // Check TTL expiration for contract entries before looking up the entry.
        if matches!(
            current_key,
            LedgerKey::ContractData(_) | LedgerKey::ContractCode(_)
        ) {
            if let Some(ttl) = live_until {
                if ttl < self.current_ledger {
                    tracing::debug!(
                        current_ledger = self.current_ledger,
                        live_until = ttl,
                        "P24 SnapshotSource: Filtering out entry with expired TTL"
                    );
                    return Ok(None);
                }
            } else {
                // No TTL means entry doesn't exist in Soroban state yet
                // (will be created by the contract invocation).
            }
        }

        // Use get_entry() to reconstruct the full LedgerEntry with correct
        // last_modified_ledger_seq and ext (sponsorship) metadata.
        // stellar-core preserves original entry metadata in its InMemorySorobanState.
        let entry = self.state.get_entry(&current_key);

        match entry {
            Some(e) => {
                let entry = convert_ledger_entry_to_p24(&e).ok_or_else(|| {
                    HostErrorP24::from(soroban_env_host24::Error::from_type_and_code(
                        soroban_env_host24::xdr::ScErrorType::Context,
                        soroban_env_host24::xdr::ScErrorCode::InternalError,
                    ))
                })?;
                Ok(Some((Rc::new(entry), live_until)))
            }
            None => Ok(None),
        }
    }
}

/// Adapter that provides snapshot access to our ledger state for Soroban (p25 host).
struct LedgerSnapshotAdapterP25<'a> {
    state: &'a LedgerStateManager,
    current_ledger: u32,
    hot_archive: Option<&'a dyn super::HotArchiveLookup>,
    ttl_key_cache: Option<&'a super::TtlKeyCache>,
}

impl<'a> LedgerSnapshotAdapterP25<'a> {
    /// Create a new snapshot adapter with hot archive and TTL key cache.
    pub fn with_hot_archive(
        state: &'a LedgerStateManager,
        current_ledger: u32,
        hot_archive: Option<&'a dyn super::HotArchiveLookup>,
        ttl_key_cache: Option<&'a super::TtlKeyCache>,
    ) -> Self {
        Self {
            state,
            current_ledger,
            hot_archive,
            ttl_key_cache,
        }
    }

    /// Get an entry using our workspace XDR types (for internal use).
    /// This is separate from the `SnapshotSource::get()` trait impl which uses
    /// soroban-env-host's XDR types.
    pub fn get_local(&self, key: &LedgerKey) -> Result<Option<EntryWithTtl>, HostErrorP25> {
        // For ContractData and ContractCode, check TTL from bucket list snapshot.
        // This matches stellar-core behavior for parallel Soroban execution:
        // - Entries with valid TTL (live_until >= current_ledger): pass to host
        // - Entries with expired TTL (live_until < current_ledger): archived, not accessible
        // - Entries without TTL in bucket list snapshot: created within ledger, not visible
        let live_until =
            get_entry_ttl_with_cache(self.state, key, self.current_ledger, self.ttl_key_cache);

        // Check TTL expiration for contract entries before looking up the entry.
        if matches!(key, LedgerKey::ContractData(_) | LedgerKey::ContractCode(_)) {
            match live_until {
                Some(ttl) if ttl >= self.current_ledger => {} // live, proceed
                _ => return Ok(None),                         // expired or no TTL
            }
        }

        // Use get_entry() to reconstruct the full LedgerEntry with correct
        // last_modified_ledger_seq and ext (sponsorship) metadata.
        let entry = self.state.get_entry(key);

        match entry {
            Some(e) => Ok(Some((Rc::new(e), live_until))),
            None => Ok(None),
        }
    }

    /// Get an archived entry without checking TTL.
    /// Used for entries that are being restored from the hot archive.
    ///
    /// This method first checks the live state (for entries with expired TTL but not yet evicted),
    /// then falls back to the hot archive bucket list (for truly evicted entries).
    ///
    /// Returns entries in our workspace XDR types (not soroban-env-host's types).
    pub fn get_archived(&self, key: &Rc<LedgerKey>) -> Result<Option<EntryWithTtl>, HostErrorP25> {
        // Get TTL but don't check if it's expired - this is for archived entries
        let live_until = get_entry_ttl_with_cache(
            self.state,
            key.as_ref(),
            self.current_ledger,
            self.ttl_key_cache,
        );

        // Use get_entry() to reconstruct the full LedgerEntry with correct
        // last_modified_ledger_seq and ext (sponsorship) metadata.
        let entry = self.state.get_entry(key.as_ref());

        // If entry found in live state, return it
        if let Some(e) = entry {
            return Ok(Some((Rc::new(e), live_until)));
        }

        // Entry not found in live state - try the hot archive bucket list.
        // This handles the case where the entry was evicted from the live bucket list
        // and is now in the hot archive, waiting to be restored.
        if let Some(hot_archive) = self.hot_archive {
            if let Some(archived_entry) = hot_archive.get(key.as_ref()) {
                tracing::debug!(
                    key_type = ?std::mem::discriminant(key.as_ref()),
                    "P25: Found archived entry in hot archive bucket list"
                );
                // Hot archive entries have no TTL (they are archived/expired)
                return Ok(Some((Rc::new(archived_entry), None)));
            }
        }

        Ok(None)
    }

    /// Get an archived entry and check if it's a live BL restore.
    /// Returns (entry, live_until, live_bl_restore_info).
    ///
    /// Returns entries in our workspace XDR types (not soroban-env-host's types).
    pub fn get_archived_with_restore_info(
        &self,
        key: &Rc<LedgerKey>,
    ) -> Result<ArchivedWithRestoreInfoP25, HostErrorP25> {
        let result = self.get_archived(key)?;

        match result {
            Some((entry, live_until)) => {
                // Check if this is a live BL restore: entry exists AND TTL is expired
                let live_bl_restore = if let Some(lu) = live_until {
                    if lu < self.current_ledger {
                        // Get the TTL entry for the restore info
                        let key_hash =
                            super::get_or_compute_key_hash(self.ttl_key_cache, key.as_ref());
                        let ttl_key = LedgerKey::Ttl(stellar_xdr::curr::LedgerKeyTtl { key_hash });
                        let ttl_entry = self.state.get_entry(&ttl_key);

                        ttl_entry.map(|te| super::protocol::LiveBucketListRestore {
                            key: key.as_ref().clone(),
                            entry: entry.as_ref().clone(),
                            ttl_key,
                            ttl_entry: te,
                        })
                    } else {
                        None
                    }
                } else {
                    None
                };

                Ok(Some((entry, live_until, live_bl_restore)))
            }
            None => Ok(None),
        }
    }
}

impl<'a> soroban_env_host25::storage::SnapshotSource for LedgerSnapshotAdapterP25<'a> {
    fn get(
        &self,
        key: &Rc<LedgerKey>,
    ) -> Result<Option<soroban_env_host25::storage::EntryWithLiveUntil>, HostErrorP25> {
        // After XDR alignment: workspace LedgerKey === soroban-env-host P25 LedgerKey,
        // so no conversion is needed. We can use the key directly.
        let live_until = get_entry_ttl_with_cache(
            self.state,
            key.as_ref(),
            self.current_ledger,
            self.ttl_key_cache,
        );

        // Check TTL expiration for contract entries before looking up the entry.
        if matches!(
            key.as_ref(),
            LedgerKey::ContractData(_) | LedgerKey::ContractCode(_)
        ) {
            match live_until {
                Some(ttl) if ttl >= self.current_ledger => {} // live, proceed
                _ => return Ok(None),                         // expired or no TTL
            }
        }

        // Use get_entry() to reconstruct the full LedgerEntry with correct
        // last_modified_ledger_seq and ext (sponsorship) metadata.
        // No conversion needed — workspace types are identical to P25 types.
        let entry = self.state.get_entry(key.as_ref());

        match entry {
            Some(e) => Ok(Some((Rc::new(e), live_until))),
            None => Ok(None),
        }
    }
}

/// Get the TTL for a ledger entry, using the TTL key cache when available.
///
/// This function returns the CURRENT TTL value (after any modifications by earlier
/// transactions in this ledger), not the ledger-start TTL. This matches stellar-core
/// behavior where the Soroban host computes rent fees based on the current state.
///
/// If TX 6 extends an entry's TTL from 682237 → 700457, TX 7 accessing the same entry
/// will see old_live_until=700457 (the post-TX-6 value) and NOT pay rent for the extension.
/// This is the correct behavior because only one transaction should pay rent for a TTL
/// extension in a given ledger.
///
/// Note: TTL emission determination (whether to emit TTL to bucket list) still
/// needs to compare against ledger-start TTL, which is handled separately in
/// the storage_changes filter.
fn get_entry_ttl_with_cache(
    state: &LedgerStateManager,
    key: &LedgerKey,
    current_ledger: u32,
    ttl_key_cache: Option<&super::TtlKeyCache>,
) -> Option<u32> {
    match key {
        LedgerKey::ContractData(_) | LedgerKey::ContractCode(_) => {
            let key_hash = super::get_or_compute_key_hash(ttl_key_cache, key);
            // Use current state TTL which includes updates from earlier TXs in this ledger.
            // This matches stellar-core sequential execution where LedgerTxn reflects all prior
            // changes, including TTL bumps from earlier transactions.
            let ttl = state
                .get_ttl(&key_hash)
                .map(|ttl| ttl.live_until_ledger_seq);
            if let Some(live_until) = ttl {
                if live_until < current_ledger {
                    tracing::debug!(
                        current_ledger,
                        live_until,
                        key_type = if matches!(key, LedgerKey::ContractCode(_)) {
                            "ContractCode"
                        } else {
                            "ContractData"
                        },
                        "Soroban entry TTL is EXPIRED"
                    );
                }
            }
            ttl
        }
        _ => None,
    }
}

fn convert_ledger_key_to_p24(key: &LedgerKey) -> Option<soroban_env_host24::xdr::LedgerKey> {
    let bytes = key.to_xdr(Limits::none()).ok()?;
    soroban_env_host24::xdr::LedgerKey::from_xdr(&bytes, soroban_env_host24::xdr::Limits::none())
        .ok()
}

fn convert_ledger_key_from_p24(key: &soroban_env_host24::xdr::LedgerKey) -> Option<LedgerKey> {
    let bytes =
        soroban_env_host24::xdr::WriteXdr::to_xdr(key, soroban_env_host24::xdr::Limits::none())
            .ok()?;
    LedgerKey::from_xdr(&bytes, Limits::none()).ok()
}

fn convert_ledger_entry_to_p24(
    entry: &LedgerEntry,
) -> Option<soroban_env_host24::xdr::LedgerEntry> {
    let bytes = entry.to_xdr(Limits::none()).ok()?;
    soroban_env_host24::xdr::LedgerEntry::from_xdr(&bytes, soroban_env_host24::xdr::Limits::none())
        .ok()
}

fn convert_contract_cost_params_to_p24(
    params: &stellar_xdr::curr::ContractCostParams,
) -> Option<soroban_env_host24::xdr::ContractCostParams> {
    let bytes = params.to_xdr(Limits::none()).ok()?;
    soroban_env_host24::xdr::ContractCostParams::from_xdr(
        &bytes,
        soroban_env_host24::xdr::Limits::none(),
    )
    .ok()
}

// Note: P25 XDR conversion functions have been removed after XDR alignment.
// The workspace stellar-xdr 25.0.0 and soroban-env-host P25's stellar-xdr 25.0.0
// are the same crate, so all types are identical and no conversion is needed.
// P24 conversion functions above are still required because P24 uses stellar-xdr 24.0.0.

// ── P25→P24 conversion functions for typed API inputs ──
// These convert workspace (P25) types to P24 types via XDR roundtrip.
// This is the irreducible cost of supporting the P24 host with P25 workspace types.

fn convert_host_function_to_p24(
    hf: &HostFunction,
) -> Option<soroban_env_host24::xdr::HostFunction> {
    let bytes = hf.to_xdr(Limits::none()).ok()?;
    soroban_env_host24::xdr::HostFunction::from_xdr(&bytes, soroban_env_host24::xdr::Limits::none())
        .ok()
}

fn convert_soroban_resources_to_p24(
    resources: &stellar_xdr::curr::SorobanResources,
) -> Option<soroban_env_host24::xdr::SorobanResources> {
    let bytes = resources.to_xdr(Limits::none()).ok()?;
    soroban_env_host24::xdr::SorobanResources::from_xdr(
        &bytes,
        soroban_env_host24::xdr::Limits::none(),
    )
    .ok()
}

fn convert_account_id_to_p24(account: &AccountId) -> Option<soroban_env_host24::xdr::AccountId> {
    let bytes = account.to_xdr(Limits::none()).ok()?;
    soroban_env_host24::xdr::AccountId::from_xdr(&bytes, soroban_env_host24::xdr::Limits::none())
        .ok()
}

fn convert_auth_entry_to_p24(
    entry: &SorobanAuthorizationEntry,
) -> Option<soroban_env_host24::xdr::SorobanAuthorizationEntry> {
    let bytes = entry.to_xdr(Limits::none()).ok()?;
    soroban_env_host24::xdr::SorobanAuthorizationEntry::from_xdr(
        &bytes,
        soroban_env_host24::xdr::Limits::none(),
    )
    .ok()
}

fn convert_ttl_entry_to_p24(
    ttl: &stellar_xdr::curr::TtlEntry,
) -> Option<soroban_env_host24::xdr::TtlEntry> {
    let bytes = ttl.to_xdr(Limits::none()).ok()?;
    soroban_env_host24::xdr::TtlEntry::from_xdr(&bytes, soroban_env_host24::xdr::Limits::none())
        .ok()
}

// ── P24→P25 conversion functions for typed API outputs ──

fn convert_sc_val_from_p24(val: &soroban_env_host24::xdr::ScVal) -> Option<ScVal> {
    let bytes =
        soroban_env_host24::xdr::WriteXdr::to_xdr(val, soroban_env_host24::xdr::Limits::none())
            .ok()?;
    ScVal::from_xdr(&bytes, Limits::none()).ok()
}

fn convert_contract_event_from_p24(
    event: &soroban_env_host24::xdr::ContractEvent,
) -> Option<stellar_xdr::curr::ContractEvent> {
    let bytes =
        soroban_env_host24::xdr::WriteXdr::to_xdr(event, soroban_env_host24::xdr::Limits::none())
            .ok()?;
    stellar_xdr::curr::ContractEvent::from_xdr(&bytes, Limits::none()).ok()
}

fn convert_ledger_entry_from_p24(
    entry: &soroban_env_host24::xdr::LedgerEntry,
) -> Option<LedgerEntry> {
    let bytes =
        soroban_env_host24::xdr::WriteXdr::to_xdr(entry, soroban_env_host24::xdr::Limits::none())
            .ok()?;
    LedgerEntry::from_xdr(&bytes, Limits::none()).ok()
}

fn convert_ledger_key_from_p24_to_p25(
    key: &soroban_env_host24::xdr::LedgerKey,
) -> Option<LedgerKey> {
    let bytes =
        soroban_env_host24::xdr::WriteXdr::to_xdr(key, soroban_env_host24::xdr::Limits::none())
            .ok()?;
    LedgerKey::from_xdr(&bytes, Limits::none()).ok()
}

// extract_rent_changes_from_typed_p24 is generated by the define_extract_rent_changes! macro above.

/// Macro to define a `WasmCompilationContext` struct for a given protocol version.
///
/// Each version needs: the struct newtype wrapping its Budget, trait impls for
/// `ErrorHandler`, `AsBudget`, and `CompilationContext`. The implementations are
/// identical except for the host crate path and type aliases.
macro_rules! define_wasm_compilation_context {
    ($struct_name:ident, $host_crate:ident, $error_handler:path, $as_budget:path, $compilation_ctx:path, $host_error:ty) => {
        #[derive(Clone)]
        struct $struct_name($host_crate::budget::Budget);

        impl $struct_name {
            /// Create a new compilation context with very high budget limits.
            /// We use 10 billion CPU instructions and 1GB memory to ensure compilation
            /// never fails due to budget constraints. The actual compilation cost is
            /// typically much lower, but we want to match stellar-core behavior which doesn't
            /// meter compilation at all.
            fn new() -> Self {
                let budget = $host_crate::budget::Budget::try_from_configs(
                    10_000_000_000,     // 10 billion CPU instructions
                    1_000_000_000,      // 1 GB memory
                    Default::default(), // Default CPU cost params
                    Default::default(), // Default memory cost params
                )
                .unwrap_or_else(|_| $host_crate::budget::Budget::default());
                Self(budget)
            }
        }

        impl $error_handler for $struct_name {
            fn map_err<T, E>(&self, res: Result<T, E>) -> Result<T, $host_error>
            where
                $host_crate::Error: From<E>,
                E: std::fmt::Debug,
            {
                res.map_err(<$host_error>::from)
            }

            fn error(
                &self,
                error: $host_crate::Error,
                _msg: &str,
                _args: &[$host_crate::Val],
            ) -> $host_error {
                <$host_error>::from(error)
            }
        }

        impl $as_budget for $struct_name {
            fn as_budget(&self) -> &$host_crate::budget::Budget {
                &self.0
            }
        }

        impl $compilation_ctx for $struct_name {}
    };
}

// Context for pre-compiling WASM modules outside of transaction execution.
// This mimics how stellar-core pre-compiles all contracts with an unlimited budget.
// We use very high budget limits (10B CPU, 1GB memory) to ensure compilation never fails
// due to budget constraints. stellar-core's SharedModuleCacheCompiler compiles
// without any budget metering.
define_wasm_compilation_context!(
    WasmCompilationContext,
    soroban_env_host24,
    ErrorHandler,
    AsBudget,
    CompilationContext,
    HostErrorP24
);

// P25 version of the compilation context.
define_wasm_compilation_context!(
    WasmCompilationContextP25,
    soroban_env_host25,
    ErrorHandlerP25,
    AsBudgetP25,
    CompilationContextP25,
    HostErrorP25
);

/// Execute a Soroban host function with an optional pre-populated module cache.
///
/// This is the same as `execute_host_function` but accepts an optional persistent
/// module cache. When provided, the cache is reused across transactions, avoiding
/// repeated WASM compilation. This matches stellar-core's SharedModuleCacheCompiler.
///
/// # Arguments
///
/// * `host_function` - The host function to execute
/// * `auth_entries` - Authorization entries for the invocation
/// * `source` - Source account for the transaction
/// * `state` - Ledger state manager for reading entries
/// * `context` - Ledger context with sequence, close time, etc.
/// * `soroban_data` - Soroban transaction data with footprint and resources
/// * `soroban_config` - Network configuration with cost parameters
/// * `module_cache` - Optional pre-populated module cache for WASM reuse
/// * `hot_archive` - Optional hot archive lookup for Protocol 23+ entry restoration
#[allow(clippy::too_many_arguments)]
pub fn execute_host_function_with_cache(
    host_function: &HostFunction,
    auth_entries: &[SorobanAuthorizationEntry],
    source: &AccountId,
    state: &LedgerStateManager,
    context: &LedgerContext,
    soroban_data: &SorobanTransactionData,
    soroban_config: &SorobanConfig,
    module_cache: Option<&PersistentModuleCache>,
    hot_archive: Option<&dyn super::HotArchiveLookup>,
    ttl_key_cache: Option<&super::TtlKeyCache>,
) -> Result<SorobanExecutionResult, SorobanExecutionError> {
    if protocol_version_starts_from(context.protocol_version, ProtocolVersion::V25) {
        let cache = module_cache
            .unwrap_or_else(|| panic!("Module cache must be provided for Soroban TX execution"));
        let p25_cache = cache.as_p25().unwrap_or_else(|| {
            panic!(
                "Module cache is not P25 but protocol version is {}",
                context.protocol_version
            )
        });
        return execute_host_function_p25(
            host_function,
            auth_entries,
            source,
            state,
            context,
            soroban_data,
            soroban_config,
            Some(p25_cache),
            hot_archive,
            ttl_key_cache,
        );
    }
    let cache = module_cache
        .unwrap_or_else(|| panic!("Module cache must be provided for Soroban TX execution"));
    let p24_cache = cache.as_p24().unwrap_or_else(|| {
        panic!(
            "Module cache is not P24 but protocol version is {}",
            context.protocol_version
        )
    });
    tracing::debug!("Dispatching to P24 path");
    execute_host_function_p24(
        host_function,
        auth_entries,
        source,
        state,
        context,
        soroban_data,
        soroban_config,
        Some(p24_cache),
        hot_archive,
        ttl_key_cache,
    )
}

/// Create a setup error with zero consumed resources (for errors before budget exists).
fn make_xdr_setup_error() -> SorobanExecutionError {
    SorobanExecutionError {
        host_error: HostErrorP25::from(soroban_env_host25::Error::from_type_and_code(
            soroban_env_host25::xdr::ScErrorType::Context,
            soroban_env_host25::xdr::ScErrorCode::InternalError,
        )),
        cpu_insns_consumed: 0,
        mem_bytes_consumed: 0,
    }
}

#[allow(clippy::too_many_arguments)]
fn execute_host_function_p24(
    host_function: &HostFunction,
    auth_entries: &[SorobanAuthorizationEntry],
    source: &AccountId,
    state: &LedgerStateManager,
    context: &LedgerContext,
    soroban_data: &SorobanTransactionData,
    soroban_config: &SorobanConfig,
    existing_cache: Option<&ModuleCache>,
    hot_archive: Option<&dyn super::HotArchiveLookup>,
    ttl_key_cache: Option<&super::TtlKeyCache>,
) -> Result<SorobanExecutionResult, SorobanExecutionError> {
    // Create budget with network cost parameters.
    let instruction_limit = soroban_data.resources.instructions as u64;
    let memory_limit = soroban_config.tx_max_memory_bytes;

    let budget = if soroban_config.has_valid_cost_params() {
        let cpu_cost_params = convert_contract_cost_params_to_p24(&soroban_config.cpu_cost_params)
            .ok_or_else(make_xdr_setup_error)?;
        let mem_cost_params = convert_contract_cost_params_to_p24(&soroban_config.mem_cost_params)
            .ok_or_else(make_xdr_setup_error)?;
        Budget::try_from_configs(
            instruction_limit,
            memory_limit,
            cpu_cost_params,
            mem_cost_params,
        )
        .map_err(|e| SorobanExecutionError {
            host_error: convert_host_error_p24_to_p25(e),
            cpu_insns_consumed: 0,
            mem_bytes_consumed: 0,
        })?
    } else {
        tracing::warn!("Using default Soroban budget - cost parameters not loaded from network.");
        Budget::default()
    };

    // Build ledger info
    let ledger_info = LedgerInfoP24 {
        protocol_version: context.protocol_version,
        sequence_number: context.sequence,
        timestamp: context.close_time,
        network_id: context.network_id.0 .0,
        base_reserve: context.base_reserve,
        min_temp_entry_ttl: soroban_config.min_temp_entry_ttl,
        min_persistent_entry_ttl: soroban_config.min_persistent_entry_ttl,
        max_entry_ttl: soroban_config.max_entry_ttl,
    };
    tracing::debug!(
        protocol_version = context.protocol_version,
        sequence_number = context.sequence,
        timestamp = context.close_time,
        instruction_limit,
        memory_limit,
        has_cost_params = soroban_config.has_valid_cost_params(),
        "P24: Soroban host ledger info configured"
    );

    let base_prng_seed: [u8; 32] = if let Some(prng_seed) = context.soroban_prng_seed {
        prng_seed
    } else {
        tracing::warn!("P24: Using fallback PRNG seed - results may differ from stellar-core");
        derive_fallback_prng_seed(context)
    };

    // ── Convert P25 (workspace) types to P24 types for the typed API ──
    let host_function_p24 =
        convert_host_function_to_p24(host_function).ok_or_else(make_xdr_setup_error)?;
    let resources_p24 = convert_soroban_resources_to_p24(&soroban_data.resources)
        .ok_or_else(make_xdr_setup_error)?;
    let source_p24 = convert_account_id_to_p24(source).ok_or_else(make_xdr_setup_error)?;
    let auth_entries_p24: Vec<soroban_env_host24::xdr::SorobanAuthorizationEntry> = auth_entries
        .iter()
        .map(|e| convert_auth_entry_to_p24(e).ok_or_else(make_xdr_setup_error))
        .collect::<Result<_, _>>()?;

    let (restored_rw_entry_indices, restored_indices_set) = extract_restored_indices(soroban_data);

    // Create snapshot adapter with hot archive access
    let snapshot = LedgerSnapshotAdapter::with_hot_archive(
        state,
        context.sequence,
        hot_archive,
        ttl_key_cache,
    );

    // ── Build typed ledger entries for P24 ──
    // Build (Rc<P24::LedgerEntry>, Option<Rc<P24::TtlEntry>>) pairs.
    let mut typed_ledger_entries: Vec<(
        Rc<soroban_env_host24::xdr::LedgerEntry>,
        Option<Rc<soroban_env_host24::xdr::TtlEntry>>,
    )> = Vec::new();
    let current_ledger = context.sequence;

    // Helper to build an Rc<P24::TtlEntry> for a given workspace key and live_until.
    let build_ttl_entry_p24 = |key: &LedgerKey,
                               live_until: Option<u32>|
     -> Option<Rc<soroban_env_host24::xdr::TtlEntry>> {
        let needs_ttl = matches!(key, LedgerKey::ContractData(_) | LedgerKey::ContractCode(_));
        if let Some(lu) = live_until {
            let key_hash = super::get_or_compute_key_hash(ttl_key_cache, key);
            let ttl_entry = stellar_xdr::curr::TtlEntry {
                key_hash,
                live_until_ledger_seq: lu,
            };
            convert_ttl_entry_to_p24(&ttl_entry).map(Rc::new)
        } else if needs_ttl {
            let key_hash = super::get_or_compute_key_hash(ttl_key_cache, key);
            let ttl_entry = stellar_xdr::curr::TtlEntry {
                key_hash,
                live_until_ledger_seq: current_ledger,
            };
            convert_ttl_entry_to_p24(&ttl_entry).map(Rc::new)
        } else {
            None
        }
    };

    // Read-only footprint entries
    for key in soroban_data.resources.footprint.read_only.iter() {
        let key_p24 = convert_ledger_key_to_p24(key).ok_or_else(make_xdr_setup_error)?;
        if let Some((entry, live_until)) =
            snapshot
                .get(&Rc::new(key_p24))
                .map_err(|e| SorobanExecutionError {
                    host_error: convert_host_error_p24_to_p25(e),
                    cpu_insns_consumed: 0,
                    mem_bytes_consumed: 0,
                })?
        {
            let ttl = build_ttl_entry_p24(key, live_until);
            typed_ledger_entries.push((entry, ttl));
        }
    }

    if !restored_indices_set.is_empty() {
        tracing::debug!(
            restored_count = restored_rw_entry_indices.len(),
            restored_indices = ?restored_rw_entry_indices,
            "P24: Transaction has archived entries to restore"
        );
    }

    let mut live_bl_restores: Vec<super::protocol::LiveBucketListRestore> = Vec::new();
    let mut actual_restored_indices: Vec<u32> = Vec::new();

    // Read-write footprint entries (with archived entry restoration)
    for (idx, key) in soroban_data
        .resources
        .footprint
        .read_write
        .iter()
        .enumerate()
    {
        let key_p24 = convert_ledger_key_to_p24(key).ok_or_else(make_xdr_setup_error)?;
        let is_being_restored = restored_indices_set.contains(&(idx as u32));
        if is_being_restored {
            let result = snapshot
                .get_archived_with_restore_info(&Rc::new(key_p24), key)
                .map_err(|e| SorobanExecutionError {
                    host_error: convert_host_error_p24_to_p25(e),
                    cpu_insns_consumed: 0,
                    mem_bytes_consumed: 0,
                })?;
            if let Some((entry, live_until, live_bl_restore)) = result {
                let is_actually_archived = match live_until {
                    None => true,
                    Some(lu) => lu < context.sequence,
                };

                if is_actually_archived {
                    let restored_live_until =
                        Some(context.sequence + soroban_config.min_persistent_entry_ttl - 1);
                    tracing::debug!(
                        idx = idx,
                        key_type = ?std::mem::discriminant(key),
                        old_live_until = ?live_until,
                        restored_live_until = ?restored_live_until,
                        is_live_bl_restore = live_bl_restore.is_some(),
                        "P24: Archived entry found for restoration"
                    );
                    let ttl = build_ttl_entry_p24(key, restored_live_until);
                    typed_ledger_entries.push((entry, ttl));
                    actual_restored_indices.push(idx as u32);
                    if let Some(restore) = live_bl_restore {
                        live_bl_restores.push(restore);
                    }
                } else {
                    tracing::debug!(
                        idx = idx,
                        key_type = ?std::mem::discriminant(key),
                        live_until = ?live_until,
                        "P24: Entry marked for restore but already live (restored by earlier TX)"
                    );
                    let ttl = build_ttl_entry_p24(key, live_until);
                    typed_ledger_entries.push((entry, ttl));
                }
            }
        } else {
            if let Some((entry, live_until)) =
                snapshot
                    .get(&Rc::new(key_p24))
                    .map_err(|e| SorobanExecutionError {
                        host_error: convert_host_error_p24_to_p25(e),
                        cpu_insns_consumed: 0,
                        mem_bytes_consumed: 0,
                    })?
            {
                let ttl = build_ttl_entry_p24(key, live_until);
                typed_ledger_entries.push((entry, ttl));
            }
        }
    }

    // Use existing module cache — it must always be provided.
    let module_cache = existing_cache.unwrap_or_else(|| {
        panic!(
            "P24: Module cache is not available — this is a bug. \
            The persistent module cache should always be initialized before TX execution."
        )
    });
    let module_cache = Some(module_cache.clone());

    // ── Call invoke_host_function_typed() with P24 types ──
    let mut diagnostic_events: Vec<soroban_env_host24::xdr::DiagnosticEvent> = Vec::new();
    let result = match soroban_env_host24::e2e_invoke::invoke_host_function_typed(
        &budget,
        true, // enable_diagnostics
        host_function_p24,
        resources_p24,
        &actual_restored_indices,
        source_p24,
        auth_entries_p24,
        ledger_info,
        typed_ledger_entries.into_iter(),
        base_prng_seed,
        &mut diagnostic_events,
        None, // trace_hook
        module_cache,
    ) {
        Ok(r) => {
            tracing::debug!(
                cpu_consumed = budget.get_cpu_insns_consumed().unwrap_or(0),
                mem_consumed = budget.get_mem_bytes_consumed().unwrap_or(0),
                "P24: e2e_invoke_typed completed successfully"
            );
            r
        }
        Err(e) => {
            tracing::debug!(
                error = %e,
                cpu_consumed = budget.get_cpu_insns_consumed().unwrap_or(0),
                mem_consumed = budget.get_mem_bytes_consumed().unwrap_or(0),
                "P24: e2e_invoke_typed returned error"
            );
            return Err(SorobanExecutionError {
                host_error: convert_host_error_p24_to_p25(e),
                cpu_insns_consumed: budget.get_cpu_insns_consumed().unwrap_or(0),
                mem_bytes_consumed: budget.get_mem_bytes_consumed().unwrap_or(0),
            });
        }
    };

    // ── Process typed result ──
    // Return value: P24 ScVal → P25 ScVal via XDR roundtrip.
    let (return_value, return_value_size) = match result.invoke_result {
        Ok(ref val) => {
            let p25_val = convert_sc_val_from_p24(val).unwrap_or(ScVal::Void);
            let size = xdr_encoded_len(&p25_val);
            (p25_val, size)
        }
        Err(ref e) => {
            tracing::debug!(
                error = %e,
                "P24: e2e_invoke_typed result contained error"
            );
            return Err(SorobanExecutionError {
                host_error: convert_host_error_p24_to_p25(e.clone()),
                cpu_insns_consumed: budget.get_cpu_insns_consumed().unwrap_or(0),
                mem_bytes_consumed: budget.get_mem_bytes_consumed().unwrap_or(0),
            });
        }
    };

    // ── Contract events: P24 ContractEvent → P25 ContractEvent ──
    let mut contract_events = Vec::new();
    let mut contract_events_size = 0u32;
    for host_event in result.events.0.iter() {
        if host_event.failed_call
            || host_event.event.type_ == soroban_env_host24::xdr::ContractEventType::Diagnostic
        {
            continue;
        }
        if let Some(p25_event) = convert_contract_event_from_p24(&host_event.event) {
            let event_size = xdr_encoded_len(&p25_event);
            contract_events_size = contract_events_size.saturating_add(event_size);
            contract_events.push(p25_event);
        }
    }

    // ── Rent: use get_ledger_changes_typed() + extract_rent_changes_from_typed_p24() ──
    let min_live_until_ledger = context.sequence + soroban_config.min_persistent_entry_ttl - 1;
    let typed_ledger_changes = soroban_env_host24::e2e_invoke::get_ledger_changes_typed(
        &budget,
        &result.storage,
        &result.init_storage_map,
        &result.ttl_map,
        min_live_until_ledger,
        &result.restored_keys,
    )
    .map_err(|e| SorobanExecutionError {
        host_error: convert_host_error_p24_to_p25(e),
        cpu_insns_consumed: budget.get_cpu_insns_consumed().unwrap_or(0),
        mem_bytes_consumed: budget.get_mem_bytes_consumed().unwrap_or(0),
    })?;

    let rent_changes_p24 = extract_rent_changes_from_typed_p24(&typed_ledger_changes);

    // ── Storage changes: map P24 TypedLedgerEntryChange to StorageChange ──
    let storage_changes: Vec<crate::soroban::StorageChange> = typed_ledger_changes
        .into_iter()
        .filter_map(|change| {
            let is_deletion = !change.read_only && change.new_entry.is_none();
            let is_modification = change.new_entry.is_some();
            let is_rent_related = change.old_entry_size_bytes_for_rent > 0;

            // Convert P24 key to P25 for TTL lookup.
            let p25_key = convert_ledger_key_from_p24_to_p25(&change.key)?;

            let ttl_extended = change
                .ttl_change
                .as_ref()
                .map(|ttl| {
                    let key_hash = super::get_or_compute_key_hash(ttl_key_cache, &p25_key);
                    let ledger_start_ttl = state.get_ttl_at_ledger_start(&key_hash).unwrap_or(0);
                    ttl.new_live_until_ledger > ledger_start_ttl
                })
                .unwrap_or(false);

            let is_read_only_ttl_bump = change.read_only && !is_modification && ttl_extended;

            if is_modification || is_deletion || ttl_extended {
                let new_entry = change
                    .new_entry
                    .and_then(|rc| convert_ledger_entry_from_p24(&rc));
                let live_until = change.ttl_change.map(|ttl| ttl.new_live_until_ledger);
                Some(StorageChange {
                    key: p25_key,
                    new_entry,
                    live_until,
                    ttl_extended,
                    is_rent_related,
                    is_read_only_ttl_bump,
                })
            } else {
                None
            }
        })
        .collect();

    // Get budget consumption
    let cpu_insns = budget.get_cpu_insns_consumed().unwrap_or(0);
    let mem_bytes = budget.get_mem_bytes_consumed().unwrap_or(0);
    let contract_events_and_return_value_size =
        contract_events_size.saturating_add(return_value_size);
    let rent_fee_config = rent_fee_config_p25_to_p24(&soroban_config.rent_fee_config);
    let rent_fee = compute_rent_fee(&rent_changes_p24, &rent_fee_config, context.sequence);
    tracing::debug!(
        computed_rent_fee = rent_fee,
        rent_changes_count = rent_changes_p24.len(),
        ledger_seq = context.sequence,
        "P24: Computed rent fee"
    );
    let diagnostic_events = convert_diagnostic_events_p24(diagnostic_events);

    Ok(SorobanExecutionResult {
        return_value,
        storage_changes,
        contract_events,
        diagnostic_events,
        cpu_insns,
        mem_bytes,
        contract_events_and_return_value_size,
        rent_fee,
        live_bucket_list_restores: live_bl_restores,
        actual_restored_indices,
    })
}

#[allow(clippy::too_many_arguments)]
fn execute_host_function_p25(
    host_function: &HostFunction,
    auth_entries: &[SorobanAuthorizationEntry],
    source: &AccountId,
    state: &LedgerStateManager,
    context: &LedgerContext,
    soroban_data: &SorobanTransactionData,
    soroban_config: &SorobanConfig,
    existing_cache: Option<&ModuleCacheP25>,
    hot_archive: Option<&dyn super::HotArchiveLookup>,
    ttl_key_cache: Option<&super::TtlKeyCache>,
) -> Result<SorobanExecutionResult, SorobanExecutionError> {
    use soroban_env_host25::{budget::Budget, e2e_invoke, fees::compute_rent_fee};

    // Helper to create error with zero consumed resources (for setup errors before budget exists)
    let make_setup_error = |e: HostErrorP25| SorobanExecutionError {
        host_error: e,
        cpu_insns_consumed: 0,
        mem_bytes_consumed: 0,
    };

    // stellar-core passes the per-transaction specified instruction limit directly
    // to the host (mResources.instructions in InvokeHostFunctionOpFrame.cpp line 547).
    // The memory limit comes from the network config (ledger_info.memory_limit).
    let instruction_limit = soroban_data.resources.instructions as u64;
    let memory_limit = soroban_config.tx_max_memory_bytes;

    let budget = if soroban_config.has_valid_cost_params() {
        // After XDR alignment: ContractCostParams from the workspace is the same type
        // as soroban-env-host P25 expects — no conversion needed.
        Budget::try_from_configs(
            instruction_limit,
            memory_limit,
            soroban_config.cpu_cost_params.clone(),
            soroban_config.mem_cost_params.clone(),
        )
        .map_err(make_setup_error)?
    } else {
        tracing::warn!("Using default Soroban budget - cost parameters not loaded from network.");
        Budget::default()
    };

    let ledger_info = soroban_env_host25::LedgerInfo {
        protocol_version: context.protocol_version,
        sequence_number: context.sequence,
        timestamp: context.close_time,
        network_id: context.network_id.0 .0,
        base_reserve: context.base_reserve,
        min_temp_entry_ttl: soroban_config.min_temp_entry_ttl,
        min_persistent_entry_ttl: soroban_config.min_persistent_entry_ttl,
        max_entry_ttl: soroban_config.max_entry_ttl,
    };

    tracing::debug!(
        protocol_version = context.protocol_version,
        sequence_number = context.sequence,
        timestamp = context.close_time,
        instruction_limit,
        memory_limit,
        has_cost_params = soroban_config.has_valid_cost_params(),
        "P25: Soroban host ledger info configured"
    );

    let base_prng_seed: [u8; 32] = if let Some(prng_seed) = context.soroban_prng_seed {
        prng_seed
    } else {
        tracing::warn!("P25: Using fallback PRNG seed - results may differ from stellar-core");
        derive_fallback_prng_seed(context)
    };

    let (restored_rw_entry_indices, restored_indices_set) = extract_restored_indices(soroban_data);

    // Create snapshot with hot archive access for Protocol 23+ entry restoration
    let snapshot = LedgerSnapshotAdapterP25::with_hot_archive(
        state,
        context.sequence,
        hot_archive,
        ttl_key_cache,
    );

    // ── Build typed ledger entries: Vec<(Rc<LedgerEntry>, Option<Rc<TtlEntry>>)> ──
    // Pass typed pairs directly to the typed API — no XDR serialization needed.
    // Budget parity with stellar-core is maintained because the vendored
    // invoke_host_function_typed() now charges ValDeser for each typed input
    // (see charge_val_deser_for_typed in e2e_invoke.rs, fixing VE-14).
    let mut typed_ledger_entries: Vec<(Rc<LedgerEntry>, Option<Rc<stellar_xdr::curr::TtlEntry>>)> =
        Vec::new();
    let current_ledger_p25 = context.sequence;

    // Helper to build an Rc<TtlEntry> for a given key and live_until.
    let build_ttl_entry =
        |key: &LedgerKey, live_until: Option<u32>| -> Option<Rc<stellar_xdr::curr::TtlEntry>> {
            let needs_ttl = matches!(key, LedgerKey::ContractData(_) | LedgerKey::ContractCode(_));
            if let Some(lu) = live_until {
                let key_hash = super::get_or_compute_key_hash(ttl_key_cache, key);
                Some(Rc::new(stellar_xdr::curr::TtlEntry {
                    key_hash,
                    live_until_ledger_seq: lu,
                }))
            } else if needs_ttl {
                // Entry needs TTL but has none — use current ledger as placeholder.
                let key_hash = super::get_or_compute_key_hash(ttl_key_cache, key);
                Some(Rc::new(stellar_xdr::curr::TtlEntry {
                    key_hash,
                    live_until_ledger_seq: current_ledger_p25,
                }))
            } else {
                None
            }
        };

    // Read-only footprint entries
    for key in soroban_data.resources.footprint.read_only.iter() {
        if let Some((entry, live_until)) = snapshot.get_local(key).map_err(make_setup_error)? {
            let ttl = build_ttl_entry(key, live_until);
            typed_ledger_entries.push((entry, ttl));
        }
        // If entry not found, skip it — the typed API's footprint loop will
        // add it to the storage map as None (entry doesn't exist yet).
    }

    if !restored_indices_set.is_empty() {
        tracing::debug!(
            restored_count = restored_rw_entry_indices.len(),
            restored_indices = ?restored_rw_entry_indices,
            "P25: Transaction has archived entries to restore"
        );
    }

    // Track entries restored from live BucketList (expired TTL but not yet evicted)
    let mut live_bl_restores: Vec<super::protocol::LiveBucketListRestore> = Vec::new();

    // Build the ACTUAL list of indices being restored in THIS transaction.
    let mut actual_restored_indices: Vec<u32> = Vec::new();

    // Read-write footprint entries (with archived entry restoration)
    for (idx, key) in soroban_data
        .resources
        .footprint
        .read_write
        .iter()
        .enumerate()
    {
        let is_being_restored = restored_indices_set.contains(&(idx as u32));
        if is_being_restored {
            let result = snapshot
                .get_archived_with_restore_info(&Rc::new(key.clone()))
                .map_err(make_setup_error)?;
            if let Some((entry, live_until, live_bl_restore)) = result {
                let is_actually_archived = match live_until {
                    None => true,
                    Some(lu) => lu < context.sequence,
                };

                if is_actually_archived {
                    let restored_live_until =
                        Some(context.sequence + soroban_config.min_persistent_entry_ttl - 1);
                    tracing::debug!(
                        idx = idx,
                        key_type = ?std::mem::discriminant(key),
                        old_live_until = ?live_until,
                        restored_live_until = ?restored_live_until,
                        is_live_bl_restore = live_bl_restore.is_some(),
                        "P25: Archived entry found for restoration"
                    );
                    let ttl = build_ttl_entry(key, restored_live_until);
                    typed_ledger_entries.push((entry, ttl));
                    actual_restored_indices.push(idx as u32);
                    if let Some(restore) = live_bl_restore {
                        live_bl_restores.push(restore);
                    }
                } else {
                    tracing::debug!(
                        idx = idx,
                        key_type = ?std::mem::discriminant(key),
                        live_until = ?live_until,
                        "P25: Entry marked for restore but already live (restored by earlier TX)"
                    );
                    let ttl = build_ttl_entry(key, live_until);
                    typed_ledger_entries.push((entry, ttl));
                }
            }
            // If archived entry not found, skip it — the typed API's footprint
            // loop will add it to the storage map as None.
        } else {
            if let Some((entry, live_until)) = snapshot.get_local(key).map_err(make_setup_error)? {
                let ttl = build_ttl_entry(key, live_until);
                typed_ledger_entries.push((entry, ttl));
            }
            // If entry not found, skip it — the typed API's footprint loop will
            // add it to the storage map as None (entry doesn't exist yet).
        }
    }

    // Use existing module cache — it must always be provided.
    let module_cache = existing_cache.unwrap_or_else(|| {
        panic!(
            "P25: Module cache is not available — this is a bug. \
            The persistent module cache should always be initialized before TX execution."
        )
    });
    let module_cache = Some(module_cache.clone());

    let mut diagnostic_events: Vec<soroban_env_host25::xdr::DiagnosticEvent> = Vec::new();

    // ── Call invoke_host_function_typed() ──
    // Pass typed values directly — no XDR serialization for inputs.
    // ValDeser budget charges are now applied inside the typed API itself.
    let result = match e2e_invoke::invoke_host_function_typed(
        &budget,
        true, // enable_diagnostics
        host_function.clone(),
        soroban_data.resources.clone(),
        &actual_restored_indices,
        source.clone(),
        auth_entries.to_vec(),
        ledger_info,
        typed_ledger_entries.into_iter(),
        base_prng_seed,
        &mut diagnostic_events,
        None, // trace_hook
        module_cache,
    ) {
        Ok(r) => {
            tracing::debug!(
                cpu_consumed = budget.get_cpu_insns_consumed().unwrap_or(0),
                mem_consumed = budget.get_mem_bytes_consumed().unwrap_or(0),
                "P25: e2e_invoke_typed completed successfully"
            );
            r
        }
        Err(e) => {
            let cpu_insns_consumed = budget.get_cpu_insns_consumed().unwrap_or(0);
            let mem_bytes_consumed = budget.get_mem_bytes_consumed().unwrap_or(0);
            tracing::debug!(
                cpu_consumed = cpu_insns_consumed,
                mem_consumed = mem_bytes_consumed,
                error = %e,
                "P25: e2e_invoke_typed failed"
            );
            for (i, event) in diagnostic_events.iter().enumerate() {
                use soroban_env_host25::xdr::WriteXdr as _;
                if let Ok(encoded) = event.to_xdr(soroban_env_host25::xdr::Limits::none()) {
                    tracing::warn!(
                        event_idx = i,
                        event_hex = hex::encode(&encoded),
                        "P25: Diagnostic event"
                    );
                }
            }
            return Err(SorobanExecutionError {
                host_error: e,
                cpu_insns_consumed,
                mem_bytes_consumed,
            });
        }
    };

    // ── Process typed result ──
    // Return value: already a typed Result<ScVal, HostError>.
    let (return_value, return_value_size) = match result.invoke_result {
        Ok(ref val) => {
            // Compute byte size without heap allocation (counting writer).
            let size = xdr_encoded_len(val);
            (val.clone(), size)
        }
        Err(ref e) => {
            let cpu_insns_consumed = budget.get_cpu_insns_consumed().unwrap_or(0);
            let mem_bytes_consumed = budget.get_mem_bytes_consumed().unwrap_or(0);
            tracing::debug!(
                cpu_consumed = cpu_insns_consumed,
                mem_consumed = mem_bytes_consumed,
                error = %e,
                "P25: e2e_invoke_typed result contained error"
            );
            return Err(SorobanExecutionError {
                host_error: e.clone(),
                cpu_insns_consumed,
                mem_bytes_consumed,
            });
        }
    };

    // ── ValSer budget charges for return value ──
    // The non-typed API calls metered_write_xdr() which charges ValSer as bytes
    // are serialized. The typed API skips serialization, so we must charge
    // explicitly to match stellar-core's budget consumption.
    {
        use soroban_env_host25::xdr::ContractCostType;
        budget
            .charge(ContractCostType::ValSer, Some(return_value_size as u64))
            .map_err(|e| SorobanExecutionError {
                host_error: e,
                cpu_insns_consumed: budget.get_cpu_insns_consumed().unwrap_or(0),
                mem_bytes_consumed: budget.get_mem_bytes_consumed().unwrap_or(0),
            })?;
    }

    // ── Contract events: extract from Events directly ──
    // After XDR alignment, HostEvent.event is our workspace ContractEvent type.
    let mut contract_events = Vec::new();
    let mut contract_events_size = 0u32;
    for host_event in result.events.0.iter() {
        if host_event.failed_call
            || host_event.event.type_ == stellar_xdr::curr::ContractEventType::Diagnostic
        {
            continue;
        }
        // Compute byte size without heap allocation (counting writer).
        let event_size = xdr_encoded_len(&host_event.event);
        contract_events_size = contract_events_size.saturating_add(event_size);
        contract_events.push(host_event.event.clone());
    }

    // ── ValSer budget charges for contract events ──
    // The non-typed API calls encode_contract_events() which charges ValSer for
    // each event via metered_write_xdr(). We charge the total event bytes in one
    // call; the linear cost (29 * bytes) dominates, so this closely matches the
    // incremental charges from the non-typed path.
    {
        use soroban_env_host25::xdr::ContractCostType;
        budget
            .charge(
                ContractCostType::ValSer,
                Some(contract_events_size as u64),
            )
            .map_err(|e| SorobanExecutionError {
                host_error: e,
                cpu_insns_consumed: budget.get_cpu_insns_consumed().unwrap_or(0),
                mem_bytes_consumed: budget.get_mem_bytes_consumed().unwrap_or(0),
            })?;
    }

    // ── Rent: use get_ledger_changes_typed() + extract_rent_changes_from_typed() ──
    let min_live_until_ledger = context.sequence + soroban_config.min_persistent_entry_ttl - 1;
    let typed_ledger_changes = e2e_invoke::get_ledger_changes_typed(
        &budget,
        &result.storage,
        &result.init_storage_map,
        &result.ttl_map,
        min_live_until_ledger,
        &result.restored_keys,
    )
    .map_err(|e| SorobanExecutionError {
        host_error: e,
        cpu_insns_consumed: budget.get_cpu_insns_consumed().unwrap_or(0),
        mem_bytes_consumed: budget.get_mem_bytes_consumed().unwrap_or(0),
    })?;

    let rent_changes = extract_rent_changes_from_typed(&typed_ledger_changes);

    // ── Storage changes: map TypedLedgerEntryChange to StorageChange ──
    let storage_changes: Vec<crate::soroban::StorageChange> = typed_ledger_changes
        .into_iter()
        .filter_map(|change| {
            let is_deletion = !change.read_only && change.new_entry.is_none();
            let is_modification = change.new_entry.is_some();
            let is_rent_related = change.old_entry_size_bytes_for_rent > 0;

            // Determine if TTL was extended from the LEDGER-START perspective.
            // Use typed key directly — no XDR deserialization needed.
            let ttl_extended = change
                .ttl_change
                .as_ref()
                .map(|ttl| {
                    let key_hash = super::get_or_compute_key_hash(ttl_key_cache, &change.key);
                    let ledger_start_ttl = state.get_ttl_at_ledger_start(&key_hash).unwrap_or(0);
                    ttl.new_live_until_ledger > ledger_start_ttl
                })
                .unwrap_or(false);

            // A read-only TTL bump: entry is read-only, wasn't modified, but TTL extended.
            let is_read_only_ttl_bump = change.read_only && !is_modification && ttl_extended;

            if is_modification || is_deletion || ttl_extended {
                // Clone out of Rc — negligible cost vs the XDR serde that was here before.
                let key = (*change.key).clone();
                let new_entry = change.new_entry.map(|rc| (*rc).clone());
                let live_until = change.ttl_change.map(|ttl| ttl.new_live_until_ledger);
                Some(StorageChange {
                    key,
                    new_entry,
                    live_until,
                    ttl_extended,
                    is_rent_related,
                    is_read_only_ttl_bump,
                })
            } else {
                None
            }
        })
        .collect();

    let cpu_insns = budget.get_cpu_insns_consumed().unwrap_or(0);
    let mem_bytes = budget.get_mem_bytes_consumed().unwrap_or(0);
    let contract_events_and_return_value_size =
        contract_events_size.saturating_add(return_value_size);
    let rent_fee = compute_rent_fee(
        &rent_changes,
        &soroban_config.rent_fee_config,
        context.sequence,
    );
    // After XDR alignment: DiagnosticEvent from soroban-env-host P25 is the
    // same type as our workspace DiagnosticEvent — no conversion needed.

    Ok(SorobanExecutionResult {
        return_value,
        storage_changes,
        contract_events,
        diagnostic_events,
        cpu_insns,
        mem_bytes,
        contract_events_and_return_value_size,
        rent_fee,
        live_bucket_list_restores: live_bl_restores,
        actual_restored_indices,
    })
}

fn convert_diagnostic_events_p24(
    events: Vec<soroban_env_host24::xdr::DiagnosticEvent>,
) -> Vec<DiagnosticEvent> {
    events
        .into_iter()
        .filter_map(|event| {
            let bytes = soroban_env_host24::xdr::WriteXdr::to_xdr(
                &event,
                soroban_env_host24::xdr::Limits::none(),
            )
            .ok()?;
            DiagnosticEvent::from_xdr(&bytes, Limits::none()).ok()
        })
        .collect()
}

fn rent_fee_config_p25_to_p24(
    config: &soroban_env_host25::fees::RentFeeConfiguration,
) -> soroban_env_host24::fees::RentFeeConfiguration {
    soroban_env_host24::fees::RentFeeConfiguration {
        fee_per_write_1kb: config.fee_per_write_1kb,
        fee_per_rent_1kb: config.fee_per_rent_1kb,
        fee_per_write_entry: config.fee_per_write_entry,
        persistent_rent_rate_denominator: config.persistent_rent_rate_denominator,
        temporary_rent_rate_denominator: config.temporary_rent_rate_denominator,
    }
}

// convert_diagnostic_events_p25 has been removed after XDR alignment.
// DiagnosticEvent is now the same type between workspace and soroban-env-host P25.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::soroban::compute_key_hash;
    use stellar_xdr::curr::{Hash, LedgerEntryData};

    #[test]
    fn test_compute_key_hash() {
        let key = LedgerKey::ContractCode(stellar_xdr::curr::LedgerKeyContractCode {
            hash: Hash([1u8; 32]),
        });
        let hash = compute_key_hash(&key);
        assert_ne!(hash.0, [0u8; 32]);
    }

    /// Test compute_key_hash produces different hashes for different keys.
    #[test]
    fn test_compute_key_hash_different_keys() {
        let key1 = LedgerKey::ContractCode(stellar_xdr::curr::LedgerKeyContractCode {
            hash: Hash([1u8; 32]),
        });
        let key2 = LedgerKey::ContractCode(stellar_xdr::curr::LedgerKeyContractCode {
            hash: Hash([2u8; 32]),
        });

        let hash1 = compute_key_hash(&key1);
        let hash2 = compute_key_hash(&key2);

        assert_ne!(hash1, hash2);
    }

    /// Test compute_key_hash is deterministic.
    #[test]
    fn test_compute_key_hash_deterministic() {
        let key = LedgerKey::ContractCode(stellar_xdr::curr::LedgerKeyContractCode {
            hash: Hash([42u8; 32]),
        });

        let hash1 = compute_key_hash(&key);
        let hash2 = compute_key_hash(&key);

        assert_eq!(hash1, hash2);
    }

    /// Test compute_key_hash with ContractData key.
    #[test]
    fn test_compute_key_hash_contract_data() {
        use stellar_xdr::curr::{ContractDataDurability, LedgerKeyContractData, ScAddress};

        let key = LedgerKey::ContractData(LedgerKeyContractData {
            contract: ScAddress::Contract(stellar_xdr::curr::ContractId(Hash([3u8; 32]))),
            key: ScVal::U32(100),
            durability: ContractDataDurability::Persistent,
        });

        let hash = compute_key_hash(&key);
        assert_ne!(hash.0, [0u8; 32]);
    }

    /// Test StorageChange struct with new entry (create/update).
    #[test]
    fn test_storage_change_with_new_entry() {
        let change = StorageChange {
            key: LedgerKey::ContractCode(stellar_xdr::curr::LedgerKeyContractCode {
                hash: Hash([4u8; 32]),
            }),
            new_entry: Some(LedgerEntry {
                last_modified_ledger_seq: 100,
                data: LedgerEntryData::ContractCode(stellar_xdr::curr::ContractCodeEntry {
                    ext: stellar_xdr::curr::ContractCodeEntryExt::V0,
                    hash: Hash([4u8; 32]),
                    code: vec![0xDE, 0xAD].try_into().unwrap(),
                }),
                ext: stellar_xdr::curr::LedgerEntryExt::V0,
            }),
            live_until: Some(1000),
            ttl_extended: false,
            is_rent_related: false,
            is_read_only_ttl_bump: false,
        };

        assert!(change.new_entry.is_some());
        assert_eq!(change.live_until, Some(1000));
    }

    /// Test StorageChange struct with TTL extension.
    #[test]
    fn test_storage_change_ttl_extended() {
        let change = StorageChange {
            key: LedgerKey::ContractCode(stellar_xdr::curr::LedgerKeyContractCode {
                hash: Hash([5u8; 32]),
            }),
            new_entry: None,
            live_until: Some(2000),
            ttl_extended: true,
            is_rent_related: true,
            is_read_only_ttl_bump: true,
        };

        assert!(change.ttl_extended);
        assert!(change.is_rent_related);
        assert!(change.is_read_only_ttl_bump);
    }

    /// Regression test for VE-14 / L61593050: XDR deserialization metering.
    ///
    /// stellar-core deserializes ledger entries from XDR, charging
    /// `ContractCostType::ValDeser` for each entry's byte length. The vendored
    /// `invoke_host_function_typed()` now applies the same charge via
    /// `charge_val_deser_for_typed()` (added in the VE-14 fix).
    ///
    /// For a TX with 45 footprint entries, the ValDeser cost is ~3M CPU instructions.
    /// At L61593050 TX 166 (instruction_limit=20M, actual consumption ~20.03M),
    /// the missing charge previously caused the TX to incorrectly succeed.
    ///
    /// This test verifies that `ValDeser` charges consume non-zero CPU instructions.
    #[test]
    fn test_xdr_deserialization_budget_charge_is_significant() {
        use soroban_env_host25::budget::Budget;
        use soroban_env_host25::xdr::ContractCostType;

        // Budget::default() has the standard cost model calibrated from mainnet.
        let budget = Budget::default();
        budget.reset_default().expect("reset budget");

        let cpu_before = budget.get_cpu_insns_consumed().expect("get cpu");

        // Simulate the cost of deserializing one ledger entry (~100 bytes, typical
        // ContractData entry). The non-typed API calls:
        //   budget.charge(ContractCostType::ValDeser, Some(entry_xdr_len as u64))
        // for each footprint entry during build_storage_map_from_xdr_ledger_entries.
        let entry_size: u64 = 100; // typical small ContractData entry
        budget
            .charge(ContractCostType::ValDeser, Some(entry_size))
            .expect("charge should succeed");

        let cpu_after = budget.get_cpu_insns_consumed().expect("get cpu");
        let cpu_for_one_entry = cpu_after - cpu_before;

        assert!(
            cpu_for_one_entry > 0,
            "VE-14: ValDeser charge must consume CPU instructions (consumed: {}). \
             The non-typed API charges this for every footprint entry deserialization.",
            cpu_for_one_entry
        );

        // With 45 entries, the total deserialization cost should be substantial.
        // The actual VE-14 bug: 45 entries × ~67K instructions each ≈ 3M total instructions.
        let estimated_45_entries = cpu_for_one_entry * 45;
        assert!(
            estimated_45_entries > 100_000,
            "VE-14: 45 entries should consume >100K CPU instructions from ValDeser alone \
             (estimated: {}). The actual L61593050 TX consumed ~3M extra instructions from \
             XDR deserialization, pushing it from 17.1M to 20.03M (over 20M limit).",
            estimated_45_entries
        );
    }

    /// Regression test for VE-16 / L61811687: XDR serialization metering.
    ///
    /// The non-typed API calls `metered_write_xdr()` to serialize the return
    /// value and contract events, charging `ContractCostType::ValSer` per byte.
    /// The typed API skips serialization entirely. Without explicit ValSer
    /// charges, the typed path under-reports CPU consumption, causing TXs near
    /// their instruction limit to succeed when stellar-core rejects them with
    /// `ResourceLimitExceeded`.
    ///
    /// At L61811687 TX 378, the missing ValSer charges let the TX succeed in
    /// Henyey while stellar-core rejected it.
    #[test]
    fn test_xdr_serialization_budget_charge_val_ser() {
        use soroban_env_host25::budget::Budget;
        use soroban_env_host25::xdr::ContractCostType;

        let budget = Budget::default();
        budget.reset_default().expect("reset budget");

        let cpu_before = budget.get_cpu_insns_consumed().expect("get cpu");

        // Simulate the cost of serializing a return value + contract events.
        // The non-typed API calls metered_write_xdr() which charges ValSer.
        // A typical Soroban TX might have ~1KB of return value + events.
        let serialize_bytes: u64 = 1000;
        budget
            .charge(ContractCostType::ValSer, Some(serialize_bytes))
            .expect("charge should succeed");

        let cpu_after = budget.get_cpu_insns_consumed().expect("get cpu");
        let cpu_for_serialization = cpu_after - cpu_before;

        assert!(
            cpu_for_serialization > 0,
            "VE-16: ValSer charge must consume CPU instructions (consumed: {}). \
             The non-typed API charges this for return value and event serialization.",
            cpu_for_serialization
        );

        // With larger payloads (e.g., 10KB events), the cost should be significant
        // enough to push a TX over its instruction limit.
        let cpu_before = budget.get_cpu_insns_consumed().expect("get cpu");
        let large_events: u64 = 10_000; // 10KB of events
        budget
            .charge(ContractCostType::ValSer, Some(large_events))
            .expect("charge should succeed");
        let cpu_after = budget.get_cpu_insns_consumed().expect("get cpu");
        let cpu_for_large_events = cpu_after - cpu_before;

        assert!(
            cpu_for_large_events > 1_000,
            "VE-16: 10KB of event serialization should consume >1K CPU instructions \
             (consumed: {}). Missing this charge caused L61811687 TX 378 to incorrectly \
             succeed instead of ResourceLimitExceeded.",
            cpu_for_large_events
        );
    }

    /// Test StorageChange struct representing a delete (new_entry = None).
    #[test]
    fn test_storage_change_delete() {
        let change = StorageChange {
            key: LedgerKey::ContractCode(stellar_xdr::curr::LedgerKeyContractCode {
                hash: Hash([6u8; 32]),
            }),
            new_entry: None,
            live_until: None,
            ttl_extended: false,
            is_rent_related: false,
            is_read_only_ttl_bump: false,
        };

        assert!(change.new_entry.is_none());
        assert!(change.live_until.is_none());
        assert!(matches!(change.key, LedgerKey::ContractCode(_)));
    }
}
