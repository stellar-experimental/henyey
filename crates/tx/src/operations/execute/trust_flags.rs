//! Trust line flag operations execution.
//!
//! This module implements the execution logic for:
//! - AllowTrust (deprecated, but still supported)
//! - SetTrustLineFlags

use stellar_xdr::curr::{
    AccountFlags, AccountId, AllowTrustOp, AllowTrustResult, AllowTrustResultCode, Asset,
    ClaimPredicate, ClaimableBalanceEntry, ClaimableBalanceId, Claimant, ClaimantV0, Hash,
    HashIdPreimage, HashIdPreimageRevokeId, LedgerKey, LedgerKeyClaimableBalance,
    LedgerKeyTrustLine, LiquidityPoolEntryBody, OfferEntry, OperationResult, OperationResultTr,
    PoolId, SequenceNumber, SetTrustLineFlagsOp, SetTrustLineFlagsResult,
    SetTrustLineFlagsResultCode, TrustLineAsset,
};

use super::{
    add_pool_reserve, add_pool_shares, ensure_account_liabilities, ensure_trustline_liabilities,
    is_authorized_to_maintain_liabilities, issuer_for_asset, TxIdentity, AUTHORIZED_FLAG,
    AUTHORIZED_TO_MAINTAIN_LIABILITIES_FLAG, TRUSTLINE_CLAWBACK_ENABLED_FLAG,
};
use crate::state::LedgerStateManager;
use crate::validation::LedgerContext;
use crate::Result;
use henyey_common::protocol::{protocol_version_is_before, ProtocolVersion};

const TRUSTLINE_AUTH_FLAGS: u32 = AUTHORIZED_FLAG | AUTHORIZED_TO_MAINTAIN_LIABILITIES_FLAG;
/// Execute an AllowTrust operation (deprecated).
///
/// This operation sets the authorized flag on a trustline. It has been
/// deprecated in favor of SetTrustLineFlags but is still supported.
pub(crate) fn execute_allow_trust(
    op: &AllowTrustOp,
    source: &AccountId,
    tx_id: &TxIdentity<'_>,
    state: &mut LedgerStateManager,
    _context: &LedgerContext,
) -> Result<OperationResult> {
    // Validate authorize value (matches stellar-core doCheckValid ordering).
    // Valid values: 0, AUTHORIZED_FLAG (1), AUTHORIZED_TO_MAINTAIN_LIABILITIES_FLAG (2).
    if op.authorize > AUTHORIZED_TO_MAINTAIN_LIABILITIES_FLAG {
        return Ok(make_allow_trust_result(AllowTrustResultCode::Malformed));
    }

    // Validate asset code content (defense-in-depth; also checked in validation layer).
    match &op.asset {
        stellar_xdr::curr::AssetCode::CreditAlphanum4(code) => {
            if !henyey_common::asset::is_asset_code4_valid(code) {
                return Ok(make_allow_trust_result(AllowTrustResultCode::Malformed));
            }
        }
        stellar_xdr::curr::AssetCode::CreditAlphanum12(code) => {
            if !henyey_common::asset::is_asset_code12_valid(code) {
                return Ok(make_allow_trust_result(AllowTrustResultCode::Malformed));
            }
        }
    }

    // Check source account exists (the issuer)
    // NOTE: stellar-core loads the source account in a nested LedgerTxn (ltxSource)
    // that gets rolled back, so the source account access is NOT recorded in the
    // transaction changes. We use get_account() (read-only) to match this behavior.
    let issuer = match state.get_account(source) {
        Some(a) => a.clone(),
        None => {
            return Ok(make_allow_trust_result(AllowTrustResultCode::Malformed));
        }
    };

    // Check if trustor == source (self not allowed)
    if &op.trustor == source {
        return Ok(make_allow_trust_result(
            AllowTrustResultCode::SelfNotAllowed,
        ));
    }

    // Check AUTH_REVOCABLE for revocation (first check - before loading trustline)
    // Cannot fully deauthorize (authorize == 0) without AUTH_REVOCABLE
    let auth_revocable = issuer.flags & (AccountFlags::RevocableFlag as u32) != 0;
    if !auth_revocable && op.authorize == 0 {
        return Ok(make_allow_trust_result(AllowTrustResultCode::CantRevoke));
    }

    // Convert the asset code to a full Asset
    let asset = asset_from_code(&op.asset, source);

    // Get the trustline
    let trustline = match state.get_trustline(&op.trustor, &asset) {
        Some(tl) => tl.clone(),
        None => {
            return Ok(make_allow_trust_result(AllowTrustResultCode::NoTrustLine));
        }
    };

    // Calculate new flags: clear auth flags, then set based on authorize value
    let mut new_flags = trustline.flags;
    new_flags &= !TRUSTLINE_AUTH_FLAGS;
    new_flags |= op.authorize;

    // Second CantRevoke check: Cannot downgrade from AUTHORIZED to
    // AUTHORIZED_TO_MAINTAIN_LIABILITIES without AUTH_REVOCABLE
    let was_authorized = trustline.flags & AUTHORIZED_FLAG != 0;
    let setting_maintain_liabilities = new_flags & AUTHORIZED_TO_MAINTAIN_LIABILITIES_FLAG != 0;
    if !auth_revocable && was_authorized && setting_maintain_liabilities {
        return Ok(make_allow_trust_result(AllowTrustResultCode::CantRevoke));
    }

    // Handle deauthorization: remove offers and redeem pool shares if revoking liabilities
    match handle_deauthorization(
        trustline.flags,
        new_flags,
        &op.trustor,
        &asset,
        tx_id,
        state,
        _context,
    )? {
        Some(RemoveResult::LowReserve) => {
            return Ok(make_allow_trust_result(AllowTrustResultCode::LowReserve));
        }
        Some(RemoveResult::TooManySponsoring) => {
            return Ok(OperationResult::OpTooManySponsoring);
        }
        _ => {}
    }

    // Update the trustline
    if let Some(tl) = state.get_trustline_mut(&op.trustor, &asset) {
        tl.flags = new_flags;
    }

    Ok(make_allow_trust_result(AllowTrustResultCode::Success))
}

/// Valid mask for set_flags: only AUTHORIZED and AUTHORIZED_TO_MAINTAIN_LIABILITIES are allowed.
const VALID_SET_FLAGS_MASK: u32 = AUTHORIZED_FLAG | AUTHORIZED_TO_MAINTAIN_LIABILITIES_FLAG;

/// Valid mask for clear_flags: AUTHORIZED, AUTHORIZED_TO_MAINTAIN_LIABILITIES, and
/// TRUSTLINE_CLAWBACK_ENABLED are allowed.
const VALID_CLEAR_FLAGS_MASK: u32 =
    AUTHORIZED_FLAG | AUTHORIZED_TO_MAINTAIN_LIABILITIES_FLAG | TRUSTLINE_CLAWBACK_ENABLED_FLAG;

/// Execute a SetTrustLineFlags operation.
///
/// This operation sets or clears specific flags on a trustline.
pub(crate) fn execute_set_trust_line_flags(
    op: &SetTrustLineFlagsOp,
    source: &AccountId,
    tx_id: &TxIdentity<'_>,
    state: &mut LedgerStateManager,
    _context: &LedgerContext,
) -> Result<OperationResult> {
    // --- doCheckValid checks (before any state access) ---

    // Self-trust modification not allowed
    if &op.trustor == source {
        return Ok(make_set_flags_result(
            SetTrustLineFlagsResultCode::Malformed,
        ));
    }

    // set_flags must only contain valid flag bits (AUTHORIZED | AUTHORIZED_TO_MAINTAIN_LIABILITIES)
    // In particular, TRUSTLINE_CLAWBACK_ENABLED_FLAG cannot be set via this operation.
    if (op.set_flags & !VALID_SET_FLAGS_MASK) != 0 {
        return Ok(make_set_flags_result(
            SetTrustLineFlagsResultCode::Malformed,
        ));
    }

    // clear_flags must only contain valid flag bits (AUTHORIZED | AUTHORIZED_TO_MAINTAIN_LIABILITIES | CLAWBACK_ENABLED)
    if (op.clear_flags & !VALID_CLEAR_FLAGS_MASK) != 0 {
        return Ok(make_set_flags_result(
            SetTrustLineFlagsResultCode::Malformed,
        ));
    }

    // Cannot set both AUTHORIZED and AUTHORIZED_TO_MAINTAIN_LIABILITIES
    if (op.set_flags & AUTHORIZED_FLAG != 0)
        && (op.set_flags & AUTHORIZED_TO_MAINTAIN_LIABILITIES_FLAG != 0)
    {
        return Ok(make_set_flags_result(
            SetTrustLineFlagsResultCode::Malformed,
        ));
    }

    // Cannot clear and set the same flag
    if (op.set_flags & op.clear_flags) != 0 {
        return Ok(make_set_flags_result(
            SetTrustLineFlagsResultCode::Malformed,
        ));
    }

    // Check source account exists (the issuer)
    // NOTE: stellar-core loads the source account in a nested LedgerTxn (ltxSource)
    // that gets rolled back, so the source account access is NOT recorded in the
    // transaction changes. We use get_account() (read-only) to match this behavior.
    let source_account = match state.get_account(source) {
        Some(a) => a.clone(),
        None => {
            return Ok(make_set_flags_result(
                SetTrustLineFlagsResultCode::Malformed,
            ));
        }
    };

    // The source must be the issuer of the asset
    let issuer = match &op.asset {
        Asset::Native => {
            return Ok(make_set_flags_result(
                SetTrustLineFlagsResultCode::Malformed,
            ));
        }
        Asset::CreditAlphanum4(a) => {
            if !henyey_common::asset::is_asset_code4_valid(&a.asset_code) {
                return Ok(make_set_flags_result(
                    SetTrustLineFlagsResultCode::Malformed,
                ));
            }
            &a.issuer
        }
        Asset::CreditAlphanum12(a) => {
            if !henyey_common::asset::is_asset_code12_valid(&a.asset_code) {
                return Ok(make_set_flags_result(
                    SetTrustLineFlagsResultCode::Malformed,
                ));
            }
            &a.issuer
        }
    };

    if issuer != source {
        return Ok(make_set_flags_result(
            SetTrustLineFlagsResultCode::Malformed,
        ));
    }

    // Check AUTH_REVOCABLE_FLAG for revocation operations (before trustline load).
    // stellar-core checks isAuthRevocationValid() before loading the trustline,
    // so CantRevoke takes priority over NoTrustLine when both conditions are true.
    //
    // If AUTH_REVOCABLE is not set on the issuer account, the following transitions
    // are not allowed:
    // 1. AUTHORIZED_FLAG -> AUTHORIZED_TO_MAINTAIN_LIABILITIES_FLAG
    // 2. AUTHORIZED_FLAG -> 0
    // 3. AUTHORIZED_TO_MAINTAIN_LIABILITIES_FLAG -> 0
    // These are all cases where we're clearing auth flags without setting AUTHORIZED.
    let auth_revocable = source_account.flags & (AccountFlags::RevocableFlag as u32) != 0;
    if !auth_revocable {
        let clearing_any_auth = (op.clear_flags & TRUSTLINE_AUTH_FLAGS) != 0;
        let setting_authorized = (op.set_flags & AUTHORIZED_FLAG) != 0;
        if clearing_any_auth && !setting_authorized {
            return Ok(make_set_flags_result(
                SetTrustLineFlagsResultCode::CantRevoke,
            ));
        }
    }

    // Get the trustline
    let trustline = match state.get_trustline(&op.trustor, &op.asset) {
        Some(tl) => tl.clone(),
        None => {
            return Ok(make_set_flags_result(
                SetTrustLineFlagsResultCode::NoTrustLine,
            ));
        }
    };

    // Calculate new flags: apply clear first, then set
    let mut new_flags = trustline.flags;
    new_flags &= !op.clear_flags;
    new_flags |= op.set_flags;

    // Check if the resulting flags are valid - cannot have both auth flags set
    if !is_trust_line_flag_auth_valid(new_flags) {
        return Ok(make_set_flags_result(
            SetTrustLineFlagsResultCode::InvalidState,
        ));
    }

    // Handle deauthorization: remove offers and redeem pool shares if revoking liabilities
    match handle_deauthorization(
        trustline.flags,
        new_flags,
        &op.trustor,
        &op.asset,
        tx_id,
        state,
        _context,
    )? {
        Some(RemoveResult::LowReserve) => {
            return Ok(make_set_flags_result(
                SetTrustLineFlagsResultCode::LowReserve,
            ));
        }
        Some(RemoveResult::TooManySponsoring) => {
            return Ok(OperationResult::OpTooManySponsoring);
        }
        _ => {}
    }

    // Update the trustline
    if let Some(tl) = state.get_trustline_mut(&op.trustor, &op.asset) {
        tl.flags = new_flags;
    }

    Ok(make_set_flags_result(SetTrustLineFlagsResultCode::Success))
}

/// Convert an AllowTrust asset code + issuer into a full Asset.
fn asset_from_code(code: &stellar_xdr::curr::AssetCode, issuer: &AccountId) -> Asset {
    match code {
        stellar_xdr::curr::AssetCode::CreditAlphanum4(c) => {
            Asset::CreditAlphanum4(stellar_xdr::curr::AlphaNum4 {
                asset_code: c.clone(),
                issuer: issuer.clone(),
            })
        }
        stellar_xdr::curr::AssetCode::CreditAlphanum12(c) => {
            Asset::CreditAlphanum12(stellar_xdr::curr::AlphaNum12 {
                asset_code: c.clone(),
                issuer: issuer.clone(),
            })
        }
    }
}

/// Handle deauthorization cleanup: remove offers and redeem pool share trustlines
/// when revoking liabilities authorization. Returns `Some(RemoveResult::LowReserve)`
/// or `Some(RemoveResult::TooManySponsoring)` if pool share redemption fails,
/// `None` if no deauthorization occurred or it succeeded.
fn handle_deauthorization(
    old_flags: u32,
    new_flags: u32,
    trustor: &AccountId,
    asset: &Asset,
    tx_id: &TxIdentity<'_>,
    state: &mut LedgerStateManager,
    context: &LedgerContext,
) -> Result<Option<RemoveResult>> {
    let was_authorized_to_maintain = is_authorized_to_maintain_liabilities(old_flags);
    let will_be_authorized_to_maintain = is_authorized_to_maintain_liabilities(new_flags);

    if was_authorized_to_maintain && !will_be_authorized_to_maintain {
        remove_offers_with_cleanup(state, trustor, asset)?;
        let result = redeem_pool_share_trustlines(state, context, trustor, asset, tx_id)?;
        if result != RemoveResult::Success {
            return Ok(Some(result));
        }
    }
    Ok(None)
}

/// Create an AllowTrust result.
fn make_allow_trust_result(code: AllowTrustResultCode) -> OperationResult {
    let result = match code {
        AllowTrustResultCode::Success => AllowTrustResult::Success,
        AllowTrustResultCode::Malformed => AllowTrustResult::Malformed,
        AllowTrustResultCode::NoTrustLine => AllowTrustResult::NoTrustLine,
        AllowTrustResultCode::TrustNotRequired => AllowTrustResult::TrustNotRequired,
        AllowTrustResultCode::CantRevoke => AllowTrustResult::CantRevoke,
        AllowTrustResultCode::SelfNotAllowed => AllowTrustResult::SelfNotAllowed,
        AllowTrustResultCode::LowReserve => AllowTrustResult::LowReserve,
    };

    OperationResult::OpInner(OperationResultTr::AllowTrust(result))
}

/// Create a SetTrustLineFlags result.
fn make_set_flags_result(code: SetTrustLineFlagsResultCode) -> OperationResult {
    let result = match code {
        SetTrustLineFlagsResultCode::Success => SetTrustLineFlagsResult::Success,
        SetTrustLineFlagsResultCode::Malformed => SetTrustLineFlagsResult::Malformed,
        SetTrustLineFlagsResultCode::NoTrustLine => SetTrustLineFlagsResult::NoTrustLine,
        SetTrustLineFlagsResultCode::CantRevoke => SetTrustLineFlagsResult::CantRevoke,
        SetTrustLineFlagsResultCode::InvalidState => SetTrustLineFlagsResult::InvalidState,
        SetTrustLineFlagsResultCode::LowReserve => SetTrustLineFlagsResult::LowReserve,
    };

    OperationResult::OpInner(OperationResultTr::SetTrustLineFlags(result))
}

/// Check if the trust line auth flags are valid.
/// Both AUTHORIZED_FLAG and AUTHORIZED_TO_MAINTAIN_LIABILITIES_FLAG cannot be set at the same time.
fn is_trust_line_flag_auth_valid(flags: u32) -> bool {
    // Multiple auth flags can't be set simultaneously
    (flags & TRUSTLINE_AUTH_FLAGS) != TRUSTLINE_AUTH_FLAGS
}

/// Remove offers by account and asset with full cleanup.
///
/// Processes each offer individually matching stellar-core's
/// `removeOffersByAccountAndAsset` (TransactionUtils.cpp:1390-1417):
/// release liabilities → delete with sponsorship cleanup.
///
/// Offers are sorted by offer_id for deterministic processing since the
/// underlying index uses `HashSet<i64>`.
fn remove_offers_with_cleanup(
    state: &mut LedgerStateManager,
    account_id: &AccountId,
    asset: &Asset,
) -> Result<()> {
    let mut offers = state.get_offers_by_account_and_asset(account_id, asset);
    offers.sort_by_key(|o| o.offer_id);

    for offer in offers {
        release_offer_liabilities(state, &offer);
        super::offer_utils::delete_offer_with_sponsorship(&offer.seller_id, offer.offer_id, state)?;
    }
    Ok(())
}

