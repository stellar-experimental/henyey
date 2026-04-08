//! Liquidity Pool operation execution.
//!
//! This module implements the execution logic for:
//! - LiquidityPoolDeposit
//! - LiquidityPoolWithdraw

use stellar_xdr::curr::{
    AccountId, Asset, LiquidityPoolDepositOp, LiquidityPoolDepositResult,
    LiquidityPoolDepositResultCode, LiquidityPoolWithdrawOp, LiquidityPoolWithdrawResult,
    LiquidityPoolWithdrawResultCode, OperationResult, OperationResultTr, Price, TrustLineAsset,
};

use super::{
    account_liabilities, add_account_balance, add_pool_reserve, add_pool_shares,
    add_trustline_balance, is_authorized_to_maintain_liabilities, is_trustline_authorized,
    sub_account_balance, sub_trustline_balance, trustline_balance_after_liabilities,
    trustline_liabilities,
};
use crate::state::LedgerStateManager;
use crate::validation::LedgerContext;
use crate::{Result, TxError};

/// Execute a LiquidityPoolDeposit operation.
///
/// This operation deposits assets into a liquidity pool in exchange for
/// pool shares.
pub(crate) fn execute_liquidity_pool_deposit(
    op: &LiquidityPoolDepositOp,
    source: &AccountId,
    state: &mut LedgerStateManager,
    context: &LedgerContext,
) -> Result<OperationResult> {
    // Validate amounts
    if op.max_amount_a <= 0 || op.max_amount_b <= 0 {
        return Ok(make_deposit_result(
            LiquidityPoolDepositResultCode::Malformed,
        ));
    }

    // Check min/max price bounds
    if op.min_price.n <= 0 || op.min_price.d <= 0 || op.max_price.n <= 0 || op.max_price.d <= 0 {
        return Ok(make_deposit_result(
            LiquidityPoolDepositResultCode::Malformed,
        ));
    }

    // minPrice must not exceed maxPrice
    if (op.min_price.n as i128) * (op.max_price.d as i128)
        > (op.min_price.d as i128) * (op.max_price.n as i128)
    {
        return Ok(make_deposit_result(
            LiquidityPoolDepositResultCode::Malformed,
        ));
    }

    // Get the liquidity pool
    let pool = match state.get_liquidity_pool(&op.liquidity_pool_id) {
        Some(p) => p.clone(),
        None => {
            return Ok(make_deposit_result(LiquidityPoolDepositResultCode::NoTrust));
        }
    };

    // Get pool parameters
    let (asset_a, asset_b, reserve_a, reserve_b, total_shares, _fee) = match &pool.body {
        stellar_xdr::curr::LiquidityPoolEntryBody::LiquidityPoolConstantProduct(cp) => {
            let params = &cp.params;
            (
                params.asset_a.clone(),
                params.asset_b.clone(),
                cp.reserve_a,
                cp.reserve_b,
                cp.total_pool_shares,
                params.fee,
            )
        }
    };

    // Check source has pool share trustline
    let pool_share_asset = TrustLineAsset::PoolShare(op.liquidity_pool_id.clone());
    let pool_share_trustline =
        match state.get_trustline_by_trustline_asset(source, &pool_share_asset) {
            Some(tl) => tl,
            None => {
                return Ok(make_deposit_result(LiquidityPoolDepositResultCode::NoTrust));
            }
        };

    // Check source has trustlines, authorization, and available balance for both assets.
    let available_a = match resolve_deposit_asset(source, &asset_a, state, context) {
        Ok(v) => v,
        Err(code) => return Ok(make_deposit_result(code)),
    };
    let available_b = match resolve_deposit_asset(source, &asset_b, state, context) {
        Ok(v) => v,
        Err(code) => return Ok(make_deposit_result(code)),
    };

    // CAP-77: Check if any relevant trustlines are frozen (apply-time check).
    if context.frozen_key_config.has_frozen_keys() {
        if !matches!(asset_a, Asset::Native)
            && context
                .frozen_key_config
                .is_key_frozen(&crate::frozen_keys::trustline_key(source, &asset_a))
        {
            return Ok(make_deposit_result(
                LiquidityPoolDepositResultCode::TrustlineFrozen,
            ));
        }
        if !matches!(asset_b, Asset::Native)
            && context
                .frozen_key_config
                .is_key_frozen(&crate::frozen_keys::trustline_key(source, &asset_b))
        {
            return Ok(make_deposit_result(
                LiquidityPoolDepositResultCode::TrustlineFrozen,
            ));
        }
    }

    let available_pool_share_limit = pool_share_trustline
        .limit
        .saturating_sub(pool_share_trustline.balance);

    let (deposit_a, deposit_b, shares_received) = if total_shares == 0 {
        match deposit_into_empty_pool(
            op.max_amount_a,
            op.max_amount_b,
            available_a,
            available_b,
            available_pool_share_limit,
            &op.min_price,
            &op.max_price,
        )? {
            DepositOutcome::Success { a, b, shares } => (a, b, shares),
            DepositOutcome::Underfunded => {
                return Ok(make_deposit_result(
                    LiquidityPoolDepositResultCode::Underfunded,
                ));
            }
            DepositOutcome::BadPrice => {
                return Ok(make_deposit_result(
                    LiquidityPoolDepositResultCode::BadPrice,
                ));
            }
            DepositOutcome::LineFull => {
                return Ok(make_deposit_result(
                    LiquidityPoolDepositResultCode::LineFull,
                ));
            }
        }
    } else {
        match deposit_into_non_empty_pool(NonEmptyPoolDepositRequest {
            max_amount_a: op.max_amount_a,
            max_amount_b: op.max_amount_b,
            available_a,
            available_b,
            available_pool_share_limit,
            reserve_a,
            reserve_b,
            total_shares,
            min_price: &op.min_price,
            max_price: &op.max_price,
        })? {
            DepositOutcome::Success { a, b, shares } => (a, b, shares),
            DepositOutcome::Underfunded => {
                return Ok(make_deposit_result(
                    LiquidityPoolDepositResultCode::Underfunded,
                ));
            }
            DepositOutcome::BadPrice => {
                return Ok(make_deposit_result(
                    LiquidityPoolDepositResultCode::BadPrice,
                ));
            }
            DepositOutcome::LineFull => {
                return Ok(make_deposit_result(
                    LiquidityPoolDepositResultCode::LineFull,
                ));
            }
        }
    };

    if shares_received < 0 {
        let pool_id_prefix = &op.liquidity_pool_id.0 .0[0..4];
        tracing::warn!(
            pool_id_prefix = ?pool_id_prefix,
            total_shares,
            reserve_a,
            reserve_b,
            deposit_a,
            deposit_b,
            shares_received,
            "Computed negative pool shares for liquidity pool deposit"
        );
    }

    if i64::MAX - reserve_a < deposit_a
        || i64::MAX - reserve_b < deposit_b
        || i64::MAX - total_shares < shares_received
    {
        return Ok(make_deposit_result(
            LiquidityPoolDepositResultCode::PoolFull,
        ));
    }

    // Deduct assets from source
    // Note: issuers can deposit their own assets without a trustline (they create from nothing)
    if let Err(code) = debit_asset(state, source, &asset_a, deposit_a) {
        return Ok(make_deposit_result(code));
    }
    if let Err(code) = debit_asset(state, source, &asset_b, deposit_b) {
        return Ok(make_deposit_result(code));
    }

    // Credit pool shares to source
    if let Some(tl) = state.get_trustline_by_trustline_asset_mut(source, &pool_share_asset) {
        add_trustline_balance(tl, shares_received)
            .map_err(|_| TxError::Internal("pool share credit overflow".into()))?;
    }

    // Update pool reserves
    if let Some(pool_mut) = state.get_liquidity_pool_mut(&op.liquidity_pool_id) {
        match &mut pool_mut.body {
            stellar_xdr::curr::LiquidityPoolEntryBody::LiquidityPoolConstantProduct(cp) => {
                add_pool_reserve(&mut cp.reserve_a, deposit_a)?;
                add_pool_reserve(&mut cp.reserve_b, deposit_b)?;
                add_pool_shares(&mut cp.total_pool_shares, shares_received)?;
            }
        }
    }

    Ok(make_deposit_result(LiquidityPoolDepositResultCode::Success))
}

