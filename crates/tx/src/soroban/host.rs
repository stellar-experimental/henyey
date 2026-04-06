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
    DiagnosticEvent, LedgerEntry, LedgerKey, Limits, ReadXdr, ScVal, SorobanTransactionData,
    SorobanTransactionDataExt, WriteXdr,
};

use super::error::convert_host_error_p24_to_p25;
use super::HostFunctionInvocation;
use crate::state::LedgerStateManager;
use crate::validation::LedgerContext;

/// Return type for `get_archived_with_restore_info` (p25 version).
/// Contains: (entry, live_until, live_bl_restore_info)
type ArchivedWithRestoreInfoP25 = Option<(
    Rc<LedgerEntry>,
    Option<u32>,
    Option<super::protocol::LiveBucketListRestore>,
)>;

/// A ledger entry paired with its optional TTL (live_until ledger sequence).
pub type EntryWithTtl = (Rc<LedgerEntry>, Option<u32>);

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

/// Adapter that provides snapshot access to our ledger state for Soroban.
/// Used by both P24 and P25 execution paths to look up entries in workspace types.
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
            if let Some(archived_entry) = hot_archive.get(key.as_ref()).map_err(|e| {
                tracing::error!(error = ?e, "Hot archive lookup failed during restore");
                HostErrorP25::from(soroban_env_host25::Error::from_type_and_code(
                    soroban_env_host25::xdr::ScErrorType::Storage,
                    soroban_env_host25::xdr::ScErrorCode::InternalError,
                ))
            })? {
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

impl soroban_env_host25::storage::SnapshotSource for LedgerSnapshotAdapterP25<'_> {
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
            /// We use generous CPU and memory budgets to ensure compilation
            /// never fails due to budget constraints. The actual compilation cost is
            /// typically much lower, but we want to match stellar-core behavior which doesn't
            /// meter compilation at all.
            const WASM_COMPILATION_CPU_BUDGET: u64 = 10_000_000_000;
            const WASM_COMPILATION_MEM_BUDGET: u64 = 1_000_000_000;

            fn new() -> Self {
                let budget = $host_crate::budget::Budget::try_from_configs(
                    Self::WASM_COMPILATION_CPU_BUDGET,
                    Self::WASM_COMPILATION_MEM_BUDGET,
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
/// The invocation request bundles the host function, authorization, ledger context,
/// Soroban config, and optional shared caches required for execution.
pub fn execute_host_function_with_cache(
    request: HostFunctionInvocation<'_>,
) -> Result<SorobanExecutionResult, SorobanExecutionError> {
    let protocol_version = request.context.protocol_version;
    if protocol_version_starts_from(protocol_version, ProtocolVersion::V25) {
        // INVARIANT: module cache always present and correct protocol type during Soroban execution
        let cache = request
            .module_cache
            .unwrap_or_else(|| panic!("Module cache must be provided for Soroban TX execution"));
        let p25_cache = cache.as_p25().unwrap_or_else(|| {
            panic!(
                "Module cache is not P25 but protocol version is {}",
                protocol_version
            )
        });
        return execute_host_function_p25(request, Some(p25_cache));
    }
    // INVARIANT: module cache always present and correct protocol type during Soroban execution
    let cache = request
        .module_cache
        .unwrap_or_else(|| panic!("Module cache must be provided for Soroban TX execution"));
    let p24_cache = cache.as_p24().unwrap_or_else(|| {
        panic!(
            "Module cache is not P24 but protocol version is {}",
            protocol_version
        )
    });
    tracing::debug!("Dispatching to P24 path");
    execute_host_function_p24(request, Some(p24_cache))
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

/// Collected footprint entries and restore info, ready for host invocation.
struct PreparedFootprintEntries {
    encoded_ledger_entries: Vec<Vec<u8>>,
    encoded_ttl_entries: Vec<Vec<u8>>,
    live_bl_restores: Vec<super::protocol::LiveBucketListRestore>,
    actual_restored_indices: Vec<u32>,
}

/// Encoded inputs for e2e_invoke::invoke_host_function().
struct EncodedInvocationInputs {
    encoded_host_fn: Vec<u8>,
    encoded_resources: Vec<u8>,
    encoded_source: Vec<u8>,
    encoded_auth: Vec<Vec<u8>>,
}

/// Protocol-neutral representation of a ledger entry change from e2e_invoke.
/// Both P24 and P25 LedgerEntryChange types have identical fields; this struct
/// lets us share the storage-change mapping logic across protocol versions.
struct NormalizedLedgerChange {
    encoded_key: Vec<u8>,
    read_only: bool,
    encoded_new_value: Option<Vec<u8>>,
    old_entry_size_bytes_for_rent: u32,
    ttl_new_live_until_ledger: Option<u32>,
}

/// Gather and encode all footprint entries (read-only and read-write) into XDR bytes
/// suitable for e2e_invoke::invoke_host_function().
///
/// This is the shared logic between P24 and P25 execution paths. Both paths use
/// workspace XDR types (`stellar_xdr::curr`) and the same `LedgerSnapshotAdapterP25`.
fn prepare_footprint_entries(
    snapshot: &LedgerSnapshotAdapterP25<'_>,
    soroban_data: &SorobanTransactionData,
    context: &LedgerContext,
    soroban_config: &super::budget::SorobanConfig,
    ttl_key_cache: Option<&super::TtlKeyCache>,
    protocol_label: &str,
) -> Result<PreparedFootprintEntries, SorobanExecutionError> {
    let (restored_rw_entry_indices, restored_indices_set) = extract_restored_indices(soroban_data);

    let mut encoded_ledger_entries: Vec<Vec<u8>> = Vec::new();
    let mut encoded_ttl_entries: Vec<Vec<u8>> = Vec::new();
    let mut live_bl_restores: Vec<super::protocol::LiveBucketListRestore> = Vec::new();
    let mut actual_restored_indices: Vec<u32> = Vec::new();

    let encode_ttl = |key: &LedgerKey, live_until: Option<u32>| -> Vec<u8> {
        let needs_ttl = matches!(key, LedgerKey::ContractData(_) | LedgerKey::ContractCode(_));
        if let Some(lu) = live_until {
            let key_hash = super::get_or_compute_key_hash(ttl_key_cache, key);
            stellar_xdr::curr::TtlEntry {
                key_hash,
                live_until_ledger_seq: lu,
            }
            .to_xdr(Limits::none())
            .unwrap_or_default()
        } else if needs_ttl {
            let key_hash = super::get_or_compute_key_hash(ttl_key_cache, key);
            stellar_xdr::curr::TtlEntry {
                key_hash,
                live_until_ledger_seq: context.sequence,
            }
            .to_xdr(Limits::none())
            .unwrap_or_default()
        } else {
            Vec::new()
        }
    };

    let map_snapshot_err = |e: HostErrorP25| SorobanExecutionError {
        host_error: e,
        cpu_insns_consumed: 0,
        mem_bytes_consumed: 0,
    };

    // Read-only footprint entries
    for key in soroban_data.resources.footprint.read_only.iter() {
        if let Some((entry, live_until)) = snapshot.get_local(key).map_err(&map_snapshot_err)? {
            encoded_ledger_entries.push(
                entry
                    .to_xdr(Limits::none())
                    .map_err(|_| make_xdr_setup_error())?,
            );
            encoded_ttl_entries.push(encode_ttl(key, live_until));
        }
    }

    if !restored_indices_set.is_empty() {
        tracing::debug!(
            restored_count = restored_rw_entry_indices.len(),
            restored_indices = ?restored_rw_entry_indices,
            "{}: Transaction has archived entries to restore",
            protocol_label
        );
    }

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
                .map_err(&map_snapshot_err)?;
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
                        "{}: Archived entry found for restoration",
                        protocol_label
                    );
                    encoded_ledger_entries.push(
                        entry
                            .to_xdr(Limits::none())
                            .map_err(|_| make_xdr_setup_error())?,
                    );
                    encoded_ttl_entries.push(encode_ttl(key, restored_live_until));
                    actual_restored_indices.push(idx as u32);
                    if let Some(restore) = live_bl_restore {
                        live_bl_restores.push(restore);
                    }
                } else {
                    tracing::debug!(
                        idx = idx,
                        key_type = ?std::mem::discriminant(key),
                        live_until = ?live_until,
                        "{}: Entry marked for restore but already live (restored by earlier TX)",
                        protocol_label
                    );
                    encoded_ledger_entries.push(
                        entry
                            .to_xdr(Limits::none())
                            .map_err(|_| make_xdr_setup_error())?,
                    );
                    encoded_ttl_entries.push(encode_ttl(key, live_until));
                }
            }
        } else if let Some((entry, live_until)) =
            snapshot.get_local(key).map_err(&map_snapshot_err)?
        {
            encoded_ledger_entries.push(
                entry
                    .to_xdr(Limits::none())
                    .map_err(|_| make_xdr_setup_error())?,
            );
            encoded_ttl_entries.push(encode_ttl(key, live_until));
        }
    }

    Ok(PreparedFootprintEntries {
        encoded_ledger_entries,
        encoded_ttl_entries,
        live_bl_restores,
        actual_restored_indices,
    })
}

