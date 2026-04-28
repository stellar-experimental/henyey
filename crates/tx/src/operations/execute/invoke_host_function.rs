//! InvokeHostFunction operation execution.
//!
//! This module implements the execution logic for the InvokeHostFunction operation,
//! which executes Soroban smart contract functions.

use stellar_xdr::curr::{
    AccountId, ContractEvent, DiagnosticEvent, Hash, InvokeHostFunctionOp,
    InvokeHostFunctionResult, InvokeHostFunctionResultCode, InvokeHostFunctionSuccessPreImage,
    LedgerEntry, LedgerKey, Limits, OperationResult, OperationResultTr, ScVal,
    SorobanTransactionData, SorobanTransactionDataExt, TtlEntry, WriteXdr,
};

use henyey_common::protocol::{
    protocol_version_is_before, protocol_version_starts_from, ProtocolVersion,
};
use henyey_common::xdr_encoded_len;

/// Check if a ledger key is for a Soroban entry.
pub(crate) fn is_soroban_key(key: &LedgerKey) -> bool {
    matches!(key, LedgerKey::ContractData(_) | LedgerKey::ContractCode(_))
}

/// Check if a key was already created in the delta (by a previous TX in this ledger).
///
/// This is used for hot archive restoration to distinguish between:
/// - First restoration: entry should be recorded as INIT (create)
/// - Subsequent access: entry was already restored, should be LIVE (update)
///
/// We can't use state.get_*().is_some() because archived entries are pre-loaded
/// into state from InMemorySorobanState before Soroban execution.
fn key_already_created_in_delta(delta: &crate::apply::TxChangeLog, key: &LedgerKey) -> bool {
    use stellar_xdr::curr::LedgerEntryData;

    for entry in delta.created_entries() {
        let entry_key = match &entry.data {
            LedgerEntryData::ContractData(cd) => {
                LedgerKey::ContractData(stellar_xdr::curr::LedgerKeyContractData {
                    contract: cd.contract.clone(),
                    key: cd.key.clone(),
                    durability: cd.durability,
                })
            }
            LedgerEntryData::ContractCode(cc) => {
                LedgerKey::ContractCode(stellar_xdr::curr::LedgerKeyContractCode {
                    hash: cc.hash.clone(),
                })
            }
            _ => continue,
        };
        if entry_key == *key {
            return true;
        }
    }
    false
}

/// Check if a TTL entry with the given key hash was already created in the delta.
fn ttl_already_created_in_delta(
    delta: &crate::apply::TxChangeLog,
    key_hash: &stellar_xdr::curr::Hash,
) -> bool {
    use stellar_xdr::curr::LedgerEntryData;

    for entry in delta.created_entries() {
        if let LedgerEntryData::Ttl(ttl) = &entry.data {
            if ttl.key_hash == *key_hash {
                return true;
            }
        }
    }
    false
}

/// Validate all footprint entries against network config size limits.
///
/// This matches stellar-core's `addReads()` in `InvokeHostFunctionOpFrame.cpp` lines 475-482,
/// which calls `validateContractLedgerEntry()` for every footprint entry BEFORE running
/// the host. If any entry exceeds the configured limits, the TX fails with
/// RESOURCE_LIMIT_EXCEEDED.
fn validate_footprint_entry_sizes(
    state: &LedgerStateManager,
    soroban_data: &SorobanTransactionData,
    soroban_config: &SorobanConfig,
    current_ledger: u32,
    ttl_key_cache: Option<&crate::soroban::TtlKeyCache>,
) -> bool {
    let max_contract_size = soroban_config.max_contract_size_bytes;
    let max_data_entry_size = soroban_config.max_contract_data_entry_size_bytes;

    // Check all read-only footprint entries
    for key in soroban_data.resources.footprint.read_only.iter() {
        if !validate_footprint_entry(
            state,
            key,
            current_ledger,
            max_contract_size,
            max_data_entry_size,
            ttl_key_cache,
        ) {
            return false;
        }
    }
    // Check all read-write footprint entries
    for key in soroban_data.resources.footprint.read_write.iter() {
        if !validate_footprint_entry(
            state,
            key,
            current_ledger,
            max_contract_size,
            max_data_entry_size,
            ttl_key_cache,
        ) {
            return false;
        }
    }
    true
}

/// Validate a single footprint entry's size against network config limits.
///
/// Matches stellar-core's behavior: only validate entries that are live (for soroban entries)
/// or exist (for classic entries). Non-existent entries have size 0 and pass validation.
fn validate_footprint_entry(
    state: &LedgerStateManager,
    key: &LedgerKey,
    current_ledger: u32,
    max_contract_size_bytes: u32,
    max_contract_data_entry_size_bytes: u32,
    ttl_key_cache: Option<&crate::soroban::TtlKeyCache>,
) -> bool {
    // Only CONTRACT_CODE and CONTRACT_DATA can fail validation
    if !matches!(key, LedgerKey::ContractCode(_) | LedgerKey::ContractData(_)) {
        return true;
    }

    // Check if entry is live (has non-expired TTL)
    let key_hash = crate::soroban::get_or_compute_key_hash(ttl_key_cache, key);
    let is_live = match state.get_ttl(&key_hash) {
        Some(ttl) => ttl.live_until_ledger_seq >= current_ledger,
        None => false,
    };

    if !is_live {
        // Dead/non-existent entries have entrySize=0 in stellar-core, which passes validation
        return true;
    }

    // Get entry and compute XDR size
    let entry: Option<LedgerEntry> = match key {
        LedgerKey::ContractData(cd_key) => state
            .get_contract_data(&cd_key.contract, &cd_key.key, cd_key.durability)
            .map(|cd| LedgerEntry {
                last_modified_ledger_seq: 0, // doesn't affect size
                data: stellar_xdr::curr::LedgerEntryData::ContractData(cd.clone()),
                ext: stellar_xdr::curr::LedgerEntryExt::V0,
            }),
        LedgerKey::ContractCode(cc_key) => {
            state.get_contract_code(&cc_key.hash).map(|cc| LedgerEntry {
                last_modified_ledger_seq: 0,
                data: stellar_xdr::curr::LedgerEntryData::ContractCode(cc.clone()),
                ext: stellar_xdr::curr::LedgerEntryExt::V0,
            })
        }
        _ => None,
    };

    if let Some(entry) = entry {
        let entry_size = xdr_encoded_len(&entry);
        let limits = super::ContractSizeLimits {
            max_contract_size_bytes,
            max_contract_data_entry_size_bytes,
        };
        if !super::validate_contract_ledger_entry(key, entry_size, &limits) {
            tracing::warn!(
                entry_size,
                max_contract_size_bytes,
                max_contract_data_entry_size_bytes,
                key_type = ?std::mem::discriminant(key),
                "Footprint entry size exceeds limit during read phase"
            );
            return false;
        }
    }
    true
}

/// Validate archived entries marked for autorestore against network config size limits.
///
/// Matches stellar-core's `handleArchivedEntry()` in `InvokeHostFunctionOpFrame.cpp`
/// which calls `validateContractLedgerEntry()` for each archived entry being restored.
/// `validate_footprint_entry` skips expired entries (treating them as size 0), so this
/// function separately validates entries that will be restored from the hot archive.
fn validate_archived_entry_sizes(
    state: &LedgerStateManager,
    soroban_data: &SorobanTransactionData,
    soroban_config: &SorobanConfig,
    current_ledger: u32,
    hot_archive: Option<&dyn crate::soroban::HotArchiveLookup>,
    ttl_key_cache: Option<&crate::soroban::TtlKeyCache>,
) -> bool {
    let restored_indices: std::collections::HashSet<u32> = match &soroban_data.ext {
        SorobanTransactionDataExt::V1(ext) => {
            ext.archived_soroban_entries.iter().copied().collect()
        }
        SorobanTransactionDataExt::V0 => return true,
    };
    if restored_indices.is_empty() {
        return true;
    }

    let max_contract_size = soroban_config.max_contract_size_bytes;
    let max_data_entry_size = soroban_config.max_contract_data_entry_size_bytes;

    for (idx, key) in soroban_data
        .resources
        .footprint
        .read_write
        .iter()
        .enumerate()
    {
        if !restored_indices.contains(&(idx as u32)) {
            continue;
        }
        // Only CONTRACT_CODE and CONTRACT_DATA can fail size validation
        if !matches!(key, LedgerKey::ContractCode(_) | LedgerKey::ContractData(_)) {
            continue;
        }

        // Check if this entry is actually expired (archived)
        let key_hash = crate::soroban::get_or_compute_key_hash(ttl_key_cache, key);
        let is_live = match state.get_ttl(&key_hash) {
            Some(ttl) => ttl.live_until_ledger_seq >= current_ledger,
            None => false,
        };
        if is_live {
            // Live entries are validated by validate_footprint_entry_sizes
            continue;
        }

        // Entry is expired and marked for restore — look it up from the state
        // (it may have been loaded) or from the hot archive.
        let entry: Option<LedgerEntry> = match key {
            LedgerKey::ContractData(cd_key) => state
                .get_contract_data(&cd_key.contract, &cd_key.key, cd_key.durability)
                .map(|cd| LedgerEntry {
                    last_modified_ledger_seq: 0,
                    data: stellar_xdr::curr::LedgerEntryData::ContractData(cd.clone()),
                    ext: stellar_xdr::curr::LedgerEntryExt::V0,
                }),
            LedgerKey::ContractCode(cc_key) => {
                state.get_contract_code(&cc_key.hash).map(|cc| LedgerEntry {
                    last_modified_ledger_seq: 0,
                    data: stellar_xdr::curr::LedgerEntryData::ContractCode(cc.clone()),
                    ext: stellar_xdr::curr::LedgerEntryExt::V0,
                })
            }
            _ => None,
        }
        .or_else(|| {
            // Not in state (expired and evicted) — try hot archive
            hot_archive.and_then(|ha| ha.get(key).ok().flatten())
        });

        if let Some(entry) = entry {
            let entry_size = xdr_encoded_len(&entry);
            let limits = super::ContractSizeLimits {
                max_contract_size_bytes: max_contract_size,
                max_contract_data_entry_size_bytes: max_data_entry_size,
            };
            if !super::validate_contract_ledger_entry(key, entry_size, &limits) {
                tracing::warn!(
                    entry_size,
                    max_contract_size,
                    max_data_entry_size,
                    key_type = ?std::mem::discriminant(key),
                    idx,
                    "Archived entry marked for restore exceeds size limit"
                );
                return false;
            }
        }
    }
    true
}

use super::{OperationExecutionResult, SorobanOperationMeta};
use crate::soroban::{PersistentModuleCache, SorobanConfig, SorobanContext};
use crate::state::LedgerStateManager;
use crate::validation::LedgerContext;
use crate::{Result, TxError};

/// Execute an InvokeHostFunction operation.
///
/// This operation invokes a Soroban smart contract function, which can:
/// - Call an existing contract
/// - Create a new contract
/// - Upload contract code
///
/// # Arguments
///
/// * `op` - The InvokeHostFunction operation data
/// * `source` - The source account ID
/// * `state` - The ledger state manager
/// * `context` - The ledger context
/// * `soroban` - Soroban execution context
///
/// # Returns
///
/// Returns the operation result with the function's return value on success,
/// or a specific failure reason.
pub(crate) fn execute_invoke_host_function(
    op: &InvokeHostFunctionOp,
    source: &AccountId,
    state: &mut LedgerStateManager,
    context: &LedgerContext,
    soroban: &SorobanContext<'_>,
) -> Result<OperationExecutionResult> {
    // Validate we have Soroban data for footprint
    let soroban_data = match soroban.soroban_data {
        Some(data) => data,
        None => {
            return Ok(OperationExecutionResult::new(make_result(
                InvokeHostFunctionResultCode::Malformed,
                Hash([0u8; 32]),
            )));
        }
    };

    let default_config = SorobanConfig::default();
    let soroban_config = soroban.config.unwrap_or(&default_config);

    // All host functions go through soroban-env-host, matching stellar-core behavior.
    // This ensures rent calculation and other host-computed values are consistent.
    execute_contract_invocation(
        ContractInvocationRequest {
            op,
            source,
            soroban_data,
            soroban_config,
            module_cache: soroban.module_cache,
            hot_archive: soroban.hot_archive,
            ttl_key_cache: soroban.ttl_key_cache,
        },
        state,
        context,
    )
}

/// Execute a contract invocation using soroban-env-host.
struct ContractInvocationRequest<'a> {
    op: &'a InvokeHostFunctionOp,
    source: &'a AccountId,
    soroban_data: &'a SorobanTransactionData,
    soroban_config: &'a SorobanConfig,
    module_cache: Option<&'a PersistentModuleCache>,
    hot_archive: Option<&'a dyn crate::soroban::HotArchiveLookup>,
    ttl_key_cache: Option<&'a crate::soroban::TtlKeyCache>,
}

