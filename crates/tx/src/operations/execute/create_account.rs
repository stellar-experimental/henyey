//! CreateAccount operation execution.

use stellar_xdr::curr::{
    AccountEntry, AccountEntryExt, AccountId, CreateAccountOp, CreateAccountResult,
    CreateAccountResultCode, LedgerKey, LedgerKeyAccount, OperationResult, OperationResultTr,
    SequenceNumber, String32, Thresholds,
};

use super::{
    account_balance_after_liabilities, account_liabilities, require_source_account,
    sub_account_balance,
};
use crate::state::LedgerStateManager;
use crate::validation::LedgerContext;
use crate::{Result, TxError};

/// Execute a CreateAccount operation.
pub(crate) fn execute_create_account(
    op: &CreateAccountOp,
    source: &AccountId,
    state: &mut LedgerStateManager,
    context: &LedgerContext,
) -> Result<OperationResult> {
    let sponsor = state.active_sponsor_for(&op.destination);
    let account_multiplier = 2i64;

    // Reject negative starting balance. Matches stellar-core's doCheckValid:
    // startingBalance < 0 returns MALFORMED (minStartingBalance = 0 for protocol >= 14).
    if op.starting_balance < 0 {
        return Ok(make_result(CreateAccountResultCode::Malformed));
    }

    // Check destination doesn't already exist (matches upstream doApply)
    if state.get_account(&op.destination).is_some() {
        return Ok(make_result(CreateAccountResultCode::AlreadyExist));
    }

    // Check sponsorship / reserve constraints first (matches upstream
    // createEntryWithPossibleSponsorship which runs before the source
    // available-balance check).
    if let Some(sponsor) = &sponsor {
        // Sponsored path: verify sponsor can afford the new account's reserve.
        let sponsor_account = state
            .get_account(sponsor)
            .ok_or(TxError::SourceAccountNotFound)?;
        let (num_sponsoring, num_sponsored) = state
            .sponsorship_counts_for_account(sponsor)
            .unwrap_or((0, 0));
        let sponsor_min_balance = state.minimum_balance_with_counts(
            context.protocol_version,
            sponsor_account.num_sub_entries as i64,
            num_sponsoring + account_multiplier,
            num_sponsored,
        )?;
        let available = account_balance_after_liabilities(sponsor_account);
        if available < sponsor_min_balance {
            return Ok(make_result(CreateAccountResultCode::LowReserve));
        }
    } else {
        // Non-sponsored path: starting balance must meet minimum reserve.
        let min_balance = state.minimum_balance_with_counts(context.protocol_version, 0, 0, 0)?;
        if op.starting_balance < min_balance {
            return Ok(make_result(CreateAccountResultCode::LowReserve));
        }
    }

    // Get source account and check balance
    let source_account = require_source_account(state, source)?;

    // Check source has sufficient available balance.
    // If source is the sponsor, its numSponsoring has already been incremented
    // by createEntryWithPossibleSponsorship in stellar-core before this check,
    // so we must include the sponsoring delta in the minimum balance.
    let sponsoring_delta = if sponsor.as_ref() == Some(source) {
        account_multiplier
    } else {
        0
    };
    let source_min_balance = state.minimum_balance_for_account_with_deltas(
        source_account,
        context.protocol_version,
        0,
        sponsoring_delta,
        0,
    )?;
    let available = (source_account.balance - source_min_balance)
        .saturating_sub(account_liabilities(source_account).selling);
    if available < op.starting_balance {
        return Ok(make_result(CreateAccountResultCode::Underfunded));
    }

    // Deduct starting_balance from source. Always call get_account_mut even
    // when starting_balance==0 so the source is tracked as modified, matching
    // stellar-core's unconditional loadAccount in doApplyFromV14
    // (CreateAccountOpFrame.cpp:120). See #1093 / AUDIT-020.
    let source_account = state
        .get_account_mut(source)
        .ok_or(TxError::SourceAccountNotFound)?;
    sub_account_balance(source_account, op.starting_balance)?;

    let starting_seq = state.starting_sequence_number()?;

    // Create new account
    let new_account = AccountEntry {
        account_id: op.destination.clone(),
        balance: op.starting_balance,
        seq_num: SequenceNumber(starting_seq),
        num_sub_entries: 0,
        inflation_dest: None,
        flags: 0,
        home_domain: String32::default(),
        thresholds: Thresholds([1, 0, 0, 0]),
        signers: vec![].try_into().unwrap(),
        ext: AccountEntryExt::V0,
    };

    let mut new_account = new_account;
    if let Some(sponsor) = sponsor {
        let ledger_key = LedgerKey::Account(LedgerKeyAccount {
            account_id: op.destination.clone(),
        });
        state.set_entry_sponsor(ledger_key, sponsor.clone());
        state.apply_account_entry_sponsorship(&mut new_account, &sponsor, account_multiplier)?;
    }

    state.create_account(new_account);

    Ok(make_result(CreateAccountResultCode::Success))
}