/// Execute a LiquidityPoolWithdraw operation.
///
/// This operation withdraws assets from a liquidity pool by redeeming
/// pool shares.
pub(crate) fn execute_liquidity_pool_withdraw(
    op: &LiquidityPoolWithdrawOp,
    source: &AccountId,
    state: &mut LedgerStateManager,
    context: &LedgerContext,
) -> Result<OperationResult> {
    // Validate amounts
    if op.amount <= 0 {
        return Ok(make_withdraw_result(
            LiquidityPoolWithdrawResultCode::Malformed,
        ));
    }

    if op.min_amount_a < 0 || op.min_amount_b < 0 {
        return Ok(make_withdraw_result(
            LiquidityPoolWithdrawResultCode::Malformed,
        ));
    }

    // Get the liquidity pool
    let pool = match state.get_liquidity_pool(&op.liquidity_pool_id) {
        Some(p) => p.clone(),
        None => {
            return Ok(make_withdraw_result(
                LiquidityPoolWithdrawResultCode::NoTrust,
            ));
        }
    };

    // Get pool parameters
    let (asset_a, asset_b, reserve_a, reserve_b, total_shares) = match &pool.body {
        stellar_xdr::curr::LiquidityPoolEntryBody::LiquidityPoolConstantProduct(cp) => {
            let params = &cp.params;
            (
                params.asset_a.clone(),
                params.asset_b.clone(),
                cp.reserve_a,
                cp.reserve_b,
                cp.total_pool_shares,
            )
        }
    };

    // Check source has pool share trustline with sufficient available balance.
    // C++ uses getAvailableBalance which subtracts selling liabilities.
    let pool_share_asset = TrustLineAsset::PoolShare(op.liquidity_pool_id.clone());
    let (_shares_balance, shares_available) =
        match state.get_trustline_by_trustline_asset(source, &pool_share_asset) {
            Some(tl) => {
                let selling = trustline_liabilities(tl).selling;
                (tl.balance, tl.balance - selling)
            }
            None => {
                return Ok(make_withdraw_result(
                    LiquidityPoolWithdrawResultCode::NoTrust,
                ));
            }
        };

    if shares_available < op.amount {
        return Ok(make_withdraw_result(
            LiquidityPoolWithdrawResultCode::Underfunded,
        ));
    }

    // CAP-77: Check if any relevant trustlines are frozen (apply-time check).
    if context.frozen_key_config.has_frozen_keys() {
        if !matches!(asset_a, Asset::Native)
            && context
                .frozen_key_config
                .is_key_frozen(&crate::frozen_keys::trustline_key(source, &asset_a))
        {
            return Ok(make_withdraw_result(
                LiquidityPoolWithdrawResultCode::TrustlineFrozen,
            ));
        }
        if !matches!(asset_b, Asset::Native)
            && context
                .frozen_key_config
                .is_key_frozen(&crate::frozen_keys::trustline_key(source, &asset_b))
        {
            return Ok(make_withdraw_result(
                LiquidityPoolWithdrawResultCode::TrustlineFrozen,
            ));
        }
    }

    let withdraw_a = get_pool_withdrawal_amount(op.amount, total_shares, reserve_a);
    let withdraw_b = get_pool_withdrawal_amount(op.amount, total_shares, reserve_b);

    if withdraw_a < op.min_amount_a || withdraw_b < op.min_amount_b {
        return Ok(make_withdraw_result(
            LiquidityPoolWithdrawResultCode::UnderMinimum,
        ));
    }

    for (asset, amount) in [(&asset_a, withdraw_a), (&asset_b, withdraw_b)] {
        match can_credit_asset(state, source, asset, amount) {
            WithdrawAssetCheck::Ok => {}
            WithdrawAssetCheck::NoTrust => {
                return Ok(make_withdraw_result(
                    LiquidityPoolWithdrawResultCode::NoTrust,
                ));
            }
            WithdrawAssetCheck::LineFull => {
                return Ok(make_withdraw_result(
                    LiquidityPoolWithdrawResultCode::LineFull,
                ));
            }
        }
    }

    credit_asset(state, source, &asset_a, withdraw_a);
    credit_asset(state, source, &asset_b, withdraw_b);

    // Deduct pool shares from source
    if let Some(tl) = state.get_trustline_by_trustline_asset_mut(source, &pool_share_asset) {
        sub_trustline_balance(tl, op.amount)?;
    }

    // Update pool reserves
    if let Some(pool_mut) = state.get_liquidity_pool_mut(&op.liquidity_pool_id) {
        match &mut pool_mut.body {
            stellar_xdr::curr::LiquidityPoolEntryBody::LiquidityPoolConstantProduct(cp) => {
                add_pool_reserve(&mut cp.reserve_a, -withdraw_a)?;
                add_pool_reserve(&mut cp.reserve_b, -withdraw_b)?;
                add_pool_shares(&mut cp.total_pool_shares, -op.amount)?;
            }
        }
    }

    Ok(make_withdraw_result(
        LiquidityPoolWithdrawResultCode::Success,
    ))
}

/// Resolve a single asset's trustline and available balance for a deposit.
///
/// Looks up the trustline (unless native or issuer), checks authorization,
/// and computes available balance. Returns `Err(code)` for early-exit conditions.
fn resolve_deposit_asset(
    source: &AccountId,
    asset: &Asset,
    state: &LedgerStateManager,
    context: &LedgerContext,
) -> std::result::Result<i64, LiquidityPoolDepositResultCode> {
    use stellar_xdr::curr::TrustLineEntry;

    let trustline: Option<&TrustLineEntry> =
        if matches!(asset, Asset::Native) || is_issuer(source, asset) {
            None
        } else {
            match state.get_trustline(source, asset) {
                Some(tl) => Some(tl),
                None => return Err(LiquidityPoolDepositResultCode::NoTrust),
            }
        };

    // Check trustline authorization unconditionally, matching stellar-core's
    // LiquidityPoolDepositOpFrame.cpp:269 which checks tlA.isAuthorized()
    // on the trustline's own flags without gating on the issuer's account flags.
    if trustline
        .map(|tl| !is_trustline_authorized(tl.flags))
        .unwrap_or(false)
    {
        return Err(LiquidityPoolDepositResultCode::NotAuthorized);
    }

    let available = match asset {
        Asset::Native => available_native_balance(source, state, context)
            .map_err(|_| LiquidityPoolDepositResultCode::Underfunded)?,
        _ if is_issuer(source, asset) => i64::MAX,
        _ => trustline
            .map(trustline_balance_after_liabilities)
            .unwrap_or(0),
    };

    Ok(available)
}

/// Debit an asset from the source account for a liquidity pool deposit.
///
/// Handles native (account balance), issuer (no-op), and trustline cases.
/// Returns `Err(Underfunded)` if the balance is insufficient.
fn debit_asset(
    state: &mut LedgerStateManager,
    source: &AccountId,
    asset: &Asset,
    amount: i64,
) -> std::result::Result<(), LiquidityPoolDepositResultCode> {
    if matches!(asset, Asset::Native) {
        if let Some(account) = state.get_account_mut(source) {
            if account.balance < amount {
                return Err(LiquidityPoolDepositResultCode::Underfunded);
            }
            sub_account_balance(account, amount)
                .map_err(|_| LiquidityPoolDepositResultCode::Underfunded)?;
        }
    } else if is_issuer(source, asset) {
        // Issuer "creates" assets out of nothing, no balance to deduct
    } else if let Some(tl) = state.get_trustline_mut(source, asset) {
        if tl.balance < amount {
            return Err(LiquidityPoolDepositResultCode::Underfunded);
        }
        sub_trustline_balance(tl, amount)
            .map_err(|_| LiquidityPoolDepositResultCode::Underfunded)?;
    }
    Ok(())
}

fn available_native_balance(
    source: &AccountId,
    state: &LedgerStateManager,
    context: &LedgerContext,
) -> Result<i64> {
    let Some(account) = state.get_account(source) else {
        return Ok(0);
    };
    let min_balance = state.minimum_balance_for_account(account, context.protocol_version, 0)?;
    let selling_liab = account_liabilities(account).selling;
    Ok(account
        .balance
        .saturating_sub(min_balance)
        .saturating_sub(selling_liab))
}

fn is_bad_price(amount_a: i64, amount_b: i64, min_price: &Price, max_price: &Price) -> bool {
    if amount_a == 0 || amount_b == 0 {
        return true;
    }

    let amount_a = amount_a as i128;
    let amount_b = amount_b as i128;
    let min_n = min_price.n as i128;
    let min_d = min_price.d as i128;
    let max_n = max_price.n as i128;
    let max_d = max_price.d as i128;

    amount_a * min_d < amount_b * min_n || amount_a * max_d > amount_b * max_n
}

#[derive(Debug)]
enum DepositOutcome {
    Success { a: i64, b: i64, shares: i64 },
    Underfunded,
    BadPrice,
    LineFull,
}

struct NonEmptyPoolDepositRequest<'a> {
    max_amount_a: i64,
    max_amount_b: i64,
    available_a: i64,
    available_b: i64,
    available_pool_share_limit: i64,
    reserve_a: i64,
    reserve_b: i64,
    total_shares: i64,
    min_price: &'a Price,
    max_price: &'a Price,
}

fn deposit_into_empty_pool(
    max_amount_a: i64,
    max_amount_b: i64,
    available_a: i64,
    available_b: i64,
    available_pool_share_limit: i64,
    min_price: &Price,
    max_price: &Price,
) -> Result<DepositOutcome> {
    if available_a < max_amount_a || available_b < max_amount_b {
        return Ok(DepositOutcome::Underfunded);
    }

    if is_bad_price(max_amount_a, max_amount_b, min_price, max_price) {
        return Ok(DepositOutcome::BadPrice);
    }

    let shares = big_square_root(max_amount_a, max_amount_b);
    if available_pool_share_limit < shares {
        return Ok(DepositOutcome::LineFull);
    }

    Ok(DepositOutcome::Success {
        a: max_amount_a,
        b: max_amount_b,
        shares,
    })
}

fn deposit_into_non_empty_pool(request: NonEmptyPoolDepositRequest<'_>) -> Result<DepositOutcome> {
    let NonEmptyPoolDepositRequest {
        max_amount_a,
        max_amount_b,
        available_a,
        available_b,
        available_pool_share_limit,
        reserve_a,
        reserve_b,
        total_shares,
        min_price,
        max_price,
    } = request;
    let shares_a = big_divide_checked(total_shares, max_amount_a, reserve_a, Round::Down);
    let shares_b = big_divide_checked(total_shares, max_amount_b, reserve_b, Round::Down);

    // minAmongValid: if one overflows, use the other; if both overflow, fail.
    // Matches stellar-core LiquidityPoolDepositOpFrame.cpp:78-98.
    let pool_shares = match (shares_a, shares_b) {
        (Some(a), Some(b)) => a.min(b),
        (Some(a), None) => a,
        (None, Some(b)) => b,
        (None, None) => {
            // INVARIANT: pool state validated at deposit entry; reserves are always positive here
            // This can't happen in practice (see C++ comment: it is guaranteed
            // that either reserveA >= totalPoolShares or reserveB >= totalPoolShares),
            // but we handle it the same way C++ does: throw / panic.
            panic!("both shares calculations overflowed");
        }
    };

    let amount_a = big_divide(pool_shares, reserve_a, total_shares, Round::Up)?;
    let amount_b = big_divide(pool_shares, reserve_b, total_shares, Round::Up)?;

    if available_a < amount_a || available_b < amount_b {
        return Ok(DepositOutcome::Underfunded);
    }

    if is_bad_price(amount_a, amount_b, min_price, max_price) {
        return Ok(DepositOutcome::BadPrice);
    }

    if available_pool_share_limit < pool_shares {
        return Ok(DepositOutcome::LineFull);
    }

    Ok(DepositOutcome::Success {
        a: amount_a,
        b: amount_b,
        shares: pool_shares,
    })
}

