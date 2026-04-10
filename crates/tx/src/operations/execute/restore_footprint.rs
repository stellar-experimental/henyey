//! RestoreFootprint operation execution.
//!
//! This module implements the execution logic for the RestoreFootprint operation,
//! which restores archived Soroban contract data entries.

use stellar_xdr::curr::{
    AccountId, LedgerEntry, LedgerEntryData, LedgerKey, Limits, OperationResult, OperationResultTr,
    RestoreFootprintOp, RestoreFootprintResult, RestoreFootprintResultCode, SorobanTransactionData,
    TtlEntry, WriteXdr,
};

use crate::state::LedgerStateManager;
use crate::validation::LedgerContext;
use crate::Result;
use henyey_common::LedgerSeq;

/// Entry to restore from the hot archive.
pub struct HotArchiveRestoreEntry {
    /// The key of the entry to restore.
    pub key: LedgerKey,
    /// The entry value from the hot archive.
    pub entry: LedgerEntry,
}

/// Soroban inputs needed to restore archived entries.
pub struct RestoreFootprintResources<'a> {
    /// Soroban transaction data containing the restore footprint.
    pub soroban_data: Option<&'a SorobanTransactionData>,
    /// Minimum persistent entry TTL from Soroban config.
    pub min_persistent_entry_ttl: u32,
    /// Entries loaded from the hot archive for this operation.
    pub hot_archive_restores: &'a [HotArchiveRestoreEntry],
    /// Optional TTL key cache for hashing restored entries.
    pub ttl_key_cache: Option<&'a crate::soroban::TtlKeyCache>,
    /// Contract size limits from SorobanConfig.
    pub size_limits: Option<super::extend_footprint_ttl::ContractSizeLimits>,
}

/// Execute a RestoreFootprint operation.
///
/// This operation restores archived entries that have expired TTLs,
/// making them live again with a new TTL.
///
/// # Arguments
///
/// * `op` - The RestoreFootprint operation data
/// * `source` - The source account ID
/// * `state` - The ledger state manager
/// * `context` - The ledger context
/// * `resources` - Bundled restore-specific resources (soroban data, min TTL, hot archive entries, TTL cache)
///
/// # Returns
///
/// Returns the operation result indicating success or a specific failure reason.
pub(crate) fn execute_restore_footprint(
    _op: &RestoreFootprintOp,
    _source: &AccountId,
    state: &mut LedgerStateManager,
    context: &LedgerContext,
    resources: RestoreFootprintResources<'_>,
) -> Result<OperationResult> {
    // Get the footprint from Soroban transaction data
    let footprint = match resources.soroban_data {
        Some(data) => &data.resources.footprint,
        None => {
            return Ok(make_result(RestoreFootprintResultCode::Malformed));
        }
    };

    if !footprint.read_only.is_empty() {
        return Ok(make_result(RestoreFootprintResultCode::Malformed));
    }

    for key in footprint.read_write.iter() {
        if !is_persistent_entry(key) {
            return Ok(make_result(RestoreFootprintResultCode::Malformed));
        }
    }

    // Calculate the new TTL for restored entries
    // Per stellar-core RestoreFootprintOpFrame.cpp line 115-116:
    //   restoredLiveUntilLedger = ledgerSeq + archivalSettings.minPersistentTTL - 1
    let current_ledger = context.sequence;
    let new_ttl = current_ledger
        .saturating_add(resources.min_persistent_entry_ttl)
        .saturating_sub(1);

    // Resource limit tracking (stellar-core: RestoreFootprintApplyHelper::apply)
    let soroban_data = resources.soroban_data.unwrap(); // safe: checked above
    let disk_read_bytes_limit = soroban_data.resources.disk_read_bytes;
    let write_bytes_limit = soroban_data.resources.write_bytes;
    let mut accumulated_read_bytes: u32 = 0;
    let mut accumulated_write_bytes: u32 = 0;

    // First, restore hot archive entries to state.
    // These entries don't exist in the live bucket list, so we need to add them.
    // SECURITY: hot_archive_restores populated by ledger execution layer from local hot archive, not external tx input
    for restore in resources.hot_archive_restores {
        tracing::debug!(
            ?restore.key,
            new_ttl,
            "RestoreFootprint: restoring entry from hot archive to state"
        );

        // Track resource limits for hot archive entries
        let entry_size = restore
            .entry
            .to_xdr(Limits::none())
            .ok()
            .map(|bytes| bytes.len() as u32)
            .unwrap_or(0);

        // Validate contract entry size against config limits before restoring.
        // Matches stellar-core validateContractLedgerEntry() in RestoreFootprintOpFrame.
        if let Some(ref limits) = resources.size_limits {
            if !super::extend_footprint_ttl::validate_contract_ledger_entry(
                &restore.key,
                &restore.entry,
                limits,
            ) {
                return Ok(make_result(
                    RestoreFootprintResultCode::ResourceLimitExceeded,
                ));
            }
        }

        accumulated_read_bytes = accumulated_read_bytes.saturating_add(entry_size);
        if accumulated_read_bytes > disk_read_bytes_limit {
            return Ok(make_result(
                RestoreFootprintResultCode::ResourceLimitExceeded,
            ));
        }
        accumulated_write_bytes = accumulated_write_bytes.saturating_add(entry_size);
        if accumulated_write_bytes > write_bytes_limit {
            return Ok(make_result(
                RestoreFootprintResultCode::ResourceLimitExceeded,
            ));
        }

        // Add the entry to state based on type
        match &restore.entry.data {
            LedgerEntryData::ContractCode(code) => {
                state.create_contract_code(code.clone());
            }
            LedgerEntryData::ContractData(data) => {
                state.create_contract_data(data.clone());
            }
            _ => {
                // Hot archive should only contain ContractCode and ContractData
                tracing::warn!(
                    ?restore.key,
                    "RestoreFootprint: unexpected entry type in hot archive"
                );
            }
        }

        // Create the TTL entry for the restored entry
        let key_hash =
            crate::soroban::get_or_compute_key_hash(resources.ttl_key_cache, &restore.key);
        let ttl_entry = TtlEntry {
            key_hash,
            live_until_ledger_seq: new_ttl,
        };
        state.create_ttl(ttl_entry);
    }

    // Restore all entries in the read-write footprint that exist in live state
    // (these have expired TTLs but the entry still exists)
    for key in footprint.read_write.iter() {
        // Skip entries that were restored from hot archive - they're already handled
        if resources.hot_archive_restores.iter().any(|r| &r.key == key) {
            continue;
        }

        if let Err(()) = restore_entry(
            key,
            new_ttl,
            state,
            current_ledger.into(),
            resources.ttl_key_cache,
            &mut accumulated_read_bytes,
            &mut accumulated_write_bytes,
            disk_read_bytes_limit,
            write_bytes_limit,
            resources.size_limits.as_ref(),
        ) {
            return Ok(make_result(
                RestoreFootprintResultCode::ResourceLimitExceeded,
            ));
        }
    }

    Ok(make_result(RestoreFootprintResultCode::Success))
}

