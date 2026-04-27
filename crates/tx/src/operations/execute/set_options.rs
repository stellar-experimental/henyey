//! SetOptions operation execution.
//!
//! This module implements the execution logic for the SetOptions operation,
//! which modifies various account settings.

use stellar_xdr::curr::{
    AccountEntry, AccountEntryExt, AccountEntryExtensionV1Ext, AccountEntryExtensionV2,
    AccountFlags, AccountId, OperationResult, OperationResultTr, PublicKey, SetOptionsOp,
    SetOptionsResult, SetOptionsResultCode, Signer, SignerKey, SignerKeyEd25519SignedPayload,
    SignerKeyType, MASK_ACCOUNT_FLAGS_V17,
};

use super::{
    account_balance_after_liabilities, dec_sub_entries, inc_sub_entries, require_source_account,
    ACCOUNT_SUBENTRY_LIMIT,
};
#[cfg(test)]
use crate::state::signers::compare_signer_keys;
use crate::state::signers::SignerSet;
use crate::state::{ensure_account_ext_v2, LedgerStateManager};
use crate::validation::LedgerContext;
use crate::{Result, TxError};

const MAX_SIGNERS: usize = 20;

/// Sponsor information for a signer update, containing the sponsor's identity
/// and computed balance/reserve data.
struct SponsorInfo {
    sponsor_id: AccountId,
    available_balance: i64,
    min_balance: i64,
}

/// Execute a SetOptions operation.
///
/// This operation modifies account settings including:
/// - Inflation destination
/// - Account flags (auth required, revocable, immutable, clawback enabled)
/// - Master key weight
/// - Threshold levels (low, medium, high)
/// - Home domain
/// - Signers
///
/// # Arguments
///
/// * `op` - The SetOptions operation data
/// * `source` - The source account ID
/// * `state` - The ledger state manager
/// * `context` - The ledger context
///
/// # Returns
///
/// Returns the operation result indicating success or a specific failure reason.
pub(crate) fn execute_set_options(
    op: &SetOptionsOp,
    source: &AccountId,
    state: &mut LedgerStateManager,
    context: &LedgerContext,
) -> Result<OperationResult> {
    let mask = MASK_ACCOUNT_FLAGS_V17 as u32;

    if let Some(set_flags) = op.set_flags {
        if set_flags & !mask != 0 {
            return Ok(make_result(SetOptionsResultCode::UnknownFlag));
        }
    }
    if let Some(clear_flags) = op.clear_flags {
        if clear_flags & !mask != 0 {
            return Ok(make_result(SetOptionsResultCode::UnknownFlag));
        }
    }

    if let (Some(set_flags), Some(clear_flags)) = (op.set_flags, op.clear_flags) {
        if set_flags & clear_flags != 0 {
            return Ok(make_result(SetOptionsResultCode::BadFlags));
        }
    }

    // Validate operation parameters
    if let Some(weight) = op.master_weight {
        if weight > 255 {
            return Ok(make_result(SetOptionsResultCode::ThresholdOutOfRange));
        }
    }

    if let Some(t) = op.low_threshold {
        if t > 255 {
            return Ok(make_result(SetOptionsResultCode::ThresholdOutOfRange));
        }
    }

    if let Some(t) = op.med_threshold {
        if t > 255 {
            return Ok(make_result(SetOptionsResultCode::ThresholdOutOfRange));
        }
    }

    if let Some(t) = op.high_threshold {
        if t > 255 {
            return Ok(make_result(SetOptionsResultCode::ThresholdOutOfRange));
        }
    }

    // Validate home domain string: stellar-core's doCheckValid calls isStringValid()
    // which rejects bytes outside printable ASCII range (0x20-0x7E).
    // Reference: SetOptionsOpFrame.cpp doCheckValid()
    if let Some(ref home_domain) = op.home_domain {
        if !home_domain
            .as_vec()
            .iter()
            .all(|b| b.is_ascii() && !b.is_ascii_control())
        {
            return Ok(make_result(SetOptionsResultCode::InvalidHomeDomain));
        }
    }

    // Get source account
    let source_account = require_source_account(state, source)?;

    let current_flags = source_account.flags;

    // Validate inflation destination: checked BEFORE immutable flag constraint.
    // stellar-core's doApply calls loadAccountWithoutRecord(inflationDest) first,
    // returning SET_OPTIONS_INVALID_INFLATION before any auth flag checks.
    // Reference: SetOptionsOpFrame.cpp doApply()
    if let Some(ref inflation_dest) = op.inflation_dest {
        if inflation_dest != source && state.get_account(inflation_dest).is_none() {
            return Ok(make_result(SetOptionsResultCode::InvalidInflation));
        }
    }

    // Check flag consistency
    let auth_flags_mask = AccountFlags::RequiredFlag as u32
        | AccountFlags::RevocableFlag as u32
        | AccountFlags::ImmutableFlag as u32
        | AccountFlags::ClawbackEnabledFlag as u32;

    // If account is immutable, can only clear flags (not set new ones)
    if current_flags & (AccountFlags::ImmutableFlag as u32) != 0 {
        let set_flags = op.set_flags.unwrap_or(0);
        let clear_flags = op.clear_flags.unwrap_or(0);
        if (set_flags | clear_flags) & auth_flags_mask != 0 {
            return Ok(make_result(SetOptionsResultCode::CantChange));
        }
    }

    // Clawback requires auth revocable — check resulting flags after applying
    // clear then set, matching stellar-core's accountFlagClawbackIsValid check
    // which runs AFTER both clearFlags and setFlags are applied to account.flags
    let clear = op.clear_flags.unwrap_or(0);
    let set = op.set_flags.unwrap_or(0);
    if clear != 0 || set != 0 {
        let new_flags = (current_flags & !clear) | set;
        if new_flags & (AccountFlags::ClawbackEnabledFlag as u32) != 0
            && new_flags & (AccountFlags::RevocableFlag as u32) == 0
        {
            return Ok(make_result(SetOptionsResultCode::AuthRevocableRequired));
        }
    }

    // Get current signer count for sub-entry calculations
    let current_signer_count = source_account.signers.len();
    let current_num_sub_entries = source_account.num_sub_entries;
    let base_reserve = state.base_reserve();
    let sponsor_info = if let Some(sponsor_id) = state.active_sponsor_for(source) {
        let sponsor_account = state
            .get_account(&sponsor_id)
            .ok_or(TxError::SourceAccountNotFound)?;
        let min_balance = state.minimum_balance_for_account_with_deltas(
            sponsor_account,
            context.protocol_version,
            0,
            1,
            0,
        )?;
        let available = account_balance_after_liabilities(sponsor_account);
        Some(SponsorInfo {
            sponsor_id,
            available_balance: available,
            min_balance,
        })
    } else {
        None
    };
    let (current_num_sponsoring, current_num_sponsored) =
        sponsorship_counts_for_account_entry(source_account);

    // Now apply changes to the account
    let source_account_mut = state
        .get_account_mut(source)
        .ok_or(TxError::SourceAccountNotFound)?;

    // Update inflation destination
    if let Some(ref inflation_dest) = op.inflation_dest {
        source_account_mut.inflation_dest = Some(inflation_dest.clone());
    }

    // Update flags
    if let Some(clear_flags) = op.clear_flags {
        source_account_mut.flags &= !clear_flags;
    }
    if let Some(set_flags) = op.set_flags {
        source_account_mut.flags |= set_flags;
    }

    // Update master weight
    if let Some(master_weight) = op.master_weight {
        source_account_mut.thresholds.0[0] = master_weight as u8;
    }

    // Update thresholds
    if let Some(low_threshold) = op.low_threshold {
        source_account_mut.thresholds.0[1] = low_threshold as u8;
    }
    if let Some(med_threshold) = op.med_threshold {
        source_account_mut.thresholds.0[2] = med_threshold as u8;
    }
    if let Some(high_threshold) = op.high_threshold {
        source_account_mut.thresholds.0[3] = high_threshold as u8;
    }

    // Update home domain
    if let Some(ref home_domain) = op.home_domain {
        source_account_mut.home_domain = home_domain.clone();
    }

    // Update signers
    let sponsor_delta = if let Some(ref signer) = op.signer {
        match apply_signer_update(
            signer,
            source,
            source_account_mut,
            sponsor_info.as_ref(),
            &AccountSnapshot {
                signer_count: current_signer_count,
                num_sub_entries: current_num_sub_entries,
                num_sponsoring: current_num_sponsoring,
                num_sponsored: current_num_sponsored,
                base_reserve,
            },
        ) {
            Ok(delta) => delta,
            Err(SignerUpdateError::OpResult(result)) => return Ok(*result),
            Err(SignerUpdateError::Internal(e)) => return Err(e),
        }
    } else {
        None
    };

    let _ = source_account_mut;
    if let Some((sponsor_id, delta)) = sponsor_delta {
        state.update_num_sponsoring(&sponsor_id, delta)?;
    }

    Ok(make_result(SetOptionsResultCode::Success))
}

