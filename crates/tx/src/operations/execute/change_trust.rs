//! ChangeTrust operation execution.

use stellar_xdr::curr::{
    AccountFlags, AccountId, Asset, ChangeTrustAsset, ChangeTrustOp, ChangeTrustResult,
    ChangeTrustResultCode, LedgerKey, LedgerKeyTrustLine, Liabilities, LiquidityPoolEntry,
    LiquidityPoolEntryBody, LiquidityPoolEntryConstantProduct, LiquidityPoolParameters,
    OperationResult, OperationResultTr, TrustLineAsset, TrustLineEntry, TrustLineEntryExt,
    TrustLineEntryExtensionV2, TrustLineEntryExtensionV2Ext, TrustLineEntryV1, TrustLineEntryV1Ext,
};

use super::{
    account_balance_after_liabilities, dec_sub_entries, inc_sub_entries,
    is_authorized_to_maintain_liabilities, require_source_account, trustline_liabilities,
    ACCOUNT_SUBENTRY_LIMIT, AUTHORIZED_FLAG, TRUSTLINE_CLAWBACK_ENABLED_FLAG,
};
use crate::state::LedgerStateManager;
use crate::validation::LedgerContext;
use crate::{Result, TxError};
// SECURITY: transaction framework handles rollback on operation failure; side effects are reverted
/// Execute a ChangeTrust operation.
pub(crate) fn execute_change_trust(
    op: &ChangeTrustOp,
    source: &AccountId,
    state: &mut LedgerStateManager,
    context: &LedgerContext,
) -> Result<OperationResult> {
    // Validate limit
    if op.limit < 0 {
        return Ok(make_result(ChangeTrustResultCode::Malformed));
    }

    let (maybe_asset, pool_params) = match &op.line {
        ChangeTrustAsset::Native => {
            return Ok(make_result(ChangeTrustResultCode::Malformed));
        }
        ChangeTrustAsset::CreditAlphanum4(a) => (Some(Asset::CreditAlphanum4(a.clone())), None),
        ChangeTrustAsset::CreditAlphanum12(a) => (Some(Asset::CreditAlphanum12(a.clone())), None),
        ChangeTrustAsset::PoolShare(params) => (None, Some(params)),
    };
    let is_pool_share = pool_params.is_some();
    let multiplier: i64 = if is_pool_share { 2 } else { 1 };
    let tl_asset = change_trust_asset_to_trust_line_asset(&op.line);

    // Check not trusting self
    if let Some(asset) = &maybe_asset {
        let issuer = get_asset_issuer(asset);
        if let Some(issuer_id) = issuer {
            if source == &issuer_id {
                return Ok(make_result(ChangeTrustResultCode::Malformed));
            }
        }
    }

    // Check source account exists
    require_source_account(state, source)?;

    // Get existing trustline if any
    let existing = state.get_trustline_by_trustline_asset(source, &tl_asset);

    if op.limit == 0 {
        // Removing trustline
        if let Some(result) = remove_trustline(
            source,
            &tl_asset,
            is_pool_share,
            pool_params,
            multiplier,
            state,
        )? {
            return Ok(result);
        }
    } else if let Some(current_tl) = existing {
        // Updating existing trustline
        let current_balance = current_tl.balance;
        let current_buying_liab = trustline_liabilities(current_tl).buying;
        if op.limit < current_balance.saturating_add(current_buying_liab) {
            return Ok(make_result(ChangeTrustResultCode::InvalidLimit));
        }

        if !is_pool_share {
            let asset = maybe_asset.as_ref().expect("asset must exist");
            let issuer = get_asset_issuer(asset);
            if let Some(issuer_id) = issuer {
                if state.get_account(&issuer_id).is_none() {
                    return Ok(make_result(ChangeTrustResultCode::NoIssuer));
                }
            }
        }

        if let Some(tl) = state.get_trustline_by_trustline_asset_mut(source, &tl_asset) {
            tl.limit = op.limit;
        }
    } else {
        // Creating new trustline
        if let Some(result) = create_trustline(
            source,
            &tl_asset,
            op.limit,
            is_pool_share,
            pool_params,
            maybe_asset.as_ref(),
            multiplier,
            state,
            context,
        )? {
            return Ok(result);
        }
    }

    Ok(make_result(ChangeTrustResultCode::Success))
}

/// Remove a trustline. Returns `Ok(Some(result))` on early error, `Ok(None)` on success.
fn remove_trustline(
    source: &AccountId,
    tl_asset: &TrustLineAsset,
    is_pool_share: bool,
    pool_params: Option<&LiquidityPoolParameters>,
    multiplier: i64,
    state: &mut LedgerStateManager,
) -> Result<Option<OperationResult>> {
    let Some(tl) = state.get_trustline_by_trustline_asset(source, tl_asset) else {
        return Ok(Some(make_result(ChangeTrustResultCode::InvalidLimit)));
    };
    if tl.balance > 0 {
        return Ok(Some(make_result(ChangeTrustResultCode::InvalidLimit)));
    }
    if trustline_liabilities(tl).buying > 0 {
        return Ok(Some(make_result(ChangeTrustResultCode::InvalidLimit)));
    }

    if !is_pool_share && liquidity_pool_use_count(tl) != 0 {
        return Ok(Some(make_result(ChangeTrustResultCode::CannotDelete)));
    }

    if is_pool_share && !manage_pool_on_deleted_trustline(state, tl_asset) {
        return Ok(Some(make_result(ChangeTrustResultCode::CannotDelete)));
    }

    if is_pool_share {
        let params = pool_params.expect("pool params must exist");
        decrement_pool_use_counts(state, source, params)?;
    }

    {
        let ledger_key = LedgerKey::Trustline(LedgerKeyTrustLine {
            account_id: source.clone(),
            asset: tl_asset.clone(),
        });
        if state.entry_sponsor(&ledger_key).is_some() {
            state.remove_entry_sponsorship_and_update_counts(&ledger_key, source, multiplier)?;
        }
    }

    // Decrease sub-entries BEFORE deleting trustline.
    // stellar-core records account STATE/UPDATED before trustline STATE/REMOVED.
    if let Some(account) = state.get_account_mut(source) {
        // stellar-core: panics on underflow (invalid account state)
        dec_sub_entries(account, multiplier as u32);
    }
    // Flush ALL account changes before recording trustline deletion.
    state.flush_all_accounts();
    state.delete_trustline_by_trustline_asset(source, tl_asset);
    Ok(None)
}