/// Encode the host function invocation inputs into XDR bytes.
fn encode_invocation_inputs(
    host_function: &stellar_xdr::curr::HostFunction,
    soroban_data: &SorobanTransactionData,
    source: &stellar_xdr::curr::AccountId,
    auth_entries: &[stellar_xdr::curr::SorobanAuthorizationEntry],
) -> Result<EncodedInvocationInputs, SorobanExecutionError> {
    let encoded_host_fn = host_function
        .to_xdr(Limits::none())
        .map_err(|_| make_xdr_setup_error())?;
    let encoded_resources = soroban_data
        .resources
        .to_xdr(Limits::none())
        .map_err(|_| make_xdr_setup_error())?;
    let encoded_source = source
        .to_xdr(Limits::none())
        .map_err(|_| make_xdr_setup_error())?;
    let encoded_auth: Vec<Vec<u8>> = auth_entries
        .iter()
        .map(|e| e.to_xdr(Limits::none()))
        .collect::<Result<_, _>>()
        .map_err(|_| make_xdr_setup_error())?;

    Ok(EncodedInvocationInputs {
        encoded_host_fn,
        encoded_resources,
        encoded_source,
        encoded_auth,
    })
}

/// Decode contract events from encoded XDR bytes.
fn decode_contract_events(
    encoded_events: &[Vec<u8>],
    make_error: &dyn Fn(&str) -> SorobanExecutionError,
) -> Result<(Vec<stellar_xdr::curr::ContractEvent>, u32), SorobanExecutionError> {
    let mut contract_events = Vec::new();
    let mut contract_events_size = 0u32;
    for encoded_event in encoded_events {
        let event = stellar_xdr::curr::ContractEvent::from_xdr(encoded_event, Limits::none())
            .map_err(|_| make_error("failed to decode ContractEvent"))?;
        contract_events_size = contract_events_size.saturating_add(encoded_event.len() as u32);
        contract_events.push(event);
    }
    Ok((contract_events, contract_events_size))
}

