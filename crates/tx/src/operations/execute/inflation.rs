//! Inflation operation execution.
//!
//! This module implements the execution logic for the Inflation operation.
//! Note: Inflation has been deprecated since Protocol 12 and always returns
//! NOT_TIME on the public network, but we implement it for completeness.

use stellar_xdr::curr::{
    AccountId, InflationPayout, InflationResult, InflationResultCode, OperationResult,
    OperationResultTr,
};

use crate::state::LedgerStateManager;
use crate::validation::LedgerContext;
use crate::Result;

/// Execute an Inflation operation.
///
/// This operation was used to distribute inflation payments but has been
/// deprecated since Protocol 12. On the public network, it always returns NOT_TIME.
///
/// For completeness, we implement the basic logic:
/// 1. Check if it's time for inflation (weekly)
/// 2. Calculate inflation pool
/// 3. Distribute to accounts that have received votes
pub(crate) fn execute_inflation(
    source: &AccountId,
    state: &mut LedgerStateManager,
    _context: &LedgerContext,
) -> Result<OperationResult> {
    // Check source account exists
    if state.get_account(source).is_none() {
        return Ok(make_inflation_result(InflationResultCode::NotTime, vec![]));
    }

    // Inflation is deprecated - on modern networks, always return NOT_TIME
    // The last inflation payout was in 2019. Since Protocol 12, inflation
    // is effectively disabled.
    //
    // If we were to implement it fully:
    // 1. Check if a week has passed since last inflation
    // 2. Calculate 1% annual inflation on total coins
    // 3. Find all accounts with inflation_dest set and enough votes (0.05% of total)
    // 4. Distribute proportionally based on votes

    // For now, return NOT_TIME as inflation is deprecated
    Ok(make_inflation_result(InflationResultCode::NotTime, vec![]))
}

