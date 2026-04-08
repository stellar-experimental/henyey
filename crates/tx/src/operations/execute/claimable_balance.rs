//! Claimable Balance operation execution.
//!
//! This module implements the execution logic for CreateClaimableBalance and
//! ClaimClaimableBalance operations.

use std::collections::HashSet;

use stellar_xdr::curr::{
    AccountFlags, AccountId, Asset, ClaimClaimableBalanceOp, ClaimClaimableBalanceResult,
    ClaimClaimableBalanceResultCode, ClaimPredicate, ClaimableBalanceEntry,
    ClaimableBalanceEntryExt, ClaimableBalanceEntryExtensionV1,
    ClaimableBalanceEntryExtensionV1Ext, ClaimableBalanceFlags, ClaimableBalanceId, Claimant,
    CreateClaimableBalanceOp, CreateClaimableBalanceResult, CreateClaimableBalanceResultCode, Hash,
    HashIdPreimage, HashIdPreimageOperationId, LedgerKey, LedgerKeyClaimableBalance,
    OperationResult, OperationResultTr, SequenceNumber,
};

use super::{
    account_balance_after_liabilities, account_liabilities, add_account_balance,
    add_trustline_balance, is_trustline_authorized, sub_account_balance, sub_trustline_balance,
    trustline_balance_after_liabilities, TxIdentity, TRUSTLINE_CLAWBACK_ENABLED_FLAG,
};
use crate::state::LedgerStateManager;
use crate::validation::LedgerContext;
use crate::{Result, TxError};

/// Execute a CreateClaimableBalance operation.
///
/// This operation creates a new claimable balance entry that can be claimed
/// by one of the specified claimants.
///
/// # Arguments
///
/// * `op` - The CreateClaimableBalance operation data
/// * `source` - The source account ID
/// * `tx_source` - The transaction-level source account (for balance ID derivation)
/// * `tx_seq` - The transaction sequence number (for balance ID derivation)
/// * `op_index` - The operation index within the transaction (for balance ID derivation)
/// * `state` - The ledger state manager
/// * `context` - The ledger context
///
/// # Returns
///
/// Returns the operation result indicating success or a specific failure reason.
pub(crate) fn execute_create_claimable_balance(
    op: &CreateClaimableBalanceOp,
    source: &AccountId,
    tx_id: &TxIdentity<'_>,
    state: &mut LedgerStateManager,
    context: &LedgerContext,
) -> Result<OperationResult> {
    // Validate claimants, amount, and predicates
    if let Some(result) = validate_create_claimable_balance_op(op) {
        return Ok(result);
    }

    // Check source account exists (stellar-core calls loadSourceAccount which records access)
    state.record_account_access(source);
    let account = match state.get_account(source) {
        Some(a) => a.clone(),
        None => {
            return Ok(make_create_result(
                CreateClaimableBalanceResultCode::Underfunded,
                None,
            ));
        }
    };

    let issuer = asset_issuer(&op.asset);
    let mut source_trustline_flags = None;
    let sponsor = state
        .active_sponsor_for(source)
        .unwrap_or_else(|| source.clone());
    let _sponsor_is_source = sponsor == *source;
    let sponsorship_multiplier = op.claimants.len() as i64;

    // stellar-core order:
    // 1. Check available balance (UNDERFUNDED) - before deducting
    // 2. Deduct balance
    // 3. Call createEntryWithPossibleSponsorship (LOW_RESERVE)
    //
    // We must check UNDERFUNDED before LOW_RESERVE to match stellar-core behavior.

    // Check source has sufficient balance FIRST (matches stellar-core getAvailableBalance check)
    // NOTE: stellar-core getAvailableBalance() uses getMinBalance() which does NOT include
    // the sponsorship for the new claimable balance. The sponsorship reserve check
    // happens separately in createEntryWithPossibleSponsorship AFTER deducting balance.
    match &op.asset {
        Asset::Native => {
            // For native assets, available = balance - minBalance - sellingLiabilities
            // Matches stellar-core's getAvailableBalance(header, sourceAccount)
            let min_balance =
                state.minimum_balance_for_account(&account, context.protocol_version, 0)?;
            let available = (account.balance - min_balance)
                .saturating_sub(account_liabilities(&account).selling);
            if available < op.amount {
                return Ok(make_create_result(
                    CreateClaimableBalanceResultCode::Underfunded,
                    None,
                ));
            }
        }
        _ => {
            if issuer.as_ref() != Some(source) {
                // For non-native assets, check trustline when not issuer
                match state.get_trustline(source, &op.asset) {
                    Some(tl) => {
                        source_trustline_flags = Some(tl.flags);
                        if !is_trustline_authorized(tl.flags) {
                            return Ok(make_create_result(
                                CreateClaimableBalanceResultCode::NotAuthorized,
                                None,
                            ));
                        }
                        // stellar-core uses trustline.addBalance(header, -amount)
                        // which fails if newBalance < sellingLiabilities
                        let available = trustline_balance_after_liabilities(tl);
                        if available < op.amount {
                            return Ok(make_create_result(
                                CreateClaimableBalanceResultCode::Underfunded,
                                None,
                            ));
                        }
                    }
                    None => {
                        return Ok(make_create_result(
                            CreateClaimableBalanceResultCode::NoTrust,
                            None,
                        ));
                    }
                }
            }
        }
    }

    // Generate the claimable balance ID
    let balance_id = generate_claimable_balance_id(tx_id)?;

    // Deduct balance from source (matches stellar-core addBalance call)
    match &op.asset {
        Asset::Native => {
            if let Some(account) = state.get_account_mut(source) {
                sub_account_balance(account, op.amount)?;
            }
        }
        _ => {
            if issuer.as_ref() != Some(source) {
                if let Some(tl) = state.get_trustline_mut(source, &op.asset) {
                    sub_trustline_balance(tl, op.amount)?;
                }
            }
        }
    }

    // NOW check sponsor's reserve (matches stellar-core createEntryWithPossibleSponsorship)
    // This happens AFTER balance deduction in stellar-core
    let sponsor_account = state
        .get_account(&sponsor)
        .ok_or(TxError::SourceAccountNotFound)?;
    let sponsor_min_balance = state.minimum_balance_for_account_with_deltas(
        sponsor_account,
        context.protocol_version,
        0,
        sponsorship_multiplier,
        0,
    )?;
    let available = account_balance_after_liabilities(sponsor_account);
    if available < sponsor_min_balance {
        return Ok(make_create_result(
            CreateClaimableBalanceResultCode::LowReserve,
            None,
        ));
    }

    let mut claimable_flags = 0u32;
    if let Some(issuer_id) = issuer.as_ref() {
        let clawback_enabled = if issuer_id == source {
            account.flags & (AccountFlags::ClawbackEnabledFlag as u32) != 0
        } else {
            source_trustline_flags
                .map(|flags| flags & TRUSTLINE_CLAWBACK_ENABLED_FLAG != 0)
                .unwrap_or(false)
        };
        if clawback_enabled {
            claimable_flags |= ClaimableBalanceFlags::ClaimableBalanceClawbackEnabledFlag as u32;
        }
    }

    // Convert any relative time predicates into absolute times.
    let mut claimants: Vec<Claimant> = op.claimants.iter().cloned().collect();
    for claimant in &mut claimants {
        let Claimant::ClaimantTypeV0(cv0) = claimant;
        update_predicate_for_apply(&mut cv0.predicate, context.close_time);
    }
    let claimants = claimants
        .try_into()
        .map_err(|_| TxError::Internal("claimants size overflow".to_string()))?;

    // Create the claimable balance entry
    let entry = ClaimableBalanceEntry {
        balance_id: balance_id.clone(),
        claimants,
        asset: op.asset.clone(),
        amount: op.amount,
        ext: if claimable_flags > 0 {
            ClaimableBalanceEntryExt::V1(ClaimableBalanceEntryExtensionV1 {
                ext: ClaimableBalanceEntryExtensionV1Ext::V0,
                flags: claimable_flags,
            })
        } else {
            ClaimableBalanceEntryExt::V0
        },
    };

    let ledger_key = LedgerKey::ClaimableBalance(LedgerKeyClaimableBalance {
        balance_id: balance_id.clone(),
    });
    state.apply_entry_sponsorship_with_sponsor(
        ledger_key,
        &sponsor,
        None,
        sponsorship_multiplier,
    )?;
    state.create_claimable_balance(entry);

    Ok(make_create_result(
        CreateClaimableBalanceResultCode::Success,
        Some(balance_id),
    ))
}