fn get_pool_withdrawal_amount(amount: i64, total_shares: i64, reserve: i64) -> i64 {
    big_divide(amount, reserve, total_shares, Round::Down).unwrap_or(0)
}

enum WithdrawAssetCheck {
    Ok,
    NoTrust,
    LineFull,
}

fn can_credit_asset(
    state: &LedgerStateManager,
    source: &AccountId,
    asset: &Asset,
    amount: i64,
) -> WithdrawAssetCheck {
    if matches!(asset, Asset::Native) {
        let Some(account) = state.get_account(source) else {
            return WithdrawAssetCheck::NoTrust;
        };
        // Overflow-safe: i64::MAX - balance < amount
        if i64::MAX - account.balance < amount {
            return WithdrawAssetCheck::LineFull;
        }
        let new_balance = account.balance + amount;
        // Buying liabilities: new_balance > i64::MAX - buying
        if new_balance > i64::MAX - account_liabilities(account).buying {
            return WithdrawAssetCheck::LineFull;
        }
        return WithdrawAssetCheck::Ok;
    }

    // Issuers don't need trustlines for their own assets - they can always hold them
    // with effectively unlimited capacity
    if is_issuer(source, asset) {
        return WithdrawAssetCheck::Ok;
    }

    let Some(tl) = state.get_trustline(source, asset) else {
        return WithdrawAssetCheck::NoTrust;
    };
    if !is_authorized_to_maintain_liabilities(tl.flags) {
        return WithdrawAssetCheck::LineFull;
    }
    // Overflow-safe: limit - balance < amount
    if tl.limit - tl.balance < amount {
        return WithdrawAssetCheck::LineFull;
    }
    let new_balance = tl.balance + amount;
    // Buying liabilities: new_balance > limit - buying
    if new_balance > tl.limit - trustline_liabilities(tl).buying {
        return WithdrawAssetCheck::LineFull;
    }
    WithdrawAssetCheck::Ok
}

fn credit_asset(state: &mut LedgerStateManager, source: &AccountId, asset: &Asset, amount: i64) {
    if matches!(asset, Asset::Native) {
        if let Some(account) = state.get_account_mut(source) {
            // stellar-core: LiquidityPoolWithdrawOpFrame.cpp — ignores addBalance return
            let _ = add_account_balance(account, amount);
        }
        return;
    }

    // Issuers don't track balance for their own assets - credits are essentially "destroyed"
    if is_issuer(source, asset) {
        return;
    }

    if let Some(tl) = state.get_trustline_mut(source, asset) {
        // stellar-core: LiquidityPoolWithdrawOpFrame.cpp — ignores addBalance return
        let _ = add_trustline_balance(tl, amount);
    }
}

#[derive(Clone, Copy)]
enum Round {
    Down,
    Up,
}

/// stellar-core: numeric.cpp:19 — asserts C > 0, aborts on zero/negative denominator.
fn big_divide(a: i64, b: i64, c: i64, round: Round) -> Result<i64> {
    assert!(c > 0, "big_divide: denominator must be positive (got {c})");
    let numerator = (a as i128) * (b as i128);
    let denominator = c as i128;
    let result = match round {
        Round::Down => numerator / denominator,
        Round::Up => {
            if numerator == 0 {
                0
            } else {
                (numerator + denominator - 1) / denominator
            }
        }
    };
    if result > i64::MAX as i128 {
        return Ok(0);
    }
    Ok(result as i64)
}

/// Checked variant of big_divide that returns None on overflow.
/// stellar-core: numeric.cpp — bigDivide returns false on overflow, asserts C > 0.
fn big_divide_checked(a: i64, b: i64, c: i64, round: Round) -> Option<i64> {
    assert!(
        c > 0,
        "big_divide_checked: denominator must be positive (got {c})"
    );
    let numerator = (a as i128) * (b as i128);
    let denominator = c as i128;
    let result = match round {
        Round::Down => numerator / denominator,
        Round::Up => {
            if numerator == 0 {
                0
            } else {
                (numerator + denominator - 1) / denominator
            }
        }
    };
    if result > i64::MAX as i128 {
        return None;
    }
    Some(result as i64)
}

/// Check if an account is the issuer of an asset.
/// The issuer doesn't need a trustline to hold their own asset.
fn is_issuer(account: &AccountId, asset: &Asset) -> bool {
    match asset {
        Asset::Native => false,
        Asset::CreditAlphanum4(a) => &a.issuer == account,
        Asset::CreditAlphanum12(a) => &a.issuer == account,
    }
}

fn big_square_root(a: i64, b: i64) -> i64 {
    let product = (a as i128) * (b as i128);
    let mut low: i128 = 0;
    let mut high: i128 = product.min(i64::MAX as i128);
    while low <= high {
        let mid = (low + high) / 2;
        let sq = mid * mid;
        if sq == product {
            return mid as i64;
        }
        if sq < product {
            low = mid + 1;
        } else {
            high = mid - 1;
        }
    }
    high.max(0) as i64
}

/// Create a LiquidityPoolDeposit result.
fn make_deposit_result(code: LiquidityPoolDepositResultCode) -> OperationResult {
    let result = match code {
        LiquidityPoolDepositResultCode::Success => LiquidityPoolDepositResult::Success,
        LiquidityPoolDepositResultCode::Malformed => LiquidityPoolDepositResult::Malformed,
        LiquidityPoolDepositResultCode::NoTrust => LiquidityPoolDepositResult::NoTrust,
        LiquidityPoolDepositResultCode::NotAuthorized => LiquidityPoolDepositResult::NotAuthorized,
        LiquidityPoolDepositResultCode::Underfunded => LiquidityPoolDepositResult::Underfunded,
        LiquidityPoolDepositResultCode::LineFull => LiquidityPoolDepositResult::LineFull,
        LiquidityPoolDepositResultCode::BadPrice => LiquidityPoolDepositResult::BadPrice,
        LiquidityPoolDepositResultCode::PoolFull => LiquidityPoolDepositResult::PoolFull,
        LiquidityPoolDepositResultCode::TrustlineFrozen => {
            LiquidityPoolDepositResult::TrustlineFrozen
        }
    };

    OperationResult::OpInner(OperationResultTr::LiquidityPoolDeposit(result))
}