/// Create a new trustline. Returns `Ok(Some(result))` on early error, `Ok(None)` on success.
#[allow(clippy::too_many_arguments)]
fn create_trustline(
    source: &AccountId,
    tl_asset: &TrustLineAsset,
    limit: i64,
    is_pool_share: bool,
    pool_params: Option<&LiquidityPoolParameters>,
    maybe_asset: Option<&Asset>,
    multiplier: i64,
    state: &mut LedgerStateManager,
    context: &LedgerContext,
) -> Result<Option<OperationResult>> {
    // Check issuer exists before subentry limit (matches stellar-core ordering)
    if is_pool_share {
        let params = pool_params.expect("pool params must exist");
        if let Err(code) = validate_pool_share_trustlines(source, params, state) {
            return Ok(Some(make_result(code)));
        }
        increment_pool_use_counts(state, source, params)?;
    } else {
        let asset = maybe_asset.expect("asset must exist");
        let issuer = get_asset_issuer(asset);
        if let Some(issuer_id) = issuer {
            if state.get_account(&issuer_id).is_none() {
                return Ok(Some(make_result(ChangeTrustResultCode::NoIssuer)));
            }
        }
    }

    // Check subentries limit
    let source_account = require_source_account(state, source)?;
    if source_account.num_sub_entries + multiplier as u32 > ACCOUNT_SUBENTRY_LIMIT {
        return Ok(Some(OperationResult::OpTooManySubentries));
    }

    // Check source can afford new sub-entry
    let sponsor = state.active_sponsor_for(source);
    if let Some(sponsor) = &sponsor {
        let sponsor_account = state
            .get_account(sponsor)
            .ok_or(TxError::SourceAccountNotFound)?;
        let new_min_balance = state.minimum_balance_for_account_with_deltas(
            sponsor_account,
            context.protocol_version,
            0,
            multiplier,
            0,
        )?;
        let available = account_balance_after_liabilities(sponsor_account);
        if available < new_min_balance {
            return Ok(Some(make_result(ChangeTrustResultCode::LowReserve)));
        }
    } else {
        let source_account = require_source_account(state, source)?;
        let new_min_balance = state.minimum_balance_for_account(
            source_account,
            context.protocol_version,
            multiplier,
        )?;
        let available = account_balance_after_liabilities(source_account);
        if available < new_min_balance {
            return Ok(Some(make_result(ChangeTrustResultCode::LowReserve)));
        }
    }

    let trustline = TrustLineEntry {
        account_id: source.clone(),
        asset: tl_asset.clone(),
        balance: 0,
        limit,
        flags: build_trustline_flags(maybe_asset, state),
        ext: TrustLineEntryExt::V0,
    };

    if sponsor.is_some() {
        let ledger_key = LedgerKey::Trustline(LedgerKeyTrustLine {
            account_id: source.clone(),
            asset: tl_asset.clone(),
        });
        state.apply_entry_sponsorship(ledger_key, source, multiplier)?;
    }
    state.create_trustline(trustline);

    if is_pool_share {
        let params = pool_params.expect("pool params must exist");
        manage_pool_on_new_trustline(state, tl_asset, params);
    }

    if let Some(account) = state.get_account_mut(source) {
        inc_sub_entries(account, multiplier as u32);
    }
    Ok(None)
}

fn change_trust_asset_to_trust_line_asset(
    asset: &ChangeTrustAsset,
) -> stellar_xdr::curr::TrustLineAsset {
    use sha2::{Digest, Sha256};
    use stellar_xdr::curr::{Hash, Limits, PoolId, WriteXdr};

    match asset {
        ChangeTrustAsset::Native => stellar_xdr::curr::TrustLineAsset::Native,
        ChangeTrustAsset::CreditAlphanum4(a) => {
            stellar_xdr::curr::TrustLineAsset::CreditAlphanum4(a.clone())
        }
        ChangeTrustAsset::CreditAlphanum12(a) => {
            stellar_xdr::curr::TrustLineAsset::CreditAlphanum12(a.clone())
        }
        ChangeTrustAsset::PoolShare(params) => {
            // Compute pool ID as SHA256 hash of the liquidity pool parameters XDR
            let pool_id = if let Ok(xdr_bytes) = params.to_xdr(Limits::none()) {
                let mut hasher = Sha256::new();
                hasher.update(&xdr_bytes);
                Hash(hasher.finalize().into())
            } else {
                Hash([0u8; 32])
            };
            stellar_xdr::curr::TrustLineAsset::PoolShare(PoolId(pool_id))
        }
    }
}

fn get_asset_issuer(asset: &Asset) -> Option<AccountId> {
    match asset {
        Asset::Native => None,
        Asset::CreditAlphanum4(a) => Some(a.issuer.clone()),
        Asset::CreditAlphanum12(a) => Some(a.issuer.clone()),
    }
}

fn build_trustline_flags(asset: Option<&Asset>, state: &LedgerStateManager) -> u32 {
    let Some(asset) = asset else {
        return 0;
    };
    let issuer = match get_asset_issuer(asset) {
        Some(issuer_id) => issuer_id,
        None => return 0,
    };
    let Some(issuer_account) = state.get_account(&issuer) else {
        return 0;
    };
    let mut flags = 0;
    if issuer_account.flags & (AccountFlags::RequiredFlag as u32) == 0 {
        flags |= AUTHORIZED_FLAG;
    }
    if issuer_account.flags & (AccountFlags::ClawbackEnabledFlag as u32) != 0 {
        flags |= TRUSTLINE_CLAWBACK_ENABLED_FLAG;
    }
    flags
}

// SECURITY: pool-share params validated during ChangeTrust validation before execution
fn validate_pool_share_trustlines(
    source: &AccountId,
    params: &LiquidityPoolParameters,
    state: &LedgerStateManager,
) -> std::result::Result<(), ChangeTrustResultCode> {
    let LiquidityPoolParameters::LiquidityPoolConstantProduct(cp) = params;
    validate_pool_asset_trustline(source, &cp.asset_a, state)?;
    validate_pool_asset_trustline(source, &cp.asset_b, state)?;
    Ok(())
}

fn validate_pool_asset_trustline(
    source: &AccountId,
    asset: &Asset,
    state: &LedgerStateManager,
) -> std::result::Result<(), ChangeTrustResultCode> {
    if matches!(asset, Asset::Native) {
        return Ok(());
    }
    if let Some(issuer) = get_asset_issuer(asset) {
        if &issuer == source {
            return Ok(());
        }
    }
    let trustline = state
        .get_trustline(source, asset)
        .ok_or(ChangeTrustResultCode::TrustLineMissing)?;
    if !is_authorized_to_maintain_liabilities(trustline.flags) {
        return Err(ChangeTrustResultCode::NotAuthMaintainLiabilities);
    }
    Ok(())
}

fn increment_pool_use_counts(
    state: &mut LedgerStateManager,
    source: &AccountId,
    params: &LiquidityPoolParameters,
) -> Result<()> {
    let LiquidityPoolParameters::LiquidityPoolConstantProduct(cp) = params;
    adjust_pool_use_count(state, source, &cp.asset_a, 1)?;
    adjust_pool_use_count(state, source, &cp.asset_b, 1)?;
    Ok(())
}

fn decrement_pool_use_counts(
    state: &mut LedgerStateManager,
    source: &AccountId,
    params: &LiquidityPoolParameters,
) -> Result<()> {
    let LiquidityPoolParameters::LiquidityPoolConstantProduct(cp) = params;
    adjust_pool_use_count(state, source, &cp.asset_a, -1)?;
    adjust_pool_use_count(state, source, &cp.asset_b, -1)?;
    Ok(())
}

/// Adjust the liquidity pool use count on a trustline by `delta` (+1 or -1).
fn adjust_pool_use_count(
    state: &mut LedgerStateManager,
    source: &AccountId,
    asset: &Asset,
    delta: i32,
) -> Result<()> {
    if matches!(asset, Asset::Native) {
        return Ok(());
    }
    if let Some(issuer) = get_asset_issuer(asset) {
        if &issuer == source {
            return Ok(());
        }
    }
    let trustline = state
        .get_trustline_mut(source, asset)
        .ok_or_else(|| TxError::Internal("missing trustline".into()))?;
    let v2 = ensure_trustline_ext_v2(trustline);
    let new_count = v2.liquidity_pool_use_count.checked_add(delta);
    match new_count {
        Some(c) if c < 0 => {
            return Err(TxError::Internal(
                "liquidity pool use count underflow".into(),
            ));
        }
        Some(c) => v2.liquidity_pool_use_count = c,
        None => {
            return Err(TxError::Internal(
                "liquidity pool use count overflow".into(),
            ));
        }
    }
    Ok(())
}

