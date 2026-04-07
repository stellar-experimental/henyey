//! ManageData operation execution.
//!
//! This module implements the execution logic for the ManageData operation,
//! which allows accounts to attach arbitrary key-value data.

use stellar_xdr::curr::{
    AccountId, DataEntry, DataEntryExt, LedgerKey, LedgerKeyData, ManageDataOp, ManageDataResult,
    ManageDataResultCode, OperationResult, OperationResultTr,
};

use super::{account_balance_after_liabilities, require_source_account, ACCOUNT_SUBENTRY_LIMIT};
use crate::state::LedgerStateManager;
use crate::validation::LedgerContext;
use crate::{Result, TxError};

/// Maximum length for data names.
const MAX_DATA_NAME_LENGTH: usize = 64;

/// Maximum length for data values.
const MAX_DATA_VALUE_LENGTH: usize = 64;

/// Execute a ManageData operation.
///
/// This operation creates, updates, or removes a data entry. Setting the value
/// to None removes the entry.
pub(crate) fn execute_manage_data(
    op: &ManageDataOp,
    source: &AccountId,
    state: &mut LedgerStateManager,
    context: &LedgerContext,
) -> Result<OperationResult> {
    // Validate data name
    let data_name = op.data_name.to_string();
    if data_name.is_empty() || data_name.len() > MAX_DATA_NAME_LENGTH {
        return Ok(make_result(ManageDataResultCode::InvalidName));
    }
    let data_name_bytes: &Vec<u8> =
        <stellar_xdr::curr::String64 as AsRef<Vec<u8>>>::as_ref(&op.data_name);
    if !is_string_valid(data_name_bytes) {
        return Ok(make_result(ManageDataResultCode::InvalidName));
    }

    // Validate data value if present
    if let Some(value) = &op.data_value {
        if value.len() > MAX_DATA_VALUE_LENGTH {
            return Ok(make_result(ManageDataResultCode::InvalidName));
        }
    }

    // Check source account exists
    require_source_account(state, source)?;

    // Check if data entry exists
    let existing_entry = state.get_data(source, &data_name);
    let sponsor = state.active_sponsor_for(source);

    match &op.data_value {
        None => {
            // Deleting data entry
            if existing_entry.is_some() {
                let ledger_key = LedgerKey::Data(LedgerKeyData {
                    account_id: source.clone(),
                    data_name: op.data_name.clone(),
                });
                if state.entry_sponsor(&ledger_key).is_some() {
                    state.remove_entry_sponsorship_and_update_counts(&ledger_key, source, 1)?;
                }

                // Decrease sub-entry count before deleting
                if let Some(account) = state.get_account_mut(source) {
                    if account.num_sub_entries > 0 {
                        account.num_sub_entries -= 1;
                    }
                }

                state.delete_data(source, &data_name);
            } else {
                return Ok(make_result(ManageDataResultCode::NameNotFound));
            }
        }
        Some(value) => {
            if existing_entry.is_some() {
                // Update existing entry
                if let Some(entry) = state.get_data_mut(source, &data_name) {
                    entry.data_value = value.clone();
                }
            } else {
                let source_account = require_source_account(state, source)?;

                // Enforce subentry limits before adding the new entry.
                let (num_sponsoring, _num_sponsored) = state
                    .sponsorship_counts_for_account(source)
                    .unwrap_or((0, 0));
                if source_account.num_sub_entries >= ACCOUNT_SUBENTRY_LIMIT
                    || source_account.num_sub_entries.saturating_add(1) > ACCOUNT_SUBENTRY_LIMIT
                {
                    return Ok(OperationResult::OpTooManySubentries);
                }
                let total = source_account.num_sub_entries as u64 + num_sponsoring as u64 + 1;
                if total > u32::MAX as u64 {
                    return Ok(OperationResult::OpTooManySubentries);
                }

                // Check source can afford new sub-entry
                if let Some(sponsor) = &sponsor {
                    let sponsor_account = state
                        .get_account(sponsor)
                        .ok_or(TxError::SourceAccountNotFound)?;
                    let new_min_balance = state.minimum_balance_for_account_with_deltas(
                        sponsor_account,
                        context.protocol_version,
                        0,
                        1,
                        0,
                    )?;
                    let available = account_balance_after_liabilities(sponsor_account);
                    if available < new_min_balance {
                        return Ok(make_result(ManageDataResultCode::LowReserve));
                    }
                } else {
                    let new_min_balance = state.minimum_balance_for_account(
                        source_account,
                        context.protocol_version,
                        1,
                    )?;
                    let available = account_balance_after_liabilities(source_account);
                    if available < new_min_balance {
                        return Ok(make_result(ManageDataResultCode::LowReserve));
                    }
                }

                // Create new data entry
                let new_entry = DataEntry {
                    account_id: source.clone(),
                    data_name: op.data_name.clone(),
                    data_value: value.clone(),
                    ext: DataEntryExt::V0,
                };

                if let Some(sponsor) = sponsor {
                    let ledger_key = LedgerKey::Data(LedgerKeyData {
                        account_id: source.clone(),
                        data_name: op.data_name.clone(),
                    });
                    state.apply_entry_sponsorship_with_sponsor(
                        ledger_key,
                        &sponsor,
                        Some(source),
                        1,
                    )?;
                }
                tracing::debug!(
                    "ManageData: creating data entry, account={:?}, name={:?}",
                    source,
                    data_name
                );
                state.create_data(new_entry);

                // Increase sub-entry count
                if let Some(account) = state.get_account_mut(source) {
                    account.num_sub_entries += 1;
                }
            }
        }
    }

    Ok(make_result(ManageDataResultCode::Success))
}