fn make_result(code: CreateAccountResultCode) -> OperationResult {
    let result = match code {
        CreateAccountResultCode::Success => CreateAccountResult::Success,
        CreateAccountResultCode::Malformed => CreateAccountResult::Malformed,
        CreateAccountResultCode::Underfunded => CreateAccountResult::Underfunded,
        CreateAccountResultCode::LowReserve => CreateAccountResult::LowReserve,
        CreateAccountResultCode::AlreadyExist => CreateAccountResult::AlreadyExist,
    };
    OperationResult::OpInner(OperationResultTr::CreateAccount(result))
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
    fn test_create_account_success() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(0);
        let dest_id = create_test_account_id(1);

        state.create_account(create_test_account(source_id.clone(), 100_000_000));

        let op = CreateAccountOp {
            destination: dest_id.clone(),
            starting_balance: 20_000_000,
        };

        let result = execute_create_account(&op, &source_id, &mut state, &context);
        assert!(result.is_ok());

        // Verify destination was created
        let dest_acc = state.get_account(&dest_id);
        assert!(dest_acc.is_some());
        let dest_acc = dest_acc.unwrap();
        assert_eq!(dest_acc.balance, 20_000_000);
        assert_eq!(dest_acc.seq_num.0, (100_i64) << 32);

        // Verify source was deducted
        assert_eq!(state.get_account(&source_id).unwrap().balance, 80_000_000);
    }

    #[test]
    fn test_create_account_already_exists() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(0);
        let dest_id = create_test_account_id(1);

        state.create_account(create_test_account(source_id.clone(), 100_000_000));
        state.create_account(create_test_account(dest_id.clone(), 50_000_000));

        let op = CreateAccountOp {
            destination: dest_id.clone(),
            starting_balance: 20_000_000,
        };

        let result = execute_create_account(&op, &source_id, &mut state, &context);
        assert!(result.is_ok());

        match result.unwrap() {
            OperationResult::OpInner(OperationResultTr::CreateAccount(r)) => {
                assert!(matches!(r, CreateAccountResult::AlreadyExist));
            }
            _ => panic!("Unexpected result type"),
        }
    }

    #[test]
    fn test_create_account_low_reserve() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(0);
        let dest_id = create_test_account_id(1);

        state.create_account(create_test_account(source_id.clone(), 100_000_000));

        let op = CreateAccountOp {
            destination: dest_id.clone(),
            starting_balance: 5_000_000, // Below minimum balance of 10M
        };

        let result = execute_create_account(&op, &source_id, &mut state, &context);
        assert!(result.is_ok());

        match result.unwrap() {
            OperationResult::OpInner(OperationResultTr::CreateAccount(r)) => {
                assert!(matches!(r, CreateAccountResult::LowReserve));
            }
            _ => panic!("Unexpected result type"),
        }
    }

    /// Test CreateAccount fails with Underfunded when source doesn't have enough balance.
    ///
    /// C++ Reference: CreateAccountTests.cpp - "underfunded" test section
    #[test]
    fn test_create_account_underfunded() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(0);
        let dest_id = create_test_account_id(1);

        // Source has exactly minimum balance (10M) - no available balance to give
        state.create_account(create_test_account(source_id.clone(), 10_000_000));

        let op = CreateAccountOp {
            destination: dest_id.clone(),
            starting_balance: 20_000_000, // More than source can provide
        };

        let result = execute_create_account(&op, &source_id, &mut state, &context);
        assert!(result.is_ok());

        match result.unwrap() {
            OperationResult::OpInner(OperationResultTr::CreateAccount(r)) => {
                assert!(matches!(r, CreateAccountResult::Underfunded));
            }
            _ => panic!("Unexpected result type"),
        }
    }

    /// Test CreateAccount respects selling liabilities when checking source balance.
    ///
    /// C++ Reference: CreateAccountTests.cpp - "with native selling liabilities" test section
    #[test]
    fn test_create_account_underfunded_with_selling_liabilities() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(0);
        let dest_id = create_test_account_id(1);

        // Create source with 50M balance but 30M selling liabilities
        // min_balance = 10M, available = (50M - 10M) - 30M = 10M
        let mut source_account = create_test_account(source_id.clone(), 50_000_000);
        source_account.ext = AccountEntryExt::V1(AccountEntryExtensionV1 {
            liabilities: Liabilities {
                buying: 0,
                selling: 30_000_000, // Selling liabilities reduce available balance
            },
            ext: AccountEntryExtensionV1Ext::V0,
        });
        state.create_account(source_account);

        let op = CreateAccountOp {
            destination: dest_id.clone(),
            starting_balance: 15_000_000, // More than available 10M
        };

        let result = execute_create_account(&op, &source_id, &mut state, &context);
        assert!(result.is_ok());

        match result.unwrap() {
            OperationResult::OpInner(OperationResultTr::CreateAccount(r)) => {
                assert!(
                    matches!(r, CreateAccountResult::Underfunded),
                    "Should be Underfunded due to selling liabilities, got {:?}",
                    r
                );
            }
            _ => panic!("Unexpected result type"),
        }
    }

    /// Test CreateAccount succeeds when source has selling liabilities but enough available.
    #[test]
    fn test_create_account_success_with_selling_liabilities() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(0);
        let dest_id = create_test_account_id(1);

        // Create source with 100M balance but 30M selling liabilities
        // min_balance = 10M, available = (100M - 10M) - 30M = 60M
        let mut source_account = create_test_account(source_id.clone(), 100_000_000);
        source_account.ext = AccountEntryExt::V1(AccountEntryExtensionV1 {
            liabilities: Liabilities {
                buying: 0,
                selling: 30_000_000,
            },
            ext: AccountEntryExtensionV1Ext::V0,
        });
        state.create_account(source_account);

        let op = CreateAccountOp {
            destination: dest_id.clone(),
            starting_balance: 20_000_000, // Less than available 60M
        };

        let result = execute_create_account(&op, &source_id, &mut state, &context);
        assert!(result.is_ok());

        match result.unwrap() {
            OperationResult::OpInner(OperationResultTr::CreateAccount(r)) => {
                assert!(matches!(r, CreateAccountResult::Success));
            }
            _ => panic!("Unexpected result type"),
        }

        // Verify destination was created
        assert!(state.get_account(&dest_id).is_some());
        assert_eq!(state.get_account(&dest_id).unwrap().balance, 20_000_000);
    }

    /// Test CreateAccount with sponsorship - sponsor pays reserve.
    ///
    /// C++ Reference: CreateAccountTests.cpp - "with sponsorship" test section
    #[test]
    fn test_create_account_with_sponsorship() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(0);
        let dest_id = create_test_account_id(1);
        let sponsor_id = create_test_account_id(2);

        // Source needs to provide the starting balance
        state.create_account(create_test_account(source_id.clone(), 100_000_000));
        // Sponsor needs to have enough for the reserve
        state.create_account(create_test_account(sponsor_id.clone(), 100_000_000));

        // Set up sponsorship: sponsor will pay reserve for the new account
        state.push_sponsorship(sponsor_id.clone(), dest_id.clone());

        // With sponsorship, starting balance can be 0 (sponsor pays reserve)
        let op = CreateAccountOp {
            destination: dest_id.clone(),
            starting_balance: 0, // Sponsor pays reserve, source gives 0
        };

        let result = execute_create_account(&op, &source_id, &mut state, &context);
        assert!(result.is_ok());

        match result.unwrap() {
            OperationResult::OpInner(OperationResultTr::CreateAccount(r)) => {
                assert!(matches!(r, CreateAccountResult::Success), "got {:?}", r);
            }
            other => panic!("Unexpected result type: {:?}", other),
        }

        // Verify destination was created with 0 balance
        let dest_acc = state.get_account(&dest_id);
        assert!(dest_acc.is_some());
        assert_eq!(dest_acc.unwrap().balance, 0);
    }

    /// Test CreateAccount with sponsor having insufficient balance.
    #[test]
    fn test_create_account_sponsor_low_reserve() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(0);
        let dest_id = create_test_account_id(1);
        let sponsor_id = create_test_account_id(2);

        state.create_account(create_test_account(source_id.clone(), 100_000_000));
        // Sponsor has minimum balance only - can't afford to sponsor
        state.create_account(create_test_account(sponsor_id.clone(), 10_000_000));

        state.push_sponsorship(sponsor_id.clone(), dest_id.clone());

        let op = CreateAccountOp {
            destination: dest_id.clone(),
            starting_balance: 0,
        };

        let result = execute_create_account(&op, &source_id, &mut state, &context);
        assert!(result.is_ok());

        match result.unwrap() {
            OperationResult::OpInner(OperationResultTr::CreateAccount(r)) => {
                assert!(
                    matches!(r, CreateAccountResult::LowReserve),
                    "Should fail with LowReserve, got {:?}",
                    r
                );
            }
            other => panic!("Unexpected result: {:?}", other),
        }
    }

    /// Regression test: when both sponsor is underfunded AND source is
    /// underfunded, we must return LowReserve (sponsor check first), not
    /// Underfunded (source check). Matches upstream stellar-core which runs
    /// createEntryWithPossibleSponsorship before the source balance check.
    ///
    /// Reproduces mainnet mismatch at ledger 61232072.
    #[test]
    fn test_create_account_sponsor_low_reserve_before_underfunded() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(0);
        let dest_id = create_test_account_id(1);
        let sponsor_id = create_test_account_id(2);

        // Source has minimum balance only — also can't afford starting_balance
        state.create_account(create_test_account(source_id.clone(), 10_000_000));
        // Sponsor has minimum balance only — can't afford to sponsor
        state.create_account(create_test_account(sponsor_id.clone(), 10_000_000));

        state.push_sponsorship(sponsor_id.clone(), dest_id.clone());

        let op = CreateAccountOp {
            destination: dest_id.clone(),
            starting_balance: 20_000_000, // source can't afford this either
        };

        let result = execute_create_account(&op, &source_id, &mut state, &context);
        assert!(result.is_ok());

        match result.unwrap() {
            OperationResult::OpInner(OperationResultTr::CreateAccount(r)) => {
                assert!(
                    matches!(r, CreateAccountResult::LowReserve),
                    "Must return LowReserve (sponsor checked first), not Underfunded; got {:?}",
                    r
                );
            }
            other => panic!("Unexpected result: {:?}", other),
        }
    }

    /// Test CreateAccount with negative starting balance returns Malformed.
    ///
    /// stellar-core rejects startingBalance < 0 as MALFORMED in doCheckValid.
    #[test]
    fn test_create_account_negative_balance() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(10);
        let dest_id = create_test_account_id(11);

        state.create_account(create_test_account(source_id.clone(), 100_000_000));

        let op = CreateAccountOp {
            destination: dest_id,
            starting_balance: -1,
        };

        let result = execute_create_account(&op, &source_id, &mut state, &context).unwrap();

        match result {
            OperationResult::OpInner(OperationResultTr::CreateAccount(r)) => {
                assert!(
                    matches!(r, CreateAccountResult::Malformed),
                    "Expected Malformed for negative balance, got {:?}",
                    r
                );
            }
            other => panic!("Unexpected result: {:?}", other),
        }
    }

    /// Test CreateAccount where destination is the source account returns AlreadyExist.
    ///
    /// When trying to create an account that already exists (including self), returns AlreadyExist.
    #[test]
    fn test_create_account_self_destination() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(12);

        state.create_account(create_test_account(source_id.clone(), 100_000_000));

        let op = CreateAccountOp {
            destination: source_id.clone(), // Same as source - already exists
            starting_balance: 10_000_000,
        };

        let result = execute_create_account(&op, &source_id, &mut state, &context).unwrap();

        match result {
            OperationResult::OpInner(OperationResultTr::CreateAccount(r)) => {
                // Source account already exists, so creating it again returns AlreadyExist
                assert!(
                    matches!(r, CreateAccountResult::AlreadyExist),
                    "Expected AlreadyExist when dest==source, got {:?}",
                    r
                );
            }
            other => panic!("Unexpected result: {:?}", other),
        }
    }

    /// Test CreateAccount with zero starting balance returns LowReserve (without sponsorship).
    ///
    /// Without sponsorship, the minimum balance is typically > 0, so zero balance fails.
    #[test]
    fn test_create_account_zero_balance_fails_without_sponsorship() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(13);
        let dest_id = create_test_account_id(14);

        state.create_account(create_test_account(source_id.clone(), 100_000_000));

        let op = CreateAccountOp {
            destination: dest_id.clone(),
            starting_balance: 0,
        };

        let result = execute_create_account(&op, &source_id, &mut state, &context).unwrap();

        match result {
            OperationResult::OpInner(OperationResultTr::CreateAccount(r)) => {
                // Without sponsorship, minimum balance > 0, so zero balance fails
                assert!(
                    matches!(r, CreateAccountResult::LowReserve),
                    "Zero balance without sponsorship should fail with LowReserve, got {:?}",
                    r
                );
            }
            other => panic!("Unexpected result: {:?}", other),
        }
    }

    /// Test CreateAccount returns Underfunded when source is also the sponsor.
    ///
    /// Regression test for ledger 59616305 mismatch: stellar-core increments
    /// the sponsor's numSponsoring via createEntryWithPossibleSponsorship BEFORE
    /// checking getAvailableBalance on the source. When source == sponsor, the
    /// increased numSponsoring raises the minimum balance, reducing available
    /// balance. Our old code checked balance before incrementing numSponsoring.
    #[test]
    fn test_create_account_underfunded_when_source_is_sponsor() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(20);
        let dest_id = create_test_account_id(21);

        // Source balance: 20M
        // Base reserve: 5M, so min_balance without sponsoring = (2 + 0 + 0 - 0) * 5M = 10M
        // Available without sponsoring: (20M - 10M) - 0 = 10M
        //
        // But if source is also the sponsor, numSponsoring increases by 2 (account_multiplier):
        // min_balance with sponsoring = (2 + 0 + 2 - 0) * 5M = 20M
        // Available with sponsoring: (20M - 20M) - 0 = 0
        // So starting_balance of 5M should fail (Underfunded)
        state.create_account(create_test_account(source_id.clone(), 20_000_000));

        // Set up sponsorship: source sponsors the new account (dest)
        state.push_sponsorship(source_id.clone(), dest_id.clone());

        let op = CreateAccountOp {
            destination: dest_id.clone(),
            starting_balance: 5_000_000,
        };

        let result = execute_create_account(&op, &source_id, &mut state, &context).unwrap();

        match result {
            OperationResult::OpInner(OperationResultTr::CreateAccount(r)) => {
                assert!(
                    matches!(r, CreateAccountResult::Underfunded),
                    "Source-as-sponsor should be Underfunded due to increased min balance, got {:?}",
                    r
                );
            }
            other => panic!("Unexpected result: {:?}", other),
        }
    }

    /// Regression test for AUDIT-C5: negative starting_balance must be rejected
    /// as Malformed, not silently processed. In the sponsored path, a negative
    /// starting_balance would pass the sponsor reserve check and the underfunded
    /// check (negative < balance), and the deduction becomes an addition to the
    /// source balance, effectively minting tokens.
    #[test]
    fn test_audit_c5_negative_starting_balance_malformed() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(30);
        let dest_id = create_test_account_id(31);

        state.create_account(create_test_account(source_id.clone(), 100_000_000));

        // Sponsored path: source sponsors the new account
        state.push_sponsorship(source_id.clone(), dest_id.clone());

        let op = CreateAccountOp {
            destination: dest_id,
            starting_balance: -1_000_000,
        };

        let result = execute_create_account(&op, &source_id, &mut state, &context).unwrap();

        match result {
            OperationResult::OpInner(OperationResultTr::CreateAccount(r)) => {
                // stellar-core returns MALFORMED for startingBalance < 0
                assert!(
                    matches!(r, CreateAccountResult::Malformed),
                    "Negative starting_balance must be Malformed, got {:?}",
                    r
                );
            }
            other => panic!("Unexpected result: {:?}", other),
        }
    }

    /// Regression test for AUDIT-020 / #1093: CreateAccount with starting_balance=0
    /// (sponsored path) must track the source account as modified.
    ///
    /// stellar-core unconditionally calls `loadAccount(ltx, getSourceID())` in
    /// doApplyFromV14, which marks the source as mutably loaded. On commit,
    /// maybeUpdateLastModified bumps lastModifiedLedgerSeq. Henyey was skipping
    /// get_account_mut when starting_balance==0, so the source was never tracked
    /// as modified — causing a lastModifiedLedgerSeq divergence (consensus-critical
    /// when op source != tx source).
    #[test]
    fn test_audit_020_zero_balance_sponsored_tracks_source_modified() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(0);
        let dest_id = create_test_account_id(1);
        let sponsor_id = create_test_account_id(2);

        // Source account (will be the operation source, distinct from sponsor)
        state.create_account(create_test_account(source_id.clone(), 100_000_000));
        // Sponsor pays reserve for the new account
        state.create_account(create_test_account(sponsor_id.clone(), 100_000_000));

        // Set up sponsorship
        state.push_sponsorship(sponsor_id.clone(), dest_id.clone());

        // Enable per-op snapshot tracking (mirrors what the executor does)
        state.begin_op_snapshot();

        let op = CreateAccountOp {
            destination: dest_id.clone(),
            starting_balance: 0,
        };

        let result = execute_create_account(&op, &source_id, &mut state, &context);
        assert!(result.is_ok());
        match result.unwrap() {
            OperationResult::OpInner(OperationResultTr::CreateAccount(r)) => {
                assert!(matches!(r, CreateAccountResult::Success), "got {:?}", r);
            }
            other => panic!("Unexpected result: {:?}", other),
        }

        // Flush modified entries to the delta — this is where the bug manifests.
        // Without the fix, the source account is not in modified_accounts,
        // so no update is recorded and lastModifiedLedgerSeq is never bumped.
        state.flush_modified_entries();

        // The source account MUST appear in updated_entries, matching stellar-core's
        // unconditional loadAccount which causes maybeUpdateLastModified on commit.
        let has_source_update = state.delta().updated_entries().iter().any(|entry| {
            if let LedgerEntryData::Account(acc) = &entry.data {
                acc.account_id == source_id
            } else {
                false
            }
        });
        assert!(
            has_source_update,
            "Source account must be tracked as modified even when starting_balance=0 \
             (sponsored path). stellar-core unconditionally loads source mutably in \
             doApplyFromV14 (CreateAccountOpFrame.cpp:120)."
        );
    }
}