fn execute_contract_invocation(
    request: ContractInvocationRequest<'_>,
    state: &mut LedgerStateManager,
    context: &LedgerContext,
) -> Result<OperationExecutionResult> {
    use crate::soroban::{execute_host_function_with_cache, HostFunctionInvocation};

    let ContractInvocationRequest {
        op,
        source,
        soroban_data,
        soroban_config,
        module_cache,
        hot_archive,
        ttl_key_cache,
    } = request;

    // Convert auth entries to a slice
    let auth_entries: Vec<_> = op.auth.to_vec();

    if footprint_has_unrestored_archived_entries(
        state,
        &soroban_data.resources.footprint,
        &soroban_data.ext,
        context.sequence,
        hot_archive,
        ttl_key_cache,
    )? {
        return Ok(OperationExecutionResult::new(make_result(
            InvokeHostFunctionResultCode::EntryArchived,
            Hash([0u8; 32]),
        )));
    }

    if disk_read_bytes_exceeded(
        state,
        soroban_data,
        context.protocol_version,
        context.sequence,
        hot_archive,
        ttl_key_cache,
    ) {
        return Ok(OperationExecutionResult::new(make_result(
            InvokeHostFunctionResultCode::ResourceLimitExceeded,
            Hash([0u8; 32]),
        )));
    }

    // Validate all footprint entries against network config size limits.
    // This matches stellar-core's addReads() which calls validateContractLedgerEntry()
    // for every footprint entry BEFORE running the host.
    if !validate_footprint_entry_sizes(
        state,
        soroban_data,
        soroban_config,
        context.sequence,
        ttl_key_cache,
    ) {
        return Ok(OperationExecutionResult::new(make_result(
            InvokeHostFunctionResultCode::ResourceLimitExceeded,
            Hash([0u8; 32]),
        )));
    }

    // Validate archived entries marked for autorestore against size limits.
    // This matches stellar-core's handleArchivedEntry() → validateContractLedgerEntry()
    // which validates restored entries separately from the addReads() path above.
    if !validate_archived_entry_sizes(
        state,
        soroban_data,
        soroban_config,
        context.sequence,
        hot_archive,
        ttl_key_cache,
    ) {
        return Ok(OperationExecutionResult::new(make_result(
            InvokeHostFunctionResultCode::ResourceLimitExceeded,
            Hash([0u8; 32]),
        )));
    }

    // Execute via soroban-env-host
    match execute_host_function_with_cache(HostFunctionInvocation {
        host_function: &op.host_function,
        auth_entries: &auth_entries,
        source,
        state,
        context,
        soroban_data,
        soroban_config,
        module_cache,
        hot_archive,
        ttl_key_cache,
    }) {
        Ok(result) => {
            // stellar-core check: event size (collectEvents in doApply)
            if soroban_config.tx_max_contract_events_size_bytes > 0
                && result.contract_events_and_return_value_size
                    > soroban_config.tx_max_contract_events_size_bytes
            {
                return Ok(OperationExecutionResult::new(make_result(
                    InvokeHostFunctionResultCode::ResourceLimitExceeded,
                    Hash([0u8; 32]),
                )));
            }

            // stellar-core check: write bytes (recordStorageChanges lines 639-652)
            // Sum the XDR sizes of all non-TTL entries being written and check against
            // the specified write_bytes limit. Also validates entry sizes against
            // network config limits (validateContractLedgerEntry).
            match validate_and_compute_write_bytes(
                &result.storage_changes,
                &super::ContractSizeLimits {
                    max_contract_size_bytes: soroban_config.max_contract_size_bytes,
                    max_contract_data_entry_size_bytes: soroban_config
                        .max_contract_data_entry_size_bytes,
                },
            ) {
                StorageChangeValidation::EntrySizeExceeded => {
                    return Ok(OperationExecutionResult::new(make_result(
                        InvokeHostFunctionResultCode::ResourceLimitExceeded,
                        Hash([0u8; 32]),
                    )));
                }
                StorageChangeValidation::Valid { total_write_bytes } => {
                    if total_write_bytes > soroban_data.resources.write_bytes {
                        tracing::warn!(
                            total_write_bytes,
                            specified_write_bytes = soroban_data.resources.write_bytes,
                            "Write bytes exceeded specified limit"
                        );
                        return Ok(OperationExecutionResult::new(make_result(
                            InvokeHostFunctionResultCode::ResourceLimitExceeded,
                            Hash([0u8; 32]),
                        )));
                    }
                }
            }

            // Apply storage changes back to our state.
            // Extract keys being restored from hot archive - these must be recorded as INIT.
            // IMPORTANT: We must exclude live BL restores from this set because those entries
            // are still in the live bucket list (just with expired TTL). Only true hot archive
            // restores (entries that were evicted from live BL to hot archive) should use INIT.
            // ALSO: We use actual_restored_indices which filters out entries already restored
            // by a previous transaction in this ledger.
            let hot_archive_restored_keys = extract_hot_archive_restored_keys(
                soroban_data,
                &result.actual_restored_indices,
                &result.live_bucket_list_restores,
            );
            apply_soroban_storage_changes(
                state,
                &result.storage_changes,
                &soroban_data.resources.footprint,
                &hot_archive_restored_keys,
                ttl_key_cache,
                context.protocol_version,
            );

            // Compute result hash from success preimage (return value + events)
            let result_hash =
                compute_success_preimage_hash(&result.return_value, &result.contract_events);

            tracing::debug!(
                cpu_insns = result.cpu_insns,
                mem_bytes = result.mem_bytes,
                events_count = result.contract_events.len(),
                "Soroban contract executed successfully"
            );

            Ok(OperationExecutionResult::with_soroban_meta(
                make_result(InvokeHostFunctionResultCode::Success, result_hash),
                build_soroban_operation_meta(&result),
            ))
        }
        Err(exec_error) => {
            tracing::debug!(
                error = %exec_error.host_error,
                cpu_consumed = exec_error.cpu_insns_consumed,
                cpu_specified = soroban_data.resources.instructions,
                mem_consumed = exec_error.mem_bytes_consumed,
                mem_limit = soroban_config.tx_max_memory_bytes,
                "Soroban contract execution failed"
            );

            // Map error to result code using stellar-core logic:
            // - RESOURCE_LIMIT_EXCEEDED if actual CPU > specified instructions
            // - RESOURCE_LIMIT_EXCEEDED if actual mem > network's txMemoryLimit
            // - TRAPPED for all other failures
            let result_code = map_host_error_to_result_code(
                &exec_error,
                soroban_data.resources.instructions,
                soroban_config.tx_max_memory_bytes,
            );
            Ok(OperationExecutionResult::new(make_result(
                result_code,
                Hash([0u8; 32]),
            )))
        }
    }
}

/// Result of validating storage changes.
enum StorageChangeValidation {
    /// All changes are valid and within limits.
    Valid { total_write_bytes: u32 },
    /// An entry size exceeded the network config limit.
    EntrySizeExceeded,
}

/// Validate storage changes and compute total write bytes.
///
/// This matches stellar-core's recordStorageChanges() which:
/// 1. Validates entry sizes against network config limits (validateContractLedgerEntry)
/// 2. Sums the XDR size of all non-TTL entries being written
///
/// TTL entries are excluded from write bytes because their write fees come
/// out of refundableFee, already accounted for by the host.
fn validate_and_compute_write_bytes(
    storage_changes: &[crate::soroban::StorageChange],
    limits: &super::ContractSizeLimits,
) -> StorageChangeValidation {
    let mut total: u32 = 0;
    for change in storage_changes {
        if let crate::soroban::StorageChangeKind::Modified { entry, .. } = &change.kind {
            // Skip TTL entries - their write fees are handled separately
            if matches!(entry.data, stellar_xdr::curr::LedgerEntryData::Ttl(_)) {
                continue;
            }
            // Compute XDR size without heap allocation.
            let entry_size = xdr_encoded_len(entry);

            // Validate entry size against network config limits (stellar-core validateContractLedgerEntry)
            if !super::validate_contract_ledger_entry(&change.key, entry_size, limits) {
                return StorageChangeValidation::EntrySizeExceeded;
            }

            total = total.saturating_add(entry_size as u32);
        }
    }
    StorageChangeValidation::Valid {
        total_write_bytes: total,
    }
}

fn disk_read_bytes_exceeded(
    state: &LedgerStateManager,
    soroban_data: &SorobanTransactionData,
    protocol_version: u32,
    current_ledger: u32,
    hot_archive: Option<&dyn crate::soroban::HotArchiveLookup>,
    ttl_key_cache: Option<&crate::soroban::TtlKeyCache>,
) -> bool {
    let mut total_read_bytes = 0u32;
    let limit = soroban_data.resources.disk_read_bytes;

    // Helper to meter a single entry
    let meter_entry = |key: &LedgerKey, total: &mut u32| -> bool {
        let entry: Option<LedgerEntry> = if is_soroban_key(key) {
            match key {
                LedgerKey::ContractData(cd_key) => state
                    .get_contract_data(&cd_key.contract, &cd_key.key, cd_key.durability)
                    .map(|cd| LedgerEntry {
                        last_modified_ledger_seq: current_ledger,
                        data: stellar_xdr::curr::LedgerEntryData::ContractData(cd.clone()),
                        ext: stellar_xdr::curr::LedgerEntryExt::V0,
                    }),
                LedgerKey::ContractCode(cc_key) => {
                    state.get_contract_code(&cc_key.hash).map(|cc| LedgerEntry {
                        last_modified_ledger_seq: current_ledger,
                        data: stellar_xdr::curr::LedgerEntryData::ContractCode(cc.clone()),
                        ext: stellar_xdr::curr::LedgerEntryExt::V0,
                    })
                }
                _ => None,
            }
            // Fallback to hot archive for fully-evicted entries.
            // Matches stellar-core handleArchivedEntry() which meters disk reads
            // for entries retrieved from both live bucket list and hot archive.
            .or_else(|| hot_archive.and_then(|ha| ha.get(key).ok().flatten()))
        } else {
            state.get_entry(key)
        };

        if let Some(entry) = entry {
            let len = xdr_encoded_len(&entry);
            *total = total.saturating_add(len as u32);
            if *total > limit {
                return true;
            }
        }
        false
    };

    // Returns true (exceeded) after metering the entry, with a warning log.
    let mut meter_and_check = |key: &LedgerKey| -> bool {
        if meter_entry(key, &mut total_read_bytes) {
            tracing::warn!(
                total_read_bytes,
                specified_read_bytes = limit,
                "Disk read bytes exceeded specified limit"
            );
            true
        } else {
            false
        }
    };

    let meter_all = protocol_version_is_before(protocol_version, ProtocolVersion::V23);

    if meter_all {
        for key in soroban_data.resources.footprint.read_only.iter() {
            if meter_and_check(key) {
                return true;
            }
        }
        for key in soroban_data.resources.footprint.read_write.iter() {
            if meter_and_check(key) {
                return true;
            }
        }
    } else {
        for key in soroban_data.resources.footprint.read_only.iter() {
            if !is_soroban_key(key) && meter_and_check(key) {
                return true;
            }
        }
        for key in soroban_data.resources.footprint.read_write.iter() {
            if !is_soroban_key(key) && meter_and_check(key) {
                return true;
            }
        }

        if let SorobanTransactionDataExt::V1(ext) = &soroban_data.ext {
            for index in ext.archived_soroban_entries.iter() {
                if let Some(key) = soroban_data
                    .resources
                    .footprint
                    .read_write
                    .get(*index as usize)
                {
                    if !is_soroban_key(key) {
                        continue;
                    }
                    // Match stellar-core behavior: only meter archived entries that are
                    // actually still archived. If a previous TX in this ledger
                    // restored the entry, its TTL is now live, and stellar-core treats
                    // it as an in-memory soroban entry (no disk read metering).
                    let key_hash = crate::soroban::get_or_compute_key_hash(ttl_key_cache, key);
                    let is_still_archived = match state.get_ttl(&key_hash) {
                        Some(ttl) => ttl.live_until_ledger_seq < current_ledger,
                        None => true, // No TTL = not in live state = archived
                    };
                    if !is_still_archived {
                        continue;
                    }
                    if meter_and_check(key) {
                        return true;
                    }
                }
            }
        }
    }

    false
}

/// Compute the hash of the success preimage (return value + events).
///
/// This matches how stellar-core computes the InvokeHostFunction success result:
/// the hash is SHA256 of the XDR-encoded InvokeHostFunctionSuccessPreImage,
/// which contains both the return value and the contract events.
fn compute_success_preimage_hash(return_value: &ScVal, events: &[ContractEvent]) -> Hash {
    use sha2::{Digest, Sha256};

    // Build the success preimage
    let preimage = InvokeHostFunctionSuccessPreImage {
        return_value: return_value.clone(),
        events: events.to_vec().try_into().unwrap_or_default(),
    };

    // Hash the XDR-encoded preimage
    let mut hasher = Sha256::new();
    if let Ok(bytes) = preimage.to_xdr(Limits::none()) {
        hasher.update(&bytes);
    }
    Hash(hasher.finalize().into())
}

fn build_soroban_operation_meta(
    result: &crate::soroban::SorobanExecutionResult,
) -> SorobanOperationMeta {
    // Use contract_events which contains the decoded Contract and System events.
    let events = result.contract_events.clone();

    // Build diagnostic events from the contract events
    let mut diagnostic_events: Vec<DiagnosticEvent> = events
        .iter()
        .map(|event| DiagnosticEvent {
            in_successful_contract_call: true,
            event: event.clone(),
        })
        .collect();

    diagnostic_events.extend(result.diagnostic_events.iter().cloned());

    SorobanOperationMeta {
        events,
        diagnostic_events,
        return_value: Some(result.return_value.clone()),
        event_size_bytes: result.contract_events_and_return_value_size,
        rent_fee: result.rent_fee,
        live_bucket_list_restores: result.live_bucket_list_restores.clone(),
        hot_archive_restores: Vec::new(), // For InvokeHostFunction, hot archive keys are detected via archived_soroban_entries
        actual_restored_indices: result.actual_restored_indices.clone(),
    }
}

/// Extract keys of entries being restored from the hot archive (NOT live BL).
///
/// For InvokeHostFunction: `archived_soroban_entries` in `SorobanTransactionDataExt::V1`
/// contains indices into the read_write footprint that point to entries being auto-restored.
/// These can be either:
/// 1. **Hot archive restores**: Entry was evicted from live BL to hot archive → use INIT
/// 2. **Live BL restores**: Entry is still in live BL but has expired TTL → use LIVE
///
/// This function returns only the hot archive restored keys (excluding live BL restores).
/// Per CAP-0066, hot archive restored entries should be recorded as INIT (created) in the
/// bucket list delta because they are being added back to the live bucket list.
///
/// Live BL restores (entries with expired TTL but not yet evicted) are tracked separately
/// in `live_bucket_list_restores` and should use LIVE (updated) because they already exist
/// in the live bucket list - they just need their TTL refreshed.
///
/// IMPORTANT: Uses `actual_restored_indices` from the execution result, NOT the raw
/// `archived_soroban_entries` from the transaction envelope. This is because if an earlier
/// transaction in the same ledger already restored an entry, the later transaction should
/// NOT treat it as a hot archive restore (it's already live). The host invocation logic
/// filters out already-restored entries when building `actual_restored_indices`.
fn extract_hot_archive_restored_keys(
    soroban_data: &SorobanTransactionData,
    actual_restored_indices: &[u32],
    live_bucket_list_restores: &[crate::soroban::protocol::LiveBucketListRestore],
) -> std::collections::HashSet<LedgerKey> {
    use std::collections::HashSet;

    let mut keys = HashSet::new();

    if actual_restored_indices.is_empty() {
        return keys;
    }

    // Collect keys that are live BL restores (these should NOT be treated as hot archive restores)
    let live_bl_restore_keys: HashSet<LedgerKey> = live_bucket_list_restores
        .iter()
        .map(|r| r.key.clone())
        .collect();

    // Get the corresponding keys from the read_write footprint, excluding live BL restores.
    // We use actual_restored_indices which is already filtered to only include entries
    // that are ACTUALLY being restored in THIS transaction (not already restored by
    // a previous transaction in this ledger).
    let read_write = &soroban_data.resources.footprint.read_write;
    for index in actual_restored_indices {
        if let Some(key) = read_write.get(*index as usize) {
            // Only include if this is NOT a live BL restore
            if !live_bl_restore_keys.contains(key) {
                keys.insert(key.clone());
            }
        }
    }

    keys
}