/// Create an OperationResult from a ManageDataResultCode.
fn make_result(code: ManageDataResultCode) -> OperationResult {
    let result = match code {
        ManageDataResultCode::Success => ManageDataResult::Success,
        ManageDataResultCode::NotSupportedYet => ManageDataResult::NotSupportedYet,
        ManageDataResultCode::NameNotFound => ManageDataResult::NameNotFound,
        ManageDataResultCode::LowReserve => ManageDataResult::LowReserve,
        ManageDataResultCode::InvalidName => ManageDataResult::InvalidName,
    };

    OperationResult::OpInner(OperationResultTr::ManageData(result))
}

fn is_string_valid(bytes: &[u8]) -> bool {
    bytes
        .iter()
        .all(|byte| byte.is_ascii() && !byte.is_ascii_control())
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

    fn with_selling_liabilities(mut account: AccountEntry, selling: i64) -> AccountEntry {
        account.ext = AccountEntryExt::V1(AccountEntryExtensionV1 {
            liabilities: Liabilities { buying: 0, selling },
            ext: AccountEntryExtensionV1Ext::V0,
        });
        account
    }

    fn make_string64(s: &str) -> String64 {
        String64::try_from(s.as_bytes().to_vec()).unwrap()
    }

    #[test]
    fn test_manage_data_create_success() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(0);
        state.create_account(create_test_account(source_id.clone(), 100_000_000));

        let op = ManageDataOp {
            data_name: make_string64("test_key"),
            data_value: Some(vec![1, 2, 3, 4].try_into().unwrap()),
        };

        let result = execute_manage_data(&op, &source_id, &mut state, &context);
        assert!(result.is_ok());

        // Verify data entry was created
        let entry = state.get_data(&source_id, "test_key");
        assert!(entry.is_some());
        assert_eq!(entry.unwrap().data_value.as_slice(), &[1, 2, 3, 4]);

        // Verify sub-entries increased
        assert_eq!(state.get_account(&source_id).unwrap().num_sub_entries, 1);
    }

    #[test]
    fn test_manage_data_low_reserve_with_selling_liabilities() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let mut context = create_test_context();
        context.protocol_version = 25;

        let source_id = create_test_account_id(0);
        let mut account = create_test_account(source_id.clone(), 0);
        let new_min_balance = state
            .minimum_balance_for_account(&account, context.protocol_version, 1)
            .expect("min balance");
        let selling_liabilities = 1_000_000;
        account.balance = new_min_balance + selling_liabilities - 1;
        account = with_selling_liabilities(account, selling_liabilities);
        state.create_account(account);

        let op = ManageDataOp {
            data_name: make_string64("test_key"),
            data_value: Some(vec![1, 2, 3, 4].try_into().unwrap()),
        };

        let result =
            execute_manage_data(&op, &source_id, &mut state, &context).expect("manage data result");

        match result {
            OperationResult::OpInner(OperationResultTr::ManageData(
                ManageDataResult::LowReserve,
            )) => {}
            other => panic!("unexpected result: {:?}", other),
        }
    }

    #[test]
    fn test_manage_data_invalid_name_non_ascii() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(0);
        state.create_account(create_test_account(source_id.clone(), 100_000_000));

        let op = ManageDataOp {
            data_name: String64::try_from(vec![0x80]).unwrap(),
            data_value: Some(vec![1, 2].try_into().unwrap()),
        };

        let result =
            execute_manage_data(&op, &source_id, &mut state, &context).expect("manage data result");

        match result {
            OperationResult::OpInner(OperationResultTr::ManageData(r)) => {
                assert!(matches!(r, ManageDataResult::InvalidName));
            }
            other => panic!("unexpected result: {:?}", other),
        }
    }

    #[test]
    fn test_manage_data_invalid_name_control() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(0);
        state.create_account(create_test_account(source_id.clone(), 100_000_000));

        let op = ManageDataOp {
            data_name: String64::try_from(vec![0x1b]).unwrap(),
            data_value: Some(vec![1, 2].try_into().unwrap()),
        };

        let result =
            execute_manage_data(&op, &source_id, &mut state, &context).expect("manage data result");

        match result {
            OperationResult::OpInner(OperationResultTr::ManageData(r)) => {
                assert!(matches!(r, ManageDataResult::InvalidName));
            }
            other => panic!("unexpected result: {:?}", other),
        }
    }

    #[test]
    fn test_manage_data_delete_success() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(0);
        let mut account = create_test_account(source_id.clone(), 100_000_000);
        account.num_sub_entries = 1;
        state.create_account(account);

        // Create initial data entry
        let initial_entry = DataEntry {
            account_id: source_id.clone(),
            data_name: make_string64("test_key"),
            data_value: vec![1, 2, 3, 4].try_into().unwrap(),
            ext: DataEntryExt::V0,
        };
        state.create_data(initial_entry);

        // Delete the entry
        let op = ManageDataOp {
            data_name: make_string64("test_key"),
            data_value: None,
        };

        let result = execute_manage_data(&op, &source_id, &mut state, &context);
        assert!(result.is_ok());

        // Verify data entry was removed
        assert!(state.get_data(&source_id, "test_key").is_none());

        // Verify sub-entries decreased
        assert_eq!(state.get_account(&source_id).unwrap().num_sub_entries, 0);
    }

    #[test]
    fn test_manage_data_delete_missing() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(0);
        state.create_account(create_test_account(source_id.clone(), 100_000_000));

        let op = ManageDataOp {
            data_name: make_string64("missing_key"),
            data_value: None,
        };

        let result =
            execute_manage_data(&op, &source_id, &mut state, &context).expect("manage data");

        match result {
            OperationResult::OpInner(OperationResultTr::ManageData(r)) => {
                assert!(matches!(r, ManageDataResult::NameNotFound));
            }
            other => panic!("unexpected result: {:?}", other),
        }
    }

    #[test]
    fn test_manage_data_low_reserve() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(0);
        // Source only has minimum balance
        state.create_account(create_test_account(source_id.clone(), 10_000_000));

        let op = ManageDataOp {
            data_name: make_string64("test_key"),
            data_value: Some(vec![1, 2, 3, 4].try_into().unwrap()),
        };

        let result = execute_manage_data(&op, &source_id, &mut state, &context);
        assert!(result.is_ok());

        match result.unwrap() {
            OperationResult::OpInner(OperationResultTr::ManageData(r)) => {
                assert!(matches!(r, ManageDataResult::LowReserve));
            }
            _ => panic!("Unexpected result type"),
        }
    }

    #[test]
    fn test_manage_data_max_value_length() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(8);
        state.create_account(create_test_account(source_id.clone(), 100_000_000));

        let op = ManageDataOp {
            data_name: make_string64("test_key"),
            data_value: Some(vec![1u8; MAX_DATA_VALUE_LENGTH].try_into().unwrap()),
        };

        let result =
            execute_manage_data(&op, &source_id, &mut state, &context).expect("manage data");

        match result {
            OperationResult::OpInner(OperationResultTr::ManageData(r)) => {
                assert!(matches!(r, ManageDataResult::Success));
            }
            other => panic!("unexpected result: {:?}", other),
        }
    }

    #[test]
    fn test_manage_data_empty_name() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(9);
        state.create_account(create_test_account(source_id.clone(), 100_000_000));

        let op = ManageDataOp {
            data_name: String64::try_from(Vec::<u8>::new()).unwrap(),
            data_value: Some(vec![1, 2, 3].try_into().unwrap()),
        };

        let result =
            execute_manage_data(&op, &source_id, &mut state, &context).expect("manage data");

        match result {
            OperationResult::OpInner(OperationResultTr::ManageData(r)) => {
                assert!(matches!(r, ManageDataResult::InvalidName));
            }
            other => panic!("unexpected result: {:?}", other),
        }
    }

    #[test]
    fn test_manage_data_update_existing() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(10);
        state.create_account(create_test_account(source_id.clone(), 100_000_000));

        let op = ManageDataOp {
            data_name: make_string64("test_key"),
            data_value: Some(vec![1, 2, 3].try_into().unwrap()),
        };

        let result =
            execute_manage_data(&op, &source_id, &mut state, &context).expect("manage data");

        match result {
            OperationResult::OpInner(OperationResultTr::ManageData(r)) => {
                assert!(matches!(r, ManageDataResult::Success));
            }
            other => panic!("unexpected result: {:?}", other),
        }

        let initial_sub_entries = state.get_account(&source_id).unwrap().num_sub_entries;

        let update_op = ManageDataOp {
            data_name: make_string64("test_key"),
            data_value: Some(vec![9, 9, 9].try_into().unwrap()),
        };

        let result =
            execute_manage_data(&update_op, &source_id, &mut state, &context).expect("manage data");

        match result {
            OperationResult::OpInner(OperationResultTr::ManageData(r)) => {
                assert!(matches!(r, ManageDataResult::Success));
            }
            other => panic!("unexpected result: {:?}", other),
        }

        let entry = state.get_data(&source_id, "test_key").unwrap();
        assert_eq!(entry.data_value.as_slice(), &[9, 9, 9]);
        assert_eq!(
            state.get_account(&source_id).unwrap().num_sub_entries,
            initial_sub_entries
        );
    }

    #[test]
    fn test_manage_data_sponsorship_success() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let sponsor_id = create_test_account_id(11);
        let source_id = create_test_account_id(12);
        state.create_account(create_test_account(sponsor_id.clone(), 100_000_000));
        state.create_account(create_test_account(source_id.clone(), 10_000_000));

        state.push_sponsorship(sponsor_id.clone(), source_id.clone());

        let op = ManageDataOp {
            data_name: make_string64("test_key"),
            data_value: Some(vec![1, 2, 3, 4].try_into().unwrap()),
        };

        let result =
            execute_manage_data(&op, &source_id, &mut state, &context).expect("manage data");

        match result {
            OperationResult::OpInner(OperationResultTr::ManageData(r)) => {
                assert!(matches!(r, ManageDataResult::Success));
            }
            other => panic!("unexpected result: {:?}", other),
        }

        let key = LedgerKey::Data(LedgerKeyData {
            account_id: source_id.clone(),
            data_name: op.data_name.clone(),
        });
        assert_eq!(state.entry_sponsor(&key).as_ref(), Some(&sponsor_id));

        let counts = state.sponsorship_counts_for_account(&sponsor_id).unwrap();
        assert_eq!(counts.0, 1);
        let counts = state.sponsorship_counts_for_account(&source_id).unwrap();
        assert_eq!(counts.1, 1);
    }

    #[test]
    fn test_manage_data_sponsorship_low_reserve() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let sponsor_id = create_test_account_id(13);
        let source_id = create_test_account_id(14);
        state.create_account(create_test_account(sponsor_id.clone(), 10_000_000));
        state.create_account(create_test_account(source_id.clone(), 100_000_000));

        state.push_sponsorship(sponsor_id.clone(), source_id.clone());

        let op = ManageDataOp {
            data_name: make_string64("test_key"),
            data_value: Some(vec![1, 2, 3, 4].try_into().unwrap()),
        };

        let result =
            execute_manage_data(&op, &source_id, &mut state, &context).expect("manage data");

        match result {
            OperationResult::OpInner(OperationResultTr::ManageData(r)) => {
                assert!(matches!(r, ManageDataResult::LowReserve));
            }
            other => panic!("unexpected result: {:?}", other),
        }
    }

    /// Test that ManageData returns OpTooManySubentries when account has reached
    /// the maximum subentries limit (1000).
    ///
    /// C++ Reference: ManageDataTests.cpp - tooManySubentries tests via SponsorshipTestUtils
    #[test]
    fn test_manage_data_too_many_subentries() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(15);

        // Create source account with max subentries (1000)
        let mut source_account = create_test_account(source_id.clone(), 100_000_000);
        source_account.num_sub_entries = ACCOUNT_SUBENTRY_LIMIT; // At the limit
        state.create_account(source_account);

        let op = ManageDataOp {
            data_name: make_string64("new_key"),
            data_value: Some(vec![1, 2, 3, 4].try_into().unwrap()),
        };

        let result = execute_manage_data(&op, &source_id, &mut state, &context);
        match result.unwrap() {
            OperationResult::OpTooManySubentries => {
                // Expected - account has max subentries, can't create new data entry
            }
            other => panic!("expected OpTooManySubentries, got {:?}", other),
        }

        // Verify num_sub_entries was not changed
        assert_eq!(
            state.get_account(&source_id).unwrap().num_sub_entries,
            ACCOUNT_SUBENTRY_LIMIT,
            "num_sub_entries should remain unchanged"
        );
    }

    /// Test that updating existing data entry works even when at subentry limit.
    /// Updating doesn't create a new subentry, so it should succeed.
    #[test]
    fn test_manage_data_update_at_subentry_limit_succeeds() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(16);

        // Create source account with max subentries
        let mut source_account = create_test_account(source_id.clone(), 100_000_000);
        source_account.num_sub_entries = ACCOUNT_SUBENTRY_LIMIT;
        state.create_account(source_account);

        // Create existing data entry
        let existing_entry = DataEntry {
            account_id: source_id.clone(),
            data_name: make_string64("existing_key"),
            data_value: vec![1, 2, 3].try_into().unwrap(),
            ext: DataEntryExt::V0,
        };
        state.create_data(existing_entry);

        // Update existing data entry - should succeed even at limit
        let op = ManageDataOp {
            data_name: make_string64("existing_key"),
            data_value: Some(vec![4, 5, 6, 7, 8].try_into().unwrap()),
        };

        let result = execute_manage_data(&op, &source_id, &mut state, &context);
        match result.unwrap() {
            OperationResult::OpInner(OperationResultTr::ManageData(r)) => {
                assert!(matches!(r, ManageDataResult::Success));
            }
            other => panic!("expected Success, got {:?}", other),
        }

        // Verify the value was updated
        let entry = state.get_data(&source_id, "existing_key").unwrap();
        assert_eq!(entry.data_value.as_slice(), &[4, 5, 6, 7, 8]);
    }
}
