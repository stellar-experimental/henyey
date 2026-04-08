//! Sponsorship operation execution.
//!
//! This module implements the execution logic for sponsorship operations:
//! - BeginSponsoringFutureReserves
//! - EndSponsoringFutureReserves
//! - RevokeSponsorship

use stellar_xdr::curr::{
    AccountId, BeginSponsoringFutureReservesOp, BeginSponsoringFutureReservesResult,
    BeginSponsoringFutureReservesResultCode, EndSponsoringFutureReservesResult,
    EndSponsoringFutureReservesResultCode, LedgerEntryData, LedgerKey, LedgerKeyAccount,
    LedgerKeyClaimableBalance, LedgerKeyData, LedgerKeyOffer, LedgerKeyTrustLine, OperationResult,
    OperationResultTr, RevokeSponsorshipOp, RevokeSponsorshipResult, RevokeSponsorshipResultCode,
    SponsorshipDescriptor, TrustLineAsset,
};

use super::account_balance_after_liabilities;
use crate::state::LedgerStateManager;
use crate::validation::LedgerContext;
use crate::{Result, TxError};

/// Execute a BeginSponsoringFutureReserves operation.
///
/// This operation marks the beginning of a sponsorship relationship where
/// the source account will pay reserves for entries created by the sponsored account.
///
/// Note: The sponsored account does NOT need to exist at this point - it may be
/// created by a later operation in the same transaction (e.g., CreateAccount).
pub(crate) fn execute_begin_sponsoring_future_reserves(
    op: &BeginSponsoringFutureReservesOp,
    source: &AccountId,
    state: &mut LedgerStateManager,
    _context: &LedgerContext,
) -> Result<OperationResult> {
    // Check source account exists (the sponsor must exist to pay reserves)
    if state.get_account(source).is_none() {
        return Ok(make_begin_result(
            BeginSponsoringFutureReservesResultCode::Malformed,
        ));
    }

    // Note: We do NOT check if sponsored_id exists here because:
    // 1. The account may be created by a later CreateAccount operation
    // 2. stellar-core does not require the sponsored account to exist

    // Cannot sponsor yourself
    if source == &op.sponsored_id {
        return Ok(make_begin_result(
            BeginSponsoringFutureReservesResultCode::Malformed,
        ));
    }

    if state.is_sponsored(&op.sponsored_id) {
        return Ok(make_begin_result(
            BeginSponsoringFutureReservesResultCode::AlreadySponsored,
        ));
    }

    if state.is_sponsored(source) || state.is_sponsoring(&op.sponsored_id) {
        return Ok(make_begin_result(
            BeginSponsoringFutureReservesResultCode::Recursive,
        ));
    }

    state.push_sponsorship(source.clone(), op.sponsored_id.clone());

    Ok(make_begin_result(
        BeginSponsoringFutureReservesResultCode::Success,
    ))
}

/// Execute an EndSponsoringFutureReserves operation.
///
/// This operation ends a sponsorship relationship that was begun with
/// BeginSponsoringFutureReserves.
///
/// Note: In a full implementation, this would pop from the sponsorship stack.
pub(crate) fn execute_end_sponsoring_future_reserves(
    source: &AccountId,
    state: &mut LedgerStateManager,
    _context: &LedgerContext,
) -> Result<OperationResult> {
    // Check source account exists
    if state.get_account(source).is_none() {
        return Ok(make_end_result(
            EndSponsoringFutureReservesResultCode::NotSponsored,
        ));
    }

    if state.remove_sponsorship_for(source).is_none() {
        return Ok(make_end_result(
            EndSponsoringFutureReservesResultCode::NotSponsored,
        ));
    }

    Ok(make_end_result(
        EndSponsoringFutureReservesResultCode::Success,
    ))
}

