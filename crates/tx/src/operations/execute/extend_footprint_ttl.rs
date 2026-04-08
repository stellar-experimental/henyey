//! ExtendFootprintTtl operation execution.
//!
//! This module implements the execution logic for the ExtendFootprintTtl operation,
//! which extends the time-to-live for Soroban contract data entries.

use stellar_xdr::curr::{
    AccountId, ExtendFootprintTtlOp, ExtendFootprintTtlResult, ExtendFootprintTtlResultCode,
    LedgerKey, OperationResult, OperationResultTr, SorobanTransactionData, WriteXdr,
};

use crate::state::LedgerStateManager;
use crate::validation::LedgerContext;
use crate::Result;

/// Max TTL extension for entries (in ledgers).
const MAX_ENTRY_TTL: u32 = 6_312_000; // ~1 year at 5-second ledger close

/// Execute an ExtendFootprintTtl operation.
///
/// This operation extends the TTL of all entries in the transaction's footprint
/// to at least the specified ledger sequence.
///
/// Matches stellar-core ExtendFootprintTTLApplyHelper::apply() behavior:
/// - Skips missing entries (not found in state)
/// - Skips archived/non-live entries (TTL < current_ledger)
/// - Skips entries whose TTL already meets or exceeds the target
/// - Tracks accumulated read bytes and fails with ResourceLimitExceeded
///   if disk_read_bytes limit is exceeded
pub(crate) fn execute_extend_footprint_ttl(
    op: &ExtendFootprintTtlOp,
    _source: &AccountId,
    state: &mut LedgerStateManager,
    context: &LedgerContext,
    soroban_data: Option<&SorobanTransactionData>,
    ttl_key_cache: Option<&crate::soroban::TtlKeyCache>,
) -> Result<OperationResult> {
    // stellar-core only rejects extend_to > MAX_ENTRY_TTL - 1;
    // extend_to=0 is valid and results in a no-op (target TTL <= any live entry's TTL).
    if op.extend_to > MAX_ENTRY_TTL.saturating_sub(1) {
        return Ok(make_result(ExtendFootprintTtlResultCode::Malformed));
    }

    // Get the footprint from Soroban transaction data
    let soroban_data = match soroban_data {
        Some(data) => data,
        None => {
            return Ok(make_result(ExtendFootprintTtlResultCode::Malformed));
        }
    };
    let footprint = &soroban_data.resources.footprint;

    if !footprint.read_write.is_empty() {
        return Ok(make_result(ExtendFootprintTtlResultCode::Malformed));
    }

    for key in footprint.read_only.iter() {
        if !is_ttl_entry(key) {
            return Ok(make_result(ExtendFootprintTtlResultCode::Malformed));
        }
    }

    // Calculate the target TTL ledger sequence
    // stellar-core: newLiveUntilLedgerSeq = getLedgerSeq() + mOpFrame.mExtendFootprintTTLOp.extendTo
    let current_ledger = context.sequence;
    let new_live_until = current_ledger.saturating_add(op.extend_to);
    let disk_read_bytes_limit = soroban_data.resources.disk_read_bytes;
    let mut accumulated_read_bytes: u32 = 0;

    // Extend TTL for all entries in the read-only footprint.
    // Matches stellar-core ExtendFootprintTTLApplyHelper::apply():
    // - Look up TTL key first; skip missing/non-live entries
    // - Skip entries whose TTL already meets target
    // - Check read bytes resource limit
    for key in footprint.read_only.iter() {
        let key_hash = crate::soroban::get_or_compute_key_hash(ttl_key_cache, key);

        // Look up the TTL entry for this key
        let ttl_entry = state.get_ttl(&key_hash).cloned();
        match ttl_entry {
            None => {
                // TTL entry not found - skip (stellar-core: !ttlLeOpt -> continue)
                continue;
            }
            Some(ttl) => {
                if ttl.live_until_ledger_seq < current_ledger {
                    // Entry is not live (archived/expired) - skip
                    // stellar-core: !isLive(*ttlLeOpt, getLedgerSeq()) -> continue
                    continue;
                }
                if ttl.live_until_ledger_seq >= new_live_until {
                    // TTL already sufficient - skip
                    // stellar-core: currLiveUntilLedgerSeq >= newLiveUntilLedgerSeq -> continue
                    continue;
                }
            }
        }

        // The main entry must exist (TTL exists and is live => entry exists)
        // stellar-core: releaseAssertOrThrow(entryOpt)
        let entry = state.get_entry(key);
        if entry.is_none() {
            // Should not happen if TTL is live, but be safe
            continue;
        }

        // Track read bytes and check limit
        // stellar-core: checkReadBytesResourceLimit(entrySize)
        //
        // On protocol >= 23 (PARALLEL_SOROBAN_PHASE_PROTOCOL_VERSION), stellar-core's
        // ExtendFootprintTTLParallelApplyHelper::checkReadBytesResourceLimit() always
        // returns true — it skips the disk_read_bytes check entirely. The resource
        // accounting is handled at the cluster level instead.
        if context.protocol_version < 23 {
            let entry_size = entry
                .and_then(|e| e.to_xdr(stellar_xdr::curr::Limits::none()).ok())
                .map(|bytes| bytes.len() as u32)
                .unwrap_or(0);
            accumulated_read_bytes = accumulated_read_bytes.saturating_add(entry_size);
            if accumulated_read_bytes > disk_read_bytes_limit {
                return Ok(make_result(
                    ExtendFootprintTtlResultCode::ResourceLimitExceeded,
                ));
            }
        }

        // Extend the TTL
        state.extend_ttl(&key_hash, new_live_until);
    }

    Ok(make_result(ExtendFootprintTtlResultCode::Success))
}

