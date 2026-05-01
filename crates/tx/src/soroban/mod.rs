//! Soroban smart contract integration.
//!
//! This module provides the integration layer between the transaction processor
//! and Soroban smart contract execution. It handles:
//!
//! - Budget tracking for CPU and memory consumption
//! - Storage interface for contract state (persistent and temporary)
//! - Event recording (contract events, system events, diagnostics)
//! - Host function execution via `soroban-env-host`
//!
//! # Architecture
//!
//! Soroban execution follows this pipeline:
//!
//! ```text
//! Transaction with InvokeHostFunction
//!            |
//!            v
//! +---------------------+
//! | Footprint Validation |  <- Verify declared read/write keys
//! +---------------------+
//!            |
//!            v
//! +---------------------+
//! | Build Storage Map    |  <- Load entries from bucket list
//! +---------------------+
//!            |
//!            v
//! +---------------------+
//! | Execute via e2e_invoke |  <- soroban-env-host execution
//! +---------------------+
//!            |
//!            v
//! +---------------------+
//! | Collect Changes      |  <- Storage changes, events, fees
//! +---------------------+
//!            |
//!            v
//! Apply to TxChangeLog
//! ```
//!
//! # Protocol Versioning
//!
//! The `protocol` submodule provides protocol-versioned host implementations.
//! Each protocol version uses the exact same `soroban-env-host` version as
//! stellar-core to ensure deterministic replay:
//!
//! | Protocol | soroban-env-host Version |
//! |----------|-------------------------|
//! | 24       | `soroban-env-host-p24`  |
//! | 25+      | `soroban-env-host-p25`  |
//!
//! This versioning is critical because:
//! - Host function semantics may change between versions
//! - Cost model parameters differ per protocol
//! - PRNG behavior must match exactly for determinism
//!
//! # Key Components
//!
//! - [`SorobanConfig`]: Network configuration for Soroban execution including
//!   cost parameters, TTL limits, and fee configuration.
//!
//! - [`SorobanBudget`]: Tracks resource consumption (CPU, memory, I/O) against
//!   declared limits to enforce execution bounds.
//!
//! - [`SorobanStorage`]: Provides the storage interface for contract state,
//!   tracking reads and writes during execution.
//!
//! - [`execute_host_function_with_cache`]: Main entry point for executing Soroban
//!   operations, handling protocol version dispatch.
//!
//! # Entry TTL and Archival
//!
//! Soroban entries (ContractData, ContractCode) have time-to-live (TTL) values:
//!
//! - **Temporary entries**: Short-lived, cheaper storage. Automatically deleted
//!   when TTL expires.
//!
//! - **Persistent entries**: Long-lived storage. When TTL expires, entries move
//!   to the "hot archive" and can be restored via `RestoreFootprint`.
//!
//! The storage adapter checks TTL values and excludes expired entries from the
//! snapshot, matching stellar-core behavior.

mod budget;
pub mod convert;
mod error;
mod host;
pub mod protocol;
mod storage;
pub mod ttl;

pub use budget::{
    FeeConfiguration, RentFeeConfiguration, ResourceLimits, SorobanBudget, SorobanConfig,
};
pub use host::{
    execute_host_function_with_cache, PersistentModuleCache, SorobanExecutionError,
    SorobanExecutionResult, StorageChange, StorageChangeKind,
};
pub use storage::{SorobanStorage, StorageEntry, StorageKey};
pub use ttl::{extend_ttl_target, restore_ttl_target};

use stellar_xdr::curr::{
    AccountId, Hash, HostFunction, LedgerEntry, LedgerKey, SorobanAuthorizationEntry,
    SorobanTransactionData,
};

use crate::{state::LedgerStateManager, validation::LedgerContext};

/// Bundles the inputs needed to execute a Soroban host function.
#[derive(Clone, Copy)]
pub struct HostFunctionInvocation<'a> {
    pub host_function: &'a HostFunction,
    pub auth_entries: &'a [SorobanAuthorizationEntry],
    pub source: &'a AccountId,
    pub state: &'a LedgerStateManager,
    pub context: &'a LedgerContext,
    pub soroban_data: &'a SorobanTransactionData,
    pub soroban_config: &'a SorobanConfig,
    pub module_cache: Option<&'a PersistentModuleCache>,
    pub guarded_hot_archive: Option<GuardedHotArchive<'a>>,
    pub ttl_key_cache: Option<&'a TtlKeyCache>,
}