/// Execute a RevokeSponsorship operation.
///
/// This operation revokes sponsorship of a ledger entry, transferring
/// the reserve responsibility back to the entry owner.
pub(crate) fn execute_revoke_sponsorship(
    op: &RevokeSponsorshipOp,
    source: &AccountId,
    state: &mut LedgerStateManager,
    context: &LedgerContext,
) -> Result<OperationResult> {
    use stellar_xdr::curr::RevokeSponsorshipOp as RSO;

    // Check source account exists
    if state.get_account(source).is_none() {
        return Ok(make_revoke_result(RevokeSponsorshipResultCode::NotSponsor));
    }

    match op {
        RSO::LedgerEntry(ledger_key) => {
            // Check if the entry exists
            let Some(entry) = state.get_entry(ledger_key) else {
                tracing::debug!(
                    "RevokeSponsorship: entry does not exist, key={:?}",
                    ledger_key
                );
                return Ok(make_revoke_result(
                    RevokeSponsorshipResultCode::DoesNotExist,
                ));
            };
            let (owner_id, multiplier) = match &entry.data {
                LedgerEntryData::Account(a) => (a.account_id.clone(), 2i64),
                LedgerEntryData::Trustline(tl) => {
                    let mult = match &tl.asset {
                        TrustLineAsset::PoolShare(_) => 2i64,
                        _ => 1i64,
                    };
                    (tl.account_id.clone(), mult)
                }
                LedgerEntryData::Offer(o) => (o.seller_id.clone(), 1i64),
                LedgerEntryData::Data(d) => (d.account_id.clone(), 1i64),
                LedgerEntryData::ClaimableBalance(cb) => {
                    let Some(sponsor) = state.entry_sponsor(ledger_key) else {
                        return Ok(make_revoke_result(RevokeSponsorshipResultCode::Malformed));
                    };
                    (sponsor, cb.claimants.len() as i64)
                }
                _ => {
                    return Ok(make_revoke_result(RevokeSponsorshipResultCode::Malformed));
                }
            };

            let current_sponsor = state.entry_sponsor(ledger_key);
            let was_sponsored = current_sponsor.is_some();

            if was_sponsored {
                if current_sponsor.as_ref() != Some(source) {
                    return Ok(make_revoke_result(RevokeSponsorshipResultCode::NotSponsor));
                }
            } else if &owner_id != source {
                return Ok(make_revoke_result(RevokeSponsorshipResultCode::NotSponsor));
            }

            let new_sponsor = state.active_sponsor_for(source);
            let will_be_sponsored = new_sponsor
                .as_ref()
                .map(|sponsor| sponsor != &owner_id)
                .unwrap_or(false);

            if matches!(entry.data, LedgerEntryData::ClaimableBalance(_)) && !will_be_sponsored {
                return Ok(make_revoke_result(
                    RevokeSponsorshipResultCode::OnlyTransferable,
                ));
            }

            let mut sponsorship_changed = false;
            if was_sponsored && will_be_sponsored {
                let new_sponsor = new_sponsor.expect("sponsor must exist");
                let new_sponsor_account = state
                    .get_account(&new_sponsor)
                    .ok_or(TxError::SourceAccountNotFound)?;
                let new_min_balance = state.minimum_balance_for_account_with_deltas(
                    new_sponsor_account,
                    context.protocol_version,
                    0,
                    multiplier,
                    0,
                )?;
                let available = account_balance_after_liabilities(new_sponsor_account);
                if available < new_min_balance {
                    return Ok(make_revoke_result(RevokeSponsorshipResultCode::LowReserve));
                }

                let old_sponsor = current_sponsor.expect("old sponsor exists");
                state.update_num_sponsoring(&old_sponsor, -multiplier)?;
                state.update_num_sponsoring(&new_sponsor, multiplier)?;
                state.set_entry_sponsor(ledger_key.clone(), new_sponsor);
                sponsorship_changed = true;
            } else if was_sponsored && !will_be_sponsored {
                if let Some(owner_account) = state.get_account(&owner_id) {
                    let new_min_balance = state.minimum_balance_for_account_with_deltas(
                        owner_account,
                        context.protocol_version,
                        0,
                        0,
                        -multiplier,
                    )?;
                    let available = account_balance_after_liabilities(owner_account);
                    if available < new_min_balance {
                        return Ok(make_revoke_result(RevokeSponsorshipResultCode::LowReserve));
                    }
                }

                if matches!(entry.data, LedgerEntryData::ClaimableBalance(_)) {
                    state.remove_entry_sponsorship_with_sponsor_counts(
                        ledger_key, None, multiplier,
                    )?;
                } else {
                    state.remove_entry_sponsorship_and_update_counts(
                        ledger_key, &owner_id, multiplier,
                    )?;
                }
                sponsorship_changed = true;
            } else if !was_sponsored && will_be_sponsored {
                let new_sponsor = new_sponsor.expect("sponsor must exist");
                let new_sponsor_account = state
                    .get_account(&new_sponsor)
                    .ok_or(TxError::SourceAccountNotFound)?;
                let new_min_balance = state.minimum_balance_for_account_with_deltas(
                    new_sponsor_account,
                    context.protocol_version,
                    0,
                    multiplier,
                    0,
                )?;
                let available = account_balance_after_liabilities(new_sponsor_account);
                if available < new_min_balance {
                    return Ok(make_revoke_result(RevokeSponsorshipResultCode::LowReserve));
                }

                state.apply_entry_sponsorship_with_sponsor(
                    ledger_key.clone(),
                    &new_sponsor,
                    Some(&owner_id),
                    multiplier,
                )?;
                sponsorship_changed = true;
            }

            if sponsorship_changed {
                update_entry_after_sponsorship(state, ledger_key)?;
            }

            Ok(make_revoke_result(RevokeSponsorshipResultCode::Success))
        }
        RSO::Signer(signer_key) => {
            // Check if the account exists
            let Some(account) = state.get_account(&signer_key.account_id) else {
                return Ok(make_revoke_result(
                    RevokeSponsorshipResultCode::DoesNotExist,
                ));
            };

            let signer_pos = account
                .signers
                .iter()
                .position(|s| s.key == signer_key.signer_key);
            let Some(pos) = signer_pos else {
                return Ok(make_revoke_result(
                    RevokeSponsorshipResultCode::DoesNotExist,
                ));
            };

            let owner_id = signer_key.account_id.clone();
            let current_sponsor = current_signer_sponsor(account, pos);
            let was_sponsored = current_sponsor.is_some();

            if was_sponsored {
                if current_sponsor.as_ref() != Some(source) {
                    return Ok(make_revoke_result(RevokeSponsorshipResultCode::NotSponsor));
                }
            } else if &owner_id != source {
                return Ok(make_revoke_result(RevokeSponsorshipResultCode::NotSponsor));
            }

            let new_sponsor = state.active_sponsor_for(source);
            let will_be_sponsored = new_sponsor
                .as_ref()
                .map(|sponsor| sponsor != &owner_id)
                .unwrap_or(false);

            if was_sponsored && will_be_sponsored {
                let new_sponsor = new_sponsor.expect("sponsor must exist");
                let new_sponsor_account = state
                    .get_account(&new_sponsor)
                    .ok_or(TxError::SourceAccountNotFound)?;
                let new_min_balance = state.minimum_balance_for_account_with_deltas(
                    new_sponsor_account,
                    context.protocol_version,
                    0,
                    1,
                    0,
                )?;
                let available = account_balance_after_liabilities(new_sponsor_account);
                if available < new_min_balance {
                    return Ok(make_revoke_result(RevokeSponsorshipResultCode::LowReserve));
                }

                let old_sponsor = current_sponsor.expect("old sponsor exists");
                state.update_num_sponsoring(&old_sponsor, -1)?;
                state.update_num_sponsoring(&new_sponsor, 1)?;
                set_signer_sponsor(state, &owner_id, pos, Some(new_sponsor))?;
            } else if was_sponsored && !will_be_sponsored {
                if let Some(owner_account) = state.get_account(&owner_id) {
                    let new_min_balance = state.minimum_balance_for_account_with_deltas(
                        owner_account,
                        context.protocol_version,
                        0,
                        0,
                        -1,
                    )?;
                    let available = account_balance_after_liabilities(owner_account);
                    if available < new_min_balance {
                        return Ok(make_revoke_result(RevokeSponsorshipResultCode::LowReserve));
                    }
                }

                let old_sponsor = current_sponsor.expect("old sponsor exists");
                state.update_num_sponsoring(&old_sponsor, -1)?;
                state.update_num_sponsored(&owner_id, -1)?;
                set_signer_sponsor(state, &owner_id, pos, None)?;
            } else if !was_sponsored && will_be_sponsored {
                let new_sponsor = new_sponsor.expect("sponsor must exist");
                let new_sponsor_account = state
                    .get_account(&new_sponsor)
                    .ok_or(TxError::SourceAccountNotFound)?;
                let new_min_balance = state.minimum_balance_for_account_with_deltas(
                    new_sponsor_account,
                    context.protocol_version,
                    0,
                    1,
                    0,
                )?;
                let available = account_balance_after_liabilities(new_sponsor_account);
                if available < new_min_balance {
                    return Ok(make_revoke_result(RevokeSponsorshipResultCode::LowReserve));
                }

                state.update_num_sponsoring(&new_sponsor, 1)?;
                state.update_num_sponsored(&owner_id, 1)?;
                set_signer_sponsor(state, &owner_id, pos, Some(new_sponsor))?;
            }

            Ok(make_revoke_result(RevokeSponsorshipResultCode::Success))
        }
    }
}