/// Calculate and release liabilities for a deleted offer.
///
/// Uses `saturating_sub` instead of `apply_liabilities_delta` because this is
/// called during bulk deauthorization where we need best-effort cleanup without
/// propagating errors from individual offer liability releases.
fn release_offer_liabilities(state: &mut LedgerStateManager, offer: &OfferEntry) {
    // Calculate liabilities: selling_liab is the offer amount,
    // buying_liab is what we'd receive at the offer price
    let (selling_liab, buying_liab) =
        match super::offer_utils::offer_liabilities_sell(offer.amount, &offer.price) {
            Ok((s, b)) => (s, b),
            Err(_) => return, // Shouldn't happen for valid offers
        };

    // Release selling liability
    if matches!(offer.selling, Asset::Native) {
        if let Some(account) = state.get_account_mut(&offer.seller_id) {
            let liab = ensure_account_liabilities(account);
            liab.selling = liab.selling.saturating_sub(selling_liab);
        }
    } else if issuer_for_asset(&offer.selling) != Some(&offer.seller_id) {
        if let Some(trustline) = state.get_trustline_mut(&offer.seller_id, &offer.selling) {
            let liab = ensure_trustline_liabilities(trustline);
            liab.selling = liab.selling.saturating_sub(selling_liab);
        }
    }

    // Release buying liability
    if matches!(offer.buying, Asset::Native) {
        if let Some(account) = state.get_account_mut(&offer.seller_id) {
            let liab = ensure_account_liabilities(account);
            liab.buying = liab.buying.saturating_sub(buying_liab);
        }
    } else if issuer_for_asset(&offer.buying) != Some(&offer.seller_id) {
        if let Some(trustline) = state.get_trustline_mut(&offer.seller_id, &offer.buying) {
            let liab = ensure_trustline_liabilities(trustline);
            liab.buying = liab.buying.saturating_sub(buying_liab);
        }
    }
}

/// Result of removing offers and pool share trustlines.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RemoveResult {
    Success,
    LowReserve,
    TooManySponsoring,
}

/// Generate a claimable balance ID for a pool share revocation.
///
/// Uses `ENVELOPE_TYPE_POOL_REVOKE_OP_ID` (different from regular claimable balance IDs
/// which use `ENVELOPE_TYPE_OP_ID`).
fn get_revoke_id(
    tx_id: &TxIdentity<'_>,
    pool_id: &PoolId,
    asset: &Asset,
) -> Result<ClaimableBalanceId> {
    let preimage = HashIdPreimage::PoolRevokeOpId(HashIdPreimageRevokeId {
        source_account: tx_id.source_id.clone(),
        seq_num: SequenceNumber(tx_id.seq),
        op_num: tx_id.op_index,
        liquidity_pool_id: pool_id.clone(),
        asset: asset.clone(),
    });
    let hash = henyey_common::Hash256::hash_xdr(&preimage);
    Ok(ClaimableBalanceId::ClaimableBalanceIdTypeV0(Hash(hash.0)))
}

/// Check if an account is the issuer of an asset.
fn is_issuer(account_id: &AccountId, asset: &Asset) -> bool {
    match asset {
        Asset::Native => false,
        Asset::CreditAlphanum4(a) => &a.issuer == account_id,
        Asset::CreditAlphanum12(a) => &a.issuer == account_id,
    }
}

/// Calculate pool withdrawal amount: floor(amount * reserve / total_shares).
fn get_pool_withdrawal_amount(amount: i64, total_shares: i64, reserve: i64) -> i64 {
    if total_shares == 0 {
        return 0;
    }
    let numerator = (amount as i128) * (reserve as i128);
    let result = numerator / (total_shares as i128);
    if result > i64::MAX as i128 {
        return 0;
    }
    result as i64
}

/// Redeem pool share trustlines for an account when deauthorizing an asset.
///
/// This implements the second half of C++ `removeOffersAndPoolShareTrustLines`
/// (the pool share trustline part). When an issuer deauthorizes a trustline,
/// any pool share trustlines that reference the deauthorized asset must be
/// redeemed: the trustline is deleted and the account's share of the pool
/// is converted to claimable balances.
///
/// Protocol < 18: no-op (pool shares didn't exist).
fn redeem_pool_share_trustlines(
    state: &mut LedgerStateManager,
    context: &LedgerContext,
    account_id: &AccountId,
    asset: &Asset,
    tx_id: &super::TxIdentity<'_>,
) -> Result<RemoveResult> {
    if protocol_version_is_before(context.protocol_version, ProtocolVersion::V18) {
        return Ok(RemoveResult::Success);
    }

    // Find all pool share trustlines for this account that reference the
    // deauthorized asset. We need to collect them first since we'll be
    // mutating state.
    let pool_share_tl_keys = find_pool_share_trustlines_for_asset(state, account_id, asset);
    if pool_share_tl_keys.is_empty() {
        return Ok(RemoveResult::Success);
    }

    for (pool_id, tl_asset) in pool_share_tl_keys {
        // Load pool share trustline data
        let Some(pool_tl) = state.get_trustline_by_trustline_asset(account_id, &tl_asset) else {
            continue;
        };
        let balance = pool_tl.balance;

        // Get the backer (sponsor or the account itself)
        let tl_ledger_key = LedgerKey::Trustline(LedgerKeyTrustLine {
            account_id: account_id.clone(),
            asset: tl_asset.clone(),
        });
        let cb_sponsoring_acc_id = state
            .entry_sponsor(&tl_ledger_key)
            .unwrap_or_else(|| account_id.clone());

        // Delete the pool share trustline: release reserves and remove.
        // Pool share trustlines have multiplier 2.
        // Atomic validate-then-mutate: clears sponsorship metadata,
        // decrements num_sponsoring/num_sponsored (if sponsored), and
        // decrements num_sub_entries by `multiplier` on the source account.
        // Mirrors stellar-core's `removeEntryWithPossibleSponsorship`
        // (SponsorshipUtils.cpp:808-847).
        let multiplier: i64 = 2;
        state.remove_sponsored_subentry(&tl_ledger_key, account_id, multiplier)?;

        // NOTE: Do NOT call flush_all_accounts() here. Unlike change_trust.rs
        // (which deletes a single trustline), this loop processes N pool-share
        // trustlines. Flushing per-iteration would emit N Account UPDATED
        // entries for the same trustor key, whereas stellar-core's
        // removeOffersAndPoolShareTrustLines accumulates all changes in one
        // LedgerTxn and commits once (TransactionUtils.cpp:1774), producing at
        // most one change per unique LedgerKey. Account modifications are
        // deferred and coalesced by flush_modified_entries() after the
        // operation returns (apply.rs:439).
        state.delete_trustline_by_trustline_asset(account_id, &tl_asset);

        // Load pool data for withdrawal calculation
        let Some(pool) = state.get_liquidity_pool(&pool_id) else {
            continue;
        };
        let LiquidityPoolEntryBody::LiquidityPoolConstantProduct(cp) = &pool.body;
        let total_pool_shares = cp.total_pool_shares;
        let reserve_a = cp.reserve_a;
        let reserve_b = cp.reserve_b;
        let asset_a = cp.params.asset_a.clone();
        let asset_b = cp.params.asset_b.clone();

        if balance != 0 {
            let amount_a = get_pool_withdrawal_amount(balance, total_pool_shares, reserve_a);

            let res = redeem_into_claimable_balance(
                state,
                &asset_a,
                amount_a,
                account_id,
                &cb_sponsoring_acc_id,
                tx_id,
                &pool_id,
            )?;
            if res != RemoveResult::Success {
                return Ok(res);
            }

            let amount_b = get_pool_withdrawal_amount(balance, total_pool_shares, reserve_b);

            let res = redeem_into_claimable_balance(
                state,
                &asset_b,
                amount_b,
                account_id,
                &cb_sponsoring_acc_id,
                tx_id,
                &pool_id,
            )?;
            if res != RemoveResult::Success {
                return Ok(res);
            }

            // Update pool reserves and shares
            if let Some(pool) = state.get_liquidity_pool_mut(&pool_id) {
                let LiquidityPoolEntryBody::LiquidityPoolConstantProduct(cp) = &mut pool.body;
                add_pool_shares(&mut cp.total_pool_shares, -balance)?;
                add_pool_reserve(&mut cp.reserve_a, -amount_a)?;
                add_pool_reserve(&mut cp.reserve_b, -amount_b)?;
            }
        }

        // Decrement liquidity pool use count on asset trustlines
        decrement_liquidity_pool_use_count(state, &asset_a, account_id)?;
        decrement_liquidity_pool_use_count(state, &asset_b, account_id)?;

        // Decrement pool shares trust line count (may delete pool if count reaches 0)
        decrement_pool_shares_trust_line_count(state, &pool_id)?;
    }

    Ok(RemoveResult::Success)
}

/// Create a claimable balance for a pool share redemption.
fn redeem_into_claimable_balance(
    state: &mut LedgerStateManager,
    asset: &Asset,
    amount: i64,
    account_id: &AccountId,
    cb_sponsoring_acc_id: &AccountId,
    tx_id: &super::TxIdentity<'_>,
    pool_id: &PoolId,
) -> Result<RemoveResult> {
    // If amount is 0, nothing to do
    if amount == 0 {
        return Ok(RemoveResult::Success);
    }

    // If the account is the issuer, no claimable balance needed
    if is_issuer(account_id, asset) {
        return Ok(RemoveResult::Success);
    }

    // Create the claimable balance entry
    let balance_id = get_revoke_id(tx_id, pool_id, asset)?;

    let claimant = Claimant::ClaimantTypeV0(ClaimantV0 {
        destination: account_id.clone(),
        predicate: ClaimPredicate::Unconditional,
    });

    let mut cb_entry = ClaimableBalanceEntry {
        balance_id: balance_id.clone(),
        claimants: vec![claimant].try_into().unwrap(),
        asset: asset.clone(),
        amount,
        ext: stellar_xdr::curr::ClaimableBalanceEntryExt::V0,
    };

    // If asset is not native, check clawback flag
    if !matches!(asset, Asset::Native) {
        if let Some(asset_tl) = state.get_trustline(account_id, asset) {
            if asset_tl.flags & TRUSTLINE_CLAWBACK_ENABLED_FLAG != 0 {
                cb_entry.ext = stellar_xdr::curr::ClaimableBalanceEntryExt::V1(
                    stellar_xdr::curr::ClaimableBalanceEntryExtensionV1 {
                        ext: stellar_xdr::curr::ClaimableBalanceEntryExtensionV1Ext::V0,
                        flags: stellar_xdr::curr::ClaimableBalanceFlags::ClaimableBalanceClawbackEnabledFlag as u32,
                    },
                );
            }
        }
    }

    // Handle sponsorship for the claimable balance.
    let cb_ledger_key = LedgerKey::ClaimableBalance(LedgerKeyClaimableBalance { balance_id });

    // Check if the sponsoring account is itself in a sponsorship sandwich
    if let Some(sandwich_sponsor) = state.active_sponsor_for(cb_sponsoring_acc_id) {
        let sponsor_account = state.get_account(&sandwich_sponsor);
        if sponsor_account.is_none() {
            return Ok(RemoveResult::LowReserve);
        }

        if let Err(e) = state.apply_entry_sponsorship_with_sponsor(
            cb_ledger_key,
            &sandwich_sponsor,
            None, // claimable balances are not subentries
            1,
        ) {
            // Match the error directly: TooManySponsoring covers both
            // individual cap and combined (numSponsoring + numSubEntries) cap.
            return Ok(if matches!(e, crate::error::TxError::TooManySponsoring) {
                RemoveResult::TooManySponsoring
            } else {
                RemoveResult::LowReserve
            });
        }
    } else {
        // Not in a sandwich — directly establish sponsorship from cb_sponsoring_acc_id.
        let (num_sponsoring, _) = state
            .sponsorship_counts_for_account(cb_sponsoring_acc_id)
            .unwrap_or((0, 0));
        if num_sponsoring > u32::MAX as i64 - 1 {
            return Ok(RemoveResult::TooManySponsoring);
        }

        state.set_entry_sponsor(cb_ledger_key, cb_sponsoring_acc_id.clone());
        state.update_num_sponsoring(cb_sponsoring_acc_id, 1)?;
    }

    state.create_claimable_balance(cb_entry);
    Ok(RemoveResult::Success)
}

/// Find pool share trustlines for an account that reference a given asset.
///
/// Calls `ensure_pool_share_trustlines_loaded` first to load any pool share
/// trustlines from the bucket list secondary index that aren't yet in memory.
/// This prevents the VE-02 bug where pool share TLs only on disk were silently
/// missed.
fn find_pool_share_trustlines_for_asset(
    state: &mut LedgerStateManager,
    account_id: &AccountId,
    asset: &Asset,
) -> Vec<(PoolId, TrustLineAsset)> {
    // Load all pool share trustlines and their liquidity pools from the
    // authoritative secondary index so the in-memory state is complete.
    if let Err(e) = state.ensure_pool_share_trustlines_loaded(account_id) {
        tracing::warn!(
            error = %e,
            "find_pool_share_trustlines_for_asset: failed to load from index"
        );
    }

    let mut result = Vec::new();

    for (key, _tl) in state.trustlines_iter() {
        let (acct_id, asset_key) = key;
        if acct_id != account_id {
            continue;
        }
        let pool_id = match asset_key {
            TrustLineAsset::PoolShare(pool_id) => pool_id,
            _ => continue,
        };

        let Some(pool) = state.get_liquidity_pool(pool_id) else {
            continue;
        };

        let LiquidityPoolEntryBody::LiquidityPoolConstantProduct(cp) = &pool.body;
        if &cp.params.asset_a == asset || &cp.params.asset_b == asset {
            result.push((pool_id.clone(), TrustLineAsset::PoolShare(pool_id.clone())));
        }
    }

    // Sort by canonical LedgerKey order for deterministic revocation.
    //
    // The iteration order of trustlines_iter() is non-deterministic (HashMap).
    // stellar-core sorts the collected pool-share LedgerKeys before processing
    // to ensure deterministic claimable balance creation and sponsorship
    // reserve interactions across all validators.
    //
    // Since all entries share the same account_id and are PoolShare type,
    // the effective canonical sort order is by pool ID bytes.
    result.sort_by_key(|a| a.0 .0 .0);

    result
}

/// Decrement the liquidity pool use count on an asset trustline.
///
/// Matches stellar-core `decrementLiquidityPoolUseCount`
/// (TransactionUtils.cpp:1796-1816) which throws on missing trustline,
/// missing V2 extension, or negative count after decrement.
fn decrement_liquidity_pool_use_count(
    state: &mut LedgerStateManager,
    asset: &Asset,
    account_id: &AccountId,
) -> Result<()> {
    if matches!(asset, Asset::Native) {
        return Ok(());
    }
    if is_issuer(account_id, asset) {
        return Ok(());
    }
    let tl = state
        .get_trustline_mut(account_id, asset)
        .ok_or_else(|| crate::TxError::Internal("asset trustline is missing".into()))?;
    let v2 = super::get_trustline_ext_v2_mut(tl)?;
    let new_count = v2
        .liquidity_pool_use_count
        .checked_sub(1)
        .ok_or_else(|| crate::TxError::Internal("liquidityPoolUseCount overflow".into()))?;
    if new_count < 0 {
        return Err(crate::TxError::Internal(
            "liquidityPoolUseCount is negative".into(),
        ));
    }
    v2.liquidity_pool_use_count = new_count;
    Ok(())
}