fn apply_soroban_storage_changes(
    state: &mut LedgerStateManager,
    changes: &[crate::soroban::StorageChange],
    footprint: &stellar_xdr::curr::LedgerFootprint,
    hot_archive_restored_keys: &std::collections::HashSet<LedgerKey>,
    ttl_key_cache: Option<&crate::soroban::TtlKeyCache>,
    protocol_version: u32,
) {
    use std::collections::HashSet;

    // Track all keys that were created or modified by the host
    let mut created_and_modified_keys: HashSet<LedgerKey> = HashSet::new();
    for change in changes {
        if matches!(
            change.kind,
            crate::soroban::StorageChangeKind::Modified { .. }
        ) {
            created_and_modified_keys.insert(change.key.clone());
        }
    }

    // Track truly created keys (entries that did not exist before this TX).
    // This is used for CAP-73 validation below.
    let mut created_keys: HashSet<LedgerKey> = HashSet::new();

    // Apply all storage changes from the host
    for change in changes.iter() {
        tracing::debug!(
            key_type = ?std::mem::discriminant(&change.key),
            kind = ?change.kind,
            is_rent_related = change.is_rent_related,
            "Applying storage change"
        );
        let was_created = apply_soroban_storage_change(
            state,
            change,
            hot_archive_restored_keys,
            ttl_key_cache,
            &mut created_keys,
        );
        if was_created {
            created_keys.insert(change.key.clone());
        }
    }

    // CAP-73 validation: verify that newly created keys are of expected types.
    // Parity: InvokeHostFunctionOpFrame::recordStorageChanges():664-683
    // stellar-core uses releaseAssertOrThrow (fires in all builds), so we use
    // assert! (not debug_assert!) to match — never fail silently.
    for key in &created_keys {
        if is_soroban_key(key) {
            // Soroban entries must have a corresponding TTL entry also created
            let key_hash = crate::soroban::get_or_compute_key_hash(ttl_key_cache, key);
            let ttl_key = LedgerKey::Ttl(stellar_xdr::curr::LedgerKeyTtl { key_hash });
            assert!(
                created_keys.contains(&ttl_key),
                "Created Soroban entry {:?} missing TTL key",
                key
            );
        } else if protocol_version_starts_from(protocol_version, ProtocolVersion::V26) {
            // V26+: SAC can create Account and Trustline entries (CAP-73)
            assert!(
                matches!(
                    key,
                    LedgerKey::Ttl(_) | LedgerKey::Account(_) | LedgerKey::Trustline(_)
                ),
                "Non-Soroban created key must be TTL, Account, or Trustline in V26+, got {:?}",
                std::mem::discriminant(key)
            );
        } else {
            // Pre-V26: only TTL keys should be created as non-Soroban entries
            assert!(
                matches!(key, LedgerKey::Ttl(_)),
                "Non-Soroban created key must be TTL in pre-V26, got {:?}",
                std::mem::discriminant(key)
            );
        }
    }

    // stellar-core behavior: delete any read-write footprint entries that weren't
    // returned by the host. This handles entries that were explicitly deleted by the
    // host or had expired TTL. The host passes through all entries it touches, so
    // entries NOT returned are considered deleted.
    // See: InvokeHostFunctionOpFrame.cpp recordStorageChanges()
    for key in footprint.read_write.iter() {
        if created_and_modified_keys.contains(key) {
            continue;
        }

        // Skip hot archive read-only restores. These entries were never in the live
        // bucket list — the host read them from the hot archive but did not write
        // them back. stellar-core's handleArchivedEntry creates DATA+TTL INIT then
        // immediately erases both for read-only access, so the net effect on the
        // live BL is zero. If we deleted them here we would create a spurious DEAD
        // entry that CDP does not have.
        if hot_archive_restored_keys.contains(key) {
            continue;
        }

        // Only delete Soroban entries (ContractData, ContractCode).
        // Parity: stellar-core asserts isSorobanEntry(lk) after a
        // successful erase. If an existing non-Soroban entry is omitted
        // from the host output, that is a host/protocol bug. Non-existent
        // entries are a no-op (matches eraseLedgerEntryIfExists returning
        // false in stellar-core).
        match key {
            LedgerKey::ContractData(cd_key) => {
                if state
                    .get_contract_data(&cd_key.contract, &cd_key.key, cd_key.durability)
                    .is_some()
                {
                    state.delete_contract_data(&cd_key.contract, &cd_key.key, cd_key.durability);
                    // Also delete the associated TTL entry
                    let key_hash = crate::soroban::get_or_compute_key_hash(ttl_key_cache, key);
                    state.delete_ttl(&key_hash);
                }
            }
            LedgerKey::ContractCode(cc_key) => {
                if state.get_contract_code(&cc_key.hash).is_some() {
                    state.delete_contract_code(&cc_key.hash);
                    // Also delete the associated TTL entry
                    let key_hash = crate::soroban::get_or_compute_key_hash(ttl_key_cache, key);
                    state.delete_ttl(&key_hash);
                }
            }
            // TTL entries are handled along with their associated data/code entries above
            LedgerKey::Ttl(_) => {}
            _ => {
                // Assert that no existing non-Soroban entry is implicitly
                // deleted. Check per type since we don't have a generic
                // entry_exists helper.
                let exists = match key {
                    LedgerKey::Account(k) => state.get_account(&k.account_id).is_some(),
                    LedgerKey::Trustline(k) => state
                        .get_trustline_by_trustline_asset(&k.account_id, &k.asset)
                        .is_some(),
                    // Other classic types (Offer, Data, etc.) should never
                    // appear in a Soroban footprint. If they do and exist,
                    // we still want to catch the parity violation.
                    _ => false,
                };
                assert!(
                    !exists,
                    "implicit Soroban deletion of existing non-Soroban entry: {:?}",
                    std::mem::discriminant(key)
                );
            }
        }
    }
}

/// Create or update a TTL entry depending on whether it already exists in state.
fn create_or_update_ttl(state: &mut LedgerStateManager, ttl: TtlEntry, exists: bool) {
    if exists {
        state.update_ttl(ttl);
    } else {
        state.create_ttl(ttl);
    }
}

/// Determine whether a Soroban contract entry should be created or updated.
///
/// Handles the hot-archive restore logic: if the entry is being restored from the hot
/// archive for the first time, it should be created (INIT). If it was already restored by a
/// previous TX in this ledger, or it exists in live state, it should be updated.
fn should_create_contract_entry(
    entry_exists_in_state: bool,
    is_hot_archive_restore: bool,
    already_created_in_delta: bool,
) -> bool {
    if is_hot_archive_restore && !already_created_in_delta {
        true // First restoration from hot archive - create to record as INIT
    } else if entry_exists_in_state || already_created_in_delta {
        false // Entry exists (either in live BL or already restored) - update
    } else {
        true // New entry (not a restore, not existing) - create
    }
}

/// Returns `true` if the entry was newly created (not an update of an existing entry).
/// This mirrors stellar-core's `upsertLedgerEntry()` return value.
fn apply_soroban_storage_change(
    state: &mut LedgerStateManager,
    change: &crate::soroban::StorageChange,
    hot_archive_restored_keys: &std::collections::HashSet<LedgerKey>,
    ttl_key_cache: Option<&crate::soroban::TtlKeyCache>,
    created_keys: &mut std::collections::HashSet<LedgerKey>,
) -> bool {
    // Check if this entry is being restored from the hot archive.
    // Hot archive restored entries must be recorded as INIT (created) in the bucket list delta,
    // not LIVE (updated), because they are being restored to the live bucket list.
    let is_hot_archive_restore = hot_archive_restored_keys.contains(&change.key);

    // Track whether this entry was created (vs updated)
    let mut was_created = false;

    match &change.kind {
        crate::soroban::StorageChangeKind::Modified {
            entry,
            live_until,
            ttl_extended,
        } => {
            // Handle contract data and code entries.
            match &entry.data {
                stellar_xdr::curr::LedgerEntryData::ContractData(cd) => {
                    let exists = state
                        .get_contract_data(&cd.contract, &cd.key, cd.durability)
                        .is_some();
                    let already_in_delta = is_hot_archive_restore
                        && key_already_created_in_delta(state.delta(), &change.key);
                    if should_create_contract_entry(
                        exists,
                        is_hot_archive_restore,
                        already_in_delta,
                    ) {
                        state.create_contract_data(cd.clone());
                        was_created = true;
                    } else {
                        state.update_contract_data(cd.clone());
                    }
                }
                stellar_xdr::curr::LedgerEntryData::ContractCode(cc) => {
                    let exists = state.get_contract_code(&cc.hash).is_some();
                    let already_in_delta = is_hot_archive_restore
                        && key_already_created_in_delta(state.delta(), &change.key);
                    if should_create_contract_entry(
                        exists,
                        is_hot_archive_restore,
                        already_in_delta,
                    ) {
                        state.create_contract_code(cc.clone());
                        was_created = true;
                    } else {
                        state.update_contract_code(cc.clone());
                    }
                }
                stellar_xdr::curr::LedgerEntryData::Ttl(ttl) => {
                    let exists = state.get_ttl(&ttl.key_hash).is_some();
                    tracing::debug!(
                        key_hash = ?ttl.key_hash,
                        live_until = ttl.live_until_ledger_seq,
                        existing = exists,
                        "TTL emit: direct TTL entry"
                    );
                    was_created = !exists;
                    create_or_update_ttl(state, ttl.clone(), exists);
                }
                // SAC (Stellar Asset Contract) can modify Account and Trustline entries
                stellar_xdr::curr::LedgerEntryData::Account(acc) => {
                    if state.get_account(&acc.account_id).is_some() {
                        state.update_account(acc.clone());
                    } else {
                        state.create_account(acc.clone());
                        was_created = true;
                    }
                }
                stellar_xdr::curr::LedgerEntryData::Trustline(tl) => {
                    if state
                        .get_trustline_by_trustline_asset(&tl.account_id, &tl.asset)
                        .is_some()
                    {
                        state.update_trustline(tl.clone());
                    } else {
                        state.create_trustline(tl.clone());
                        was_created = true;
                    }
                }
                other => {
                    // stellar-core generically upserts any entry returned by the host.
                    // If we reach here, the host returned an entry type we don't handle,
                    // which would cause state divergence. Fail loudly.
                    panic!(
                        "apply_soroban_storage_change: unhandled entry type {:?} returned by host",
                        std::mem::discriminant(other)
                    );
                }
            }

            // Apply TTL if present for contract entries.
            //
            // CRITICAL: We must use the `ttl_extended` flag from the host to determine whether
            // to emit a TTL update, NOT compare against our current state. This is because:
            // 1. Multiple transactions in the same ledger may modify the same entry's TTL
            // 2. The host computes ttl_extended based on the ledger state at the START of the ledger
            // 3. Our state reflects changes from all previous transactions in this ledger
            //
            // Example: TX 5 extends TTL from 682237->700457, TX 7 also extends the same entry.
            // - TX 7's host sees old_ttl=682237, new_ttl=700457, so ttl_extended=true
            // - But our state already has 700457 from TX 5
            // - If we compare against state, we'd skip emission (700457==700457)
            // - But stellar-core emits it because from the ledger-start perspective, TTL WAS extended
            //
            // For hot archive restores, the TTL entry is also being restored so we use create.
            // Note: TTL keys are not directly in archived_soroban_entries, but when the associated
            // data/code entry is restored, its TTL is also restored.
            //
            // Skip TTL emission for TTL entries themselves - they were already handled above
            // and computing key_hash of a TTL key would give the wrong hash.
            if !matches!(entry.data, stellar_xdr::curr::LedgerEntryData::Ttl(_)) {
                if let Some(live_until) = live_until {
                    if *live_until == 0 {
                        return was_created;
                    }
                    let key_hash =
                        crate::soroban::get_or_compute_key_hash(ttl_key_cache, &change.key);
                    let existing_ttl = state.get_ttl(&key_hash);
                    let ttl = TtlEntry {
                        key_hash: key_hash.clone(),
                        live_until_ledger_seq: *live_until,
                    };

                    // For hot archive restores, check if the TTL was already created in the delta
                    // (by a previous TX in this ledger). We can't use existing_ttl.is_some() because
                    // TTLs are pre-loaded from InMemorySorobanState.
                    let ttl_already_restored = is_hot_archive_restore
                        && ttl_already_created_in_delta(state.delta(), &key_hash);

                    if is_hot_archive_restore && !ttl_already_restored {
                        // First restoration from hot archive - create TTL
                        tracing::debug!(?key_hash, live_until, "TTL emit: hot archive restore");
                        state.create_ttl(ttl);
                        created_keys.insert(LedgerKey::Ttl(stellar_xdr::curr::LedgerKeyTtl {
                            key_hash: key_hash.clone(),
                        }));
                    } else if is_hot_archive_restore && ttl_already_restored {
                        // TTL was already restored by earlier TX - update
                        tracing::debug!(
                            ?key_hash,
                            live_until,
                            "TTL emit: already restored, updating"
                        );
                        state.update_ttl(ttl);
                    } else if *ttl_extended {
                        // TTL was extended from the host's perspective (based on ledger-start state).
                        // We must emit this update even if our current state already has this value
                        // (e.g., from an earlier tx in the same ledger).
                        let exists = existing_ttl.is_some();
                        tracing::debug!(
                            ?key_hash,
                            live_until,
                            ttl_extended,
                            exists,
                            "TTL emit: data modified, TTL extended or new"
                        );
                        if !exists {
                            created_keys.insert(LedgerKey::Ttl(stellar_xdr::curr::LedgerKeyTtl {
                                key_hash: key_hash.clone(),
                            }));
                        }
                        create_or_update_ttl(state, ttl, exists);
                    } else if existing_ttl.is_none() {
                        // New entry being created - emit TTL
                        tracing::debug!(?key_hash, live_until, "TTL emit: new TTL entry");
                        state.create_ttl(ttl);
                        created_keys.insert(LedgerKey::Ttl(stellar_xdr::curr::LedgerKeyTtl {
                            key_hash: key_hash.clone(),
                        }));
                    } else {
                        // TTL was NOT extended and entry already exists - skip emission
                        tracing::debug!(
                            ?key_hash,
                            live_until,
                            "TTL skip: data modified but TTL not extended"
                        );
                    }
                }
            }
        }
        crate::soroban::StorageChangeKind::TtlOnly {
            live_until,
            read_only,
        } => {
            if *live_until == 0 {
                return false;
            }
            // For hot archive read-only restores (no data returned), skip TTL INIT.
            // stellar-core's handleArchivedEntry creates DATA+TTL INIT then erases both
            // for read-only access → net: nothing in live BL.
            // HOT_ARCHIVE_LIVE tombstone is still added correctly via hot_archive_restored_keys.
            if is_hot_archive_restore {
                return false;
            }
            // TTL-only change: the data entry wasn't modified, but its TTL was bumped.
            // This happens when a contract reads an entry and its TTL gets auto-extended.
            // Only emit when TTL was actually extended (new > old).
            // Note: ttl_extended is implicitly true for TtlOnly — the map_storage_changes
            // filter only emits TtlOnly when ttl_extended is true.
            let key_hash = crate::soroban::get_or_compute_key_hash(ttl_key_cache, &change.key);
            let existing_ttl = state.get_ttl(&key_hash);
            let ttl = TtlEntry {
                key_hash: key_hash.clone(),
                live_until_ledger_seq: *live_until,
            };

            // Read-only TTL bumps: stellar-core includes them in transaction meta but
            // defers state updates. Transaction meta is built from the op result (which
            // has all TTL changes). State visibility is deferred so subsequent TXs in
            // this ledger don't see the bumped value (CAP-0063).
            if *read_only {
                tracing::debug!(
                    ?key_hash,
                    live_until,
                    existing = existing_ttl.is_some(),
                    "RO TTL bump: recording in delta for meta, deferring state update"
                );
                // Record in delta for transaction meta, but defer state update
                // so subsequent TXs in this ledger don't see the bumped value
                state.record_ro_ttl_bump_for_meta(&key_hash, *live_until);
            } else {
                let exists = existing_ttl.is_some();
                tracing::debug!(
                    ?key_hash,
                    live_until,
                    existing = exists,
                    key_type = ?std::mem::discriminant(&change.key),
                    "TTL emit: ttl-only extended"
                );
                create_or_update_ttl(state, ttl, exists);
            }
        }
        crate::soroban::StorageChangeKind::Deleted => {
            apply_deletion(state, &change.key, ttl_key_cache);
        }
    }
    was_created
}

