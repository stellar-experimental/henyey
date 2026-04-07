//! AccountMerge operation execution.

use stellar_xdr::curr::{
    AccountEntry, AccountEntryExt, AccountEntryExtensionV1Ext, AccountFlags, AccountId,
    AccountMergeResult, AccountMergeResultCode, LedgerKey, LedgerKeyAccount, MuxedAccount,
    OperationResult, OperationResultTr, SponsorshipDescriptor,
};

use super::{add_account_balance, require_source_account_cloned};
use crate::frame::muxed_to_account_id;
use crate::state::LedgerStateManager;
use crate::validation::LedgerContext;
use crate::{Result, TxError};

/// Execute an AccountMerge operation.
pub(crate) fn execute_account_merge(
    dest: &MuxedAccount,
    source: &AccountId,
    state: &mut LedgerStateManager,
    _context: &LedgerContext,
) -> Result<OperationResult> {
    let dest_account_id = muxed_to_account_id(dest);

    // Check destination exists
    if state.get_account(&dest_account_id).is_none() {
        return Ok(make_result(AccountMergeResultCode::NoAccount));
    }
    if &dest_account_id == source {
        return Ok(make_result(AccountMergeResultCode::Malformed));
    }

    // Get source account
    let source_account = require_source_account_cloned(state, source)?;

    // Check source is not immutable (checked first per stellar-core doApplyFromV16)
    if source_account.flags & (AccountFlags::ImmutableFlag as u32) != 0 {
        return Ok(make_result(AccountMergeResultCode::ImmutableSet));
    }

    // Check source has no sub-entries besides signers
    if source_account.num_sub_entries != source_account.signers.len() as u32 {
        return Ok(make_result(AccountMergeResultCode::HasSubEntries));
    }

    let starting_seq = state.starting_sequence_number()?;
    if let Some(&max_seq) = state.get_max_seq_num_to_apply(source) {
        if max_seq >= starting_seq {
            return Ok(make_result(AccountMergeResultCode::SeqnumTooFar));
        }
    }
    if source_account.seq_num.0 >= starting_seq {
        return Ok(make_result(AccountMergeResultCode::SeqnumTooFar));
    }

    if num_sponsoring(&source_account) > 0 {
        return Ok(make_result(AccountMergeResultCode::IsSponsor));
    }

    let source_balance = source_account.balance;

    // Transfer balance to destination
    let dest_acc = state
        .get_account_mut(&dest_account_id)
        .ok_or_else(|| TxError::Internal("destination account disappeared".into()))?;
    if !add_account_balance(dest_acc, source_balance) {
        return Ok(make_result(AccountMergeResultCode::DestFull));
    }

    if let Some(sponsored_signers) = signer_sponsoring_ids(&source_account) {
        for sponsor in sponsored_signers {
            state.update_num_sponsoring(&sponsor, -1)?;
        }
    }

    let ledger_key = LedgerKey::Account(LedgerKeyAccount {
        account_id: source.clone(),
    });
    if state.entry_sponsor(&ledger_key).is_some() {
        state.remove_entry_sponsorship_and_update_counts(&ledger_key, source, 2)?;
    }

    // Flush ALL account changes EXCEPT the source being deleted, before recording deletion.
    // stellar-core records all pending account STATE/UPDATED pairs
    // (e.g., destination balance update, any accounts from previous ops in multi-op tx)
    // before the source deletion.
    // The source account should NOT be flushed as STATE/UPDATED since it's about to be deleted.
    state.flush_all_accounts_except(Some(source));

    // Delete source account
    state.delete_account(source);

    Ok(OperationResult::OpInner(OperationResultTr::AccountMerge(
        AccountMergeResult::Success(source_balance),
    )))
}

fn make_result(code: AccountMergeResultCode) -> OperationResult {
    let result = match code {
        AccountMergeResultCode::Success => unreachable!("success handled in execute_account_merge"),
        AccountMergeResultCode::Malformed => AccountMergeResult::Malformed,
        AccountMergeResultCode::NoAccount => AccountMergeResult::NoAccount,
        AccountMergeResultCode::ImmutableSet => AccountMergeResult::ImmutableSet,
        AccountMergeResultCode::HasSubEntries => AccountMergeResult::HasSubEntries,
        AccountMergeResultCode::SeqnumTooFar => AccountMergeResult::SeqnumTooFar,
        AccountMergeResultCode::DestFull => AccountMergeResult::DestFull,
        AccountMergeResultCode::IsSponsor => AccountMergeResult::IsSponsor,
    };
    OperationResult::OpInner(OperationResultTr::AccountMerge(result))
}