/// Create a LiquidityPoolWithdraw result.
fn make_withdraw_result(code: LiquidityPoolWithdrawResultCode) -> OperationResult {
    let result = match code {
        LiquidityPoolWithdrawResultCode::Success => LiquidityPoolWithdrawResult::Success,
        LiquidityPoolWithdrawResultCode::Malformed => LiquidityPoolWithdrawResult::Malformed,
        LiquidityPoolWithdrawResultCode::NoTrust => LiquidityPoolWithdrawResult::NoTrust,
        LiquidityPoolWithdrawResultCode::Underfunded => LiquidityPoolWithdrawResult::Underfunded,
        LiquidityPoolWithdrawResultCode::LineFull => LiquidityPoolWithdrawResult::LineFull,
        LiquidityPoolWithdrawResultCode::UnderMinimum => LiquidityPoolWithdrawResult::UnderMinimum,
        LiquidityPoolWithdrawResultCode::TrustlineFrozen => {
            LiquidityPoolWithdrawResult::TrustlineFrozen
        }
    };

    OperationResult::OpInner(OperationResultTr::LiquidityPoolWithdraw(result))
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

    fn create_test_context() -> LedgerContext {
        LedgerContext::testnet(1, 1000)
    }

    fn create_pool_entry(
        pool_id: PoolId,
        asset_a: Asset,
        asset_b: Asset,
        reserve_a: i64,
        reserve_b: i64,
        total_shares: i64,
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
                    pool_shares_trust_line_count: 1,
                },
            ),
        }
    }

    #[test]
    fn test_liquidity_pool_deposit_no_pool() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(0);

        let op = LiquidityPoolDepositOp {
            liquidity_pool_id: PoolId(Hash([0u8; 32])),
            max_amount_a: 1000,
            max_amount_b: 1000,
            min_price: Price { n: 1, d: 2 },
            max_price: Price { n: 2, d: 1 },
        };

        let result = execute_liquidity_pool_deposit(&op, &source_id, &mut state, &context);
        assert!(result.is_ok());

        match result.unwrap() {
            OperationResult::OpInner(OperationResultTr::LiquidityPoolDeposit(r)) => {
                assert!(matches!(r, LiquidityPoolDepositResult::NoTrust));
            }
            _ => panic!("Unexpected result type"),
        }
    }

    #[test]
    fn test_liquidity_pool_withdraw_no_pool() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(0);

        let op = LiquidityPoolWithdrawOp {
            liquidity_pool_id: PoolId(Hash([0u8; 32])),
            amount: 100,
            min_amount_a: 0,
            min_amount_b: 0,
        };

        let result = execute_liquidity_pool_withdraw(&op, &source_id, &mut state, &context);
        assert!(result.is_ok());

        match result.unwrap() {
            OperationResult::OpInner(OperationResultTr::LiquidityPoolWithdraw(r)) => {
                assert!(matches!(r, LiquidityPoolWithdrawResult::NoTrust));
            }
            _ => panic!("Unexpected result type"),
        }
    }

    #[test]
    fn test_liquidity_pool_deposit_not_authorized() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(0);
        let issuer_a = create_test_account_id(1);
        let issuer_b = create_test_account_id(2);
        state.create_account(create_test_account(source_id.clone(), 100_000_000, 0));
        state.create_account(create_test_account(
            issuer_a.clone(),
            100_000_000,
            AccountFlags::RequiredFlag as u32,
        ));
        state.create_account(create_test_account(issuer_b.clone(), 100_000_000, 0));

        let asset_a = Asset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4(*b"USD\0"),
            issuer: issuer_a,
        });
        let asset_b = Asset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4(*b"EUR\0"),
            issuer: issuer_b,
        });
        let pool_id = PoolId(Hash([1u8; 32]));
        state.create_liquidity_pool(create_pool_entry(
            pool_id.clone(),
            asset_a.clone(),
            asset_b.clone(),
            0,
            0,
            0,
        ));

        let trustline_a = TrustLineEntry {
            account_id: source_id.clone(),
            asset: TrustLineAsset::CreditAlphanum4(match &asset_a {
                Asset::CreditAlphanum4(a) => a.clone(),
                _ => unreachable!(),
            }),
            balance: 1_000,
            limit: 10_000,
            flags: 0,
            ext: TrustLineEntryExt::V0,
        };
        let trustline_b = TrustLineEntry {
            account_id: source_id.clone(),
            asset: TrustLineAsset::CreditAlphanum4(match &asset_b {
                Asset::CreditAlphanum4(a) => a.clone(),
                _ => unreachable!(),
            }),
            balance: 1_000,
            limit: 10_000,
            flags: TrustLineFlags::AuthorizedFlag as u32,
            ext: TrustLineEntryExt::V0,
        };
        let pool_share_tl = TrustLineEntry {
            account_id: source_id.clone(),
            asset: TrustLineAsset::PoolShare(pool_id.clone()),
            balance: 0,
            limit: 10_000,
            flags: 0,
            ext: TrustLineEntryExt::V0,
        };
        state.create_trustline(trustline_a);
        state.create_trustline(trustline_b);
        state.create_trustline(pool_share_tl);
        state.get_account_mut(&source_id).unwrap().num_sub_entries += 3;

        let op = LiquidityPoolDepositOp {
            liquidity_pool_id: pool_id,
            max_amount_a: 100,
            max_amount_b: 100,
            min_price: Price { n: 1, d: 1 },
            max_price: Price { n: 1, d: 1 },
        };

        let result = execute_liquidity_pool_deposit(&op, &source_id, &mut state, &context);
        match result.unwrap() {
            OperationResult::OpInner(OperationResultTr::LiquidityPoolDeposit(r)) => {
                assert!(matches!(r, LiquidityPoolDepositResult::NotAuthorized));
            }
            other => panic!("unexpected result: {:?}", other),
        }
    }

    #[test]
    fn test_liquidity_pool_deposit_line_full() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(0);
        let issuer_a = create_test_account_id(1);
        let issuer_b = create_test_account_id(2);
        state.create_account(create_test_account(source_id.clone(), 100_000_000, 0));
        state.create_account(create_test_account(issuer_a.clone(), 100_000_000, 0));
        state.create_account(create_test_account(issuer_b.clone(), 100_000_000, 0));

        let asset_a = Asset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4(*b"USD\0"),
            issuer: issuer_a,
        });
        let asset_b = Asset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4(*b"EUR\0"),
            issuer: issuer_b,
        });
        let pool_id = PoolId(Hash([2u8; 32]));
        state.create_liquidity_pool(create_pool_entry(
            pool_id.clone(),
            asset_a.clone(),
            asset_b.clone(),
            0,
            0,
            0,
        ));

        let trustline_a = TrustLineEntry {
            account_id: source_id.clone(),
            asset: TrustLineAsset::CreditAlphanum4(match &asset_a {
                Asset::CreditAlphanum4(a) => a.clone(),
                _ => unreachable!(),
            }),
            balance: 1_000,
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
            balance: 1_000,
            limit: 10_000,
            flags: TrustLineFlags::AuthorizedFlag as u32,
            ext: TrustLineEntryExt::V0,
        };
        let pool_share_tl = TrustLineEntry {
            account_id: source_id.clone(),
            asset: TrustLineAsset::PoolShare(pool_id.clone()),
            balance: 0,
            limit: 1,
            flags: 0,
            ext: TrustLineEntryExt::V0,
        };
        state.create_trustline(trustline_a);
        state.create_trustline(trustline_b);
        state.create_trustline(pool_share_tl);
        state.get_account_mut(&source_id).unwrap().num_sub_entries += 3;

        let op = LiquidityPoolDepositOp {
            liquidity_pool_id: pool_id,
            max_amount_a: 100,
            max_amount_b: 100,
            min_price: Price { n: 1, d: 1 },
            max_price: Price { n: 1, d: 1 },
        };

        let result = execute_liquidity_pool_deposit(&op, &source_id, &mut state, &context);
        match result.unwrap() {
            OperationResult::OpInner(OperationResultTr::LiquidityPoolDeposit(r)) => {
                assert!(matches!(r, LiquidityPoolDepositResult::LineFull));
            }
            other => panic!("unexpected result: {:?}", other),
        }
    }

    #[test]
    fn test_liquidity_pool_withdraw_line_full() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(0);
        let issuer_a = create_test_account_id(1);
        let issuer_b = create_test_account_id(2);
        state.create_account(create_test_account(source_id.clone(), 100_000_000, 0));
        state.create_account(create_test_account(issuer_a.clone(), 100_000_000, 0));
        state.create_account(create_test_account(issuer_b.clone(), 100_000_000, 0));

        let asset_a = Asset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4(*b"USD\0"),
            issuer: issuer_a,
        });
        let asset_b = Asset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4(*b"EUR\0"),
            issuer: issuer_b,
        });
        let pool_id = PoolId(Hash([3u8; 32]));
        state.create_liquidity_pool(create_pool_entry(
            pool_id.clone(),
            asset_a.clone(),
            asset_b.clone(),
            100,
            100,
            100,
        ));

        let trustline_a = TrustLineEntry {
            account_id: source_id.clone(),
            asset: TrustLineAsset::CreditAlphanum4(match &asset_a {
                Asset::CreditAlphanum4(a) => a.clone(),
                _ => unreachable!(),
            }),
            balance: 10,
            limit: 10,
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
            limit: 1_000,
            flags: TrustLineFlags::AuthorizedFlag as u32,
            ext: TrustLineEntryExt::V0,
        };
        let pool_share_tl = TrustLineEntry {
            account_id: source_id.clone(),
            asset: TrustLineAsset::PoolShare(pool_id.clone()),
            balance: 10,
            limit: 10_000,
            flags: 0,
            ext: TrustLineEntryExt::V0,
        };
        state.create_trustline(trustline_a);
        state.create_trustline(trustline_b);
        state.create_trustline(pool_share_tl);
        state.get_account_mut(&source_id).unwrap().num_sub_entries += 3;

        let op = LiquidityPoolWithdrawOp {
            liquidity_pool_id: pool_id,
            amount: 10,
            min_amount_a: 0,
            min_amount_b: 0,
        };

        let result = execute_liquidity_pool_withdraw(&op, &source_id, &mut state, &context);
        match result.unwrap() {
            OperationResult::OpInner(OperationResultTr::LiquidityPoolWithdraw(r)) => {
                assert!(matches!(r, LiquidityPoolWithdrawResult::LineFull));
            }
            other => panic!("unexpected result: {:?}", other),
        }
    }

    /// Test that asset issuers can deposit into liquidity pools without trustlines.
    /// This is a regression test for the bug at ledger 419086.
    #[test]
    fn test_liquidity_pool_deposit_issuer_no_trustline() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        // The issuer is the source account depositing
        let issuer_id = create_test_account_id(0);
        let other_issuer = create_test_account_id(1);
        state.create_account(create_test_account(issuer_id.clone(), 100_000_000, 0));
        state.create_account(create_test_account(other_issuer.clone(), 100_000_000, 0));

        // Asset B is issued by the depositor
        let asset_a = Asset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4(*b"USD\0"),
            issuer: other_issuer.clone(),
        });
        let asset_b = Asset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4(*b"RDCI"),
            issuer: issuer_id.clone(), // Issuer is the depositor
        });
        let pool_id = PoolId(Hash([10u8; 32]));
        state.create_liquidity_pool(create_pool_entry(
            pool_id.clone(),
            asset_a.clone(),
            asset_b.clone(),
            0,
            0,
            0,
        ));

        // Create trustline for asset_a (which issuer_id is NOT the issuer of)
        let trustline_a = TrustLineEntry {
            account_id: issuer_id.clone(),
            asset: TrustLineAsset::CreditAlphanum4(match &asset_a {
                Asset::CreditAlphanum4(a) => a.clone(),
                _ => unreachable!(),
            }),
            balance: 1_000_000,
            limit: 10_000_000,
            flags: TrustLineFlags::AuthorizedFlag as u32,
            ext: TrustLineEntryExt::V0,
        };
        // No trustline for asset_b since issuer_id IS the issuer

        let pool_share_tl = TrustLineEntry {
            account_id: issuer_id.clone(),
            asset: TrustLineAsset::PoolShare(pool_id.clone()),
            balance: 0,
            limit: i64::MAX,
            flags: 0,
            ext: TrustLineEntryExt::V0,
        };
        state.create_trustline(trustline_a);
        state.create_trustline(pool_share_tl);
        state.get_account_mut(&issuer_id).unwrap().num_sub_entries += 2;

        let op = LiquidityPoolDepositOp {
            liquidity_pool_id: pool_id,
            max_amount_a: 500_000,
            max_amount_b: 500_000,
            min_price: Price { n: 1, d: 2 },
            max_price: Price { n: 2, d: 1 },
        };

        let result = execute_liquidity_pool_deposit(&op, &issuer_id, &mut state, &context);
        match result.unwrap() {
            OperationResult::OpInner(OperationResultTr::LiquidityPoolDeposit(r)) => {
                // Should succeed - issuers can deposit their own assets without trustlines
                assert!(
                    matches!(r, LiquidityPoolDepositResult::Success),
                    "Expected Success, got {:?}",
                    r
                );
            }
            other => panic!("unexpected result: {:?}", other),
        }
    }

    /// Test that asset issuers can withdraw from liquidity pools and receive their own assets.
    #[test]
    fn test_liquidity_pool_withdraw_issuer_no_trustline() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        // The issuer is the source account withdrawing
        let issuer_id = create_test_account_id(0);
        let other_issuer = create_test_account_id(1);
        state.create_account(create_test_account(issuer_id.clone(), 100_000_000, 0));
        state.create_account(create_test_account(other_issuer.clone(), 100_000_000, 0));

        // Asset B is issued by the withdrawer
        let asset_a = Asset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4(*b"USD\0"),
            issuer: other_issuer.clone(),
        });
        let asset_b = Asset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4(*b"RDCI"),
            issuer: issuer_id.clone(), // Issuer is the withdrawer
        });
        let pool_id = PoolId(Hash([11u8; 32]));
        state.create_liquidity_pool(create_pool_entry(
            pool_id.clone(),
            asset_a.clone(),
            asset_b.clone(),
            1_000_000,
            1_000_000,
            1_000_000,
        ));

        // Create trustline for asset_a (which issuer_id is NOT the issuer of)
        let trustline_a = TrustLineEntry {
            account_id: issuer_id.clone(),
            asset: TrustLineAsset::CreditAlphanum4(match &asset_a {
                Asset::CreditAlphanum4(a) => a.clone(),
                _ => unreachable!(),
            }),
            balance: 0,
            limit: 10_000_000,
            flags: TrustLineFlags::AuthorizedFlag as u32,
            ext: TrustLineEntryExt::V0,
        };
        // No trustline for asset_b since issuer_id IS the issuer

        let pool_share_tl = TrustLineEntry {
            account_id: issuer_id.clone(),
            asset: TrustLineAsset::PoolShare(pool_id.clone()),
            balance: 100_000,
            limit: i64::MAX,
            flags: 0,
            ext: TrustLineEntryExt::V0,
        };
        state.create_trustline(trustline_a);
        state.create_trustline(pool_share_tl);
        state.get_account_mut(&issuer_id).unwrap().num_sub_entries += 2;

        let op = LiquidityPoolWithdrawOp {
            liquidity_pool_id: pool_id,
            amount: 50_000,
            min_amount_a: 0,
            min_amount_b: 0,
        };

        let result = execute_liquidity_pool_withdraw(&op, &issuer_id, &mut state, &context);
        match result.unwrap() {
            OperationResult::OpInner(OperationResultTr::LiquidityPoolWithdraw(r)) => {
                // Should succeed - issuers can receive their own assets without trustlines
                assert!(
                    matches!(r, LiquidityPoolWithdrawResult::Success),
                    "Expected Success, got {:?}",
                    r
                );
            }
            other => panic!("unexpected result: {:?}", other),
        }
    }

    /// Test successful deposit into an empty pool.
    ///
    /// C++ Reference: LiquidityPoolDepositTests.cpp - "deposit into empty pool"
    #[test]
    fn test_deposit_empty_pool_success() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(20);
        let issuer_a = create_test_account_id(21);
        let issuer_b = create_test_account_id(22);
        state.create_account(create_test_account(source_id.clone(), 100_000_000, 0));
        state.create_account(create_test_account(issuer_a.clone(), 100_000_000, 0));
        state.create_account(create_test_account(issuer_b.clone(), 100_000_000, 0));

        let asset_a = Asset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4(*b"USD\0"),
            issuer: issuer_a,
        });
        let asset_b = Asset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4(*b"EUR\0"),
            issuer: issuer_b,
        });
        let pool_id = PoolId(Hash([20u8; 32]));
        // Empty pool - no reserves, no shares
        state.create_liquidity_pool(create_pool_entry(
            pool_id.clone(),
            asset_a.clone(),
            asset_b.clone(),
            0,
            0,
            0,
        ));

        // Create authorized trustlines with sufficient balance
        let trustline_a = TrustLineEntry {
            account_id: source_id.clone(),
            asset: TrustLineAsset::CreditAlphanum4(match &asset_a {
                Asset::CreditAlphanum4(a) => a.clone(),
                _ => unreachable!(),
            }),
            balance: 10_000,
            limit: 100_000,
            flags: TrustLineFlags::AuthorizedFlag as u32,
            ext: TrustLineEntryExt::V0,
        };
        let trustline_b = TrustLineEntry {
            account_id: source_id.clone(),
            asset: TrustLineAsset::CreditAlphanum4(match &asset_b {
                Asset::CreditAlphanum4(a) => a.clone(),
                _ => unreachable!(),
            }),
            balance: 10_000,
            limit: 100_000,
            flags: TrustLineFlags::AuthorizedFlag as u32,
            ext: TrustLineEntryExt::V0,
        };
        let pool_share_tl = TrustLineEntry {
            account_id: source_id.clone(),
            asset: TrustLineAsset::PoolShare(pool_id.clone()),
            balance: 0,
            limit: i64::MAX,
            flags: 0,
            ext: TrustLineEntryExt::V0,
        };
        state.create_trustline(trustline_a);
        state.create_trustline(trustline_b);
        state.create_trustline(pool_share_tl);
        state.get_account_mut(&source_id).unwrap().num_sub_entries += 3;

        let op = LiquidityPoolDepositOp {
            liquidity_pool_id: pool_id,
            max_amount_a: 1000,
            max_amount_b: 1000,
            min_price: Price { n: 1, d: 2 },
            max_price: Price { n: 2, d: 1 },
        };

        let result = execute_liquidity_pool_deposit(&op, &source_id, &mut state, &context);
        match result.unwrap() {
            OperationResult::OpInner(OperationResultTr::LiquidityPoolDeposit(r)) => {
                assert!(
                    matches!(r, LiquidityPoolDepositResult::Success),
                    "Expected Success for empty pool deposit, got {:?}",
                    r
                );
            }
            other => panic!("unexpected result: {:?}", other),
        }
    }

    /// Test deposit fails when user doesn't have enough balance (Underfunded).
    ///
    /// C++ Reference: LiquidityPoolDepositTests.cpp - "underfunded" section
    #[test]
    fn test_deposit_underfunded() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(25);
        let issuer_a = create_test_account_id(26);
        let issuer_b = create_test_account_id(27);
        state.create_account(create_test_account(source_id.clone(), 100_000_000, 0));
        state.create_account(create_test_account(issuer_a.clone(), 100_000_000, 0));
        state.create_account(create_test_account(issuer_b.clone(), 100_000_000, 0));

        let asset_a = Asset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4(*b"USD\0"),
            issuer: issuer_a,
        });
        let asset_b = Asset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4(*b"EUR\0"),
            issuer: issuer_b,
        });
        let pool_id = PoolId(Hash([25u8; 32]));
        state.create_liquidity_pool(create_pool_entry(
            pool_id.clone(),
            asset_a.clone(),
            asset_b.clone(),
            0,
            0,
            0,
        ));

        // Create trustlines with INSUFFICIENT balance (only 50 but trying to deposit 1000)
        let trustline_a = TrustLineEntry {
            account_id: source_id.clone(),
            asset: TrustLineAsset::CreditAlphanum4(match &asset_a {
                Asset::CreditAlphanum4(a) => a.clone(),
                _ => unreachable!(),
            }),
            balance: 50, // Not enough
            limit: 100_000,
            flags: TrustLineFlags::AuthorizedFlag as u32,
            ext: TrustLineEntryExt::V0,
        };
        let trustline_b = TrustLineEntry {
            account_id: source_id.clone(),
            asset: TrustLineAsset::CreditAlphanum4(match &asset_b {
                Asset::CreditAlphanum4(a) => a.clone(),
                _ => unreachable!(),
            }),
            balance: 10_000,
            limit: 100_000,
            flags: TrustLineFlags::AuthorizedFlag as u32,
            ext: TrustLineEntryExt::V0,
        };
        let pool_share_tl = TrustLineEntry {
            account_id: source_id.clone(),
            asset: TrustLineAsset::PoolShare(pool_id.clone()),
            balance: 0,
            limit: i64::MAX,
            flags: 0,
            ext: TrustLineEntryExt::V0,
        };
        state.create_trustline(trustline_a);
        state.create_trustline(trustline_b);
        state.create_trustline(pool_share_tl);
        state.get_account_mut(&source_id).unwrap().num_sub_entries += 3;

        let op = LiquidityPoolDepositOp {
            liquidity_pool_id: pool_id,
            max_amount_a: 1000, // Trying to deposit more than balance
            max_amount_b: 1000,
            min_price: Price { n: 1, d: 2 },
            max_price: Price { n: 2, d: 1 },
        };

        let result = execute_liquidity_pool_deposit(&op, &source_id, &mut state, &context);
        match result.unwrap() {
            OperationResult::OpInner(OperationResultTr::LiquidityPoolDeposit(r)) => {
                assert!(
                    matches!(r, LiquidityPoolDepositResult::Underfunded),
                    "Expected Underfunded, got {:?}",
                    r
                );
            }
            other => panic!("unexpected result: {:?}", other),
        }
    }

    /// Test successful withdrawal from a pool with shares.
    ///
    /// C++ Reference: LiquidityPoolWithdrawTests.cpp - "basic withdraw"
    #[test]
    fn test_withdraw_basic_success() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(30);
        let issuer_a = create_test_account_id(31);
        let issuer_b = create_test_account_id(32);
        state.create_account(create_test_account(source_id.clone(), 100_000_000, 0));
        state.create_account(create_test_account(issuer_a.clone(), 100_000_000, 0));
        state.create_account(create_test_account(issuer_b.clone(), 100_000_000, 0));

        let asset_a = Asset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4(*b"USD\0"),
            issuer: issuer_a,
        });
        let asset_b = Asset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4(*b"EUR\0"),
            issuer: issuer_b,
        });
        let pool_id = PoolId(Hash([30u8; 32]));
        // Pool with reserves
        state.create_liquidity_pool(create_pool_entry(
            pool_id.clone(),
            asset_a.clone(),
            asset_b.clone(),
            1_000_000,
            1_000_000,
            1_000_000,
        ));

        // Create trustlines with room to receive assets
        let trustline_a = TrustLineEntry {
            account_id: source_id.clone(),
            asset: TrustLineAsset::CreditAlphanum4(match &asset_a {
                Asset::CreditAlphanum4(a) => a.clone(),
                _ => unreachable!(),
            }),
            balance: 0,
            limit: 10_000_000,
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
            limit: 10_000_000,
            flags: TrustLineFlags::AuthorizedFlag as u32,
            ext: TrustLineEntryExt::V0,
        };
        // Pool share trustline with balance to withdraw
        let pool_share_tl = TrustLineEntry {
            account_id: source_id.clone(),
            asset: TrustLineAsset::PoolShare(pool_id.clone()),
            balance: 100_000,
            limit: i64::MAX,
            flags: 0,
            ext: TrustLineEntryExt::V0,
        };
        state.create_trustline(trustline_a);
        state.create_trustline(trustline_b);
        state.create_trustline(pool_share_tl);
        state.get_account_mut(&source_id).unwrap().num_sub_entries += 3;

        let op = LiquidityPoolWithdrawOp {
            liquidity_pool_id: pool_id,
            amount: 50_000, // Withdraw half of our shares
            min_amount_a: 0,
            min_amount_b: 0,
        };

        let result = execute_liquidity_pool_withdraw(&op, &source_id, &mut state, &context);
        match result.unwrap() {
            OperationResult::OpInner(OperationResultTr::LiquidityPoolWithdraw(r)) => {
                assert!(
                    matches!(r, LiquidityPoolWithdrawResult::Success),
                    "Expected Success, got {:?}",
                    r
                );
            }
            other => panic!("unexpected result: {:?}", other),
        }
    }

    /// Test withdraw fails when user doesn't have enough pool shares (Underfunded).
    ///
    /// C++ Reference: LiquidityPoolWithdrawTests.cpp - "underfunded" section
    #[test]
    fn test_withdraw_underfunded() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(35);
        let issuer_a = create_test_account_id(36);
        let issuer_b = create_test_account_id(37);
        state.create_account(create_test_account(source_id.clone(), 100_000_000, 0));
        state.create_account(create_test_account(issuer_a.clone(), 100_000_000, 0));
        state.create_account(create_test_account(issuer_b.clone(), 100_000_000, 0));

        let asset_a = Asset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4(*b"USD\0"),
            issuer: issuer_a,
        });
        let asset_b = Asset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4(*b"EUR\0"),
            issuer: issuer_b,
        });
        let pool_id = PoolId(Hash([35u8; 32]));
        state.create_liquidity_pool(create_pool_entry(
            pool_id.clone(),
            asset_a.clone(),
            asset_b.clone(),
            1_000_000,
            1_000_000,
            1_000_000,
        ));

        let trustline_a = TrustLineEntry {
            account_id: source_id.clone(),
            asset: TrustLineAsset::CreditAlphanum4(match &asset_a {
                Asset::CreditAlphanum4(a) => a.clone(),
                _ => unreachable!(),
            }),
            balance: 0,
            limit: 10_000_000,
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
            limit: 10_000_000,
            flags: TrustLineFlags::AuthorizedFlag as u32,
            ext: TrustLineEntryExt::V0,
        };
        // Pool share trustline with only 100 shares
        let pool_share_tl = TrustLineEntry {
            account_id: source_id.clone(),
            asset: TrustLineAsset::PoolShare(pool_id.clone()),
            balance: 100,
            limit: i64::MAX,
            flags: 0,
            ext: TrustLineEntryExt::V0,
        };
        state.create_trustline(trustline_a);
        state.create_trustline(trustline_b);
        state.create_trustline(pool_share_tl);
        state.get_account_mut(&source_id).unwrap().num_sub_entries += 3;

        let op = LiquidityPoolWithdrawOp {
            liquidity_pool_id: pool_id,
            amount: 50_000, // Trying to withdraw more than we have
            min_amount_a: 0,
            min_amount_b: 0,
        };

        let result = execute_liquidity_pool_withdraw(&op, &source_id, &mut state, &context);
        match result.unwrap() {
            OperationResult::OpInner(OperationResultTr::LiquidityPoolWithdraw(r)) => {
                assert!(
                    matches!(r, LiquidityPoolWithdrawResult::Underfunded),
                    "Expected Underfunded, got {:?}",
                    r
                );
            }
            other => panic!("unexpected result: {:?}", other),
        }
    }

    /// Test LiquidityPoolWithdraw underfunded due to selling liabilities on pool shares.
    ///
    /// C++ uses `getAvailableBalance(header, tlPool)` which subtracts selling
    /// liabilities. If pool share trustline has balance=1000 and selling
    /// liabilities=400, only 600 shares are available for withdrawal.
    ///
    /// C++ Reference: LiquidityPoolWithdrawOpFrame.cpp:47
    #[test]
    fn test_withdraw_underfunded_due_to_selling_liabilities() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(60);
        let issuer_a = create_test_account_id(61);
        let issuer_b = create_test_account_id(62);
        state.create_account(create_test_account(source_id.clone(), 100_000_000, 0));
        state.create_account(create_test_account(issuer_a.clone(), 100_000_000, 0));
        state.create_account(create_test_account(issuer_b.clone(), 100_000_000, 0));

        let asset_a = Asset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4(*b"USD\0"),
            issuer: issuer_a,
        });
        let asset_b = Asset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4(*b"EUR\0"),
            issuer: issuer_b,
        });
        let pool_id = PoolId(Hash([60u8; 32]));
        state.create_liquidity_pool(create_pool_entry(
            pool_id.clone(),
            asset_a.clone(),
            asset_b.clone(),
            1_000_000,
            1_000_000,
            1_000_000,
        ));

        let trustline_a = TrustLineEntry {
            account_id: source_id.clone(),
            asset: TrustLineAsset::CreditAlphanum4(match &asset_a {
                Asset::CreditAlphanum4(a) => a.clone(),
                _ => unreachable!(),
            }),
            balance: 0,
            limit: 10_000_000,
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
            limit: 10_000_000,
            flags: TrustLineFlags::AuthorizedFlag as u32,
            ext: TrustLineEntryExt::V0,
        };
        // Pool share trustline with balance=1000 and selling liabilities=400
        // Available = 1000 - 400 = 600
        let pool_share_tl = TrustLineEntry {
            account_id: source_id.clone(),
            asset: TrustLineAsset::PoolShare(pool_id.clone()),
            balance: 1000,
            limit: i64::MAX,
            flags: 0,
            ext: TrustLineEntryExt::V1(TrustLineEntryV1 {
                liabilities: Liabilities {
                    buying: 0,
                    selling: 400,
                },
                ext: TrustLineEntryV1Ext::V0,
            }),
        };
        state.create_trustline(trustline_a);
        state.create_trustline(trustline_b);
        state.create_trustline(pool_share_tl);
        state.get_account_mut(&source_id).unwrap().num_sub_entries += 3;

        // Try to withdraw 700 shares — more than available (600)
        let op = LiquidityPoolWithdrawOp {
            liquidity_pool_id: pool_id,
            amount: 700,
            min_amount_a: 0,
            min_amount_b: 0,
        };

        let result = execute_liquidity_pool_withdraw(&op, &source_id, &mut state, &context);
        match result.unwrap() {
            OperationResult::OpInner(OperationResultTr::LiquidityPoolWithdraw(r)) => {
                assert!(
                    matches!(r, LiquidityPoolWithdrawResult::Underfunded),
                    "Expected Underfunded when withdrawal exceeds available balance (balance - selling liabilities), got {:?}",
                    r
                );
            }
            other => panic!("unexpected result: {:?}", other),
        }
    }

    /// Test deposit fails when price is outside specified bounds (BadPrice).
    ///
    /// C++ Reference: LiquidityPoolDepositTests.cpp - "bad price" section
    #[test]
    fn test_deposit_bad_price() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(40);
        let issuer_a = create_test_account_id(41);
        let issuer_b = create_test_account_id(42);
        state.create_account(create_test_account(source_id.clone(), 100_000_000, 0));
        state.create_account(create_test_account(issuer_a.clone(), 100_000_000, 0));
        state.create_account(create_test_account(issuer_b.clone(), 100_000_000, 0));

        let asset_a = Asset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4(*b"USD\0"),
            issuer: issuer_a,
        });
        let asset_b = Asset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4(*b"EUR\0"),
            issuer: issuer_b,
        });
        let pool_id = PoolId(Hash([40u8; 32]));
        // Pool with 10:1 ratio (reserve_a: 1_000_000, reserve_b: 100_000)
        // Price (b/a) = 100_000/1_000_000 = 0.1
        state.create_liquidity_pool(create_pool_entry(
            pool_id.clone(),
            asset_a.clone(),
            asset_b.clone(),
            1_000_000,
            100_000,
            316_227, // sqrt(1_000_000 * 100_000) approx
        ));

        let trustline_a = TrustLineEntry {
            account_id: source_id.clone(),
            asset: TrustLineAsset::CreditAlphanum4(match &asset_a {
                Asset::CreditAlphanum4(a) => a.clone(),
                _ => unreachable!(),
            }),
            balance: 10_000,
            limit: 100_000,
            flags: TrustLineFlags::AuthorizedFlag as u32,
            ext: TrustLineEntryExt::V0,
        };
        let trustline_b = TrustLineEntry {
            account_id: source_id.clone(),
            asset: TrustLineAsset::CreditAlphanum4(match &asset_b {
                Asset::CreditAlphanum4(a) => a.clone(),
                _ => unreachable!(),
            }),
            balance: 10_000,
            limit: 100_000,
            flags: TrustLineFlags::AuthorizedFlag as u32,
            ext: TrustLineEntryExt::V0,
        };
        let pool_share_tl = TrustLineEntry {
            account_id: source_id.clone(),
            asset: TrustLineAsset::PoolShare(pool_id.clone()),
            balance: 0,
            limit: i64::MAX,
            flags: 0,
            ext: TrustLineEntryExt::V0,
        };
        state.create_trustline(trustline_a);
        state.create_trustline(trustline_b);
        state.create_trustline(pool_share_tl);
        state.get_account_mut(&source_id).unwrap().num_sub_entries += 3;

        // Set min_price to 0.5 (1/2), but pool price is 0.1
        // Pool price is lower than min_price, so should fail
        let op = LiquidityPoolDepositOp {
            liquidity_pool_id: pool_id,
            max_amount_a: 1000,
            max_amount_b: 1000,
            min_price: Price { n: 1, d: 2 }, // 0.5 - min acceptable price
            max_price: Price { n: 2, d: 1 }, // 2.0 - max acceptable price
        };

        let result = execute_liquidity_pool_deposit(&op, &source_id, &mut state, &context);
        match result.unwrap() {
            OperationResult::OpInner(OperationResultTr::LiquidityPoolDeposit(r)) => {
                assert!(
                    matches!(r, LiquidityPoolDepositResult::BadPrice),
                    "Expected BadPrice, got {:?}",
                    r
                );
            }
            other => panic!("unexpected result: {:?}", other),
        }
    }

    /// Test withdraw fails when min amounts aren't met (UnderMinimum).
    ///
    /// C++ Reference: LiquidityPoolWithdrawTests.cpp - "under minimum" section
    #[test]
    fn test_withdraw_under_minimum() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(45);
        let issuer_a = create_test_account_id(46);
        let issuer_b = create_test_account_id(47);
        state.create_account(create_test_account(source_id.clone(), 100_000_000, 0));
        state.create_account(create_test_account(issuer_a.clone(), 100_000_000, 0));
        state.create_account(create_test_account(issuer_b.clone(), 100_000_000, 0));

        let asset_a = Asset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4(*b"USD\0"),
            issuer: issuer_a,
        });
        let asset_b = Asset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4(*b"EUR\0"),
            issuer: issuer_b,
        });
        let pool_id = PoolId(Hash([45u8; 32]));
        // Pool: 1M reserves each, 1M total shares
        state.create_liquidity_pool(create_pool_entry(
            pool_id.clone(),
            asset_a.clone(),
            asset_b.clone(),
            1_000_000,
            1_000_000,
            1_000_000,
        ));

        let trustline_a = TrustLineEntry {
            account_id: source_id.clone(),
            asset: TrustLineAsset::CreditAlphanum4(match &asset_a {
                Asset::CreditAlphanum4(a) => a.clone(),
                _ => unreachable!(),
            }),
            balance: 0,
            limit: 10_000_000,
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
            limit: 10_000_000,
            flags: TrustLineFlags::AuthorizedFlag as u32,
            ext: TrustLineEntryExt::V0,
        };
        // User has 1000 pool shares (0.1% of pool)
        // Withdrawing 1000 shares would give ~1000 of each asset
        let pool_share_tl = TrustLineEntry {
            account_id: source_id.clone(),
            asset: TrustLineAsset::PoolShare(pool_id.clone()),
            balance: 1_000,
            limit: i64::MAX,
            flags: 0,
            ext: TrustLineEntryExt::V0,
        };
        state.create_trustline(trustline_a);
        state.create_trustline(trustline_b);
        state.create_trustline(pool_share_tl);
        state.get_account_mut(&source_id).unwrap().num_sub_entries += 3;

        // Withdraw with unreasonably high min_amount requirements
        let op = LiquidityPoolWithdrawOp {
            liquidity_pool_id: pool_id,
            amount: 1_000,
            min_amount_a: 10_000, // Expecting way more than withdrawal would provide
            min_amount_b: 10_000,
        };

        let result = execute_liquidity_pool_withdraw(&op, &source_id, &mut state, &context);
        match result.unwrap() {
            OperationResult::OpInner(OperationResultTr::LiquidityPoolWithdraw(r)) => {
                assert!(
                    matches!(r, LiquidityPoolWithdrawResult::UnderMinimum),
                    "Expected UnderMinimum, got {:?}",
                    r
                );
            }
            other => panic!("unexpected result: {:?}", other),
        }
    }

    /// Regression test: LiquidityPoolDeposit available balance must deduct
    /// selling liabilities from trustline balances (matching stellar-core's
    /// TrustLineWrapper::getAvailableBalance).
    #[test]
    fn test_deposit_underfunded_due_to_selling_liabilities() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(50);
        let issuer_a = create_test_account_id(51);
        let issuer_b = create_test_account_id(52);
        state.create_account(create_test_account(source_id.clone(), 100_000_000, 0));
        state.create_account(create_test_account(issuer_a.clone(), 100_000_000, 0));
        state.create_account(create_test_account(issuer_b.clone(), 100_000_000, 0));

        let asset_a = Asset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4(*b"USD\0"),
            issuer: issuer_a,
        });
        let asset_b = Asset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4(*b"EUR\0"),
            issuer: issuer_b,
        });
        let pool_id = PoolId(Hash([50u8; 32]));
        state.create_liquidity_pool(create_pool_entry(
            pool_id.clone(),
            asset_a.clone(),
            asset_b.clone(),
            0,
            0,
            0,
        ));

        // Trustline A: balance=1000 but selling_liabilities=900, so available=100
        let trustline_a = TrustLineEntry {
            account_id: source_id.clone(),
            asset: TrustLineAsset::CreditAlphanum4(match &asset_a {
                Asset::CreditAlphanum4(a) => a.clone(),
                _ => unreachable!(),
            }),
            balance: 1_000,
            limit: 100_000,
            flags: TrustLineFlags::AuthorizedFlag as u32,
            ext: TrustLineEntryExt::V1(TrustLineEntryV1 {
                liabilities: Liabilities {
                    buying: 0,
                    selling: 900,
                },
                ext: TrustLineEntryV1Ext::V0,
            }),
        };
        let trustline_b = TrustLineEntry {
            account_id: source_id.clone(),
            asset: TrustLineAsset::CreditAlphanum4(match &asset_b {
                Asset::CreditAlphanum4(a) => a.clone(),
                _ => unreachable!(),
            }),
            balance: 10_000,
            limit: 100_000,
            flags: TrustLineFlags::AuthorizedFlag as u32,
            ext: TrustLineEntryExt::V0,
        };
        let pool_share_tl = TrustLineEntry {
            account_id: source_id.clone(),
            asset: TrustLineAsset::PoolShare(pool_id.clone()),
            balance: 0,
            limit: i64::MAX,
            flags: 0,
            ext: TrustLineEntryExt::V0,
        };
        state.create_trustline(trustline_a);
        state.create_trustline(trustline_b);
        state.create_trustline(pool_share_tl);
        state.get_account_mut(&source_id).unwrap().num_sub_entries += 3;

        // Deposit 500 of each — raw balance (1000) >= 500,
        // but available (1000 - 900 = 100) < 500
        let op = LiquidityPoolDepositOp {
            liquidity_pool_id: pool_id,
            max_amount_a: 500,
            max_amount_b: 500,
            min_price: Price { n: 1, d: 2 },
            max_price: Price { n: 2, d: 1 },
        };

        let result = execute_liquidity_pool_deposit(&op, &source_id, &mut state, &context);
        match result.unwrap() {
            OperationResult::OpInner(OperationResultTr::LiquidityPoolDeposit(r)) => {
                assert!(
                    matches!(r, LiquidityPoolDepositResult::Underfunded),
                    "Expected Underfunded when selling liabilities reduce available balance, got {:?}",
                    r
                );
            }
            other => panic!("unexpected result: {:?}", other),
        }
    }

    /// Test that big_divide_checked returns None on overflow (matching C++ bigDivide
    /// returning false).
    #[test]
    fn test_big_divide_checked_overflow() {
        // Normal case: should return Some
        let result = big_divide_checked(100, 200, 10, Round::Down);
        assert_eq!(result, Some(2000));

        // Overflow case: i64::MAX * i64::MAX / 1 overflows i64
        let result = big_divide_checked(i64::MAX, i64::MAX, 1, Round::Down);
        assert_eq!(result, None, "Should return None on overflow");

        // Just under overflow: should succeed
        let result = big_divide_checked(i64::MAX, 1, 1, Round::Down);
        assert_eq!(result, Some(i64::MAX));
    }

    /// Test minAmongValid logic in deposit_into_non_empty_pool.
    ///
    /// When one of the two bigDivide share calculations overflows, C++ uses the
    /// other valid result via minAmongValid. Henyey must do the same instead of
    /// silently using 0 for the overflowed value.
    #[test]
    fn test_deposit_non_empty_pool_one_share_overflows() {
        // Set up a scenario where shares_a overflows but shares_b is valid.
        //
        // shares_a = bigDivide(total_shares, max_amount_a, reserve_a, ROUND_DOWN)
        // shares_b = bigDivide(total_shares, max_amount_b, reserve_b, ROUND_DOWN)
        //
        // To make shares_a overflow: total_shares * max_amount_a must overflow i64
        // when divided by reserve_a. Use very large total_shares and max_amount_a
        // with small reserve_a.
        let total_shares = i64::MAX; // ~9.2e18
        let reserve_a = 1; // Makes shares_a = total_shares * max_amount_a / 1
        let max_amount_a = i64::MAX; // shares_a = i64::MAX * i64::MAX = overflow

        // For shares_b: use values that won't overflow
        let reserve_b = i64::MAX;
        let max_amount_b = 100; // shares_b = i64::MAX * 100 / i64::MAX = 100

        let min_price = Price { n: 1, d: i32::MAX };
        let max_price = Price { n: i32::MAX, d: 1 };

        let result = deposit_into_non_empty_pool(NonEmptyPoolDepositRequest {
            max_amount_a,
            max_amount_b,
            available_a: i64::MAX,
            available_b: i64::MAX,
            available_pool_share_limit: i64::MAX,
            reserve_a,
            reserve_b,
            total_shares,
            min_price: &min_price,
            max_price: &max_price,
        });

        // The key assertion: with minAmongValid, when shares_a overflows,
        // pool_shares should be shares_b (100), not min(0, 100) = 0.
        // Previously big_divide returned Ok(0) on overflow, causing pool_shares = 0.
        match result.unwrap() {
            DepositOutcome::Success { shares, .. } => {
                assert!(
                    shares > 0,
                    "With minAmongValid, when one share overflows, pool_shares should be the valid one (> 0), got {}",
                    shares
                );
                assert_eq!(shares, 100, "pool_shares should be shares_b=100");
            }
            other => {
                // Any non-success result is acceptable as long as it's not caused
                // by pool_shares being 0 due to overflow.
                // But in this case we expect Success since all limits are i64::MAX.
                panic!("Expected Success, got {:?}", other);
            }
        }
    }

    /// Test deposit with max_amount_a <= 0 returns Malformed.
    #[test]
    fn test_liquidity_pool_deposit_malformed_negative_max_amount() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(0);
        state.create_account(create_test_account(source_id.clone(), 100_000_000, 0));

        let op = LiquidityPoolDepositOp {
            liquidity_pool_id: PoolId(Hash([1u8; 32])),
            max_amount_a: -1,
            max_amount_b: 100,
            min_price: Price { n: 1, d: 1 },
            max_price: Price { n: 1, d: 1 },
        };
        let result = execute_liquidity_pool_deposit(&op, &source_id, &mut state, &context);
        assert!(matches!(
            result.unwrap(),
            OperationResult::OpInner(OperationResultTr::LiquidityPoolDeposit(
                LiquidityPoolDepositResult::Malformed
            ))
        ));
    }

    /// Test deposit with price numerator <= 0 returns Malformed.
    #[test]
    fn test_liquidity_pool_deposit_malformed_invalid_price() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(0);
        state.create_account(create_test_account(source_id.clone(), 100_000_000, 0));

        let op = LiquidityPoolDepositOp {
            liquidity_pool_id: PoolId(Hash([1u8; 32])),
            max_amount_a: 100,
            max_amount_b: 100,
            min_price: Price { n: 0, d: 1 },
            max_price: Price { n: 1, d: 1 },
        };
        let result = execute_liquidity_pool_deposit(&op, &source_id, &mut state, &context);
        assert!(matches!(
            result.unwrap(),
            OperationResult::OpInner(OperationResultTr::LiquidityPoolDeposit(
                LiquidityPoolDepositResult::Malformed
            ))
        ));
    }

    /// Test deposit with min_price > max_price returns Malformed.
    #[test]
    fn test_liquidity_pool_deposit_malformed_min_price_exceeds_max() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(0);
        state.create_account(create_test_account(source_id.clone(), 100_000_000, 0));

        let op = LiquidityPoolDepositOp {
            liquidity_pool_id: PoolId(Hash([1u8; 32])),
            max_amount_a: 100,
            max_amount_b: 100,
            min_price: Price { n: 3, d: 1 }, // min = 3.0
            max_price: Price { n: 1, d: 1 }, // max = 1.0
        };
        let result = execute_liquidity_pool_deposit(&op, &source_id, &mut state, &context);
        assert!(matches!(
            result.unwrap(),
            OperationResult::OpInner(OperationResultTr::LiquidityPoolDeposit(
                LiquidityPoolDepositResult::Malformed
            ))
        ));
    }

    /// Test deposit when pool reserve would overflow returns PoolFull.
    #[test]
    fn test_liquidity_pool_deposit_pool_full() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(0);
        let issuer_a = create_test_account_id(1);
        let issuer_b = create_test_account_id(2);
        state.create_account(create_test_account(source_id.clone(), i64::MAX, 0));
        state.create_account(create_test_account(issuer_a.clone(), 100_000_000, 0));
        state.create_account(create_test_account(issuer_b.clone(), 100_000_000, 0));

        let asset_a = Asset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4(*b"EUR\0"),
            issuer: issuer_a.clone(),
        });
        let asset_b = Asset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4(*b"USD\0"),
            issuer: issuer_b.clone(),
        });

        let pool_id = PoolId(Hash([1u8; 32]));
        // Pool with reserves near i64::MAX
        state.create_liquidity_pool(create_pool_entry(
            pool_id.clone(),
            asset_a.clone(),
            asset_b.clone(),
            i64::MAX - 1,
            i64::MAX - 1,
            i64::MAX - 1,
        ));

        // Create trustlines with huge balances
        let tl_a = TrustLineEntry {
            account_id: source_id.clone(),
            asset: TrustLineAsset::CreditAlphanum4(match &asset_a {
                Asset::CreditAlphanum4(a) => a.clone(),
                _ => unreachable!(),
            }),
            balance: i64::MAX / 2,
            limit: i64::MAX,
            flags: TrustLineFlags::AuthorizedFlag as u32,
            ext: TrustLineEntryExt::V0,
        };
        let tl_b = TrustLineEntry {
            account_id: source_id.clone(),
            asset: TrustLineAsset::CreditAlphanum4(match &asset_b {
                Asset::CreditAlphanum4(a) => a.clone(),
                _ => unreachable!(),
            }),
            balance: i64::MAX / 2,
            limit: i64::MAX,
            flags: TrustLineFlags::AuthorizedFlag as u32,
            ext: TrustLineEntryExt::V0,
        };
        let pool_share_tl = TrustLineEntry {
            account_id: source_id.clone(),
            asset: TrustLineAsset::PoolShare(pool_id.clone()),
            balance: 0,
            limit: i64::MAX,
            flags: 0,
            ext: TrustLineEntryExt::V0,
        };
        state.create_trustline(tl_a);
        state.create_trustline(tl_b);
        state.create_trustline(pool_share_tl);
        state.get_account_mut(&source_id).unwrap().num_sub_entries += 3;

        let op = LiquidityPoolDepositOp {
            liquidity_pool_id: pool_id,
            max_amount_a: 100,
            max_amount_b: 100,
            min_price: Price { n: 1, d: 2 },
            max_price: Price { n: 2, d: 1 },
        };
        let result = execute_liquidity_pool_deposit(&op, &source_id, &mut state, &context);
        match result.unwrap() {
            OperationResult::OpInner(OperationResultTr::LiquidityPoolDeposit(r)) => {
                assert!(
                    matches!(r, LiquidityPoolDepositResult::PoolFull),
                    "Expected PoolFull, got {:?}",
                    r
                );
            }
            other => panic!("unexpected: {:?}", other),
        }
    }

    /// Test withdraw with amount <= 0 returns Malformed.
    #[test]
    fn test_liquidity_pool_withdraw_malformed_zero_amount() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(0);
        state.create_account(create_test_account(source_id.clone(), 100_000_000, 0));

        let op = LiquidityPoolWithdrawOp {
            liquidity_pool_id: PoolId(Hash([1u8; 32])),
            amount: 0,
            min_amount_a: 0,
            min_amount_b: 0,
        };
        let result = execute_liquidity_pool_withdraw(&op, &source_id, &mut state, &context);
        assert!(matches!(
            result.unwrap(),
            OperationResult::OpInner(OperationResultTr::LiquidityPoolWithdraw(
                LiquidityPoolWithdrawResult::Malformed
            ))
        ));
    }

    /// Test withdraw with negative min_amount returns Malformed.
    #[test]
    fn test_liquidity_pool_withdraw_malformed_negative_min_amount() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(0);
        state.create_account(create_test_account(source_id.clone(), 100_000_000, 0));

        let op = LiquidityPoolWithdrawOp {
            liquidity_pool_id: PoolId(Hash([1u8; 32])),
            amount: 100,
            min_amount_a: -1,
            min_amount_b: 0,
        };
        let result = execute_liquidity_pool_withdraw(&op, &source_id, &mut state, &context);
        assert!(matches!(
            result.unwrap(),
            OperationResult::OpInner(OperationResultTr::LiquidityPoolWithdraw(
                LiquidityPoolWithdrawResult::Malformed
            ))
        ));
    }

    /// Regression test for AUDIT-010: Deposit auth check bypassed when issuer lacks AUTH_REQUIRED.
    ///
    /// stellar-core checks trustline authorization unconditionally via tlA.isAuthorized().
    /// Henyey was gating the check behind is_auth_required() which checked the issuer's
    /// account flags, allowing deposits with deauthorized trustlines when the issuer had
    /// cleared AUTH_REQUIRED.
    #[test]
    fn test_audit_010_deposit_deauthorized_trustline_without_auth_required() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(0);
        let issuer_a = create_test_account_id(1);
        let issuer_b = create_test_account_id(2);
        state.create_account(create_test_account(source_id.clone(), 100_000_000, 0));
        // Issuer A has NO AUTH_REQUIRED flag (flags=0).
        state.create_account(create_test_account(issuer_a.clone(), 100_000_000, 0));
        state.create_account(create_test_account(issuer_b.clone(), 100_000_000, 0));

        let asset_a = Asset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4(*b"USD\0"),
            issuer: issuer_a,
        });
        let asset_b = Asset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4(*b"EUR\0"),
            issuer: issuer_b,
        });
        let pool_id = PoolId(Hash([1u8; 32]));
        state.create_liquidity_pool(create_pool_entry(
            pool_id.clone(),
            asset_a.clone(),
            asset_b.clone(),
            0,
            0,
            0,
        ));

        // Trustline A: deauthorized (flags=0, no AUTHORIZED_FLAG)
        let trustline_a = TrustLineEntry {
            account_id: source_id.clone(),
            asset: TrustLineAsset::CreditAlphanum4(match &asset_a {
                Asset::CreditAlphanum4(a) => a.clone(),
                _ => unreachable!(),
            }),
            balance: 1_000,
            limit: 10_000,
            flags: 0, // deauthorized!
            ext: TrustLineEntryExt::V0,
        };
        // Trustline B: authorized
        let trustline_b = TrustLineEntry {
            account_id: source_id.clone(),
            asset: TrustLineAsset::CreditAlphanum4(match &asset_b {
                Asset::CreditAlphanum4(a) => a.clone(),
                _ => unreachable!(),
            }),
            balance: 1_000,
            limit: 10_000,
            flags: TrustLineFlags::AuthorizedFlag as u32,
            ext: TrustLineEntryExt::V0,
        };
        let pool_share_tl = TrustLineEntry {
            account_id: source_id.clone(),
            asset: TrustLineAsset::PoolShare(pool_id.clone()),
            balance: 0,
            limit: 10_000,
            flags: 0,
            ext: TrustLineEntryExt::V0,
        };
        state.create_trustline(trustline_a);
        state.create_trustline(trustline_b);
        state.create_trustline(pool_share_tl);
        state.get_account_mut(&source_id).unwrap().num_sub_entries += 3;

        let op = LiquidityPoolDepositOp {
            liquidity_pool_id: pool_id,
            max_amount_a: 100,
            max_amount_b: 100,
            min_price: Price { n: 1, d: 1 },
            max_price: Price { n: 1, d: 1 },
        };

        // Must reject with NotAuthorized even though issuer lacks AUTH_REQUIRED,
        // because the trustline itself is deauthorized (flags=0).
        let result = execute_liquidity_pool_deposit(&op, &source_id, &mut state, &context);
        match result.unwrap() {
            OperationResult::OpInner(OperationResultTr::LiquidityPoolDeposit(r)) => {
                assert!(
                    matches!(r, LiquidityPoolDepositResult::NotAuthorized),
                    "Expected NotAuthorized, got {:?}",
                    r
                );
            }
            other => panic!("unexpected result: {:?}", other),
        }
    }
}