/// Bundles the optional Soroban parameters that thread through operation execution.
///
/// Many operation-execution functions need access to these five optional fields
/// for Soroban-enabled transactions. This struct eliminates the need to pass
/// them as five separate parameters (and the associated `clippy::too_many_arguments`
/// suppressions).
///
/// For non-Soroban transactions, create with [`SorobanContext::none()`].
pub struct SorobanContext<'a> {
    pub soroban_data: Option<&'a SorobanTransactionData>,
    pub config: Option<&'a SorobanConfig>,
    pub module_cache: Option<&'a PersistentModuleCache>,
    pub guarded_hot_archive: Option<GuardedHotArchive<'a>>,
    pub ttl_key_cache: Option<&'a TtlKeyCache>,
}

impl SorobanContext<'_> {
    /// Create a context with no Soroban data (for non-Soroban transactions).
    pub fn none() -> Self {
        Self {
            soroban_data: None,
            config: None,
            module_cache: None,
            guarded_hot_archive: None,
            ttl_key_cache: None,
        }
    }
}

/// Cache of pre-computed TTL key hashes. Built during footprint loading,
/// passed through to all Soroban validation and execution functions.
/// Each entry maps a ContractData/ContractCode LedgerKey to its SHA-256 hash.
pub type TtlKeyCache = std::collections::HashMap<LedgerKey, Hash>;

/// Get or compute the TTL key hash for a ContractData/ContractCode key.
/// Uses the cache if available, falls back to computing on the spot.
pub fn get_or_compute_key_hash(cache: Option<&TtlKeyCache>, key: &LedgerKey) -> Hash {
    if let Some(cache) = cache {
        if let Some(hash) = cache.get(key) {
            return hash.clone();
        }
    }
    compute_key_hash(key)
}

/// Compute the hash of a ledger key for TTL lookup.
pub fn compute_key_hash(key: &LedgerKey) -> Hash {
    henyey_common::Hash256::hash_xdr(key).into()
}

/// Trait for looking up archived entries from the hot archive.
///
/// This trait provides dependency inversion between `stellar-core-tx` (which executes
/// transactions) and `stellar-core-bucket`/`stellar-core-ledger` (which manage the
/// hot archive bucket list).
///
/// # Protocol 23+ Hot Archive
///
/// Starting from Protocol 23, persistent Soroban entries (ContractData, ContractCode)
/// that expire are moved to the "hot archive" bucket list rather than being deleted.
/// These entries can be restored via the `RestoreFootprint` operation or auto-restored
/// when marked in `SorobanTransactionDataExt::V1::archived_soroban_entries`.
///
/// When a transaction needs to restore an archived entry:
/// 1. The entry may still be in the live bucket list with an expired TTL ("live BL restore")
/// 2. The entry may have been fully evicted to the hot archive ("hot archive restore")
///
/// This trait handles case (2) by providing lookups into the hot archive bucket list.
pub trait HotArchiveLookup: Send + Sync {
    /// Look up an entry in the hot archive.
    ///
    /// Returns `Ok(Some(entry))` if the key exists in the hot archive as an `Archived` entry.
    /// Returns `Ok(None)` if the key is not found or exists as a `Live` marker (restored).
    /// Returns `Err` if an I/O or deserialization error occurs during lookup.
    ///
    /// Errors must be propagated, not silently swallowed. A transient I/O error on one
    /// validator but not another would cause disagreement on entry existence and different
    /// ledger hashes — a consensus split.
    fn get(
        &self,
        key: &LedgerKey,
    ) -> std::result::Result<Option<LedgerEntry>, Box<dyn std::error::Error + Send + Sync>>;
}

/// A no-op implementation of HotArchiveLookup that always returns None.
///
/// This is used when:
/// - Hot archive is not available (e.g., pre-Protocol 23)
/// - Running in a context where hot archive lookups are not needed
pub struct NoHotArchive;

impl HotArchiveLookup for NoHotArchive {
    fn get(
        &self,
        _key: &LedgerKey,
    ) -> std::result::Result<Option<LedgerEntry>, Box<dyn std::error::Error + Send + Sync>> {
        Ok(None)
    }
}

/// A hot-archive lookup that automatically skips keys already restored in this ledger.
///
/// Wraps a `HotArchiveLookup` together with the set of keys that were previously
/// restored from the hot archive. `get()` transparently returns `Ok(None)` for any
/// key in that set, and `was_previously_restored()` lets callers query the set
/// independently (e.g., for archival-status checks in disk-read metering).
#[derive(Clone, Copy)]
pub struct GuardedHotArchive<'a> {
    inner: &'a dyn HotArchiveLookup,
    restored_keys: &'a std::collections::HashSet<LedgerKey>,
}