fn num_sponsoring(account: &AccountEntry) -> i64 {
    match &account.ext {
        AccountEntryExt::V0 => 0,
        AccountEntryExt::V1(v1) => match &v1.ext {
            AccountEntryExtensionV1Ext::V0 => 0,
            AccountEntryExtensionV1Ext::V2(v2) => v2.num_sponsoring as i64,
        },
    }
}

fn signer_sponsoring_ids(account: &AccountEntry) -> Option<Vec<AccountId>> {
    match &account.ext {
        AccountEntryExt::V0 => None,
        AccountEntryExt::V1(v1) => match &v1.ext {
            AccountEntryExtensionV1Ext::V0 => None,
            AccountEntryExtensionV1Ext::V2(v2) => {
                let mut sponsors = Vec::new();
                for descriptor in v2.signer_sponsoring_i_ds.iter() {
                    if let SponsorshipDescriptor(Some(id)) = descriptor {
                        sponsors.push(id.clone());
                    }
                }
                Some(sponsors)
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::create_test_account_id;
    use stellar_xdr::curr::*;

    fn create_test_muxed_account(seed: u8) -> MuxedAccount {
        MuxedAccount::Ed25519(Uint256([seed; 32]))
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

    fn create_test_account_with_liabilities(
        account_id: AccountId,
        balance: i64,
        buying: i64,
        selling: i64,
    ) -> AccountEntry {
        let mut entry = create_test_account(account_id, balance);
        entry.ext = AccountEntryExt::V1(AccountEntryExtensionV1 {
            liabilities: Liabilities { buying, selling },
            ext: AccountEntryExtensionV1Ext::V0,
        });
        entry
    }

    fn create_test_account_with_sponsoring(
        account_id: AccountId,
        balance: i64,
        num_sponsoring: u32,
    ) -> AccountEntry {
        let mut entry = create_test_account(account_id, balance);
        entry.ext = AccountEntryExt::V1(AccountEntryExtensionV1 {
            liabilities: Liabilities {
                buying: 0,
                selling: 0,
            },
            ext: AccountEntryExtensionV1Ext::V2(AccountEntryExtensionV2 {
                num_sponsored: 0,
                num_sponsoring,
                signer_sponsoring_i_ds: vec![].try_into().unwrap(),
                ext: AccountEntryExtensionV2Ext::V0,
            }),
        });
        entry
    }

    fn create_test_context() -> LedgerContext {
        LedgerContext::testnet(1, 1000)
    }

    #[test]
    fn test_account_merge_success() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(0);
        let dest_id = create_test_account_id(1);

        state.create_account(create_test_account(source_id.clone(), 100_000_000));
        state.create_account(create_test_account(dest_id.clone(), 50_000_000));

        let result = execute_account_merge(
            &create_test_muxed_account(1),
            &source_id,
            &mut state,
            &context,
        );
        let result = result.expect("account merge");
        match result {
            OperationResult::OpInner(OperationResultTr::AccountMerge(
                AccountMergeResult::Success(amount),
            )) => {
                assert_eq!(amount, 100_000_000);
            }
            other => panic!("unexpected result: {:?}", other),
        }

        // Source should be gone
        assert!(state.get_account(&source_id).is_none());

        // Dest should have combined balance
        assert_eq!(state.get_account(&dest_id).unwrap().balance, 150_000_000);
    }

    #[test]
    fn test_account_merge_malformed_self() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(0);
        state.create_account(create_test_account(source_id.clone(), 100_000_000));

        let result = execute_account_merge(
            &create_test_muxed_account(0),
            &source_id,
            &mut state,
            &context,
        )
        .unwrap();

        match result {
            OperationResult::OpInner(OperationResultTr::AccountMerge(r)) => {
                assert!(matches!(r, AccountMergeResult::Malformed));
            }
            other => panic!("unexpected result: {:?}", other),
        }
    }

    #[test]
    fn test_account_merge_dest_full() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(0);
        let dest_id = create_test_account_id(1);

        state.create_account(create_test_account(source_id.clone(), 100));
        state.create_account(create_test_account_with_liabilities(
            dest_id.clone(),
            i64::MAX - 50,
            60,
            0,
        ));

        let result = execute_account_merge(
            &create_test_muxed_account(1),
            &source_id,
            &mut state,
            &context,
        )
        .unwrap();

        match result {
            OperationResult::OpInner(OperationResultTr::AccountMerge(r)) => {
                assert!(matches!(r, AccountMergeResult::DestFull));
            }
            other => panic!("unexpected result: {:?}", other),
        }
    }

    #[test]
    fn test_account_merge_seqnum_too_far() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(7);
        let dest_id = create_test_account_id(8);

        let starting_seq = state.starting_sequence_number().unwrap();
        let mut source_entry = create_test_account(source_id.clone(), 100_000_000);
        source_entry.seq_num = SequenceNumber(starting_seq);
        state.create_account(source_entry);
        state.create_account(create_test_account(dest_id.clone(), 50_000_000));

        let result = execute_account_merge(
            &create_test_muxed_account(8),
            &source_id,
            &mut state,
            &context,
        )
        .unwrap();

        match result {
            OperationResult::OpInner(OperationResultTr::AccountMerge(r)) => {
                assert!(matches!(r, AccountMergeResult::SeqnumTooFar));
            }
            other => panic!("unexpected result: {:?}", other),
        }
    }

    #[test]
    fn test_account_merge_is_sponsor() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let mut context = create_test_context();
        context.protocol_version = 19;

        let source_id = create_test_account_id(3);
        let dest_id = create_test_account_id(4);

        state.create_account(create_test_account_with_sponsoring(
            source_id.clone(),
            100_000_000,
            1,
        ));
        state.create_account(create_test_account(dest_id.clone(), 50_000_000));

        let result = execute_account_merge(
            &create_test_muxed_account(4),
            &source_id,
            &mut state,
            &context,
        )
        .unwrap();

        match result {
            OperationResult::OpInner(OperationResultTr::AccountMerge(r)) => {
                assert!(matches!(r, AccountMergeResult::IsSponsor));
            }
            other => panic!("unexpected result: {:?}", other),
        }
    }

    /// Test AccountMerge fails with NoAccount when destination doesn't exist.
    ///
    /// C++ Reference: MergeTests.cpp - "no destination" test section
    #[test]
    fn test_account_merge_no_destination() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(0);

        state.create_account(create_test_account(source_id.clone(), 100_000_000));

        // Destination (seed=1) doesn't exist
        let result = execute_account_merge(
            &create_test_muxed_account(1),
            &source_id,
            &mut state,
            &context,
        )
        .unwrap();

        match result {
            OperationResult::OpInner(OperationResultTr::AccountMerge(r)) => {
                assert!(
                    matches!(r, AccountMergeResult::NoAccount),
                    "Expected NoAccount, got {:?}",
                    r
                );
            }
            other => panic!("unexpected result: {:?}", other),
        }
    }

    /// Test AccountMerge fails with HasSubEntries when source has subentries (not signers).
    ///
    /// C++ Reference: MergeTests.cpp - "has sub entries" test section
    #[test]
    fn test_account_merge_has_subentries() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(0);
        let dest_id = create_test_account_id(1);

        // Source has 1 sub-entry (e.g., trustline, offer, or data entry)
        let mut source = create_test_account(source_id.clone(), 100_000_000);
        source.num_sub_entries = 1;
        state.create_account(source);
        state.create_account(create_test_account(dest_id.clone(), 50_000_000));

        let result = execute_account_merge(
            &create_test_muxed_account(1),
            &source_id,
            &mut state,
            &context,
        )
        .unwrap();

        match result {
            OperationResult::OpInner(OperationResultTr::AccountMerge(r)) => {
                assert!(
                    matches!(r, AccountMergeResult::HasSubEntries),
                    "Expected HasSubEntries, got {:?}",
                    r
                );
            }
            other => panic!("unexpected result: {:?}", other),
        }
    }

    /// Test AccountMerge fails with ImmutableSet when source has AUTH_IMMUTABLE flag.
    ///
    /// C++ Reference: MergeTests.cpp - "auth immutable" test section
    #[test]
    fn test_account_merge_auth_immutable() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(0);
        let dest_id = create_test_account_id(1);

        // Source has AUTH_IMMUTABLE_FLAG (0x4) set
        let mut source = create_test_account(source_id.clone(), 100_000_000);
        source.flags = 0x4; // AUTH_IMMUTABLE_FLAG
        state.create_account(source);
        state.create_account(create_test_account(dest_id.clone(), 50_000_000));

        let result = execute_account_merge(
            &create_test_muxed_account(1),
            &source_id,
            &mut state,
            &context,
        )
        .unwrap();

        match result {
            OperationResult::OpInner(OperationResultTr::AccountMerge(r)) => {
                assert!(
                    matches!(r, AccountMergeResult::ImmutableSet),
                    "Expected ImmutableSet, got {:?}",
                    r
                );
            }
            other => panic!("unexpected result: {:?}", other),
        }
    }

    /// Test AccountMerge succeeds when source has signers (signers are a special case).
    ///
    /// Signers count as subentries but don't prevent merge - they're cleared during merge.
    ///
    /// C++ Reference: MergeTests.cpp - "with signers" test section
    #[test]
    fn test_account_merge_with_signers() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(0);
        let dest_id = create_test_account_id(1);
        let signer_id = create_test_account_id(2);

        // Source has 1 signer (which counts as subentry but is allowed for merge)
        let mut source = create_test_account(source_id.clone(), 100_000_000);
        let signer = Signer {
            key: SignerKey::Ed25519(match signer_id.0 {
                PublicKey::PublicKeyTypeEd25519(k) => k,
            }),
            weight: 1,
        };
        source.signers = vec![signer].try_into().unwrap();
        source.num_sub_entries = 1; // 1 signer
        state.create_account(source);
        state.create_account(create_test_account(dest_id.clone(), 50_000_000));

        let result = execute_account_merge(
            &create_test_muxed_account(1),
            &source_id,
            &mut state,
            &context,
        )
        .unwrap();

        match result {
            OperationResult::OpInner(OperationResultTr::AccountMerge(
                AccountMergeResult::Success(amount),
            )) => {
                assert_eq!(amount, 100_000_000);
            }
            other => panic!("Expected Success with signers allowed, got {:?}", other),
        }

        // Source should be deleted
        assert!(state.get_account(&source_id).is_none());
    }

    /// Test AccountMerge with destination buying liabilities near overflow.
    ///
    /// max_receive = INT64_MAX - dest.balance - dest.buyingLiabilities
    /// If source.balance > max_receive, should return DestFull.
    ///
    /// C++ Reference: MergeTests.cpp - "destination buying liabilities" test section
    #[test]
    fn test_account_merge_dest_buying_liabilities_overflow() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(0);
        let dest_id = create_test_account_id(1);

        // Source has 1000 balance
        state.create_account(create_test_account(source_id.clone(), 1000));
        // Dest has balance near max with buying liabilities
        // INT64_MAX - 500 balance + 600 buying liabilities
        // max_receive = INT64_MAX - (INT64_MAX - 500) - 600 = -100 (overflow scenario)
        state.create_account(create_test_account_with_liabilities(
            dest_id.clone(),
            i64::MAX - 500,
            600,
            0,
        ));

        let result = execute_account_merge(
            &create_test_muxed_account(1),
            &source_id,
            &mut state,
            &context,
        )
        .unwrap();

        match result {
            OperationResult::OpInner(OperationResultTr::AccountMerge(r)) => {
                assert!(
                    matches!(r, AccountMergeResult::DestFull),
                    "Expected DestFull due to buying liabilities, got {:?}",
                    r
                );
            }
            other => panic!("unexpected result: {:?}", other),
        }
    }

    /// Test AccountMerge succeeds when source has zero balance.
    ///
    /// C++ Reference: MergeTests.cpp - "zero balance" test section
    #[test]
    fn test_account_merge_zero_balance() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(0);
        let dest_id = create_test_account_id(1);

        state.create_account(create_test_account(source_id.clone(), 0));
        state.create_account(create_test_account(dest_id.clone(), 50_000_000));

        let result = execute_account_merge(
            &create_test_muxed_account(1),
            &source_id,
            &mut state,
            &context,
        )
        .unwrap();

        match result {
            OperationResult::OpInner(OperationResultTr::AccountMerge(
                AccountMergeResult::Success(amount),
            )) => {
                assert_eq!(amount, 0);
            }
            other => panic!("Expected Success with zero balance, got {:?}", other),
        }

        // Source should be deleted even with zero balance
        assert!(state.get_account(&source_id).is_none());
        // Dest balance unchanged
        assert_eq!(state.get_account(&dest_id).unwrap().balance, 50_000_000);
    }

    /// Test AccountMerge with source account sponsored.
    ///
    /// When a sponsored source account is merged, the sponsorship is released.
    ///
    /// C++ Reference: MergeTests.cpp - "source is sponsored" test section
    #[test]
    fn test_account_merge_source_is_sponsored() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(0);
        let dest_id = create_test_account_id(1);
        let sponsor_id = create_test_account_id(2);

        // Source account (sponsored entry)
        // Create source with num_sponsored = 2 (account = 2 base entries)
        let mut source = create_test_account(source_id.clone(), 100_000_000);
        source.ext = AccountEntryExt::V1(AccountEntryExtensionV1 {
            liabilities: Liabilities {
                buying: 0,
                selling: 0,
            },
            ext: AccountEntryExtensionV1Ext::V2(AccountEntryExtensionV2 {
                num_sponsored: 2, // Account itself counts as 2 entries when sponsored
                num_sponsoring: 0,
                signer_sponsoring_i_ds: vec![].try_into().unwrap(),
                ext: AccountEntryExtensionV2Ext::V0,
            }),
        });
        state.create_account(source);

        // Sponsor has num_sponsoring = 2 to match the sponsored account
        state.create_account(create_test_account_with_sponsoring(
            sponsor_id.clone(),
            500_000_000,
            2, // Sponsoring 2 entries (the source account)
        ));

        // Destination
        state.create_account(create_test_account(dest_id.clone(), 50_000_000));

        // Mark source account as sponsored by sponsor
        let ledger_key = LedgerKey::Account(LedgerKeyAccount {
            account_id: source_id.clone(),
        });
        state.set_entry_sponsor(ledger_key, sponsor_id.clone());

        let result = execute_account_merge(
            &create_test_muxed_account(1),
            &source_id,
            &mut state,
            &context,
        )
        .unwrap();

        match result {
            OperationResult::OpInner(OperationResultTr::AccountMerge(
                AccountMergeResult::Success(amount),
            )) => {
                assert_eq!(amount, 100_000_000);
            }
            other => panic!("Expected Success, got {:?}", other),
        }

        // Source should be deleted
        assert!(state.get_account(&source_id).is_none());
    }

    /// Test that ImmutableSet is checked before HasSubEntries.
    ///
    /// stellar-core checks ImmutableSet first in doApplyFromV16.
    /// When both conditions are true, result should be ImmutableSet.
    ///
    /// C++ Reference: MergeOpFrame.cpp:209-223 (ImmutableSet before HasSubEntries)
    #[test]
    fn test_account_merge_immutable_checked_before_has_sub_entries() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(40);
        let dest_id = create_test_account_id(41);

        // Source has both AUTH_IMMUTABLE flag AND sub-entries
        let mut source = create_test_account(source_id.clone(), 100_000_000);
        source.flags = 0x4; // AUTH_IMMUTABLE_FLAG
        source.num_sub_entries = 1; // Has sub-entries too
        state.create_account(source);
        state.create_account(create_test_account(dest_id.clone(), 50_000_000));

        let result = execute_account_merge(
            &create_test_muxed_account(41),
            &source_id,
            &mut state,
            &context,
        )
        .unwrap();

        match result {
            OperationResult::OpInner(OperationResultTr::AccountMerge(r)) => {
                assert!(
                    matches!(r, AccountMergeResult::ImmutableSet),
                    "Expected ImmutableSet (checked before HasSubEntries), got {:?}",
                    r
                );
            }
            other => panic!("unexpected result: {:?}", other),
        }
    }

    /// Test that ImmutableSet is checked before SeqnumTooFar.
    ///
    /// When both conditions are true, result should be ImmutableSet.
    ///
    /// C++ Reference: MergeOpFrame.cpp:209-229 (ImmutableSet before SeqnumTooFar)
    #[test]
    fn test_account_merge_immutable_checked_before_seqnum_too_far() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(42);
        let dest_id = create_test_account_id(43);

        let starting_seq = state.starting_sequence_number().unwrap();
        let mut source = create_test_account(source_id.clone(), 100_000_000);
        source.flags = 0x4; // AUTH_IMMUTABLE_FLAG
        source.seq_num = SequenceNumber(starting_seq); // SeqnumTooFar condition
        state.create_account(source);
        state.create_account(create_test_account(dest_id.clone(), 50_000_000));

        let result = execute_account_merge(
            &create_test_muxed_account(43),
            &source_id,
            &mut state,
            &context,
        )
        .unwrap();

        match result {
            OperationResult::OpInner(OperationResultTr::AccountMerge(r)) => {
                assert!(
                    matches!(r, AccountMergeResult::ImmutableSet),
                    "Expected ImmutableSet (checked before SeqnumTooFar), got {:?}",
                    r
                );
            }
            other => panic!("unexpected result: {:?}", other),
        }
    }

    /// Test AccountMerge with sequence number just below the limit.
    ///
    /// Source seqNum must be < starting_seq for merge to succeed.
    /// This tests the boundary: seqNum == starting_seq - 1 should succeed.
    ///
    /// C++ Reference: MergeTests.cpp - "seqnum boundary" test section
    #[test]
    fn test_account_merge_seqnum_at_boundary() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(0);
        let dest_id = create_test_account_id(1);

        let starting_seq = state.starting_sequence_number().unwrap();
        let mut source = create_test_account(source_id.clone(), 100_000_000);
        // Exactly one below the starting sequence - should succeed
        source.seq_num = SequenceNumber(starting_seq - 1);
        state.create_account(source);
        state.create_account(create_test_account(dest_id.clone(), 50_000_000));

        let result = execute_account_merge(
            &create_test_muxed_account(1),
            &source_id,
            &mut state,
            &context,
        )
        .unwrap();

        match result {
            OperationResult::OpInner(OperationResultTr::AccountMerge(
                AccountMergeResult::Success(_),
            )) => {
                // Success is expected when seqnum is below starting_seq
            }
            other => panic!("Expected Success at seqnum boundary, got {:?}", other),
        }
    }

    /// Test that AccountMerge is blocked by MAX_SEQ_NUM_TO_APPLY even when
    /// source.seq_num < starting_seq.
    ///
    /// stellar-core's MergeOpFrame::isSeqnumTooFar checks the per-account max
    /// sequence number from the tx set against starting_seq. This prevents
    /// merging an account when another transaction in the same set would
    /// advance the sequence number past the boundary.
    ///
    /// C++ Reference: MergeOpFrame.cpp:36-56 (isSeqnumTooFar / MAX_SEQ_NUM_TO_APPLY)
    #[test]
    fn test_account_merge_blocked_by_max_seq_num_to_apply() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(10);
        let dest_id = create_test_account_id(11);

        // Source seq_num is well below starting_seq — would normally succeed
        let source = create_test_account(source_id.clone(), 100_000_000);
        assert!(source.seq_num.0 < state.starting_sequence_number().unwrap());
        state.create_account(source);
        state.create_account(create_test_account(dest_id.clone(), 50_000_000));

        // Set max_seq_num_to_apply for this source to >= starting_seq
        let starting_seq = state.starting_sequence_number().unwrap();
        let mut map = std::collections::HashMap::new();
        map.insert(source_id.clone(), starting_seq);
        state.set_max_seq_num_to_apply(map);

        let result = execute_account_merge(
            &create_test_muxed_account(11),
            &source_id,
            &mut state,
            &context,
        )
        .unwrap();

        match result {
            OperationResult::OpInner(OperationResultTr::AccountMerge(r)) => {
                assert!(
                    matches!(r, AccountMergeResult::SeqnumTooFar),
                    "Expected SeqnumTooFar due to max_seq_num_to_apply, got {:?}",
                    r
                );
            }
            other => panic!("unexpected result: {:?}", other),
        }
    }

    /// Test that max_seq_num_to_apply does NOT block merge when the max
    /// sequence number is below starting_seq.
    #[test]
    fn test_account_merge_allowed_when_max_seq_below_starting() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(12);
        let dest_id = create_test_account_id(13);

        let source = create_test_account(source_id.clone(), 100_000_000);
        state.create_account(source);
        state.create_account(create_test_account(dest_id.clone(), 50_000_000));

        // Set max_seq_num_to_apply below starting_seq — merge should succeed
        let starting_seq = state.starting_sequence_number().unwrap();
        let mut map = std::collections::HashMap::new();
        map.insert(source_id.clone(), starting_seq - 1);
        state.set_max_seq_num_to_apply(map);

        let result = execute_account_merge(
            &create_test_muxed_account(13),
            &source_id,
            &mut state,
            &context,
        )
        .unwrap();

        match result {
            OperationResult::OpInner(OperationResultTr::AccountMerge(
                AccountMergeResult::Success(amount),
            )) => {
                assert_eq!(amount, 100_000_000);
            }
            other => panic!(
                "Expected Success when max_seq < starting_seq, got {:?}",
                other
            ),
        }
    }
}
