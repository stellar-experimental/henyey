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
    pub size_limits: Option<super::ContractSizeLimits>,
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
    let mut accumulator = ResourceAccumulator::new(
        soroban_data.resources.disk_read_bytes,
        soroban_data.resources.write_bytes,
    );

    // First, restore hot archive entries to state.
    // These entries don't exist in the live bucket list, so we need to add them.
    // SECURITY: hot_archive_restores populated by ledger execution layer from local hot archive, not external tx input
    for restore in resources.hot_archive_restores {
        tracing::debug!(
            ?restore.key,
            new_ttl,
            "RestoreFootprint: restoring entry from hot archive to state"
        );

        let entry_size = xdr_entry_size(&restore.entry);

        // stellar-core ordering: read_bytes → validate_contract → write_bytes
        if accumulator.add_read(entry_size).is_err() {
            return Ok(make_result(
                RestoreFootprintResultCode::ResourceLimitExceeded,
            ));
        }
        if let Some(ref limits) = resources.size_limits {
            if !super::validate_contract_ledger_entry(&restore.key, entry_size as usize, limits) {
                return Ok(make_result(
                    RestoreFootprintResultCode::ResourceLimitExceeded,
                ));
            }
        }
        if accumulator.add_write(entry_size).is_err() {
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
    let restore_ctx = RestoreContext {
        new_ttl,
        current_ledger,
        ttl_key_cache: resources.ttl_key_cache,
        size_limits: resources.size_limits.as_ref(),
    };
    for key in footprint.read_write.iter() {
        // Skip entries that were restored from hot archive - they're already handled
        if resources.hot_archive_restores.iter().any(|r| &r.key == key) {
            continue;
        }

        if restore_entry(key, state, &restore_ctx, &mut accumulator).is_err() {
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

/// Compute XDR-serialized size of a ledger entry.
///
/// Returns 0 if serialization fails (entry will effectively pass limit checks
/// with zero cost, matching stellar-core's `xdr_size` which never fails on
/// well-formed entries).
fn xdr_entry_size(entry: &LedgerEntry) -> u32 {
    entry
        .to_xdr(Limits::none())
        .ok()
        .map(|bytes| bytes.len() as u32)
        .unwrap_or(0)
}

/// Tracks accumulated read and write bytes against declared resource limits.
///
/// Mirrors stellar-core's `mMetrics.mLedgerReadByte` / `mMetrics.mLedgerWriteByte`
/// tracking in `RestoreFootprintApplyHelper::apply`.
struct ResourceAccumulator {
    read_bytes: u32,
    write_bytes: u32,
    read_limit: u32,
    write_limit: u32,
}

impl ResourceAccumulator {
    fn new(read_limit: u32, write_limit: u32) -> Self {
        Self {
            read_bytes: 0,
            write_bytes: 0,
            read_limit,
            write_limit,
        }
    }

    /// Add to read byte accumulator. Returns Err(()) if limit exceeded.
    fn add_read(&mut self, size: u32) -> std::result::Result<(), ()> {
        self.read_bytes = self.read_bytes.saturating_add(size);
        if self.read_bytes > self.read_limit {
            return Err(());
        }
        Ok(())
    }

    /// Add to write byte accumulator. Returns Err(()) if limit exceeded.
    fn add_write(&mut self, size: u32) -> std::result::Result<(), ()> {
        self.write_bytes = self.write_bytes.saturating_add(size);
        if self.write_bytes > self.write_limit {
            return Err(());
        }
        Ok(())
    }
}

/// Context for restoring entries, bundling TTL and validation config.
struct RestoreContext<'a> {
    new_ttl: u32,
    current_ledger: u32,
    ttl_key_cache: Option<&'a crate::soroban::TtlKeyCache>,
    size_limits: Option<&'a super::ContractSizeLimits>,
}

/// Restore a single ledger entry.
///
/// Checks resource limits via the accumulator and restores the TTL.
/// Returns `Err(())` if any resource limit is exceeded.
fn restore_entry(
    key: &LedgerKey,
    state: &mut LedgerStateManager,
    ctx: &RestoreContext,
    accumulator: &mut ResourceAccumulator,
) -> std::result::Result<(), ()> {
    // Only contract data and contract code can be restored
    match key {
        LedgerKey::ContractData(_) | LedgerKey::ContractCode(_) => {}
        _ => return Ok(()),
    }

    // Compute the key hash for TTL lookup
    let key_hash = crate::soroban::get_or_compute_key_hash(ctx.ttl_key_cache, key);

    // Check the current TTL status
    let current_ttl = state.get_ttl(&key_hash).map(|t| t.live_until_ledger_seq);

    match current_ttl {
        Some(ttl) if ttl >= ctx.current_ledger => {
            // Entry is still live, no restoration needed.
            Ok(())
        }
        Some(_) | None => {
            let entry = match state.get_entry(key) {
                None => return Ok(()),
                Some(e) => e,
            };

            let entry_size = xdr_entry_size(&entry);

            // stellar-core ordering: read_bytes → validate_contract → write_bytes
            accumulator.add_read(entry_size)?;
            if let Some(limits) = ctx.size_limits {
                if !super::validate_contract_ledger_entry(key, entry_size as usize, limits) {
                    return Err(());
                }
            }
            accumulator.add_write(entry_size)?;

            // Create or update the TTL entry to restore the entry
            let ttl_entry = TtlEntry {
                key_hash: key_hash.clone(),
                live_until_ledger_seq: ctx.new_ttl,
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
        let mut state = LedgerStateManager::new(5_000_000, 100);
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
        let mut state = LedgerStateManager::new(5_000_000, 100);
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
        let mut state = LedgerStateManager::new(5_000_000, 100);
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
        let mut state = LedgerStateManager::new(5_000_000, 100);
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
        let mut state = LedgerStateManager::new(5_000_000, 100);
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
        let mut state = LedgerStateManager::new(5_000_000, 100);
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
        let mut state = LedgerStateManager::new(5_000_000, 100);
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
        let mut state = LedgerStateManager::new(5_000_000, 100);
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
        let mut state = LedgerStateManager::new(5_000_000, 100);
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

    fn make_contract_code_entry(hash: Hash) -> LedgerEntry {
        LedgerEntry {
            last_modified_ledger_seq: 1,
            data: LedgerEntryData::ContractCode(ContractCodeEntry {
                ext: ContractCodeEntryExt::V0,
                hash: hash.clone(),
                code: vec![0u8; 100].try_into().unwrap(),
            }),
            ext: LedgerEntryExt::V0,
        }
    }

    fn assert_restore_result(
        result: crate::Result<OperationResult>,
        expected: RestoreFootprintResult,
    ) {
        match result.unwrap() {
            OperationResult::OpInner(OperationResultTr::RestoreFootprint(r)) => {
                assert_eq!(r, expected, "Unexpected restore result");
            }
            other => panic!("Unexpected result type: {:?}", other),
        }
    }

    /// Test hot archive entry restoration with sufficient resource limits.
    #[test]
    fn test_restore_footprint_hot_archive_success() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();
        let source = create_test_account_id(0);
        let op = RestoreFootprintOp {
            ext: ExtensionPoint::V0,
        };

        let hash = Hash([50u8; 32]);
        let key = LedgerKey::ContractCode(LedgerKeyContractCode { hash: hash.clone() });
        let entry = make_contract_code_entry(hash);

        let hot_restores = vec![HotArchiveRestoreEntry {
            key: key.clone(),
            entry,
        }];

        let soroban_data = SorobanTransactionData {
            ext: SorobanTransactionDataExt::V0,
            resources: SorobanResources {
                footprint: LedgerFootprint {
                    read_only: vec![].try_into().unwrap(),
                    read_write: vec![key].try_into().unwrap(),
                },
                instructions: 0,
                disk_read_bytes: 10_000,
                write_bytes: 10_000,
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
                hot_archive_restores: &hot_restores,
                ttl_key_cache: None,
                size_limits: None,
            },
        );
        assert_restore_result(result, RestoreFootprintResult::Success);
    }

    /// Test hot archive entry exceeds read_bytes limit.
    #[test]
    fn test_restore_footprint_hot_archive_read_limit_exceeded() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();
        let source = create_test_account_id(0);
        let op = RestoreFootprintOp {
            ext: ExtensionPoint::V0,
        };

        let hash = Hash([51u8; 32]);
        let key = LedgerKey::ContractCode(LedgerKeyContractCode { hash: hash.clone() });
        let entry = make_contract_code_entry(hash);

        let hot_restores = vec![HotArchiveRestoreEntry {
            key: key.clone(),
            entry,
        }];

        // disk_read_bytes = 0 means any entry exceeds it
        let soroban_data = SorobanTransactionData {
            ext: SorobanTransactionDataExt::V0,
            resources: SorobanResources {
                footprint: LedgerFootprint {
                    read_only: vec![].try_into().unwrap(),
                    read_write: vec![key].try_into().unwrap(),
                },
                instructions: 0,
                disk_read_bytes: 0,
                write_bytes: 10_000,
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
                hot_archive_restores: &hot_restores,
                ttl_key_cache: None,
                size_limits: None,
            },
        );
        assert_restore_result(result, RestoreFootprintResult::ResourceLimitExceeded);
    }

    /// Test combined hot archive + live entry accumulation crosses the write limit.
    #[test]
    fn test_restore_footprint_combined_hot_and_live_write_limit() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();
        let source = create_test_account_id(0);
        let op = RestoreFootprintOp {
            ext: ExtensionPoint::V0,
        };

        // Hot archive entry
        let hot_hash = Hash([60u8; 32]);
        let hot_key = LedgerKey::ContractCode(LedgerKeyContractCode {
            hash: hot_hash.clone(),
        });
        let hot_entry = make_contract_code_entry(hot_hash);
        let hot_entry_size = hot_entry.to_xdr(Limits::none()).unwrap().len() as u32;

        let hot_restores = vec![HotArchiveRestoreEntry {
            key: hot_key.clone(),
            entry: hot_entry,
        }];

        // Live entry with expired TTL
        let live_hash = Hash([61u8; 32]);
        let live_key = LedgerKey::ContractCode(LedgerKeyContractCode {
            hash: live_hash.clone(),
        });
        let live_entry = ContractCodeEntry {
            ext: ContractCodeEntryExt::V0,
            hash: live_hash.clone(),
            code: vec![0u8; 100].try_into().unwrap(),
        };
        state.create_contract_code(live_entry);
        let live_key_hash = crate::soroban::compute_key_hash(&live_key);
        state.create_ttl(TtlEntry {
            key_hash: live_key_hash,
            live_until_ledger_seq: context.sequence - 1, // expired
        });

        // Set write_bytes just enough for the hot entry but not both
        let soroban_data = SorobanTransactionData {
            ext: SorobanTransactionDataExt::V0,
            resources: SorobanResources {
                footprint: LedgerFootprint {
                    read_only: vec![].try_into().unwrap(),
                    read_write: vec![hot_key, live_key].try_into().unwrap(),
                },
                instructions: 0,
                disk_read_bytes: 100_000,
                write_bytes: hot_entry_size, // just enough for first, not second
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
                hot_archive_restores: &hot_restores,
                ttl_key_cache: None,
                size_limits: None,
            },
        );
        assert_restore_result(result, RestoreFootprintResult::ResourceLimitExceeded);
    }
}