/// Create an Inflation result.
fn make_inflation_result(
    code: InflationResultCode,
    payouts: Vec<InflationPayout>,
) -> OperationResult {
    let result = match code {
        InflationResultCode::Success => {
            InflationResult::Success(payouts.try_into().unwrap_or_default())
        }
        InflationResultCode::NotTime => InflationResult::NotTime,
    };

    OperationResult::OpInner(OperationResultTr::Inflation(result))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::create_test_account_id;
    use stellar_xdr::curr::*;

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

    fn create_test_context() -> LedgerContext {
        LedgerContext::testnet(1, 1000)
    }

    #[test]
    fn test_inflation_not_time() {
        let mut state = LedgerStateManager::new(5_000_000, 100.into());
        let context = create_test_context();

        let source_id = create_test_account_id(0);
        state.create_account(create_test_account(source_id.clone(), 100_000_000));

        let result = execute_inflation(&source_id, &mut state, &context);
        assert!(result.is_ok());

        match result.unwrap() {
            OperationResult::OpInner(OperationResultTr::Inflation(r)) => {
                assert!(matches!(r, InflationResult::NotTime));
            }
            _ => panic!("Unexpected result type"),
        }
    }

    /// Test inflation with source account that doesn't exist.
    ///
    /// C++ Reference: InflationTests.cpp - "no account" section
    #[test]
    fn test_inflation_no_source_account() {
        let mut state = LedgerStateManager::new(5_000_000, 100.into());
        let context = create_test_context();

        let source_id = create_test_account_id(1);
        // Don't create the account

        let result = execute_inflation(&source_id, &mut state, &context);
        assert!(result.is_ok());

        // Even without source account, returns NOT_TIME (deprecated operation)
        match result.unwrap() {
            OperationResult::OpInner(OperationResultTr::Inflation(r)) => {
                assert!(matches!(r, InflationResult::NotTime));
            }
            _ => panic!("Unexpected result type"),
        }
    }

    /// Test inflation with account that has inflation destination set.
    /// Still returns NOT_TIME since inflation is deprecated.
    ///
    /// C++ Reference: InflationTests.cpp - "with inflation dest" section
    #[test]
    fn test_inflation_with_inflation_dest() {
        let mut state = LedgerStateManager::new(5_000_000, 100.into());
        let context = create_test_context();

        let source_id = create_test_account_id(2);
        let dest_id = create_test_account_id(3);

        // Create accounts, source has inflation_dest set
        let mut source_account = create_test_account(source_id.clone(), 100_000_000);
        source_account.inflation_dest = Some(dest_id.clone());
        state.create_account(source_account);
        state.create_account(create_test_account(dest_id.clone(), 100_000_000));

        let result = execute_inflation(&source_id, &mut state, &context);
        assert!(result.is_ok());

        // Still returns NOT_TIME because inflation is deprecated
        match result.unwrap() {
            OperationResult::OpInner(OperationResultTr::Inflation(r)) => {
                assert!(matches!(r, InflationResult::NotTime));
            }
            _ => panic!("Unexpected result type"),
        }
    }

    /// Test make_inflation_result with success code.
    #[test]
    fn test_make_inflation_result_success() {
        let payout = InflationPayout {
            destination: create_test_account_id(4),
            amount: 1000000,
        };
        let result = make_inflation_result(InflationResultCode::Success, vec![payout]);

        match result {
            OperationResult::OpInner(OperationResultTr::Inflation(r)) => match r {
                InflationResult::Success(payouts) => {
                    assert_eq!(payouts.len(), 1);
                    assert_eq!(payouts[0].amount, 1000000);
                }
                _ => panic!("Expected Success"),
            },
            _ => panic!("Unexpected result type"),
        }
    }

    /// Test make_inflation_result with NOT_TIME code.
    #[test]
    fn test_make_inflation_result_not_time() {
        let result = make_inflation_result(InflationResultCode::NotTime, vec![]);

        match result {
            OperationResult::OpInner(OperationResultTr::Inflation(r)) => {
                assert!(matches!(r, InflationResult::NotTime));
            }
            _ => panic!("Unexpected result type"),
        }
    }

    /// Test inflation with multiple accounts.
    #[test]
    fn test_inflation_multiple_accounts() {
        let mut state = LedgerStateManager::new(5_000_000, 100.into());
        let context = create_test_context();

        let source_id = create_test_account_id(5);
        let other_id1 = create_test_account_id(6);
        let other_id2 = create_test_account_id(7);

        state.create_account(create_test_account(source_id.clone(), 100_000_000));
        state.create_account(create_test_account(other_id1, 50_000_000));
        state.create_account(create_test_account(other_id2, 200_000_000));

        let result = execute_inflation(&source_id, &mut state, &context);
        assert!(result.is_ok());

        // Inflation is deprecated, always returns NOT_TIME
        match result.unwrap() {
            OperationResult::OpInner(OperationResultTr::Inflation(r)) => {
                assert!(matches!(r, InflationResult::NotTime));
            }
            _ => panic!("Unexpected result type"),
        }
    }

    /// Test inflation with different ledger contexts.
    #[test]
    fn test_inflation_different_context() {
        let mut state = LedgerStateManager::new(5_000_000, 100.into());

        // Create context with different ledger parameters
        let context = LedgerContext::new(
            1000,       // ledger sequence
            2000000000, // close time (far in the future)
            100,        // base fee
            5_000_000,  // base reserve
            25,         // protocol version
            henyey_common::NetworkId::testnet(),
        );

        let source_id = create_test_account_id(8);
        state.create_account(create_test_account(source_id.clone(), 100_000_000));

        let result = execute_inflation(&source_id, &mut state, &context);
        assert!(result.is_ok());

        // Still NOT_TIME regardless of context
        match result.unwrap() {
            OperationResult::OpInner(OperationResultTr::Inflation(r)) => {
                assert!(matches!(r, InflationResult::NotTime));
            }
            _ => panic!("Unexpected result type"),
        }
    }
}