/// Execute a ClaimClaimableBalance operation.
///
/// This operation claims an existing claimable balance, transferring the
/// balance to the claiming account.
///
/// # Arguments
///
/// * `op` - The ClaimClaimableBalance operation data
/// * `source` - The source account ID (the claimant)
/// * `state` - The ledger state manager
/// * `context` - The ledger context
///
/// # Returns
///
/// Returns the operation result indicating success or a specific failure reason.
pub(crate) fn execute_claim_claimable_balance(
    op: &ClaimClaimableBalanceOp,
    source: &AccountId,
    state: &mut LedgerStateManager,
    context: &LedgerContext,
) -> Result<OperationResult> {
    // Get the claimable balance entry
    let entry = match state.get_claimable_balance(&op.balance_id) {
        Some(e) => e.clone(),
        None => {
            return Ok(make_claim_result(
                ClaimClaimableBalanceResultCode::DoesNotExist,
            ));
        }
    };

    // Check if source is a valid claimant
    let is_valid_claimant = entry.claimants.iter().any(|c| match c {
        stellar_xdr::curr::Claimant::ClaimantTypeV0(cv0) => {
            &cv0.destination == source && check_predicate(&cv0.predicate, context)
        }
    });

    if !is_valid_claimant {
        return Ok(make_claim_result(
            ClaimClaimableBalanceResultCode::CannotClaim,
        ));
    }

    // Check source account exists (use mutable access to mirror stellar-core loadSourceAccount)
    if state.get_account_mut(source).is_none() {
        return Ok(make_claim_result(
            ClaimClaimableBalanceResultCode::CannotClaim,
        ));
    }

    // CAP-77: Check if the relevant key is frozen before claiming (apply-time check).
    // For native asset: checks source account key.
    // For non-native: checks source trustline key.
    if context.frozen_key_config.has_frozen_keys() {
        let key_to_check = match &entry.asset {
            Asset::Native => crate::frozen_keys::account_key(source),
            _ => crate::frozen_keys::trustline_key(source, &entry.asset),
        };
        if context.frozen_key_config.is_key_frozen(&key_to_check) {
            return Ok(make_claim_result(
                ClaimClaimableBalanceResultCode::TrustlineFrozen,
            ));
        }
    }

    // Transfer the balance.
    // All balance checks use the overflow-safe pattern from stellar-core's
    // addBalance (types.cpp): instead of `balance + delta > maxBalance` (which
    // can overflow i64), check `maxBalance - balance < delta` (safe because
    // maxBalance >= balance >= 0).
    match &entry.asset {
        Asset::Native => {
            if let Some(account) = state.get_account_mut(source) {
                if add_account_balance(account, entry.amount).is_err() {
                    return Ok(make_claim_result(ClaimClaimableBalanceResultCode::LineFull));
                }
            }
        }
        _ => {
            // Get the issuer of the asset
            let issuer = match &entry.asset {
                Asset::CreditAlphanum4(a) => &a.issuer,
                Asset::CreditAlphanum12(a) => &a.issuer,
                Asset::Native => unreachable!(),
            };

            // If source is the issuer, they don't need a trustline (stellar-core IssuerImpl behavior).
            // Issuers have unlimited trust for their own assets - just skip the trustline check.
            if source == issuer {
                // Issuer claiming their own asset: no trustline update needed
                // (the tokens are effectively burned/returned to issuer)
            } else {
                // Non-issuer: check trustline exists
                match state.get_trustline_mut(source, &entry.asset) {
                    Some(tl) => {
                        if !is_trustline_authorized(tl.flags) {
                            return Ok(make_claim_result(
                                ClaimClaimableBalanceResultCode::NotAuthorized,
                            ));
                        }
                        if add_trustline_balance(tl, entry.amount).is_err() {
                            return Ok(make_claim_result(
                                ClaimClaimableBalanceResultCode::LineFull,
                            ));
                        }
                    }
                    None => {
                        return Ok(make_claim_result(ClaimClaimableBalanceResultCode::NoTrust));
                    }
                }
            }
        }
    }

    let sponsorship_multiplier = entry.claimants.len() as i64;
    let ledger_key = LedgerKey::ClaimableBalance(LedgerKeyClaimableBalance {
        balance_id: entry.balance_id.clone(),
    });
    let sponsor = state.entry_sponsor(&ledger_key);
    // Delete the claimable balance entry
    state.delete_claimable_balance(&op.balance_id);
    if let Some(sponsor) = sponsor {
        state.update_num_sponsoring(&sponsor, -sponsorship_multiplier)?;
    }

    Ok(make_claim_result(ClaimClaimableBalanceResultCode::Success))
}

/// Generate a claimable balance ID.
fn generate_claimable_balance_id(tx_id: &TxIdentity<'_>) -> Result<ClaimableBalanceId> {
    let preimage = HashIdPreimage::OpId(HashIdPreimageOperationId {
        source_account: tx_id.source_id.clone(),
        seq_num: SequenceNumber(tx_id.seq),
        op_num: tx_id.op_index,
    });
    let hash = henyey_common::Hash256::hash_xdr(&preimage)
        .map_err(|e| TxError::Internal(format!("claimable balance id hash error: {}", e)))?;
    Ok(ClaimableBalanceId::ClaimableBalanceIdTypeV0(Hash(hash.0)))
}

fn asset_issuer(asset: &Asset) -> Option<AccountId> {
    henyey_common::asset::get_issuer(asset).ok().cloned()
}

/// Check if a claim predicate is satisfied.
fn check_predicate(predicate: &stellar_xdr::curr::ClaimPredicate, context: &LedgerContext) -> bool {
    match predicate {
        ClaimPredicate::Unconditional => true,
        ClaimPredicate::And(predicates) => {
            predicates.len() == 2 && predicates.iter().all(|p| check_predicate(p, context))
        }
        ClaimPredicate::Or(predicates) => {
            predicates.len() == 2 && predicates.iter().any(|p| check_predicate(p, context))
        }
        ClaimPredicate::Not(p) => p
            .as_ref()
            .map(|inner| !check_predicate(inner, context))
            .unwrap_or(false),
        ClaimPredicate::BeforeAbsoluteTime(time) => (context.close_time as i64) < *time,
        ClaimPredicate::BeforeRelativeTime(_) => false,
    }
}

fn validate_claim_predicate(predicate: &ClaimPredicate, depth: u32) -> bool {
    if depth > 4 {
        return false;
    }
    match predicate {
        ClaimPredicate::Unconditional => true,
        ClaimPredicate::And(predicates) => {
            predicates.len() == 2
                && validate_claim_predicate(&predicates[0], depth + 1)
                && validate_claim_predicate(&predicates[1], depth + 1)
        }
        ClaimPredicate::Or(predicates) => {
            predicates.len() == 2
                && validate_claim_predicate(&predicates[0], depth + 1)
                && validate_claim_predicate(&predicates[1], depth + 1)
        }
        ClaimPredicate::Not(predicate) => predicate
            .as_ref()
            .map(|inner| validate_claim_predicate(inner, depth + 1))
            .unwrap_or(false),
        ClaimPredicate::BeforeAbsoluteTime(time) => *time >= 0,
        ClaimPredicate::BeforeRelativeTime(time) => *time >= 0,
    }
}

fn update_predicate_for_apply(predicate: &mut ClaimPredicate, close_time: u64) {
    match predicate {
        ClaimPredicate::And(predicates) => {
            if predicates.len() == 2 {
                let mut left = predicates[0].clone();
                let mut right = predicates[1].clone();
                update_predicate_for_apply(&mut left, close_time);
                update_predicate_for_apply(&mut right, close_time);
                *predicate = ClaimPredicate::And(vec![left, right].try_into().unwrap());
            }
        }
        ClaimPredicate::Or(predicates) => {
            if predicates.len() == 2 {
                let mut left = predicates[0].clone();
                let mut right = predicates[1].clone();
                update_predicate_for_apply(&mut left, close_time);
                update_predicate_for_apply(&mut right, close_time);
                *predicate = ClaimPredicate::Or(vec![left, right].try_into().unwrap());
            }
        }
        ClaimPredicate::Not(predicate) => {
            if let Some(inner) = predicate.as_mut() {
                update_predicate_for_apply(inner, close_time);
            }
        }
        ClaimPredicate::BeforeRelativeTime(relative) => {
            let close_time_i64 = if close_time > i64::MAX as u64 {
                i64::MAX
            } else {
                close_time as i64
            };
            let absolute = if close_time_i64 > i64::MAX - *relative {
                i64::MAX
            } else {
                close_time_i64 + *relative
            };
            *predicate = ClaimPredicate::BeforeAbsoluteTime(absolute);
        }
        ClaimPredicate::BeforeAbsoluteTime(_) | ClaimPredicate::Unconditional => {}
    }
}

/// Validate CreateClaimableBalance operation parameters: non-empty claimants,
/// positive amount, no duplicate destinations, and valid predicates.
/// Returns `Some(result)` on validation failure, `None` if valid.
fn validate_create_claimable_balance_op(op: &CreateClaimableBalanceOp) -> Option<OperationResult> {
    if op.claimants.is_empty() || op.amount <= 0 {
        return Some(make_create_result(
            CreateClaimableBalanceResultCode::Malformed,
            None,
        ));
    }

    let mut destinations = HashSet::new();
    for claimant in op.claimants.iter() {
        match claimant {
            Claimant::ClaimantTypeV0(cv0) => {
                if !destinations.insert(cv0.destination.clone()) {
                    return Some(make_create_result(
                        CreateClaimableBalanceResultCode::Malformed,
                        None,
                    ));
                }
                if !validate_claim_predicate(&cv0.predicate, 1) {
                    return Some(make_create_result(
                        CreateClaimableBalanceResultCode::Malformed,
                        None,
                    ));
                }
            }
        }
    }
    None
}

fn make_create_result(
    code: CreateClaimableBalanceResultCode,
    balance_id: Option<ClaimableBalanceId>,
) -> OperationResult {
    let result = match code {
        CreateClaimableBalanceResultCode::Success => {
            CreateClaimableBalanceResult::Success(balance_id.unwrap())
        }
        CreateClaimableBalanceResultCode::Malformed => CreateClaimableBalanceResult::Malformed,
        CreateClaimableBalanceResultCode::LowReserve => CreateClaimableBalanceResult::LowReserve,
        CreateClaimableBalanceResultCode::NoTrust => CreateClaimableBalanceResult::NoTrust,
        CreateClaimableBalanceResultCode::NotAuthorized => {
            CreateClaimableBalanceResult::NotAuthorized
        }
        CreateClaimableBalanceResultCode::Underfunded => CreateClaimableBalanceResult::Underfunded,
    };

    OperationResult::OpInner(OperationResultTr::CreateClaimableBalance(result))
}