/// Map protocol-neutral ledger changes into `StorageChange` values.
///
/// This is the shared mapping logic between P24 and P25. Both protocol versions'
/// `LedgerEntryChange` types have identical fields; callers normalize them into
/// `NormalizedLedgerChange` before calling this function.
fn map_storage_changes(
    changes: Vec<NormalizedLedgerChange>,
    state: &LedgerStateManager,
    ttl_key_cache: Option<&super::TtlKeyCache>,
) -> Vec<StorageChange> {
    changes
        .into_iter()
        .filter_map(|change| {
            let key = LedgerKey::from_xdr(&change.encoded_key, Limits::none()).ok()?;
            let is_deletion = !change.read_only && change.encoded_new_value.is_none();
            let is_modification = change.encoded_new_value.is_some();
            let is_rent_related = change.old_entry_size_bytes_for_rent > 0;

            let ttl_extended = change
                .ttl_new_live_until_ledger
                .map(|new_live_until| {
                    let key_hash = super::get_or_compute_key_hash(ttl_key_cache, &key);
                    let ledger_start_ttl = state.get_ttl_at_ledger_start(&key_hash).unwrap_or(0);
                    new_live_until > ledger_start_ttl
                })
                .unwrap_or(false);

            let is_read_only_ttl_bump = change.read_only && !is_modification && ttl_extended;

            if is_modification || is_deletion || ttl_extended {
                let new_entry = change
                    .encoded_new_value
                    .as_ref()
                    .and_then(|bytes| LedgerEntry::from_xdr(bytes, Limits::none()).ok());
                let live_until = change.ttl_new_live_until_ledger;
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
        .collect()
}

fn execute_host_function_p24(
    request: HostFunctionInvocation<'_>,
    existing_cache: Option<&ModuleCache>,
) -> Result<SorobanExecutionResult, SorobanExecutionError> {
    let HostFunctionInvocation {
        host_function,
        auth_entries,
        source,
        state,
        context,
        soroban_data,
        soroban_config,
        hot_archive,
        ttl_key_cache,
        ..
    } = request;

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

    // SECURITY: PRNG seed always Some in production Soroban execution path
    let base_prng_seed: [u8; 32] = if let Some(prng_seed) = context.soroban_prng_seed {
        prng_seed
    } else {
        tracing::warn!("P24: Using fallback PRNG seed - results may differ from stellar-core");
        derive_fallback_prng_seed(context)
    };

    // Use P25 snapshot adapter for entry lookups (workspace types).
    // The non-typed API accepts XDR bytes, so we serialize workspace types directly.
    // P24 host deserializes using P24 XDR, which is wire-compatible for all P24 types.
    let snapshot = LedgerSnapshotAdapterP25::with_hot_archive(
        state,
        context.sequence,
        hot_archive,
        ttl_key_cache,
    );

    // ── Gather footprint entries ──
    let footprint = prepare_footprint_entries(
        &snapshot,
        soroban_data,
        context,
        soroban_config,
        ttl_key_cache,
        "P24",
    )?;

    // Use existing module cache — it must always be provided.
    let module_cache = existing_cache
        .unwrap_or_else(|| {
            panic!(
                "P24: Module cache is not available — this is a bug. \
                The persistent module cache should always be initialized before TX execution."
            )
        })
        .clone();
    let module_cache = Some(module_cache);

    // ── Encode inputs and call non-typed invoke_host_function() ──
    let inputs = encode_invocation_inputs(host_function, soroban_data, source, auth_entries)?;

    let mut diagnostic_events: Vec<soroban_env_host24::xdr::DiagnosticEvent> = Vec::new();
    let result = match soroban_env_host24::e2e_invoke::invoke_host_function(
        &budget,
        true, // enable_diagnostics
        inputs.encoded_host_fn,
        inputs.encoded_resources,
        &footprint.actual_restored_indices,
        inputs.encoded_source,
        inputs.encoded_auth.into_iter(),
        ledger_info,
        footprint.encoded_ledger_entries.into_iter(),
        footprint.encoded_ttl_entries.into_iter(),
        base_prng_seed.to_vec(),
        &mut diagnostic_events,
        None, // trace_hook
        module_cache,
    ) {
        Ok(r) => {
            tracing::debug!(
                cpu_consumed = budget.get_cpu_insns_consumed().unwrap_or(0),
                mem_consumed = budget.get_mem_bytes_consumed().unwrap_or(0),
                "P24: e2e_invoke completed successfully"
            );
            r
        }
        Err(e) => {
            tracing::debug!(
                error = %e,
                cpu_consumed = budget.get_cpu_insns_consumed().unwrap_or(0),
                mem_consumed = budget.get_mem_bytes_consumed().unwrap_or(0),
                "P24: e2e_invoke returned error"
            );
            return Err(SorobanExecutionError {
                host_error: convert_host_error_p24_to_p25(e),
                cpu_insns_consumed: budget.get_cpu_insns_consumed().unwrap_or(0),
                mem_bytes_consumed: budget.get_mem_bytes_consumed().unwrap_or(0),
            });
        }
    };

    // ── Decode result ──
    let make_budget_error = |desc: &str| -> SorobanExecutionError {
        tracing::debug!(desc, "P24: XDR decode error in result processing");
        SorobanExecutionError {
            host_error: HostErrorP25::from(soroban_env_host25::Error::from_type_and_code(
                soroban_env_host25::xdr::ScErrorType::Context,
                soroban_env_host25::xdr::ScErrorCode::InternalError,
            )),
            cpu_insns_consumed: budget.get_cpu_insns_consumed().unwrap_or(0),
            mem_bytes_consumed: budget.get_mem_bytes_consumed().unwrap_or(0),
        }
    };

    let (return_value, return_value_size) = match result.encoded_invoke_result {
        Ok(ref bytes) => {
            let val = ScVal::from_xdr(bytes, Limits::none())
                .map_err(|_| make_budget_error("failed to decode ScVal"))?;
            (val, bytes.len() as u32)
        }
        Err(ref e) => {
            tracing::debug!(
                error = %e,
                "P24: e2e_invoke result contained error"
            );
            return Err(SorobanExecutionError {
                host_error: convert_host_error_p24_to_p25(e.clone()),
                cpu_insns_consumed: budget.get_cpu_insns_consumed().unwrap_or(0),
                mem_bytes_consumed: budget.get_mem_bytes_consumed().unwrap_or(0),
            });
        }
    };

    // ── Contract events ──
    let (contract_events, contract_events_size) =
        decode_contract_events(&result.encoded_contract_events, &make_budget_error)?;

    // ── Rent: use soroban-env-host's extract_rent_changes() ──
    let rent_changes_p24 =
        soroban_env_host24::e2e_invoke::extract_rent_changes(&result.ledger_changes);
    let rent_fee_config = rent_fee_config_p25_to_p24(&soroban_config.rent_fee_config);
    let rent_fee = compute_rent_fee(&rent_changes_p24, &rent_fee_config, context.sequence);
    tracing::debug!(
        computed_rent_fee = rent_fee,
        rent_changes_count = rent_changes_p24.len(),
        ledger_seq = context.sequence,
        "P24: Computed rent fee"
    );

    // ── Storage changes ──
    let normalized_changes: Vec<NormalizedLedgerChange> = result
        .ledger_changes
        .into_iter()
        .map(|c| NormalizedLedgerChange {
            encoded_key: c.encoded_key,
            read_only: c.read_only,
            encoded_new_value: c.encoded_new_value,
            old_entry_size_bytes_for_rent: c.old_entry_size_bytes_for_rent,
            ttl_new_live_until_ledger: c.ttl_change.map(|t| t.new_live_until_ledger),
        })
        .collect();
    let storage_changes = map_storage_changes(normalized_changes, state, ttl_key_cache);

    // Get budget consumption
    let cpu_insns = budget.get_cpu_insns_consumed().unwrap_or(0);
    let mem_bytes = budget.get_mem_bytes_consumed().unwrap_or(0);
    let contract_events_and_return_value_size =
        contract_events_size.saturating_add(return_value_size);
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
        live_bucket_list_restores: footprint.live_bl_restores,
        actual_restored_indices: footprint.actual_restored_indices,
    })
}

fn execute_host_function_p25(
    request: HostFunctionInvocation<'_>,
    existing_cache: Option<&ModuleCacheP25>,
) -> Result<SorobanExecutionResult, SorobanExecutionError> {
    use soroban_env_host25::{budget::Budget, e2e_invoke, fees::compute_rent_fee};

    let HostFunctionInvocation {
        host_function,
        auth_entries,
        source,
        state,
        context,
        soroban_data,
        soroban_config,
        hot_archive,
        ttl_key_cache,
        ..
    } = request;

    let make_setup_error = |e: HostErrorP25| SorobanExecutionError {
        host_error: e,
        cpu_insns_consumed: 0,
        mem_bytes_consumed: 0,
    };

    let instruction_limit = soroban_data.resources.instructions as u64;
    let memory_limit = soroban_config.tx_max_memory_bytes;

    let budget = if soroban_config.has_valid_cost_params() {
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

    // SECURITY: PRNG seed always Some in production Soroban execution path
    let base_prng_seed: [u8; 32] = if let Some(prng_seed) = context.soroban_prng_seed {
        prng_seed
    } else {
        tracing::warn!("P25: Using fallback PRNG seed - results may differ from stellar-core");
        derive_fallback_prng_seed(context)
    };

    let snapshot = LedgerSnapshotAdapterP25::with_hot_archive(
        state,
        context.sequence,
        hot_archive,
        ttl_key_cache,
    );

    // ── Gather footprint entries ──
    let footprint = prepare_footprint_entries(
        &snapshot,
        soroban_data,
        context,
        soroban_config,
        ttl_key_cache,
        "P25",
    )?;

    // Use existing module cache — it must always be provided.
    let module_cache = existing_cache
        .unwrap_or_else(|| {
            panic!(
                "P25: Module cache is not available — this is a bug. \
                The persistent module cache should always be initialized before TX execution."
            )
        })
        .clone();
    let module_cache = Some(module_cache);

    // ── Encode inputs and call non-typed invoke_host_function() ──
    // All budget metering (ValDeser for inputs, ValSer for outputs) is handled
    // internally by the non-typed API, eliminating VE-14 and VE-16 bug classes.
    let inputs = encode_invocation_inputs(host_function, soroban_data, source, auth_entries)?;

    let mut diagnostic_events: Vec<soroban_env_host25::xdr::DiagnosticEvent> = Vec::new();

    let result = match e2e_invoke::invoke_host_function(
        &budget,
        true, // enable_diagnostics
        inputs.encoded_host_fn,
        inputs.encoded_resources,
        &footprint.actual_restored_indices,
        inputs.encoded_source,
        inputs.encoded_auth.into_iter(),
        ledger_info,
        footprint.encoded_ledger_entries.into_iter(),
        footprint.encoded_ttl_entries.into_iter(),
        base_prng_seed.to_vec(),
        &mut diagnostic_events,
        None, // trace_hook
        module_cache,
    ) {
        Ok(r) => {
            tracing::debug!(
                cpu_consumed = budget.get_cpu_insns_consumed().unwrap_or(0),
                mem_consumed = budget.get_mem_bytes_consumed().unwrap_or(0),
                "P25: e2e_invoke completed successfully"
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
                "P25: e2e_invoke failed"
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

    // ── Decode result ──
    let make_budget_error = |desc: &str| -> SorobanExecutionError {
        tracing::debug!(desc, "P25: XDR decode error in result processing");
        SorobanExecutionError {
            host_error: HostErrorP25::from(soroban_env_host25::Error::from_type_and_code(
                soroban_env_host25::xdr::ScErrorType::Context,
                soroban_env_host25::xdr::ScErrorCode::InternalError,
            )),
            cpu_insns_consumed: budget.get_cpu_insns_consumed().unwrap_or(0),
            mem_bytes_consumed: budget.get_mem_bytes_consumed().unwrap_or(0),
        }
    };

    let (return_value, return_value_size) = match result.encoded_invoke_result {
        Ok(ref bytes) => {
            let val = ScVal::from_xdr(bytes, Limits::none())
                .map_err(|_| make_budget_error("failed to decode ScVal"))?;
            (val, bytes.len() as u32)
        }
        Err(ref e) => {
            let cpu_insns_consumed = budget.get_cpu_insns_consumed().unwrap_or(0);
            let mem_bytes_consumed = budget.get_mem_bytes_consumed().unwrap_or(0);
            tracing::debug!(
                cpu_consumed = cpu_insns_consumed,
                mem_consumed = mem_bytes_consumed,
                error = %e,
                "P25: e2e_invoke result contained error"
            );
            return Err(SorobanExecutionError {
                host_error: e.clone(),
                cpu_insns_consumed,
                mem_bytes_consumed,
            });
        }
    };

    // ── Contract events ──
    // The non-typed API already filters out diagnostic and failed events
    // and serializes via metered_write_xdr internally (VE-16 parity by construction).
    let (contract_events, contract_events_size) =
        decode_contract_events(&result.encoded_contract_events, &make_budget_error)?;

    // ── Rent: use soroban-env-host's extract_rent_changes() ──
    let rent_changes = e2e_invoke::extract_rent_changes(&result.ledger_changes);
    let rent_fee = compute_rent_fee(
        &rent_changes,
        &soroban_config.rent_fee_config,
        context.sequence,
    );

    // ── Storage changes ──
    let normalized_changes: Vec<NormalizedLedgerChange> = result
        .ledger_changes
        .into_iter()
        .map(|c| NormalizedLedgerChange {
            encoded_key: c.encoded_key,
            read_only: c.read_only,
            encoded_new_value: c.encoded_new_value,
            old_entry_size_bytes_for_rent: c.old_entry_size_bytes_for_rent,
            ttl_new_live_until_ledger: c.ttl_change.map(|t| t.new_live_until_ledger),
        })
        .collect();
    let storage_changes = map_storage_changes(normalized_changes, state, ttl_key_cache);

    let cpu_insns = budget.get_cpu_insns_consumed().unwrap_or(0);
    let mem_bytes = budget.get_mem_bytes_consumed().unwrap_or(0);
    let contract_events_and_return_value_size =
        contract_events_size.saturating_add(return_value_size);

    Ok(SorobanExecutionResult {
        return_value,
        storage_changes,
        contract_events,
        diagnostic_events,
        cpu_insns,
        mem_bytes,
        contract_events_and_return_value_size,
        rent_fee,
        live_bucket_list_restores: footprint.live_bl_restores,
        actual_restored_indices: footprint.actual_restored_indices,
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
    /// The non-typed API (`invoke_host_function()`) deserializes ledger entries
    /// from XDR, charging `ContractCostType::ValDeser` for each entry's byte
    /// length. This metering is now handled internally by the published crate,
    /// eliminating the VE-14 bug class by construction.
    ///
    /// This test verifies that ValDeser charges consume non-zero CPU instructions.
    #[test]
    fn test_xdr_deserialization_budget_charge_is_significant() {
        use soroban_env_host25::budget::Budget;
        use soroban_env_host25::xdr::ContractCostType;

        let budget = Budget::default();
        budget.reset_default().expect("reset budget");

        let cpu_before = budget.get_cpu_insns_consumed().expect("get cpu");

        let entry_size: u64 = 100;
        budget
            .charge(ContractCostType::ValDeser, Some(entry_size))
            .expect("charge should succeed");

        let cpu_after = budget.get_cpu_insns_consumed().expect("get cpu");
        let cpu_for_one_entry = cpu_after - cpu_before;

        assert!(
            cpu_for_one_entry > 0,
            "VE-14: ValDeser charge must consume CPU instructions (consumed: {})",
            cpu_for_one_entry
        );

        let estimated_45_entries = cpu_for_one_entry * 45;
        assert!(
            estimated_45_entries > 100_000,
            "VE-14: 45 entries should consume >100K CPU instructions from ValDeser alone \
             (estimated: {})",
            estimated_45_entries
        );
    }

    /// Regression test for VE-16 / L61811687: metered XDR serialization.
    ///
    /// The non-typed API (`invoke_host_function()`) serializes the return value
    /// and contract events via `metered_write_xdr()` internally, charging
    /// `ContractCostType::ValSer` per `write()` call. This metering is now
    /// handled internally by the published crate, eliminating the VE-16 bug
    /// class by construction.
    ///
    /// This test verifies that ValSer charges consume non-zero CPU instructions.
    #[test]
    fn test_val_ser_budget_charge_is_significant() {
        use soroban_env_host25::budget::Budget;
        use soroban_env_host25::xdr::ContractCostType;

        let budget = Budget::default();
        budget.reset_default().expect("reset budget");

        let cpu_before = budget.get_cpu_insns_consumed().expect("get cpu");

        // Simulate the cost of serializing 1KB of output data.
        let output_size: u64 = 1000;
        budget
            .charge(ContractCostType::ValSer, Some(output_size))
            .expect("charge should succeed");

        let cpu_after = budget.get_cpu_insns_consumed().expect("get cpu");
        let cpu_for_serialization = cpu_after - cpu_before;

        assert!(
            cpu_for_serialization > 0,
            "VE-16: ValSer charge must consume CPU instructions (consumed: {})",
            cpu_for_serialization
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