fn make_begin_result(code: BeginSponsoringFutureReservesResultCode) -> OperationResult {
    let result = match code {
        BeginSponsoringFutureReservesResultCode::Success => {
            BeginSponsoringFutureReservesResult::Success
        }
        BeginSponsoringFutureReservesResultCode::Malformed => {
            BeginSponsoringFutureReservesResult::Malformed
        }
        BeginSponsoringFutureReservesResultCode::AlreadySponsored => {
            BeginSponsoringFutureReservesResult::AlreadySponsored
        }
        BeginSponsoringFutureReservesResultCode::Recursive => {
            BeginSponsoringFutureReservesResult::Recursive
        }
    };

    OperationResult::OpInner(OperationResultTr::BeginSponsoringFutureReserves(result))
}

/// Create an EndSponsoringFutureReserves result.
fn make_end_result(code: EndSponsoringFutureReservesResultCode) -> OperationResult {
    let result = match code {
        EndSponsoringFutureReservesResultCode::Success => {
            EndSponsoringFutureReservesResult::Success
        }
        EndSponsoringFutureReservesResultCode::NotSponsored => {
            EndSponsoringFutureReservesResult::NotSponsored
        }
    };

    OperationResult::OpInner(OperationResultTr::EndSponsoringFutureReserves(result))
}

/// Create a RevokeSponsorship result.
fn make_revoke_result(code: RevokeSponsorshipResultCode) -> OperationResult {
    let result = match code {
        RevokeSponsorshipResultCode::Success => RevokeSponsorshipResult::Success,
        RevokeSponsorshipResultCode::DoesNotExist => RevokeSponsorshipResult::DoesNotExist,
        RevokeSponsorshipResultCode::NotSponsor => RevokeSponsorshipResult::NotSponsor,
        RevokeSponsorshipResultCode::LowReserve => RevokeSponsorshipResult::LowReserve,
        RevokeSponsorshipResultCode::OnlyTransferable => RevokeSponsorshipResult::OnlyTransferable,
        RevokeSponsorshipResultCode::Malformed => RevokeSponsorshipResult::Malformed,
    };

    OperationResult::OpInner(OperationResultTr::RevokeSponsorship(result))
}

fn update_entry_after_sponsorship(
    state: &mut LedgerStateManager,
    ledger_key: &LedgerKey,
) -> Result<()> {
    match ledger_key {
        LedgerKey::Account(LedgerKeyAccount { account_id }) => {
            let _ = state.get_account_mut(account_id);
        }
        LedgerKey::Trustline(LedgerKeyTrustLine { account_id, asset }) => {
            let _ = state.get_trustline_by_trustline_asset_mut(account_id, asset);
        }
        LedgerKey::Offer(LedgerKeyOffer {
            seller_id,
            offer_id,
        }) => {
            let _ = state.get_offer_mut(seller_id, *offer_id);
        }
        LedgerKey::Data(LedgerKeyData {
            account_id,
            data_name,
        }) => {
            let name = String::from_utf8_lossy(data_name.as_vec()).to_string();
            let _ = state.get_data_mut(account_id, &name);
        }
        LedgerKey::ClaimableBalance(LedgerKeyClaimableBalance { balance_id }) => {
            let _ = state.get_claimable_balance_mut(balance_id);
        }
        _ => {}
    }
    Ok(())
}

fn current_signer_sponsor(
    account: &stellar_xdr::curr::AccountEntry,
    pos: usize,
) -> Option<AccountId> {
    match &account.ext {
        stellar_xdr::curr::AccountEntryExt::V0 => None,
        stellar_xdr::curr::AccountEntryExt::V1(v1) => match &v1.ext {
            stellar_xdr::curr::AccountEntryExtensionV1Ext::V0 => None,
            stellar_xdr::curr::AccountEntryExtensionV1Ext::V2(v2) => {
                v2.signer_sponsoring_i_ds.get(pos).and_then(|id| match id {
                    SponsorshipDescriptor(Some(s)) => Some(s.clone()),
                    _ => None,
                })
            }
        },
    }
}