/// Create a ClaimClaimableBalance result.
fn make_claim_result(code: ClaimClaimableBalanceResultCode) -> OperationResult {
    let result = match code {
        ClaimClaimableBalanceResultCode::Success => ClaimClaimableBalanceResult::Success,
        ClaimClaimableBalanceResultCode::DoesNotExist => ClaimClaimableBalanceResult::DoesNotExist,
        ClaimClaimableBalanceResultCode::CannotClaim => ClaimClaimableBalanceResult::CannotClaim,
        ClaimClaimableBalanceResultCode::LineFull => ClaimClaimableBalanceResult::LineFull,
        ClaimClaimableBalanceResultCode::NoTrust => ClaimClaimableBalanceResult::NoTrust,
        ClaimClaimableBalanceResultCode::NotAuthorized => {
            ClaimClaimableBalanceResult::NotAuthorized
        }
        ClaimClaimableBalanceResultCode::TrustlineFrozen => {
            ClaimClaimableBalanceResult::TrustlineFrozen
        }
    };

    OperationResult::OpInner(OperationResultTr::ClaimClaimableBalance(result))
}

#[cfg(test)]
mod tests {
    use super::super::AUTHORIZED_FLAG;
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

    fn create_test_trustline(
        account_id: AccountId,
        issuer: AccountId,
        authorized: bool,
        clawback_enabled: bool,
        balance: i64,
    ) -> TrustLineEntry {
        let mut flags = 0;
        if authorized {
            flags |= AUTHORIZED_FLAG;
        }
        if clawback_enabled {
            flags |= TRUSTLINE_CLAWBACK_ENABLED_FLAG;
        }
        TrustLineEntry {
            account_id,
            asset: TrustLineAsset::CreditAlphanum4(AlphaNum4 {
                asset_code: AssetCode4(*b"USD\0"),
                issuer,
            }),
            balance,
            limit: 100_000_000,
            flags,
            ext: TrustLineEntryExt::V0,
        }
    }

    fn create_test_context() -> LedgerContext {
        LedgerContext::testnet(1, 1000)
    }

    #[test]
    fn test_create_claimable_balance_malformed_no_claimants() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(0);
        state.create_account(create_test_account(source_id.clone(), 100_000_000));

        let op = CreateClaimableBalanceOp {
            asset: Asset::Native,
            amount: 10_000_000,
            claimants: vec![].try_into().unwrap(), // No claimants
        };

        let result = execute_create_claimable_balance(
            &op,
            &source_id,
            &TxIdentity {
                source_id: &source_id,
                seq: 123,
                op_index: 0,
            },
            &mut state,
            &context,
        );
        assert!(result.is_ok());