fn is_persistent_entry(key: &LedgerKey) -> bool {
    match key {
        LedgerKey::ContractCode(_) => true,
        LedgerKey::ContractData(cd) => matches!(
            cd.durability,
            stellar_xdr::curr::ContractDataDurability::Persistent
        ),
        _ => false,
    }
}

/// Restore a single ledger entry.
///
/// Returns `Err(())` if the resource limits would be exceeded.
#[allow(clippy::too_many_arguments)]
fn restore_entry(
    key: &LedgerKey,
    new_ttl: u32,
    state: &mut LedgerStateManager,
    current_ledger: LedgerSeq,
    ttl_key_cache: Option<&crate::soroban::TtlKeyCache>,
    accumulated_read_bytes: &mut u32,
    accumulated_write_bytes: &mut u32,
    disk_read_bytes_limit: u32,
    write_bytes_limit: u32,
    size_limits: Option<&super::extend_footprint_ttl::ContractSizeLimits>,
) -> std::result::Result<(), ()> {
    // Only contract data and contract code can be restored
    match key {
        LedgerKey::ContractData(_) | LedgerKey::ContractCode(_) => {}
        _ => return Ok(()), // Non-contract entries don't need restoration
    }

    // Compute the key hash for TTL lookup
    let key_hash = crate::soroban::get_or_compute_key_hash(ttl_key_cache, key);

    // Check the current TTL status
    let current_ttl = state.get_ttl(&key_hash).map(|t| t.live_until_ledger_seq);

    match current_ttl {
        Some(ttl) if ttl >= current_ledger.get() => {
            // Entry is still live, no restoration needed.
            Ok(())
        }
        Some(_) | None => {
            // Entry is archived or has no TTL entry
            // We need to check if the entry itself exists in state
            let entry = state.get_entry(key);
            let entry = match entry {
                None => {
                    // Neither live nor archived entry exists; skip per stellar-core behavior.
                    return Ok(());
                }
                Some(e) => e,
            };

            // Track resource limits (stellar-core: checkResourceLimits)
            let entry_size = entry
                .to_xdr(Limits::none())
                .ok()
                .map(|bytes| bytes.len() as u32)
                .unwrap_or(0);

            // Validate contract entry size against config limits.
            if let Some(limits) = size_limits {
                if !super::extend_footprint_ttl::validate_contract_ledger_entry(key, &entry, limits)
                {
                    return Err(());
                }
            }

            *accumulated_read_bytes = accumulated_read_bytes.saturating_add(entry_size);
            if *accumulated_read_bytes > disk_read_bytes_limit {
                return Err(());
            }
            *accumulated_write_bytes = accumulated_write_bytes.saturating_add(entry_size);
            if *accumulated_write_bytes > write_bytes_limit {
                return Err(());
            }

            // Create or update the TTL entry to restore the entry
            let ttl_entry = TtlEntry {
                key_hash: key_hash.clone(),
                live_until_ledger_seq: new_ttl,
            };

            if state.get_ttl(&key_hash).is_some() {
                state.update_ttl(ttl_entry);
            } else {
                state.create_ttl(ttl_entry);
            }

            Ok(())
        }
    }
}