fn set_signer_sponsor(
    state: &mut LedgerStateManager,
    account_id: &AccountId,
    pos: usize,
    sponsor: Option<AccountId>,
) -> Result<()> {
    let account = state
        .get_account_mut(account_id)
        .ok_or(TxError::SourceAccountNotFound)?;
    let ext = crate::state::ensure_account_ext_v2(account);
    let mut sponsoring_ids: Vec<SponsorshipDescriptor> =
        ext.signer_sponsoring_i_ds.iter().cloned().collect();
    if sponsoring_ids.len() <= pos {
        return Err(TxError::Internal(
            "signer sponsoring ids out of range".to_string(),
        ));
    }
    sponsoring_ids[pos] = SponsorshipDescriptor(sponsor);
    ext.signer_sponsoring_i_ds = sponsoring_ids.try_into().unwrap_or_default();
    Ok(())
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

    fn create_data_entry(account_id: &AccountId, name: &str) -> DataEntry {
        DataEntry {
            account_id: account_id.clone(),
            data_name: String64::try_from(name.as_bytes().to_vec()).unwrap(),
            data_value: vec![1u8].try_into().unwrap(),
            ext: DataEntryExt::V0,
        }
    }

    fn create_claimable_balance_entry(
        sponsor_id: &AccountId,
        balance_id: ClaimableBalanceId,
    ) -> ClaimableBalanceEntry {
        let claimant = Claimant::ClaimantTypeV0(ClaimantV0 {
            destination: sponsor_id.clone(),
            predicate: ClaimPredicate::Unconditional,
        });
        ClaimableBalanceEntry {
            balance_id,
            claimants: vec![claimant].try_into().unwrap(),
            asset: Asset::Native,
            amount: 10,
            ext: ClaimableBalanceEntryExt::V0,
        }
    }

    fn create_account_with_signer(
        account_id: AccountId,
        signer_key: SignerKey,
        sponsor: Option<AccountId>,
    ) -> AccountEntry {
        let signers = vec![Signer {
            key: signer_key,
            weight: 1,
        }];
        let sponsoring_ids = vec![SponsorshipDescriptor(sponsor.clone())];
        AccountEntry {
            account_id,
            balance: 100_000_000,
            seq_num: SequenceNumber(1),
            num_sub_entries: 1,
            inflation_dest: None,
            flags: 0,
            home_domain: String32::default(),
            thresholds: Thresholds([1, 0, 0, 0]),
            signers: signers.try_into().unwrap(),
            ext: AccountEntryExt::V1(AccountEntryExtensionV1 {
                liabilities: Liabilities {
                    buying: 0,
                    selling: 0,
                },
                ext: AccountEntryExtensionV1Ext::V2(AccountEntryExtensionV2 {
                    num_sponsored: if sponsor.is_some() { 1 } else { 0 },
                    num_sponsoring: 0,
                    signer_sponsoring_i_ds: sponsoring_ids.try_into().unwrap(),
                    ext: AccountEntryExtensionV2Ext::V0,
                }),
            }),
        }
    }

    #[test]
    fn test_begin_sponsoring_self() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(0);
        state.create_account(create_test_account(source_id.clone(), 100_000_000));

        let op = BeginSponsoringFutureReservesOp {
            sponsored_id: source_id.clone(), // Sponsor self
        };

        let result =
            execute_begin_sponsoring_future_reserves(&op, &source_id, &mut state, &context);
        assert!(result.is_ok());

        match result.unwrap() {
            OperationResult::OpInner(OperationResultTr::BeginSponsoringFutureReserves(r)) => {
                assert!(matches!(r, BeginSponsoringFutureReservesResult::Malformed));
            }
            _ => panic!("Unexpected result type"),
        }
    }

    #[test]
    fn test_begin_sponsoring_success() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let sponsor_id = create_test_account_id(0);
        let sponsored_id = create_test_account_id(1);
        state.create_account(create_test_account(sponsor_id.clone(), 100_000_000));
        state.create_account(create_test_account(sponsored_id.clone(), 10_000_000));

        let op = BeginSponsoringFutureReservesOp {
            sponsored_id: sponsored_id.clone(),
        };

        let result =
            execute_begin_sponsoring_future_reserves(&op, &sponsor_id, &mut state, &context);
        assert!(result.is_ok());

        match result.unwrap() {
            OperationResult::OpInner(OperationResultTr::BeginSponsoringFutureReserves(r)) => {
                assert!(matches!(r, BeginSponsoringFutureReservesResult::Success));
            }
            _ => panic!("Unexpected result type"),
        }
    }

    #[test]
    fn test_end_sponsoring_success() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let sponsor_id = create_test_account_id(0);
        let sponsored_id = create_test_account_id(1);
        state.create_account(create_test_account(sponsor_id.clone(), 100_000_000));
        state.create_account(create_test_account(sponsored_id.clone(), 100_000_000));

        let begin = BeginSponsoringFutureReservesOp {
            sponsored_id: sponsored_id.clone(),
        };
        execute_begin_sponsoring_future_reserves(&begin, &sponsor_id, &mut state, &context)
            .unwrap();

        let result = execute_end_sponsoring_future_reserves(&sponsored_id, &mut state, &context);
        assert!(result.is_ok());

        match result.unwrap() {
            OperationResult::OpInner(OperationResultTr::EndSponsoringFutureReserves(r)) => {
                assert!(matches!(r, EndSponsoringFutureReservesResult::Success));
            }
            _ => panic!("Unexpected result type"),
        }
    }

    #[test]
    fn test_end_sponsoring_not_sponsored() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let account_id = create_test_account_id(0);
        state.create_account(create_test_account(account_id.clone(), 100_000_000));

        let result = execute_end_sponsoring_future_reserves(&account_id, &mut state, &context);
        assert!(result.is_ok());

        match result.unwrap() {
            OperationResult::OpInner(OperationResultTr::EndSponsoringFutureReserves(r)) => {
                assert!(matches!(r, EndSponsoringFutureReservesResult::NotSponsored));
            }
            _ => panic!("Unexpected result type"),
        }
    }

    #[test]
    fn test_revoke_sponsorship_not_exists() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(0);
        let nonexistent_id = create_test_account_id(99);
        state.create_account(create_test_account(source_id.clone(), 100_000_000));

        let op = RevokeSponsorshipOp::LedgerEntry(LedgerKey::Account(LedgerKeyAccount {
            account_id: nonexistent_id,
        }));

        let result = execute_revoke_sponsorship(&op, &source_id, &mut state, &context);
        assert!(result.is_ok());

        match result.unwrap() {
            OperationResult::OpInner(OperationResultTr::RevokeSponsorship(r)) => {
                assert!(matches!(r, RevokeSponsorshipResult::DoesNotExist));
            }
            _ => panic!("Unexpected result type"),
        }
    }

    #[test]
    fn test_revoke_sponsorship_removes_entry_sponsorship() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let sponsor_id = create_test_account_id(1);
        let owner_id = create_test_account_id(2);

        state.create_account(create_test_account(sponsor_id.clone(), 100_000_000));
        state.create_account(create_test_account(owner_id.clone(), 100_000_000));

        let data_entry = create_data_entry(&owner_id, "test");
        state.create_data(data_entry);

        let ledger_key = LedgerKey::Data(LedgerKeyData {
            account_id: owner_id.clone(),
            data_name: String64::try_from("test".as_bytes().to_vec()).unwrap(),
        });
        state
            .apply_entry_sponsorship_with_sponsor(
                ledger_key.clone(),
                &sponsor_id,
                Some(&owner_id),
                1,
            )
            .unwrap();

        let op = RevokeSponsorshipOp::LedgerEntry(ledger_key.clone());
        let result = execute_revoke_sponsorship(&op, &sponsor_id, &mut state, &context);
        assert!(matches!(
            result.unwrap(),
            OperationResult::OpInner(OperationResultTr::RevokeSponsorship(
                RevokeSponsorshipResult::Success
            ))
        ));
        assert!(state.entry_sponsor(&ledger_key).is_none());
    }

    #[test]
    fn test_revoke_sponsorship_not_sponsor() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let owner_id = create_test_account_id(1);
        let other_id = create_test_account_id(2);

        state.create_account(create_test_account(owner_id.clone(), 100_000_000));
        state.create_account(create_test_account(other_id.clone(), 100_000_000));

        let data_entry = create_data_entry(&owner_id, "test");
        state.create_data(data_entry);

        let ledger_key = LedgerKey::Data(LedgerKeyData {
            account_id: owner_id.clone(),
            data_name: String64::try_from("test".as_bytes().to_vec()).unwrap(),
        });
        let op = RevokeSponsorshipOp::LedgerEntry(ledger_key);
        let result = execute_revoke_sponsorship(&op, &other_id, &mut state, &context);
        assert!(matches!(
            result.unwrap(),
            OperationResult::OpInner(OperationResultTr::RevokeSponsorship(
                RevokeSponsorshipResult::NotSponsor
            ))
        ));
    }

    #[test]
    fn test_revoke_sponsorship_transfer() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let old_sponsor = create_test_account_id(1);
        let new_sponsor = create_test_account_id(2);
        let owner_id = create_test_account_id(3);

        state.create_account(create_test_account(old_sponsor.clone(), 100_000_000));
        state.create_account(create_test_account(new_sponsor.clone(), 100_000_000));
        state.create_account(create_test_account(owner_id.clone(), 100_000_000));

        let data_entry = create_data_entry(&owner_id, "test");
        state.create_data(data_entry);

        let ledger_key = LedgerKey::Data(LedgerKeyData {
            account_id: owner_id.clone(),
            data_name: String64::try_from("test".as_bytes().to_vec()).unwrap(),
        });
        state
            .apply_entry_sponsorship_with_sponsor(
                ledger_key.clone(),
                &old_sponsor,
                Some(&owner_id),
                1,
            )
            .unwrap();

        let begin = BeginSponsoringFutureReservesOp {
            sponsored_id: old_sponsor.clone(),
        };
        execute_begin_sponsoring_future_reserves(&begin, &new_sponsor, &mut state, &context)
            .unwrap();

        let op = RevokeSponsorshipOp::LedgerEntry(ledger_key.clone());
        let result = execute_revoke_sponsorship(&op, &old_sponsor, &mut state, &context);
        assert!(matches!(
            result.unwrap(),
            OperationResult::OpInner(OperationResultTr::RevokeSponsorship(
                RevokeSponsorshipResult::Success
            ))
        ));

        let sponsor = state.entry_sponsor(&ledger_key);
        assert_eq!(sponsor, Some(new_sponsor));
    }

    #[test]
    fn test_revoke_sponsorship_claimable_balance_only_transferable() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let sponsor_id = create_test_account_id(1);
        state.create_account(create_test_account(sponsor_id.clone(), 100_000_000));

        let balance_id = ClaimableBalanceId::ClaimableBalanceIdTypeV0(Hash([1u8; 32]));
        let entry = create_claimable_balance_entry(&sponsor_id, balance_id.clone());
        state.create_claimable_balance(entry);

        let ledger_key = LedgerKey::ClaimableBalance(LedgerKeyClaimableBalance {
            balance_id: balance_id.clone(),
        });
        state
            .apply_entry_sponsorship_with_sponsor(ledger_key.clone(), &sponsor_id, None, 1)
            .unwrap();

        let op = RevokeSponsorshipOp::LedgerEntry(ledger_key);
        let result = execute_revoke_sponsorship(&op, &sponsor_id, &mut state, &context);
        assert!(matches!(
            result.unwrap(),
            OperationResult::OpInner(OperationResultTr::RevokeSponsorship(
                RevokeSponsorshipResult::OnlyTransferable
            ))
        ));
    }

    #[test]
    fn test_revoke_sponsorship_signer_removes_sponsorship() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let sponsor_id = create_test_account_id(1);
        let owner_id = create_test_account_id(2);
        let signer_key = SignerKey::Ed25519(Uint256([7u8; 32]));

        state.create_account(create_test_account(sponsor_id.clone(), 100_000_000));
        state.create_account(create_account_with_signer(
            owner_id.clone(),
            signer_key.clone(),
            Some(sponsor_id.clone()),
        ));
        if let Some(account) = state.get_account_mut(&sponsor_id) {
            let ext = crate::state::ensure_account_ext_v2(account);
            ext.num_sponsoring = 1;
        }

        let op = RevokeSponsorshipOp::Signer(RevokeSponsorshipOpSigner {
            account_id: owner_id.clone(),
            signer_key: signer_key.clone(),
        });
        let result = execute_revoke_sponsorship(&op, &sponsor_id, &mut state, &context);
        assert!(matches!(
            result.unwrap(),
            OperationResult::OpInner(OperationResultTr::RevokeSponsorship(
                RevokeSponsorshipResult::Success
            ))
        ));
    }

    #[test]
    fn test_revoke_sponsorship_signer_transfer() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let old_sponsor = create_test_account_id(1);
        let new_sponsor = create_test_account_id(2);
        let owner_id = create_test_account_id(3);
        let signer_key = SignerKey::Ed25519(Uint256([8u8; 32]));

        state.create_account(create_test_account(old_sponsor.clone(), 100_000_000));
        state.create_account(create_test_account(new_sponsor.clone(), 100_000_000));
        state.create_account(create_account_with_signer(
            owner_id.clone(),
            signer_key.clone(),
            Some(old_sponsor.clone()),
        ));
        if let Some(account) = state.get_account_mut(&old_sponsor) {
            let ext = crate::state::ensure_account_ext_v2(account);
            ext.num_sponsoring = 1;
        }

        let begin = BeginSponsoringFutureReservesOp {
            sponsored_id: old_sponsor.clone(),
        };
        execute_begin_sponsoring_future_reserves(&begin, &new_sponsor, &mut state, &context)
            .unwrap();

        let op = RevokeSponsorshipOp::Signer(RevokeSponsorshipOpSigner {
            account_id: owner_id.clone(),
            signer_key: signer_key.clone(),
        });
        let result = execute_revoke_sponsorship(&op, &old_sponsor, &mut state, &context);
        assert!(matches!(
            result.unwrap(),
            OperationResult::OpInner(OperationResultTr::RevokeSponsorship(
                RevokeSponsorshipResult::Success
            ))
        ));
    }

    /// Test revoke sponsorship for a trustline entry.
    ///
    /// C++ Reference: RevokeSponsorshipTests.cpp - "trustline revoke"
    #[test]
    fn test_revoke_sponsorship_trustline() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let sponsor_id = create_test_account_id(10);
        let holder_id = create_test_account_id(11);
        let issuer_id = create_test_account_id(12);

        state.create_account(create_test_account(sponsor_id.clone(), 100_000_000));
        state.create_account(create_test_account(holder_id.clone(), 50_000_000));
        state.create_account(create_test_account(issuer_id.clone(), 100_000_000));

        let _asset = Asset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4(*b"USD\0"),
            issuer: issuer_id.clone(),
        });

        // Create a sponsored trustline
        let trustline = TrustLineEntry {
            account_id: holder_id.clone(),
            asset: TrustLineAsset::CreditAlphanum4(AlphaNum4 {
                asset_code: AssetCode4(*b"USD\0"),
                issuer: issuer_id.clone(),
            }),
            balance: 1000,
            limit: 100_000_000,
            flags: TrustLineFlags::AuthorizedFlag as u32,
            ext: TrustLineEntryExt::V0,
        };
        state.create_trustline(trustline);
        state.get_account_mut(&holder_id).unwrap().num_sub_entries += 1;

        // Set up sponsorship
        let ledger_key = LedgerKey::Trustline(LedgerKeyTrustLine {
            account_id: holder_id.clone(),
            asset: TrustLineAsset::CreditAlphanum4(AlphaNum4 {
                asset_code: AssetCode4(*b"USD\0"),
                issuer: issuer_id.clone(),
            }),
        });
        state.set_entry_sponsor(ledger_key, sponsor_id.clone());

        // Update sponsor's num_sponsoring
        if let Some(account) = state.get_account_mut(&sponsor_id) {
            let ext = crate::state::ensure_account_ext_v2(account);
            ext.num_sponsoring = 1;
        }

        // Update holder's num_sponsored (since the trustline is sponsored)
        if let Some(account) = state.get_account_mut(&holder_id) {
            let ext = crate::state::ensure_account_ext_v2(account);
            ext.num_sponsored = 1;
        }

        // Revoke the sponsorship (sponsor is the source, holder takes over reserve)
        let op = RevokeSponsorshipOp::LedgerEntry(LedgerKey::Trustline(LedgerKeyTrustLine {
            account_id: holder_id.clone(),
            asset: TrustLineAsset::CreditAlphanum4(AlphaNum4 {
                asset_code: AssetCode4(*b"USD\0"),
                issuer: issuer_id.clone(),
            }),
        }));

        let result = execute_revoke_sponsorship(&op, &sponsor_id, &mut state, &context);

        assert!(
            matches!(
                result.unwrap(),
                OperationResult::OpInner(OperationResultTr::RevokeSponsorship(
                    RevokeSponsorshipResult::Success
                ))
            ),
            "Expected success revoking trustline sponsorship"
        );
    }

    /// Test revoke sponsorship for an offer entry.
    ///
    /// C++ Reference: RevokeSponsorshipTests.cpp - "offer revoke"
    #[test]
    fn test_revoke_sponsorship_offer() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let sponsor_id = create_test_account_id(20);
        let seller_id = create_test_account_id(21);
        let issuer_id = create_test_account_id(22);

        state.create_account(create_test_account(sponsor_id.clone(), 100_000_000));
        state.create_account(create_test_account(seller_id.clone(), 50_000_000));
        state.create_account(create_test_account(issuer_id.clone(), 100_000_000));

        // Create a sponsored offer
        let offer = OfferEntry {
            seller_id: seller_id.clone(),
            offer_id: 123,
            selling: Asset::Native,
            buying: Asset::CreditAlphanum4(AlphaNum4 {
                asset_code: AssetCode4(*b"USD\0"),
                issuer: issuer_id.clone(),
            }),
            amount: 1000,
            price: Price { n: 1, d: 1 },
            flags: 0,
            ext: OfferEntryExt::V0,
        };
        state.create_offer(offer);
        state.get_account_mut(&seller_id).unwrap().num_sub_entries += 1;

        // Set up sponsorship
        let ledger_key = LedgerKey::Offer(LedgerKeyOffer {
            seller_id: seller_id.clone(),
            offer_id: 123,
        });
        state.set_entry_sponsor(ledger_key, sponsor_id.clone());

        // Update sponsor's num_sponsoring
        if let Some(account) = state.get_account_mut(&sponsor_id) {
            let ext = crate::state::ensure_account_ext_v2(account);
            ext.num_sponsoring = 1;
        }

        // Update seller's num_sponsored (since the offer is sponsored)
        if let Some(account) = state.get_account_mut(&seller_id) {
            let ext = crate::state::ensure_account_ext_v2(account);
            ext.num_sponsored = 1;
        }

        // Revoke the sponsorship
        let op = RevokeSponsorshipOp::LedgerEntry(LedgerKey::Offer(LedgerKeyOffer {
            seller_id: seller_id.clone(),
            offer_id: 123,
        }));

        let result = execute_revoke_sponsorship(&op, &sponsor_id, &mut state, &context);

        assert!(
            matches!(
                result.unwrap(),
                OperationResult::OpInner(OperationResultTr::RevokeSponsorship(
                    RevokeSponsorshipResult::Success
                ))
            ),
            "Expected success revoking offer sponsorship"
        );
    }

    /// Test begin sponsoring fails when target is already being sponsored (AlreadySponsored).
    ///
    /// C++ Reference: BeginSponsoringFutureReservesTests.cpp - "already sponsored"
    #[test]
    fn test_begin_sponsoring_already_sponsored() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let sponsor1_id = create_test_account_id(30);
        let sponsor2_id = create_test_account_id(31);
        let sponsored_id = create_test_account_id(32);

        state.create_account(create_test_account(sponsor1_id.clone(), 100_000_000));
        state.create_account(create_test_account(sponsor2_id.clone(), 100_000_000));
        state.create_account(create_test_account(sponsored_id.clone(), 50_000_000));

        // First sponsor begins sponsoring
        let op1 = BeginSponsoringFutureReservesOp {
            sponsored_id: sponsored_id.clone(),
        };
        let result1 =
            execute_begin_sponsoring_future_reserves(&op1, &sponsor1_id, &mut state, &context);
        assert!(
            matches!(
                result1.unwrap(),
                OperationResult::OpInner(OperationResultTr::BeginSponsoringFutureReserves(
                    BeginSponsoringFutureReservesResult::Success
                ))
            ),
            "First sponsor should succeed"
        );

        // Second sponsor tries to begin sponsoring the same account
        let op2 = BeginSponsoringFutureReservesOp {
            sponsored_id: sponsored_id.clone(),
        };
        let result2 =
            execute_begin_sponsoring_future_reserves(&op2, &sponsor2_id, &mut state, &context);

        match result2.unwrap() {
            OperationResult::OpInner(OperationResultTr::BeginSponsoringFutureReserves(r)) => {
                assert!(
                    matches!(r, BeginSponsoringFutureReservesResult::AlreadySponsored),
                    "Expected AlreadySponsored, got {:?}",
                    r
                );
            }
            _ => panic!("Unexpected result type"),
        }
    }

    /// Test begin sponsoring fails when recursive sponsorship is detected.
    ///
    /// Case 1: Source account is already being sponsored by someone else.
    /// C++ Reference: BeginSponsoringFutureReservesTests.cpp - "recursive"
    #[test]
    fn test_begin_sponsoring_recursive_source_is_sponsored() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let sponsor_a = create_test_account_id(50);
        let sponsor_b = create_test_account_id(51);
        let sponsored_id = create_test_account_id(52);

        state.create_account(create_test_account(sponsor_a.clone(), 100_000_000));
        state.create_account(create_test_account(sponsor_b.clone(), 100_000_000));
        state.create_account(create_test_account(sponsored_id.clone(), 50_000_000));

        // A begins sponsoring B
        let begin_ab = BeginSponsoringFutureReservesOp {
            sponsored_id: sponsor_b.clone(),
        };
        let r =
            execute_begin_sponsoring_future_reserves(&begin_ab, &sponsor_a, &mut state, &context);
        assert!(matches!(
            r.unwrap(),
            OperationResult::OpInner(OperationResultTr::BeginSponsoringFutureReserves(
                BeginSponsoringFutureReservesResult::Success
            ))
        ));

        // B (who is already being sponsored) tries to sponsor C → Recursive
        let begin_bc = BeginSponsoringFutureReservesOp {
            sponsored_id: sponsored_id.clone(),
        };
        let r2 =
            execute_begin_sponsoring_future_reserves(&begin_bc, &sponsor_b, &mut state, &context);
        assert!(matches!(
            r2.unwrap(),
            OperationResult::OpInner(OperationResultTr::BeginSponsoringFutureReserves(
                BeginSponsoringFutureReservesResult::Recursive
            ))
        ));
    }

    /// Test begin sponsoring fails when sponsored account is already sponsoring someone.
    ///
    /// Case 2: The target sponsored_id is already a sponsor in the current tx.
    #[test]
    fn test_begin_sponsoring_recursive_sponsored_is_sponsoring() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let a = create_test_account_id(53);
        let b = create_test_account_id(54);
        let c = create_test_account_id(55);

        state.create_account(create_test_account(a.clone(), 100_000_000));
        state.create_account(create_test_account(b.clone(), 100_000_000));
        state.create_account(create_test_account(c.clone(), 100_000_000));

        // B begins sponsoring C (B is now a sponsor / "sponsoring" someone)
        let begin_bc = BeginSponsoringFutureReservesOp {
            sponsored_id: c.clone(),
        };
        let r = execute_begin_sponsoring_future_reserves(&begin_bc, &b, &mut state, &context);
        assert!(matches!(
            r.unwrap(),
            OperationResult::OpInner(OperationResultTr::BeginSponsoringFutureReserves(
                BeginSponsoringFutureReservesResult::Success
            ))
        ));

        // A tries to sponsor B, but B is already sponsoring C → Recursive
        // (is_sponsoring(&B) returns true)
        let begin_ab = BeginSponsoringFutureReservesOp {
            sponsored_id: b.clone(),
        };
        let r2 = execute_begin_sponsoring_future_reserves(&begin_ab, &a, &mut state, &context);
        assert!(matches!(
            r2.unwrap(),
            OperationResult::OpInner(OperationResultTr::BeginSponsoringFutureReserves(
                BeginSponsoringFutureReservesResult::Recursive
            ))
        ));
    }

    /// Test revoke sponsorship returns Malformed when revoking a ClaimableBalance
    /// that has no sponsor set.
    #[test]
    fn test_revoke_sponsorship_malformed_claimable_balance_no_sponsor() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(60);
        state.create_account(create_test_account(source_id.clone(), 100_000_000));

        // Create a claimable balance with NO sponsor
        let balance_id = ClaimableBalanceId::ClaimableBalanceIdTypeV0(Hash([2u8; 32]));
        let entry = create_claimable_balance_entry(&source_id, balance_id.clone());
        state.create_claimable_balance(entry);
        // Note: no apply_entry_sponsorship — the entry has no sponsor

        let ledger_key = LedgerKey::ClaimableBalance(LedgerKeyClaimableBalance { balance_id });
        let op = RevokeSponsorshipOp::LedgerEntry(ledger_key);
        let result = execute_revoke_sponsorship(&op, &source_id, &mut state, &context);
        assert!(matches!(
            result.unwrap(),
            OperationResult::OpInner(OperationResultTr::RevokeSponsorship(
                RevokeSponsorshipResult::Malformed
            ))
        ));
    }

    /// Test revoke sponsorship for signer variant returns DoesNotExist when
    /// the account doesn't exist.
    #[test]
    fn test_revoke_sponsorship_signer_account_does_not_exist() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(61);
        let nonexistent_id = create_test_account_id(99);
        state.create_account(create_test_account(source_id.clone(), 100_000_000));

        let signer_key = SignerKey::Ed25519(Uint256([9u8; 32]));
        let op = RevokeSponsorshipOp::Signer(RevokeSponsorshipOpSigner {
            account_id: nonexistent_id,
            signer_key,
        });
        let result = execute_revoke_sponsorship(&op, &source_id, &mut state, &context);
        assert!(matches!(
            result.unwrap(),
            OperationResult::OpInner(OperationResultTr::RevokeSponsorship(
                RevokeSponsorshipResult::DoesNotExist
            ))
        ));
    }

    /// Test revoke sponsorship for signer variant returns DoesNotExist when
    /// the signer is not found on the account.
    #[test]
    fn test_revoke_sponsorship_signer_not_found() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(62);
        let owner_id = create_test_account_id(63);
        let wrong_signer = SignerKey::Ed25519(Uint256([99u8; 32]));

        state.create_account(create_test_account(source_id.clone(), 100_000_000));
        // Create account with a different signer
        let real_signer = SignerKey::Ed25519(Uint256([10u8; 32]));
        state.create_account(create_account_with_signer(
            owner_id.clone(),
            real_signer,
            None,
        ));

        let op = RevokeSponsorshipOp::Signer(RevokeSponsorshipOpSigner {
            account_id: owner_id,
            signer_key: wrong_signer,
        });
        let result = execute_revoke_sponsorship(&op, &source_id, &mut state, &context);
        assert!(matches!(
            result.unwrap(),
            OperationResult::OpInner(OperationResultTr::RevokeSponsorship(
                RevokeSponsorshipResult::DoesNotExist
            ))
        ));
    }

    /// Test revoke sponsorship fails when sponsored account can't afford reserve (LowReserve).
    ///
    /// C++ Reference: RevokeSponsorshipTests.cpp - "low reserve"
    #[test]
    fn test_revoke_sponsorship_low_reserve() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let sponsor_id = create_test_account_id(40);
        let holder_id = create_test_account_id(41);

        state.create_account(create_test_account(sponsor_id.clone(), 100_000_000));
        // Holder has very low balance - can't afford reserve if sponsorship is revoked
        // min_balance = 2 * 5_000_000 = 10_000_000 for 0 subentries
        // With 1 subentry, min_balance = 15_000_000
        // But holder only has 10_000_001 - just above min for 0 subentries
        state.create_account(create_test_account(holder_id.clone(), 10_000_001));

        // Create a sponsored data entry
        let data_entry = create_data_entry(&holder_id, "mydata");
        state.create_data(data_entry);
        state.get_account_mut(&holder_id).unwrap().num_sub_entries += 1;

        // Set up sponsorship
        let data_name = String64::try_from("mydata".as_bytes().to_vec()).unwrap();
        let ledger_key = LedgerKey::Data(LedgerKeyData {
            account_id: holder_id.clone(),
            data_name,
        });
        state.set_entry_sponsor(ledger_key, sponsor_id.clone());

        // Update sponsor's num_sponsoring
        if let Some(account) = state.get_account_mut(&sponsor_id) {
            let ext = crate::state::ensure_account_ext_v2(account);
            ext.num_sponsoring = 1;
        }

        // Update holder's num_sponsored
        if let Some(account) = state.get_account_mut(&holder_id) {
            let ext = crate::state::ensure_account_ext_v2(account);
            ext.num_sponsored = 1;
        }

        // Try to revoke the sponsorship - holder can't afford the reserve
        let data_name2 = String64::try_from("mydata".as_bytes().to_vec()).unwrap();
        let op = RevokeSponsorshipOp::LedgerEntry(LedgerKey::Data(LedgerKeyData {
            account_id: holder_id.clone(),
            data_name: data_name2,
        }));

        let result = execute_revoke_sponsorship(&op, &sponsor_id, &mut state, &context);

        match result.unwrap() {
            OperationResult::OpInner(OperationResultTr::RevokeSponsorship(r)) => {
                assert!(
                    matches!(r, RevokeSponsorshipResult::LowReserve),
                    "Expected LowReserve, got {:?}",
                    r
                );
            }
            _ => panic!("Unexpected result type"),
        }
    }
}