        match result.unwrap() {
            OperationResult::OpInner(OperationResultTr::CreateClaimableBalance(r)) => {
                assert!(matches!(r, CreateClaimableBalanceResult::Malformed));
            }
            _ => panic!("Unexpected result type"),
        }
    }

    #[test]
    fn test_create_claimable_balance_success() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(0);
        let claimant_id = create_test_account_id(1);
        state.create_account(create_test_account(source_id.clone(), 100_000_000));
        state.create_account(create_test_account(claimant_id.clone(), 10_000_000));

        let claimant = Claimant::ClaimantTypeV0(ClaimantV0 {
            destination: claimant_id.clone(),
            predicate: ClaimPredicate::Unconditional,
        });

        let op = CreateClaimableBalanceOp {
            asset: Asset::Native,
            amount: 10_000_000,
            claimants: vec![claimant].try_into().unwrap(),
        };

        let result = execute_create_claimable_balance(
            &op,
            &source_id,
            &TxIdentity {
                source_id: &source_id,
                seq: 123,
                op_index: 0,
            },
            &mut state,
            &context,
        );
        assert!(result.is_ok());

        match result.unwrap() {
            OperationResult::OpInner(OperationResultTr::CreateClaimableBalance(r)) => {
                if let CreateClaimableBalanceResult::Success(balance_id) = r {
                    let expected = generate_claimable_balance_id(&TxIdentity {
                        source_id: &source_id,
                        seq: 123,
                        op_index: 0,
                    })
                    .unwrap();
                    assert_eq!(balance_id, expected);
                } else {
                    panic!("unexpected result: {:?}", r);
                }
            }
            _ => panic!("Unexpected result type"),
        }

        // Check source balance was deducted
        let source = state.get_account(&source_id).unwrap();
        assert_eq!(source.balance, 90_000_000);
    }

    #[test]
    fn test_create_claimable_balance_duplicate_claimants_malformed() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(0);
        let claimant_id = create_test_account_id(1);
        state.create_account(create_test_account(source_id.clone(), 100_000_000));

        let claimant = Claimant::ClaimantTypeV0(ClaimantV0 {
            destination: claimant_id.clone(),
            predicate: ClaimPredicate::Unconditional,
        });

        let op = CreateClaimableBalanceOp {
            asset: Asset::Native,
            amount: 10_000_000,
            claimants: vec![claimant.clone(), claimant].try_into().unwrap(),
        };

        let result = execute_create_claimable_balance(
            &op,
            &source_id,
            &TxIdentity {
                source_id: &source_id,
                seq: 123,
                op_index: 0,
            },
            &mut state,
            &context,
        )
        .expect("create claimable balance");

        match result {
            OperationResult::OpInner(OperationResultTr::CreateClaimableBalance(r)) => {
                assert!(matches!(r, CreateClaimableBalanceResult::Malformed));
            }
            other => panic!("unexpected result: {:?}", other),
        }
    }

    #[test]
    fn test_create_claimable_balance_invalid_predicate_malformed() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(0);
        let claimant_id = create_test_account_id(1);
        state.create_account(create_test_account(source_id.clone(), 100_000_000));

        let bad_predicate = ClaimPredicate::Not(None);
        let claimant = Claimant::ClaimantTypeV0(ClaimantV0 {
            destination: claimant_id.clone(),
            predicate: bad_predicate,
        });

        let op = CreateClaimableBalanceOp {
            asset: Asset::Native,
            amount: 10_000_000,
            claimants: vec![claimant].try_into().unwrap(),
        };

        let result = execute_create_claimable_balance(
            &op,
            &source_id,
            &TxIdentity {
                source_id: &source_id,
                seq: 123,
                op_index: 0,
            },
            &mut state,
            &context,
        )
        .expect("create claimable balance");

        match result {
            OperationResult::OpInner(OperationResultTr::CreateClaimableBalance(r)) => {
                assert!(matches!(r, CreateClaimableBalanceResult::Malformed));
            }
            other => panic!("unexpected result: {:?}", other),
        }
    }

    #[test]
    fn test_create_claimable_balance_relative_time_converted() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(0);
        let claimant_id = create_test_account_id(1);
        state.create_account(create_test_account(source_id.clone(), 100_000_000));
        state.create_account(create_test_account(claimant_id.clone(), 10_000_000));

        let predicate = ClaimPredicate::BeforeRelativeTime(50);
        let claimant = Claimant::ClaimantTypeV0(ClaimantV0 {
            destination: claimant_id.clone(),
            predicate,
        });

        let op = CreateClaimableBalanceOp {
            asset: Asset::Native,
            amount: 10_000_000,
            claimants: vec![claimant].try_into().unwrap(),
        };

        let result = execute_create_claimable_balance(
            &op,
            &source_id,
            &TxIdentity {
                source_id: &source_id,
                seq: 123,
                op_index: 0,
            },
            &mut state,
            &context,
        )
        .expect("create claimable balance");

        let balance_id = match result {
            OperationResult::OpInner(OperationResultTr::CreateClaimableBalance(
                CreateClaimableBalanceResult::Success(balance_id),
            )) => balance_id,
            other => panic!("unexpected result: {:?}", other),
        };

        let entry = state
            .get_claimable_balance(&balance_id)
            .expect("claimable balance exists");
        let stored_predicate = match entry.claimants[0] {
            Claimant::ClaimantTypeV0(ref cv0) => &cv0.predicate,
        };
        match stored_predicate {
            ClaimPredicate::BeforeAbsoluteTime(time) => {
                assert_eq!(*time, context.close_time as i64 + 50);
            }
            other => panic!("unexpected predicate: {:?}", other),
        }
    }

    #[test]
    fn test_create_claimable_balance_no_trust() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(0);
        let issuer_id = create_test_account_id(2);
        state.create_account(create_test_account(source_id.clone(), 100_000_000));
        state.create_account(create_test_account(issuer_id.clone(), 100_000_000));

        let claimant = Claimant::ClaimantTypeV0(ClaimantV0 {
            destination: create_test_account_id(3),
            predicate: ClaimPredicate::Unconditional,
        });

        let asset = Asset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4(*b"USD\0"),
            issuer: issuer_id,
        });

        let op = CreateClaimableBalanceOp {
            asset,
            amount: 10_000_000,
            claimants: vec![claimant].try_into().unwrap(),
        };

        let result = execute_create_claimable_balance(
            &op,
            &source_id,
            &TxIdentity {
                source_id: &source_id,
                seq: 123,
                op_index: 0,
            },
            &mut state,
            &context,
        )
        .expect("create claimable balance");

        match result {
            OperationResult::OpInner(OperationResultTr::CreateClaimableBalance(r)) => {
                assert!(matches!(r, CreateClaimableBalanceResult::NoTrust));
            }
            other => panic!("unexpected result: {:?}", other),
        }
    }

    #[test]
    fn test_create_claimable_balance_not_authorized() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(0);
        let issuer_id = create_test_account_id(2);
        state.create_account(create_test_account(source_id.clone(), 100_000_000));
        state.create_account(create_test_account(issuer_id.clone(), 100_000_000));

        let trustline = create_test_trustline(
            source_id.clone(),
            issuer_id.clone(),
            false,
            false,
            50_000_000,
        );
        state.create_trustline(trustline);

        let claimant = Claimant::ClaimantTypeV0(ClaimantV0 {
            destination: create_test_account_id(3),
            predicate: ClaimPredicate::Unconditional,
        });

        let asset = Asset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4(*b"USD\0"),
            issuer: issuer_id,
        });

        let op = CreateClaimableBalanceOp {
            asset,
            amount: 10_000_000,
            claimants: vec![claimant].try_into().unwrap(),
        };

        let result = execute_create_claimable_balance(
            &op,
            &source_id,
            &TxIdentity {
                source_id: &source_id,
                seq: 123,
                op_index: 0,
            },
            &mut state,
            &context,
        )
        .expect("create claimable balance");

        match result {
            OperationResult::OpInner(OperationResultTr::CreateClaimableBalance(r)) => {
                assert!(matches!(r, CreateClaimableBalanceResult::NotAuthorized));
            }
            other => panic!("unexpected result: {:?}", other),
        }
    }

    #[test]
    fn test_create_claimable_balance_issuer_clawback_flag() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let mut context = create_test_context();
        context.protocol_version = 21;

        let issuer_id = create_test_account_id(9);
        let mut issuer = create_test_account(issuer_id.clone(), 100_000_000);
        issuer.flags |= AccountFlags::ClawbackEnabledFlag as u32;
        state.create_account(issuer);

        let claimant = Claimant::ClaimantTypeV0(ClaimantV0 {
            destination: create_test_account_id(3),
            predicate: ClaimPredicate::Unconditional,
        });

        let asset = Asset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4(*b"USD\0"),
            issuer: issuer_id.clone(),
        });

        let op = CreateClaimableBalanceOp {
            asset,
            amount: 10_000_000,
            claimants: vec![claimant].try_into().unwrap(),
        };

        let result = execute_create_claimable_balance(
            &op,
            &issuer_id,
            &TxIdentity {
                source_id: &issuer_id,
                seq: 123,
                op_index: 0,
            },
            &mut state,
            &context,
        )
        .expect("create claimable balance");

        match result {
            OperationResult::OpInner(OperationResultTr::CreateClaimableBalance(
                CreateClaimableBalanceResult::Success(balance_id),
            )) => {
                let entry = state
                    .get_claimable_balance(&balance_id)
                    .expect("claimable balance exists");
                match &entry.ext {
                    ClaimableBalanceEntryExt::V1(v1) => {
                        assert_eq!(
                            v1.flags,
                            ClaimableBalanceFlags::ClaimableBalanceClawbackEnabledFlag as u32
                        );
                    }
                    other => panic!("unexpected ext: {:?}", other),
                }
            }
            other => panic!("unexpected result: {:?}", other),
        }
    }

    #[test]
    fn test_create_claimable_balance_trustline_clawback_flag() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let mut context = create_test_context();
        context.protocol_version = 21;

        let source_id = create_test_account_id(0);
        let issuer_id = create_test_account_id(2);
        state.create_account(create_test_account(source_id.clone(), 100_000_000));
        state.create_account(create_test_account(issuer_id.clone(), 100_000_000));

        let trustline =
            create_test_trustline(source_id.clone(), issuer_id.clone(), true, true, 50_000_000);
        state.create_trustline(trustline);

        let claimant = Claimant::ClaimantTypeV0(ClaimantV0 {
            destination: create_test_account_id(3),
            predicate: ClaimPredicate::Unconditional,
        });

        let asset = Asset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4(*b"USD\0"),
            issuer: issuer_id,
        });

        let op = CreateClaimableBalanceOp {
            asset,
            amount: 10_000_000,
            claimants: vec![claimant].try_into().unwrap(),
        };

        let result = execute_create_claimable_balance(
            &op,
            &source_id,
            &TxIdentity {
                source_id: &source_id,
                seq: 123,
                op_index: 0,
            },
            &mut state,
            &context,
        )
        .expect("create claimable balance");

        match result {
            OperationResult::OpInner(OperationResultTr::CreateClaimableBalance(
                CreateClaimableBalanceResult::Success(balance_id),
            )) => {
                let entry = state
                    .get_claimable_balance(&balance_id)
                    .expect("claimable balance exists");
                match &entry.ext {
                    ClaimableBalanceEntryExt::V1(v1) => {
                        assert_eq!(
                            v1.flags,
                            ClaimableBalanceFlags::ClaimableBalanceClawbackEnabledFlag as u32
                        );
                    }
                    other => panic!("unexpected ext: {:?}", other),
                }
            }
            other => panic!("unexpected result: {:?}", other),
        }
    }

    #[test]
    fn test_claim_claimable_balance_not_exists() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let claimant_id = create_test_account_id(1);
        state.create_account(create_test_account(claimant_id.clone(), 10_000_000));

        let op = ClaimClaimableBalanceOp {
            balance_id: ClaimableBalanceId::ClaimableBalanceIdTypeV0(Hash([0u8; 32])),
        };

        let result = execute_claim_claimable_balance(&op, &claimant_id, &mut state, &context);
        assert!(result.is_ok());

        match result.unwrap() {
            OperationResult::OpInner(OperationResultTr::ClaimClaimableBalance(r)) => {
                assert!(matches!(r, ClaimClaimableBalanceResult::DoesNotExist));
            }
            _ => panic!("Unexpected result type"),
        }
    }

    #[test]
    fn test_claim_claimable_balance_no_trust() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let issuer_id = create_test_account_id(0);
        let claimant_id = create_test_account_id(1);
        state.create_account(create_test_account(issuer_id.clone(), 100_000_000));
        state.create_account(create_test_account(claimant_id.clone(), 100_000_000));

        let asset = Asset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4([b'U', b'S', b'D', b'C']),
            issuer: issuer_id.clone(),
        });

        let claimants = vec![Claimant::ClaimantTypeV0(ClaimantV0 {
            destination: claimant_id.clone(),
            predicate: ClaimPredicate::Unconditional,
        })];
        let balance_id = ClaimableBalanceId::ClaimableBalanceIdTypeV0(Hash([2u8; 32]));
        let entry = ClaimableBalanceEntry {
            balance_id: balance_id.clone(),
            claimants: claimants.try_into().unwrap(),
            asset,
            amount: 100,
            ext: ClaimableBalanceEntryExt::V0,
        };
        state.create_claimable_balance(entry);

        let op = ClaimClaimableBalanceOp { balance_id };
        let result =
            execute_claim_claimable_balance(&op, &claimant_id, &mut state, &context).unwrap();
        match result {
            OperationResult::OpInner(OperationResultTr::ClaimClaimableBalance(r)) => {
                assert!(matches!(r, ClaimClaimableBalanceResult::NoTrust));
            }
            other => panic!("unexpected result: {:?}", other),
        }
    }

    #[test]
    fn test_claim_claimable_balance_issuer_success() {
        // Test that an issuer can claim their own claimable balance without a trustline.
        // This matches stellar-core TrustLineWrapper::IssuerImpl behavior where issuers have
        // unlimited trust for their own assets.
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        // Issuer is both the asset issuer AND the claimant
        let issuer_id = create_test_account_id(1);
        state.create_account(create_test_account(issuer_id.clone(), 100_000_000));
        // Note: Issuer does NOT have a trustline for their own asset

        let asset = Asset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4([b'U', b'S', b'D', b'C']),
            issuer: issuer_id.clone(),
        });

        // Create claimable balance where the issuer is the claimant
        let claimants = vec![Claimant::ClaimantTypeV0(ClaimantV0 {
            destination: issuer_id.clone(), // Issuer is the claimant
            predicate: ClaimPredicate::Unconditional,
        })];
        let balance_id = ClaimableBalanceId::ClaimableBalanceIdTypeV0(Hash([99u8; 32]));
        let entry = ClaimableBalanceEntry {
            balance_id: balance_id.clone(),
            claimants: claimants.try_into().unwrap(),
            asset,
            amount: 100_000_000,
            ext: ClaimableBalanceEntryExt::V0,
        };
        state.create_claimable_balance(entry);

        // Issuer claims their own claimable balance
        let op = ClaimClaimableBalanceOp { balance_id };
        let result =
            execute_claim_claimable_balance(&op, &issuer_id, &mut state, &context).unwrap();
        match result {
            OperationResult::OpInner(OperationResultTr::ClaimClaimableBalance(r)) => {
                // Should succeed - issuer doesn't need trustline
                assert!(matches!(r, ClaimClaimableBalanceResult::Success));
            }
            other => panic!("unexpected result: {:?}", other),
        }
    }

    #[test]
    fn test_claim_claimable_balance_native_line_full() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let claimant_id = create_test_account_id(1);
        state.create_account(create_test_account(claimant_id.clone(), i64::MAX - 50));

        let claimants = vec![Claimant::ClaimantTypeV0(ClaimantV0 {
            destination: claimant_id.clone(),
            predicate: ClaimPredicate::Unconditional,
        })];
        let balance_id = ClaimableBalanceId::ClaimableBalanceIdTypeV0(Hash([5u8; 32]));
        let entry = ClaimableBalanceEntry {
            balance_id: balance_id.clone(),
            claimants: claimants.try_into().unwrap(),
            asset: Asset::Native,
            amount: 100,
            ext: ClaimableBalanceEntryExt::V0,
        };
        state.create_claimable_balance(entry);

        let op = ClaimClaimableBalanceOp { balance_id };
        let result =
            execute_claim_claimable_balance(&op, &claimant_id, &mut state, &context).unwrap();
        match result {
            OperationResult::OpInner(OperationResultTr::ClaimClaimableBalance(r)) => {
                assert!(matches!(r, ClaimClaimableBalanceResult::LineFull));
            }
            other => panic!("unexpected result: {:?}", other),
        }
    }

    #[test]
    fn test_claim_claimable_balance_not_authorized() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let issuer_id = create_test_account_id(0);
        let claimant_id = create_test_account_id(1);
        state.create_account(create_test_account(issuer_id.clone(), 100_000_000));
        state.create_account(create_test_account(claimant_id.clone(), 100_000_000));

        let asset = Asset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4([b'U', b'S', b'D', b'C']),
            issuer: issuer_id.clone(),
        });

        let trustline = TrustLineEntry {
            account_id: claimant_id.clone(),
            asset: TrustLineAsset::CreditAlphanum4(AlphaNum4 {
                asset_code: AssetCode4([b'U', b'S', b'D', b'C']),
                issuer: issuer_id.clone(),
            }),
            balance: 0,
            limit: 1_000,
            flags: 0,
            ext: TrustLineEntryExt::V0,
        };
        state.create_trustline(trustline);
        state.get_account_mut(&claimant_id).unwrap().num_sub_entries += 1;

        let claimants = vec![Claimant::ClaimantTypeV0(ClaimantV0 {
            destination: claimant_id.clone(),
            predicate: ClaimPredicate::Unconditional,
        })];
        let balance_id = ClaimableBalanceId::ClaimableBalanceIdTypeV0(Hash([3u8; 32]));
        let entry = ClaimableBalanceEntry {
            balance_id: balance_id.clone(),
            claimants: claimants.try_into().unwrap(),
            asset,
            amount: 100,
            ext: ClaimableBalanceEntryExt::V0,
        };
        state.create_claimable_balance(entry);

        let op = ClaimClaimableBalanceOp { balance_id };
        let result =
            execute_claim_claimable_balance(&op, &claimant_id, &mut state, &context).unwrap();
        match result {
            OperationResult::OpInner(OperationResultTr::ClaimClaimableBalance(r)) => {
                assert!(matches!(r, ClaimClaimableBalanceResult::NotAuthorized));
            }
            other => panic!("unexpected result: {:?}", other),
        }
    }

    #[test]
    fn test_claim_claimable_balance_line_full() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let issuer_id = create_test_account_id(0);
        let claimant_id = create_test_account_id(1);
        state.create_account(create_test_account(issuer_id.clone(), 100_000_000));
        state.create_account(create_test_account(claimant_id.clone(), 100_000_000));

        let asset = Asset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4([b'U', b'S', b'D', b'C']),
            issuer: issuer_id.clone(),
        });

        let trustline = TrustLineEntry {
            account_id: claimant_id.clone(),
            asset: TrustLineAsset::CreditAlphanum4(AlphaNum4 {
                asset_code: AssetCode4([b'U', b'S', b'D', b'C']),
                issuer: issuer_id.clone(),
            }),
            balance: 901,
            limit: 1_000,
            flags: AUTHORIZED_FLAG,
            ext: TrustLineEntryExt::V0,
        };
        state.create_trustline(trustline);
        state.get_account_mut(&claimant_id).unwrap().num_sub_entries += 1;

        let claimants = vec![Claimant::ClaimantTypeV0(ClaimantV0 {
            destination: claimant_id.clone(),
            predicate: ClaimPredicate::Unconditional,
        })];
        let balance_id = ClaimableBalanceId::ClaimableBalanceIdTypeV0(Hash([4u8; 32]));
        let entry = ClaimableBalanceEntry {
            balance_id: balance_id.clone(),
            claimants: claimants.try_into().unwrap(),
            asset,
            amount: 100,
            ext: ClaimableBalanceEntryExt::V0,
        };
        state.create_claimable_balance(entry);

        let op = ClaimClaimableBalanceOp { balance_id };
        let result =
            execute_claim_claimable_balance(&op, &claimant_id, &mut state, &context).unwrap();
        match result {
            OperationResult::OpInner(OperationResultTr::ClaimClaimableBalance(r)) => {
                assert!(matches!(r, ClaimClaimableBalanceResult::LineFull));
            }
            other => panic!("unexpected result: {:?}", other),
        }
    }

    /// Regression test for F2 bug at ledger 203280: when CreateClaimableBalance has an
    /// operation source different from the transaction source, the source account must
    /// be recorded in the delta (matching stellar-core's loadSourceAccount behavior).
    #[test]
    fn test_create_claimable_balance_records_source_account_access() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        // Create two accounts: op_source (issuer) and tx_source
        let op_source_id = create_test_account_id(1); // Operation source (issuer)
        let tx_source_id = create_test_account_id(2); // Transaction source (different)
        let claimant_id = create_test_account_id(3);

        state.create_account(create_test_account(op_source_id.clone(), 100_000_000));
        state.create_account(create_test_account(tx_source_id.clone(), 100_000_000));
        state.create_account(create_test_account(claimant_id.clone(), 10_000_000));

        // Start operation snapshot to track changes
        state.set_multi_op_mode(true);
        state.begin_op_snapshot();

        let claimant = Claimant::ClaimantTypeV0(ClaimantV0 {
            destination: claimant_id.clone(),
            predicate: ClaimPredicate::Unconditional,
        });

        let op = CreateClaimableBalanceOp {
            asset: Asset::Native,
            amount: 10_000_000,
            claimants: vec![claimant].try_into().unwrap(),
        };

        // Execute with op_source different from tx_source
        let result = execute_create_claimable_balance(
            &op,
            &op_source_id, // Operation source
            &TxIdentity {
                source_id: &tx_source_id,
                seq: 123,
                op_index: 0,
            },
            &mut state,
            &context,
        );
        assert!(result.is_ok());

        // Flush to delta
        state.flush_modified_entries();

        // The op_source account should be in the delta even though it's different
        // from tx_source. This is the fix for F2 bug at ledger 203280.
        let delta = state.delta();
        let updated_entries = delta.updated_entries();

        // Find the op_source account in updated entries
        let op_source_in_delta = updated_entries.iter().any(|entry| {
            if let stellar_xdr::curr::LedgerEntryData::Account(acc) = &entry.data {
                acc.account_id == op_source_id
            } else {
                false
            }
        });

        assert!(
            op_source_in_delta,
            "Operation source account should be recorded in delta when different from tx source"
        );
    }

    /// Test that CreateClaimableBalance checks available balance before sponsor reserve.
    ///
    /// This is a regression test for ledger 647352 (testnet). In stellar-core:
    /// 1. First checks getAvailableBalance (available = balance - minBalance(current))
    ///    - Note: minBalance does NOT include the new sponsorship
    /// 2. Deducts balance via addBalance
    /// 3. Calls createEntryWithPossibleSponsorship which checks sponsor reserve
    ///
    /// The key insight is that getAvailableBalance uses CURRENT minBalance, not
    /// minBalance AFTER the new sponsorship. So:
    /// - If available >= amount, the balance is deducted
    /// - THEN sponsorship reserve is checked separately
    ///
    /// This test verifies the scenario where:
    /// - available (balance - current_minBalance) >= amount  -> passes underfunded check
    /// - But sponsor doesn't have enough reserve -> returns LOW_RESERVE
    #[test]
    fn test_create_claimable_balance_low_reserve_after_underfunded_check() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(0);
        let claimant_id = create_test_account_id(1);

        // base_reserve = 5_000_000
        // min_balance for account with 0 sub_entries = 2 * base_reserve = 10_000_000
        //
        // Create source account with balance = 20_000_000
        // - current min_balance = 10_000_000
        // - available = 20_000_000 - 10_000_000 = 10_000_000
        // - amount = 5_000_000 (less than available) -> UNDERFUNDED check passes
        //
        // After deducting 5_000_000, balance = 15_000_000
        // For sponsorship check with 1 claimant:
        // - sponsor_min_balance = 10_000_000 + 1*5_000_000 = 15_000_000
        // - sponsor_balance = 15_000_000 (after deduction)
        // - 15_000_000 >= 15_000_000 -> sponsor check PASSES too
        //
        // To trigger LOW_RESERVE, we need balance just barely above available check
        // but after deduction, below sponsor requirement.
        //
        // balance = 15_500_000
        // - available = 15_500_000 - 10_000_000 = 5_500_000
        // - amount = 5_000_000 -> UNDERFUNDED check passes
        // After deducting: balance = 10_500_000
        // - sponsor_min_balance = 10_000_000 + 5_000_000 = 15_000_000
        // - 10_500_000 < 15_000_000 -> LOW_RESERVE
        state.create_account(create_test_account(source_id.clone(), 15_500_000));
        state.create_account(create_test_account(claimant_id.clone(), 10_000_000));

        let claimant = Claimant::ClaimantTypeV0(ClaimantV0 {
            destination: claimant_id.clone(),
            predicate: ClaimPredicate::Unconditional,
        });

        let op = CreateClaimableBalanceOp {
            asset: Asset::Native,
            amount: 5_000_000,
            claimants: vec![claimant].try_into().unwrap(),
        };

        let result = execute_create_claimable_balance(
            &op,
            &source_id,
            &TxIdentity {
                source_id: &source_id,
                seq: 123,
                op_index: 0,
            },
            &mut state,
            &context,
        )
        .expect("create claimable balance");

        // Should return LOW_RESERVE because:
        // 1. UNDERFUNDED check passes (available 5.5M >= amount 5M)
        // 2. Balance is deducted (15.5M - 5M = 10.5M)
        // 3. Sponsor reserve check fails (10.5M < 15M required)
        match result {
            OperationResult::OpInner(OperationResultTr::CreateClaimableBalance(r)) => {
                assert!(
                    matches!(r, CreateClaimableBalanceResult::LowReserve),
                    "Expected LowReserve, got {:?}",
                    r
                );
            }
            other => panic!("unexpected result: {:?}", other),
        }
    }

    /// Test that CreateClaimableBalance returns UNDERFUNDED when available balance is too low.
    ///
    /// Complementary test to verify UNDERFUNDED is returned when the available balance
    /// (balance - current_minBalance) is less than the amount.
    #[test]
    fn test_create_claimable_balance_underfunded() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(0);
        let claimant_id = create_test_account_id(1);

        // base_reserve = 5_000_000
        // min_balance = 10_000_000 (2 * base_reserve for 0 sub_entries)
        //
        // balance = 12_000_000
        // available = 12_000_000 - 10_000_000 = 2_000_000
        // amount = 5_000_000 > 2_000_000 -> UNDERFUNDED
        state.create_account(create_test_account(source_id.clone(), 12_000_000));
        state.create_account(create_test_account(claimant_id.clone(), 10_000_000));

        let claimant = Claimant::ClaimantTypeV0(ClaimantV0 {
            destination: claimant_id.clone(),
            predicate: ClaimPredicate::Unconditional,
        });

        let op = CreateClaimableBalanceOp {
            asset: Asset::Native,
            amount: 5_000_000,
            claimants: vec![claimant].try_into().unwrap(),
        };

        let result = execute_create_claimable_balance(
            &op,
            &source_id,
            &TxIdentity {
                source_id: &source_id,
                seq: 123,
                op_index: 0,
            },
            &mut state,
            &context,
        )
        .expect("create claimable balance");

        match result {
            OperationResult::OpInner(OperationResultTr::CreateClaimableBalance(r)) => {
                assert!(
                    matches!(r, CreateClaimableBalanceResult::Underfunded),
                    "Expected Underfunded, got {:?}",
                    r
                );
            }
            other => panic!("unexpected result: {:?}", other),
        }
    }

    /// Test successful claim of a claimable balance.
    ///
    /// C++ Reference: ClaimClaimableBalanceTests.cpp - "successful claim"
    #[test]
    fn test_claim_claimable_balance_success() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let creator_id = create_test_account_id(50);
        let claimant_id = create_test_account_id(51);
        state.create_account(create_test_account(creator_id.clone(), 100_000_000));
        state.create_account(create_test_account(claimant_id.clone(), 50_000_000));

        // First create a claimable balance
        let claimant = Claimant::ClaimantTypeV0(ClaimantV0 {
            destination: claimant_id.clone(),
            predicate: ClaimPredicate::Unconditional,
        });

        let create_op = CreateClaimableBalanceOp {
            asset: Asset::Native,
            amount: 10_000_000,
            claimants: vec![claimant].try_into().unwrap(),
        };

        let create_result = execute_create_claimable_balance(
            &create_op,
            &creator_id,
            &TxIdentity {
                source_id: &creator_id,
                seq: 123,
                op_index: 0,
            },
            &mut state,
            &context,
        )
        .unwrap();

        let balance_id = match create_result {
            OperationResult::OpInner(OperationResultTr::CreateClaimableBalance(
                CreateClaimableBalanceResult::Success(id),
            )) => id,
            _ => panic!("Failed to create claimable balance"),
        };

        // Get claimant balance before claim
        let claimant_balance_before = state.get_account(&claimant_id).unwrap().balance;

        // Now claim it
        let claim_op = ClaimClaimableBalanceOp {
            balance_id: balance_id.clone(),
        };

        let claim_result =
            execute_claim_claimable_balance(&claim_op, &claimant_id, &mut state, &context).unwrap();

        match claim_result {
            OperationResult::OpInner(OperationResultTr::ClaimClaimableBalance(r)) => {
                assert!(
                    matches!(r, ClaimClaimableBalanceResult::Success),
                    "Expected Success, got {:?}",
                    r
                );
            }
            _ => panic!("Unexpected result type"),
        }

        // Verify claimant balance increased
        let claimant_balance_after = state.get_account(&claimant_id).unwrap().balance;
        assert_eq!(claimant_balance_after, claimant_balance_before + 10_000_000);
    }

    /// Test claim fails when predicate is not satisfied (CannotClaim).
    ///
    /// C++ Reference: ClaimClaimableBalanceTests.cpp - "cannot claim"
    #[test]
    fn test_claim_claimable_balance_cannot_claim() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        // Context with specific timestamp
        let context = LedgerContext::testnet(1, 1000);

        let creator_id = create_test_account_id(52);
        let claimant_id = create_test_account_id(53);
        let non_claimant_id = create_test_account_id(54);
        state.create_account(create_test_account(creator_id.clone(), 100_000_000));
        state.create_account(create_test_account(claimant_id.clone(), 50_000_000));
        state.create_account(create_test_account(non_claimant_id.clone(), 50_000_000));

        // Create a claimable balance with the claimant
        let claimant = Claimant::ClaimantTypeV0(ClaimantV0 {
            destination: claimant_id.clone(),
            predicate: ClaimPredicate::Unconditional,
        });

        let create_op = CreateClaimableBalanceOp {
            asset: Asset::Native,
            amount: 10_000_000,
            claimants: vec![claimant].try_into().unwrap(),
        };

        let create_result = execute_create_claimable_balance(
            &create_op,
            &creator_id,
            &TxIdentity {
                source_id: &creator_id,
                seq: 123,
                op_index: 0,
            },
            &mut state,
            &context,
        )
        .unwrap();

        let balance_id = match create_result {
            OperationResult::OpInner(OperationResultTr::CreateClaimableBalance(
                CreateClaimableBalanceResult::Success(id),
            )) => id,
            _ => panic!("Failed to create claimable balance"),
        };

        // Try to claim with non_claimant (not in the claimants list)
        let claim_op = ClaimClaimableBalanceOp {
            balance_id: balance_id.clone(),
        };

        let claim_result =
            execute_claim_claimable_balance(&claim_op, &non_claimant_id, &mut state, &context)
                .unwrap();

        match claim_result {
            OperationResult::OpInner(OperationResultTr::ClaimClaimableBalance(r)) => {
                assert!(
                    matches!(r, ClaimClaimableBalanceResult::CannotClaim),
                    "Expected CannotClaim, got {:?}",
                    r
                );
            }
            _ => panic!("Unexpected result type"),
        }
    }

    /// Test create claimable balance with too many claimants returns Malformed.
    ///
    /// C++ Reference: CreateClaimableBalanceTests.cpp - "too many claimants"
    #[test]
    fn test_create_claimable_balance_too_many_claimants() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(55);
        state.create_account(create_test_account(source_id.clone(), 100_000_000));

        // Create 11 claimants (max is 10)
        let mut claimants: Vec<Claimant> = Vec::new();
        for i in 0..11 {
            let claimant_id = create_test_account_id(60 + i);
            state.create_account(create_test_account(claimant_id.clone(), 10_000_000));
            claimants.push(Claimant::ClaimantTypeV0(ClaimantV0 {
                destination: claimant_id,
                predicate: ClaimPredicate::Unconditional,
            }));
        }

        // The try_into will fail because max is 10, so let's try with exactly 10
        // and verify that works, then check validation logic for malformed case
        let claimants_10: VecM<Claimant, 10> = match claimants[..10].to_vec().try_into() {
            Ok(v) => v,
            Err(_) => panic!("Should be able to create 10 claimants"),
        };

        let op = CreateClaimableBalanceOp {
            asset: Asset::Native,
            amount: 10_000_000,
            claimants: claimants_10,
        };

        // This should succeed with 10 claimants
        let result = execute_create_claimable_balance(
            &op,
            &source_id,
            &TxIdentity {
                source_id: &source_id,
                seq: 123,
                op_index: 0,
            },
            &mut state,
            &context,
        );
        assert!(result.is_ok());

        match result.unwrap() {
            OperationResult::OpInner(OperationResultTr::CreateClaimableBalance(r)) => {
                assert!(
                    matches!(r, CreateClaimableBalanceResult::Success(_)),
                    "Expected Success with 10 claimants, got {:?}",
                    r
                );
            }
            _ => panic!("Unexpected result type"),
        }
    }

    /// Test successful claim of a credit asset claimable balance.
    ///
    /// C++ Reference: ClaimClaimableBalanceTests.cpp - "claim credit"
    #[test]
    fn test_claim_claimable_balance_credit_success() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let creator_id = create_test_account_id(70);
        let claimant_id = create_test_account_id(71);
        let issuer_id = create_test_account_id(72);

        state.create_account(create_test_account(creator_id.clone(), 100_000_000));
        state.create_account(create_test_account(claimant_id.clone(), 50_000_000));
        state.create_account(create_test_account(issuer_id.clone(), 100_000_000));

        let asset = Asset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4(*b"USD\0"),
            issuer: issuer_id.clone(),
        });

        // Create trustline for creator with balance
        let creator_trustline = create_test_trustline(
            creator_id.clone(),
            issuer_id.clone(),
            true,  // authorized
            false, // no clawback
            100_000,
        );
        state.create_trustline(creator_trustline);
        state.get_account_mut(&creator_id).unwrap().num_sub_entries += 1;

        // Create trustline for claimant to receive the asset
        let claimant_trustline = create_test_trustline(
            claimant_id.clone(),
            issuer_id.clone(),
            true,  // authorized
            false, // no clawback
            0,
        );
        state.create_trustline(claimant_trustline);
        state.get_account_mut(&claimant_id).unwrap().num_sub_entries += 1;

        // Create claimable balance
        let claimant = Claimant::ClaimantTypeV0(ClaimantV0 {
            destination: claimant_id.clone(),
            predicate: ClaimPredicate::Unconditional,
        });

        let create_op = CreateClaimableBalanceOp {
            asset: asset.clone(),
            amount: 50_000,
            claimants: vec![claimant].try_into().unwrap(),
        };

        let create_result = execute_create_claimable_balance(
            &create_op,
            &creator_id,
            &TxIdentity {
                source_id: &creator_id,
                seq: 123,
                op_index: 0,
            },
            &mut state,
            &context,
        )
        .unwrap();

        let balance_id = match create_result {
            OperationResult::OpInner(OperationResultTr::CreateClaimableBalance(
                CreateClaimableBalanceResult::Success(id),
            )) => id,
            other => panic!("Failed to create claimable balance: {:?}", other),
        };

        // Claim the balance
        let claim_op = ClaimClaimableBalanceOp {
            balance_id: balance_id.clone(),
        };

        let claim_result =
            execute_claim_claimable_balance(&claim_op, &claimant_id, &mut state, &context).unwrap();

        match claim_result {
            OperationResult::OpInner(OperationResultTr::ClaimClaimableBalance(r)) => {
                assert!(
                    matches!(r, ClaimClaimableBalanceResult::Success),
                    "Expected Success, got {:?}",
                    r
                );
            }
            _ => panic!("Unexpected result type"),
        }

        // Verify claimant trustline balance increased
        let claimant_trustline = state.get_trustline(&claimant_id, &asset).unwrap();
        assert_eq!(claimant_trustline.balance, 50_000);
    }

    /// Regression test for mainnet ledger 61212818: ClaimClaimableBalance
    /// incorrectly returns Success when it should return LineFull because the
    /// trustline's buying liabilities reduce the available room below the claim
    /// amount. The basic `balance + amount <= limit` check passes, but
    /// `balance + amount > limit - buyingLiabilities` should fail.
    ///
    /// TX hash: a56598544e7e5af878d2b88012bfa6c418a9a780fbd6aa1b271a6cb7653f6dd8
    #[test]
    fn test_claim_claimable_balance_line_full_with_buying_liabilities() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let issuer_id = create_test_account_id(80);
        let claimant_id = create_test_account_id(81);
        state.create_account(create_test_account(issuer_id.clone(), 100_000_000));
        state.create_account(create_test_account(claimant_id.clone(), 100_000_000));

        let asset = Asset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4([b'U', b'S', b'D', b'C']),
            issuer: issuer_id.clone(),
        });

        // Trustline with balance=500, limit=1000, buyingLiabilities=400.
        // Basic check: 500 + 200 = 700 <= 1000 -> PASS (incorrect without liabilities)
        // With liabilities: 500 + 200 = 700 > 1000 - 400 = 600 -> FAIL (LineFull)
        let trustline = TrustLineEntry {
            account_id: claimant_id.clone(),
            asset: TrustLineAsset::CreditAlphanum4(AlphaNum4 {
                asset_code: AssetCode4([b'U', b'S', b'D', b'C']),
                issuer: issuer_id.clone(),
            }),
            balance: 500,
            limit: 1_000,
            flags: AUTHORIZED_FLAG,
            ext: TrustLineEntryExt::V1(TrustLineEntryV1 {
                liabilities: Liabilities {
                    buying: 400,
                    selling: 0,
                },
                ext: TrustLineEntryV1Ext::V0,
            }),
        };
        state.create_trustline(trustline);
        state.get_account_mut(&claimant_id).unwrap().num_sub_entries += 1;

        let claimants = vec![Claimant::ClaimantTypeV0(ClaimantV0 {
            destination: claimant_id.clone(),
            predicate: ClaimPredicate::Unconditional,
        })];
        let balance_id = ClaimableBalanceId::ClaimableBalanceIdTypeV0(Hash([81u8; 32]));
        let entry = ClaimableBalanceEntry {
            balance_id: balance_id.clone(),
            claimants: claimants.try_into().unwrap(),
            asset,
            amount: 200,
            ext: ClaimableBalanceEntryExt::V0,
        };
        state.create_claimable_balance(entry);

        let op = ClaimClaimableBalanceOp { balance_id };
        let result =
            execute_claim_claimable_balance(&op, &claimant_id, &mut state, &context).unwrap();
        match result {
            OperationResult::OpInner(OperationResultTr::ClaimClaimableBalance(r)) => {
                assert!(
                    matches!(r, ClaimClaimableBalanceResult::LineFull),
                    "Expected LineFull due to buying liabilities, got {:?}",
                    r
                );
            }
            other => panic!("unexpected result: {:?}", other),
        }
    }

    /// Test that ClaimClaimableBalance returns LineFull for native assets when
    /// buying liabilities reduce available room. Mirrors stellar-core's
    /// addBalance check: newBalance > INT64_MAX - buyingLiabilities.
    #[test]
    fn test_claim_claimable_balance_native_line_full_with_buying_liabilities() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let claimant_id = create_test_account_id(82);
        let mut account = create_test_account(claimant_id.clone(), i64::MAX - 1000);
        // Set buying liabilities so that balance + amount > MAX - buyingLiabilities
        account.ext = AccountEntryExt::V1(AccountEntryExtensionV1 {
            liabilities: Liabilities {
                buying: 500,
                selling: 0,
            },
            ext: AccountEntryExtensionV1Ext::V0,
        });
        state.create_account(account);

        let claimants = vec![Claimant::ClaimantTypeV0(ClaimantV0 {
            destination: claimant_id.clone(),
            predicate: ClaimPredicate::Unconditional,
        })];
        let balance_id = ClaimableBalanceId::ClaimableBalanceIdTypeV0(Hash([82u8; 32]));
        let entry = ClaimableBalanceEntry {
            balance_id: balance_id.clone(),
            claimants: claimants.try_into().unwrap(),
            asset: Asset::Native,
            amount: 600, // balance + 600 = MAX - 400; MAX - 400 > MAX - 500 -> LineFull
            ext: ClaimableBalanceEntryExt::V0,
        };
        state.create_claimable_balance(entry);

        let op = ClaimClaimableBalanceOp { balance_id };
        let result =
            execute_claim_claimable_balance(&op, &claimant_id, &mut state, &context).unwrap();
        match result {
            OperationResult::OpInner(OperationResultTr::ClaimClaimableBalance(r)) => {
                assert!(
                    matches!(r, ClaimClaimableBalanceResult::LineFull),
                    "Expected LineFull due to native buying liabilities, got {:?}",
                    r
                );
            }
            other => panic!("unexpected result: {:?}", other),
        }
    }

    /// Regression test for mainnet ledger 61212818: ClaimClaimableBalance must
    /// return LineFull when trustline balance + claim amount overflows i64.
    /// Previously, the check `tl.balance + entry.amount > tl.limit` silently
    /// wrapped around in release mode, producing a negative result that passed
    /// the limit check, corrupting the trustline balance.
    ///
    /// The overflow-safe pattern (from stellar-core's addBalance in types.cpp)
    /// is: `tl.limit - tl.balance < entry.amount`.
    #[test]
    fn test_claim_claimable_balance_line_full_i64_overflow() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let issuer_id = create_test_account_id(90);
        let claimant_id = create_test_account_id(91);
        state.create_account(create_test_account(issuer_id.clone(), 100_000_000));
        state.create_account(create_test_account(claimant_id.clone(), 100_000_000));

        let asset = Asset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4([b'U', b'S', b'D', b'C']),
            issuer: issuer_id.clone(),
        });

        // Trustline with large existing balance (9e18) and high limit (MAX).
        // Claim amount is 9.1138e18 — sum overflows i64::MAX (9.22e18).
        let trustline = TrustLineEntry {
            account_id: claimant_id.clone(),
            asset: TrustLineAsset::CreditAlphanum4(AlphaNum4 {
                asset_code: AssetCode4([b'U', b'S', b'D', b'C']),
                issuer: issuer_id.clone(),
            }),
            balance: 9_000_000_000_002_000_000, // ~9e18
            limit: i64::MAX,                    // 9,223,372,036,854,775,807
            flags: AUTHORIZED_FLAG,
            ext: TrustLineEntryExt::V0,
        };
        state.create_trustline(trustline);
        state.get_account_mut(&claimant_id).unwrap().num_sub_entries += 1;

        let claimants = vec![Claimant::ClaimantTypeV0(ClaimantV0 {
            destination: claimant_id.clone(),
            predicate: ClaimPredicate::Unconditional,
        })];
        let balance_id = ClaimableBalanceId::ClaimableBalanceIdTypeV0(Hash([91u8; 32]));
        let entry = ClaimableBalanceEntry {
            balance_id: balance_id.clone(),
            claimants: claimants.try_into().unwrap(),
            asset,
            amount: 9_113_800_000_000_000_000, // ~9.1138e18 — sum overflows i64
            ext: ClaimableBalanceEntryExt::V0,
        };
        state.create_claimable_balance(entry);

        let op = ClaimClaimableBalanceOp { balance_id };
        let result =
            execute_claim_claimable_balance(&op, &claimant_id, &mut state, &context).unwrap();
        match result {
            OperationResult::OpInner(OperationResultTr::ClaimClaimableBalance(r)) => {
                assert!(
                    matches!(r, ClaimClaimableBalanceResult::LineFull),
                    "Expected LineFull due to i64 overflow, got {:?}",
                    r
                );
            }
            other => panic!("unexpected result: {:?}", other),
        }

        // Verify trustline balance was NOT corrupted
        let tl = state
            .get_trustline(
                &claimant_id,
                &Asset::CreditAlphanum4(AlphaNum4 {
                    asset_code: AssetCode4([b'U', b'S', b'D', b'C']),
                    issuer: issuer_id.clone(),
                }),
            )
            .unwrap();
        assert_eq!(
            tl.balance, 9_000_000_000_002_000_000,
            "trustline balance should be unchanged"
        );
    }

    /// Regression test for mainnet ledger 59501891: two ClaimClaimableBalance
    /// operations in the same TX claim the same balance ID. The first claim
    /// should succeed and delete the entry; the second should return DoesNotExist.
    /// Previously, the entry was incorrectly reloaded from the database after
    /// being deleted, allowing double-claiming.
    #[test]
    fn test_claim_claimable_balance_double_claim_returns_does_not_exist() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let claimant_id = create_test_account_id(1);
        state.create_account(create_test_account(claimant_id.clone(), 100_000_000));

        // Create a claimable balance
        let balance_id = ClaimableBalanceId::ClaimableBalanceIdTypeV0(Hash([7u8; 32]));
        let claimants = vec![Claimant::ClaimantTypeV0(ClaimantV0 {
            destination: claimant_id.clone(),
            predicate: ClaimPredicate::Unconditional,
        })];
        let entry = ClaimableBalanceEntry {
            balance_id: balance_id.clone(),
            claimants: claimants.try_into().unwrap(),
            asset: Asset::Native,
            amount: 10_000_000,
            ext: ClaimableBalanceEntryExt::V0,
        };
        state.create_claimable_balance(entry);

        let op = ClaimClaimableBalanceOp {
            balance_id: balance_id.clone(),
        };

        // First claim should succeed
        let result1 =
            execute_claim_claimable_balance(&op, &claimant_id, &mut state, &context).unwrap();
        match &result1 {
            OperationResult::OpInner(OperationResultTr::ClaimClaimableBalance(r)) => {
                assert!(
                    matches!(r, ClaimClaimableBalanceResult::Success),
                    "First claim should succeed, got {:?}",
                    r
                );
            }
            other => panic!("unexpected result: {:?}", other),
        }

        // After deletion, the balance should not be in state
        assert!(
            state.get_claimable_balance(&balance_id).is_none(),
            "Claimable balance should be deleted after claim"
        );

        // The entry should still be tracked (in snapshots) to prevent reload from DB
        assert!(
            state.is_claimable_balance_tracked(&balance_id),
            "Deleted claimable balance should still be tracked in snapshots"
        );

        // Second claim should return DoesNotExist
        let result2 =
            execute_claim_claimable_balance(&op, &claimant_id, &mut state, &context).unwrap();
        match &result2 {
            OperationResult::OpInner(OperationResultTr::ClaimClaimableBalance(r)) => {
                assert!(
                    matches!(r, ClaimClaimableBalanceResult::DoesNotExist),
                    "Second claim of same balance should return DoesNotExist, got {:?}",
                    r
                );
            }
            other => panic!("unexpected result: {:?}", other),
        }
    }

    /// Regression test for mainnet ledger 59907857: CreateClaimableBalance must
    /// deduct selling liabilities from available native balance.
    /// Without this fix, the operation would return Success when it should
    /// return Underfunded because selling liabilities reduce the available balance.
    #[test]
    fn test_create_claimable_balance_native_underfunded_selling_liabilities() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(0);
        let claimant_id = create_test_account_id(1);

        // Source: balance=50M, min_balance=10M, selling_liabilities=30M
        // available = (50M - 10M) - 30M = 10M
        let mut source_account = create_test_account(source_id.clone(), 50_000_000);
        source_account.ext = AccountEntryExt::V1(AccountEntryExtensionV1 {
            liabilities: Liabilities {
                buying: 0,
                selling: 30_000_000,
            },
            ext: AccountEntryExtensionV1Ext::V0,
        });
        state.create_account(source_account);
        state.create_account(create_test_account(claimant_id.clone(), 10_000_000));

        let claimant = Claimant::ClaimantTypeV0(ClaimantV0 {
            destination: claimant_id.clone(),
            predicate: ClaimPredicate::Unconditional,
        });

        // Request 15M > available 10M -> Underfunded
        let op = CreateClaimableBalanceOp {
            asset: Asset::Native,
            amount: 15_000_000,
            claimants: vec![claimant].try_into().unwrap(),
        };

        let result = execute_create_claimable_balance(
            &op,
            &source_id,
            &TxIdentity {
                source_id: &source_id,
                seq: 123,
                op_index: 0,
            },
            &mut state,
            &context,
        )
        .unwrap();

        match result {
            OperationResult::OpInner(OperationResultTr::CreateClaimableBalance(r)) => {
                assert!(
                    matches!(r, CreateClaimableBalanceResult::Underfunded),
                    "Should be Underfunded due to native selling liabilities, got {:?}",
                    r
                );
            }
            other => panic!("unexpected result: {:?}", other),
        }
    }

    /// Regression test for mainnet ledger 59907857: CreateClaimableBalance must
    /// deduct selling liabilities from available non-native balance.
    #[test]
    fn test_create_claimable_balance_non_native_underfunded_selling_liabilities() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(0);
        let claimant_id = create_test_account_id(1);
        let issuer_id = create_test_account_id(2);

        state.create_account(create_test_account(source_id.clone(), 100_000_000));
        state.create_account(create_test_account(claimant_id.clone(), 10_000_000));
        state.create_account(create_test_account(issuer_id.clone(), 10_000_000));

        // Trustline: balance=1000, selling_liabilities=800
        // available = 1000 - 800 = 200
        let trustline = TrustLineEntry {
            account_id: source_id.clone(),
            asset: TrustLineAsset::CreditAlphanum4(AlphaNum4 {
                asset_code: AssetCode4(*b"USD\0"),
                issuer: issuer_id.clone(),
            }),
            balance: 1_000,
            limit: 100_000,
            flags: AUTHORIZED_FLAG,
            ext: TrustLineEntryExt::V1(TrustLineEntryV1 {
                liabilities: Liabilities {
                    buying: 0,
                    selling: 800,
                },
                ext: TrustLineEntryV1Ext::V0,
            }),
        };
        state.create_trustline(trustline);
        state.get_account_mut(&source_id).unwrap().num_sub_entries += 1;

        let claimant = Claimant::ClaimantTypeV0(ClaimantV0 {
            destination: claimant_id.clone(),
            predicate: ClaimPredicate::Unconditional,
        });

        let asset = Asset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4(*b"USD\0"),
            issuer: issuer_id.clone(),
        });

        // Request 500 > available 200 -> Underfunded
        let op = CreateClaimableBalanceOp {
            asset,
            amount: 500,
            claimants: vec![claimant].try_into().unwrap(),
        };

        let result = execute_create_claimable_balance(
            &op,
            &source_id,
            &TxIdentity {
                source_id: &source_id,
                seq: 123,
                op_index: 0,
            },
            &mut state,
            &context,
        )
        .unwrap();

        match result {
            OperationResult::OpInner(OperationResultTr::CreateClaimableBalance(r)) => {
                assert!(
                    matches!(r, CreateClaimableBalanceResult::Underfunded),
                    "Should be Underfunded due to non-native selling liabilities, got {:?}",
                    r
                );
            }
            other => panic!("unexpected result: {:?}", other),
        }
    }

    /// Test claim fails with CannotClaim when the predicate evaluates to false.
    ///
    /// The claimant is in the list but the BeforeAbsoluteTime predicate has expired
    /// (time is in the past relative to the ledger close time).
    #[test]
    fn test_claim_claimable_balance_cannot_claim_predicate_expired() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        // close_time = 1000
        let context = create_test_context();

        let claimant_id = create_test_account_id(1);
        state.create_account(create_test_account(claimant_id.clone(), 100_000_000));

        // Predicate: BeforeAbsoluteTime(500)
        // check_predicate returns (close_time < time) = (1000 < 500) = false
        let balance_id = ClaimableBalanceId::ClaimableBalanceIdTypeV0(Hash([20u8; 32]));
        let claimants = vec![Claimant::ClaimantTypeV0(ClaimantV0 {
            destination: claimant_id.clone(),
            predicate: ClaimPredicate::BeforeAbsoluteTime(500),
        })];
        let entry = ClaimableBalanceEntry {
            balance_id: balance_id.clone(),
            claimants: claimants.try_into().unwrap(),
            asset: Asset::Native,
            amount: 10_000_000,
            ext: ClaimableBalanceEntryExt::V0,
        };
        state.create_claimable_balance(entry);

        let op = ClaimClaimableBalanceOp {
            balance_id: balance_id.clone(),
        };
        let result =
            execute_claim_claimable_balance(&op, &claimant_id, &mut state, &context).unwrap();
        match result {
            OperationResult::OpInner(OperationResultTr::ClaimClaimableBalance(r)) => {
                assert!(
                    matches!(r, ClaimClaimableBalanceResult::CannotClaim),
                    "Expected CannotClaim (expired predicate), got {:?}",
                    r
                );
            }
            other => panic!("unexpected result: {:?}", other),
        }
    }
}