/// Decrement the pool shares trust line count and delete the pool if it reaches 0.
///
/// Matches stellar-core `decrementPoolSharesTrustLineCount`
/// (TransactionUtils.cpp:1779-1795) which throws on negative count.
fn decrement_pool_shares_trust_line_count(
    state: &mut LedgerStateManager,
    pool_id: &PoolId,
) -> Result<()> {
    let pool = state
        .get_liquidity_pool_mut(pool_id)
        .ok_or_else(|| crate::TxError::Internal("liquidity pool is missing".into()))?;
    let LiquidityPoolEntryBody::LiquidityPoolConstantProduct(cp) = &mut pool.body;
    let new_count = cp
        .pool_shares_trust_line_count
        .checked_sub(1)
        .ok_or_else(|| crate::TxError::Internal("poolSharesTrustLineCount overflow".into()))?;
    if new_count < 0 {
        return Err(crate::TxError::Internal(
            "poolSharesTrustLineCount is negative".into(),
        ));
    }
    cp.pool_shares_trust_line_count = new_count;
    if new_count == 0 {
        state.delete_liquidity_pool(pool_id);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::create_test_account_id;
    use henyey_common::LIQUIDITY_POOL_FEE_V18;
    use stellar_xdr::curr::*;

    fn create_test_account(account_id: AccountId, balance: i64, flags: u32) -> AccountEntry {
        AccountEntry {
            account_id,
            balance,
            seq_num: SequenceNumber(1),
            num_sub_entries: 0,
            inflation_dest: None,
            flags,
            home_domain: String32::default(),
            thresholds: Thresholds([1, 0, 0, 0]),
            signers: vec![].try_into().unwrap(),
            ext: AccountEntryExt::V0,
        }
    }

    fn create_test_trustline_with_liabilities(
        account_id: AccountId,
        asset: TrustLineAsset,
        balance: i64,
        limit: i64,
        flags: u32,
        buying: i64,
        selling: i64,
    ) -> TrustLineEntry {
        TrustLineEntry {
            account_id,
            asset,
            balance,
            limit,
            flags,
            ext: TrustLineEntryExt::V1(TrustLineEntryV1 {
                liabilities: Liabilities { buying, selling },
                ext: TrustLineEntryV1Ext::V0,
            }),
        }
    }

    fn create_test_context() -> LedgerContext {
        LedgerContext::testnet(1, 1000)
    }

    fn create_asset(issuer: &AccountId) -> Asset {
        Asset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4([b'U', b'S', b'D', b'C']),
            issuer: issuer.clone(),
        })
    }

    fn create_test_trustline(
        account_id: AccountId,
        asset: TrustLineAsset,
        balance: i64,
        limit: i64,
        flags: u32,
    ) -> TrustLineEntry {
        TrustLineEntry {
            account_id,
            asset,
            balance,
            limit,
            flags,
            ext: TrustLineEntryExt::V0,
        }
    }

    #[test]
    fn test_allow_trust_no_auth_required() {
        // In protocol 16+ (CAP-0035), the AUTH_REQUIRED check was removed from AllowTrust.
        // AllowTrust should succeed even when issuer doesn't have AUTH_REQUIRED flag.
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let issuer_id = create_test_account_id(0);
        let trustor_id = create_test_account_id(1);

        // Issuer without AUTH_REQUIRED flag
        state.create_account(create_test_account(issuer_id.clone(), 100_000_000, 0));
        state.create_account(create_test_account(trustor_id.clone(), 10_000_000, 0));

        // Create a trustline for the trustor (auto-authorized since issuer has no AUTH_REQUIRED)
        state.create_trustline(create_test_trustline_with_liabilities(
            trustor_id.clone(),
            TrustLineAsset::CreditAlphanum4(AlphaNum4 {
                asset_code: AssetCode4([b'U', b'S', b'D', b'C']),
                issuer: issuer_id.clone(),
            }),
            0,
            1_000_000,
            AUTHORIZED_FLAG,
            0,
            0,
        ));

        let op = AllowTrustOp {
            trustor: trustor_id,
            asset: AssetCode::CreditAlphanum4(AssetCode4([b'U', b'S', b'D', b'C'])),
            authorize: 1,
        };

        let result = execute_allow_trust(
            &op,
            &issuer_id,
            &TxIdentity {
                source_id: &issuer_id,
                seq: 1,
                op_index: 0,
            },
            &mut state,
            &context,
        );
        assert!(result.is_ok());

        // In protocol 16+, AllowTrust succeeds even without AUTH_REQUIRED
        match result.unwrap() {
            OperationResult::OpInner(OperationResultTr::AllowTrust(r)) => {
                assert!(matches!(r, AllowTrustResult::Success));
            }
            _ => panic!("Unexpected result type"),
        }
    }

    #[test]
    fn test_allow_trust_cant_revoke_with_liabilities() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let issuer_id = create_test_account_id(0);
        let trustor_id = create_test_account_id(1);
        state.create_account(create_test_account(
            issuer_id.clone(),
            100_000_000,
            AccountFlags::RequiredFlag as u32,
        ));
        state.create_account(create_test_account(trustor_id.clone(), 100_000_000, 0));

        let _asset = Asset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4([b'U', b'S', b'D', b'C']),
            issuer: issuer_id.clone(),
        });

        state.create_trustline(create_test_trustline_with_liabilities(
            trustor_id.clone(),
            TrustLineAsset::CreditAlphanum4(AlphaNum4 {
                asset_code: AssetCode4([b'U', b'S', b'D', b'C']),
                issuer: issuer_id.clone(),
            }),
            100,
            1_000_000,
            AUTHORIZED_FLAG,
            0,
            1,
        ));

        let op = AllowTrustOp {
            trustor: trustor_id.clone(),
            asset: stellar_xdr::curr::AssetCode::CreditAlphanum4(AssetCode4([
                b'U', b'S', b'D', b'C',
            ])),
            authorize: 0,
        };

        let result = execute_allow_trust(
            &op,
            &issuer_id,
            &TxIdentity {
                source_id: &issuer_id,
                seq: 1,
                op_index: 0,
            },
            &mut state,
            &context,
        )
        .unwrap();
        match result {
            OperationResult::OpInner(OperationResultTr::AllowTrust(r)) => {
                assert!(matches!(r, AllowTrustResult::CantRevoke));
            }
            other => panic!("unexpected result: {:?}", other),
        }
    }

    #[test]
    fn test_allow_trust_self_not_allowed() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let issuer_id = create_test_account_id(0);
        state.create_account(create_test_account(
            issuer_id.clone(),
            100_000_000,
            AccountFlags::RequiredFlag as u32,
        ));

        let op = AllowTrustOp {
            trustor: issuer_id.clone(),
            asset: AssetCode::CreditAlphanum4(AssetCode4([b'U', b'S', b'D', b'C'])),
            authorize: 1,
        };

        let result = execute_allow_trust(
            &op,
            &issuer_id,
            &TxIdentity {
                source_id: &issuer_id,
                seq: 1,
                op_index: 0,
            },
            &mut state,
            &context,
        )
        .expect("allow trust");
        match result {
            OperationResult::OpInner(OperationResultTr::AllowTrust(r)) => {
                assert!(matches!(r, AllowTrustResult::SelfNotAllowed));
            }
            other => panic!("unexpected result: {:?}", other),
        }
    }

    #[test]
    fn test_set_trust_line_flags_cant_revoke_with_liabilities() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let issuer_id = create_test_account_id(0);
        let trustor_id = create_test_account_id(1);
        state.create_account(create_test_account(issuer_id.clone(), 100_000_000, 0));
        state.create_account(create_test_account(trustor_id.clone(), 100_000_000, 0));

        let asset = Asset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4([b'U', b'S', b'D', b'C']),
            issuer: issuer_id.clone(),
        });

        state.create_trustline(create_test_trustline_with_liabilities(
            trustor_id.clone(),
            TrustLineAsset::CreditAlphanum4(AlphaNum4 {
                asset_code: AssetCode4([b'U', b'S', b'D', b'C']),
                issuer: issuer_id.clone(),
            }),
            100,
            1_000_000,
            AUTHORIZED_FLAG,
            0,
            1,
        ));

        let op = SetTrustLineFlagsOp {
            trustor: trustor_id.clone(),
            asset,
            clear_flags: AUTHORIZED_FLAG,
            set_flags: 0,
        };

        let result = execute_set_trust_line_flags(
            &op,
            &issuer_id,
            &TxIdentity {
                source_id: &issuer_id,
                seq: 1,
                op_index: 0,
            },
            &mut state,
            &context,
        )
        .unwrap();
        match result {
            OperationResult::OpInner(OperationResultTr::SetTrustLineFlags(r)) => {
                assert!(matches!(r, SetTrustLineFlagsResult::CantRevoke));
            }
            other => panic!("unexpected result: {:?}", other),
        }
    }

    #[test]
    fn test_set_trust_line_flags_success() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let issuer_id = create_test_account_id(0);
        let trustor_id = create_test_account_id(1);

        state.create_account(create_test_account(issuer_id.clone(), 100_000_000, 0x1)); // AUTH_REQUIRED
        state.create_account(create_test_account(trustor_id.clone(), 10_000_000, 0));

        // Create the asset and trustline
        let asset = Asset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4([b'U', b'S', b'D', b'C']),
            issuer: issuer_id.clone(),
        });

        let trustline = TrustLineEntry {
            account_id: trustor_id.clone(),
            asset: TrustLineAsset::CreditAlphanum4(AlphaNum4 {
                asset_code: AssetCode4([b'U', b'S', b'D', b'C']),
                issuer: issuer_id.clone(),
            }),
            balance: 0,
            limit: i64::MAX,
            flags: 0, // Not authorized
            ext: TrustLineEntryExt::V0,
        };
        state.create_trustline(trustline);

        let op = SetTrustLineFlagsOp {
            trustor: trustor_id.clone(),
            asset: asset.clone(),
            clear_flags: 0,
            set_flags: AUTHORIZED_FLAG,
        };

        let result = execute_set_trust_line_flags(
            &op,
            &issuer_id,
            &TxIdentity {
                source_id: &issuer_id,
                seq: 1,
                op_index: 0,
            },
            &mut state,
            &context,
        );
        assert!(result.is_ok());

        match result.unwrap() {
            OperationResult::OpInner(OperationResultTr::SetTrustLineFlags(r)) => {
                assert!(matches!(r, SetTrustLineFlagsResult::Success));
            }
            _ => panic!("Unexpected result type"),
        }

        // Verify the flag was set
        let tl = state.get_trustline(&trustor_id, &asset).unwrap();
        assert_eq!(tl.flags & AUTHORIZED_FLAG, AUTHORIZED_FLAG);
    }

    /// Regression test: Verify that SetTrustLineFlags does NOT record the issuer account
    /// in the delta when the issuer calls it on someone else's trustline.
    ///
    /// stellar-core loads the source account in a nested LedgerTxn that gets rolled back,
    /// so the source account access is NOT recorded in the transaction changes. We need to
    /// match this behavior to avoid bucket list hash mismatches.
    ///
    /// This test was added to prevent regression of the bug fixed for ledger 500254+
    /// where the issuer account was incorrectly appearing in the LIVE delta.
    #[test]
    fn test_set_trust_line_flags_does_not_record_issuer_in_delta() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let issuer_id = create_test_account_id(0);
        let trustor_id = create_test_account_id(1);

        // Create issuer with AUTH_REVOCABLE so we can test flag changes
        state.create_account(create_test_account(
            issuer_id.clone(),
            100_000_000,
            AccountFlags::RequiredFlag as u32 | AccountFlags::RevocableFlag as u32,
        ));
        state.create_account(create_test_account(trustor_id.clone(), 10_000_000, 0));

        // Create the asset and trustline (authorized)
        let asset = Asset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4([b'U', b'S', b'D', b'C']),
            issuer: issuer_id.clone(),
        });

        let trustline = TrustLineEntry {
            account_id: trustor_id.clone(),
            asset: TrustLineAsset::CreditAlphanum4(AlphaNum4 {
                asset_code: AssetCode4([b'U', b'S', b'D', b'C']),
                issuer: issuer_id.clone(),
            }),
            balance: 0,
            limit: i64::MAX,
            flags: AUTHORIZED_FLAG,
            ext: TrustLineEntryExt::V0,
        };
        state.create_trustline(trustline);

        // Clear modification tracking from setup
        state.commit();

        // Now execute SetTrustLineFlags to revoke authorization
        let op = SetTrustLineFlagsOp {
            trustor: trustor_id.clone(),
            asset: asset.clone(),
            clear_flags: AUTHORIZED_FLAG,
            set_flags: 0,
        };

        let result = execute_set_trust_line_flags(
            &op,
            &issuer_id,
            &TxIdentity {
                source_id: &issuer_id,
                seq: 1,
                op_index: 0,
            },
            &mut state,
            &context,
        );
        assert!(result.is_ok());

        // Flush changes to delta
        state.flush_modified_entries();

        // Get the delta's updated entries
        let delta = state.delta();
        let updated_entries = delta.updated_entries();

        // The issuer account should NOT be in the updated entries
        // Only the trustline should be recorded as updated
        for entry in updated_entries {
            if let stellar_xdr::curr::LedgerEntryData::Account(acc) = &entry.data {
                // Check this isn't the issuer account
                assert_ne!(
                    acc.account_id, issuer_id,
                    "Issuer account should NOT be in updated_entries for SetTrustLineFlags"
                );
            }
        }

        // The trustline SHOULD be in the updated entries (it was updated)
        let has_trustline = updated_entries.iter().any(|e| {
            matches!(&e.data, stellar_xdr::curr::LedgerEntryData::Trustline(tl)
                if tl.account_id == trustor_id)
        });
        assert!(
            has_trustline,
            "Trustline should be in updated_entries after SetTrustLineFlags"
        );
    }

    /// Regression test: Verify that AllowTrust does NOT record the issuer account
    /// in the delta when the issuer calls it on someone else's trustline.
    #[test]
    fn test_allow_trust_does_not_record_issuer_in_delta() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let issuer_id = create_test_account_id(0);
        let trustor_id = create_test_account_id(1);

        // Create issuer with AUTH_REQUIRED and AUTH_REVOCABLE
        state.create_account(create_test_account(
            issuer_id.clone(),
            100_000_000,
            AccountFlags::RequiredFlag as u32 | AccountFlags::RevocableFlag as u32,
        ));
        state.create_account(create_test_account(trustor_id.clone(), 10_000_000, 0));

        // Create trustline (authorized)
        state.create_trustline(create_test_trustline_with_liabilities(
            trustor_id.clone(),
            TrustLineAsset::CreditAlphanum4(AlphaNum4 {
                asset_code: AssetCode4([b'U', b'S', b'D', b'C']),
                issuer: issuer_id.clone(),
            }),
            0,
            1_000_000,
            AUTHORIZED_FLAG,
            0,
            0,
        ));

        // Clear modification tracking from setup
        state.commit();

        // Execute AllowTrust to revoke authorization
        let op = AllowTrustOp {
            trustor: trustor_id.clone(),
            asset: AssetCode::CreditAlphanum4(AssetCode4([b'U', b'S', b'D', b'C'])),
            authorize: 0, // Revoke
        };

        let result = execute_allow_trust(
            &op,
            &issuer_id,
            &TxIdentity {
                source_id: &issuer_id,
                seq: 1,
                op_index: 0,
            },
            &mut state,
            &context,
        );
        assert!(result.is_ok());

        // Flush changes to delta
        state.flush_modified_entries();

        // Get the delta's updated entries
        let delta = state.delta();
        let updated_entries = delta.updated_entries();

        // The issuer account should NOT be in the updated entries
        for entry in updated_entries {
            if let stellar_xdr::curr::LedgerEntryData::Account(acc) = &entry.data {
                assert_ne!(
                    acc.account_id, issuer_id,
                    "Issuer account should NOT be in updated_entries for AllowTrust"
                );
            }
        }

        // The trustline SHOULD be in the updated entries
        let has_trustline = updated_entries.iter().any(|e| {
            matches!(&e.data, stellar_xdr::curr::LedgerEntryData::Trustline(tl)
                if tl.account_id == trustor_id)
        });
        assert!(
            has_trustline,
            "Trustline should be in updated_entries after AllowTrust"
        );
    }

    /// Test SetTrustLineFlags when source is not the issuer returns NoTrustLine.
    ///
    /// C++ Reference: SetTrustLineFlagsTests.cpp - "not issuer" test section
    #[test]
    fn test_set_trust_line_flags_not_issuer() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let issuer_id = create_test_account_id(40);
        let trustor_id = create_test_account_id(41);
        let non_issuer_id = create_test_account_id(42);

        state.create_account(create_test_account(issuer_id.clone(), 100_000_000, 0));
        state.create_account(create_test_account(trustor_id.clone(), 100_000_000, 0));
        state.create_account(create_test_account(non_issuer_id.clone(), 100_000_000, 0));

        let asset = create_asset(&issuer_id);
        state.create_trustline(create_test_trustline(
            trustor_id.clone(),
            TrustLineAsset::CreditAlphanum4(AlphaNum4 {
                asset_code: AssetCode4([b'U', b'S', b'D', b'C']),
                issuer: issuer_id.clone(),
            }),
            1000,
            1_000_000,
            AccountFlags::RequiredFlag as u32,
        ));

        // Non-issuer tries to set flags
        let op = SetTrustLineFlagsOp {
            trustor: trustor_id.clone(),
            asset,
            clear_flags: 0,
            set_flags: TrustLineFlags::AuthorizedFlag as u32,
        };

        let result = execute_set_trust_line_flags(
            &op,
            &non_issuer_id,
            &TxIdentity {
                source_id: &non_issuer_id,
                seq: 1,
                op_index: 0,
            },
            &mut state,
            &context,
        )
        .unwrap();
        match result {
            OperationResult::OpInner(OperationResultTr::SetTrustLineFlags(r)) => {
                // When source != issuer, the check happens before trustline lookup,
                // so we get Malformed (not issuer) rather than NoTrustLine
                assert!(
                    matches!(r, SetTrustLineFlagsResult::Malformed),
                    "Expected Malformed when not issuer, got {:?}",
                    r
                );
            }
            _ => panic!("Unexpected result type"),
        }
    }

    /// Test SetTrustLineFlags when trustline doesn't exist returns NoTrustLine.
    ///
    /// C++ Reference: SetTrustLineFlagsTests.cpp - "no trust line" test section
    #[test]
    fn test_set_trust_line_flags_no_trust_line() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let issuer_id = create_test_account_id(43);
        let trustor_id = create_test_account_id(44);

        state.create_account(create_test_account(issuer_id.clone(), 100_000_000, 0));
        state.create_account(create_test_account(trustor_id.clone(), 100_000_000, 0));

        let asset = create_asset(&issuer_id);
        // No trustline created

        let op = SetTrustLineFlagsOp {
            trustor: trustor_id.clone(),
            asset,
            clear_flags: 0,
            set_flags: TrustLineFlags::AuthorizedFlag as u32,
        };

        let result = execute_set_trust_line_flags(
            &op,
            &issuer_id,
            &TxIdentity {
                source_id: &issuer_id,
                seq: 1,
                op_index: 0,
            },
            &mut state,
            &context,
        )
        .unwrap();
        match result {
            OperationResult::OpInner(OperationResultTr::SetTrustLineFlags(r)) => {
                assert!(
                    matches!(r, SetTrustLineFlagsResult::NoTrustLine),
                    "Expected NoTrustLine, got {:?}",
                    r
                );
            }
            _ => panic!("Unexpected result type"),
        }
    }

    /// Test AllowTrust when trustline doesn't exist returns NoTrustLine.
    ///
    /// C++ Reference: AllowTrustTests.cpp - "no trust line" test section
    #[test]
    fn test_allow_trust_no_trust_line() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let issuer_id = create_test_account_id(45);
        let trustor_id = create_test_account_id(46);

        state.create_account(create_test_account(issuer_id.clone(), 100_000_000, 0));
        state.create_account(create_test_account(trustor_id.clone(), 100_000_000, 0));

        // No trustline created

        let op = AllowTrustOp {
            trustor: trustor_id.clone(),
            asset: AssetCode::CreditAlphanum4(AssetCode4([b'U', b'S', b'D', b'C'])),
            authorize: 1,
        };

        let result = execute_allow_trust(
            &op,
            &issuer_id,
            &TxIdentity {
                source_id: &issuer_id,
                seq: 1,
                op_index: 0,
            },
            &mut state,
            &context,
        )
        .unwrap();
        match result {
            OperationResult::OpInner(OperationResultTr::AllowTrust(r)) => {
                assert!(
                    matches!(r, AllowTrustResult::NoTrustLine),
                    "Expected NoTrustLine, got {:?}",
                    r
                );
            }
            _ => panic!("Unexpected result type"),
        }
    }

    /// Regression test for ledger 61257945 hash mismatch: When both AUTH_REVOCABLE
    /// is not set AND the trustline doesn't exist, stellar-core returns CantRevoke
    /// (checks AUTH_REVOCABLE first) while we were returning NoTrustLine (checked
    /// trustline existence first). The check order must match stellar-core:
    /// isAuthRevocationValid() before trustline load.
    #[test]
    fn test_set_trust_line_flags_cant_revoke_before_no_trustline() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let issuer_id = create_test_account_id(50);
        let trustor_id = create_test_account_id(51);

        // Issuer WITHOUT AUTH_REVOCABLE
        state.create_account(create_test_account(issuer_id.clone(), 100_000_000, 0));
        state.create_account(create_test_account(trustor_id.clone(), 100_000_000, 0));

        let asset = create_asset(&issuer_id);
        // NO trustline created — both CantRevoke and NoTrustLine conditions are true

        // Try to clear AUTHORIZED_FLAG without setting AUTHORIZED — this is a revocation
        let op = SetTrustLineFlagsOp {
            trustor: trustor_id.clone(),
            asset,
            clear_flags: AUTHORIZED_FLAG,
            set_flags: 0,
        };

        let result = execute_set_trust_line_flags(
            &op,
            &issuer_id,
            &TxIdentity {
                source_id: &issuer_id,
                seq: 1,
                op_index: 0,
            },
            &mut state,
            &context,
        )
        .unwrap();
        match result {
            OperationResult::OpInner(OperationResultTr::SetTrustLineFlags(r)) => {
                // stellar-core checks AUTH_REVOCABLE before trustline existence,
                // so CantRevoke must take priority over NoTrustLine
                assert!(
                    matches!(r, SetTrustLineFlagsResult::CantRevoke),
                    "Expected CantRevoke (AUTH_REVOCABLE check before trustline load), got {:?}",
                    r
                );
            }
            _ => panic!("Unexpected result type"),
        }
    }

    /// Test SetTrustLineFlags invalid flag combination returns InvalidState.
    ///
    /// InvalidState is returned when the RESULT of clear_flags/set_flags operations
    /// would leave both AUTHORIZED_FLAG and AUTHORIZED_TO_MAINTAIN_LIABILITIES_FLAG set.
    /// (Note: setting both flags directly in set_flags returns Malformed instead)
    ///
    /// C++ Reference: SetTrustLineFlagsTests.cpp - "invalid state" test section
    #[test]
    fn test_set_trust_line_flags_invalid_state() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let issuer_id = create_test_account_id(47);
        let trustor_id = create_test_account_id(48);

        state.create_account(create_test_account(issuer_id.clone(), 100_000_000, 0));
        state.create_account(create_test_account(trustor_id.clone(), 100_000_000, 0));

        let asset = create_asset(&issuer_id);
        // Create trustline that already has AUTHORIZED_FLAG
        state.create_trustline(create_test_trustline(
            trustor_id.clone(),
            TrustLineAsset::CreditAlphanum4(AlphaNum4 {
                asset_code: AssetCode4([b'U', b'S', b'D', b'C']),
                issuer: issuer_id.clone(),
            }),
            1000,
            1_000_000,
            TrustLineFlags::AuthorizedFlag as u32, // Already authorized
        ));

        // Now try to SET AUTHORIZED_TO_MAINTAIN_LIABILITIES without clearing AUTHORIZED
        // This would result in both flags being set, which is InvalidState
        let op = SetTrustLineFlagsOp {
            trustor: trustor_id.clone(),
            asset,
            clear_flags: 0, // Not clearing AUTHORIZED_FLAG
            set_flags: TrustLineFlags::AuthorizedToMaintainLiabilitiesFlag as u32,
        };

        let result = execute_set_trust_line_flags(
            &op,
            &issuer_id,
            &TxIdentity {
                source_id: &issuer_id,
                seq: 1,
                op_index: 0,
            },
            &mut state,
            &context,
        )
        .unwrap();
        match result {
            OperationResult::OpInner(OperationResultTr::SetTrustLineFlags(r)) => {
                assert!(
                    matches!(r, SetTrustLineFlagsResult::InvalidState),
                    "Expected InvalidState for conflicting flags, got {:?}",
                    r
                );
            }
            _ => panic!("Unexpected result type"),
        }
    }

    /// Bug #1095: SetTrustLineFlags must reject trustor == source (self-trust modification).
    ///
    /// C++ Reference: SetTrustLineFlagsOpFrame::doCheckValid - "mSetTrustLineFlags.trustor == getSourceID()"
    #[test]
    fn test_set_trust_line_flags_malformed_self_trust() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let issuer_id = create_test_account_id(90);

        state.create_account(create_test_account(issuer_id.clone(), 100_000_000, 0));

        // Issuer tries to set flags on their own trustline (trustor == source)
        let asset = create_asset(&issuer_id);
        let op = SetTrustLineFlagsOp {
            trustor: issuer_id.clone(), // same as source
            asset,
            clear_flags: 0,
            set_flags: AUTHORIZED_FLAG,
        };

        let result = execute_set_trust_line_flags(
            &op,
            &issuer_id,
            &TxIdentity {
                source_id: &issuer_id,
                seq: 1,
                op_index: 0,
            },
            &mut state,
            &context,
        )
        .unwrap();
        match result {
            OperationResult::OpInner(OperationResultTr::SetTrustLineFlags(r)) => {
                assert!(
                    matches!(r, SetTrustLineFlagsResult::Malformed),
                    "Expected Malformed for self-trust, got {:?}",
                    r
                );
            }
            _ => panic!("Unexpected result type"),
        }
    }

    /// Bug #1095: SetTrustLineFlags must reject TRUSTLINE_CLAWBACK_ENABLED_FLAG in set_flags.
    /// Clawback can only be cleared, never set via this operation.
    ///
    /// C++ Reference: SetTrustLineFlagsOpFrame::doCheckValid - "!trustLineFlagIsValid"
    #[test]
    fn test_set_trust_line_flags_malformed_clawback_in_set_flags() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let issuer_id = create_test_account_id(91);
        let trustor_id = create_test_account_id(92);

        state.create_account(create_test_account(issuer_id.clone(), 100_000_000, 0));
        state.create_account(create_test_account(trustor_id.clone(), 100_000_000, 0));

        let asset = create_asset(&issuer_id);
        let op = SetTrustLineFlagsOp {
            trustor: trustor_id.clone(),
            asset,
            clear_flags: 0,
            set_flags: TRUSTLINE_CLAWBACK_ENABLED_FLAG, // Cannot set clawback via this op
        };

        let result = execute_set_trust_line_flags(
            &op,
            &issuer_id,
            &TxIdentity {
                source_id: &issuer_id,
                seq: 1,
                op_index: 0,
            },
            &mut state,
            &context,
        )
        .unwrap();
        match result {
            OperationResult::OpInner(OperationResultTr::SetTrustLineFlags(r)) => {
                assert!(
                    matches!(r, SetTrustLineFlagsResult::Malformed),
                    "Expected Malformed for clawback in set_flags, got {:?}",
                    r
                );
            }
            _ => panic!("Unexpected result type"),
        }
    }

    /// SetTrustLineFlags must reject set_flags with bits outside the valid mask.
    ///
    /// C++ Reference: SetTrustLineFlagsOpFrame::doCheckValid - "!trustLineFlagIsValid"
    #[test]
    fn test_set_trust_line_flags_malformed_invalid_set_flags_bits() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let issuer_id = create_test_account_id(93);
        let trustor_id = create_test_account_id(94);

        state.create_account(create_test_account(issuer_id.clone(), 100_000_000, 0));
        state.create_account(create_test_account(trustor_id.clone(), 100_000_000, 0));

        let asset = create_asset(&issuer_id);
        // Use a high bit (0x8) that is not in the valid set_flags mask
        let op = SetTrustLineFlagsOp {
            trustor: trustor_id.clone(),
            asset,
            clear_flags: 0,
            set_flags: 0x8, // bit outside AUTHORIZED | AUTHORIZED_TO_MAINTAIN_LIABILITIES
        };

        let result = execute_set_trust_line_flags(
            &op,
            &issuer_id,
            &TxIdentity {
                source_id: &issuer_id,
                seq: 1,
                op_index: 0,
            },
            &mut state,
            &context,
        )
        .unwrap();
        match result {
            OperationResult::OpInner(OperationResultTr::SetTrustLineFlags(r)) => {
                assert!(
                    matches!(r, SetTrustLineFlagsResult::Malformed),
                    "Expected Malformed for invalid set_flags bits, got {:?}",
                    r
                );
            }
            _ => panic!("Unexpected result type"),
        }
    }

    /// SetTrustLineFlags must reject clear_flags with bits outside the valid mask.
    ///
    /// C++ Reference: SetTrustLineFlagsOpFrame::doCheckValid - "!trustLineFlagMaskCheckIsValid"
    #[test]
    fn test_set_trust_line_flags_malformed_invalid_clear_flags_bits() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let issuer_id = create_test_account_id(95);
        let trustor_id = create_test_account_id(96);

        state.create_account(create_test_account(issuer_id.clone(), 100_000_000, 0));
        state.create_account(create_test_account(trustor_id.clone(), 100_000_000, 0));

        let asset = create_asset(&issuer_id);
        // Use a high bit (0x8) that is not in the valid clear_flags mask (0x7)
        let op = SetTrustLineFlagsOp {
            trustor: trustor_id.clone(),
            asset,
            clear_flags: 0x8, // bit outside AUTHORIZED | AUTHORIZED_TO_MAINTAIN_LIABILITIES | CLAWBACK_ENABLED
            set_flags: 0,
        };

        let result = execute_set_trust_line_flags(
            &op,
            &issuer_id,
            &TxIdentity {
                source_id: &issuer_id,
                seq: 1,
                op_index: 0,
            },
            &mut state,
            &context,
        )
        .unwrap();
        match result {
            OperationResult::OpInner(OperationResultTr::SetTrustLineFlags(r)) => {
                assert!(
                    matches!(r, SetTrustLineFlagsResult::Malformed),
                    "Expected Malformed for invalid clear_flags bits, got {:?}",
                    r
                );
            }
            _ => panic!("Unexpected result type"),
        }
    }

    // --- T-02 regression tests: pool share trustline redemption on deauthorize ---

    fn create_pool_entry(
        pool_id: PoolId,
        asset_a: Asset,
        asset_b: Asset,
        reserve_a: i64,
        reserve_b: i64,
        total_shares: i64,
        trust_line_count: i64,
    ) -> LiquidityPoolEntry {
        LiquidityPoolEntry {
            liquidity_pool_id: pool_id,
            body: LiquidityPoolEntryBody::LiquidityPoolConstantProduct(
                LiquidityPoolEntryConstantProduct {
                    params: LiquidityPoolConstantProductParameters {
                        asset_a,
                        asset_b,
                        fee: LIQUIDITY_POOL_FEE_V18,
                    },
                    reserve_a,
                    reserve_b,
                    total_pool_shares: total_shares,
                    pool_shares_trust_line_count: trust_line_count,
                },
            ),
        }
    }

    fn create_trustline_v2(
        account_id: AccountId,
        asset: TrustLineAsset,
        balance: i64,
        limit: i64,
        flags: u32,
        pool_use_count: i32,
    ) -> TrustLineEntry {
        TrustLineEntry {
            account_id,
            asset,
            balance,
            limit,
            flags,
            ext: TrustLineEntryExt::V1(TrustLineEntryV1 {
                liabilities: Liabilities {
                    buying: 0,
                    selling: 0,
                },
                ext: TrustLineEntryV1Ext::V2(TrustLineEntryExtensionV2 {
                    liquidity_pool_use_count: pool_use_count,
                    ext: TrustLineEntryExtensionV2Ext::V0,
                }),
            }),
        }
    }

    /// T-02: Verify that deauthorizing via SetTrustLineFlags redeems pool share
    /// trustlines and creates claimable balances for each asset in the pool.
    ///
    /// C++ Reference: TransactionUtils.cpp `removeOffersAndPoolShareTrustLines`
    #[test]
    fn test_set_trust_line_flags_redeems_pool_shares_on_deauthorize() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let issuer_id = create_test_account_id(60);
        let trustor_id = create_test_account_id(61);
        let other_issuer_id = create_test_account_id(62);

        // Issuer needs AUTH_REQUIRED | AUTH_REVOCABLE
        state.create_account(create_test_account(
            issuer_id.clone(),
            100_000_000,
            AccountFlags::RequiredFlag as u32 | AccountFlags::RevocableFlag as u32,
        ));
        state.create_account(create_test_account(trustor_id.clone(), 100_000_000, 0));
        state.create_account(create_test_account(other_issuer_id.clone(), 100_000_000, 0));

        let asset_a = Asset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4(*b"USD\0"),
            issuer: issuer_id.clone(),
        });
        let asset_b = Asset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4(*b"EUR\0"),
            issuer: other_issuer_id.clone(),
        });

        let pool_id = PoolId(Hash([60u8; 32]));
        state.create_liquidity_pool(create_pool_entry(
            pool_id.clone(),
            asset_a.clone(),
            asset_b.clone(),
            1000,
            2000,
            500,
            1,
        ));

        // Asset A trustline (the one being deauthorized)
        state.create_trustline(create_trustline_v2(
            trustor_id.clone(),
            TrustLineAsset::CreditAlphanum4(AlphaNum4 {
                asset_code: AssetCode4(*b"USD\0"),
                issuer: issuer_id.clone(),
            }),
            5000,
            100_000,
            AUTHORIZED_FLAG,
            1,
        ));

        // Asset B trustline
        state.create_trustline(create_trustline_v2(
            trustor_id.clone(),
            TrustLineAsset::CreditAlphanum4(AlphaNum4 {
                asset_code: AssetCode4(*b"EUR\0"),
                issuer: other_issuer_id.clone(),
            }),
            5000,
            100_000,
            AUTHORIZED_FLAG,
            1,
        ));

        // Pool share trustline: trustor holds 100 of 500 total shares
        state.create_trustline(TrustLineEntry {
            account_id: trustor_id.clone(),
            asset: TrustLineAsset::PoolShare(pool_id.clone()),
            balance: 100,
            limit: i64::MAX,
            flags: 0,
            ext: TrustLineEntryExt::V0,
        });

        // Adjust sub-entries: 2 asset TLs (1 each) + 1 pool share TL (counts as 2) = 4
        if let Some(acct) = state.get_account_mut(&trustor_id) {
            acct.num_sub_entries += 4;
        }

        let tx_source_id = issuer_id.clone();
        let tx_seq: i64 = 1;
        let op_index: u32 = 0;

        let op = SetTrustLineFlagsOp {
            trustor: trustor_id.clone(),
            asset: asset_a.clone(),
            clear_flags: AUTHORIZED_FLAG,
            set_flags: 0,
        };

        let result = execute_set_trust_line_flags(
            &op,
            &issuer_id,
            &TxIdentity {
                source_id: &tx_source_id,
                seq: tx_seq,
                op_index,
            },
            &mut state,
            &context,
        )
        .unwrap();

        match &result {
            OperationResult::OpInner(OperationResultTr::SetTrustLineFlags(r)) => {
                assert!(
                    matches!(r, SetTrustLineFlagsResult::Success),
                    "Expected Success, got {:?}",
                    r
                );
            }
            other => panic!("unexpected result: {:?}", other),
        }

        // Pool share trustline should be deleted
        assert!(state
            .get_trustline_by_trustline_asset(
                &trustor_id,
                &TrustLineAsset::PoolShare(pool_id.clone())
            )
            .is_none());

        // Claimable balances should exist:
        // amount_a = floor(100 * 1000 / 500) = 200
        // amount_b = floor(100 * 2000 / 500) = 400
        let cb_id_a = get_revoke_id(
            &TxIdentity {
                source_id: &tx_source_id,
                seq: tx_seq,
                op_index,
            },
            &pool_id,
            &asset_a,
        )
        .unwrap();
        let cb_id_b = get_revoke_id(
            &TxIdentity {
                source_id: &tx_source_id,
                seq: tx_seq,
                op_index,
            },
            &pool_id,
            &asset_b,
        )
        .unwrap();

        let cb_a = state
            .get_claimable_balance(&cb_id_a)
            .expect("claimable balance for asset_a should exist");
        assert_eq!(cb_a.amount, 200);
        assert_eq!(cb_a.asset, asset_a);

        let cb_b = state
            .get_claimable_balance(&cb_id_b)
            .expect("claimable balance for asset_b should exist");
        assert_eq!(cb_b.amount, 400);
        assert_eq!(cb_b.asset, asset_b);

        // Pool should be deleted (only 1 trust line count, now 0)
        assert!(state.get_liquidity_pool(&pool_id).is_none());

        // Pool use counts should be decremented on asset trustlines
        let tl_a = state
            .get_trustline(&trustor_id, &asset_a)
            .expect("asset A trustline should still exist");
        match &tl_a.ext {
            TrustLineEntryExt::V1(v1) => match &v1.ext {
                TrustLineEntryV1Ext::V2(v2) => {
                    assert_eq!(v2.liquidity_pool_use_count, 0);
                }
                _ => panic!("expected V2 ext"),
            },
            _ => panic!("expected V1 ext"),
        }
    }

    /// T-02: Verify that deauthorizing via AllowTrust also triggers pool share
    /// trustline redemption.
    #[test]
    fn test_allow_trust_redeems_pool_shares_on_deauthorize() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let issuer_id = create_test_account_id(63);
        let trustor_id = create_test_account_id(64);
        let other_issuer_id = create_test_account_id(65);

        state.create_account(create_test_account(
            issuer_id.clone(),
            100_000_000,
            AccountFlags::RequiredFlag as u32 | AccountFlags::RevocableFlag as u32,
        ));
        state.create_account(create_test_account(trustor_id.clone(), 100_000_000, 0));
        state.create_account(create_test_account(other_issuer_id.clone(), 100_000_000, 0));

        let asset_a = Asset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4(*b"USD\0"),
            issuer: issuer_id.clone(),
        });
        let asset_b = Asset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4(*b"EUR\0"),
            issuer: other_issuer_id.clone(),
        });

        let pool_id = PoolId(Hash([63u8; 32]));
        state.create_liquidity_pool(create_pool_entry(
            pool_id.clone(),
            asset_a.clone(),
            asset_b.clone(),
            800,
            400,
            200,
            1,
        ));

        state.create_trustline(create_trustline_v2(
            trustor_id.clone(),
            TrustLineAsset::CreditAlphanum4(AlphaNum4 {
                asset_code: AssetCode4(*b"USD\0"),
                issuer: issuer_id.clone(),
            }),
            5000,
            100_000,
            AUTHORIZED_FLAG,
            1,
        ));

        state.create_trustline(create_trustline_v2(
            trustor_id.clone(),
            TrustLineAsset::CreditAlphanum4(AlphaNum4 {
                asset_code: AssetCode4(*b"EUR\0"),
                issuer: other_issuer_id.clone(),
            }),
            5000,
            100_000,
            AUTHORIZED_FLAG,
            1,
        ));

        state.create_trustline(TrustLineEntry {
            account_id: trustor_id.clone(),
            asset: TrustLineAsset::PoolShare(pool_id.clone()),
            balance: 50,
            limit: i64::MAX,
            flags: 0,
            ext: TrustLineEntryExt::V0,
        });

        if let Some(acct) = state.get_account_mut(&trustor_id) {
            acct.num_sub_entries += 4;
        }

        let tx_source_id = issuer_id.clone();

        // Revoke via AllowTrust (authorize=0)
        let op = AllowTrustOp {
            trustor: trustor_id.clone(),
            asset: AssetCode::CreditAlphanum4(AssetCode4(*b"USD\0")),
            authorize: 0,
        };

        let result = execute_allow_trust(
            &op,
            &issuer_id,
            &TxIdentity {
                source_id: &tx_source_id,
                seq: 1,
                op_index: 0,
            },
            &mut state,
            &context,
        )
        .unwrap();

        match &result {
            OperationResult::OpInner(OperationResultTr::AllowTrust(r)) => {
                assert!(
                    matches!(r, AllowTrustResult::Success),
                    "Expected Success, got {:?}",
                    r
                );
            }
            other => panic!("unexpected result: {:?}", other),
        }

        // Pool share trustline should be deleted
        assert!(state
            .get_trustline_by_trustline_asset(
                &trustor_id,
                &TrustLineAsset::PoolShare(pool_id.clone())
            )
            .is_none());

        // Claimable balances: floor(50*800/200)=200, floor(50*400/200)=100
        let cb_id_a = get_revoke_id(
            &TxIdentity {
                source_id: &tx_source_id,
                seq: 1,
                op_index: 0,
            },
            &pool_id,
            &asset_a,
        )
        .unwrap();
        let cb_id_b = get_revoke_id(
            &TxIdentity {
                source_id: &tx_source_id,
                seq: 1,
                op_index: 0,
            },
            &pool_id,
            &asset_b,
        )
        .unwrap();

        let cb_a = state
            .get_claimable_balance(&cb_id_a)
            .expect("claimable balance for asset_a");
        assert_eq!(cb_a.amount, 200);

        let cb_b = state
            .get_claimable_balance(&cb_id_b)
            .expect("claimable balance for asset_b");
        assert_eq!(cb_b.amount, 100);
    }

    /// T-02: When pool share trustline has zero balance, no claimable balances
    /// are created but the trustline is still deleted and pool counts are updated.
    #[test]
    fn test_deauthorize_zero_balance_pool_share_no_claimable_balances() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let issuer_id = create_test_account_id(66);
        let trustor_id = create_test_account_id(67);
        let other_issuer_id = create_test_account_id(68);

        state.create_account(create_test_account(
            issuer_id.clone(),
            100_000_000,
            AccountFlags::RequiredFlag as u32 | AccountFlags::RevocableFlag as u32,
        ));
        state.create_account(create_test_account(trustor_id.clone(), 100_000_000, 0));
        state.create_account(create_test_account(other_issuer_id.clone(), 100_000_000, 0));

        let asset_a = Asset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4(*b"USD\0"),
            issuer: issuer_id.clone(),
        });
        let asset_b = Asset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4(*b"EUR\0"),
            issuer: other_issuer_id.clone(),
        });

        let pool_id = PoolId(Hash([66u8; 32]));
        state.create_liquidity_pool(create_pool_entry(
            pool_id.clone(),
            asset_a.clone(),
            asset_b.clone(),
            1000,
            2000,
            500,
            1,
        ));

        state.create_trustline(create_trustline_v2(
            trustor_id.clone(),
            TrustLineAsset::CreditAlphanum4(AlphaNum4 {
                asset_code: AssetCode4(*b"USD\0"),
                issuer: issuer_id.clone(),
            }),
            5000,
            100_000,
            AUTHORIZED_FLAG,
            1,
        ));

        state.create_trustline(create_trustline_v2(
            trustor_id.clone(),
            TrustLineAsset::CreditAlphanum4(AlphaNum4 {
                asset_code: AssetCode4(*b"EUR\0"),
                issuer: other_issuer_id.clone(),
            }),
            5000,
            100_000,
            AUTHORIZED_FLAG,
            1,
        ));

        // Pool share trustline with ZERO balance
        state.create_trustline(TrustLineEntry {
            account_id: trustor_id.clone(),
            asset: TrustLineAsset::PoolShare(pool_id.clone()),
            balance: 0,
            limit: i64::MAX,
            flags: 0,
            ext: TrustLineEntryExt::V0,
        });

        if let Some(acct) = state.get_account_mut(&trustor_id) {
            acct.num_sub_entries += 4;
        }

        let op = SetTrustLineFlagsOp {
            trustor: trustor_id.clone(),
            asset: asset_a.clone(),
            clear_flags: AUTHORIZED_FLAG,
            set_flags: 0,
        };

        let result = execute_set_trust_line_flags(
            &op,
            &issuer_id,
            &TxIdentity {
                source_id: &issuer_id,
                seq: 1,
                op_index: 0,
            },
            &mut state,
            &context,
        )
        .unwrap();

        match &result {
            OperationResult::OpInner(OperationResultTr::SetTrustLineFlags(r)) => {
                assert!(
                    matches!(r, SetTrustLineFlagsResult::Success),
                    "Expected Success, got {:?}",
                    r
                );
            }
            other => panic!("unexpected result: {:?}", other),
        }

        // Pool share trustline should be deleted even with 0 balance
        assert!(state
            .get_trustline_by_trustline_asset(
                &trustor_id,
                &TrustLineAsset::PoolShare(pool_id.clone())
            )
            .is_none());

        // No claimable balances should be created for zero balance
        let cb_id_a = get_revoke_id(
            &TxIdentity {
                source_id: &issuer_id,
                seq: 1,
                op_index: 0,
            },
            &pool_id,
            &asset_a,
        )
        .unwrap();
        let cb_id_b = get_revoke_id(
            &TxIdentity {
                source_id: &issuer_id,
                seq: 1,
                op_index: 0,
            },
            &pool_id,
            &asset_b,
        )
        .unwrap();
        assert!(state.get_claimable_balance(&cb_id_a).is_none());
        assert!(state.get_claimable_balance(&cb_id_b).is_none());

        // Pool should still be deleted (trust line count was 1, now 0)
        assert!(state.get_liquidity_pool(&pool_id).is_none());
    }

    /// T-02: When the trustor is the issuer of one of the pool's assets,
    /// no claimable balance is created for that asset (issuer can mint).
    #[test]
    fn test_deauthorize_pool_share_issuer_skips_claimable_balance() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let issuer_id = create_test_account_id(69);
        let trustor_id = create_test_account_id(70);

        // The trustor is the issuer of asset_b
        state.create_account(create_test_account(
            issuer_id.clone(),
            100_000_000,
            AccountFlags::RequiredFlag as u32 | AccountFlags::RevocableFlag as u32,
        ));
        state.create_account(create_test_account(trustor_id.clone(), 100_000_000, 0));

        let asset_a = Asset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4(*b"USD\0"),
            issuer: issuer_id.clone(),
        });
        // trustor IS the issuer of asset_b
        let asset_b = Asset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4(*b"EUR\0"),
            issuer: trustor_id.clone(),
        });

        let pool_id = PoolId(Hash([69u8; 32]));
        state.create_liquidity_pool(create_pool_entry(
            pool_id.clone(),
            asset_a.clone(),
            asset_b.clone(),
            1000,
            2000,
            500,
            1,
        ));

        state.create_trustline(create_trustline_v2(
            trustor_id.clone(),
            TrustLineAsset::CreditAlphanum4(AlphaNum4 {
                asset_code: AssetCode4(*b"USD\0"),
                issuer: issuer_id.clone(),
            }),
            5000,
            100_000,
            AUTHORIZED_FLAG,
            1,
        ));

        // No asset_b trustline needed since trustor IS the issuer

        state.create_trustline(TrustLineEntry {
            account_id: trustor_id.clone(),
            asset: TrustLineAsset::PoolShare(pool_id.clone()),
            balance: 100,
            limit: i64::MAX,
            flags: 0,
            ext: TrustLineEntryExt::V0,
        });

        if let Some(acct) = state.get_account_mut(&trustor_id) {
            acct.num_sub_entries += 3; // 1 asset TL + 1 pool share TL (2 subentries)
        }

        let op = SetTrustLineFlagsOp {
            trustor: trustor_id.clone(),
            asset: asset_a.clone(),
            clear_flags: AUTHORIZED_FLAG,
            set_flags: 0,
        };

        let result = execute_set_trust_line_flags(
            &op,
            &issuer_id,
            &TxIdentity {
                source_id: &issuer_id,
                seq: 1,
                op_index: 0,
            },
            &mut state,
            &context,
        )
        .unwrap();

        match &result {
            OperationResult::OpInner(OperationResultTr::SetTrustLineFlags(r)) => {
                assert!(
                    matches!(r, SetTrustLineFlagsResult::Success),
                    "Expected Success, got {:?}",
                    r
                );
            }
            other => panic!("unexpected result: {:?}", other),
        }

        // Claimable balance for asset_a should exist (trustor is NOT issuer of A)
        let cb_id_a = get_revoke_id(
            &TxIdentity {
                source_id: &issuer_id,
                seq: 1,
                op_index: 0,
            },
            &pool_id,
            &asset_a,
        )
        .unwrap();
        let cb_a = state
            .get_claimable_balance(&cb_id_a)
            .expect("claimable balance for asset_a should exist");
        assert_eq!(cb_a.amount, 200); // floor(100 * 1000 / 500)

        // Claimable balance for asset_b should NOT exist (trustor IS issuer of B)
        let cb_id_b = get_revoke_id(
            &TxIdentity {
                source_id: &issuer_id,
                seq: 1,
                op_index: 0,
            },
            &pool_id,
            &asset_b,
        )
        .unwrap();
        assert!(
            state.get_claimable_balance(&cb_id_b).is_none(),
            "No claimable balance should be created when trustor is the asset issuer"
        );
    }

    /// Verify that pool-share trustlines are processed in canonical sorted
    /// order (by pool ID bytes) when multiple pools reference the same asset.
    ///
    /// Regression test for deterministic revocation ordering. Without sorting,
    /// HashMap iteration order could cause different claimable balance creation
    /// sequences across validators, breaking consensus.
    #[test]
    fn test_pool_share_revocation_canonical_order_with_multiple_pools() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let issuer_id = create_test_account_id(80);
        let trustor_id = create_test_account_id(81);
        let other_issuer_id = create_test_account_id(82);

        state.create_account(create_test_account(
            issuer_id.clone(),
            100_000_000,
            AccountFlags::RequiredFlag as u32 | AccountFlags::RevocableFlag as u32,
        ));
        state.create_account(create_test_account(trustor_id.clone(), 100_000_000, 0));
        state.create_account(create_test_account(other_issuer_id.clone(), 100_000_000, 0));

        let asset_a = Asset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4(*b"USD\0"),
            issuer: issuer_id.clone(),
        });
        let asset_b = Asset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4(*b"EUR\0"),
            issuer: other_issuer_id.clone(),
        });
        let asset_c = Asset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4(*b"GBP\0"),
            issuer: other_issuer_id.clone(),
        });

        // Create three pools referencing asset_a, with pool IDs that are NOT
        // in ascending order when inserted. This ensures the sort is tested.
        let pool_id_high = PoolId(Hash([0xFFu8; 32])); // highest
        let pool_id_mid = PoolId(Hash([0x80u8; 32])); // middle
        let pool_id_low = PoolId(Hash([0x01u8; 32])); // lowest

        for (pool_id, other_asset, reserve_a, reserve_b, total_shares) in [
            (&pool_id_high, &asset_b, 900i64, 300i64, 300i64),
            (&pool_id_mid, &asset_c, 600, 200, 200),
            (&pool_id_low, &asset_b, 300, 100, 100),
        ] {
            state.create_liquidity_pool(create_pool_entry(
                pool_id.clone(),
                asset_a.clone(),
                other_asset.clone(),
                reserve_a,
                reserve_b,
                total_shares,
                1,
            ));

            state.create_trustline(TrustLineEntry {
                account_id: trustor_id.clone(),
                asset: TrustLineAsset::PoolShare(pool_id.clone()),
                balance: 50,
                limit: i64::MAX,
                flags: 0,
                ext: TrustLineEntryExt::V0,
            });
        }

        // Asset trustlines
        state.create_trustline(create_trustline_v2(
            trustor_id.clone(),
            TrustLineAsset::CreditAlphanum4(AlphaNum4 {
                asset_code: AssetCode4(*b"USD\0"),
                issuer: issuer_id.clone(),
            }),
            5000,
            100_000,
            AUTHORIZED_FLAG,
            3, // 3 pool share trustlines reference this asset
        ));
        state.create_trustline(create_trustline_v2(
            trustor_id.clone(),
            TrustLineAsset::CreditAlphanum4(AlphaNum4 {
                asset_code: AssetCode4(*b"EUR\0"),
                issuer: other_issuer_id.clone(),
            }),
            5000,
            100_000,
            AUTHORIZED_FLAG,
            2,
        ));
        state.create_trustline(create_trustline_v2(
            trustor_id.clone(),
            TrustLineAsset::CreditAlphanum4(AlphaNum4 {
                asset_code: AssetCode4(*b"GBP\0"),
                issuer: other_issuer_id.clone(),
            }),
            5000,
            100_000,
            AUTHORIZED_FLAG,
            1,
        ));

        // sub-entries: 3 asset TLs + 3 pool share TLs (2 each) = 3 + 6 = 9
        if let Some(acct) = state.get_account_mut(&trustor_id) {
            acct.num_sub_entries += 9;
        }

        let op = SetTrustLineFlagsOp {
            trustor: trustor_id.clone(),
            asset: asset_a.clone(),
            clear_flags: AUTHORIZED_FLAG,
            set_flags: 0,
        };

        let result = execute_set_trust_line_flags(
            &op,
            &issuer_id,
            &TxIdentity {
                source_id: &issuer_id,
                seq: 1,
                op_index: 0,
            },
            &mut state,
            &context,
        )
        .unwrap();

        match &result {
            OperationResult::OpInner(OperationResultTr::SetTrustLineFlags(r)) => {
                assert!(
                    matches!(r, SetTrustLineFlagsResult::Success),
                    "Expected Success, got {:?}",
                    r
                );
            }
            other => panic!("unexpected result: {:?}", other),
        }

        // All three pool share trustlines should be deleted
        for pool_id in [&pool_id_low, &pool_id_mid, &pool_id_high] {
            assert!(
                state
                    .get_trustline_by_trustline_asset(
                        &trustor_id,
                        &TrustLineAsset::PoolShare(pool_id.clone())
                    )
                    .is_none(),
                "Pool share trustline for {:?} should be deleted",
                pool_id
            );
        }

        // Claimable balances should exist for each pool, verifying all three
        // were processed. The canonical sorted order is: pool_id_low (0x01),
        // pool_id_mid (0x80), pool_id_high (0xFF).
        //
        // pool_id_low: amount_a = floor(50 * 300 / 100) = 150
        let cb_low_a = get_revoke_id(
            &TxIdentity {
                source_id: &issuer_id,
                seq: 1,
                op_index: 0,
            },
            &pool_id_low,
            &asset_a,
        )
        .unwrap();
        let cb = state
            .get_claimable_balance(&cb_low_a)
            .expect("claimable balance for pool_id_low asset_a should exist");
        assert_eq!(cb.amount, 150);

        // pool_id_mid: amount_a = floor(50 * 600 / 200) = 150
        let cb_mid_a = get_revoke_id(
            &TxIdentity {
                source_id: &issuer_id,
                seq: 1,
                op_index: 0,
            },
            &pool_id_mid,
            &asset_a,
        )
        .unwrap();
        let cb = state
            .get_claimable_balance(&cb_mid_a)
            .expect("claimable balance for pool_id_mid asset_a should exist");
        assert_eq!(cb.amount, 150);

        // pool_id_high: amount_a = floor(50 * 900 / 300) = 150
        let cb_high_a = get_revoke_id(
            &TxIdentity {
                source_id: &issuer_id,
                seq: 1,
                op_index: 0,
            },
            &pool_id_high,
            &asset_a,
        )
        .unwrap();
        let cb = state
            .get_claimable_balance(&cb_high_a)
            .expect("claimable balance for pool_id_high asset_a should exist");
        assert_eq!(cb.amount, 150);
    }

    /// Bug #1101: authorize value > AUTHORIZED_TO_MAINTAIN_LIABILITIES_FLAG (2)
    /// should be rejected as Malformed. stellar-core validates this in doCheckValid
    /// before any other checks.
    #[test]
    fn test_allow_trust_malformed_invalid_authorize_value() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let issuer_id = create_test_account_id(90);
        let trustor_id = create_test_account_id(91);

        state.create_account(create_test_account(
            issuer_id.clone(),
            100_000_000,
            AccountFlags::RequiredFlag as u32,
        ));

        // authorize = 3 is invalid (only 0, 1, 2 are valid)
        let op = AllowTrustOp {
            trustor: trustor_id.clone(),
            asset: AssetCode::CreditAlphanum4(AssetCode4([b'U', b'S', b'D', b'C'])),
            authorize: 3,
        };

        let result = execute_allow_trust(
            &op,
            &issuer_id,
            &TxIdentity {
                source_id: &issuer_id,
                seq: 1,
                op_index: 0,
            },
            &mut state,
            &context,
        )
        .unwrap();
        match result {
            OperationResult::OpInner(OperationResultTr::AllowTrust(r)) => {
                assert!(
                    matches!(r, AllowTrustResult::Malformed),
                    "Expected Malformed for authorize=3, got {:?}",
                    r
                );
            }
            other => panic!("unexpected result: {:?}", other),
        }

        // Also test with a large value
        let op_large = AllowTrustOp {
            trustor: trustor_id.clone(),
            asset: AssetCode::CreditAlphanum4(AssetCode4([b'U', b'S', b'D', b'C'])),
            authorize: 255,
        };

        let result = execute_allow_trust(
            &op_large,
            &issuer_id,
            &TxIdentity {
                source_id: &issuer_id,
                seq: 1,
                op_index: 0,
            },
            &mut state,
            &context,
        )
        .unwrap();
        match result {
            OperationResult::OpInner(OperationResultTr::AllowTrust(r)) => {
                assert!(
                    matches!(r, AllowTrustResult::Malformed),
                    "Expected Malformed for authorize=255, got {:?}",
                    r
                );
            }
            other => panic!("unexpected result: {:?}", other),
        }
    }

    /// AllowTrust with authorize=0 (full revocation) that triggers pool share
    /// trustline redemption returns LowReserve when the sponsoring account for
    /// the claimable balance cannot cover the reserve.
    ///
    /// This test places the sponsoring account in a sponsorship sandwich where
    /// the sandwich sponsor does not exist, causing the claimable balance
    /// creation to fail with LowReserve.
    #[test]
    fn test_allow_trust_low_reserve() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let issuer_id = create_test_account_id(92);
        let trustor_id = create_test_account_id(93);
        let other_issuer_id = create_test_account_id(94);
        let ghost_sponsor_id = create_test_account_id(95); // does not exist in state

        // Issuer needs AUTH_REQUIRED | AUTH_REVOCABLE
        state.create_account(create_test_account(
            issuer_id.clone(),
            100_000_000,
            AccountFlags::RequiredFlag as u32 | AccountFlags::RevocableFlag as u32,
        ));
        state.create_account(create_test_account(trustor_id.clone(), 100_000_000, 0));
        state.create_account(create_test_account(other_issuer_id.clone(), 100_000_000, 0));
        // ghost_sponsor_id is NOT created — it doesn't exist

        let asset_a = Asset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4(*b"AAA\0"),
            issuer: issuer_id.clone(),
        });
        let asset_b = Asset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4(*b"BBB\0"),
            issuer: other_issuer_id.clone(),
        });

        let pool_id = PoolId(Hash([92u8; 32]));
        state.create_liquidity_pool(create_pool_entry(
            pool_id.clone(),
            asset_a.clone(),
            asset_b.clone(),
            1000,
            2000,
            500,
            1,
        ));

        // Asset trustlines
        state.create_trustline(create_trustline_v2(
            trustor_id.clone(),
            TrustLineAsset::CreditAlphanum4(AlphaNum4 {
                asset_code: AssetCode4(*b"AAA\0"),
                issuer: issuer_id.clone(),
            }),
            5000,
            100_000,
            AUTHORIZED_FLAG,
            1,
        ));
        state.create_trustline(create_trustline_v2(
            trustor_id.clone(),
            TrustLineAsset::CreditAlphanum4(AlphaNum4 {
                asset_code: AssetCode4(*b"BBB\0"),
                issuer: other_issuer_id.clone(),
            }),
            5000,
            100_000,
            AUTHORIZED_FLAG,
            1,
        ));

        // Pool share trustline with non-zero balance so claimable balances are needed
        state.create_trustline(TrustLineEntry {
            account_id: trustor_id.clone(),
            asset: TrustLineAsset::PoolShare(pool_id.clone()),
            balance: 100,
            limit: i64::MAX,
            flags: 0,
            ext: TrustLineEntryExt::V0,
        });

        if let Some(acct) = state.get_account_mut(&trustor_id) {
            acct.num_sub_entries += 4;
        }

        // Push a sponsorship sandwich: the trustor is sponsored by the ghost
        // (non-existent) account. When redeem_into_claimable_balance looks up
        // active_sponsor_for(cb_sponsoring_acc_id) and finds the ghost sponsor,
        // it will try to load the ghost sponsor account, find it missing, and
        // return LowReserve.
        state.push_sponsorship(ghost_sponsor_id.clone(), trustor_id.clone());

        let op = AllowTrustOp {
            trustor: trustor_id.clone(),
            asset: AssetCode::CreditAlphanum4(AssetCode4(*b"AAA\0")),
            authorize: 0,
        };

        let result = execute_allow_trust(
            &op,
            &issuer_id,
            &TxIdentity {
                source_id: &issuer_id,
                seq: 1,
                op_index: 0,
            },
            &mut state,
            &context,
        )
        .unwrap();
        match result {
            OperationResult::OpInner(OperationResultTr::AllowTrust(r)) => {
                assert!(
                    matches!(r, AllowTrustResult::LowReserve),
                    "Expected LowReserve, got {:?}",
                    r
                );
            }
            other => panic!("unexpected result: {:?}", other),
        }
    }

    /// AUTH_REQUIRED (TrustNotRequired) is dead code for protocol 24+.
    /// stellar-core removed the AUTH_REQUIRED check from AllowTrust in protocol 16
    /// (CAP-0035). Since henyey only supports protocol 24+, this code path
    /// is unreachable in production. This test is ignored as documentation.
    #[test]
    #[ignore]
    fn test_allow_trust_trust_not_required() {
        // In stellar-core < protocol 16, AllowTrust would return TrustNotRequired
        // if the issuer did not have the AUTH_REQUIRED flag set. This check was
        // removed in protocol 16 (CAP-0035), and since henyey only supports
        // protocol 24+, the TrustNotRequired result code is dead code.
        //
        // The existing test_allow_trust_no_auth_required test above demonstrates
        // that AllowTrust succeeds even without AUTH_REQUIRED in protocol 24+.
    }

    /// AllowTrust returns Malformed when the source (issuer) account doesn't exist.
    #[test]
    fn test_allow_trust_malformed_source_not_found() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(80);
        let trustor_id = create_test_account_id(81);

        // source_id NOT created in state
        state.create_account(create_test_account(trustor_id.clone(), 100_000_000, 0));

        let op = AllowTrustOp {
            trustor: trustor_id,
            asset: AssetCode::CreditAlphanum4(AssetCode4(*b"USD\0")),
            authorize: TrustLineFlags::AuthorizedFlag as u32,
        };

        let tx_id = TxIdentity {
            source_id: &source_id,
            seq: 0,
            op_index: 0,
        };

        let result = execute_allow_trust(&op, &source_id, &tx_id, &mut state, &context);
        match result.unwrap() {
            OperationResult::OpInner(OperationResultTr::AllowTrust(r)) => {
                assert!(
                    matches!(r, AllowTrustResult::Malformed),
                    "Expected Malformed, got {:?}",
                    r
                );
            }
            other => panic!("unexpected: {:?}", other),
        }
    }

    /// AllowTrust revoke returns OpTooManySponsoring (not LowReserve) when the
    /// claimable balance sponsor's num_sponsoring is at u32::MAX.
    ///
    /// Non-sandwich path: the trustor owns the pool share trustline directly
    /// (no entry sponsor) and has num_sponsoring == u32::MAX.
    #[test]
    fn test_allow_trust_revoke_too_many_sponsoring_non_sandwich() {
        use crate::test_utils::create_test_account_with_sponsorship;

        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let issuer_id = create_test_account_id(200);
        let trustor_id = create_test_account_id(201);
        let other_issuer_id = create_test_account_id(202);

        // Issuer needs AUTH_REQUIRED | AUTH_REVOCABLE
        state.create_account(create_test_account(
            issuer_id.clone(),
            100_000_000,
            AccountFlags::RequiredFlag as u32 | AccountFlags::RevocableFlag as u32,
        ));
        // Trustor: num_sponsoring == u32::MAX so any +1 would overflow
        state.create_account(create_test_account_with_sponsorship(
            trustor_id.clone(),
            100_000_000,
            4,        // num_sub_entries (2 asset TLs + 1 pool share TL × multiplier 2)
            0,        // num_sponsored
            u32::MAX, // num_sponsoring — at the limit
        ));
        state.create_account(create_test_account(other_issuer_id.clone(), 100_000_000, 0));

        let asset_a = Asset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4(*b"AAA\0"),
            issuer: issuer_id.clone(),
        });
        let asset_b = Asset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4(*b"BBB\0"),
            issuer: other_issuer_id.clone(),
        });

        let pool_id = PoolId(Hash([200u8; 32]));
        state.create_liquidity_pool(create_pool_entry(
            pool_id.clone(),
            asset_a.clone(),
            asset_b.clone(),
            1000,
            2000,
            500,
            1,
        ));

        // Asset trustlines
        state.create_trustline(create_trustline_v2(
            trustor_id.clone(),
            TrustLineAsset::CreditAlphanum4(AlphaNum4 {
                asset_code: AssetCode4(*b"AAA\0"),
                issuer: issuer_id.clone(),
            }),
            5000,
            100_000,
            AUTHORIZED_FLAG,
            1,
        ));
        state.create_trustline(create_trustline_v2(
            trustor_id.clone(),
            TrustLineAsset::CreditAlphanum4(AlphaNum4 {
                asset_code: AssetCode4(*b"BBB\0"),
                issuer: other_issuer_id.clone(),
            }),
            5000,
            100_000,
            AUTHORIZED_FLAG,
            1,
        ));

        // Pool share trustline with non-zero balance
        state.create_trustline(TrustLineEntry {
            account_id: trustor_id.clone(),
            asset: TrustLineAsset::PoolShare(pool_id.clone()),
            balance: 100,
            limit: i64::MAX,
            flags: 0,
            ext: TrustLineEntryExt::V0,
        });

        // No sponsorship sandwich — cb_sponsoring_acc_id IS the trustor
        let op = AllowTrustOp {
            trustor: trustor_id.clone(),
            asset: AssetCode::CreditAlphanum4(AssetCode4(*b"AAA\0")),
            authorize: 0,
        };

        let result = execute_allow_trust(
            &op,
            &issuer_id,
            &TxIdentity {
                source_id: &issuer_id,
                seq: 1,
                op_index: 0,
            },
            &mut state,
            &context,
        )
        .unwrap();

        assert!(
            matches!(result, OperationResult::OpTooManySponsoring),
            "Expected OpTooManySponsoring, got {:?}",
            result
        );
    }

    /// AllowTrust revoke returns OpTooManySponsoring (not LowReserve) when a
    /// sandwich sponsor's num_sponsoring is at u32::MAX.
    ///
    /// Sandwich path: the trustor is in a sponsorship sandwich; the sandwich
    /// sponsor exists but has num_sponsoring == u32::MAX.
    #[test]
    fn test_allow_trust_revoke_too_many_sponsoring_sandwich() {
        use crate::test_utils::create_test_account_with_sponsorship;

        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let issuer_id = create_test_account_id(210);
        let trustor_id = create_test_account_id(211);
        let other_issuer_id = create_test_account_id(212);
        let sandwich_sponsor_id = create_test_account_id(213);

        // Issuer needs AUTH_REQUIRED | AUTH_REVOCABLE
        state.create_account(create_test_account(
            issuer_id.clone(),
            100_000_000,
            AccountFlags::RequiredFlag as u32 | AccountFlags::RevocableFlag as u32,
        ));
        state.create_account(create_test_account(trustor_id.clone(), 100_000_000, 0));
        state.create_account(create_test_account(other_issuer_id.clone(), 100_000_000, 0));
        // Sandwich sponsor: exists but num_sponsoring == u32::MAX
        state.create_account(create_test_account_with_sponsorship(
            sandwich_sponsor_id.clone(),
            100_000_000,
            0,
            0,
            u32::MAX,
        ));

        let asset_a = Asset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4(*b"AAA\0"),
            issuer: issuer_id.clone(),
        });
        let asset_b = Asset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4(*b"BBB\0"),
            issuer: other_issuer_id.clone(),
        });

        let pool_id = PoolId(Hash([210u8; 32]));
        state.create_liquidity_pool(create_pool_entry(
            pool_id.clone(),
            asset_a.clone(),
            asset_b.clone(),
            1000,
            2000,
            500,
            1,
        ));

        // Asset trustlines
        state.create_trustline(create_trustline_v2(
            trustor_id.clone(),
            TrustLineAsset::CreditAlphanum4(AlphaNum4 {
                asset_code: AssetCode4(*b"AAA\0"),
                issuer: issuer_id.clone(),
            }),
            5000,
            100_000,
            AUTHORIZED_FLAG,
            1,
        ));
        state.create_trustline(create_trustline_v2(
            trustor_id.clone(),
            TrustLineAsset::CreditAlphanum4(AlphaNum4 {
                asset_code: AssetCode4(*b"BBB\0"),
                issuer: other_issuer_id.clone(),
            }),
            5000,
            100_000,
            AUTHORIZED_FLAG,
            1,
        ));

        // Pool share trustline with non-zero balance
        state.create_trustline(TrustLineEntry {
            account_id: trustor_id.clone(),
            asset: TrustLineAsset::PoolShare(pool_id.clone()),
            balance: 100,
            limit: i64::MAX,
            flags: 0,
            ext: TrustLineEntryExt::V0,
        });

        if let Some(acct) = state.get_account_mut(&trustor_id) {
            acct.num_sub_entries += 4;
        }

        // Push sandwich: trustor is sponsored by sandwich_sponsor_id
        state.push_sponsorship(sandwich_sponsor_id.clone(), trustor_id.clone());

        let op = AllowTrustOp {
            trustor: trustor_id.clone(),
            asset: AssetCode::CreditAlphanum4(AssetCode4(*b"AAA\0")),
            authorize: 0,
        };

        let result = execute_allow_trust(
            &op,
            &issuer_id,
            &TxIdentity {
                source_id: &issuer_id,
                seq: 1,
                op_index: 0,
            },
            &mut state,
            &context,
        )
        .unwrap();

        assert!(
            matches!(result, OperationResult::OpTooManySponsoring),
            "Expected OpTooManySponsoring, got {:?}",
            result
        );
    }

    /// Test AUDIT-071: AllowTrust with invalid asset code returns Malformed.
    #[test]
    fn test_audit_071_allow_trust_invalid_asset_code_returns_malformed() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let issuer_id = create_test_account_id(1);
        let trustor_id = create_test_account_id(2);

        state.create_account(AccountEntry {
            account_id: issuer_id.clone(),
            balance: 100_000_000,
            seq_num: SequenceNumber(0),
            num_sub_entries: 0,
            inflation_dest: None,
            flags: AccountFlags::RevocableFlag as u32,
            home_domain: stellar_xdr::curr::String32::default(),
            thresholds: Thresholds([1, 0, 0, 0]),
            signers: Vec::new().try_into().unwrap(),
            ext: AccountEntryExt::V0,
        });

        // Asset code with embedded null byte: [b'U', 0, b'S', b'D']
        let invalid_code = AssetCode4([b'U', 0, b'S', b'D']);
        let op = AllowTrustOp {
            trustor: trustor_id.clone(),
            asset: stellar_xdr::curr::AssetCode::CreditAlphanum4(invalid_code),
            authorize: 1,
        };

        let tx_id = TxIdentity {
            source_id: &issuer_id,
            seq: 1,
            op_index: 0,
        };

        let result = execute_allow_trust(&op, &issuer_id, &tx_id, &mut state, &context).unwrap();
        match result {
            OperationResult::OpInner(OperationResultTr::AllowTrust(r)) => {
                assert!(
                    matches!(r, AllowTrustResult::Malformed),
                    "Expected Malformed for invalid asset code, got {:?}",
                    r
                );
            }
            other => panic!("Expected AllowTrust result, got {:?}", other),
        }
    }

    /// Test AUDIT-071: SetTrustLineFlags with invalid asset code returns Malformed.
    #[test]
    fn test_audit_071_set_trust_line_flags_invalid_asset_returns_malformed() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let issuer_id = create_test_account_id(1);
        let trustor_id = create_test_account_id(2);

        state.create_account(AccountEntry {
            account_id: issuer_id.clone(),
            balance: 100_000_000,
            seq_num: SequenceNumber(0),
            num_sub_entries: 0,
            inflation_dest: None,
            flags: AccountFlags::RevocableFlag as u32,
            home_domain: stellar_xdr::curr::String32::default(),
            thresholds: Thresholds([1, 0, 0, 0]),
            signers: Vec::new().try_into().unwrap(),
            ext: AccountEntryExt::V0,
        });

        // Invalid asset with embedded null in code
        let invalid_asset = Asset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4([b'U', 0, b'S', b'D']),
            issuer: issuer_id.clone(),
        });

        let op = SetTrustLineFlagsOp {
            trustor: trustor_id.clone(),
            asset: invalid_asset,
            clear_flags: 0,
            set_flags: 1, // AUTHORIZED_FLAG
        };

        let tx_id = TxIdentity {
            source_id: &issuer_id,
            seq: 1,
            op_index: 0,
        };

        let result =
            execute_set_trust_line_flags(&op, &issuer_id, &tx_id, &mut state, &context).unwrap();
        match result {
            OperationResult::OpInner(OperationResultTr::SetTrustLineFlags(r)) => {
                assert!(
                    matches!(r, SetTrustLineFlagsResult::Malformed),
                    "Expected Malformed for invalid asset, got {:?}",
                    r
                );
            }
            other => panic!("Expected SetTrustLineFlags result, got {:?}", other),
        }
    }

    // ========================================================================
    // Validation parity test (issue #1510)
    // ========================================================================

    /// #1505 — Trust-flag revocation leaks sponsorship counters.
    /// When deauthorizing a trustline causes sponsored offers to be deleted,
    /// the sponsor's num_sponsoring and the trustor's num_sponsored must be
    /// correctly decremented. If not, the counters leak and eventually prevent
    /// future sponsoring operations.
    #[test]
    fn test_trust_flag_revocation_decrements_sponsorship_counters() {
        use crate::test_utils::{create_test_account_id, create_test_account_with_sponsorship};

        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let issuer_id = create_test_account_id(150);
        let trustor_id = create_test_account_id(151);
        let sponsor_id = create_test_account_id(152);

        // Issuer: AUTH_REQUIRED | AUTH_REVOCABLE
        state.create_account(create_test_account(
            issuer_id.clone(),
            100_000_000,
            AccountFlags::RequiredFlag as u32 | AccountFlags::RevocableFlag as u32,
        ));

        // Trustor: has 1 subentry (the trustline), 1 sponsored entry
        state.create_account(create_test_account_with_sponsorship(
            trustor_id.clone(),
            100_000_000,
            2, // num_sub_entries: 1 trustline + 1 offer
            1, // num_sponsored: the offer is sponsored
            0, // num_sponsoring
        ));

        // Sponsor: sponsoring 1 entry (the trustor's offer)
        state.create_account(create_test_account_with_sponsorship(
            sponsor_id.clone(),
            100_000_000,
            0, // num_sub_entries
            0, // num_sponsored
            1, // num_sponsoring
        ));

        let asset = Asset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4(*b"USD\0"),
            issuer: issuer_id.clone(),
        });

        // Create authorized trustline for trustor
        state.create_trustline(TrustLineEntry {
            account_id: trustor_id.clone(),
            asset: TrustLineAsset::CreditAlphanum4(AlphaNum4 {
                asset_code: AssetCode4(*b"USD\0"),
                issuer: issuer_id.clone(),
            }),
            balance: 1000,
            limit: 10000,
            flags: TrustLineFlags::AuthorizedFlag as u32,
            ext: TrustLineEntryExt::V0,
        });

        // Create a sponsored offer for trustor selling the asset
        let offer = OfferEntry {
            seller_id: trustor_id.clone(),
            offer_id: 1,
            selling: asset.clone(),
            buying: Asset::Native,
            amount: 100,
            price: Price { n: 1, d: 1 },
            flags: 0,
            ext: OfferEntryExt::V0,
        };
        state.create_offer(offer);

        // Register sponsorship for the offer
        let offer_key = LedgerKey::Offer(LedgerKeyOffer {
            seller_id: trustor_id.clone(),
            offer_id: 1,
        });
        state.set_entry_sponsor(offer_key, sponsor_id.clone());

        // Revoke authorization (clear AUTHORIZED flag)
        let op = SetTrustLineFlagsOp {
            trustor: trustor_id.clone(),
            asset: asset.clone(),
            clear_flags: TrustLineFlags::AuthorizedFlag as u32,
            set_flags: 0,
        };
        let tx_id = TxIdentity {
            source_id: &issuer_id,
            seq: 1,
            op_index: 0,
        };

        let _result = execute_set_trust_line_flags(&op, &issuer_id, &tx_id, &mut state, &context);

        // The offer should have been deleted as part of deauthorization
        assert!(
            state.get_offer(&trustor_id, 1).is_none(),
            "Offer should be deleted on trustline deauthorization"
        );

        // Verify sponsorship counters are decremented
        let sponsor_account = state.get_account(&sponsor_id).unwrap();
        let sponsor_ext = match &sponsor_account.ext {
            AccountEntryExt::V1(v1) => match &v1.ext {
                AccountEntryExtensionV1Ext::V2(v2) => v2,
                _ => panic!("Expected V2 extension on sponsor account"),
            },
            _ => panic!("Expected V1 extension on sponsor account"),
        };
        assert_eq!(
            sponsor_ext.num_sponsoring, 0,
            "Sponsor's num_sponsoring should be decremented after offer deletion"
        );

        let trustor_account = state.get_account(&trustor_id).unwrap();
        let trustor_ext = match &trustor_account.ext {
            AccountEntryExt::V1(v1) => match &v1.ext {
                AccountEntryExtensionV1Ext::V2(v2) => v2,
                _ => panic!("Expected V2 extension on trustor account"),
            },
            _ => panic!("Expected V1 extension on trustor account"),
        };
        assert_eq!(
            trustor_ext.num_sponsored, 0,
            "Trustor's num_sponsored should be decremented after sponsored offer deletion"
        );
    }

    // ========================================================================
    // Regression tests for remove_offers_with_cleanup sponsorship errors (#1995)
    // ========================================================================

    /// Helper to set up a basic deauthorization scenario with a sponsored offer.
    /// Returns (state, context, issuer_id, trustor_id, sponsor_id, asset).
    fn setup_sponsored_offer_scenario(
        trustor_sub_entries: u32,
        trustor_num_sponsored: u32,
        sponsor_num_sponsoring: u32,
    ) -> (
        LedgerStateManager,
        LedgerContext,
        AccountId,
        AccountId,
        AccountId,
        Asset,
    ) {
        use crate::test_utils::{create_test_account_id, create_test_account_with_sponsorship};

        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let issuer_id = create_test_account_id(200);
        let trustor_id = create_test_account_id(201);
        let sponsor_id = create_test_account_id(202);

        // Issuer: AUTH_REQUIRED | AUTH_REVOCABLE
        state.create_account(create_test_account(
            issuer_id.clone(),
            100_000_000,
            AccountFlags::RequiredFlag as u32 | AccountFlags::RevocableFlag as u32,
        ));

        // Trustor
        state.create_account(create_test_account_with_sponsorship(
            trustor_id.clone(),
            100_000_000,
            trustor_sub_entries,
            trustor_num_sponsored,
            0,
        ));

        // Sponsor
        state.create_account(create_test_account_with_sponsorship(
            sponsor_id.clone(),
            100_000_000,
            0,
            0,
            sponsor_num_sponsoring,
        ));

        let asset = Asset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4(*b"USD\0"),
            issuer: issuer_id.clone(),
        });

        // Create authorized trustline
        state.create_trustline(TrustLineEntry {
            account_id: trustor_id.clone(),
            asset: TrustLineAsset::CreditAlphanum4(AlphaNum4 {
                asset_code: AssetCode4(*b"USD\0"),
                issuer: issuer_id.clone(),
            }),
            balance: 1000,
            limit: 10000,
            flags: TrustLineFlags::AuthorizedFlag as u32,
            ext: TrustLineEntryExt::V0,
        });

        // Create offer
        state.create_offer(OfferEntry {
            seller_id: trustor_id.clone(),
            offer_id: 1,
            selling: asset.clone(),
            buying: Asset::Native,
            amount: 100,
            price: Price { n: 1, d: 1 },
            flags: 0,
            ext: OfferEntryExt::V0,
        });

        // Register sponsorship
        let offer_key = LedgerKey::Offer(LedgerKeyOffer {
            seller_id: trustor_id.clone(),
            offer_id: 1,
        });
        state.set_entry_sponsor(offer_key, sponsor_id.clone());

        (state, context, issuer_id, trustor_id, sponsor_id, asset)
    }

    /// #1995 — Deauthorization with missing sponsor account returns an error
    /// instead of silently leaving inconsistent sponsorship counts.
    #[test]
    fn test_remove_offers_with_cleanup_missing_sponsor_errors() {
        let (mut state, _context, _issuer_id, trustor_id, _sponsor_id, asset) =
            setup_sponsored_offer_scenario(
                2, // num_sub_entries: trustline + offer
                1, // num_sponsored
                1, // num_sponsoring (will be irrelevant — sponsor account missing)
            );

        // Remove the sponsor account to simulate corrupt state
        state.delete_account(&_sponsor_id);

        let result = remove_offers_with_cleanup(&mut state, &trustor_id, &asset);
        assert!(
            result.is_err(),
            "Should error when sponsor account is missing"
        );

        // Verify no partial mutation: offer should still exist
        assert!(
            state.get_offer(&trustor_id, 1).is_some(),
            "Offer should not be deleted when sponsorship validation fails"
        );
    }

    /// #1995 — Deauthorization with sponsor num_sponsoring == 0 returns an error.
    #[test]
    fn test_remove_offers_with_cleanup_zero_num_sponsoring_errors() {
        let (mut state, _context, _issuer_id, trustor_id, _sponsor_id, asset) =
            setup_sponsored_offer_scenario(
                2, // num_sub_entries
                1, // num_sponsored
                0, // num_sponsoring: zero — would underflow
            );

        let result = remove_offers_with_cleanup(&mut state, &trustor_id, &asset);
        assert!(
            result.is_err(),
            "Should error when num_sponsoring would underflow"
        );

        // Verify no partial mutation
        assert!(
            state.get_offer(&trustor_id, 1).is_some(),
            "Offer should not be deleted when sponsorship validation fails"
        );

        // Verify sponsorship counts unchanged
        let (_, num_sponsored) = state.sponsorship_counts_for_account(&trustor_id).unwrap();
        assert_eq!(
            num_sponsored, 1,
            "num_sponsored should be unchanged after error"
        );
    }

    /// #1995 — Deauthorization with seller num_sponsored == 0 returns an error.
    #[test]
    fn test_remove_offers_with_cleanup_zero_num_sponsored_errors() {
        let (mut state, _context, _issuer_id, trustor_id, _sponsor_id, asset) =
            setup_sponsored_offer_scenario(
                2, // num_sub_entries
                0, // num_sponsored: zero — would underflow
                1, // num_sponsoring
            );

        let result = remove_offers_with_cleanup(&mut state, &trustor_id, &asset);
        assert!(
            result.is_err(),
            "Should error when num_sponsored would underflow"
        );

        // Verify no partial mutation
        assert!(
            state.get_offer(&trustor_id, 1).is_some(),
            "Offer should not be deleted when sponsorship validation fails"
        );

        // Verify sponsor counts unchanged
        let (num_sponsoring, _) = state.sponsorship_counts_for_account(&_sponsor_id).unwrap();
        assert_eq!(
            num_sponsoring, 1,
            "num_sponsoring should be unchanged after error"
        );
    }

    /// #1995 — Non-sponsored offer is deleted successfully without sponsorship path.
    #[test]
    fn test_remove_offers_with_cleanup_non_sponsored_succeeds() {
        use crate::test_utils::create_test_account_id;

        let mut state = LedgerStateManager::new(5_000_000, 100);

        let issuer_id = create_test_account_id(210);
        let trustor_id = create_test_account_id(211);

        state.create_account(create_test_account(
            issuer_id.clone(),
            100_000_000,
            AccountFlags::RequiredFlag as u32 | AccountFlags::RevocableFlag as u32,
        ));
        state.create_account(create_test_account(trustor_id.clone(), 100_000_000, 0));

        // Manually set num_sub_entries (create_test_account sets 0, we need 2)
        if let Some(account) = state.get_account_mut(&trustor_id) {
            account.num_sub_entries = 2; // trustline + offer
        }

        let asset = Asset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4(*b"USD\0"),
            issuer: issuer_id.clone(),
        });

        state.create_trustline(TrustLineEntry {
            account_id: trustor_id.clone(),
            asset: TrustLineAsset::CreditAlphanum4(AlphaNum4 {
                asset_code: AssetCode4(*b"USD\0"),
                issuer: issuer_id.clone(),
            }),
            balance: 1000,
            limit: 10000,
            flags: TrustLineFlags::AuthorizedFlag as u32,
            ext: TrustLineEntryExt::V0,
        });

        state.create_offer(OfferEntry {
            seller_id: trustor_id.clone(),
            offer_id: 1,
            selling: asset.clone(),
            buying: Asset::Native,
            amount: 100,
            price: Price { n: 1, d: 1 },
            flags: 0,
            ext: OfferEntryExt::V0,
        });

        // No sponsorship registered — this is a non-sponsored offer
        let result = remove_offers_with_cleanup(&mut state, &trustor_id, &asset);
        assert!(result.is_ok(), "Non-sponsored offer removal should succeed");

        // Offer should be deleted
        assert!(
            state.get_offer(&trustor_id, 1).is_none(),
            "Offer should be deleted after cleanup"
        );

        // Sub-entries should be decremented
        let account = state.get_account(&trustor_id).unwrap();
        assert_eq!(
            account.num_sub_entries, 1,
            "num_sub_entries should be decremented by 1"
        );
    }

    /// #1995 — Deauthorization with seller num_sub_entries == 0 returns an error.
    #[test]
    fn test_remove_offers_with_cleanup_zero_sub_entries_errors() {
        let (mut state, _context, _issuer_id, trustor_id, _sponsor_id, asset) =
            setup_sponsored_offer_scenario(
                0, // num_sub_entries: zero — would underflow
                1, // num_sponsored
                1, // num_sponsoring
            );

        let result = remove_offers_with_cleanup(&mut state, &trustor_id, &asset);
        assert!(
            result.is_err(),
            "Should error when num_sub_entries would underflow"
        );

        // Verify no partial mutation: offer still exists
        assert!(
            state.get_offer(&trustor_id, 1).is_some(),
            "Offer should not be deleted when sub-entry validation fails"
        );
    }

    /// #1995 — Multiple offers are processed in deterministic order (sorted by offer_id).
    #[test]
    fn test_remove_offers_with_cleanup_multiple_offers_deterministic() {
        use crate::test_utils::{create_test_account_id, create_test_account_with_sponsorship};

        let mut state = LedgerStateManager::new(5_000_000, 100);
        let _context = create_test_context();

        let issuer_id = create_test_account_id(220);
        let trustor_id = create_test_account_id(221);

        state.create_account(create_test_account(
            issuer_id.clone(),
            100_000_000,
            AccountFlags::RequiredFlag as u32 | AccountFlags::RevocableFlag as u32,
        ));
        state.create_account(create_test_account_with_sponsorship(
            trustor_id.clone(),
            100_000_000,
            4, // num_sub_entries: trustline + 3 offers
            0,
            0,
        ));

        let asset = Asset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4(*b"USD\0"),
            issuer: issuer_id.clone(),
        });

        state.create_trustline(TrustLineEntry {
            account_id: trustor_id.clone(),
            asset: TrustLineAsset::CreditAlphanum4(AlphaNum4 {
                asset_code: AssetCode4(*b"USD\0"),
                issuer: issuer_id.clone(),
            }),
            balance: 1000,
            limit: 10000,
            flags: TrustLineFlags::AuthorizedFlag as u32,
            ext: TrustLineEntryExt::V0,
        });

        // Create 3 non-sponsored offers with different IDs
        for id in [3, 1, 2] {
            state.create_offer(OfferEntry {
                seller_id: trustor_id.clone(),
                offer_id: id,
                selling: asset.clone(),
                buying: Asset::Native,
                amount: 100,
                price: Price { n: 1, d: 1 },
                flags: 0,
                ext: OfferEntryExt::V0,
            });
        }

        let result = remove_offers_with_cleanup(&mut state, &trustor_id, &asset);
        assert!(result.is_ok(), "Multiple offer removal should succeed");

        // All offers should be deleted
        for id in [1, 2, 3] {
            assert!(
                state.get_offer(&trustor_id, id).is_none(),
                "Offer {id} should be deleted"
            );
        }

        // Sub-entries decremented by 3
        let account = state.get_account(&trustor_id).unwrap();
        assert_eq!(
            account.num_sub_entries, 1,
            "num_sub_entries should be decremented by 3 (trustline remains)"
        );
    }

    /// Regression test for #2049: verify that pool-share trustline deletion
    /// during SetTrustLineFlags deauthorization produces at most one Account
    /// UPDATED entry per unique account key in the delta.
    ///
    /// This exercises the path in `redeem_pool_share_trustlines` with two
    /// pool-share trustlines sharing the same trustor. The invariant is:
    /// flush_all_accounts() must NOT be called inside the loop (unlike the
    /// single-trustline path in change_trust.rs), because doing so would
    /// produce duplicate Account UPDATED entries for the same key — violating
    /// stellar-core's at-most-one-change-per-key-per-operation guarantee.
    #[test]
    fn test_pool_share_deauth_one_account_updated_per_key() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let issuer_id = create_test_account_id(0);
        let trustor_id = create_test_account_id(1);

        // Issuer needs RequiredFlag + RevocableFlag to deauthorize
        let issuer_flags = AccountFlags::RequiredFlag as u32 | AccountFlags::RevocableFlag as u32;
        state.create_account(create_test_account(
            issuer_id.clone(),
            100_000_000,
            issuer_flags,
        ));

        // Trustor needs enough balance for reserves and enough sub-entries.
        // Pool-share trustlines have multiplier 2, so 2 pool-share TLs = 4 sub-entries.
        // Plus 3 asset trustlines = 7 sub-entries total.
        let mut trustor_acc = create_test_account(trustor_id.clone(), 100_000_000, 0);
        trustor_acc.num_sub_entries = 7;
        state.create_account(trustor_acc);

        // Create 3 assets: USDC (to be deauthorized), EUR, and GBP
        let usdc_asset = Asset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4(*b"USD\0"),
            issuer: issuer_id.clone(),
        });
        let eur_issuer = create_test_account_id(2);
        state.create_account(create_test_account(eur_issuer.clone(), 100_000_000, 0));
        let eur_asset = Asset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4(*b"EUR\0"),
            issuer: eur_issuer.clone(),
        });
        let gbp_issuer = create_test_account_id(3);
        state.create_account(create_test_account(gbp_issuer.clone(), 100_000_000, 0));
        let gbp_asset = Asset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4(*b"GBP\0"),
            issuer: gbp_issuer.clone(),
        });

        // Create asset trustlines for trustor (authorized)
        for (asset, flag) in [
            (&usdc_asset, AUTHORIZED_FLAG),
            (&eur_asset, AUTHORIZED_FLAG),
            (&gbp_asset, AUTHORIZED_FLAG),
        ] {
            let tl_asset = match asset {
                Asset::CreditAlphanum4(a) => TrustLineAsset::CreditAlphanum4(a.clone()),
                _ => unreachable!(),
            };
            state.create_trustline(create_test_trustline(
                trustor_id.clone(),
                tl_asset,
                0,
                1_000_000,
                flag,
            ));
        }

        // Set liquidityPoolUseCount on the asset trustlines involved in pools.
        // USDC is in 2 pools (EUR/USDC and GBP/USDC), so its count is 2.
        // EUR and GBP are each in 1 pool, so their counts are 1.
        fn set_pool_use_count(
            state: &mut LedgerStateManager,
            acct: &AccountId,
            asset: &Asset,
            count: i32,
        ) {
            if let Some(tl) = state.get_trustline_mut(acct, asset) {
                super::super::ensure_trustline_ext_v2(tl).liquidity_pool_use_count = count;
            }
        }
        set_pool_use_count(&mut state, &trustor_id, &usdc_asset, 2);
        set_pool_use_count(&mut state, &trustor_id, &eur_asset, 1);
        set_pool_use_count(&mut state, &trustor_id, &gbp_asset, 1);

        // Helper to compute pool ID from params
        fn compute_pool_id(params: &LiquidityPoolParameters) -> PoolId {
            PoolId(henyey_common::Hash256::hash_xdr(params).into())
        }

        // Pool 1: USDC/EUR  (uses usdc_asset)
        let params1 = LiquidityPoolParameters::LiquidityPoolConstantProduct(
            LiquidityPoolConstantProductParameters {
                asset_a: eur_asset.clone(),
                asset_b: usdc_asset.clone(),
                fee: LIQUIDITY_POOL_FEE_V18,
            },
        );
        let pool_id_1 = compute_pool_id(&params1);

        // Pool 2: USDC/GBP  (uses usdc_asset)
        let params2 = LiquidityPoolParameters::LiquidityPoolConstantProduct(
            LiquidityPoolConstantProductParameters {
                asset_a: gbp_asset.clone(),
                asset_b: usdc_asset.clone(),
                fee: LIQUIDITY_POOL_FEE_V18,
            },
        );
        let pool_id_2 = compute_pool_id(&params2);

        // Create liquidity pools
        let LiquidityPoolParameters::LiquidityPoolConstantProduct(ref cp_params1) = params1;
        state.create_liquidity_pool(LiquidityPoolEntry {
            liquidity_pool_id: pool_id_1.clone(),
            body: LiquidityPoolEntryBody::LiquidityPoolConstantProduct(
                LiquidityPoolEntryConstantProduct {
                    params: cp_params1.clone(),
                    reserve_a: 1000,
                    reserve_b: 1000,
                    total_pool_shares: 1000,
                    pool_shares_trust_line_count: 1,
                },
            ),
        });

        let LiquidityPoolParameters::LiquidityPoolConstantProduct(ref cp_params2) = params2;
        state.create_liquidity_pool(LiquidityPoolEntry {
            liquidity_pool_id: pool_id_2.clone(),
            body: LiquidityPoolEntryBody::LiquidityPoolConstantProduct(
                LiquidityPoolEntryConstantProduct {
                    params: cp_params2.clone(),
                    reserve_a: 1000,
                    reserve_b: 1000,
                    total_pool_shares: 1000,
                    pool_shares_trust_line_count: 1,
                },
            ),
        });

        // Create pool-share trustlines with zero balance (simplifies the test
        // by skipping claimable balance creation)
        state.create_trustline(create_test_trustline(
            trustor_id.clone(),
            TrustLineAsset::PoolShare(pool_id_1.clone()),
            0,
            1_000_000,
            AUTHORIZED_FLAG,
        ));
        state.create_trustline(create_test_trustline(
            trustor_id.clone(),
            TrustLineAsset::PoolShare(pool_id_2.clone()),
            0,
            1_000_000,
            AUTHORIZED_FLAG,
        ));

        // Record how many delta entries exist before the operation
        let pre_op_change_count = state.delta().change_order().len();

        // Enable op snapshots (as the real execution framework does)
        state.begin_op_snapshot();

        // Execute SetTrustLineFlags to deauthorize USDC trustline
        let op = SetTrustLineFlagsOp {
            trustor: trustor_id.clone(),
            asset: usdc_asset.clone(),
            clear_flags: AUTHORIZED_FLAG,
            set_flags: 0,
        };

        let result = execute_set_trust_line_flags(
            &op,
            &issuer_id,
            &TxIdentity {
                source_id: &issuer_id,
                seq: 1,
                op_index: 0,
            },
            &mut state,
            &context,
        )
        .expect("execute should not error");

        match &result {
            OperationResult::OpInner(OperationResultTr::SetTrustLineFlags(r)) => {
                assert!(
                    matches!(r, SetTrustLineFlagsResult::Success),
                    "expected success, got {:?}",
                    r
                );
            }
            other => panic!("unexpected result: {:?}", other),
        }

        // Simulate the post-operation flush that the framework does (apply.rs:439)
        state.flush_modified_entries();
        state.end_op_snapshot();

        // Inspect the delta entries produced by the operation
        let change_order = state.delta().change_order();
        let op_changes = &change_order[pre_op_change_count..];

        // Count Account UPDATED entries per unique account key
        let mut account_updated_keys: Vec<AccountId> = Vec::new();
        for change_ref in op_changes {
            if let crate::apply::ChangeRef::Updated(idx) = change_ref {
                let post_state = &state.delta().updated_entries()[*idx];
                if let LedgerEntryData::Account(acc) = &post_state.data {
                    account_updated_keys.push(acc.account_id.clone());
                }
            }
        }

        // Verify: at most one Account UPDATED per unique account key
        let total = account_updated_keys.len();
        account_updated_keys.dedup();
        let unique_count = account_updated_keys.len();
        assert_eq!(
            total, unique_count,
            "Expected at most one Account UPDATED per unique key, but found duplicates"
        );

        // Verify trustor account was updated (sub-entries decremented)
        assert!(
            account_updated_keys.iter().any(|k| k == &trustor_id),
            "trustor account should have an UPDATED entry"
        );

        // Verify both pool-share trustlines were deleted
        let deleted_count = op_changes
            .iter()
            .filter(|cr| matches!(cr, crate::apply::ChangeRef::Deleted(_)))
            .count();
        assert!(
            deleted_count >= 2,
            "expected at least 2 deleted entries (pool-share trustlines), got {}",
            deleted_count
        );

        // Verify trustor's num_sub_entries was decremented by 4 (2 pool-share TLs × multiplier 2)
        let trustor = state.get_account(&trustor_id).unwrap();
        assert_eq!(
            trustor.num_sub_entries, 3,
            "num_sub_entries should be 7 - 4 = 3 (3 asset trustlines remain)"
        );
    }

    // --- Tests for decrement_liquidity_pool_use_count parity fix (AUDIT-224) ---

    fn create_trustline_with_pool_use_count(
        account_id: AccountId,
        asset: TrustLineAsset,
        pool_use_count: i32,
    ) -> TrustLineEntry {
        TrustLineEntry {
            account_id,
            asset,
            balance: 1_000_000,
            limit: i64::MAX,
            flags: AUTHORIZED_FLAG,
            ext: TrustLineEntryExt::V1(TrustLineEntryV1 {
                liabilities: Liabilities {
                    buying: 0,
                    selling: 0,
                },
                ext: TrustLineEntryV1Ext::V2(TrustLineEntryExtensionV2 {
                    liquidity_pool_use_count: pool_use_count,
                    ext: TrustLineEntryExtensionV2Ext::V0,
                }),
            }),
        }
    }

    #[test]
    fn test_decrement_pool_use_count_missing_trustline() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let account_id = create_test_account_id(1);
        let issuer_id = create_test_account_id(2);
        let asset = Asset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4(*b"USD\0"),
            issuer: issuer_id,
        });

        // No trustline exists — should return TxError::Internal
        let result = super::decrement_liquidity_pool_use_count(&mut state, &asset, &account_id);
        assert!(matches!(result, Err(crate::TxError::Internal(_))));
    }

    #[test]
    fn test_decrement_pool_use_count_zero_count() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let account_id = create_test_account_id(1);
        let issuer_id = create_test_account_id(2);
        let asset = Asset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4(*b"USD\0"),
            issuer: issuer_id,
        });
        let tl_asset = TrustLineAsset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4(*b"USD\0"),
            issuer: create_test_account_id(2),
        });

        // Trustline with count=0 — decrement should produce negative → error
        state.create_trustline(create_trustline_with_pool_use_count(
            account_id.clone(),
            tl_asset,
            0,
        ));

        let result = super::decrement_liquidity_pool_use_count(&mut state, &asset, &account_id);
        assert!(matches!(result, Err(crate::TxError::Internal(_))));
    }

    #[test]
    fn test_decrement_pool_use_count_missing_v2_extension() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let account_id = create_test_account_id(1);
        let issuer_id = create_test_account_id(2);
        let asset = Asset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4(*b"USD\0"),
            issuer: issuer_id,
        });
        let tl_asset = TrustLineAsset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4(*b"USD\0"),
            issuer: create_test_account_id(2),
        });

        // Trustline without V2 extension — should error
        state.create_trustline(create_test_trustline(
            account_id.clone(),
            tl_asset,
            1_000_000,
            i64::MAX,
            AUTHORIZED_FLAG,
        ));

        let result = super::decrement_liquidity_pool_use_count(&mut state, &asset, &account_id);
        assert!(matches!(result, Err(crate::TxError::Internal(_))));
    }

    #[test]
    fn test_decrement_pool_use_count_valid() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let account_id = create_test_account_id(1);
        let issuer_id = create_test_account_id(2);
        let asset = Asset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4(*b"USD\0"),
            issuer: issuer_id,
        });
        let tl_asset = TrustLineAsset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4(*b"USD\0"),
            issuer: create_test_account_id(2),
        });

        // Trustline with count=1 — should decrement to 0 successfully
        state.create_trustline(create_trustline_with_pool_use_count(
            account_id.clone(),
            tl_asset.clone(),
            1,
        ));

        let result = super::decrement_liquidity_pool_use_count(&mut state, &asset, &account_id);
        assert!(result.is_ok());

        // Verify count is now 0
        let tl = state
            .get_trustline_by_trustline_asset(&account_id, &tl_asset)
            .unwrap();
        if let TrustLineEntryExt::V1(v1) = &tl.ext {
            if let TrustLineEntryV1Ext::V2(v2) = &v1.ext {
                assert_eq!(v2.liquidity_pool_use_count, 0);
            } else {
                panic!("expected V2 extension");
            }
        } else {
            panic!("expected V1 extension");
        }
    }

    #[test]
    fn test_decrement_pool_use_count_native_asset_noop() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let account_id = create_test_account_id(1);

        // Native asset — should return Ok(()) without looking up trustline
        let result =
            super::decrement_liquidity_pool_use_count(&mut state, &Asset::Native, &account_id);
        assert!(result.is_ok());
    }

    #[test]
    fn test_decrement_pool_use_count_issuer_noop() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let issuer_id = create_test_account_id(1);
        let asset = Asset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4(*b"USD\0"),
            issuer: issuer_id.clone(),
        });

        // Issuer of the asset — should return Ok(()) without looking up trustline
        let result = super::decrement_liquidity_pool_use_count(&mut state, &asset, &issuer_id);
        assert!(result.is_ok());
    }

    // --- Tests for decrement_pool_shares_trust_line_count parity fix ---

    #[test]
    fn test_decrement_pool_shares_tl_count_missing_pool() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let pool_id = PoolId(Hash([0u8; 32]));

        // No pool exists — should return TxError::Internal
        let result = super::decrement_pool_shares_trust_line_count(&mut state, &pool_id);
        assert!(matches!(result, Err(crate::TxError::Internal(_))));
    }

    #[test]
    fn test_decrement_pool_shares_tl_count_zero_count() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let pool_id = PoolId(Hash([1u8; 32]));

        // Create pool with count=0
        let pool = LiquidityPoolEntry {
            liquidity_pool_id: pool_id.clone(),
            body: LiquidityPoolEntryBody::LiquidityPoolConstantProduct(
                LiquidityPoolEntryConstantProduct {
                    params: LiquidityPoolConstantProductParameters {
                        asset_a: Asset::Native,
                        asset_b: Asset::CreditAlphanum4(AlphaNum4 {
                            asset_code: AssetCode4(*b"USD\0"),
                            issuer: create_test_account_id(1),
                        }),
                        fee: LIQUIDITY_POOL_FEE_V18,
                    },
                    reserve_a: 0,
                    reserve_b: 0,
                    total_pool_shares: 0,
                    pool_shares_trust_line_count: 0,
                },
            ),
        };
        state.create_liquidity_pool(pool);

        // Count=0, decrement would give -1 → error
        let result = super::decrement_pool_shares_trust_line_count(&mut state, &pool_id);
        assert!(matches!(result, Err(crate::TxError::Internal(_))));
    }

    #[test]
    fn test_decrement_pool_shares_tl_count_valid_deletes_pool() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let pool_id = PoolId(Hash([2u8; 32]));

        // Create pool with count=1
        let pool = LiquidityPoolEntry {
            liquidity_pool_id: pool_id.clone(),
            body: LiquidityPoolEntryBody::LiquidityPoolConstantProduct(
                LiquidityPoolEntryConstantProduct {
                    params: LiquidityPoolConstantProductParameters {
                        asset_a: Asset::Native,
                        asset_b: Asset::CreditAlphanum4(AlphaNum4 {
                            asset_code: AssetCode4(*b"USD\0"),
                            issuer: create_test_account_id(1),
                        }),
                        fee: LIQUIDITY_POOL_FEE_V18,
                    },
                    reserve_a: 100,
                    reserve_b: 100,
                    total_pool_shares: 100,
                    pool_shares_trust_line_count: 1,
                },
            ),
        };
        state.create_liquidity_pool(pool);

        // Count=1 → decrement to 0 → pool deleted
        let result = super::decrement_pool_shares_trust_line_count(&mut state, &pool_id);
        assert!(result.is_ok());
        assert!(state.get_liquidity_pool(&pool_id).is_none());
    }
}