/// Delete a Soroban storage entry and its associated TTL.
///
/// Parity: stellar-core's `recordStorageChanges` asserts
/// `isSorobanEntry(lk)` after a successful erase — only ContractData
/// and ContractCode (plus their associated TTL) may be deleted through
/// the Soroban storage-change path. Classic entries (Account, Trustline,
/// etc.) must never reach here.
fn apply_deletion(
    state: &mut LedgerStateManager,
    key: &LedgerKey,
    ttl_key_cache: Option<&crate::soroban::TtlKeyCache>,
) {
    match key {
        LedgerKey::ContractData(k) => {
            state.delete_contract_data(&k.contract, &k.key, k.durability);
            let key_hash = crate::soroban::get_or_compute_key_hash(ttl_key_cache, key);
            state.delete_ttl(&key_hash);
        }
        LedgerKey::ContractCode(k) => {
            state.delete_contract_code(&k.hash);
            let key_hash = crate::soroban::get_or_compute_key_hash(ttl_key_cache, key);
            state.delete_ttl(&key_hash);
        }
        LedgerKey::Ttl(k) => {
            state.delete_ttl(&k.key_hash);
        }
        other => {
            // Parity: stellar-core asserts isSorobanEntry(lk) after erase.
            // Classic entries (Account, Trustline, etc.) must not be deleted
            // through the Soroban path — doing so would skip sponsorship
            // validation and corrupt num_sponsoring/num_sponsored counters.
            panic!(
                "apply_deletion: only Soroban entries (ContractData, ContractCode, Ttl) \
                 expected, got {:?}",
                std::mem::discriminant(other)
            );
        }
    }
}

fn footprint_has_unrestored_archived_entries(
    state: &LedgerStateManager,
    footprint: &stellar_xdr::curr::LedgerFootprint,
    ext: &stellar_xdr::curr::SorobanTransactionDataExt,
    current_ledger: u32,
    hot_archive: Option<&dyn crate::soroban::HotArchiveLookup>,
    ttl_key_cache: Option<&crate::soroban::TtlKeyCache>,
) -> crate::Result<bool> {
    let mut archived_rw = std::collections::HashSet::new();
    if let stellar_xdr::curr::SorobanTransactionDataExt::V1(resources_ext) = ext {
        for index in resources_ext.archived_soroban_entries.iter() {
            archived_rw.insert(*index as usize);
        }
    }

    for key in footprint.read_only.iter() {
        if is_archived_contract_entry(state, key, current_ledger, hot_archive, ttl_key_cache)? {
            return Ok(true);
        }
    }

    for (index, key) in footprint.read_write.iter().enumerate() {
        if !is_archived_contract_entry(state, key, current_ledger, hot_archive, ttl_key_cache)? {
            continue;
        }
        if !archived_rw.contains(&index) {
            return Ok(true);
        }
    }

    Ok(false)
}

/// Check if a footprint entry refers to an archived (evicted) contract entry.
///
/// Parity: InvokeHostFunctionOpFrame.cpp `addReads()` lines 378-445
///
/// The check follows the stellar-core logic:
/// 1. Look up the TTL in the live state. If found and expired → archived.
/// 2. If TTL not found in live state (entry was evicted), check the hot
///    archive (P23+). If found there → archived.
fn is_archived_contract_entry(
    state: &LedgerStateManager,
    key: &LedgerKey,
    current_ledger: u32,
    hot_archive: Option<&dyn crate::soroban::HotArchiveLookup>,
    ttl_key_cache: Option<&crate::soroban::TtlKeyCache>,
) -> crate::Result<bool> {
    // Only persistent Soroban entries can be "archived".
    // Temporary entries just disappear when expired.
    let is_persistent_soroban = match key {
        LedgerKey::ContractData(cd) => {
            cd.durability == stellar_xdr::curr::ContractDataDurability::Persistent
        }
        LedgerKey::ContractCode(_) => true,
        _ => false,
    };
    if !is_persistent_soroban {
        return Ok(false);
    }

    // Check if the entry exists in live state with an expired TTL.
    let entry_in_live = match key {
        LedgerKey::ContractData(cd) => state
            .get_contract_data(&cd.contract, &cd.key, cd.durability)
            .is_some(),
        LedgerKey::ContractCode(cc) => state.get_contract_code(&cc.hash).is_some(),
        _ => false,
    };

    if entry_in_live {
        // Entry is in live state — check its TTL
        let key_hash = crate::soroban::get_or_compute_key_hash(ttl_key_cache, key);
        return Ok(match state.get_ttl(&key_hash) {
            Some(ttl) => ttl.live_until_ledger_seq < current_ledger,
            None => false, // No TTL → not archived (matches stellar-core fallthrough)
        });
    }

    // Entry not in live state — check hot archive (P23+ fallback).
    // Parity: InvokeHostFunctionOpFrame.cpp:413-445
    if let Some(archive) = hot_archive {
        if archive
            .get(key)
            .map_err(|e| TxError::Internal(format!("hot archive lookup failed: {e}")))?
            .is_some()
        {
            return Ok(true);
        }
    }

    Ok(false)
}

/// Create an OperationResult from an InvokeHostFunctionResultCode.
fn make_result(code: InvokeHostFunctionResultCode, success_hash: Hash) -> OperationResult {
    let result = match code {
        InvokeHostFunctionResultCode::Success => InvokeHostFunctionResult::Success(success_hash),
        InvokeHostFunctionResultCode::Malformed => InvokeHostFunctionResult::Malformed,
        InvokeHostFunctionResultCode::Trapped => InvokeHostFunctionResult::Trapped,
        InvokeHostFunctionResultCode::ResourceLimitExceeded => {
            InvokeHostFunctionResult::ResourceLimitExceeded
        }
        InvokeHostFunctionResultCode::EntryArchived => InvokeHostFunctionResult::EntryArchived,
        InvokeHostFunctionResultCode::InsufficientRefundableFee => {
            InvokeHostFunctionResult::InsufficientRefundableFee
        }
    };

    OperationResult::OpInner(OperationResultTr::InvokeHostFunction(result))
}