/// Create an OperationResult from a RestoreFootprintResultCode.
fn make_result(code: RestoreFootprintResultCode) -> OperationResult {
    let result = match code {
        RestoreFootprintResultCode::Success => RestoreFootprintResult::Success,
        RestoreFootprintResultCode::Malformed => RestoreFootprintResult::Malformed,
        RestoreFootprintResultCode::ResourceLimitExceeded => {
            RestoreFootprintResult::ResourceLimitExceeded
        }
        RestoreFootprintResultCode::InsufficientRefundableFee => {
            RestoreFootprintResult::InsufficientRefundableFee
        }
    };

    OperationResult::OpInner(OperationResultTr::RestoreFootprint(result))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::create_test_account_id;
    use stellar_xdr::curr::*;

    /// Default min persistent TTL for tests (matches testnet config)
    const TEST_MIN_PERSISTENT_TTL: u32 = 120960;

    fn create_test_context() -> LedgerContext {
        LedgerContext::testnet(1, 1000)
    }

    #[test]
    fn test_restore_footprint_no_soroban_data() {
        let mut state = LedgerStateManager::new(5_000_000, 100.into());
        let context = create_test_context();
        let source = create_test_account_id(0);

        let op = RestoreFootprintOp {
            ext: ExtensionPoint::V0,
        };

        let result = execute_restore_footprint(
            &op,
            &source,
            &mut state,
            &context,
            RestoreFootprintResources {
                soroban_data: None,
                min_persistent_entry_ttl: TEST_MIN_PERSISTENT_TTL,
                hot_archive_restores: &[], // No hot archive restores
                ttl_key_cache: None,
                size_limits: None,
            },
        );
        assert!(result.is_ok());

        match result.unwrap() {
            OperationResult::OpInner(OperationResultTr::RestoreFootprint(r)) => {
                assert!(matches!(r, RestoreFootprintResult::Malformed));
            }
            _ => panic!("Unexpected result type"),
        }
    }

    #[test]
    fn test_restore_footprint_empty_footprint() {
        let mut state = LedgerStateManager::new(5_000_000, 100.into());
        let context = create_test_context();
        let source = create_test_account_id(0);

        let op = RestoreFootprintOp {
            ext: ExtensionPoint::V0,
        };

        let soroban_data = SorobanTransactionData {
            ext: SorobanTransactionDataExt::V0,
            resources: SorobanResources {
                footprint: LedgerFootprint {
                    read_only: vec![].try_into().unwrap(),
                    read_write: vec![].try_into().unwrap(),
                },
                instructions: 0,
                disk_read_bytes: 0,
                write_bytes: 0,
            },
            resource_fee: 0,
        };

        let result = execute_restore_footprint(
            &op,
            &source,
            &mut state,
            &context,
            RestoreFootprintResources {
                soroban_data: Some(&soroban_data),
                min_persistent_entry_ttl: TEST_MIN_PERSISTENT_TTL,
                hot_archive_restores: &[], // No hot archive restores
                ttl_key_cache: None,
                size_limits: None,
            },
        );
        assert!(result.is_ok());

        match result.unwrap() {
            OperationResult::OpInner(OperationResultTr::RestoreFootprint(r)) => {
                assert!(matches!(r, RestoreFootprintResult::Success));
            }
            _ => panic!("Unexpected result type"),
        }
    }

    #[test]
    fn test_restore_footprint_rejects_read_only() {
        let mut state = LedgerStateManager::new(5_000_000, 100.into());
        let context = create_test_context();
        let source = create_test_account_id(0);

        let op = RestoreFootprintOp {
            ext: ExtensionPoint::V0,
        };

        let contract_key = LedgerKey::ContractCode(LedgerKeyContractCode {
            hash: Hash([1u8; 32]),
        });

        let soroban_data = SorobanTransactionData {
            ext: SorobanTransactionDataExt::V0,
            resources: SorobanResources {
                footprint: LedgerFootprint {
                    read_only: vec![contract_key].try_into().unwrap(),
                    read_write: vec![].try_into().unwrap(),
                },
                instructions: 0,
                disk_read_bytes: 0,
                write_bytes: 0,
            },
            resource_fee: 0,
        };

        let result = execute_restore_footprint(
            &op,
            &source,
            &mut state,
            &context,
            RestoreFootprintResources {
                soroban_data: Some(&soroban_data),
                min_persistent_entry_ttl: TEST_MIN_PERSISTENT_TTL,
                hot_archive_restores: &[], // No hot archive restores
                ttl_key_cache: None,
                size_limits: None,
            },
        );
        assert!(result.is_ok());

        match result.unwrap() {
            OperationResult::OpInner(OperationResultTr::RestoreFootprint(r)) => {
                assert!(matches!(r, RestoreFootprintResult::Malformed));
            }
            _ => panic!("Unexpected result type"),
        }
    }

    #[test]
    fn test_restore_footprint_rejects_non_persistent_entry() {
        let mut state = LedgerStateManager::new(5_000_000, 100.into());
        let context = create_test_context();
        let source = create_test_account_id(0);

        let op = RestoreFootprintOp {
            ext: ExtensionPoint::V0,
        };

        let temp_key = LedgerKey::ContractData(LedgerKeyContractData {
            contract: ScAddress::Contract(ContractId(Hash([2u8; 32]))),
            key: ScVal::U32(1),
            durability: ContractDataDurability::Temporary,
        });

        let soroban_data = SorobanTransactionData {
            ext: SorobanTransactionDataExt::V0,
            resources: SorobanResources {
                footprint: LedgerFootprint {
                    read_only: vec![].try_into().unwrap(),
                    read_write: vec![temp_key].try_into().unwrap(),
                },
                instructions: 0,
                disk_read_bytes: 0,
                write_bytes: 0,
            },
            resource_fee: 0,
        };

        let result = execute_restore_footprint(
            &op,
            &source,
            &mut state,
            &context,
            RestoreFootprintResources {
                soroban_data: Some(&soroban_data),
                min_persistent_entry_ttl: TEST_MIN_PERSISTENT_TTL,
                hot_archive_restores: &[], // No hot archive restores
                ttl_key_cache: None,
                size_limits: None,
            },
        );
        assert!(result.is_ok());

        match result.unwrap() {
            OperationResult::OpInner(OperationResultTr::RestoreFootprint(r)) => {
                assert!(matches!(r, RestoreFootprintResult::Malformed));
            }
            _ => panic!("Unexpected result type"),
        }
    }

    /// Test RestoreFootprint rejects Account entries (not Soroban).
    ///
    /// C++ Reference: SorobanTest.cpp - "restore rejects account key" test section
    #[test]
    fn test_restore_footprint_rejects_account_entry() {
        let mut state = LedgerStateManager::new(5_000_000, 100.into());
        let context = create_test_context();
        let source = create_test_account_id(0);

        let op = RestoreFootprintOp {
            ext: ExtensionPoint::V0,
        };

        let account_key = LedgerKey::Account(LedgerKeyAccount {
            account_id: create_test_account_id(1),
        });

        let soroban_data = SorobanTransactionData {
            ext: SorobanTransactionDataExt::V0,
            resources: SorobanResources {
                footprint: LedgerFootprint {
                    read_only: vec![].try_into().unwrap(),
                    read_write: vec![account_key].try_into().unwrap(),
                },
                instructions: 0,
                disk_read_bytes: 0,
                write_bytes: 0,
            },
            resource_fee: 0,
        };

        let result = execute_restore_footprint(
            &op,
            &source,
            &mut state,
            &context,
            RestoreFootprintResources {
                soroban_data: Some(&soroban_data),
                min_persistent_entry_ttl: TEST_MIN_PERSISTENT_TTL,
                hot_archive_restores: &[],
                ttl_key_cache: None,
                size_limits: None,
            },
        );
        assert!(result.is_ok());

        match result.unwrap() {
            OperationResult::OpInner(OperationResultTr::RestoreFootprint(r)) => {
                assert!(
                    matches!(r, RestoreFootprintResult::Malformed),
                    "Account key should be rejected, got {:?}",
                    r
                );
            }
            _ => panic!("Unexpected result type"),
        }
    }

    /// Test RestoreFootprint with persistent ContractCode entry succeeds.
    ///
    /// Entry doesn't exist but that's OK - we just skip it.
    ///
    /// C++ Reference: SorobanTest.cpp - "restore contract code" test section
    #[test]
    fn test_restore_footprint_contract_code_missing() {
        let mut state = LedgerStateManager::new(5_000_000, 100.into());
        let context = create_test_context();
        let source = create_test_account_id(0);

        let op = RestoreFootprintOp {
            ext: ExtensionPoint::V0,
        };

        // ContractCode is always persistent
        let code_key = LedgerKey::ContractCode(LedgerKeyContractCode {
            hash: Hash([10u8; 32]),
        });

        let soroban_data = SorobanTransactionData {
            ext: SorobanTransactionDataExt::V0,
            resources: SorobanResources {
                footprint: LedgerFootprint {
                    read_only: vec![].try_into().unwrap(),
                    read_write: vec![code_key].try_into().unwrap(),
                },
                instructions: 0,
                disk_read_bytes: 0,
                write_bytes: 0,
            },
            resource_fee: 0,
        };

        let result = execute_restore_footprint(
            &op,
            &source,
            &mut state,
            &context,
            RestoreFootprintResources {
                soroban_data: Some(&soroban_data),
                min_persistent_entry_ttl: TEST_MIN_PERSISTENT_TTL,
                hot_archive_restores: &[],
                ttl_key_cache: None,
                size_limits: None,
            },
        );
        assert!(result.is_ok());

        match result.unwrap() {
            OperationResult::OpInner(OperationResultTr::RestoreFootprint(r)) => {
                // Missing entry is skipped - operation succeeds
                assert!(
                    matches!(r, RestoreFootprintResult::Success),
                    "Missing entry should be skipped, got {:?}",
                    r
                );
            }
            _ => panic!("Unexpected result type"),
        }
    }

    /// Test RestoreFootprint with persistent ContractData entry succeeds.
    ///
    /// C++ Reference: SorobanTest.cpp - "restore persistent data" test section
    #[test]
    fn test_restore_footprint_persistent_data_missing() {
        let mut state = LedgerStateManager::new(5_000_000, 100.into());
        let context = create_test_context();
        let source = create_test_account_id(0);

        let op = RestoreFootprintOp {
            ext: ExtensionPoint::V0,
        };

        let data_key = LedgerKey::ContractData(LedgerKeyContractData {
            contract: ScAddress::Contract(ContractId(Hash([20u8; 32]))),
            key: ScVal::I32(100),
            durability: ContractDataDurability::Persistent,
        });

        let soroban_data = SorobanTransactionData {
            ext: SorobanTransactionDataExt::V0,
            resources: SorobanResources {
                footprint: LedgerFootprint {
                    read_only: vec![].try_into().unwrap(),
                    read_write: vec![data_key].try_into().unwrap(),
                },
                instructions: 0,
                disk_read_bytes: 0,
                write_bytes: 0,
            },
            resource_fee: 0,
        };

        let result = execute_restore_footprint(
            &op,
            &source,
            &mut state,
            &context,
            RestoreFootprintResources {
                soroban_data: Some(&soroban_data),
                min_persistent_entry_ttl: TEST_MIN_PERSISTENT_TTL,
                hot_archive_restores: &[],
                ttl_key_cache: None,
                size_limits: None,
            },
        );
        assert!(result.is_ok());

        match result.unwrap() {
            OperationResult::OpInner(OperationResultTr::RestoreFootprint(r)) => {
                assert!(
                    matches!(r, RestoreFootprintResult::Success),
                    "Missing persistent entry should be skipped, got {:?}",
                    r
                );
            }
            _ => panic!("Unexpected result type"),
        }
    }

    /// Test RestoreFootprint rejects TrustLine entry.
    ///
    /// C++ Reference: SorobanTest.cpp - "restore rejects trustline" test section
    #[test]
    fn test_restore_footprint_rejects_trustline() {
        let mut state = LedgerStateManager::new(5_000_000, 100.into());
        let context = create_test_context();
        let source = create_test_account_id(0);

        let op = RestoreFootprintOp {
            ext: ExtensionPoint::V0,
        };

        let tl_key = LedgerKey::Trustline(LedgerKeyTrustLine {
            account_id: create_test_account_id(1),
            asset: TrustLineAsset::CreditAlphanum4(AlphaNum4 {
                asset_code: AssetCode4(*b"USD\0"),
                issuer: create_test_account_id(2),
            }),
        });

        let soroban_data = SorobanTransactionData {
            ext: SorobanTransactionDataExt::V0,
            resources: SorobanResources {
                footprint: LedgerFootprint {
                    read_only: vec![].try_into().unwrap(),
                    read_write: vec![tl_key].try_into().unwrap(),
                },
                instructions: 0,
                disk_read_bytes: 0,
                write_bytes: 0,
            },
            resource_fee: 0,
        };

        let result = execute_restore_footprint(
            &op,
            &source,
            &mut state,
            &context,
            RestoreFootprintResources {
                soroban_data: Some(&soroban_data),
                min_persistent_entry_ttl: TEST_MIN_PERSISTENT_TTL,
                hot_archive_restores: &[],
                ttl_key_cache: None,
                size_limits: None,
            },
        );
        assert!(result.is_ok());

        match result.unwrap() {
            OperationResult::OpInner(OperationResultTr::RestoreFootprint(r)) => {
                assert!(
                    matches!(r, RestoreFootprintResult::Malformed),
                    "TrustLine key should be rejected, got {:?}",
                    r
                );
            }
            _ => panic!("Unexpected result type"),
        }
    }

    /// Regression test for #1116: RestoreFootprint must check disk_read_bytes
    /// and write_bytes resource limits. With a limit of 0 and an actual entry
    /// that needs restoring, ResourceLimitExceeded should be returned.
    #[test]
    fn test_restore_footprint_resource_limit_exceeded() {
        let mut state = LedgerStateManager::new(5_000_000, 100.into());
        let context = create_test_context();
        let source = create_test_account_id(0);

        let op = RestoreFootprintOp {
            ext: ExtensionPoint::V0,
        };

        // Create a contract data entry with an expired TTL
        let contract_id = ScAddress::Contract(ContractId(Hash([30u8; 32])));
        let data_key = LedgerKey::ContractData(LedgerKeyContractData {
            contract: contract_id.clone(),
            key: ScVal::U32(42),
            durability: ContractDataDurability::Persistent,
        });

        let cd_entry = ContractDataEntry {
            ext: ExtensionPoint::V0,
            contract: contract_id,
            key: ScVal::U32(42),
            durability: ContractDataDurability::Persistent,
            val: ScVal::I32(7),
        };
        state.create_contract_data(cd_entry);

        // Create expired TTL entry
        let key_hash = crate::soroban::compute_key_hash(&data_key);
        state.create_ttl(TtlEntry {
            key_hash,
            live_until_ledger_seq: context.sequence - 1, // expired
        });

        // Set resource limits to 0 — any actual restore should fail
        let soroban_data = SorobanTransactionData {
            ext: SorobanTransactionDataExt::V0,
            resources: SorobanResources {
                footprint: LedgerFootprint {
                    read_only: vec![].try_into().unwrap(),
                    read_write: vec![data_key].try_into().unwrap(),
                },
                instructions: 0,
                disk_read_bytes: 0,
                write_bytes: 0,
            },
            resource_fee: 0,
        };

        let result = execute_restore_footprint(
            &op,
            &source,
            &mut state,
            &context,
            RestoreFootprintResources {
                soroban_data: Some(&soroban_data),
                min_persistent_entry_ttl: TEST_MIN_PERSISTENT_TTL,
                hot_archive_restores: &[],
                ttl_key_cache: None,
                size_limits: None,
            },
        );
        assert!(result.is_ok());

        match result.unwrap() {
            OperationResult::OpInner(OperationResultTr::RestoreFootprint(r)) => {
                assert!(
                    matches!(r, RestoreFootprintResult::ResourceLimitExceeded),
                    "Expected ResourceLimitExceeded with 0 disk_read_bytes, got {:?}",
                    r
                );
            }
            _ => panic!("Unexpected result type"),
        }
    }
}