impl<'a> GuardedHotArchive<'a> {
    pub fn new(
        inner: &'a dyn HotArchiveLookup,
        restored_keys: &'a std::collections::HashSet<LedgerKey>,
    ) -> Self {
        Self {
            inner,
            restored_keys,
        }
    }

    /// Look up an entry, returning `Ok(None)` if the key was already restored.
    pub fn get(
        &self,
        key: &LedgerKey,
    ) -> std::result::Result<Option<LedgerEntry>, Box<dyn std::error::Error + Send + Sync>> {
        if self.restored_keys.contains(key) {
            return Ok(None);
        }
        self.inner.get(key)
    }

    /// Check whether a key was previously restored from the hot archive.
    /// Used independently of `get()` — e.g., for archival-status checks.
    pub fn was_previously_restored(&self, key: &LedgerKey) -> bool {
        self.restored_keys.contains(key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use stellar_xdr::curr::{
        AccountId, ContractDataDurability, ContractDataEntry, ContractId, ExtensionPoint, Hash,
        LedgerEntryData, LedgerEntryExt, LedgerKeyContractData, PublicKey, ScAddress, ScVal,
        Uint256,
    };

    fn test_contract_data_key() -> LedgerKey {
        LedgerKey::ContractData(LedgerKeyContractData {
            contract: ScAddress::Contract(ContractId(Hash([1u8; 32]))),
            key: ScVal::Void,
            durability: ContractDataDurability::Persistent,
        })
    }

    fn test_contract_data_entry() -> LedgerEntry {
        LedgerEntry {
            last_modified_ledger_seq: 100,
            data: LedgerEntryData::ContractData(ContractDataEntry {
                ext: ExtensionPoint::V0,
                contract: ScAddress::Contract(ContractId(Hash([1u8; 32]))),
                key: ScVal::Void,
                durability: ContractDataDurability::Persistent,
                val: ScVal::I32(42),
            }),
            ext: LedgerEntryExt::V0,
        }
    }

    /// Test NoHotArchive always returns Ok(None).
    #[test]
    fn test_no_hot_archive_get() {
        let archive = NoHotArchive;
        let key = test_contract_data_key();
        assert!(archive.get(&key).unwrap().is_none());
    }

    /// Test NoHotArchive returns Ok(None) for various key types.
    #[test]
    fn test_no_hot_archive_different_keys() {
        let archive = NoHotArchive;

        // Contract data key
        let contract_key = test_contract_data_key();
        assert!(archive.get(&contract_key).unwrap().is_none());

        // Account key
        let account_key = LedgerKey::Account(stellar_xdr::curr::LedgerKeyAccount {
            account_id: AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([0u8; 32]))),
        });
        assert!(archive.get(&account_key).unwrap().is_none());
    }

    /// Test a custom HotArchiveLookup implementation.
    #[test]
    fn test_custom_hot_archive_lookup() {
        use std::collections::HashMap;

        struct TestHotArchive {
            entries: HashMap<Vec<u8>, LedgerEntry>,
        }

        impl HotArchiveLookup for TestHotArchive {
            fn get(
                &self,
                key: &LedgerKey,
            ) -> std::result::Result<Option<LedgerEntry>, Box<dyn std::error::Error + Send + Sync>>
            {
                let key_bytes =
                    stellar_xdr::curr::WriteXdr::to_xdr(key, stellar_xdr::curr::Limits::none())?;
                Ok(self.entries.get(&key_bytes).cloned())
            }
        }

        let mut entries = HashMap::new();
        let key = test_contract_data_key();
        let entry = test_contract_data_entry();
        let key_bytes =
            stellar_xdr::curr::WriteXdr::to_xdr(&key, stellar_xdr::curr::Limits::none()).unwrap();
        entries.insert(key_bytes, entry.clone());

        let archive = TestHotArchive { entries };

        // Key that exists
        let result = archive.get(&key).unwrap();
        assert_eq!(result.expect("expected Some").last_modified_ledger_seq, 100);

        // Key that doesn't exist
        let other_key = LedgerKey::ContractData(LedgerKeyContractData {
            contract: ScAddress::Contract(ContractId(Hash([2u8; 32]))),
            key: ScVal::Void,
            durability: ContractDataDurability::Persistent,
        });
        assert!(archive.get(&other_key).unwrap().is_none());
    }