fn liquidity_pool_use_count(trustline: &TrustLineEntry) -> i32 {
    match &trustline.ext {
        TrustLineEntryExt::V0 => 0,
        TrustLineEntryExt::V1(v1) => match &v1.ext {
            TrustLineEntryV1Ext::V0 => 0,
            TrustLineEntryV1Ext::V2(v2) => v2.liquidity_pool_use_count,
        },
    }
}

fn ensure_trustline_ext_v2(trustline: &mut TrustLineEntry) -> &mut TrustLineEntryExtensionV2 {
    match &mut trustline.ext {
        TrustLineEntryExt::V0 => {
            trustline.ext = TrustLineEntryExt::V1(TrustLineEntryV1 {
                liabilities: Liabilities {
                    buying: 0,
                    selling: 0,
                },
                ext: TrustLineEntryV1Ext::V2(TrustLineEntryExtensionV2 {
                    liquidity_pool_use_count: 0,
                    ext: TrustLineEntryExtensionV2Ext::V0,
                }),
            });
        }
        TrustLineEntryExt::V1(v1) => match v1.ext {
            TrustLineEntryV1Ext::V0 => {
                v1.ext = TrustLineEntryV1Ext::V2(TrustLineEntryExtensionV2 {
                    liquidity_pool_use_count: 0,
                    ext: TrustLineEntryExtensionV2Ext::V0,
                });
            }
            TrustLineEntryV1Ext::V2(_) => {}
        },
    }

    match &mut trustline.ext {
        TrustLineEntryExt::V1(v1) => match &mut v1.ext {
            TrustLineEntryV1Ext::V2(v2) => v2,
            TrustLineEntryV1Ext::V0 => {
                unreachable!("trustline v2 ext was not initialized")
            }
        },
        TrustLineEntryExt::V0 => unreachable!("trustline v1 ext was not initialized"),
    }
}

fn manage_pool_on_new_trustline(
    state: &mut LedgerStateManager,
    tl_asset: &TrustLineAsset,
    params: &LiquidityPoolParameters,
) {
    let pool_id = match tl_asset {
        TrustLineAsset::PoolShare(pool_id) => pool_id.clone(),
        _ => return,
    };

    if let Some(pool) = state.get_liquidity_pool_mut(&pool_id) {
        let LiquidityPoolEntryBody::LiquidityPoolConstantProduct(cp) = &mut pool.body;
        cp.pool_shares_trust_line_count += 1;
        return;
    }

    let LiquidityPoolParameters::LiquidityPoolConstantProduct(cp_params) = params;
    let entry = LiquidityPoolEntry {
        liquidity_pool_id: pool_id.clone(),
        body: LiquidityPoolEntryBody::LiquidityPoolConstantProduct(
            LiquidityPoolEntryConstantProduct {
                params: cp_params.clone(),
                reserve_a: 0,
                reserve_b: 0,
                total_pool_shares: 0,
                pool_shares_trust_line_count: 1,
            },
        ),
    };
    state.create_liquidity_pool(entry);
}

fn manage_pool_on_deleted_trustline(
    state: &mut LedgerStateManager,
    tl_asset: &TrustLineAsset,
) -> bool {
    let pool_id = match tl_asset {
        TrustLineAsset::PoolShare(pool_id) => pool_id.clone(),
        _ => return true,
    };

    // First, check if pool exists and get/decrement the count
    let should_delete = {
        let Some(pool) = state.get_liquidity_pool_mut(&pool_id) else {
            return false;
        };
        let LiquidityPoolEntryBody::LiquidityPoolConstantProduct(cp) = &mut pool.body;
        if cp.pool_shares_trust_line_count == 0 {
            return false;
        }
        cp.pool_shares_trust_line_count -= 1;
        // Check if we should delete (count reached 0 after decrement)
        cp.pool_shares_trust_line_count == 0
    };

    // Delete the pool if count reached 0 (matching stellar-core behavior)
    if should_delete {
        state.delete_liquidity_pool(&pool_id);
    }

    true
}