fn is_ttl_entry(key: &LedgerKey) -> bool {
    matches!(key, LedgerKey::ContractData(_) | LedgerKey::ContractCode(_))
}

/// Create an OperationResult from an ExtendFootprintTtlResultCode.
fn make_result(code: ExtendFootprintTtlResultCode) -> OperationResult {
    let result = match code {
        ExtendFootprintTtlResultCode::Success => ExtendFootprintTtlResult::Success,
        ExtendFootprintTtlResultCode::Malformed => ExtendFootprintTtlResult::Malformed,
        ExtendFootprintTtlResultCode::ResourceLimitExceeded => {
            ExtendFootprintTtlResult::ResourceLimitExceeded
        }
        ExtendFootprintTtlResultCode::InsufficientRefundableFee => {
            ExtendFootprintTtlResult::InsufficientRefundableFee
        }
    };

    OperationResult::OpInner(OperationResultTr::ExtendFootprintTtl(result))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::create_test_account_id;
    use stellar_xdr::curr::*;

    fn create_test_context() -> LedgerContext {
        LedgerContext::testnet(1, 1000)
    }

    /// Regression test for #1118: extend_to=0 must be accepted as a no-op,
    /// not rejected as Malformed. stellar-core does not check extend_to==0.
    #[test]
    fn test_extend_footprint_ttl_extend_to_zero_is_noop() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();
        let source = create_test_account_id(0);

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

        let op = ExtendFootprintTtlOp {
            ext: ExtensionPoint::V0,
            extend_to: 0,
        };

        let result = execute_extend_footprint_ttl(
            &op,
            &source,
            &mut state,
            &context,
            Some(&soroban_data),
            None,
        );
        assert!(result.is_ok());

        match result.unwrap() {
            OperationResult::OpInner(OperationResultTr::ExtendFootprintTtl(r)) => {
                assert!(
                    matches!(r, ExtendFootprintTtlResult::Success),
                    "extend_to=0 should succeed as a no-op, got {:?}",
                    r
                );
            }
            _ => panic!("Unexpected result type"),
        }
    }

    #[test]
    fn test_extend_footprint_ttl_no_soroban_data() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();
        let source = create_test_account_id(0);

        let op = ExtendFootprintTtlOp {
            ext: ExtensionPoint::V0,
            extend_to: 1000,
        };

        // No Soroban data provided
        let result = execute_extend_footprint_ttl(&op, &source, &mut state, &context, None, None);
        assert!(result.is_ok());

        match result.unwrap() {
            OperationResult::OpInner(OperationResultTr::ExtendFootprintTtl(r)) => {
                assert!(matches!(r, ExtendFootprintTtlResult::Malformed));
            }
            _ => panic!("Unexpected result type"),
        }
    }

    #[test]
    fn test_extend_footprint_ttl_rejects_read_write() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();
        let source = create_test_account_id(0);

        let op = ExtendFootprintTtlOp {
            ext: ExtensionPoint::V0,
            extend_to: 1000,
        };

        let contract_key = LedgerKey::ContractCode(LedgerKeyContractCode {
            hash: Hash([1u8; 32]),
        });

        let soroban_data = SorobanTransactionData {
            ext: SorobanTransactionDataExt::V0,
            resources: SorobanResources {
                footprint: LedgerFootprint {
                    read_only: vec![].try_into().unwrap(),
                    read_write: vec![contract_key].try_into().unwrap(),
                },
                instructions: 0,
                disk_read_bytes: 0,
                write_bytes: 0,
            },
            resource_fee: 0,
        };

        let result = execute_extend_footprint_ttl(
            &op,
            &source,
            &mut state,
            &context,
            Some(&soroban_data),
            None,
        );
        assert!(result.is_ok());

        match result.unwrap() {
            OperationResult::OpInner(OperationResultTr::ExtendFootprintTtl(r)) => {
                assert!(matches!(r, ExtendFootprintTtlResult::Malformed));
            }
            _ => panic!("Unexpected result type"),
        }
    }

    #[test]
    fn test_extend_footprint_ttl_rejects_non_ttl_entry() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();
        let source = create_test_account_id(0);

        let op = ExtendFootprintTtlOp {
            ext: ExtensionPoint::V0,
            extend_to: 1000,
        };

        let account_key = LedgerKey::Account(LedgerKeyAccount {
            account_id: create_test_account_id(1),
        });

        let soroban_data = SorobanTransactionData {
            ext: SorobanTransactionDataExt::V0,
            resources: SorobanResources {
                footprint: LedgerFootprint {
                    read_only: vec![account_key].try_into().unwrap(),
                    read_write: vec![].try_into().unwrap(),
                },
                instructions: 0,
                disk_read_bytes: 0,
                write_bytes: 0,
            },
            resource_fee: 0,
        };

        let result = execute_extend_footprint_ttl(
            &op,
            &source,
            &mut state,
            &context,
            Some(&soroban_data),
            None,
        );
        assert!(result.is_ok());

        match result.unwrap() {
            OperationResult::OpInner(OperationResultTr::ExtendFootprintTtl(r)) => {
                assert!(matches!(r, ExtendFootprintTtlResult::Malformed));
            }
            _ => panic!("Unexpected result type"),
        }
    }

    #[test]
    fn test_extend_footprint_ttl_rejects_large_extend_to() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();
        let source = create_test_account_id(0);

        let op = ExtendFootprintTtlOp {
            ext: ExtensionPoint::V0,
            extend_to: MAX_ENTRY_TTL,
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

        let result = execute_extend_footprint_ttl(
            &op,
            &source,
            &mut state,
            &context,
            Some(&soroban_data),
            None,
        );
        assert!(result.is_ok());

        match result.unwrap() {
            OperationResult::OpInner(OperationResultTr::ExtendFootprintTtl(r)) => {
                assert!(matches!(r, ExtendFootprintTtlResult::Malformed));
            }
            _ => panic!("Unexpected result type"),
        }
    }

    /// Test ExtendFootprintTtl succeeds with empty footprint (no-op).
    ///
    /// C++ Reference: SorobanTest.cpp - "extend ttl empty footprint" test section
    #[test]
    fn test_extend_footprint_ttl_success_empty_footprint() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();
        let source = create_test_account_id(0);

        let op = ExtendFootprintTtlOp {
            ext: ExtensionPoint::V0,
            extend_to: 1000, // Valid extend_to value
        };

        // Empty footprint is valid
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

        let result = execute_extend_footprint_ttl(
            &op,
            &source,
            &mut state,
            &context,
            Some(&soroban_data),
            None,
        );
        assert!(result.is_ok());

        match result.unwrap() {
            OperationResult::OpInner(OperationResultTr::ExtendFootprintTtl(r)) => {
                assert!(
                    matches!(r, ExtendFootprintTtlResult::Success),
                    "Empty footprint should succeed, got {:?}",
                    r
                );
            }
            _ => panic!("Unexpected result type"),
        }
    }

    /// Test ExtendFootprintTtl at max valid extend_to (MAX_ENTRY_TTL - 1).
    ///
    /// C++ Reference: SorobanTest.cpp - "extend ttl boundary" test section
    #[test]
    fn test_extend_footprint_ttl_at_max_boundary() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();
        let source = create_test_account_id(0);

        // MAX_ENTRY_TTL - 1 is the highest valid value
        let op = ExtendFootprintTtlOp {
            ext: ExtensionPoint::V0,
            extend_to: MAX_ENTRY_TTL - 1,
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

        let result = execute_extend_footprint_ttl(
            &op,
            &source,
            &mut state,
            &context,
            Some(&soroban_data),
            None,
        );
        assert!(result.is_ok());

        match result.unwrap() {
            OperationResult::OpInner(OperationResultTr::ExtendFootprintTtl(r)) => {
                assert!(
                    matches!(r, ExtendFootprintTtlResult::Success),
                    "Max boundary should succeed, got {:?}",
                    r
                );
            }
            _ => panic!("Unexpected result type"),
        }
    }

    /// Test ExtendFootprintTtl with missing ContractCode entry succeeds (skips).
    ///
    /// C++ Reference: ExtendFootprintTTLOpFrame.cpp lines 121-131 -
    /// missing entries are skipped with `continue`, not failed.
    /// Testnet ledger 197881: ExtendFootprintTtl with archived entries must succeed.
    #[test]
    fn test_extend_footprint_ttl_skips_missing_contract_code() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();
        let source = create_test_account_id(0);

        let op = ExtendFootprintTtlOp {
            ext: ExtensionPoint::V0,
            extend_to: 1000,
        };

        // ContractCode key - valid for TTL but entry doesn't exist in state
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

        let result = execute_extend_footprint_ttl(
            &op,
            &source,
            &mut state,
            &context,
            Some(&soroban_data),
            None,
        );
        assert!(result.is_ok());

        match result.unwrap() {
            OperationResult::OpInner(OperationResultTr::ExtendFootprintTtl(r)) => {
                assert!(
                    matches!(r, ExtendFootprintTtlResult::Success),
                    "Missing entry should be skipped (success), got {:?}",
                    r
                );
            }
            _ => panic!("Unexpected result type"),
        }
    }

    /// Test ExtendFootprintTtl with missing ContractData entry succeeds (skips).
    ///
    /// C++ Reference: ExtendFootprintTTLOpFrame.cpp lines 121-131 -
    /// missing entries are skipped with `continue`, not failed.
    #[test]
    fn test_extend_footprint_ttl_skips_missing_contract_data() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();
        let source = create_test_account_id(0);

        let op = ExtendFootprintTtlOp {
            ext: ExtensionPoint::V0,
            extend_to: 1000,
        };

        // ContractData key - valid for TTL but entry doesn't exist in state
        let contract_data_key = LedgerKey::ContractData(LedgerKeyContractData {
            contract: ScAddress::Account(create_test_account_id(1)),
            key: ScVal::I32(42),
            durability: ContractDataDurability::Persistent,
        });

        let soroban_data = SorobanTransactionData {
            ext: SorobanTransactionDataExt::V0,
            resources: SorobanResources {
                footprint: LedgerFootprint {
                    read_only: vec![contract_data_key].try_into().unwrap(),
                    read_write: vec![].try_into().unwrap(),
                },
                instructions: 0,
                disk_read_bytes: 0,
                write_bytes: 0,
            },
            resource_fee: 0,
        };

        let result = execute_extend_footprint_ttl(
            &op,
            &source,
            &mut state,
            &context,
            Some(&soroban_data),
            None,
        );
        assert!(result.is_ok());

        match result.unwrap() {
            OperationResult::OpInner(OperationResultTr::ExtendFootprintTtl(r)) => {
                assert!(
                    matches!(r, ExtendFootprintTtlResult::Success),
                    "Missing entry should be skipped (success), got {:?}",
                    r
                );
            }
            _ => panic!("Unexpected result type"),
        }
    }

    /// Regression test for VE-17 (L61997500): On protocol >= 23
    /// (PARALLEL_SOROBAN_PHASE_PROTOCOL_VERSION), stellar-core's
    /// ExtendFootprintTTLParallelApplyHelper::checkReadBytesResourceLimit()
    /// always returns true — disk_read_bytes enforcement is skipped.
    /// Our code was incorrectly enforcing it on all protocols, causing
    /// ResourceLimitExceeded where stellar-core returned Success.
    #[test]
    fn test_extend_footprint_ttl_skips_disk_read_bytes_on_protocol_23_plus() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        // Protocol 25 (mainnet current) — must skip disk_read_bytes check
        let context = LedgerContext {
            protocol_version: 25,
            ..LedgerContext::testnet(1, 1000)
        };
        let source = create_test_account_id(0);

        let contract_hash = Hash([99u8; 32]);
        let contract_key = LedgerKey::ContractCode(LedgerKeyContractCode {
            hash: contract_hash.clone(),
        });

        // Insert a contract code entry with enough bytes to exceed disk_read_bytes=1
        let code_entry = ContractCodeEntry {
            ext: stellar_xdr::curr::ContractCodeEntryExt::V0,
            hash: contract_hash.clone(),
            code: vec![0u8; 500].try_into().unwrap(),
        };
        state.create_contract_code(code_entry);

        // Insert a TTL entry that needs extending
        let key_hash = crate::soroban::compute_key_hash(&contract_key);
        let ttl_entry = stellar_xdr::curr::TtlEntry {
            key_hash: key_hash.clone(),
            live_until_ledger_seq: context.sequence + 10,
        };
        state.create_ttl(ttl_entry);

        let op = ExtendFootprintTtlOp {
            ext: ExtensionPoint::V0,
            extend_to: 1000,
        };

        // disk_read_bytes = 1 — would fail on pre-23 protocol since entry > 1 byte
        let soroban_data = SorobanTransactionData {
            ext: SorobanTransactionDataExt::V0,
            resources: SorobanResources {
                footprint: LedgerFootprint {
                    read_only: vec![contract_key].try_into().unwrap(),
                    read_write: vec![].try_into().unwrap(),
                },
                instructions: 0,
                disk_read_bytes: 1, // Tiny limit — would fail on pre-23
                write_bytes: 0,
            },
            resource_fee: 0,
        };

        let result = execute_extend_footprint_ttl(
            &op,
            &source,
            &mut state,
            &context,
            Some(&soroban_data),
            None,
        );
        assert!(result.is_ok());

        match result.unwrap() {
            OperationResult::OpInner(OperationResultTr::ExtendFootprintTtl(r)) => {
                assert!(
                    matches!(r, ExtendFootprintTtlResult::Success),
                    "Protocol >= 23 should skip disk_read_bytes check and succeed, got {:?}",
                    r
                );
            }
            _ => panic!("Unexpected result type"),
        }
    }

    /// Verify that pre-23 protocol still enforces disk_read_bytes for ExtendFootprintTtl.
    /// This is the counterpart to the VE-17 regression test above.
    #[test]
    fn test_extend_footprint_ttl_enforces_disk_read_bytes_on_pre_v23() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        // Protocol 22 — must enforce disk_read_bytes check
        let context = LedgerContext {
            protocol_version: 22,
            ..LedgerContext::testnet(1, 1000)
        };
        let source = create_test_account_id(0);

        let contract_hash = Hash([98u8; 32]);
        let contract_key = LedgerKey::ContractCode(LedgerKeyContractCode {
            hash: contract_hash.clone(),
        });

        let code_entry = ContractCodeEntry {
            ext: stellar_xdr::curr::ContractCodeEntryExt::V0,
            hash: contract_hash.clone(),
            code: vec![0u8; 500].try_into().unwrap(),
        };
        state.create_contract_code(code_entry);

        let key_hash = crate::soroban::compute_key_hash(&contract_key);
        let ttl_entry = stellar_xdr::curr::TtlEntry {
            key_hash: key_hash.clone(),
            live_until_ledger_seq: context.sequence + 10,
        };
        state.create_ttl(ttl_entry);

        let op = ExtendFootprintTtlOp {
            ext: ExtensionPoint::V0,
            extend_to: 1000,
        };

        // disk_read_bytes = 1 — entry is ~500+ bytes, should exceed limit on pre-23
        let soroban_data = SorobanTransactionData {
            ext: SorobanTransactionDataExt::V0,
            resources: SorobanResources {
                footprint: LedgerFootprint {
                    read_only: vec![contract_key].try_into().unwrap(),
                    read_write: vec![].try_into().unwrap(),
                },
                instructions: 0,
                disk_read_bytes: 1,
                write_bytes: 0,
            },
            resource_fee: 0,
        };

        let result = execute_extend_footprint_ttl(
            &op,
            &source,
            &mut state,
            &context,
            Some(&soroban_data),
            None,
        );
        assert!(result.is_ok());

        match result.unwrap() {
            OperationResult::OpInner(OperationResultTr::ExtendFootprintTtl(r)) => {
                assert!(
                    matches!(r, ExtendFootprintTtlResult::ResourceLimitExceeded),
                    "Protocol < 23 should enforce disk_read_bytes and reject, got {:?}",
                    r
                );
            }
            _ => panic!("Unexpected result type"),
        }
    }

    /// Regression test for AUDIT-M33: zero disk_read_bytes limit must reject
    /// any non-zero accumulated read bytes, not skip enforcement entirely.
    ///
    /// stellar-core: ExtendFootprintTTLOpFrame.cpp line 219 —
    /// `if (mResources.diskReadBytes < mMetrics.mLedgerReadByte)` has no > 0 guard.
    #[test]
    fn test_extend_footprint_ttl_zero_disk_read_bytes_rejects() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();
        let source = create_test_account_id(0);

        let contract_hash = Hash([42u8; 32]);
        let contract_key = LedgerKey::ContractCode(LedgerKeyContractCode {
            hash: contract_hash.clone(),
        });

        // Insert a contract code entry into state so the lookup succeeds
        let code_entry = ContractCodeEntry {
            ext: stellar_xdr::curr::ContractCodeEntryExt::V0,
            hash: contract_hash.clone(),
            code: vec![0u8; 100].try_into().unwrap(),
        };
        state.create_contract_code(code_entry);

        // Insert a TTL entry that needs extending (live_until < current + extend_to)
        let key_hash = crate::soroban::compute_key_hash(&contract_key);
        let ttl_entry = stellar_xdr::curr::TtlEntry {
            key_hash: key_hash.clone(),
            live_until_ledger_seq: context.sequence + 10, // live but needs extending
        };
        state.create_ttl(ttl_entry);

        let op = ExtendFootprintTtlOp {
            ext: ExtensionPoint::V0,
            extend_to: 1000, // target = current + 1000, well beyond current TTL
        };

        // disk_read_bytes = 0 means zero budget: any read should exceed it
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

        let result = execute_extend_footprint_ttl(
            &op,
            &source,
            &mut state,
            &context,
            Some(&soroban_data),
            None,
        );
        assert!(result.is_ok());

        match result.unwrap() {
            OperationResult::OpInner(OperationResultTr::ExtendFootprintTtl(r)) => {
                assert!(
                    matches!(r, ExtendFootprintTtlResult::ResourceLimitExceeded),
                    "Zero disk_read_bytes should reject any read, got {:?}",
                    r
                );
            }
            _ => panic!("Unexpected result type"),
        }
    }
}