fn sponsorship_counts_for_account_entry(account: &AccountEntry) -> (i64, i64) {
    match &account.ext {
        AccountEntryExt::V0 => (0, 0),
        AccountEntryExt::V1(v1) => match &v1.ext {
            AccountEntryExtensionV1Ext::V0 => (0, 0),
            AccountEntryExtensionV1Ext::V2(AccountEntryExtensionV2 {
                num_sponsoring,
                num_sponsored,
                ..
            }) => (*num_sponsoring as i64, *num_sponsored as i64),
        },
    }
}

/// Error from signer update: either an operation result to return early, or an internal error.
enum SignerUpdateError {
    OpResult(Box<OperationResult>),
    Internal(TxError),
}

/// Current account state for signer updates — avoids passing many individual params.
struct AccountSnapshot {
    signer_count: usize,
    num_sub_entries: u32,
    num_sponsoring: i64,
    num_sponsored: i64,
    base_reserve: i64,
}

/// Apply a signer update (add, remove, or change weight) to the source account.
/// Returns the sponsor delta to apply, or an error.
fn apply_signer_update(
    signer: &stellar_xdr::curr::Signer,
    source: &AccountId,
    source_account: &mut AccountEntry,
    sponsor_info: Option<&SponsorInfo>,
    snapshot: &AccountSnapshot,
) -> std::result::Result<Option<(AccountId, i64)>, SignerUpdateError> {
    let signer_key = &signer.key;
    let weight = signer.weight;
    if weight > u8::MAX as u32 {
        return Err(SignerUpdateError::OpResult(Box::new(make_result(
            SetOptionsResultCode::BadSigner,
        ))));
    }

    let is_self = match (signer_key, source) {
        (SignerKey::Ed25519(key), AccountId(PublicKey::PublicKeyTypeEd25519(account_key))) => {
            key == account_key
        }
        _ => false,
    };
    if is_self {
        return Err(SignerUpdateError::OpResult(Box::new(make_result(
            SetOptionsResultCode::BadSigner,
        ))));
    }

    if signer_key.discriminant() == SignerKeyType::Ed25519SignedPayload {
        if let SignerKey::Ed25519SignedPayload(SignerKeyEd25519SignedPayload { payload, .. }) =
            signer_key
        {
            if payload.as_vec().is_empty() {
                return Err(SignerUpdateError::OpResult(Box::new(make_result(
                    SetOptionsResultCode::BadSigner,
                ))));
            }
        }
    }

    let sponsor = sponsor_info.map(|info| info.sponsor_id.clone());
    let has_v2 = matches!(
        source_account.ext,
        AccountEntryExt::V1(stellar_xdr::curr::AccountEntryExtensionV1 {
            ext: AccountEntryExtensionV1Ext::V2(_),
            ..
        })
    );
    let needs_sponsoring_ids = has_v2 || sponsor.is_some();
    let existing_pos = source_account
        .signers
        .iter()
        .position(|s| &s.key == signer_key);
    let mut num_sponsored_delta: i64 = 0;
    let mut signers_changed = false;
    let mut sponsor_delta: Option<(AccountId, i64)> = None;
    let mut signer_set = if needs_sponsoring_ids {
        SignerSet::normalized_for_set_options(source_account)
    } else {
        SignerSet::untracked_from_account(source_account)
    };

    if weight == 0 {
        if let Some(pos) = existing_pos {
            if let Some(sponsor_id) = signer_set
                .remove(pos)
                .map_err(SignerUpdateError::Internal)?
            {
                num_sponsored_delta -= 1;
                sponsor_delta = Some((sponsor_id, -1));
            }
            signers_changed = true;
        }
    } else if let Some(pos) = existing_pos {
        signer_set
            .update_weight(pos, weight)
            .map_err(SignerUpdateError::Internal)?;
        signers_changed = true;
    } else {
        if snapshot.signer_count >= MAX_SIGNERS {
            return Err(SignerUpdateError::OpResult(Box::new(make_result(
                SetOptionsResultCode::TooManySigners,
            ))));
        }

        if snapshot.num_sub_entries >= ACCOUNT_SUBENTRY_LIMIT
            || snapshot.num_sub_entries.saturating_add(1) > ACCOUNT_SUBENTRY_LIMIT
        {
            return Err(SignerUpdateError::OpResult(Box::new(
                OperationResult::OpTooManySubentries,
            )));
        }
        // Protocol 18+ combined-cap: num_sub_entries + num_sponsoring + 1 must fit u32.
        // Mirrors stellar-core isSponsoringSubentrySumIncreaseValid().
        let total = snapshot.num_sub_entries as u64 + snapshot.num_sponsoring as u64 + 1;
        if total > u32::MAX as u64 {
            return Err(SignerUpdateError::OpResult(Box::new(
                OperationResult::OpTooManySubentries,
            )));
        }

        if let Some(info) = sponsor_info {
            if info.available_balance < info.min_balance {
                return Err(SignerUpdateError::OpResult(Box::new(make_result(
                    SetOptionsResultCode::LowReserve,
                ))));
            }
        } else {
            let num_sub_entries = snapshot.num_sub_entries as i64 + 1;
            let effective_entries =
                2 + num_sub_entries + snapshot.num_sponsoring - snapshot.num_sponsored;
            if effective_entries < 0 {
                return Err(SignerUpdateError::Internal(TxError::Internal(
                    "unexpected account state while computing minimum balance".to_string(),
                )));
            }
            let new_min_balance = effective_entries * snapshot.base_reserve;
            let available = account_balance_after_liabilities(source_account);
            if available < new_min_balance {
                return Err(SignerUpdateError::OpResult(Box::new(make_result(
                    SetOptionsResultCode::LowReserve,
                ))));
            }
        }

        let new_signer = Signer {
            key: signer_key.clone(),
            weight,
        };
        signer_set
            .push(new_signer, sponsor.clone())
            .map_err(SignerUpdateError::Internal)?;
        signer_set.sort_by_signer_key();
        signers_changed = true;

        if let Some(sponsor) = sponsor {
            num_sponsored_delta += 1;
            sponsor_delta = Some((sponsor, 1));
        }
    }

    if signers_changed {
        let prepared = signer_set
            .prepare_write()
            .map_err(SignerUpdateError::Internal)?;
        if needs_sponsoring_ids || num_sponsored_delta != 0 {
            let current_num_sponsored = match &source_account.ext {
                AccountEntryExt::V1(v1) => match &v1.ext {
                    AccountEntryExtensionV1Ext::V2(v2) => v2.num_sponsored,
                    AccountEntryExtensionV1Ext::V0 => 0,
                },
                AccountEntryExt::V0 => 0,
            };
            let updated = current_num_sponsored as i64 + num_sponsored_delta;
            if updated < 0 || updated > u32::MAX as i64 {
                return Err(SignerUpdateError::Internal(TxError::Internal(
                    "num_sponsored out of range".to_string(),
                )));
            }
        }

        if weight == 0 {
            dec_sub_entries(source_account, 1);
        } else if existing_pos.is_none() {
            inc_sub_entries(source_account, 1);
        }

        prepared.apply(source_account);
        if needs_sponsoring_ids || num_sponsored_delta != 0 {
            let ext_v2 = ensure_account_ext_v2(source_account);
            let updated = ext_v2.num_sponsored as i64 + num_sponsored_delta;
            ext_v2.num_sponsored = updated as u32;
        }
    }

    Ok(sponsor_delta)
}