fn make_result(code: ChangeTrustResultCode) -> OperationResult {
    let result = match code {
        ChangeTrustResultCode::Success => ChangeTrustResult::Success,
        ChangeTrustResultCode::Malformed => ChangeTrustResult::Malformed,
        ChangeTrustResultCode::NoIssuer => ChangeTrustResult::NoIssuer,
        ChangeTrustResultCode::InvalidLimit => ChangeTrustResult::InvalidLimit,
        ChangeTrustResultCode::LowReserve => ChangeTrustResult::LowReserve,
        ChangeTrustResultCode::SelfNotAllowed => ChangeTrustResult::SelfNotAllowed,
        ChangeTrustResultCode::TrustLineMissing => ChangeTrustResult::TrustLineMissing,
        ChangeTrustResultCode::CannotDelete => ChangeTrustResult::CannotDelete,
        ChangeTrustResultCode::NotAuthMaintainLiabilities => {
            ChangeTrustResult::NotAuthMaintainLiabilities
        }
    };
    OperationResult::OpInner(OperationResultTr::ChangeTrust(result))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::operations::execute::manage_offer::execute_manage_sell_offer;
    use crate::test_utils::create_test_account_id;
    use henyey_common::LIQUIDITY_POOL_FEE_V18;
    use sha2::{Digest, Sha256};
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

    fn create_test_account_with_flags(
        account_id: AccountId,
        balance: i64,
        flags: u32,
    ) -> AccountEntry {
        let mut entry = create_test_account(account_id, balance);
        entry.flags = flags;
        entry
    }

    fn create_test_context() -> LedgerContext {
        LedgerContext::testnet(1, 1000)
    }

    fn pool_id_from_params(params: &LiquidityPoolParameters) -> PoolId {
        let xdr = params.to_xdr(Limits::none()).expect("pool params xdr");
        let mut hasher = Sha256::new();
        hasher.update(&xdr);
        PoolId(Hash(hasher.finalize().into()))
    }

    fn trustline_with_pool_use_count(
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
    fn test_change_trust_create() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(0);
        let issuer_id = create_test_account_id(1);
        state.create_account(create_test_account(source_id.clone(), 100_000_000));
        state.create_account(create_test_account(issuer_id.clone(), 100_000_000));

        let asset = AlphaNum4 {
            asset_code: AssetCode4(*b"USD\0"),
            issuer: issuer_id,
        };

        let op = ChangeTrustOp {
            line: ChangeTrustAsset::CreditAlphanum4(asset),
            limit: 1_000_000_000,
        };

        let result = execute_change_trust(&op, &source_id, &mut state, &context);
        assert!(result.is_ok());

        // Verify sub-entries increased
        assert_eq!(state.get_account(&source_id).unwrap().num_sub_entries, 1);
    }

    #[test]
    fn test_change_trust_invalid_limit_no_trustline() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(0);
        let issuer_id = create_test_account_id(1);
        state.create_account(create_test_account(source_id.clone(), 100_000_000));
        state.create_account(create_test_account(issuer_id.clone(), 100_000_000));

        let asset = AlphaNum4 {
            asset_code: AssetCode4(*b"USD\0"),
            issuer: issuer_id,
        };

        let op = ChangeTrustOp {
            line: ChangeTrustAsset::CreditAlphanum4(asset),
            limit: 0,
        };

        let result = execute_change_trust(&op, &source_id, &mut state, &context);
        match result.unwrap() {
            OperationResult::OpInner(OperationResultTr::ChangeTrust(r)) => {
                assert!(matches!(r, ChangeTrustResult::InvalidLimit));
            }
            other => panic!("unexpected result: {:?}", other),
        }
    }

    #[test]
    fn test_change_trust_no_issuer() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(0);
        let issuer_id = create_test_account_id(1);
        state.create_account(create_test_account(source_id.clone(), 100_000_000));

        let asset = AlphaNum4 {
            asset_code: AssetCode4(*b"USD\0"),
            issuer: issuer_id,
        };

        let op = ChangeTrustOp {
            line: ChangeTrustAsset::CreditAlphanum4(asset),
            limit: 1_000,
        };

        let result = execute_change_trust(&op, &source_id, &mut state, &context);
        match result.unwrap() {
            OperationResult::OpInner(OperationResultTr::ChangeTrust(r)) => {
                assert!(matches!(r, ChangeTrustResult::NoIssuer));
            }
            other => panic!("unexpected result: {:?}", other),
        }
    }

    #[test]
    fn test_change_trust_self_issuer_malformed_protocol_16_plus() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let mut context = create_test_context();
        context.protocol_version = 23;

        let source_id = create_test_account_id(0);
        state.create_account(create_test_account(source_id.clone(), 100_000_000));

        let asset = AlphaNum4 {
            asset_code: AssetCode4(*b"USD\0"),
            issuer: source_id.clone(),
        };

        let op = ChangeTrustOp {
            line: ChangeTrustAsset::CreditAlphanum4(asset),
            limit: 1_000,
        };

        let result = execute_change_trust(&op, &source_id, &mut state, &context);
        match result.unwrap() {
            OperationResult::OpInner(OperationResultTr::ChangeTrust(r)) => {
                assert!(matches!(r, ChangeTrustResult::Malformed));
            }
            other => panic!("unexpected result: {:?}", other),
        }
    }

    #[test]
    fn test_change_trust_limit_below_buying_liabilities_or_delete() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(0);
        let issuer_id = create_test_account_id(1);
        let min_balance = state
            .minimum_balance_with_counts(context.protocol_version, 0, 0, 0)
            .unwrap();
        state.create_account(create_test_account(
            source_id.clone(),
            min_balance + 20_000_000,
        ));
        state.create_account(create_test_account(issuer_id.clone(), 100_000_000));

        let asset = AlphaNum4 {
            asset_code: AssetCode4(*b"IDR\0"),
            issuer: issuer_id.clone(),
        };
        let trust_asset = Asset::CreditAlphanum4(asset.clone());
        state.create_trustline(create_test_trustline(
            source_id.clone(),
            TrustLineAsset::CreditAlphanum4(asset.clone()),
            0,
            1_000,
            TrustLineFlags::AuthorizedFlag as u32,
        ));
        state.get_account_mut(&source_id).unwrap().num_sub_entries += 1;

        let offer = ManageSellOfferOp {
            selling: Asset::Native,
            buying: trust_asset.clone(),
            amount: 500,
            price: Price { n: 1, d: 1 },
            offer_id: 0,
        };
        let offer_result =
            execute_manage_sell_offer(&offer, &source_id, &mut state, &context).unwrap();
        match offer_result {
            OperationResult::OpInner(OperationResultTr::ManageSellOffer(r)) => {
                assert!(matches!(r, ManageSellOfferResult::Success(_)), "{r:?}");
            }
            other => panic!("unexpected result: {:?}", other),
        }

        let reduce_ok = ChangeTrustOp {
            line: ChangeTrustAsset::CreditAlphanum4(asset.clone()),
            limit: 500,
        };
        let reduce_ok_res = execute_change_trust(&reduce_ok, &source_id, &mut state, &context);
        match reduce_ok_res.unwrap() {
            OperationResult::OpInner(OperationResultTr::ChangeTrust(r)) => {
                assert!(matches!(r, ChangeTrustResult::Success));
            }
            other => panic!("unexpected result: {:?}", other),
        }

        let reduce_bad = ChangeTrustOp {
            line: ChangeTrustAsset::CreditAlphanum4(asset.clone()),
            limit: 499,
        };
        let reduce_bad_res = execute_change_trust(&reduce_bad, &source_id, &mut state, &context);
        match reduce_bad_res.unwrap() {
            OperationResult::OpInner(OperationResultTr::ChangeTrust(r)) => {
                assert!(matches!(r, ChangeTrustResult::InvalidLimit));
            }
            other => panic!("unexpected result: {:?}", other),
        }

        let delete_bad = ChangeTrustOp {
            line: ChangeTrustAsset::CreditAlphanum4(asset),
            limit: 0,
        };
        let delete_bad_res = execute_change_trust(&delete_bad, &source_id, &mut state, &context);
        match delete_bad_res.unwrap() {
            OperationResult::OpInner(OperationResultTr::ChangeTrust(r)) => {
                assert!(matches!(r, ChangeTrustResult::InvalidLimit));
            }
            other => panic!("unexpected result: {:?}", other),
        }
    }

    #[test]
    fn test_change_trust_native_asset_malformed() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(0);
        state.create_account(create_test_account(source_id.clone(), 100_000_000));

        let op = ChangeTrustOp {
            line: ChangeTrustAsset::Native,
            limit: 1_000,
        };

        let result = execute_change_trust(&op, &source_id, &mut state, &context);
        match result.unwrap() {
            OperationResult::OpInner(OperationResultTr::ChangeTrust(r)) => {
                assert!(matches!(r, ChangeTrustResult::Malformed));
            }
            other => panic!("unexpected result: {:?}", other),
        }
    }

    #[test]
    fn test_change_trust_delete_with_balance_invalid() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(0);
        let issuer_id = create_test_account_id(1);
        state.create_account(create_test_account(source_id.clone(), 100_000_000));
        state.create_account(create_test_account(issuer_id.clone(), 100_000_000));

        let asset = AlphaNum4 {
            asset_code: AssetCode4(*b"USD\0"),
            issuer: issuer_id,
        };

        state.create_trustline(create_test_trustline(
            source_id.clone(),
            TrustLineAsset::CreditAlphanum4(asset.clone()),
            100,
            1_000,
            TrustLineFlags::AuthorizedFlag as u32,
        ));
        state.get_account_mut(&source_id).unwrap().num_sub_entries += 1;

        let op = ChangeTrustOp {
            line: ChangeTrustAsset::CreditAlphanum4(asset),
            limit: 0,
        };

        let result = execute_change_trust(&op, &source_id, &mut state, &context);
        match result.unwrap() {
            OperationResult::OpInner(OperationResultTr::ChangeTrust(r)) => {
                assert!(matches!(r, ChangeTrustResult::InvalidLimit));
            }
            other => panic!("unexpected result: {:?}", other),
        }
    }

    #[test]
    fn test_change_trust_limit_below_balance() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(0);
        let issuer_id = create_test_account_id(1);
        state.create_account(create_test_account(source_id.clone(), 100_000_000));
        state.create_account(create_test_account(issuer_id.clone(), 100_000_000));

        let asset = AlphaNum4 {
            asset_code: AssetCode4(*b"USD\0"),
            issuer: issuer_id.clone(),
        };

        let trustline = TrustLineEntry {
            account_id: source_id.clone(),
            asset: TrustLineAsset::CreditAlphanum4(asset.clone()),
            balance: 500,
            limit: 1_000,
            flags: TrustLineFlags::AuthorizedFlag as u32,
            ext: TrustLineEntryExt::V0,
        };
        state.create_trustline(trustline);
        state.get_account_mut(&source_id).unwrap().num_sub_entries += 1;

        let op = ChangeTrustOp {
            line: ChangeTrustAsset::CreditAlphanum4(asset),
            limit: 100,
        };

        let result = execute_change_trust(&op, &source_id, &mut state, &context);
        match result.unwrap() {
            OperationResult::OpInner(OperationResultTr::ChangeTrust(r)) => {
                assert!(matches!(r, ChangeTrustResult::InvalidLimit));
            }
            other => panic!("unexpected result: {:?}", other),
        }
    }

    #[test]
    fn test_change_trust_sets_authorized_flag() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(0);
        let issuer_id = create_test_account_id(1);
        state.create_account(create_test_account(source_id.clone(), 100_000_000));
        state.create_account(create_test_account_with_flags(
            issuer_id.clone(),
            100_000_000,
            0,
        ));

        let asset = AlphaNum4 {
            asset_code: AssetCode4(*b"USD\0"),
            issuer: issuer_id.clone(),
        };

        let op = ChangeTrustOp {
            line: ChangeTrustAsset::CreditAlphanum4(asset.clone()),
            limit: 1_000,
        };

        let result = execute_change_trust(&op, &source_id, &mut state, &context);
        assert!(result.is_ok());

        let trustline = state.get_trustline(&source_id, &Asset::CreditAlphanum4(asset));
        assert!(trustline.is_some());
        let flags = trustline.unwrap().flags;
        assert!(flags & TrustLineFlags::AuthorizedFlag as u32 != 0);
    }

    #[test]
    fn test_change_trust_pool_share_creates_pool() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(0);
        let issuer_a = create_test_account_id(1);
        let issuer_b = create_test_account_id(2);
        state.create_account(create_test_account(source_id.clone(), 100_000_000));
        state.create_account(create_test_account(issuer_a.clone(), 100_000_000));
        state.create_account(create_test_account(issuer_b.clone(), 100_000_000));

        let asset_a = Asset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4(*b"USD\0"),
            issuer: issuer_a,
        });
        let asset_b = Asset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4(*b"EUR\0"),
            issuer: issuer_b,
        });

        let trustline_a = TrustLineEntry {
            account_id: source_id.clone(),
            asset: TrustLineAsset::CreditAlphanum4(match &asset_a {
                Asset::CreditAlphanum4(a) => a.clone(),
                _ => unreachable!(),
            }),
            balance: 0,
            limit: 10_000,
            flags: TrustLineFlags::AuthorizedFlag as u32,
            ext: TrustLineEntryExt::V0,
        };
        let trustline_b = TrustLineEntry {
            account_id: source_id.clone(),
            asset: TrustLineAsset::CreditAlphanum4(match &asset_b {
                Asset::CreditAlphanum4(a) => a.clone(),
                _ => unreachable!(),
            }),
            balance: 0,
            limit: 10_000,
            flags: TrustLineFlags::AuthorizedFlag as u32,
            ext: TrustLineEntryExt::V0,
        };
        state.create_trustline(trustline_a);
        state.create_trustline(trustline_b);
        state.get_account_mut(&source_id).unwrap().num_sub_entries += 2;

        let params = LiquidityPoolParameters::LiquidityPoolConstantProduct(
            LiquidityPoolConstantProductParameters {
                asset_a: asset_a.clone(),
                asset_b: asset_b.clone(),
                fee: LIQUIDITY_POOL_FEE_V18,
            },
        );
        let pool_id = pool_id_from_params(&params);
        let op = ChangeTrustOp {
            line: ChangeTrustAsset::PoolShare(params),
            limit: 1_000,
        };

        let result = execute_change_trust(&op, &source_id, &mut state, &context);
        assert!(result.is_ok());

        let pool = state.get_liquidity_pool(&pool_id);
        assert!(pool.is_some());
        let tl_asset = TrustLineAsset::PoolShare(pool_id.clone());
        let trustline = state.get_trustline_by_trustline_asset(&source_id, &tl_asset);
        assert!(trustline.is_some());
    }

    #[test]
    fn test_change_trust_cannot_delete_with_pool_use_count() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(0);
        let issuer_id = create_test_account_id(1);
        state.create_account(create_test_account(source_id.clone(), 100_000_000));
        state.create_account(create_test_account(issuer_id.clone(), 100_000_000));

        let asset = AlphaNum4 {
            asset_code: AssetCode4(*b"USD\0"),
            issuer: issuer_id,
        };
        let trustline = trustline_with_pool_use_count(
            source_id.clone(),
            TrustLineAsset::CreditAlphanum4(asset.clone()),
            0,
            1_000,
            TrustLineFlags::AuthorizedFlag as u32,
            1,
        );
        state.create_trustline(trustline);
        state.get_account_mut(&source_id).unwrap().num_sub_entries += 1;

        let op = ChangeTrustOp {
            line: ChangeTrustAsset::CreditAlphanum4(asset),
            limit: 0,
        };

        let result = execute_change_trust(&op, &source_id, &mut state, &context);
        match result.unwrap() {
            OperationResult::OpInner(OperationResultTr::ChangeTrust(r)) => {
                assert!(matches!(r, ChangeTrustResult::CannotDelete));
            }
            other => panic!("unexpected result: {:?}", other),
        }
    }

    #[test]
    fn test_change_trust_pool_share_increments_use_count() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(0);
        let issuer_a = create_test_account_id(1);
        let issuer_b = create_test_account_id(2);
        state.create_account(create_test_account(source_id.clone(), 100_000_000));
        state.create_account(create_test_account(issuer_a.clone(), 100_000_000));
        state.create_account(create_test_account(issuer_b.clone(), 100_000_000));

        let asset_a = Asset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4(*b"USD\0"),
            issuer: issuer_a,
        });
        let asset_b = Asset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4(*b"EUR\0"),
            issuer: issuer_b,
        });

        let tl_a = trustline_with_pool_use_count(
            source_id.clone(),
            TrustLineAsset::CreditAlphanum4(match &asset_a {
                Asset::CreditAlphanum4(a) => a.clone(),
                _ => unreachable!(),
            }),
            0,
            10_000,
            TrustLineFlags::AuthorizedFlag as u32,
            0,
        );
        let tl_b = trustline_with_pool_use_count(
            source_id.clone(),
            TrustLineAsset::CreditAlphanum4(match &asset_b {
                Asset::CreditAlphanum4(a) => a.clone(),
                _ => unreachable!(),
            }),
            0,
            10_000,
            TrustLineFlags::AuthorizedFlag as u32,
            0,
        );
        state.create_trustline(tl_a);
        state.create_trustline(tl_b);
        state.get_account_mut(&source_id).unwrap().num_sub_entries += 2;

        let params = LiquidityPoolParameters::LiquidityPoolConstantProduct(
            LiquidityPoolConstantProductParameters {
                asset_a: asset_a.clone(),
                asset_b: asset_b.clone(),
                fee: LIQUIDITY_POOL_FEE_V18,
            },
        );
        let op = ChangeTrustOp {
            line: ChangeTrustAsset::PoolShare(params),
            limit: 1_000,
        };

        let result = execute_change_trust(&op, &source_id, &mut state, &context);
        assert!(result.is_ok());

        let tl_a_after = state.get_trustline(&source_id, &asset_a).unwrap();
        let tl_b_after = state.get_trustline(&source_id, &asset_b).unwrap();
        assert_eq!(liquidity_pool_use_count(tl_a_after), 1);
        assert_eq!(liquidity_pool_use_count(tl_b_after), 1);
    }

    /// Regression test: When the last pool share trustline is deleted, the liquidity pool
    /// itself should be deleted (pool_shares_trust_line_count reaches 0).
    ///
    /// This matches stellar-core's behavior where removing the last trustline
    /// that references a liquidity pool causes the pool to be removed from state.
    #[test]
    fn test_change_trust_pool_deleted_when_last_trustline_removed() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(0);
        let issuer_a = create_test_account_id(1);
        let issuer_b = create_test_account_id(2);
        state.create_account(create_test_account(source_id.clone(), 100_000_000));
        state.create_account(create_test_account(issuer_a.clone(), 100_000_000));
        state.create_account(create_test_account(issuer_b.clone(), 100_000_000));

        let asset_a = Asset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4(*b"USD\0"),
            issuer: issuer_a,
        });
        let asset_b = Asset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4(*b"EUR\0"),
            issuer: issuer_b,
        });

        // Create trustlines for both assets with pool_use_count=0 initially
        let tl_a = trustline_with_pool_use_count(
            source_id.clone(),
            TrustLineAsset::CreditAlphanum4(match &asset_a {
                Asset::CreditAlphanum4(a) => a.clone(),
                _ => unreachable!(),
            }),
            0,
            10_000,
            TrustLineFlags::AuthorizedFlag as u32,
            0,
        );
        let tl_b = trustline_with_pool_use_count(
            source_id.clone(),
            TrustLineAsset::CreditAlphanum4(match &asset_b {
                Asset::CreditAlphanum4(a) => a.clone(),
                _ => unreachable!(),
            }),
            0,
            10_000,
            TrustLineFlags::AuthorizedFlag as u32,
            0,
        );
        state.create_trustline(tl_a);
        state.create_trustline(tl_b);
        state.get_account_mut(&source_id).unwrap().num_sub_entries += 2;

        // Create pool share trustline (this creates the pool with trust_line_count=1)
        let params = LiquidityPoolParameters::LiquidityPoolConstantProduct(
            LiquidityPoolConstantProductParameters {
                asset_a: asset_a.clone(),
                asset_b: asset_b.clone(),
                fee: LIQUIDITY_POOL_FEE_V18,
            },
        );
        let pool_id = pool_id_from_params(&params);
        let create_op = ChangeTrustOp {
            line: ChangeTrustAsset::PoolShare(params.clone()),
            limit: 1_000,
        };

        let result = execute_change_trust(&create_op, &source_id, &mut state, &context);
        assert!(result.is_ok());

        // Verify pool exists with trust_line_count=1
        let pool = state.get_liquidity_pool(&pool_id);
        assert!(pool.is_some(), "Pool should exist after creating trustline");
        let LiquidityPoolEntryBody::LiquidityPoolConstantProduct(cp) = &pool.unwrap().body;
        assert_eq!(
            cp.pool_shares_trust_line_count, 1,
            "Pool should have trust_line_count=1"
        );

        // Now delete the pool share trustline (limit=0)
        let delete_op = ChangeTrustOp {
            line: ChangeTrustAsset::PoolShare(params),
            limit: 0,
        };

        let result = execute_change_trust(&delete_op, &source_id, &mut state, &context);
        assert!(result.is_ok());

        // Verify pool is deleted (trust_line_count reached 0)
        let pool_after = state.get_liquidity_pool(&pool_id);
        assert!(
            pool_after.is_none(),
            "Pool should be deleted when last trustline is removed"
        );

        // Verify the pool share trustline is also gone
        let tl_asset = TrustLineAsset::PoolShare(pool_id);
        let trustline_after = state.get_trustline_by_trustline_asset(&source_id, &tl_asset);
        assert!(
            trustline_after.is_none(),
            "Pool share trustline should be deleted"
        );
    }

    /// Regression test for F20: ChangeTrust delete sponsored trustline
    ///
    /// When deleting a trustline that has a sponsor, the sponsor account's
    /// num_sponsoring counter must be decremented. This test verifies that
    /// the sponsorship is properly cleaned up.
    ///
    /// Bug discovered at testnet ledger 677219 - ChangeTrust operation failed
    /// with "source account not found" because the sponsor account wasn't
    /// loaded into state.
    #[test]
    fn test_change_trust_delete_sponsored_trustline_updates_sponsor() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(0);
        let issuer_id = create_test_account_id(1);
        let sponsor_id = create_test_account_id(2);

        // Create accounts with sufficient balance
        state.create_account(create_test_account(source_id.clone(), 100_000_000));
        state.create_account(create_test_account(issuer_id.clone(), 100_000_000));

        // Create sponsor with num_sponsoring = 1 (sponsoring the trustline)
        let mut sponsor_account = create_test_account(sponsor_id.clone(), 100_000_000);
        sponsor_account.ext = AccountEntryExt::V1(AccountEntryExtensionV1 {
            liabilities: Liabilities {
                buying: 0,
                selling: 0,
            },
            ext: AccountEntryExtensionV1Ext::V2(AccountEntryExtensionV2 {
                num_sponsored: 0,
                num_sponsoring: 1, // Sponsoring the trustline
                signer_sponsoring_i_ds: vec![].try_into().unwrap(),
                ext: AccountEntryExtensionV2Ext::V0,
            }),
        });
        state.create_account(sponsor_account);

        // Create a trustline with 0 balance (can be deleted)
        let asset = AlphaNum4 {
            asset_code: AssetCode4(*b"USD\0"),
            issuer: issuer_id,
        };
        let tl_asset = TrustLineAsset::CreditAlphanum4(asset.clone());

        state.create_trustline(create_test_trustline(
            source_id.clone(),
            tl_asset.clone(),
            0, // 0 balance - can be deleted
            1_000,
            TrustLineFlags::AuthorizedFlag as u32,
        ));

        // Set up source account with num_sub_entries and num_sponsored
        {
            let source_account = state.get_account_mut(&source_id).unwrap();
            source_account.num_sub_entries = 1;
            source_account.ext = AccountEntryExt::V1(AccountEntryExtensionV1 {
                liabilities: Liabilities {
                    buying: 0,
                    selling: 0,
                },
                ext: AccountEntryExtensionV1Ext::V2(AccountEntryExtensionV2 {
                    num_sponsored: 1, // This trustline is sponsored
                    num_sponsoring: 0,
                    signer_sponsoring_i_ds: vec![].try_into().unwrap(),
                    ext: AccountEntryExtensionV2Ext::V0,
                }),
            });
        }

        // Set up entry sponsorship for the trustline
        let ledger_key = LedgerKey::Trustline(LedgerKeyTrustLine {
            account_id: source_id.clone(),
            asset: tl_asset.clone(),
        });
        state.set_entry_sponsor(ledger_key.clone(), sponsor_id.clone());

        // Verify initial state
        assert_eq!(state.entry_sponsor(&ledger_key).as_ref(), Some(&sponsor_id));
        let sponsor_before = state.get_account(&sponsor_id).unwrap();
        let sponsor_ext = match &sponsor_before.ext {
            AccountEntryExt::V1(v1) => match &v1.ext {
                AccountEntryExtensionV1Ext::V2(v2) => v2,
                _ => panic!("expected V2 ext"),
            },
            _ => panic!("expected V1 ext"),
        };
        assert_eq!(sponsor_ext.num_sponsoring, 1);

        // Delete the trustline
        let op = ChangeTrustOp {
            line: ChangeTrustAsset::CreditAlphanum4(asset),
            limit: 0,
        };

        let result = execute_change_trust(&op, &source_id, &mut state, &context);
        assert!(result.is_ok(), "ChangeTrust delete should succeed");

        // Verify trustline is deleted
        let trustline_after = state.get_trustline_by_trustline_asset(&source_id, &tl_asset);
        assert!(trustline_after.is_none(), "Trustline should be deleted");

        // Verify sponsor's num_sponsoring was decremented
        let sponsor_after = state.get_account(&sponsor_id).unwrap();
        let sponsor_ext_after = match &sponsor_after.ext {
            AccountEntryExt::V1(v1) => match &v1.ext {
                AccountEntryExtensionV1Ext::V2(v2) => v2,
                _ => panic!("expected V2 ext"),
            },
            _ => panic!("expected V1 ext"),
        };
        assert_eq!(
            sponsor_ext_after.num_sponsoring, 0,
            "Sponsor's num_sponsoring should be decremented from 1 to 0"
        );

        // Verify source's num_sponsored was decremented
        let source_after = state.get_account(&source_id).unwrap();
        let source_ext_after = match &source_after.ext {
            AccountEntryExt::V1(v1) => match &v1.ext {
                AccountEntryExtensionV1Ext::V2(v2) => v2,
                _ => panic!("expected V2 ext"),
            },
            _ => panic!("expected V1 ext"),
        };
        assert_eq!(
            source_ext_after.num_sponsored, 0,
            "Source's num_sponsored should be decremented from 1 to 0"
        );

        // Verify sponsorship entry was removed
        assert!(
            state.entry_sponsor(&ledger_key).is_none(),
            "Entry sponsorship should be removed"
        );
    }

    /// Regression test: ChangeTrust should return OpTooManySubentries when account
    /// has reached the maximum subentries limit (1000).
    ///
    /// Bug discovered at mainnet ledger 54003784 TX 94 - the account had 1000 subentries
    /// and ChangeTrust was incorrectly allowing a new trustline to be created.
    #[test]
    fn test_change_trust_too_many_subentries() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(0);
        let issuer_id = create_test_account_id(1);

        // Create source account with max subentries (1000)
        let mut source_account = create_test_account(source_id.clone(), 100_000_000);
        source_account.num_sub_entries = 1000; // At the limit
        state.create_account(source_account);
        state.create_account(create_test_account(issuer_id.clone(), 100_000_000));

        let asset = AlphaNum4 {
            asset_code: AssetCode4(*b"USD\0"),
            issuer: issuer_id,
        };

        let op = ChangeTrustOp {
            line: ChangeTrustAsset::CreditAlphanum4(asset),
            limit: 1_000_000,
        };

        let result = execute_change_trust(&op, &source_id, &mut state, &context);
        match result.unwrap() {
            OperationResult::OpTooManySubentries => {
                // Expected - account has max subentries, can't create new trustline
            }
            other => panic!("expected OpTooManySubentries, got {:?}", other),
        }

        // Verify num_sub_entries was not changed
        assert_eq!(
            state.get_account(&source_id).unwrap().num_sub_entries,
            1000,
            "num_sub_entries should remain unchanged"
        );
    }

    /// Test ChangeTrust with source account having native selling liabilities.
    ///
    /// Selling liabilities don't affect the reserve check for creating trustlines.
    /// stellar-core uses getAvailableBalance (balance - sellingLiabilities) for the reserve check.
    /// This test verifies that selling liabilities are deducted when checking reserves,
    /// but that the trustline creation succeeds when available balance is sufficient.
    ///
    /// C++ Reference: SponsorshipUtils.cpp - canCreateEntryWithoutSponsorship uses getAvailableBalance
    #[test]
    fn test_change_trust_with_native_selling_liabilities() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(0);
        let issuer_id = create_test_account_id(1);

        // Calculate minimum balance needed for 1 trustline
        let min_balance_with_tl = state
            .minimum_balance_with_counts(context.protocol_version, 1, 0, 0)
            .unwrap();

        let selling_liabilities = 200i64;
        // Create source with selling liabilities and enough balance so that
        // available = balance - selling_liabilities >= min_balance_with_tl
        let mut source = create_test_account(
            source_id.clone(),
            min_balance_with_tl + selling_liabilities + 100,
        );
        source.ext = AccountEntryExt::V1(AccountEntryExtensionV1 {
            liabilities: Liabilities {
                buying: 0,
                selling: selling_liabilities,
            },
            ext: AccountEntryExtensionV1Ext::V0,
        });
        state.create_account(source);
        state.create_account(create_test_account(issuer_id.clone(), 100_000_000));

        let asset = AlphaNum4 {
            asset_code: AssetCode4(*b"USD\0"),
            issuer: issuer_id,
        };

        let op = ChangeTrustOp {
            line: ChangeTrustAsset::CreditAlphanum4(asset),
            limit: 1_000_000,
        };

        // ChangeTrust should succeed - available balance covers the reserve
        let result = execute_change_trust(&op, &source_id, &mut state, &context);
        match result.unwrap() {
            OperationResult::OpInner(OperationResultTr::ChangeTrust(r)) => {
                assert!(
                    matches!(r, ChangeTrustResult::Success),
                    "Expected Success when available balance covers reserve, got {:?}",
                    r
                );
            }
            other => panic!("unexpected result: {:?}", other),
        }

        // Verify trustline was created
        assert_eq!(state.get_account(&source_id).unwrap().num_sub_entries, 1);
    }

    /// Test ChangeTrust update existing trustline with issuer that was deleted.
    ///
    /// If updating an existing trustline and the issuer account no longer exists,
    /// should return NoIssuer.
    ///
    /// C++ Reference: ChangeTrustTests.cpp - "no issuer on update" test section
    #[test]
    fn test_change_trust_update_no_issuer() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(0);
        let issuer_id = create_test_account_id(1);

        state.create_account(create_test_account(source_id.clone(), 100_000_000));
        // Create issuer initially
        state.create_account(create_test_account(issuer_id.clone(), 100_000_000));

        let asset = AlphaNum4 {
            asset_code: AssetCode4(*b"USD\0"),
            issuer: issuer_id.clone(),
        };

        // Create trustline
        state.create_trustline(create_test_trustline(
            source_id.clone(),
            TrustLineAsset::CreditAlphanum4(asset.clone()),
            0,
            1_000,
            TrustLineFlags::AuthorizedFlag as u32,
        ));
        state.get_account_mut(&source_id).unwrap().num_sub_entries += 1;

        // Delete the issuer
        state.delete_account(&issuer_id);

        // Try to update the trustline
        let op = ChangeTrustOp {
            line: ChangeTrustAsset::CreditAlphanum4(asset),
            limit: 2_000,
        };

        let result = execute_change_trust(&op, &source_id, &mut state, &context);
        match result.unwrap() {
            OperationResult::OpInner(OperationResultTr::ChangeTrust(r)) => {
                assert!(
                    matches!(r, ChangeTrustResult::NoIssuer),
                    "Expected NoIssuer when issuer deleted, got {:?}",
                    r
                );
            }
            other => panic!("unexpected result: {:?}", other),
        }
    }

    /// Test ChangeTrust with negative limit returns Malformed.
    ///
    /// C++ Reference: ChangeTrustTests.cpp - "negative limit" test section
    #[test]
    fn test_change_trust_negative_limit_malformed() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(0);
        let issuer_id = create_test_account_id(1);

        state.create_account(create_test_account(source_id.clone(), 100_000_000));
        state.create_account(create_test_account(issuer_id.clone(), 100_000_000));

        let asset = AlphaNum4 {
            asset_code: AssetCode4(*b"USD\0"),
            issuer: issuer_id,
        };

        let op = ChangeTrustOp {
            line: ChangeTrustAsset::CreditAlphanum4(asset),
            limit: -1,
        };

        let result = execute_change_trust(&op, &source_id, &mut state, &context);
        match result.unwrap() {
            OperationResult::OpInner(OperationResultTr::ChangeTrust(r)) => {
                assert!(
                    matches!(r, ChangeTrustResult::Malformed),
                    "Expected Malformed for negative limit, got {:?}",
                    r
                );
            }
            other => panic!("unexpected result: {:?}", other),
        }
    }

    /// Test that pool share trustlines (which count as 2 subentries) are properly
    /// checked against the subentries limit.
    #[test]
    fn test_change_trust_pool_share_too_many_subentries() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(0);
        let issuer_a = create_test_account_id(1);
        let issuer_b = create_test_account_id(2);

        // Create source account with 999 subentries (pool share needs 2, so this should fail)
        let mut source_account = create_test_account(source_id.clone(), 100_000_000);
        source_account.num_sub_entries = 999; // One below limit, but pool share needs 2
        state.create_account(source_account);
        state.create_account(create_test_account(issuer_a.clone(), 100_000_000));
        state.create_account(create_test_account(issuer_b.clone(), 100_000_000));

        let asset_a = Asset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4(*b"USD\0"),
            issuer: issuer_a,
        });
        let asset_b = Asset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4(*b"EUR\0"),
            issuer: issuer_b,
        });

        // Create existing trustlines for the pool assets (required for pool share)
        let trustline_a = TrustLineEntry {
            account_id: source_id.clone(),
            asset: TrustLineAsset::CreditAlphanum4(match &asset_a {
                Asset::CreditAlphanum4(a) => a.clone(),
                _ => unreachable!(),
            }),
            balance: 0,
            limit: 10_000,
            flags: TrustLineFlags::AuthorizedFlag as u32,
            ext: TrustLineEntryExt::V0,
        };
        let trustline_b = TrustLineEntry {
            account_id: source_id.clone(),
            asset: TrustLineAsset::CreditAlphanum4(match &asset_b {
                Asset::CreditAlphanum4(a) => a.clone(),
                _ => unreachable!(),
            }),
            balance: 0,
            limit: 10_000,
            flags: TrustLineFlags::AuthorizedFlag as u32,
            ext: TrustLineEntryExt::V0,
        };
        state.create_trustline(trustline_a);
        state.create_trustline(trustline_b);

        let params = LiquidityPoolParameters::LiquidityPoolConstantProduct(
            LiquidityPoolConstantProductParameters {
                asset_a,
                asset_b,
                fee: LIQUIDITY_POOL_FEE_V18,
            },
        );

        let op = ChangeTrustOp {
            line: ChangeTrustAsset::PoolShare(params),
            limit: 1_000,
        };

        let result = execute_change_trust(&op, &source_id, &mut state, &context);
        match result.unwrap() {
            OperationResult::OpTooManySubentries => {
                // Expected - 999 + 2 = 1001 > 1000 limit
            }
            other => panic!("expected OpTooManySubentries, got {:?}", other),
        }
    }

    /// Regression test: ChangeTrust LowReserve check must deduct selling liabilities.
    ///
    /// Before the fix, the reserve check compared raw balance against min_balance,
    /// ignoring selling liabilities. This allowed accounts with high selling liabilities
    /// to create new trustlines even when their available balance (balance - liabilities)
    /// was below the reserve requirement.
    ///
    /// Found at mainnet ledger 59501777: a ChangeTrust succeeded in our code but stellar-core
    /// correctly returned LowReserve because getAvailableBalance deducts selling liabilities.
    #[test]
    fn test_change_trust_low_reserve_with_selling_liabilities() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(0);
        let issuer_id = create_test_account_id(1);

        // Compute reserve requirements:
        // min_balance for 0 subentries = base_reserve * 2 = 10_000_000
        // Adding a trustline needs 1 extra subentry, so new_min_balance = base_reserve * 3 = 15_000_000
        let new_min_balance = state
            .minimum_balance_with_counts(context.protocol_version, 1, 0, 0)
            .unwrap();

        let selling_liabilities = 1_000_000i64;
        // Set balance so raw balance >= new_min_balance, but available < new_min_balance:
        //   balance = new_min_balance + selling_liabilities - 1
        //   available = balance - selling_liabilities = new_min_balance - 1 < new_min_balance
        let balance = new_min_balance + selling_liabilities - 1;
        assert!(
            balance >= new_min_balance,
            "raw balance must be >= new_min_balance"
        );
        assert!(
            balance - selling_liabilities < new_min_balance,
            "available balance must be < new_min_balance"
        );

        let mut source_account = create_test_account(source_id.clone(), balance);
        source_account.ext = AccountEntryExt::V1(AccountEntryExtensionV1 {
            liabilities: Liabilities {
                buying: 0,
                selling: selling_liabilities,
            },
            ext: AccountEntryExtensionV1Ext::V0,
        });
        state.create_account(source_account);
        state.create_account(create_test_account(issuer_id.clone(), 100_000_000));

        let op = ChangeTrustOp {
            line: ChangeTrustAsset::CreditAlphanum4(AlphaNum4 {
                asset_code: AssetCode4(*b"USD\0"),
                issuer: issuer_id.clone(),
            }),
            limit: 1_000,
        };

        let result = execute_change_trust(&op, &source_id, &mut state, &context).unwrap();
        match result {
            OperationResult::OpInner(OperationResultTr::ChangeTrust(r)) => {
                assert!(
                    matches!(r, ChangeTrustResult::LowReserve),
                    "Expected LowReserve when selling liabilities reduce available balance below reserve, got {:?}",
                    r
                );
            }
            other => panic!("Unexpected result type: {:?}", other),
        }

        // Verify that without selling liabilities, the same balance succeeds
        let source_id2 = create_test_account_id(2);
        let mut source_account2 = create_test_account(source_id2.clone(), balance);
        source_account2.ext = AccountEntryExt::V1(AccountEntryExtensionV1 {
            liabilities: Liabilities {
                buying: 0,
                selling: 0,
            },
            ext: AccountEntryExtensionV1Ext::V0,
        });
        state.create_account(source_account2);

        let op2 = ChangeTrustOp {
            line: ChangeTrustAsset::CreditAlphanum4(AlphaNum4 {
                asset_code: AssetCode4(*b"USD\0"),
                issuer: issuer_id.clone(),
            }),
            limit: 1_000,
        };

        let result2 = execute_change_trust(&op2, &source_id2, &mut state, &context).unwrap();
        match result2 {
            OperationResult::OpInner(OperationResultTr::ChangeTrust(
                ChangeTrustResult::Success,
            )) => {
                // Expected: without liabilities, same balance is sufficient
            }
            other => panic!(
                "Expected Success without selling liabilities, got {:?}",
                other
            ),
        }
    }

    /// Regression test: When both NoIssuer and TooManySubentries conditions apply,
    /// stellar-core returns NoIssuer because it checks issuer existence before the
    /// subentry limit (via createEntryWithPossibleSponsorship).
    #[test]
    fn test_change_trust_no_issuer_before_too_many_subentries() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(0);
        let issuer_id = create_test_account_id(1);

        // Source account at max subentries AND issuer doesn't exist
        let mut source_account = create_test_account(source_id.clone(), 100_000_000);
        source_account.num_sub_entries = 1000;
        state.create_account(source_account);
        // Do NOT create issuer account

        let asset = AlphaNum4 {
            asset_code: AssetCode4(*b"USD\0"),
            issuer: issuer_id,
        };

        let op = ChangeTrustOp {
            line: ChangeTrustAsset::CreditAlphanum4(asset),
            limit: 1_000_000,
        };

        let result = execute_change_trust(&op, &source_id, &mut state, &context);
        match result.unwrap() {
            OperationResult::OpInner(OperationResultTr::ChangeTrust(r)) => {
                assert!(
                    matches!(r, ChangeTrustResult::NoIssuer),
                    "Expected NoIssuer (checked before TooManySubentries), got {:?}",
                    r
                );
            }
            other => panic!("Expected ChangeTrust(NoIssuer), got {:?}", other),
        }
    }
}