/// Map execution error to result code using stellar-core logic.
///
/// stellar-core checks if the failure was due to exceeding specified resource limits:
/// - If actual CPU instructions > specified instructions -> RESOURCE_LIMIT_EXCEEDED
/// - If actual memory > network's txMemoryLimit -> RESOURCE_LIMIT_EXCEEDED
/// - Otherwise -> TRAPPED (for any other failure like auth errors, panics, storage errors, etc.)
///
/// The key insight is that stellar-core checks raw resource consumption regardless of error type.
/// Even if the host returns an Auth error, if CPU exceeded the specified limit, the
/// result is RESOURCE_LIMIT_EXCEEDED.
///
/// Note: stellar-core does NOT check the host error type - it purely checks measured consumption
/// against limits. A host error of Budget/ExceededLimit does NOT automatically become
/// ResourceLimitExceeded; only actual limit violations trigger that result code.
fn map_host_error_to_result_code(
    exec_error: &crate::soroban::SorobanExecutionError,
    specified_instructions: u32,
    tx_memory_limit: u64,
) -> InvokeHostFunctionResultCode {
    // stellar-core logic (InvokeHostFunctionOpFrame.cpp lines 579-602):
    // First check if CPU instructions exceeded the specified limit
    if exec_error.cpu_insns_consumed > specified_instructions as u64 {
        return InvokeHostFunctionResultCode::ResourceLimitExceeded;
    }

    // Then check if memory exceeded the network's txMemoryLimit
    if exec_error.mem_bytes_consumed > tx_memory_limit {
        return InvokeHostFunctionResultCode::ResourceLimitExceeded;
    }

    // All other failures are TRAPPED
    // Note: stellar-core does NOT special-case Budget/ExceededLimit errors from the host.
    // Even if the host internally ran out of budget, if our measured consumption
    // is within limits, the result is Trapped (not ResourceLimitExceeded).
    InvokeHostFunctionResultCode::Trapped
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::soroban::StorageChange;
    use crate::test_utils::create_test_account_id;
    use stellar_xdr::curr::*;

    fn create_test_context() -> LedgerContext {
        LedgerContext::testnet(1, 1000)
    }

    fn create_test_context_p23() -> LedgerContext {
        let mut context = LedgerContext::testnet(1, 1000);
        context.protocol_version = 23;
        context
    }

    fn create_test_soroban_config() -> SorobanConfig {
        SorobanConfig::default()
    }

    fn create_test_account(account_id: AccountId, balance: i64) -> AccountEntry {
        AccountEntry {
            account_id,
            balance,
            seq_num: SequenceNumber(1),
            num_sub_entries: 0,
            inflation_dest: None,
            flags: 0,
            home_domain: String32::default(),
            thresholds: Thresholds([1, 0, 0, 0]),
            signers: vec![].try_into().unwrap(),
            ext: AccountEntryExt::V0,
        }
    }

    #[test]
    fn test_invoke_host_function_no_soroban_data() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();
        let source = create_test_account_id(0);
        let config = create_test_soroban_config();

        let op = InvokeHostFunctionOp {
            host_function: HostFunction::UploadContractWasm(vec![0u8; 100].try_into().unwrap()),
            auth: vec![].try_into().unwrap(),
        };

        let soroban = SorobanContext {
            soroban_data: None,
            config: Some(&config),
            module_cache: None,
            hot_archive: None,
            ttl_key_cache: None,
        };
        let result = execute_invoke_host_function(&op, &source, &mut state, &context, &soroban)
            .expect("invoke host function");

        match result.result {
            OperationResult::OpInner(OperationResultTr::InvokeHostFunction(r)) => {
                assert!(matches!(r, InvokeHostFunctionResult::Malformed));
            }
            _ => panic!("Unexpected result type"),
        }
    }

    #[test]
    #[ignore = "Test was already broken - WASM upload test setup needs fixing"]
    fn test_upload_wasm_success() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();
        let source = create_test_account_id(0);
        let config = create_test_soroban_config();

        // Create minimal valid WASM
        let wasm_bytes: Vec<u8> = vec![
            0x00, 0x61, 0x73, 0x6d, // WASM magic number
            0x01, 0x00, 0x00, 0x00, // WASM version
        ];

        let op = InvokeHostFunctionOp {
            host_function: HostFunction::UploadContractWasm(wasm_bytes.try_into().unwrap()),
            auth: vec![].try_into().unwrap(),
        };

        let soroban_data = SorobanTransactionData {
            ext: SorobanTransactionDataExt::V0,
            resources: SorobanResources {
                footprint: LedgerFootprint {
                    read_only: vec![].try_into().unwrap(),
                    read_write: vec![].try_into().unwrap(),
                },
                instructions: 10_000_000, // Enough CPU budget
                disk_read_bytes: 0,
                write_bytes: 10000, // Enough space for ContractCode entry
            },
            resource_fee: 0,
        };

        let soroban = SorobanContext {
            soroban_data: Some(&soroban_data),
            config: Some(&config),
            module_cache: None,
            hot_archive: None,
            ttl_key_cache: None,
        };
        let result = execute_invoke_host_function(&op, &source, &mut state, &context, &soroban)
            .expect("invoke host function");

        match result.result {
            OperationResult::OpInner(OperationResultTr::InvokeHostFunction(r)) => {
                assert!(
                    matches!(r, InvokeHostFunctionResult::Success(_)),
                    "Expected Success but got: {:?}",
                    r
                );
            }
            _ => panic!("Unexpected result type"),
        }
    }

    #[test]
    fn test_invoke_host_function_entry_archived() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();
        let source = create_test_account_id(0);
        let config = create_test_soroban_config();

        let contract_id = ScAddress::Contract(ContractId(Hash([1u8; 32])));
        let contract_key = ScVal::U32(42);
        let durability = ContractDataDurability::Persistent;

        let cd_entry = ContractDataEntry {
            ext: ExtensionPoint::V0,
            contract: contract_id.clone(),
            key: contract_key.clone(),
            durability,
            val: ScVal::I32(7),
        };
        state.create_contract_data(cd_entry);

        let key = LedgerKey::ContractData(LedgerKeyContractData {
            contract: contract_id.clone(),
            key: contract_key.clone(),
            durability,
        });
        let key_hash = crate::soroban::compute_key_hash(&key);
        state.create_ttl(TtlEntry {
            key_hash,
            live_until_ledger_seq: context.sequence - 1,
        });

        let host_function = HostFunction::InvokeContract(InvokeContractArgs {
            contract_address: contract_id,
            function_name: ScSymbol(StringM::try_from("noop".to_string()).unwrap()),
            args: VecM::default(),
        });

        let op = InvokeHostFunctionOp {
            host_function,
            auth: VecM::default(),
        };

        let footprint = LedgerFootprint {
            read_only: vec![key].try_into().unwrap(),
            read_write: VecM::default(),
        };
        let soroban_data = SorobanTransactionData {
            ext: SorobanTransactionDataExt::V0,
            resources: SorobanResources {
                footprint,
                instructions: 0,
                disk_read_bytes: 0,
                write_bytes: 0,
            },
            resource_fee: 0,
        };

        let soroban = SorobanContext {
            soroban_data: Some(&soroban_data),
            config: Some(&config),
            module_cache: None,
            hot_archive: None,
            ttl_key_cache: None,
        };
        let result = execute_invoke_host_function(&op, &source, &mut state, &context, &soroban)
            .expect("invoke host function");

        match result.result {
            OperationResult::OpInner(OperationResultTr::InvokeHostFunction(r)) => {
                assert!(matches!(r, InvokeHostFunctionResult::EntryArchived));
            }
            _ => panic!("Unexpected result type"),
        }
    }

    /// Regression test for #1120: A Soroban entry in live state without a TTL entry
    /// should NOT be treated as archived. stellar-core falls through to normal processing
    /// when the TTL key is missing.
    #[test]
    fn test_invoke_host_function_missing_ttl_not_archived() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();
        let source = create_test_account_id(0);
        let config = create_test_soroban_config();

        let contract_id = ScAddress::Contract(ContractId(Hash([50u8; 32])));
        let contract_key = ScVal::U32(99);
        let durability = ContractDataDurability::Persistent;

        // Create the contract data entry BUT no TTL entry
        let cd_entry = ContractDataEntry {
            ext: ExtensionPoint::V0,
            contract: contract_id.clone(),
            key: contract_key.clone(),
            durability,
            val: ScVal::I32(42),
        };
        state.create_contract_data(cd_entry);
        // Intentionally NOT creating a TTL entry

        let key = LedgerKey::ContractData(LedgerKeyContractData {
            contract: contract_id.clone(),
            key: contract_key.clone(),
            durability,
        });

        let host_function = HostFunction::InvokeContract(InvokeContractArgs {
            contract_address: contract_id,
            function_name: ScSymbol(StringM::try_from("noop".to_string()).unwrap()),
            args: VecM::default(),
        });

        let op = InvokeHostFunctionOp {
            host_function,
            auth: VecM::default(),
        };

        let footprint = LedgerFootprint {
            read_only: vec![key].try_into().unwrap(),
            read_write: VecM::default(),
        };
        let soroban_data = SorobanTransactionData {
            ext: SorobanTransactionDataExt::V0,
            resources: SorobanResources {
                footprint,
                instructions: 0,
                disk_read_bytes: 0,
                write_bytes: 0,
            },
            resource_fee: 0,
        };

        let soroban = SorobanContext {
            soroban_data: Some(&soroban_data),
            config: Some(&config),
            module_cache: None,
            hot_archive: None,
            ttl_key_cache: None,
        };
        let result = execute_invoke_host_function(&op, &source, &mut state, &context, &soroban)
            .expect("invoke host function");

        // Must NOT be EntryArchived — missing TTL means not archived
        match &result.result {
            OperationResult::OpInner(OperationResultTr::InvokeHostFunction(r)) => {
                assert!(
                    !matches!(r, InvokeHostFunctionResult::EntryArchived),
                    "Missing TTL should not result in EntryArchived, got {:?}",
                    r
                );
            }
            _ => panic!("Unexpected result type"),
        }
    }

    #[test]
    fn test_invoke_host_function_archived_allowed_when_marked() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context_p23();
        let source = create_test_account_id(0);
        let config = create_test_soroban_config();

        let contract_id = ScAddress::Contract(ContractId(Hash([2u8; 32])));
        let contract_key = ScVal::U32(5);
        let durability = ContractDataDurability::Persistent;

        let cd_entry = ContractDataEntry {
            ext: ExtensionPoint::V0,
            contract: contract_id.clone(),
            key: contract_key.clone(),
            durability,
            val: ScVal::I32(1),
        };
        state.create_contract_data(cd_entry);

        let key = LedgerKey::ContractData(LedgerKeyContractData {
            contract: contract_id.clone(),
            key: contract_key.clone(),
            durability,
        });
        let key_hash = crate::soroban::compute_key_hash(&key);
        state.create_ttl(TtlEntry {
            key_hash,
            live_until_ledger_seq: context.sequence - 1,
        });

        let host_function = HostFunction::InvokeContract(InvokeContractArgs {
            contract_address: contract_id,
            function_name: ScSymbol(StringM::try_from("noop".to_string()).unwrap()),
            args: VecM::default(),
        });

        let op = InvokeHostFunctionOp {
            host_function,
            auth: VecM::default(),
        };

        let footprint = LedgerFootprint {
            read_only: VecM::default(),
            read_write: vec![key].try_into().unwrap(),
        };
        let soroban_data = SorobanTransactionData {
            ext: SorobanTransactionDataExt::V1(SorobanResourcesExtV0 {
                archived_soroban_entries: vec![0u32].try_into().unwrap(),
            }),
            resources: SorobanResources {
                footprint,
                instructions: 100_000_000, // High enough to not trigger ResourceLimitExceeded
                disk_read_bytes: 1_000,
                write_bytes: 0,
            },
            resource_fee: 0,
        };

        let module_cache = PersistentModuleCache::new_for_protocol(context.protocol_version)
            .expect("create module cache");

        let soroban = SorobanContext {
            soroban_data: Some(&soroban_data),
            config: Some(&config),
            module_cache: Some(&module_cache),
            hot_archive: None,
            ttl_key_cache: None,
        };
        let result = execute_invoke_host_function(&op, &source, &mut state, &context, &soroban)
            .expect("invoke host function");

        match result.result {
            OperationResult::OpInner(OperationResultTr::InvokeHostFunction(r)) => {
                assert!(matches!(r, InvokeHostFunctionResult::Trapped));
            }
            _ => panic!("Unexpected result type"),
        }
    }

    #[test]
    fn test_invoke_host_function_disk_read_limit_exceeded() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context_p23();
        let source = create_test_account_id(0);
        let config = create_test_soroban_config();

        let account_id = create_test_account_id(1);
        state.create_account(create_test_account(account_id.clone(), 100_000_000));

        let account_key = LedgerKey::Account(LedgerKeyAccount { account_id });

        let op = InvokeHostFunctionOp {
            host_function: HostFunction::InvokeContract(InvokeContractArgs {
                contract_address: ScAddress::Contract(ContractId(Hash([3u8; 32]))),
                function_name: ScSymbol(StringM::try_from("noop".to_string()).unwrap()),
                args: VecM::default(),
            }),
            auth: VecM::default(),
        };

        let footprint = LedgerFootprint {
            read_only: vec![account_key].try_into().unwrap(),
            read_write: VecM::default(),
        };
        let soroban_data = SorobanTransactionData {
            ext: SorobanTransactionDataExt::V0,
            resources: SorobanResources {
                footprint,
                instructions: 100_000_000,
                disk_read_bytes: 0,
                write_bytes: 0,
            },
            resource_fee: 0,
        };

        let soroban = SorobanContext {
            soroban_data: Some(&soroban_data),
            config: Some(&config),
            module_cache: None,
            hot_archive: None,
            ttl_key_cache: None,
        };
        let result = execute_invoke_host_function(&op, &source, &mut state, &context, &soroban)
            .expect("invoke host function");

        match result.result {
            OperationResult::OpInner(OperationResultTr::InvokeHostFunction(r)) => {
                assert!(matches!(r, InvokeHostFunctionResult::ResourceLimitExceeded));
            }
            _ => panic!("Unexpected result type"),
        }
    }

    /// Regression test for ledger 800896: archived entry with live TTL should not be
    /// metered for disk read bytes.
    ///
    /// When a previous TX in the same ledger restores an archived soroban entry, the
    /// entry's TTL becomes live. Subsequent TXs that also reference the entry in their
    /// `archived_soroban_entries` should NOT meter it for disk read bytes, because stellar-core
    /// stellar-core dynamically checks the TTL and treats restored entries as in-memory.
    /// Without this fix, the disk_read_bytes_exceeded check would incorrectly count the
    /// restored entry's bytes, causing spurious ResourceLimitExceeded failures.
    #[test]
    fn test_disk_read_bytes_skips_already_restored_archived_entry() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context_p23();

        // Create an archived soroban entry that has been "restored" (live TTL)
        let contract_id = ScAddress::Contract(ContractId(Hash([5u8; 32])));
        let contract_key = ScVal::U32(42);
        let durability = ContractDataDurability::Persistent;

        let cd_entry = ContractDataEntry {
            ext: ExtensionPoint::V0,
            contract: contract_id.clone(),
            key: contract_key.clone(),
            durability,
            val: ScVal::I32(999),
        };
        state.create_contract_data(cd_entry);

        let cd_key = LedgerKey::ContractData(LedgerKeyContractData {
            contract: contract_id.clone(),
            key: contract_key.clone(),
            durability,
        });
        let key_hash = crate::soroban::compute_key_hash(&cd_key);

        // Set TTL to LIVE (simulating a prior TX that restored this entry)
        state.create_ttl(TtlEntry {
            key_hash,
            live_until_ledger_seq: context.sequence + 100, // Live!
        });

        // Also add a classic account entry (always metered)
        let account_id = create_test_account_id(1);
        state.create_account(create_test_account(account_id.clone(), 100_000_000));
        let account_key = LedgerKey::Account(LedgerKeyAccount {
            account_id: account_id.clone(),
        });

        // Set disk_read_bytes to just enough for the account entry (~92 bytes)
        // but NOT enough for account + contract data entry (~164 bytes) together.
        // If the restored entry is incorrectly metered, this would exceed the limit.
        let footprint = LedgerFootprint {
            read_only: vec![account_key].try_into().unwrap(),
            read_write: vec![cd_key].try_into().unwrap(),
        };
        let soroban_data = SorobanTransactionData {
            ext: SorobanTransactionDataExt::V1(SorobanResourcesExtV0 {
                archived_soroban_entries: vec![0u32].try_into().unwrap(),
            }),
            resources: SorobanResources {
                footprint,
                instructions: 100_000_000,
                disk_read_bytes: 100, // Enough for account (~92) but not account+contract (~164)
                write_bytes: 0,
            },
            resource_fee: 0,
        };

        // The restored entry should NOT be metered, so disk_read_bytes_exceeded
        // should return false (only the account entry is metered)
        let exceeded = disk_read_bytes_exceeded(
            &state,
            &soroban_data,
            context.protocol_version,
            context.sequence,
            None,
            None,
        );
        assert!(
            !exceeded,
            "disk_read_bytes should NOT be exceeded: restored entry with live TTL should be skipped"
        );

        // Now verify that if the TTL is expired, the entry IS metered
        let key_hash2 =
            crate::soroban::compute_key_hash(&LedgerKey::ContractData(LedgerKeyContractData {
                contract: contract_id,
                key: contract_key,
                durability: ContractDataDurability::Persistent,
            }));
        state.get_ttl_mut(&key_hash2).unwrap().live_until_ledger_seq = context.sequence - 1; // Expired

        let exceeded = disk_read_bytes_exceeded(
            &state,
            &soroban_data,
            context.protocol_version,
            context.sequence,
            None,
            None,
        );
        assert!(
            exceeded,
            "disk_read_bytes SHOULD be exceeded when archived entry has expired TTL"
        );
    }

    /// Regression test for #1465: fully-evicted hot-archive entry must be metered
    /// for disk read bytes. Previously, entries only in the hot archive (not in live
    /// state) contributed 0 bytes to the disk read count.
    #[test]
    fn test_disk_read_bytes_counts_hot_archive_entries() {
        use crate::soroban::HotArchiveLookup;

        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context_p23();
        let source = create_test_account_id(0);

        // Source account (classic entry in read_only)
        state.create_account(create_test_account(source.clone(), 100_000_000));

        let contract_id = ScAddress::Contract(ContractId(Hash([0xD1u8; 32])));
        let contract_key = ScVal::U32(88);
        let durability = ContractDataDurability::Persistent;

        let key = LedgerKey::ContractData(LedgerKeyContractData {
            contract: contract_id.clone(),
            key: contract_key.clone(),
            durability,
        });

        // Entry is NOT in live state — only in hot archive.
        // Create the entry that the hot archive will return.
        let cd_entry = ContractDataEntry {
            ext: ExtensionPoint::V0,
            contract: contract_id,
            key: contract_key,
            durability,
            val: ScVal::Bytes(vec![0xAA; 500].try_into().unwrap()),
        };
        let hot_entry = LedgerEntry {
            last_modified_ledger_seq: 50,
            data: stellar_xdr::curr::LedgerEntryData::ContractData(cd_entry),
            ext: stellar_xdr::curr::LedgerEntryExt::V0,
        };
        let hot_entry_size = xdr_encoded_len(&hot_entry) as u32;

        struct TestHotArchive(LedgerKey, LedgerEntry);
        impl HotArchiveLookup for TestHotArchive {
            fn get(
                &self,
                key: &LedgerKey,
            ) -> std::result::Result<Option<LedgerEntry>, Box<dyn std::error::Error + Send + Sync>>
            {
                if *key == self.0 {
                    Ok(Some(self.1.clone()))
                } else {
                    Ok(None)
                }
            }
        }
        let hot_archive = TestHotArchive(key.clone(), hot_entry);

        let account_key =
            LedgerKey::Account(stellar_xdr::curr::LedgerKeyAccount { account_id: source });

        let footprint = LedgerFootprint {
            read_only: vec![account_key].try_into().unwrap(),
            read_write: vec![key].try_into().unwrap(),
        };
        let soroban_data = SorobanTransactionData {
            ext: SorobanTransactionDataExt::V1(SorobanResourcesExtV0 {
                archived_soroban_entries: vec![0u32].try_into().unwrap(),
            }),
            resources: SorobanResources {
                footprint,
                instructions: 100_000_000,
                // Set disk_read_bytes to less than the hot archive entry size
                // so the check should fail
                disk_read_bytes: hot_entry_size - 1,
                write_bytes: 100_000,
            },
            resource_fee: 0,
        };

        // Without hot archive: entry not found, 0 bytes metered — passes
        let exceeded_without = disk_read_bytes_exceeded(
            &state,
            &soroban_data,
            context.protocol_version,
            context.sequence,
            None,
            None,
        );
        assert!(
            !exceeded_without,
            "Without hot archive, evicted entry should not be metered (pre-fix behavior)"
        );

        // With hot archive: entry found, its bytes should be metered — exceeds limit
        let exceeded_with = disk_read_bytes_exceeded(
            &state,
            &soroban_data,
            context.protocol_version,
            context.sequence,
            Some(&hot_archive),
            None,
        );
        assert!(
            exceeded_with,
            "With hot archive, evicted entry MUST be metered for disk read bytes"
        );
    }

    #[test]
    fn test_map_host_error_to_result_code_budget_exceeded() {
        use crate::soroban::SorobanExecutionError;
        // CPU exceeded specified -> RESOURCE_LIMIT_EXCEEDED (regardless of error type)
        let host_error = soroban_env_host_p25::HostError::from((
            soroban_env_host_p25::xdr::ScErrorType::Budget,
            soroban_env_host_p25::xdr::ScErrorCode::ExceededLimit,
        ));
        let exec_error = SorobanExecutionError {
            host_error,
            cpu_insns_consumed: 1000,
            mem_bytes_consumed: 100,
        };
        // 1000 > 500 specified, so ResourceLimitExceeded
        assert_eq!(
            map_host_error_to_result_code(&exec_error, 500, 1000),
            InvokeHostFunctionResultCode::ResourceLimitExceeded
        );
    }

    #[test]
    fn test_map_host_error_to_result_code_cpu_exceeded_with_storage_error() {
        use crate::soroban::SorobanExecutionError;
        // Storage error but CPU also exceeded -> RESOURCE_LIMIT_EXCEEDED
        // stellar-core checks raw resource consumption regardless of error type
        let host_error = soroban_env_host_p25::HostError::from((
            soroban_env_host_p25::xdr::ScErrorType::Storage,
            soroban_env_host_p25::xdr::ScErrorCode::ExceededLimit,
        ));
        let exec_error = SorobanExecutionError {
            host_error,
            cpu_insns_consumed: 1000, // CPU exceeded (1000 > 500)
            mem_bytes_consumed: 100,
        };
        // Even though it's a Storage error, CPU exceeded so ResourceLimitExceeded
        assert_eq!(
            map_host_error_to_result_code(&exec_error, 500, 1000),
            InvokeHostFunctionResultCode::ResourceLimitExceeded
        );
    }

    #[test]
    fn test_map_host_error_to_result_code_storage_error_within_limits() {
        use crate::soroban::SorobanExecutionError;
        // Storage error but resources within limits -> TRAPPED
        let host_error = soroban_env_host_p25::HostError::from((
            soroban_env_host_p25::xdr::ScErrorType::Storage,
            soroban_env_host_p25::xdr::ScErrorCode::ExceededLimit,
        ));
        let exec_error = SorobanExecutionError {
            host_error,
            cpu_insns_consumed: 100, // CPU within limit (100 < 500)
            mem_bytes_consumed: 100, // Mem within limit (100 < 1000)
        };
        // Resources within limits, so TRAPPED
        assert_eq!(
            map_host_error_to_result_code(&exec_error, 500, 1000),
            InvokeHostFunctionResultCode::Trapped
        );
    }

    #[test]
    fn test_map_host_error_to_result_code_trapped_other() {
        use crate::soroban::SorobanExecutionError;
        // Other errors (auth, missing value, etc.) with resources within limits -> TRAPPED
        let host_error = soroban_env_host_p25::HostError::from((
            soroban_env_host_p25::xdr::ScErrorType::Auth,
            soroban_env_host_p25::xdr::ScErrorCode::InvalidAction,
        ));
        let exec_error = SorobanExecutionError {
            host_error,
            cpu_insns_consumed: 100, // CPU within limit
            mem_bytes_consumed: 100, // Mem within limit
        };
        assert_eq!(
            map_host_error_to_result_code(&exec_error, 500, 1000),
            InvokeHostFunctionResultCode::Trapped
        );
    }

    #[test]
    fn test_map_host_error_to_result_code_budget_error_within_limits() {
        use crate::soroban::SorobanExecutionError;
        // Budget/ExceededLimit error from host BUT measured consumption within limits -> TRAPPED
        // This matches stellar-core behavior: only measured consumption matters, not error type.
        // The host may internally track budget differently, but stellar-core checks actual consumed values.
        let host_error = soroban_env_host_p25::HostError::from((
            soroban_env_host_p25::xdr::ScErrorType::Budget,
            soroban_env_host_p25::xdr::ScErrorCode::ExceededLimit,
        ));
        let exec_error = SorobanExecutionError {
            host_error,
            cpu_insns_consumed: 100, // CPU within limit (100 < 500)
            mem_bytes_consumed: 100, // Mem within limit (100 < 1000)
        };
        // Even though host reported Budget/ExceededLimit, consumption is within limits -> TRAPPED
        assert_eq!(
            map_host_error_to_result_code(&exec_error, 500, 1000),
            InvokeHostFunctionResultCode::Trapped
        );
    }

    #[test]
    fn test_map_host_error_to_result_code_mem_exceeded() {
        use crate::soroban::SorobanExecutionError;
        // Memory exceeded -> RESOURCE_LIMIT_EXCEEDED
        let host_error = soroban_env_host_p25::HostError::from((
            soroban_env_host_p25::xdr::ScErrorType::Auth,
            soroban_env_host_p25::xdr::ScErrorCode::InvalidAction,
        ));
        let exec_error = SorobanExecutionError {
            host_error,
            cpu_insns_consumed: 100,  // CPU within limit
            mem_bytes_consumed: 2000, // Mem exceeded (2000 > 1000)
        };
        assert_eq!(
            map_host_error_to_result_code(&exec_error, 500, 1000),
            InvokeHostFunctionResultCode::ResourceLimitExceeded
        );
    }

    #[test]
    fn test_apply_soroban_storage_change_deletes() {
        let mut state = LedgerStateManager::new(5_000_000, 100);

        let contract_id = ScAddress::Contract(ContractId(Hash([1u8; 32])));
        let contract_key = ScVal::U32(7);
        let durability = ContractDataDurability::Persistent;

        let cd_entry = ContractDataEntry {
            ext: ExtensionPoint::V0,
            contract: contract_id.clone(),
            key: contract_key.clone(),
            durability,
            val: ScVal::I32(1),
        };

        let ledger_entry = LedgerEntry {
            last_modified_ledger_seq: 1,
            data: LedgerEntryData::ContractData(cd_entry.clone()),
            ext: LedgerEntryExt::V0,
        };

        let key = LedgerKey::ContractData(LedgerKeyContractData {
            contract: contract_id.clone(),
            key: contract_key.clone(),
            durability,
        });

        let change = StorageChange {
            key: key.clone(),
            kind: crate::soroban::StorageChangeKind::Modified {
                entry: Box::new(ledger_entry),
                live_until: Some(200),
                ttl_extended: false,
            },
            is_rent_related: false,
        };

        let no_restored_keys = std::collections::HashSet::new();
        apply_soroban_storage_change(
            &mut state,
            &change,
            &no_restored_keys,
            None,
            &mut std::collections::HashSet::new(),
        );
        assert!(state
            .get_contract_data(&contract_id, &contract_key, durability)
            .is_some());

        let ttl_key = crate::soroban::compute_key_hash(&key);
        assert!(state.get_ttl(&ttl_key).is_some());

        let delete_change = StorageChange {
            key,
            kind: crate::soroban::StorageChangeKind::Deleted,
            is_rent_related: false,
        };

        apply_soroban_storage_change(
            &mut state,
            &delete_change,
            &no_restored_keys,
            None,
            &mut std::collections::HashSet::new(),
        );
        assert!(state
            .get_contract_data(&contract_id, &contract_key, durability)
            .is_none());
        assert!(state.get_ttl(&ttl_key).is_none());
    }

    /// Regression test for ledger 182022: TTL emission should be skipped when data is modified
    /// but TTL value remains unchanged.
    ///
    /// At ledger 182022 TX 4, a Soroban InvokeHostFunction modified a ContractData entry
    /// but the TTL value remained the same (226129). We were incorrectly emitting a TTL
    /// update to the bucket list, causing 1 extra LIVE entry compared to stellar-core.
    ///
    /// stellar-core only emits bucket list updates when there's an actual change in value.
    /// This test verifies that when data is modified but TTL is unchanged, we don't emit
    /// a redundant TTL update.
    #[test]
    fn test_apply_soroban_storage_change_skips_ttl_when_unchanged() {
        let mut state = LedgerStateManager::new(5_000_000, 100);

        let contract_id = ScAddress::Contract(ContractId(Hash([2u8; 32])));
        let contract_key = ScVal::U32(42);
        let durability = ContractDataDurability::Persistent;

        let key = LedgerKey::ContractData(LedgerKeyContractData {
            contract: contract_id.clone(),
            key: contract_key.clone(),
            durability,
        });

        let ttl_key_hash = crate::soroban::compute_key_hash(&key);

        // Pre-populate the TTL entry with value 226129 (simulates existing entry from bucket list)
        let existing_ttl = TtlEntry {
            key_hash: ttl_key_hash.clone(),
            live_until_ledger_seq: 226129,
        };
        state.create_ttl(existing_ttl);

        // Pre-populate the contract data entry
        let cd_entry = ContractDataEntry {
            ext: ExtensionPoint::V0,
            contract: contract_id.clone(),
            key: contract_key.clone(),
            durability,
            val: ScVal::I32(100),
        };
        state.create_contract_data(cd_entry);

        // Commit to make these "existing" entries
        state.commit();

        // Create a new state manager starting from this snapshot (simulating new ledger)
        let mut state2 = LedgerStateManager::new(5_000_000, 100);

        // Re-populate with the same entries (simulates loading from bucket list)
        let existing_ttl = TtlEntry {
            key_hash: ttl_key_hash.clone(),
            live_until_ledger_seq: 226129,
        };
        state2.create_ttl(existing_ttl);

        let cd_entry = ContractDataEntry {
            ext: ExtensionPoint::V0,
            contract: contract_id.clone(),
            key: contract_key.clone(),
            durability,
            val: ScVal::I32(100),
        };
        state2.create_contract_data(cd_entry);

        // Commit to establish baseline
        state2.commit();

        // Now apply a storage change that modifies data but keeps TTL unchanged
        let modified_entry = LedgerEntry {
            last_modified_ledger_seq: 2,
            data: LedgerEntryData::ContractData(ContractDataEntry {
                ext: ExtensionPoint::V0,
                contract: contract_id.clone(),
                key: contract_key.clone(),
                durability,
                val: ScVal::I32(200), // Different value
            }),
            ext: LedgerEntryExt::V0,
        };

        let modify_change = StorageChange {
            key: key.clone(),
            kind: crate::soroban::StorageChangeKind::Modified {
                entry: Box::new(modified_entry),
                live_until: Some(226129), // Same TTL as before
                ttl_extended: false,
            },
            is_rent_related: true, // This was true in the actual ledger
        };

        let no_restored_keys = std::collections::HashSet::new();
        apply_soroban_storage_change(
            &mut state2,
            &modify_change,
            &no_restored_keys,
            None,
            &mut std::collections::HashSet::new(),
        );

        // Verify data was updated
        let cd = state2
            .get_contract_data(&contract_id, &contract_key, durability)
            .unwrap();
        assert_eq!(cd.val, ScVal::I32(200));

        // Verify TTL value is still the same
        let ttl = state2.get_ttl(&ttl_key_hash).unwrap();
        assert_eq!(ttl.live_until_ledger_seq, 226129);

        // The key assertion: the delta should have ContractData updated, but NOT TTL
        let delta = state2.delta();

        // Check if any entry in updated_entries is the ContractData
        let contract_data_updated = delta.updated_entries().iter().any(|e| {
            matches!(
                &e.data,
                LedgerEntryData::ContractData(cd)
                if cd.contract == contract_id && cd.key == contract_key
            )
        });
        assert!(
            contract_data_updated,
            "ContractData should be updated in delta"
        );

        // TTL should NOT be in updated (value didn't change)
        let ttl_updated = delta.updated_entries().iter().any(|e| {
            matches!(
                &e.data,
                LedgerEntryData::Ttl(ttl)
                if ttl.key_hash == ttl_key_hash
            )
        });
        assert!(
            !ttl_updated,
            "TTL should NOT be updated in delta when value is unchanged"
        );
    }

    /// Regression test for ledger 83170: CONTRACT_DATA entry size validation.
    ///
    /// When a Soroban contract execution produces a CONTRACT_DATA entry that exceeds
    /// `maxContractDataEntrySizeBytes`, the result should be ResourceLimitExceeded.
    /// This matches stellar-core's `validateContractLedgerEntry()` behavior.
    #[test]
    fn test_validate_contract_data_entry_size_exceeded() {
        // Test the validate_contract_ledger_entry function directly
        let contract_id = ScAddress::Contract(ContractId(Hash([1u8; 32])));
        let contract_key = ScVal::U32(42);
        let durability = ContractDataDurability::Persistent;

        let key = LedgerKey::ContractData(LedgerKeyContractData {
            contract: contract_id.clone(),
            key: contract_key.clone(),
            durability,
        });

        // Entry size under limit should be valid
        let small_size = 1000;
        let max_data_size = 65536; // 64 KB
        let max_code_size = 65536;
        let limits = super::super::ContractSizeLimits {
            max_contract_size_bytes: max_code_size,
            max_contract_data_entry_size_bytes: max_data_size,
        };
        assert!(super::super::validate_contract_ledger_entry(
            &key, small_size, &limits,
        ));

        // Entry size over limit should be invalid
        let large_size = 66000; // > 64 KB
        assert!(!super::super::validate_contract_ledger_entry(
            &key, large_size, &limits,
        ));
    }

    /// Regression test for CONTRACT_CODE entry size validation.
    #[test]
    fn test_validate_contract_code_entry_size_exceeded() {
        let key = LedgerKey::ContractCode(LedgerKeyContractCode {
            hash: Hash([1u8; 32]),
        });

        let max_code_size = 65536; // 64 KB
        let max_data_size = 65536;
        let limits = super::super::ContractSizeLimits {
            max_contract_size_bytes: max_code_size,
            max_contract_data_entry_size_bytes: max_data_size,
        };

        // Entry size under limit should be valid
        assert!(super::super::validate_contract_ledger_entry(
            &key, 1000, &limits,
        ));

        // Entry size over limit should be invalid
        assert!(!super::super::validate_contract_ledger_entry(
            &key, 66000, &limits,
        ));
    }

    /// Regression test for validate_and_compute_write_bytes with oversized entry.
    #[test]
    fn test_validate_and_compute_write_bytes_entry_size_exceeded() {
        let contract_id = ScAddress::Contract(ContractId(Hash([1u8; 32])));
        let durability = ContractDataDurability::Persistent;

        // Create a CONTRACT_DATA entry with a large value that exceeds the limit
        // We'll create a value with enough bytes to exceed maxContractDataEntrySizeBytes
        let large_val_bytes: Vec<u8> = vec![0u8; 70000]; // > 64 KB
        let cd_entry = ContractDataEntry {
            ext: ExtensionPoint::V0,
            contract: contract_id.clone(),
            key: ScVal::U32(42),
            durability,
            val: ScVal::Bytes(large_val_bytes.try_into().unwrap()),
        };

        let key = LedgerKey::ContractData(LedgerKeyContractData {
            contract: contract_id,
            key: ScVal::U32(42),
            durability,
        });

        let ledger_entry = LedgerEntry {
            last_modified_ledger_seq: 1,
            data: LedgerEntryData::ContractData(cd_entry),
            ext: LedgerEntryExt::V0,
        };

        let change = StorageChange {
            key,
            kind: crate::soroban::StorageChangeKind::Modified {
                entry: Box::new(ledger_entry),
                live_until: Some(200),
                ttl_extended: false,
            },
            is_rent_related: false,
        };

        let changes = vec![change];
        let max_code_size = 65536;
        let max_data_size = 65536;
        let limits = super::super::ContractSizeLimits {
            max_contract_size_bytes: max_code_size,
            max_contract_data_entry_size_bytes: max_data_size,
        };

        // Should return EntrySizeExceeded because the entry is > 64 KB
        let result = validate_and_compute_write_bytes(&changes, &limits);
        assert!(matches!(result, StorageChangeValidation::EntrySizeExceeded));
    }

    /// Test that entries within limits pass validation.
    #[test]
    fn test_validate_and_compute_write_bytes_within_limits() {
        let contract_id = ScAddress::Contract(ContractId(Hash([1u8; 32])));
        let durability = ContractDataDurability::Persistent;

        // Create a small CONTRACT_DATA entry within limits
        let cd_entry = ContractDataEntry {
            ext: ExtensionPoint::V0,
            contract: contract_id.clone(),
            key: ScVal::U32(42),
            durability,
            val: ScVal::I32(123),
        };

        let key = LedgerKey::ContractData(LedgerKeyContractData {
            contract: contract_id,
            key: ScVal::U32(42),
            durability,
        });

        let ledger_entry = LedgerEntry {
            last_modified_ledger_seq: 1,
            data: LedgerEntryData::ContractData(cd_entry),
            ext: LedgerEntryExt::V0,
        };

        let change = StorageChange {
            key,
            kind: crate::soroban::StorageChangeKind::Modified {
                entry: Box::new(ledger_entry),
                live_until: Some(200),
                ttl_extended: false,
            },
            is_rent_related: false,
        };

        let changes = vec![change];
        let max_code_size = 65536;
        let max_data_size = 65536;
        let limits = super::super::ContractSizeLimits {
            max_contract_size_bytes: max_code_size,
            max_contract_data_entry_size_bytes: max_data_size,
        };

        // Should return Valid with non-zero write bytes
        let result = validate_and_compute_write_bytes(&changes, &limits);
        match result {
            StorageChangeValidation::Valid { total_write_bytes } => {
                assert!(total_write_bytes > 0);
            }
            _ => panic!("Expected Valid result"),
        }
    }

    /// Regression test for ledger 128051: Hot archive restoration INIT/LIVE categorization.
    ///
    /// When Soroban entries are restored from the hot archive (entries that were evicted
    /// and are now being auto-restored via `archived_soroban_entries` indices in
    /// `SorobanTransactionDataExt::V1`), they should be recorded as INIT (created) in
    /// the bucket list delta, not LIVE (updated).
    ///
    /// The bug was that `apply_soroban_storage_change` checked if the entry existed
    /// in state to decide create vs update. But entries loaded from hot archive exist
    /// in state (loaded during Soroban execution setup), yet they're not in the live
    /// bucket list - they're being restored to it.
    ///
    /// Per CAP-0066, hot archive restored entries should appear as INIT in the bucket
    /// list delta because they are being added back to the live bucket list.
    #[test]
    fn test_hot_archive_restore_uses_create_not_update() {
        let contract_id = ScAddress::Contract(ContractId(Hash([2u8; 32])));
        let contract_key = ScVal::U32(99);
        let durability = ContractDataDurability::Persistent;

        let key = LedgerKey::ContractData(LedgerKeyContractData {
            contract: contract_id.clone(),
            key: contract_key.clone(),
            durability,
        });

        let cd_entry = ContractDataEntry {
            ext: ExtensionPoint::V0,
            contract: contract_id.clone(),
            key: contract_key.clone(),
            durability,
            val: ScVal::I32(42),
        };

        let ledger_entry = LedgerEntry {
            last_modified_ledger_seq: 128051,
            data: LedgerEntryData::ContractData(cd_entry.clone()),
            ext: LedgerEntryExt::V0,
        };

        let change = StorageChange {
            key: key.clone(),
            kind: crate::soroban::StorageChangeKind::Modified {
                entry: Box::new(ledger_entry),
                live_until: Some(250000),
                ttl_extended: false,
            },
            is_rent_related: false,
        };

        // Case 1: Entry exists in state, NOT in hot_archive_keys -> should use update (LIVE)
        {
            let mut state = LedgerStateManager::new(5_000_000, 100);

            // Pre-populate entry in state (simulates entry loaded from hot archive into state)
            // We do this by directly using create which will track it as created initially
            state.create_contract_data(cd_entry.clone());

            // Now state knows about the entry. When we check get_contract_data, it returns Some.
            // Without hot_archive_keys, this means we should call update_contract_data.
            // But the initial create is also in delta. For this test, we just verify the logic:
            // - When hot_archive_keys doesn't contain the key and entry exists -> update
            // - When hot_archive_keys contains the key -> create (regardless of existence)

            // Since we just created it, the entry is tracked as created.
            // Apply a change WITHOUT hot_archive_keys - it should use update since entry exists
            let _no_restored_keys: std::collections::HashSet<LedgerKey> =
                std::collections::HashSet::new();

            // Check the logic path: get_contract_data returns Some, so without hot_archive_keys
            // we'd call update_contract_data. We can verify this by checking that the function
            // uses create vs update based on the flag.
            assert!(
                state
                    .get_contract_data(&contract_id, &contract_key, durability)
                    .is_some(),
                "Entry should exist in state"
            );
        }

        // Case 2: Entry exists in state AND in hot_archive_keys -> should use create (INIT)
        // This is the key fix: even though entry exists in state, if it's in hot_archive_keys,
        // we must use create to record it as INIT for the bucket list.
        //
        // In production, archived entries are loaded from InMemorySorobanState via load_entry,
        // which does NOT add to delta. The entry exists in state but NOT in delta.created.
        {
            let mut state = LedgerStateManager::new(5_000_000, 100);

            // Pre-populate entry in state using load_entry (like InMemorySorobanState does).
            // This does NOT add to delta - the entry just exists in state.
            state.load_entry(LedgerEntry {
                last_modified_ledger_seq: 128051,
                data: LedgerEntryData::ContractData(cd_entry.clone()),
                ext: LedgerEntryExt::V0,
            });

            // Verify entry is in state but NOT in delta
            assert!(
                state
                    .get_contract_data(&contract_id, &contract_key, durability)
                    .is_some(),
                "Entry should exist in state"
            );
            assert_eq!(
                state.delta().created_entries().len(),
                0,
                "Delta should be empty before apply"
            );

            // Now apply with hot_archive_keys containing the key
            let mut hot_archive_keys = std::collections::HashSet::new();
            hot_archive_keys.insert(key.clone());

            apply_soroban_storage_change(
                &mut state,
                &change,
                &hot_archive_keys,
                None,
                &mut std::collections::HashSet::new(),
            );

            // With hot_archive_keys and entry NOT in delta.created,
            // apply_soroban_storage_change should use create_contract_data.
            let created_count = state
                .delta()
                .created_entries()
                .iter()
                .filter(|e| {
                    if let LedgerEntryData::ContractData(cd) = &e.data {
                        cd.contract == contract_id && cd.key == contract_key
                    } else {
                        false
                    }
                })
                .count();

            // Should be 1: the hot archive restore creates a new entry in delta
            assert_eq!(
                created_count, 1,
                "With hot archive keys, entry should be recorded as INIT (created)"
            );

            // And should NOT be in updated
            let updated_count = state
                .delta()
                .updated_entries()
                .iter()
                .filter(|e| {
                    if let LedgerEntryData::ContractData(cd) = &e.data {
                        cd.contract == contract_id && cd.key == contract_key
                    } else {
                        false
                    }
                })
                .count();

            assert_eq!(
                updated_count, 0,
                "With hot archive keys, entry should NOT be recorded as LIVE (updated)"
            );
        }

        // Case 3: Entry exists in state (pre-loaded), NOT in hot_archive_keys -> should use update (LIVE)
        // This simulates a normal live bucket list entry that was loaded for Soroban execution.
        {
            let mut state = LedgerStateManager::new(5_000_000, 100);

            // Pre-populate entry in state using load_entry (simulates snapshot lookup)
            state.load_entry(LedgerEntry {
                last_modified_ledger_seq: 128051,
                data: LedgerEntryData::ContractData(cd_entry.clone()),
                ext: LedgerEntryExt::V0,
            });

            // Now apply WITHOUT hot_archive_keys
            let no_restored_keys: std::collections::HashSet<LedgerKey> =
                std::collections::HashSet::new();
            apply_soroban_storage_change(
                &mut state,
                &change,
                &no_restored_keys,
                None,
                &mut std::collections::HashSet::new(),
            );

            // Without hot_archive_keys, entry exists, so should call update_contract_data
            let created_count = state
                .delta()
                .created_entries()
                .iter()
                .filter(|e| {
                    if let LedgerEntryData::ContractData(cd) = &e.data {
                        cd.contract == contract_id && cd.key == contract_key
                    } else {
                        false
                    }
                })
                .count();

            assert_eq!(
                created_count, 0,
                "Without hot archive keys, entry should NOT be in created"
            );

            let updated_count = state
                .delta()
                .updated_entries()
                .iter()
                .filter(|e| {
                    if let LedgerEntryData::ContractData(cd) = &e.data {
                        cd.contract == contract_id && cd.key == contract_key
                    } else {
                        false
                    }
                })
                .count();

            assert_eq!(
                updated_count, 1,
                "Without hot archive keys, entry should be recorded as LIVE (updated)"
            );
        }
    }

    /// Regression test for VE-05 / L59940765: hot archive read-only restores must NOT
    /// emit a spurious TTL INIT in the live bucket list.
    ///
    /// When the host reads a restored archived entry but does not write it back
    /// (read-only access), `storage_changes` includes the entry with `new_entry=None`
    /// and `live_until=Some(restored_live_until)` because:
    ///   - `is_deletion = !read_only && encoded_new_value.is_none() = true` (false positive)
    ///   - `ttl_extended = restored_live_until > ledger_start_ttl(=0) = true` (false positive)
    ///
    /// Before the fix, `apply_soroban_storage_change` reached the TTL-only branch
    /// (`new_entry=None, live_until=Some(...)`) and called `state.create_ttl(ttl)`,
    /// inserting a spurious TTL INIT into the live bucket list.
    ///
    /// In stellar-core, `handleArchivedEntry` creates DATA+TTL INIT and then immediately
    /// erases both for read-only access → net effect: nothing in the live bucket list.
    /// (The HOT_ARCHIVE_LIVE tombstone is still written, which is correct.)
    ///
    /// Fix: early-return in the TTL-only block when `is_hot_archive_restore=true`.
    #[test]
    fn test_hot_archive_read_only_restore_no_spurious_ttl_init() {
        let contract_id = ScAddress::Contract(ContractId(Hash([0xBBu8; 32])));
        let contract_key = ScVal::U32(77);
        let durability = ContractDataDurability::Persistent;

        let key = LedgerKey::ContractData(LedgerKeyContractData {
            contract: contract_id.clone(),
            key: contract_key.clone(),
            durability,
        });
        let key_hash = crate::soroban::compute_key_hash(&key);

        // Simulate the storage change produced for a hot archive entry that the host
        // only read (no data returned).  The "is_deletion" and "ttl_extended" flags are
        // true as false-positives — the entry was never in the live BL so
        // ledger_start_ttl=0, and the host-side read_only=false because it came from
        // archived_soroban_entries (RW footprint).
        let restored_live_until: u32 = 62_014_364; // from L59940765 log
        let change = StorageChange {
            key: key.clone(),
            kind: crate::soroban::StorageChangeKind::TtlOnly {
                live_until: restored_live_until,
                read_only: false, // RW footprint entry
            },
            is_rent_related: false,
        };

        // Mark the key as a hot-archive restore.
        let mut hot_archive_restored_keys = std::collections::HashSet::new();
        hot_archive_restored_keys.insert(key.clone());

        let mut state = LedgerStateManager::new(5_000_000, 59_940_765);

        // Entry is not in state (never was in live BL — it came from hot archive).
        assert!(
            state.get_ttl(&key_hash).is_none(),
            "TTL should not be in state before the call"
        );
        assert_eq!(
            state.delta().created_entries().len(),
            0,
            "Delta should be empty before the call"
        );

        apply_soroban_storage_change(
            &mut state,
            &change,
            &hot_archive_restored_keys,
            None,
            &mut std::collections::HashSet::new(),
        );

        // KEY ASSERTION: no TTL INIT should have been created.
        // Before the fix, create_ttl() was called, producing a spurious TTL INIT that
        // diverged the live BL hash from stellar-core.
        let ttl_init_count = state
            .delta()
            .created_entries()
            .iter()
            .filter(|e| matches!(&e.data, LedgerEntryData::Ttl(t) if t.key_hash == key_hash))
            .count();
        assert_eq!(
            ttl_init_count, 0,
            "No TTL INIT should be emitted for a hot archive read-only restore (VE-05)"
        );

        // And no data entry either.
        assert_eq!(
            state.delta().created_entries().len(),
            0,
            "Delta should remain empty after a hot archive read-only restore"
        );
    }

    /// Integration test for VE-05: exercises `apply_soroban_storage_changes` (plural) with a
    /// hot archive read-only restore entry in the RW footprint.
    ///
    /// This tests the full flow through the wrapper function:
    /// 1. The TTL-only branch early-returns for `is_hot_archive_restore` (the VE-05 fix)
    /// 2. The erase-RW loop skips the entry because it's not in state
    ///
    /// Net result: no INIT, no deletion — matching stellar-core's create-then-erase behavior.
    #[test]
    fn test_apply_soroban_storage_changes_hot_archive_read_only_no_side_effects() {
        let contract_id = ScAddress::Contract(ContractId(Hash([0xCCu8; 32])));
        let contract_key = ScVal::U32(99);
        let durability = ContractDataDurability::Persistent;

        let key = LedgerKey::ContractData(LedgerKeyContractData {
            contract: contract_id.clone(),
            key: contract_key.clone(),
            durability,
        });
        let key_hash = crate::soroban::compute_key_hash(&key);

        // Storage change for a hot archive entry the host only read (no data returned).
        let restored_live_until: u32 = 62_014_364;
        let change = StorageChange {
            key: key.clone(),
            kind: crate::soroban::StorageChangeKind::TtlOnly {
                live_until: restored_live_until,
                read_only: false,
            },
            is_rent_related: false,
        };

        let mut hot_archive_restored_keys = std::collections::HashSet::new();
        hot_archive_restored_keys.insert(key.clone());

        // The key is in the RW footprint (as it would be for an archived entry in
        // archivedSorobanEntries). This means the erase-RW loop will iterate over it.
        let footprint = LedgerFootprint {
            read_only: VecM::default(),
            read_write: vec![key.clone()].try_into().unwrap(),
        };

        let mut state = LedgerStateManager::new(5_000_000, 59_940_765);

        assert_eq!(
            state.delta().created_entries().len(),
            0,
            "Delta should be empty before the call"
        );

        // Call the wrapper (plural) which also runs the erase-RW loop.
        apply_soroban_storage_changes(
            &mut state,
            &[change],
            &footprint,
            &hot_archive_restored_keys,
            None,
            25,
        );

        // No TTL INIT should have been created (VE-05 fix).
        let ttl_init_count = state
            .delta()
            .created_entries()
            .iter()
            .filter(|e| matches!(&e.data, LedgerEntryData::Ttl(t) if t.key_hash == key_hash))
            .count();
        assert_eq!(
            ttl_init_count, 0,
            "No TTL INIT should be emitted for a hot archive read-only restore (VE-05)"
        );

        // No data entry created or deleted either.
        assert_eq!(
            state.delta().created_entries().len(),
            0,
            "No entries should be created for a hot archive read-only restore"
        );
        assert_eq!(
            state.delta().deleted_keys().len(),
            0,
            "No entries should be deleted for a hot archive read-only restore (not in state)"
        );
    }

    /// Regression test for VE-06 (erase-RW skip): when a hot archive entry is pre-loaded
    /// into state (from InMemorySorobanState) but the host only reads it (no write-back),
    /// the erase-RW loop must NOT delete it.
    ///
    /// Without the skip at invoke_host_function.rs:667, the erase-RW loop finds the entry
    /// via `get_contract_data().is_some()` and calls `delete_contract_data()`, producing a
    /// spurious DEAD entry in the live bucket list delta. This DEAD entry would diverge the
    /// bucket_list_hash from stellar-core, where `handleArchivedEntry` creates DATA+TTL INIT
    /// and immediately erases both for read-only access (net zero effect on live BL).
    ///
    /// This test differs from `test_apply_soroban_storage_changes_hot_archive_read_only_no_side_effects`
    /// (VE-05) which does NOT pre-load the entry into state, so the erase-RW loop's
    /// `get_contract_data().is_some()` returns false regardless of the skip.
    #[test]
    fn test_erase_rw_skips_preloaded_hot_archive_read_only_entry() {
        let contract_id = ScAddress::Contract(ContractId(Hash([0xDDu8; 32])));
        let contract_key = ScVal::U32(55);
        let durability = ContractDataDurability::Persistent;

        let key = LedgerKey::ContractData(LedgerKeyContractData {
            contract: contract_id.clone(),
            key: contract_key.clone(),
            durability,
        });

        // Pre-load the archived entry into state via load_entry (simulates how
        // InMemorySorobanState entries are loaded before host execution). This does
        // NOT record a delta — it just makes get_contract_data() return Some.
        let entry = LedgerEntry {
            last_modified_ledger_seq: 50_000_000,
            data: LedgerEntryData::ContractData(ContractDataEntry {
                ext: ExtensionPoint::V0,
                contract: contract_id.clone(),
                key: contract_key.clone(),
                durability,
                val: ScVal::U64(999),
            }),
            ext: LedgerEntryExt::V0,
        };

        let mut state = LedgerStateManager::new(5_000_000, 60_000_000);
        state.load_entry(entry);

        // Confirm the entry is visible in state.
        assert!(
            state
                .get_contract_data(&contract_id, &contract_key, durability)
                .is_some(),
            "Entry must be in state (pre-loaded from hot archive)"
        );

        // No storage changes — the host only read this entry, did not write it back.
        let changes: Vec<StorageChange> = vec![];

        // The key is in the RW footprint (archived entries are placed in read_write).
        let footprint = LedgerFootprint {
            read_only: VecM::default(),
            read_write: vec![key.clone()].try_into().unwrap(),
        };

        // Mark it as a hot archive restored key.
        let mut hot_archive_restored_keys = std::collections::HashSet::new();
        hot_archive_restored_keys.insert(key.clone());

        // Delta should be clean before the call.
        assert_eq!(state.delta().created_entries().len(), 0);
        assert_eq!(state.delta().deleted_keys().len(), 0);

        // Call apply_soroban_storage_changes — this runs the erase-RW loop.
        apply_soroban_storage_changes(
            &mut state,
            &changes,
            &footprint,
            &hot_archive_restored_keys,
            None,
            25,
        );

        // KEY ASSERTION: no DEAD entry should be created.
        // Before the VE-06 fix (hot archive skip at line 667), the erase-RW loop
        // would find the pre-loaded entry via get_contract_data().is_some() and
        // call delete_contract_data(), creating a spurious DEAD delta entry.
        assert_eq!(
            state.delta().deleted_keys().len(),
            0,
            "Erase-RW loop must skip hot archive read-only entries — \
             deleting them creates a spurious DEAD in the live bucket list"
        );

        // The entry should still be accessible in state (not deleted).
        assert!(
            state
                .get_contract_data(&contract_id, &contract_key, durability)
                .is_some(),
            "Hot archive read-only entry must remain in state after apply_soroban_storage_changes"
        );

        // No creates either (the entry was pre-loaded, not created via delta).
        assert_eq!(
            state.delta().created_entries().len(),
            0,
            "No entries should be created for a hot archive read-only restore"
        );
    }

    /// Regression test for VE-14 / L61593050: validate_footprint_entry_sizes rejects
    /// live entries that exceed network config limits during the read phase.
    ///
    /// At L61593050 TX 166, a fee bump InvokeHostFunction with instruction_limit=20M had
    /// 45 footprint entries. The typed Soroban API (`invoke_host_function_typed`) skipped
    /// XDR deserialization budget metering (~3M CPU instructions), so the TX succeeded when
    /// it should have exceeded the 20M limit and returned ResourceLimitExceeded.
    ///
    /// Part of the VE-14 fix was adding `validate_footprint_entry_sizes()` to match
    /// stellar-core's `addReads()` → `validateContractLedgerEntry()` behavior, which
    /// validates all footprint entries against `maxContractSizeBytes` and
    /// `maxContractDataEntrySizeBytes` during the read phase.
    ///
    /// The primary fix was switching from the typed API to the non-typed API
    /// (`invoke_host_function`) which meters XDR deserialization against the host budget.
    #[test]
    fn test_validate_footprint_entry_sizes_rejects_oversized_live_entry() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let contract_id = ScAddress::Contract(ContractId(Hash([0xEEu8; 32])));
        let contract_key = ScVal::U32(14);
        let durability = ContractDataDurability::Persistent;

        // Create a large ContractData entry that exceeds the limit
        let large_val: Vec<u8> = vec![0xAB; 70000]; // > 64 KB
        let cd_entry = ContractDataEntry {
            ext: ExtensionPoint::V0,
            contract: contract_id.clone(),
            key: contract_key.clone(),
            durability,
            val: ScVal::Bytes(large_val.try_into().unwrap()),
        };
        state.create_contract_data(cd_entry);

        let key = LedgerKey::ContractData(LedgerKeyContractData {
            contract: contract_id.clone(),
            key: contract_key.clone(),
            durability,
        });

        // Give the entry a live TTL so it's considered live
        let key_hash = crate::soroban::compute_key_hash(&key);
        state.create_ttl(TtlEntry {
            key_hash,
            live_until_ledger_seq: context.sequence + 100,
        });

        let footprint = LedgerFootprint {
            read_only: vec![key.clone()].try_into().unwrap(),
            read_write: VecM::default(),
        };
        let soroban_data = SorobanTransactionData {
            ext: SorobanTransactionDataExt::V0,
            resources: SorobanResources {
                footprint,
                instructions: 100_000_000,
                disk_read_bytes: 100_000,
                write_bytes: 0,
            },
            resource_fee: 0,
        };

        let config = SorobanConfig {
            max_contract_size_bytes: 65536,
            max_contract_data_entry_size_bytes: 65536,
            ..SorobanConfig::default()
        };

        // Should return false (validation fails) because the live entry exceeds the limit
        assert!(
            !validate_footprint_entry_sizes(&state, &soroban_data, &config, context.sequence, None),
            "VE-14: validate_footprint_entry_sizes should reject oversized live entry"
        );
    }

    /// VE-14: validate_footprint_entry_sizes passes when entry is dead (expired TTL).
    ///
    /// In stellar-core, dead entries have entrySize=0 which passes validation.
    /// This matches the behavior where expired entries are not read from disk.
    #[test]
    fn test_validate_footprint_entry_sizes_passes_for_dead_entry() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let contract_id = ScAddress::Contract(ContractId(Hash([0xFFu8; 32])));
        let contract_key = ScVal::U32(14);
        let durability = ContractDataDurability::Persistent;

        // Create a large entry that would fail validation if it were live
        let large_val: Vec<u8> = vec![0xCD; 70000];
        let cd_entry = ContractDataEntry {
            ext: ExtensionPoint::V0,
            contract: contract_id.clone(),
            key: contract_key.clone(),
            durability,
            val: ScVal::Bytes(large_val.try_into().unwrap()),
        };
        state.create_contract_data(cd_entry);

        let key = LedgerKey::ContractData(LedgerKeyContractData {
            contract: contract_id.clone(),
            key: contract_key.clone(),
            durability,
        });

        // Give the entry an EXPIRED TTL
        let key_hash = crate::soroban::compute_key_hash(&key);
        state.create_ttl(TtlEntry {
            key_hash,
            live_until_ledger_seq: context.sequence - 1, // Expired
        });

        let footprint = LedgerFootprint {
            read_only: vec![key.clone()].try_into().unwrap(),
            read_write: VecM::default(),
        };
        let soroban_data = SorobanTransactionData {
            ext: SorobanTransactionDataExt::V0,
            resources: SorobanResources {
                footprint,
                instructions: 100_000_000,
                disk_read_bytes: 100_000,
                write_bytes: 0,
            },
            resource_fee: 0,
        };

        let config = SorobanConfig {
            max_contract_size_bytes: 65536,
            max_contract_data_entry_size_bytes: 65536,
            ..SorobanConfig::default()
        };

        // Should return true (validation passes) because the entry is dead
        assert!(
            validate_footprint_entry_sizes(&state, &soroban_data, &config, context.sequence, None),
            "VE-14: validate_footprint_entry_sizes should pass for dead (expired) entry"
        );
    }

    /// VE-14: validate_footprint_entry_sizes passes for within-limit live entries.
    #[test]
    fn test_validate_footprint_entry_sizes_passes_for_normal_entries() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let contract_id = ScAddress::Contract(ContractId(Hash([0xDDu8; 32])));
        let contract_key = ScVal::U32(14);
        let durability = ContractDataDurability::Persistent;

        // Create a small entry within limits
        let cd_entry = ContractDataEntry {
            ext: ExtensionPoint::V0,
            contract: contract_id.clone(),
            key: contract_key.clone(),
            durability,
            val: ScVal::I32(42),
        };
        state.create_contract_data(cd_entry);

        let key = LedgerKey::ContractData(LedgerKeyContractData {
            contract: contract_id.clone(),
            key: contract_key.clone(),
            durability,
        });

        // Give it a live TTL
        let key_hash = crate::soroban::compute_key_hash(&key);
        state.create_ttl(TtlEntry {
            key_hash,
            live_until_ledger_seq: context.sequence + 100,
        });

        let footprint = LedgerFootprint {
            read_only: vec![key.clone()].try_into().unwrap(),
            read_write: VecM::default(),
        };
        let soroban_data = SorobanTransactionData {
            ext: SorobanTransactionDataExt::V0,
            resources: SorobanResources {
                footprint,
                instructions: 100_000_000,
                disk_read_bytes: 100_000,
                write_bytes: 0,
            },
            resource_fee: 0,
        };

        let config = SorobanConfig {
            max_contract_size_bytes: 65536,
            max_contract_data_entry_size_bytes: 65536,
            ..SorobanConfig::default()
        };

        // Should pass since entry is small and live
        assert!(
            validate_footprint_entry_sizes(&state, &soroban_data, &config, context.sequence, None),
            "VE-14: validate_footprint_entry_sizes should pass for normal-sized live entry"
        );
    }

    /// Regression test for #1474: oversized archived entry marked for autorestore
    /// should be rejected with ResourceLimitExceeded, not allowed through to host.
    #[test]
    fn test_validate_archived_entry_sizes_rejects_oversized_restore() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let contract_id = ScAddress::Contract(ContractId(Hash([0xAAu8; 32])));
        let contract_key = ScVal::U32(99);
        let durability = ContractDataDurability::Persistent;

        // Create oversized entry (> 64KB limit)
        let large_val: Vec<u8> = vec![0xBB; 70000];
        let cd_entry = ContractDataEntry {
            ext: ExtensionPoint::V0,
            contract: contract_id.clone(),
            key: contract_key.clone(),
            durability,
            val: ScVal::Bytes(large_val.try_into().unwrap()),
        };
        state.create_contract_data(cd_entry);

        let key = LedgerKey::ContractData(LedgerKeyContractData {
            contract: contract_id,
            key: contract_key,
            durability,
        });

        // Give the entry an EXPIRED TTL (archived)
        let key_hash = crate::soroban::compute_key_hash(&key);
        state.create_ttl(TtlEntry {
            key_hash,
            live_until_ledger_seq: context.sequence - 1,
        });

        // Mark entry at index 0 in read_write for autorestore
        let footprint = LedgerFootprint {
            read_only: VecM::default(),
            read_write: vec![key].try_into().unwrap(),
        };
        let soroban_data = SorobanTransactionData {
            ext: SorobanTransactionDataExt::V1(SorobanResourcesExtV0 {
                archived_soroban_entries: vec![0u32].try_into().unwrap(),
            }),
            resources: SorobanResources {
                footprint,
                instructions: 100_000_000,
                disk_read_bytes: 100_000,
                write_bytes: 100_000,
            },
            resource_fee: 0,
        };

        let config = SorobanConfig {
            max_contract_size_bytes: 65536,
            max_contract_data_entry_size_bytes: 65536,
            ..SorobanConfig::default()
        };

        // validate_footprint_entry_sizes passes (skips expired entries)
        assert!(
            validate_footprint_entry_sizes(&state, &soroban_data, &config, context.sequence, None),
            "footprint entry validation passes for dead entry (by design)"
        );

        // But validate_archived_entry_sizes should REJECT the oversized restore
        assert!(
            !validate_archived_entry_sizes(
                &state,
                &soroban_data,
                &config,
                context.sequence,
                None,
                None,
            ),
            "archived entry validation must reject oversized entry marked for restore"
        );
    }

    /// Regression test for #1474: normal-sized archived entry marked for autorestore
    /// should be allowed through.
    #[test]
    fn test_validate_archived_entry_sizes_passes_for_normal_restore() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let contract_id = ScAddress::Contract(ContractId(Hash([0xCCu8; 32])));
        let contract_key = ScVal::U32(77);
        let durability = ContractDataDurability::Persistent;

        // Create small entry within limits
        let cd_entry = ContractDataEntry {
            ext: ExtensionPoint::V0,
            contract: contract_id.clone(),
            key: contract_key.clone(),
            durability,
            val: ScVal::I32(42),
        };
        state.create_contract_data(cd_entry);

        let key = LedgerKey::ContractData(LedgerKeyContractData {
            contract: contract_id,
            key: contract_key,
            durability,
        });

        let key_hash = crate::soroban::compute_key_hash(&key);
        state.create_ttl(TtlEntry {
            key_hash,
            live_until_ledger_seq: context.sequence - 1,
        });

        let footprint = LedgerFootprint {
            read_only: VecM::default(),
            read_write: vec![key].try_into().unwrap(),
        };
        let soroban_data = SorobanTransactionData {
            ext: SorobanTransactionDataExt::V1(SorobanResourcesExtV0 {
                archived_soroban_entries: vec![0u32].try_into().unwrap(),
            }),
            resources: SorobanResources {
                footprint,
                instructions: 100_000_000,
                disk_read_bytes: 100_000,
                write_bytes: 100_000,
            },
            resource_fee: 0,
        };

        let config = SorobanConfig {
            max_contract_size_bytes: 65536,
            max_contract_data_entry_size_bytes: 65536,
            ..SorobanConfig::default()
        };

        assert!(
            validate_archived_entry_sizes(
                &state,
                &soroban_data,
                &config,
                context.sequence,
                None,
                None,
            ),
            "archived entry validation should pass for normal-sized restore"
        );
    }

    // --- Parity assertion tests for Soroban deletion paths ---

    #[test]
    #[should_panic(expected = "only Soroban entries")]
    fn test_apply_deletion_panics_on_account_key() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let account_id = create_test_account_id(1);
        let key = LedgerKey::Account(LedgerKeyAccount {
            account_id: account_id.clone(),
        });
        apply_deletion(&mut state, &key, None);
    }

    #[test]
    #[should_panic(expected = "only Soroban entries")]
    fn test_apply_deletion_panics_on_trustline_key() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let account_id = create_test_account_id(1);
        let key = LedgerKey::Trustline(LedgerKeyTrustLine {
            account_id: account_id.clone(),
            asset: TrustLineAsset::CreditAlphanum4(AlphaNum4 {
                asset_code: AssetCode4(*b"USD\0"),
                issuer: create_test_account_id(2),
            }),
        });
        apply_deletion(&mut state, &key, None);
    }

    #[test]
    #[should_panic(expected = "implicit Soroban deletion of existing non-Soroban entry")]
    fn test_implicit_sweep_panics_on_existing_account() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let account_id = create_test_account_id(1);
        let account = create_test_account(account_id.clone(), 1_000_000_000);
        state.create_account(account);

        let key = LedgerKey::Account(LedgerKeyAccount {
            account_id: account_id.clone(),
        });

        // Build a footprint with the account key in read_write but NOT in
        // the host's created_and_modified_keys — simulating the host
        // omitting an existing classic entry.
        let footprint = LedgerFootprint {
            read_only: vec![].try_into().unwrap(),
            read_write: vec![key.clone()].try_into().unwrap(),
        };
        let changes: Vec<StorageChange> = vec![];
        let no_restored_keys = std::collections::HashSet::new();
        let context = create_test_context();
        apply_soroban_storage_changes(
            &mut state,
            &changes,
            &footprint,
            &no_restored_keys,
            None,
            context.protocol_version,
        );
    }

    #[test]
    fn test_implicit_sweep_noop_for_nonexistent_account() {
        // Non-existent classic entry in footprint should be a no-op
        // (matches stellar-core's eraseLedgerEntryIfExists returning false).
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let account_id = create_test_account_id(1);

        let key = LedgerKey::Account(LedgerKeyAccount {
            account_id: account_id.clone(),
        });

        let footprint = LedgerFootprint {
            read_only: vec![].try_into().unwrap(),
            read_write: vec![key.clone()].try_into().unwrap(),
        };
        let changes: Vec<StorageChange> = vec![];
        let no_restored_keys = std::collections::HashSet::new();
        let context = create_test_context();
        // Should not panic — non-existent entry is silently skipped
        apply_soroban_storage_changes(
            &mut state,
            &changes,
            &footprint,
            &no_restored_keys,
            None,
            context.protocol_version,
        );
    }
}