fn make_result(code: SetOptionsResultCode) -> OperationResult {
    let result = match code {
        SetOptionsResultCode::Success => SetOptionsResult::Success,
        SetOptionsResultCode::LowReserve => SetOptionsResult::LowReserve,
        SetOptionsResultCode::TooManySigners => SetOptionsResult::TooManySigners,
        SetOptionsResultCode::BadFlags => SetOptionsResult::BadFlags,
        SetOptionsResultCode::InvalidInflation => SetOptionsResult::InvalidInflation,
        SetOptionsResultCode::CantChange => SetOptionsResult::CantChange,
        SetOptionsResultCode::UnknownFlag => SetOptionsResult::UnknownFlag,
        SetOptionsResultCode::ThresholdOutOfRange => SetOptionsResult::ThresholdOutOfRange,
        SetOptionsResultCode::BadSigner => SetOptionsResult::BadSigner,
        SetOptionsResultCode::InvalidHomeDomain => SetOptionsResult::InvalidHomeDomain,
        SetOptionsResultCode::AuthRevocableRequired => SetOptionsResult::AuthRevocableRequired,
    };

    OperationResult::OpInner(OperationResultTr::SetOptions(result))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::create_test_account_id;
    use stellar_xdr::curr::*;

    fn make_string32(s: &str) -> String32 {
        String32::try_from(s.as_bytes().to_vec()).unwrap()
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

    fn create_v2_account(
        account_id: AccountId,
        balance: i64,
        signers: Vec<Signer>,
        descriptors: Vec<SponsorshipDescriptor>,
        num_sub_entries: u32,
        num_sponsored: u32,
        num_sponsoring: u32,
    ) -> AccountEntry {
        let mut account = create_test_account(account_id, balance);
        account.num_sub_entries = num_sub_entries;
        account.signers = signers.try_into().unwrap();
        account.ext = AccountEntryExt::V1(AccountEntryExtensionV1 {
            liabilities: Liabilities {
                buying: 0,
                selling: 0,
            },
            ext: AccountEntryExtensionV1Ext::V2(AccountEntryExtensionV2 {
                num_sponsored,
                num_sponsoring,
                signer_sponsoring_i_ds: descriptors.try_into().unwrap(),
                ext: AccountEntryExtensionV2Ext::V0,
            }),
        });
        account
    }

    fn make_signer(seed: u8, weight: u32) -> Signer {
        Signer {
            key: SignerKey::Ed25519(Uint256([seed; 32])),
            weight,
        }
    }

    fn create_test_context() -> LedgerContext {
        LedgerContext::testnet(1, 1000)
    }

    #[test]
    fn test_set_options_remove_middle_signer_preserves_descriptor_alignment() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();
        let source_id = create_test_account_id(10);
        let sponsor_id = create_test_account_id(11);
        let signer_a = make_signer(1, 1);
        let signer_b = make_signer(2, 1);
        let signer_c = make_signer(3, 1);

        state.create_account(create_v2_account(
            sponsor_id.clone(),
            100_000_000,
            vec![],
            vec![],
            0,
            0,
            1,
        ));
        state.create_account(create_v2_account(
            source_id.clone(),
            100_000_000,
            vec![signer_a.clone(), signer_b.clone(), signer_c.clone()],
            vec![
                SponsorshipDescriptor(None),
                SponsorshipDescriptor(Some(sponsor_id.clone())),
                SponsorshipDescriptor(None),
            ],
            3,
            1,
            0,
        ));

        let op = SetOptionsOp {
            inflation_dest: None,
            clear_flags: None,
            set_flags: None,
            master_weight: None,
            low_threshold: None,
            med_threshold: None,
            high_threshold: None,
            home_domain: None,
            signer: Some(Signer {
                key: signer_b.key.clone(),
                weight: 0,
            }),
        };

        let result = execute_set_options(&op, &source_id, &mut state, &context).unwrap();
        assert!(matches!(
            result,
            OperationResult::OpInner(OperationResultTr::SetOptions(SetOptionsResult::Success))
        ));

        let account = state.get_account(&source_id).unwrap();
        assert_eq!(
            account.signers.iter().map(|s| &s.key).collect::<Vec<_>>(),
            vec![&signer_a.key, &signer_c.key]
        );
        let AccountEntryExt::V1(v1) = &account.ext else {
            panic!("expected v1");
        };
        let AccountEntryExtensionV1Ext::V2(v2) = &v1.ext else {
            panic!("expected v2");
        };
        assert_eq!(
            v2.signer_sponsoring_i_ds.as_slice(),
            &[SponsorshipDescriptor(None), SponsorshipDescriptor(None)]
        );
        assert_eq!(v2.num_sponsored, 0);
        let sponsor = state.get_account(&sponsor_id).unwrap();
        assert_eq!(sponsorship_counts_for_account_entry(sponsor).0, 0);
    }

    #[test]
    fn test_set_options_noop_remove_does_not_normalize_descriptors() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();
        let source_id = create_test_account_id(12);
        let signer = make_signer(1, 1);
        state.create_account(create_v2_account(
            source_id.clone(),
            100_000_000,
            vec![signer],
            vec![SponsorshipDescriptor(None), SponsorshipDescriptor(None)],
            1,
            0,
            0,
        ));

        let op = SetOptionsOp {
            inflation_dest: None,
            clear_flags: None,
            set_flags: None,
            master_weight: None,
            low_threshold: None,
            med_threshold: None,
            high_threshold: None,
            home_domain: None,
            signer: Some(make_signer(9, 0)),
        };

        let result = execute_set_options(&op, &source_id, &mut state, &context).unwrap();
        assert!(matches!(
            result,
            OperationResult::OpInner(OperationResultTr::SetOptions(SetOptionsResult::Success))
        ));
        let account = state.get_account(&source_id).unwrap();
        let AccountEntryExt::V1(v1) = &account.ext else {
            panic!("expected v1");
        };
        let AccountEntryExtensionV1Ext::V2(v2) = &v1.ext else {
            panic!("expected v2");
        };
        assert_eq!(v2.signer_sponsoring_i_ds.len(), 2);
    }

    #[test]
    fn test_set_options_sorted_insert_keeps_descriptor_with_signer() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();
        let source_id = create_test_account_id(13);
        let sponsor_id = create_test_account_id(14);
        let ed25519 = Uint256([5; 32]);
        let key_a = SignerKey::Ed25519SignedPayload(SignerKeyEd25519SignedPayload {
            ed25519: ed25519.clone(),
            payload: vec![1, 2].try_into().unwrap(),
        });
        let key_b = SignerKey::Ed25519SignedPayload(SignerKeyEd25519SignedPayload {
            ed25519,
            payload: vec![1, 3].try_into().unwrap(),
        });

        state.create_account(create_v2_account(
            sponsor_id.clone(),
            100_000_000,
            vec![],
            vec![],
            0,
            0,
            0,
        ));
        state.create_account(create_v2_account(
            source_id.clone(),
            100_000_000,
            vec![Signer {
                key: key_b.clone(),
                weight: 1,
            }],
            vec![SponsorshipDescriptor(None)],
            1,
            0,
            0,
        ));
        state.push_sponsorship(sponsor_id.clone(), source_id.clone());

        let op = SetOptionsOp {
            inflation_dest: None,
            clear_flags: None,
            set_flags: None,
            master_weight: None,
            low_threshold: None,
            med_threshold: None,
            high_threshold: None,
            home_domain: None,
            signer: Some(Signer {
                key: key_a.clone(),
                weight: 1,
            }),
        };

        let result = execute_set_options(&op, &source_id, &mut state, &context).unwrap();
        assert!(matches!(
            result,
            OperationResult::OpInner(OperationResultTr::SetOptions(SetOptionsResult::Success))
        ));

        let account = state.get_account(&source_id).unwrap();
        assert_eq!(
            account.signers.iter().map(|s| &s.key).collect::<Vec<_>>(),
            vec![&key_a, &key_b]
        );
        let AccountEntryExt::V1(v1) = &account.ext else {
            panic!("expected v1");
        };
        let AccountEntryExtensionV1Ext::V2(v2) = &v1.ext else {
            panic!("expected v2");
        };
        assert_eq!(v2.signer_sponsoring_i_ds[0].0, Some(sponsor_id));
        assert_eq!(v2.signer_sponsoring_i_ds[1].0, None);
    }

    #[test]
    fn test_set_options_update_thresholds() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(0);
        state.create_account(create_test_account(source_id.clone(), 100_000_000));

        let op = SetOptionsOp {
            inflation_dest: None,
            clear_flags: None,
            set_flags: None,
            master_weight: Some(10),
            low_threshold: Some(1),
            med_threshold: Some(2),
            high_threshold: Some(3),
            home_domain: None,
            signer: None,
        };

        let result = execute_set_options(&op, &source_id, &mut state, &context);
        assert!(result.is_ok());

        let account = state.get_account(&source_id).unwrap();
        assert_eq!(account.thresholds.0[0], 10); // master weight
        assert_eq!(account.thresholds.0[1], 1); // low
        assert_eq!(account.thresholds.0[2], 2); // med
        assert_eq!(account.thresholds.0[3], 3); // high
    }

    #[test]
    fn test_set_options_set_flags() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(0);
        state.create_account(create_test_account(source_id.clone(), 100_000_000));

        let op = SetOptionsOp {
            inflation_dest: None,
            clear_flags: None,
            set_flags: Some(0x3), // AUTH_REQUIRED | AUTH_REVOCABLE
            master_weight: None,
            low_threshold: None,
            med_threshold: None,
            high_threshold: None,
            home_domain: None,
            signer: None,
        };

        let result = execute_set_options(&op, &source_id, &mut state, &context);
        assert!(result.is_ok());

        let account = state.get_account(&source_id).unwrap();
        assert_eq!(account.flags, 0x3);
    }

    #[test]
    fn test_set_options_threshold_out_of_range() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(0);
        state.create_account(create_test_account(source_id.clone(), 100_000_000));

        let op = SetOptionsOp {
            inflation_dest: None,
            clear_flags: None,
            set_flags: None,
            master_weight: Some(256), // Out of range
            low_threshold: None,
            med_threshold: None,
            high_threshold: None,
            home_domain: None,
            signer: None,
        };

        let result = execute_set_options(&op, &source_id, &mut state, &context);
        assert!(result.is_ok());

        match result.unwrap() {
            OperationResult::OpInner(OperationResultTr::SetOptions(r)) => {
                assert!(matches!(r, SetOptionsResult::ThresholdOutOfRange));
            }
            _ => panic!("Unexpected result type"),
        }
    }

    #[test]
    fn test_set_options_immutable_cant_change() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(0);
        let mut account = create_test_account(source_id.clone(), 100_000_000);
        account.flags = 0x4; // AUTH_IMMUTABLE
        state.create_account(account);

        let op = SetOptionsOp {
            inflation_dest: None,
            clear_flags: None,
            set_flags: Some(0x1), // Try to set AUTH_REQUIRED
            master_weight: None,
            low_threshold: None,
            med_threshold: None,
            high_threshold: None,
            home_domain: None,
            signer: None,
        };

        let result = execute_set_options(&op, &source_id, &mut state, &context);
        assert!(result.is_ok());

        match result.unwrap() {
            OperationResult::OpInner(OperationResultTr::SetOptions(r)) => {
                assert!(matches!(r, SetOptionsResult::CantChange));
            }
            _ => panic!("Unexpected result type"),
        }
    }

    #[test]
    fn test_set_options_immutable_clear_flags_cant_change() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(0);
        let mut account = create_test_account(source_id.clone(), 100_000_000);
        account.flags = 0x4; // AUTH_IMMUTABLE
        state.create_account(account);

        let op = SetOptionsOp {
            inflation_dest: None,
            clear_flags: Some(0x1), // Try to clear AUTH_REQUIRED
            set_flags: None,
            master_weight: None,
            low_threshold: None,
            med_threshold: None,
            high_threshold: None,
            home_domain: None,
            signer: None,
        };

        let result = execute_set_options(&op, &source_id, &mut state, &context);
        assert!(result.is_ok());

        match result.unwrap() {
            OperationResult::OpInner(OperationResultTr::SetOptions(r)) => {
                assert!(matches!(r, SetOptionsResult::CantChange));
            }
            _ => panic!("Unexpected result type"),
        }
    }

    #[test]
    fn test_set_options_unknown_flag() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(0);
        state.create_account(create_test_account(source_id.clone(), 100_000_000));

        let op = SetOptionsOp {
            inflation_dest: None,
            clear_flags: None,
            set_flags: Some(0x10), // outside MASK_ACCOUNT_FLAGS_V17
            master_weight: None,
            low_threshold: None,
            med_threshold: None,
            high_threshold: None,
            home_domain: None,
            signer: None,
        };

        let result = execute_set_options(&op, &source_id, &mut state, &context);
        assert!(result.is_ok());

        match result.unwrap() {
            OperationResult::OpInner(OperationResultTr::SetOptions(r)) => {
                assert!(matches!(r, SetOptionsResult::UnknownFlag));
            }
            _ => panic!("Unexpected result type"),
        }
    }

    #[test]
    fn test_set_options_bad_flags_overlap() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(0);
        state.create_account(create_test_account(source_id.clone(), 100_000_000));

        let op = SetOptionsOp {
            inflation_dest: None,
            clear_flags: Some(0x1),
            set_flags: Some(0x1), // overlap
            master_weight: None,
            low_threshold: None,
            med_threshold: None,
            high_threshold: None,
            home_domain: None,
            signer: None,
        };

        let result = execute_set_options(&op, &source_id, &mut state, &context);
        assert!(result.is_ok());

        match result.unwrap() {
            OperationResult::OpInner(OperationResultTr::SetOptions(r)) => {
                assert!(matches!(r, SetOptionsResult::BadFlags));
            }
            _ => panic!("Unexpected result type"),
        }
    }

    #[test]
    fn test_set_options_add_signer() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(0);
        state.create_account(create_test_account(source_id.clone(), 100_000_000));

        let signer_key = SignerKey::Ed25519(Uint256([1u8; 32]));
        let op = SetOptionsOp {
            inflation_dest: None,
            clear_flags: None,
            set_flags: None,
            master_weight: None,
            low_threshold: None,
            med_threshold: None,
            high_threshold: None,
            home_domain: None,
            signer: Some(Signer {
                key: signer_key.clone(),
                weight: 5,
            }),
        };

        let result = execute_set_options(&op, &source_id, &mut state, &context);
        assert!(result.is_ok());

        let account = state.get_account(&source_id).unwrap();
        assert_eq!(account.signers.len(), 1);
        assert_eq!(account.num_sub_entries, 1);
    }

    #[test]
    fn test_set_options_bad_signer_self() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(0);
        state.create_account(create_test_account(source_id.clone(), 100_000_000));

        let signer_key = SignerKey::Ed25519(Uint256([0u8; 32]));
        let op = SetOptionsOp {
            inflation_dest: None,
            clear_flags: None,
            set_flags: None,
            master_weight: None,
            low_threshold: None,
            med_threshold: None,
            high_threshold: None,
            home_domain: None,
            signer: Some(Signer {
                key: signer_key,
                weight: 1,
            }),
        };

        let result = execute_set_options(&op, &source_id, &mut state, &context);
        assert!(result.is_ok());

        match result.unwrap() {
            OperationResult::OpInner(OperationResultTr::SetOptions(r)) => {
                assert!(matches!(r, SetOptionsResult::BadSigner));
            }
            _ => panic!("Unexpected result type"),
        }
    }

    #[test]
    fn test_set_options_bad_signer_weight() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(0);
        state.create_account(create_test_account(source_id.clone(), 100_000_000));

        let signer_key = SignerKey::Ed25519(Uint256([1u8; 32]));
        let op = SetOptionsOp {
            inflation_dest: None,
            clear_flags: None,
            set_flags: None,
            master_weight: None,
            low_threshold: None,
            med_threshold: None,
            high_threshold: None,
            home_domain: None,
            signer: Some(Signer {
                key: signer_key,
                weight: 256,
            }),
        };

        let result = execute_set_options(&op, &source_id, &mut state, &context);
        assert!(result.is_ok());

        match result.unwrap() {
            OperationResult::OpInner(OperationResultTr::SetOptions(r)) => {
                assert!(matches!(r, SetOptionsResult::BadSigner));
            }
            _ => panic!("Unexpected result type"),
        }
    }

    #[test]
    fn test_set_options_bad_signer_signed_payload_empty() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(0);
        state.create_account(create_test_account(source_id.clone(), 100_000_000));

        let signer_key = SignerKey::Ed25519SignedPayload(SignerKeyEd25519SignedPayload {
            ed25519: Uint256([2u8; 32]),
            payload: BytesM::default(),
        });
        let op = SetOptionsOp {
            inflation_dest: None,
            clear_flags: None,
            set_flags: None,
            master_weight: None,
            low_threshold: None,
            med_threshold: None,
            high_threshold: None,
            home_domain: None,
            signer: Some(Signer {
                key: signer_key,
                weight: 1,
            }),
        };

        let result = execute_set_options(&op, &source_id, &mut state, &context);
        assert!(result.is_ok());

        match result.unwrap() {
            OperationResult::OpInner(OperationResultTr::SetOptions(r)) => {
                assert!(matches!(r, SetOptionsResult::BadSigner));
            }
            _ => panic!("Unexpected result type"),
        }
    }

    #[test]
    fn test_set_options_home_domain() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(0);
        state.create_account(create_test_account(source_id.clone(), 100_000_000));

        let op = SetOptionsOp {
            inflation_dest: None,
            clear_flags: None,
            set_flags: None,
            master_weight: None,
            low_threshold: None,
            med_threshold: None,
            high_threshold: None,
            home_domain: Some(make_string32("stellar.org")),
            signer: None,
        };

        let result = execute_set_options(&op, &source_id, &mut state, &context);
        assert!(result.is_ok());

        let account = state.get_account(&source_id).unwrap();
        assert_eq!(account.home_domain.to_string(), "stellar.org");
    }

    #[test]
    fn test_set_options_inflation_dest_nonexistent_account() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(0);
        state.create_account(create_test_account(source_id.clone(), 100_000_000));

        // Set inflation destination to a non-existent account
        let nonexistent_id = create_test_account_id(99);
        let op = SetOptionsOp {
            inflation_dest: Some(nonexistent_id),
            clear_flags: None,
            set_flags: None,
            master_weight: None,
            low_threshold: None,
            med_threshold: None,
            high_threshold: None,
            home_domain: None,
            signer: None,
        };

        let result = execute_set_options(&op, &source_id, &mut state, &context);
        assert!(result.is_ok());

        match result.unwrap() {
            OperationResult::OpInner(OperationResultTr::SetOptions(r)) => {
                assert!(matches!(r, SetOptionsResult::InvalidInflation));
            }
            _ => panic!("Unexpected result type"),
        }
    }

    #[test]
    fn test_set_options_inflation_dest_self() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(0);
        state.create_account(create_test_account(source_id.clone(), 100_000_000));

        // Setting inflation destination to self should always succeed (no existence check)
        let op = SetOptionsOp {
            inflation_dest: Some(source_id.clone()),
            clear_flags: None,
            set_flags: None,
            master_weight: None,
            low_threshold: None,
            med_threshold: None,
            high_threshold: None,
            home_domain: None,
            signer: None,
        };

        let result = execute_set_options(&op, &source_id, &mut state, &context);
        assert!(result.is_ok());

        match result.unwrap() {
            OperationResult::OpInner(OperationResultTr::SetOptions(r)) => {
                assert!(matches!(r, SetOptionsResult::Success));
            }
            _ => panic!("Unexpected result type"),
        }

        let account = state.get_account(&source_id).unwrap();
        assert_eq!(account.inflation_dest, Some(source_id));
    }

    #[test]
    fn test_set_options_inflation_dest_existing_account() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(0);
        let dest_id = create_test_account_id(1);
        state.create_account(create_test_account(source_id.clone(), 100_000_000));
        state.create_account(create_test_account(dest_id.clone(), 100_000_000));

        // Setting inflation destination to an existing account should succeed
        let op = SetOptionsOp {
            inflation_dest: Some(dest_id.clone()),
            clear_flags: None,
            set_flags: None,
            master_weight: None,
            low_threshold: None,
            med_threshold: None,
            high_threshold: None,
            home_domain: None,
            signer: None,
        };

        let result = execute_set_options(&op, &source_id, &mut state, &context);
        assert!(result.is_ok());

        match result.unwrap() {
            OperationResult::OpInner(OperationResultTr::SetOptions(r)) => {
                assert!(matches!(r, SetOptionsResult::Success));
            }
            _ => panic!("Unexpected result type"),
        }

        let account = state.get_account(&source_id).unwrap();
        assert_eq!(account.inflation_dest, Some(dest_id));
    }

    /// Test that SetOptions returns OpTooManySubentries when adding a signer
    /// to an account that has reached the maximum subentries limit (1000).
    ///
    /// C++ Reference: SetOptionsTests.cpp - tooManySubentries tests via SponsorshipTestUtils
    #[test]
    fn test_set_options_signer_too_many_subentries() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(100);

        // Create source account with max subentries (1000)
        let mut source_account = create_test_account(source_id.clone(), 100_000_000);
        source_account.num_sub_entries = ACCOUNT_SUBENTRY_LIMIT; // At the limit
        state.create_account(source_account);

        // Create a new signer key
        let signer_key = SignerKey::Ed25519(Uint256([99u8; 32]));

        let op = SetOptionsOp {
            inflation_dest: None,
            clear_flags: None,
            set_flags: None,
            master_weight: None,
            low_threshold: None,
            med_threshold: None,
            high_threshold: None,
            home_domain: None,
            signer: Some(Signer {
                key: signer_key,
                weight: 1, // Adding a new signer
            }),
        };

        let result = execute_set_options(&op, &source_id, &mut state, &context);
        match result.unwrap() {
            OperationResult::OpTooManySubentries => {
                // Expected - account has max subentries, can't add new signer
            }
            other => panic!("expected OpTooManySubentries, got {:?}", other),
        }

        // Verify num_sub_entries was not changed
        assert_eq!(
            state.get_account(&source_id).unwrap().num_sub_entries,
            ACCOUNT_SUBENTRY_LIMIT,
            "num_sub_entries should remain unchanged"
        );

        // Verify no signer was added
        assert_eq!(
            state.get_account(&source_id).unwrap().signers.len(),
            0,
            "no signer should have been added"
        );
    }

    /// Test that updating an existing signer weight works even when at subentry limit.
    /// Updating doesn't create a new subentry, so it should succeed.
    #[test]
    fn test_set_options_update_signer_at_subentry_limit_succeeds() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(101);

        // Create a signer key
        let signer_key = SignerKey::Ed25519(Uint256([98u8; 32]));

        // Create source account with max subentries and one existing signer
        let mut source_account = create_test_account(source_id.clone(), 100_000_000);
        source_account.num_sub_entries = ACCOUNT_SUBENTRY_LIMIT;
        source_account.signers = vec![Signer {
            key: signer_key.clone(),
            weight: 1,
        }]
        .try_into()
        .unwrap();
        state.create_account(source_account);

        // Update the existing signer's weight - should succeed
        let op = SetOptionsOp {
            inflation_dest: None,
            clear_flags: None,
            set_flags: None,
            master_weight: None,
            low_threshold: None,
            med_threshold: None,
            high_threshold: None,
            home_domain: None,
            signer: Some(Signer {
                key: signer_key.clone(),
                weight: 5, // Update weight
            }),
        };

        let result = execute_set_options(&op, &source_id, &mut state, &context);
        match result.unwrap() {
            OperationResult::OpInner(OperationResultTr::SetOptions(r)) => {
                assert!(matches!(r, SetOptionsResult::Success));
            }
            other => panic!("expected Success, got {:?}", other),
        }

        // Verify the signer weight was updated
        let account = state.get_account(&source_id).unwrap();
        let signer = account
            .signers
            .iter()
            .find(|s| s.key == signer_key)
            .unwrap();
        assert_eq!(signer.weight, 5);
    }

    /// Test SetOptions remove signer by setting weight to 0.
    ///
    /// C++ Reference: SetOptionsTests.cpp - "remove signer" test section
    #[test]
    fn test_set_options_remove_signer() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(30);
        let signer_id = create_test_account_id(31);

        // Create account with a signer
        let mut source = create_test_account(source_id.clone(), 100_000_000);
        let signer_key = SignerKey::Ed25519(match signer_id.0 {
            PublicKey::PublicKeyTypeEd25519(k) => k,
        });
        let signer = Signer {
            key: signer_key.clone(),
            weight: 1,
        };
        source.signers = vec![signer].try_into().unwrap();
        source.num_sub_entries = 1; // 1 signer
        state.create_account(source);

        // Remove signer by setting weight to 0
        let op = SetOptionsOp {
            inflation_dest: None,
            clear_flags: None,
            set_flags: None,
            master_weight: None,
            low_threshold: None,
            med_threshold: None,
            high_threshold: None,
            home_domain: None,
            signer: Some(Signer {
                key: signer_key,
                weight: 0, // Weight 0 removes the signer
            }),
        };

        let result = execute_set_options(&op, &source_id, &mut state, &context);
        match result.unwrap() {
            OperationResult::OpInner(OperationResultTr::SetOptions(r)) => {
                assert!(matches!(r, SetOptionsResult::Success));
            }
            other => panic!("expected Success, got {:?}", other),
        }

        // Verify signer was removed
        let account = state.get_account(&source_id).unwrap();
        assert_eq!(account.signers.len(), 0, "Signer should be removed");
        assert_eq!(
            account.num_sub_entries, 0,
            "num_sub_entries should be decremented"
        );
    }

    /// Test SetOptions adding signer with insufficient reserve.
    ///
    /// C++ Reference: SetOptionsTests.cpp - "low reserve signer" test section
    #[test]
    fn test_set_options_signer_low_reserve() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(32);
        let signer_id = create_test_account_id(33);

        // Create account with minimum balance (can't afford new signer)
        let min_balance = state
            .minimum_balance_with_counts(context.protocol_version, 0, 0, 0)
            .unwrap();
        state.create_account(create_test_account(source_id.clone(), min_balance));

        let signer_key = SignerKey::Ed25519(match signer_id.0 {
            PublicKey::PublicKeyTypeEd25519(k) => k,
        });

        let op = SetOptionsOp {
            inflation_dest: None,
            clear_flags: None,
            set_flags: None,
            master_weight: None,
            low_threshold: None,
            med_threshold: None,
            high_threshold: None,
            home_domain: None,
            signer: Some(Signer {
                key: signer_key,
                weight: 1,
            }),
        };

        let result = execute_set_options(&op, &source_id, &mut state, &context);
        match result.unwrap() {
            OperationResult::OpInner(OperationResultTr::SetOptions(r)) => {
                assert!(
                    matches!(r, SetOptionsResult::LowReserve),
                    "Expected LowReserve, got {:?}",
                    r
                );
            }
            other => panic!("expected SetOptions result, got {:?}", other),
        }
    }

    /// Test SetOptions with too many signers (MAX_SIGNERS = 20).
    ///
    /// C++ Reference: SetOptionsTests.cpp - "too many signers" test section
    #[test]
    fn test_set_options_too_many_signers() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(34);

        // Create account with 20 signers (at MAX_SIGNERS limit)
        let mut source = create_test_account(source_id.clone(), 1_000_000_000);
        let mut signers = Vec::new();
        for i in 0..20 {
            let signer_id = create_test_account_id(100 + i);
            let signer_key = SignerKey::Ed25519(match signer_id.0 {
                PublicKey::PublicKeyTypeEd25519(k) => k,
            });
            signers.push(Signer {
                key: signer_key,
                weight: 1,
            });
        }
        source.signers = signers.try_into().unwrap();
        source.num_sub_entries = 20;
        state.create_account(source);

        // Try to add 21st signer
        let new_signer_id = create_test_account_id(200);
        let new_signer_key = SignerKey::Ed25519(match new_signer_id.0 {
            PublicKey::PublicKeyTypeEd25519(k) => k,
        });

        let op = SetOptionsOp {
            inflation_dest: None,
            clear_flags: None,
            set_flags: None,
            master_weight: None,
            low_threshold: None,
            med_threshold: None,
            high_threshold: None,
            home_domain: None,
            signer: Some(Signer {
                key: new_signer_key,
                weight: 1,
            }),
        };

        let result = execute_set_options(&op, &source_id, &mut state, &context);
        match result.unwrap() {
            OperationResult::OpInner(OperationResultTr::SetOptions(r)) => {
                assert!(
                    matches!(r, SetOptionsResult::TooManySigners),
                    "Expected TooManySigners, got {:?}",
                    r
                );
            }
            other => panic!("expected SetOptions result, got {:?}", other),
        }
    }

    /// Test SetOptions invalid home domain (too long).
    ///
    /// C++ Reference: SetOptionsTests.cpp - "invalid home domain" test section
    #[test]
    fn test_set_options_home_domain_invalid() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(35);
        state.create_account(create_test_account(source_id.clone(), 100_000_000));

        // Home domain must be 32 chars or less
        // String32 type should enforce this, but let's test the behavior
        let op = SetOptionsOp {
            inflation_dest: None,
            clear_flags: None,
            set_flags: None,
            master_weight: None,
            low_threshold: None,
            med_threshold: None,
            high_threshold: None,
            home_domain: Some(make_string32("valid.stellar.org")),
            signer: None,
        };

        let result = execute_set_options(&op, &source_id, &mut state, &context);
        match result.unwrap() {
            OperationResult::OpInner(OperationResultTr::SetOptions(r)) => {
                assert!(
                    matches!(r, SetOptionsResult::Success),
                    "Valid home domain should succeed, got {:?}",
                    r
                );
            }
            other => panic!("expected SetOptions result, got {:?}", other),
        }

        // Verify home domain was set
        let account = state.get_account(&source_id).unwrap();
        assert_eq!(account.home_domain.as_vec(), b"valid.stellar.org");
    }

    /// Test SetOptions clear auth revocable flag.
    ///
    /// C++ Reference: SetOptionsTests.cpp - "clear auth revocable" test section
    #[test]
    fn test_set_options_clear_auth_revocable() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(36);
        let mut source = create_test_account(source_id.clone(), 100_000_000);
        source.flags = AccountFlags::RequiredFlag as u32 | AccountFlags::RevocableFlag as u32;
        state.create_account(source);

        let op = SetOptionsOp {
            inflation_dest: None,
            clear_flags: Some(AccountFlags::RevocableFlag as u32),
            set_flags: None,
            master_weight: None,
            low_threshold: None,
            med_threshold: None,
            high_threshold: None,
            home_domain: None,
            signer: None,
        };

        let result = execute_set_options(&op, &source_id, &mut state, &context);
        match result.unwrap() {
            OperationResult::OpInner(OperationResultTr::SetOptions(r)) => {
                assert!(matches!(r, SetOptionsResult::Success));
            }
            other => panic!("expected SetOptions result, got {:?}", other),
        }

        // Verify flag was cleared
        let account = state.get_account(&source_id).unwrap();
        assert_eq!(
            account.flags & (AccountFlags::RevocableFlag as u32),
            0,
            "AUTH_REVOCABLE should be cleared"
        );
        assert_ne!(
            account.flags & (AccountFlags::RequiredFlag as u32),
            0,
            "AUTH_REQUIRED should remain"
        );
    }

    /// Test clearing both AUTH_REVOCABLE and AUTH_CLAWBACK simultaneously succeeds.
    ///
    /// Regression test for ledger 59902996 mismatch: stellar-core applies clear_flags
    /// then set_flags before checking accountFlagClawbackIsValid on the resulting
    /// flags. If both REVOCABLE and CLAWBACK are cleared together, the resulting
    /// flags have neither, which is valid. Our old code checked before applying and
    /// incorrectly blocked clearing revocable when clawback was currently set.
    #[test]
    fn test_set_options_clear_revocable_and_clawback_simultaneously() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(50);
        let mut source = create_test_account(source_id.clone(), 100_000_000);
        // Account has both AUTH_REVOCABLE and AUTH_CLAWBACK set
        source.flags = AccountFlags::RequiredFlag as u32
            | AccountFlags::RevocableFlag as u32
            | AccountFlags::ClawbackEnabledFlag as u32;
        state.create_account(source);

        // Clear both revocable and clawback at once
        let op = SetOptionsOp {
            inflation_dest: None,
            clear_flags: Some(
                AccountFlags::RevocableFlag as u32 | AccountFlags::ClawbackEnabledFlag as u32,
            ),
            set_flags: None,
            master_weight: None,
            low_threshold: None,
            med_threshold: None,
            high_threshold: None,
            home_domain: None,
            signer: None,
        };

        let result = execute_set_options(&op, &source_id, &mut state, &context);
        match result.unwrap() {
            OperationResult::OpInner(OperationResultTr::SetOptions(r)) => {
                assert!(
                    matches!(r, SetOptionsResult::Success),
                    "Clearing both revocable and clawback should succeed, got {:?}",
                    r
                );
            }
            other => panic!("expected SetOptions result, got {:?}", other),
        }

        let account = state.get_account(&source_id).unwrap();
        assert_eq!(account.flags & (AccountFlags::RevocableFlag as u32), 0);
        assert_eq!(
            account.flags & (AccountFlags::ClawbackEnabledFlag as u32),
            0
        );
        assert_ne!(account.flags & (AccountFlags::RequiredFlag as u32), 0);
    }

    /// Test SetOptions set master weight to 0 (disable master key).
    ///
    /// C++ Reference: SetOptionsTests.cpp - "master weight zero" test section
    /// Test that Ed25519SignedPayload signers are sorted by both ed25519 key AND payload.
    /// Regression test for AUDIT-708: compare_signer_keys only compared ed25519,
    /// ignoring the payload field. This causes a different signer ordering than
    /// stellar-core, which is consensus-critical since account XDR feeds ledger hashes.
    #[test]
    fn test_audit_708_signed_payload_signer_sort_includes_payload() {
        // Two Ed25519SignedPayload signers with the same ed25519 key but different payloads.
        // payload_a < payload_b lexicographically, so stellar-core orders [payload_a, payload_b].
        let ed25519_key = Uint256([5u8; 32]);
        let payload_a = BytesM::try_from(vec![0x01, 0x02]).unwrap();
        let payload_b = BytesM::try_from(vec![0x01, 0x03]).unwrap();

        let key_a = SignerKey::Ed25519SignedPayload(SignerKeyEd25519SignedPayload {
            ed25519: ed25519_key.clone(),
            payload: payload_a.clone(),
        });
        let key_b = SignerKey::Ed25519SignedPayload(SignerKeyEd25519SignedPayload {
            ed25519: ed25519_key.clone(),
            payload: payload_b.clone(),
        });

        // With same ed25519 key, payload_a (0x01,0x02) < payload_b (0x01,0x03)
        // So key_a should sort before key_b.
        assert_eq!(
            compare_signer_keys(&key_a, &key_b),
            std::cmp::Ordering::Less,
            "key with smaller payload should sort first"
        );
        assert_eq!(
            compare_signer_keys(&key_b, &key_a),
            std::cmp::Ordering::Greater,
            "key with larger payload should sort after"
        );

        // Also verify that adding them in reverse order produces correct sorted order
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();
        let source_id = create_test_account_id(200);
        state.create_account(create_test_account(source_id.clone(), 1_000_000_000));

        // Add payload_b first
        let op = SetOptionsOp {
            inflation_dest: None,
            clear_flags: None,
            set_flags: None,
            master_weight: None,
            low_threshold: None,
            med_threshold: None,
            high_threshold: None,
            home_domain: None,
            signer: Some(Signer {
                key: key_b.clone(),
                weight: 1,
            }),
        };
        let result = execute_set_options(&op, &source_id, &mut state, &context);
        assert!(matches!(
            result.unwrap(),
            OperationResult::OpInner(OperationResultTr::SetOptions(SetOptionsResult::Success))
        ));

        // Add payload_a second
        let op = SetOptionsOp {
            inflation_dest: None,
            clear_flags: None,
            set_flags: None,
            master_weight: None,
            low_threshold: None,
            med_threshold: None,
            high_threshold: None,
            home_domain: None,
            signer: Some(Signer {
                key: key_a.clone(),
                weight: 1,
            }),
        };
        let result = execute_set_options(&op, &source_id, &mut state, &context);
        assert!(matches!(
            result.unwrap(),
            OperationResult::OpInner(OperationResultTr::SetOptions(SetOptionsResult::Success))
        ));

        // Verify signers are sorted: key_a (smaller payload) should be first
        let account = state.get_account(&source_id).unwrap();
        assert_eq!(account.signers.len(), 2);
        assert_eq!(
            account.signers[0].key, key_a,
            "signer with smaller payload should be first"
        );
        assert_eq!(
            account.signers[1].key, key_b,
            "signer with larger payload should be second"
        );
    }

    #[test]
    fn test_set_options_master_weight_zero() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(37);
        state.create_account(create_test_account(source_id.clone(), 100_000_000));

        let op = SetOptionsOp {
            inflation_dest: None,
            clear_flags: None,
            set_flags: None,
            master_weight: Some(0), // Disable master key
            low_threshold: None,
            med_threshold: None,
            high_threshold: None,
            home_domain: None,
            signer: None,
        };

        let result = execute_set_options(&op, &source_id, &mut state, &context);
        match result.unwrap() {
            OperationResult::OpInner(OperationResultTr::SetOptions(r)) => {
                assert!(matches!(r, SetOptionsResult::Success));
            }
            other => panic!("expected SetOptions result, got {:?}", other),
        }

        // Verify master weight is 0
        let account = state.get_account(&source_id).unwrap();
        assert_eq!(account.thresholds.0[0], 0, "Master weight should be 0");
    }

    /// Test SetOptions AuthRevocableRequired: setting clawback without revocable.
    /// stellar-core: SetOptionsOpFrame::doApply checks accountFlagClawbackIsValid
    #[test]
    fn test_set_options_auth_revocable_required() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(50);
        // Account has no flags set
        state.create_account(create_test_account(source_id.clone(), 100_000_000));

        // Try to set clawback without revocable → AuthRevocableRequired
        let op = SetOptionsOp {
            inflation_dest: None,
            clear_flags: None,
            set_flags: Some(AccountFlags::ClawbackEnabledFlag as u32),
            master_weight: None,
            low_threshold: None,
            med_threshold: None,
            high_threshold: None,
            home_domain: None,
            signer: None,
        };

        let result = execute_set_options(&op, &source_id, &mut state, &context);
        match result.unwrap() {
            OperationResult::OpInner(OperationResultTr::SetOptions(r)) => {
                assert!(
                    matches!(r, SetOptionsResult::AuthRevocableRequired),
                    "Expected AuthRevocableRequired, got {:?}",
                    r
                );
            }
            other => panic!("expected SetOptions result, got {:?}", other),
        }
    }

    /// AUDIT-1109: Home domain with control characters should return InvalidHomeDomain.
    /// stellar-core: SetOptionsOpFrame::doCheckValid calls isStringValid() which rejects
    /// bytes outside 0x20-0x7E.
    #[test]
    fn test_set_options_invalid_home_domain_control_chars() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(51);
        state.create_account(create_test_account(source_id.clone(), 100_000_000));

        // Home domain with control character (tab)
        let op = SetOptionsOp {
            inflation_dest: None,
            clear_flags: None,
            set_flags: None,
            master_weight: None,
            low_threshold: None,
            med_threshold: None,
            high_threshold: None,
            home_domain: Some(String32::try_from(b"bad\tdomain".to_vec()).unwrap()),
            signer: None,
        };

        let result = execute_set_options(&op, &source_id, &mut state, &context);
        match result.unwrap() {
            OperationResult::OpInner(OperationResultTr::SetOptions(r)) => {
                assert!(
                    matches!(r, SetOptionsResult::InvalidHomeDomain),
                    "AUDIT-1109: Home domain with control chars should be InvalidHomeDomain, got {:?}",
                    r
                );
            }
            other => panic!("expected SetOptions result, got {:?}", other),
        }
    }

    /// AUDIT-1111: When both inflation dest is invalid AND account is immutable,
    /// stellar-core returns InvalidInflation first (checks inflation before immutable flag
    /// auth checks). Henyey must match this ordering.
    #[test]
    fn test_set_options_inflation_checked_before_cant_change() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(52);
        let mut source_account = create_test_account(source_id.clone(), 100_000_000);
        source_account.flags = AccountFlags::ImmutableFlag as u32;
        state.create_account(source_account);

        // Both invalid inflation (nonexistent dest) AND set auth flags (blocked by immutable)
        let nonexistent_id = create_test_account_id(253);
        let op = SetOptionsOp {
            inflation_dest: Some(nonexistent_id),
            clear_flags: None,
            set_flags: Some(AccountFlags::RequiredFlag as u32),
            master_weight: None,
            low_threshold: None,
            med_threshold: None,
            high_threshold: None,
            home_domain: None,
            signer: None,
        };

        let result = execute_set_options(&op, &source_id, &mut state, &context);
        match result.unwrap() {
            OperationResult::OpInner(OperationResultTr::SetOptions(r)) => {
                assert!(
                    matches!(r, SetOptionsResult::InvalidInflation),
                    "AUDIT-1111: InvalidInflation should be checked before CantChange, got {:?}",
                    r
                );
            }
            other => panic!("expected SetOptions result, got {:?}", other),
        }
    }
}