    /// Test trait object usage for HotArchiveLookup.
    #[test]
    fn test_hot_archive_trait_object() {
        let archive: Box<dyn HotArchiveLookup> = Box::new(NoHotArchive);
        let key = test_contract_data_key();
        assert!(archive.get(&key).unwrap().is_none());
    }

    /// Test NoHotArchive is Send + Sync (required by trait bounds).
    #[test]
    fn test_no_hot_archive_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<NoHotArchive>();
    }

    /// Regression test for AUDIT-C12: hot archive lookup errors must propagate,
    /// not be silently swallowed. If errors are swallowed, a transient I/O error
    /// on one validator would cause it to report "entry not found" while others
    /// report "entry found", leading to a consensus split on the resulting ledger hash.
    #[test]
    fn test_hot_archive_error_propagates_not_swallowed() {
        struct FailingHotArchive;

        impl HotArchiveLookup for FailingHotArchive {
            fn get(
                &self,
                _key: &LedgerKey,
            ) -> std::result::Result<Option<LedgerEntry>, Box<dyn std::error::Error + Send + Sync>>
            {
                Err("simulated I/O error during hot archive lookup".into())
            }
        }

        let archive = FailingHotArchive;
        let key = test_contract_data_key();

        // The error must propagate — previously this would have returned None,
        // silently hiding the I/O failure.
        let result = archive.get(&key);
        assert!(result.is_err(), "I/O errors must not be silently swallowed");
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("simulated I/O error"),
            "Error message must be preserved"
        );
    }

    #[test]
    fn test_guarded_hot_archive_get_returns_none_for_restored_key() {
        use std::collections::HashSet;

        let key = test_contract_data_key();
        let mut restored = HashSet::new();
        restored.insert(key.clone());

        // Create a GuardedHotArchive with a NoHotArchive inner and the restored key.
        let guarded = GuardedHotArchive::new(&NoHotArchive, &restored);

        // Key is in restored set → get() returns Ok(None), never calling inner.
        let result = guarded.get(&key).unwrap();
        assert!(result.is_none(), "guarded key must return None");
    }

    #[test]
    fn test_guarded_hot_archive_get_delegates_for_non_restored_key() {
        use std::collections::HashSet;

        let key = test_contract_data_key();
        let restored = HashSet::new();

        // Create a GuardedHotArchive with an empty restored set.
        let guarded = GuardedHotArchive::new(&NoHotArchive, &restored);

        // Key is NOT in restored set → get() delegates to inner.
        // NoHotArchive always returns Ok(None), so result is None.
        let result = guarded.get(&key).unwrap();
        assert!(result.is_none(), "unguarded key delegates to inner");
    }

    #[test]
    fn test_guarded_hot_archive_was_previously_restored() {
        use std::collections::HashSet;

        let key = test_contract_data_key();
        let mut restored = HashSet::new();
        restored.insert(key.clone());

        // A key with a different ScVal value
        let other_key = LedgerKey::ContractData(LedgerKeyContractData {
            contract: ScAddress::Contract(ContractId(Hash([2u8; 32]))),
            key: ScVal::U32(42),
            durability: ContractDataDurability::Persistent,
        });
        let guarded = GuardedHotArchive::new(&NoHotArchive, &restored);

        assert!(guarded.was_previously_restored(&key));
        assert!(!guarded.was_previously_restored(&other_key));
    }

    #[test]
    fn test_guarded_hot_archive_error_propagation() {
        use std::collections::HashSet;

        struct FailingHotArchive;
        impl HotArchiveLookup for FailingHotArchive {
            fn get(
                &self,
                _key: &LedgerKey,
            ) -> std::result::Result<Option<LedgerEntry>, Box<dyn std::error::Error + Send + Sync>>
            {
                Err("simulated I/O error".into())
            }
        }

        let key = test_contract_data_key();
        let mut other_key = key.clone();
        if let LedgerKey::ContractData(ref mut cd) = other_key {
            cd.key = ScVal::U32(999);
        }
        let restored = HashSet::new();
        let guarded = GuardedHotArchive::new(&FailingHotArchive, &restored);

        // Non-restored key → delegates to inner → error propagates
        let result = guarded.get(&other_key);
        assert!(result.is_err(), "error must propagate for non-restored key");
    }
}
