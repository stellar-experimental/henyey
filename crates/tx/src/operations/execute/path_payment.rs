//! Path Payment operation execution.
//!
//! This module implements the execution logic for PathPaymentStrictReceive
//! and PathPaymentStrictSend operations, which transfer assets through a path.

use sha2::{Digest, Sha256};
use stellar_xdr::curr::{
    AccountId, Asset, ClaimAtom, ClaimLiquidityAtom, Hash, Limits, LiquidityPoolEntryBody,
    LiquidityPoolParameters, OperationResult, OperationResultTr, PathPaymentStrictReceiveOp,
    PathPaymentStrictReceiveResult, PathPaymentStrictReceiveResultCode,
    PathPaymentStrictReceiveResultSuccess, PathPaymentStrictSendOp, PathPaymentStrictSendResult,
    PathPaymentStrictSendResultCode, PathPaymentStrictSendResultSuccess, PoolId,
    SimplePaymentResult, WriteXdr, LIQUIDITY_POOL_FEE_V18,
};

use super::offer_exchange::{ConversionParams, RoundingType};
use super::offer_utils::{delete_offer_with_sponsorship, offer_liabilities_sell};
use super::{
    account_liabilities, add_account_balance, add_trustline_balance, apply_liabilities_delta,
    is_trustline_authorized, issuer_for_asset, trustline_liabilities,
};
use crate::frame::muxed_to_account_id;
use crate::frozen_keys::offer_accesses_frozen_key;
use crate::state::LedgerStateManager;
use crate::validation::LedgerContext;
use crate::{Result, TxError};

/// Execute a PathPaymentStrictReceive operation.
///
/// This operation sends at most `send_max` of `send_asset` to receive exactly
/// `dest_amount` of `dest_asset` at the destination.
pub(crate) fn execute_path_payment_strict_receive(
    op: &PathPaymentStrictReceiveOp,
    source: &AccountId,
    state: &mut LedgerStateManager,
    context: &LedgerContext,
) -> Result<OperationResult> {
    let dest = muxed_to_account_id(&op.destination);

    // Validate amounts
    if op.send_max <= 0 || op.dest_amount <= 0 {
        return Ok(make_strict_receive_result(
            PathPaymentStrictReceiveResultCode::Malformed,
            None,
        ));
    }

    let bypass_issuer_check =
        should_bypass_issuer_check(op.path.as_slice(), &op.send_asset, &op.dest_asset, &dest);
    if !bypass_issuer_check && state.get_account(&dest).is_none() {
        return Ok(make_strict_receive_result(
            PathPaymentStrictReceiveResultCode::NoDestination,
            None,
        ));
    }

    let mut offers_claimed: Vec<ClaimAtom> = Vec::new();

    if let Err(err) = update_dest_balance(
        &dest,
        &op.dest_asset,
        op.dest_amount,
        bypass_issuer_check,
        state,
        context,
    ) {
        return Ok(make_strict_receive_result_with_asset(
            err.code,
            err.no_issuer_asset,
            None,
        ));
    }

    let mut success = PathPaymentStrictReceiveResultSuccess {
        offers: vec![].try_into().unwrap(),
        last: SimplePaymentResult {
            destination: dest.clone(),
            asset: op.dest_asset.clone(),
            amount: op.dest_amount,
        },
    };

    let mut full_path: Vec<Asset> = op.path.iter().cloned().rev().collect();
    full_path.push(op.send_asset.clone());

    let mut recv_asset = op.dest_asset.clone();
    let mut max_amount_recv = op.dest_amount;
    for send_asset in full_path {
        if recv_asset == send_asset {
            continue;
        }

        if !bypass_issuer_check {
            if let Err(err) = check_issuer(&send_asset, state, context) {
                return Ok(make_strict_receive_result_with_asset(
                    err.code,
                    err.no_issuer_asset,
                    None,
                ));
            }
        }

        let mut amount_send = 0;
        let mut amount_recv = 0;
        let mut offer_trail = Vec::new();
        let max_offers_to_cross = MAX_OFFERS_TO_CROSS - offers_claimed.len() as i64;
        let convert_res = convert_with_offers_and_pools(
            &mut ConversionParams {
                source,
                selling: &send_asset,
                buying: &recv_asset,
                max_send: i64::MAX,
                max_receive: max_amount_recv,
                round: RoundingType::PathPaymentStrictReceive,
                offer_trail: &mut offer_trail,
                state,
                context,
                frozen_key_config: &context.frozen_key_config,
            },
            &mut amount_send,
            &mut amount_recv,
            max_offers_to_cross,
        )?;

        if convert_res == ConvertResult::FilterStopCrossSelf {
            return Ok(make_strict_receive_result(
                PathPaymentStrictReceiveResultCode::OfferCrossSelf,
                None,
            ));
        }
        if convert_res == ConvertResult::CrossedTooMany {
            return Ok(OperationResult::OpExceededWorkLimit);
        }
        if convert_res != ConvertResult::Ok || amount_recv != max_amount_recv {
            return Ok(make_strict_receive_result(
                PathPaymentStrictReceiveResultCode::TooFewOffers,
                None,
            ));
        }

        max_amount_recv = amount_send;
        recv_asset = send_asset;

        offers_claimed.splice(0..0, offer_trail);
    }

    if max_amount_recv > op.send_max {
        return Ok(make_strict_receive_result(
            PathPaymentStrictReceiveResultCode::OverSendmax,
            None,
        ));
    }

    if let Err(err) = update_source_balance(
        source,
        &op.send_asset,
        max_amount_recv,
        bypass_issuer_check,
        state,
        context,
    ) {
        return Ok(make_strict_receive_result_with_asset(
            err.code,
            err.no_issuer_asset,
            None,
        ));
    }

    success.offers = offers_claimed.try_into().unwrap();
    Ok(make_strict_receive_result(
        PathPaymentStrictReceiveResultCode::Success,
        Some(success),
    ))
}

/// Execute a PathPaymentStrictSend operation.
///
/// This operation sends exactly `send_amount` of `send_asset` to receive at least
/// `dest_min` of `dest_asset` at the destination.
pub(crate) fn execute_path_payment_strict_send(
    op: &PathPaymentStrictSendOp,
    source: &AccountId,
    state: &mut LedgerStateManager,
    context: &LedgerContext,
) -> Result<OperationResult> {
    let dest = muxed_to_account_id(&op.destination);

    // Validate amounts
    if op.send_amount <= 0 || op.dest_min <= 0 {
        return Ok(make_strict_send_result(
            PathPaymentStrictSendResultCode::Malformed,
            None,
        ));
    }

    let bypass_issuer_check =
        should_bypass_issuer_check(op.path.as_slice(), &op.send_asset, &op.dest_asset, &dest);
    if !bypass_issuer_check && state.get_account(&dest).is_none() {
        return Ok(make_strict_send_result(
            PathPaymentStrictSendResultCode::NoDestination,
            None,
        ));
    }

    if let Err(err) = update_source_balance(
        source,
        &op.send_asset,
        op.send_amount,
        bypass_issuer_check,
        state,
        context,
    ) {
        return Ok(make_strict_send_result_with_asset(
            convert_receive_to_send_code(err.code),
            err.no_issuer_asset,
            None,
        ));
    }

    let mut offers_claimed: Vec<ClaimAtom> = Vec::new();
    let mut full_path: Vec<Asset> = op.path.iter().cloned().collect();
    full_path.push(op.dest_asset.clone());

    let mut send_asset = op.send_asset.clone();
    let mut max_amount_send = op.send_amount;
    for recv_asset in full_path {
        if recv_asset == send_asset {
            continue;
        }

        if !bypass_issuer_check {
            if let Err(err) = check_issuer(&recv_asset, state, context) {
                return Ok(make_strict_send_result_with_asset(
                    convert_receive_to_send_code(err.code),
                    err.no_issuer_asset,
                    None,
                ));
            }
        }

        let mut amount_send = 0;
        let mut amount_recv = 0;
        let mut offer_trail = Vec::new();
        let max_offers_to_cross = MAX_OFFERS_TO_CROSS - offers_claimed.len() as i64;
        let convert_res = convert_with_offers_and_pools(
            &mut ConversionParams {
                source,
                selling: &send_asset,
                buying: &recv_asset,
                max_send: max_amount_send,
                max_receive: i64::MAX,
                round: RoundingType::PathPaymentStrictSend,
                offer_trail: &mut offer_trail,
                state,
                context,
                frozen_key_config: &context.frozen_key_config,
            },
            &mut amount_send,
            &mut amount_recv,
            max_offers_to_cross,
        )?;

        if convert_res == ConvertResult::FilterStopCrossSelf {
            return Ok(make_strict_send_result(
                PathPaymentStrictSendResultCode::OfferCrossSelf,
                None,
            ));
        }
        if convert_res == ConvertResult::CrossedTooMany {
            return Ok(OperationResult::OpExceededWorkLimit);
        }
        if convert_res != ConvertResult::Ok || amount_send != max_amount_send {
            return Ok(make_strict_send_result(
                PathPaymentStrictSendResultCode::TooFewOffers,
                None,
            ));
        }

        max_amount_send = amount_recv;
        send_asset = recv_asset;

        offers_claimed.extend(offer_trail);
    }

    if max_amount_send < op.dest_min {
        return Ok(make_strict_send_result(
            PathPaymentStrictSendResultCode::UnderDestmin,
            None,
        ));
    }

    if let Err(err) = update_dest_balance(
        &dest,
        &op.dest_asset,
        max_amount_send,
        bypass_issuer_check,
        state,
        context,
    ) {
        return Ok(make_strict_send_result_with_asset(
            convert_receive_to_send_code(err.code),
            err.no_issuer_asset,
            None,
        ));
    }

    let success = PathPaymentStrictSendResultSuccess {
        offers: offers_claimed.try_into().unwrap(),
        last: SimplePaymentResult {
            destination: dest,
            asset: op.dest_asset.clone(),
            amount: max_amount_send,
        },
    };
    Ok(make_strict_send_result(
        PathPaymentStrictSendResultCode::Success,
        Some(success),
    ))
}

/// Execute an asset transfer from source to destination.
struct TransferError {
    code: PathPaymentStrictReceiveResultCode,
    no_issuer_asset: Option<Asset>,
}

fn check_issuer(
    _asset: &Asset,
    _state: &LedgerStateManager,
    _context: &LedgerContext,
) -> std::result::Result<(), TransferError> {
    // In protocol 13+, issuer checks were removed (CAP-0017)
    Ok(())
}

fn should_bypass_issuer_check(
    path: &[Asset],
    send_asset: &Asset,
    dest_asset: &Asset,
    dest: &AccountId,
) -> bool {
    !matches!(dest_asset, Asset::Native)
        && path.is_empty()
        && send_asset == dest_asset
        && issuer_for_asset(dest_asset) == Some(dest)
}

fn update_source_balance(
    source: &AccountId,
    asset: &Asset,
    amount: i64,
    bypass_issuer_check: bool,
    state: &mut LedgerStateManager,
    context: &LedgerContext,
) -> std::result::Result<(), TransferError> {
    if matches!(asset, Asset::Native) {
        let source_account = state.get_account(source).ok_or(TransferError {
            code: PathPaymentStrictReceiveResultCode::Underfunded,
            no_issuer_asset: None,
        })?;
        let min_balance = state
            .minimum_balance_for_account(source_account, context.protocol_version, 0)
            .map_err(|_| TransferError {
                code: PathPaymentStrictReceiveResultCode::Malformed,
                no_issuer_asset: None,
            })?;
        let available =
            source_account.balance - min_balance - account_liabilities(source_account).selling;
        if available < amount {
            return Err(TransferError {
                code: PathPaymentStrictReceiveResultCode::Underfunded,
                no_issuer_asset: None,
            });
        }
        let source_account_mut = state.get_account_mut(source).ok_or(TransferError {
            code: PathPaymentStrictReceiveResultCode::Underfunded,
            no_issuer_asset: None,
        })?;
        source_account_mut.balance -= amount;
        return Ok(());
    }

    if !bypass_issuer_check {
        check_issuer(asset, state, context)?;
    }

    if issuer_for_asset(asset) == Some(source) {
        return Ok(());
    }

    let source_trustline = state.get_trustline(source, asset).ok_or(TransferError {
        code: PathPaymentStrictReceiveResultCode::SrcNoTrust,
        no_issuer_asset: None,
    })?;
    // Check source is authorized - this is unconditional (not dependent on issuer's auth_required)
    // The AUTH_REQUIRED flag on issuer only affects whether NEW trustlines start authorized,
    // but once a trustline exists, its AUTHORIZED flag controls whether it can send.
    if !is_trustline_authorized(source_trustline.flags) {
        return Err(TransferError {
            code: PathPaymentStrictReceiveResultCode::SrcNotAuthorized,
            no_issuer_asset: None,
        });
    }
    let available = source_trustline.balance - trustline_liabilities(source_trustline).selling;
    if available < amount {
        return Err(TransferError {
            code: PathPaymentStrictReceiveResultCode::Underfunded,
            no_issuer_asset: None,
        });
    }

    let source_trustline_mut = state
        .get_trustline_mut(source, asset)
        .ok_or(TransferError {
            code: PathPaymentStrictReceiveResultCode::SrcNoTrust,
            no_issuer_asset: None,
        })?;
    source_trustline_mut.balance -= amount;
    Ok(())
}

fn update_dest_balance(
    dest: &AccountId,
    asset: &Asset,
    amount: i64,
    bypass_issuer_check: bool,
    state: &mut LedgerStateManager,
    context: &LedgerContext,
) -> std::result::Result<(), TransferError> {
    if matches!(asset, Asset::Native) {
        let dest_account_mut = state.get_account_mut(dest).ok_or(TransferError {
            code: PathPaymentStrictReceiveResultCode::NoDestination,
            no_issuer_asset: None,
        })?;
        if !add_account_balance(dest_account_mut, amount) {
            return Err(TransferError {
                code: PathPaymentStrictReceiveResultCode::LineFull,
                no_issuer_asset: None,
            });
        }
        return Ok(());
    }

    if !bypass_issuer_check {
        check_issuer(asset, state, context)?;
    }

    if issuer_for_asset(asset) == Some(dest) {
        return Ok(());
    }

    let dest_trustline = state.get_trustline(dest, asset).ok_or(TransferError {
        code: PathPaymentStrictReceiveResultCode::NoTrust,
        no_issuer_asset: None,
    })?;
    // Check destination is authorized - this is unconditional (not dependent on issuer's auth_required)
    if !is_trustline_authorized(dest_trustline.flags) {
        return Err(TransferError {
            code: PathPaymentStrictReceiveResultCode::NotAuthorized,
            no_issuer_asset: None,
        });
    }
    let dest_trustline_mut = state.get_trustline_mut(dest, asset).ok_or(TransferError {
        code: PathPaymentStrictReceiveResultCode::NoTrust,
        no_issuer_asset: None,
    })?;
    if !add_trustline_balance(dest_trustline_mut, amount) {
        return Err(TransferError {
            code: PathPaymentStrictReceiveResultCode::LineFull,
            no_issuer_asset: None,
        });
    }
    Ok(())
}

/// Maximum number of offers that can be crossed per path payment operation.
/// Matches stellar-core's MAX_OFFERS_TO_CROSS (protocol 11+).
const MAX_OFFERS_TO_CROSS: i64 = 1000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ConvertResult {
    Ok,
    Partial,
    FilterStopCrossSelf,
    CrossedTooMany,
}

fn convert_with_offers_and_pools(
    params: &mut ConversionParams<'_>,
    amount_send: &mut i64,
    amount_recv: &mut i64,
    max_offers_to_cross: i64,
) -> Result<ConvertResult> {
    if params.round == RoundingType::Normal {
        return convert_with_offers(params, amount_send, amount_recv, max_offers_to_cross);
    }

    let pool_exchange = compute_pool_exchange(
        params.selling,
        params.max_send,
        params.buying,
        params.max_receive,
        params.round,
        params.state,
    )?;

    if pool_exchange.is_none() {
        return convert_with_offers(params, amount_send, amount_recv, max_offers_to_cross);
    }

    // stellar-core fast-fails if maxOffersToCross == 0 and pool exchange would add to trail
    // (protocol 18+, but we only support 23+)
    if max_offers_to_cross == 0 {
        return Ok(ConvertResult::CrossedTooMany);
    }

    let mut book_amount_send = 0;
    let mut book_amount_recv = 0;
    let mut book_offer_trail = Vec::new();
    // Use savepoint instead of cloning entire state (avoids O(n) clone of 911K+ offers).
    // Run the orderbook path speculatively on the real state, and rollback if pool wins.
    let t_sp = std::time::Instant::now();
    let savepoint = params.state.create_savepoint();
    let savepoint_us = t_sp.elapsed().as_micros() as u64;
    let book_res = convert_with_offers(
        &mut ConversionParams {
            source: params.source,
            selling: params.selling,
            buying: params.buying,
            max_send: params.max_send,
            max_receive: params.max_receive,
            round: params.round,
            offer_trail: &mut book_offer_trail,
            state: params.state,
            context: params.context,
            frozen_key_config: params.frozen_key_config,
        },
        &mut book_amount_send,
        &mut book_amount_recv,
        max_offers_to_cross,
    )?;

    let pool_exchange = pool_exchange.unwrap();
    let use_book = match book_res {
        ConvertResult::Ok => {
            let lhs = (pool_exchange.send as i128) * (book_amount_recv as i128);
            let rhs = (pool_exchange.recv as i128) * (book_amount_send as i128);
            lhs > rhs
        }
        _ => false,
    };

    if use_book {
        // Book wins — keep speculative changes (drop savepoint)
        if savepoint_us > 100 {
            tracing::debug!(
                savepoint_us,
                rollback = false,
                "savepoint profiling (book wins)"
            );
        }
        *amount_send = book_amount_send;
        *amount_recv = book_amount_recv;
        params.offer_trail.clear();
        params.offer_trail.extend(book_offer_trail);
        return Ok(book_res);
    }

    // Pool wins — undo speculative book changes
    let t_rb = std::time::Instant::now();
    params.state.rollback_to_savepoint(savepoint);
    let rollback_us = t_rb.elapsed().as_micros() as u64;
    if savepoint_us > 100 || rollback_us > 100 {
        tracing::debug!(savepoint_us, rollback_us, "savepoint profiling (pool wins)");
    }

    params.offer_trail.clear();
    if apply_pool_exchange(
        params.selling,
        params.buying,
        pool_exchange.send,
        pool_exchange.recv,
        params.state,
    )? {
        *amount_send = pool_exchange.send;
        *amount_recv = pool_exchange.recv;
        params
            .offer_trail
            .push(ClaimAtom::LiquidityPool(ClaimLiquidityAtom {
                liquidity_pool_id: pool_exchange.pool_id,
                asset_sold: params.buying.clone(),
                amount_sold: pool_exchange.recv,
                asset_bought: params.selling.clone(),
                amount_bought: pool_exchange.send,
            }));
        return Ok(ConvertResult::Ok);
    }

    convert_with_offers(params, amount_send, amount_recv, max_offers_to_cross)
}

fn convert_with_offers(
    params: &mut ConversionParams<'_>,
    amount_send: &mut i64,
    amount_recv: &mut i64,
    max_offers_to_cross: i64,
) -> Result<ConvertResult> {
    params.offer_trail.clear();
    *amount_send = 0;
    *amount_recv = 0;

    let mut max_send = params.max_send;
    let mut max_recv = params.max_receive;
    let mut need_more = max_send > 0 && max_recv > 0;

    // Fast-fail if already at the limit (protocol 18+, but we only support 23+)
    if need_more && max_offers_to_cross == 0 {
        return Ok(ConvertResult::CrossedTooMany);
    }

    // Micro-profiling accumulators
    let loop_start = std::time::Instant::now();
    let mut best_offer_us = 0u64;
    let mut cross_offer_us = 0u64;
    let mut offers_crossed = 0u32;

    while need_more {
        let t0 = std::time::Instant::now();
        let offer = params.state.best_offer(params.selling, params.buying);
        best_offer_us += t0.elapsed().as_micros() as u64;
        let Some(offer) = offer else {
            break;
        };

        if offer.seller_id == *params.source {
            return Ok(ConvertResult::FilterStopCrossSelf);
        }

        // CAP-77: Skip and delete offers that access frozen keys.
        // Parity: stellar-core checks frozen AFTER cross-self filter.
        // When an offer is both frozen and cross-self, cross-self takes priority.
        if offer_accesses_frozen_key(&offer, params.frozen_key_config) {
            params.state.ensure_offer_entries_loaded(
                &offer.seller_id,
                &offer.selling,
                &offer.buying,
            )?;
            let (selling_liab, buying_liab) = offer_liabilities_sell(offer.amount, &offer.price)?;
            apply_liabilities_delta(
                &offer.seller_id,
                &offer.selling,
                &offer.buying,
                -selling_liab,
                -buying_liab,
                params.state,
            )?;
            delete_offer_with_sponsorship(&offer.seller_id, offer.offer_id, params.state)?;
            continue;
        }

        // Enforce max offers to cross limit (matches stellar-core)
        if params.offer_trail.len() as i64 >= max_offers_to_cross {
            return Ok(ConvertResult::CrossedTooMany);
        }

        let t1 = std::time::Instant::now();
        let (recv, send, wheat_stays) = super::offer_utils::cross_offer_v10(
            &offer,
            max_recv,
            max_send,
            params.round,
            params.offer_trail,
            params.state,
            params.context,
        )?;
        cross_offer_us += t1.elapsed().as_micros() as u64;
        offers_crossed += 1;

        // Update totals and remaining limits
        *amount_send += send;
        *amount_recv += recv;
        max_send -= send;
        max_recv -= recv;

        // stellar-core logic for continuing the loop:
        // needMore = !wheatStays && (maxWheatReceive > 0 && maxSheepSend > 0)
        need_more = !wheat_stays && max_send > 0 && max_recv > 0;
    }

    let total_us = loop_start.elapsed().as_micros() as u64;
    if total_us > 1000 {
        tracing::trace!(
            offers_crossed,
            total_us,
            best_offer_us,
            cross_offer_us,
            "convert_with_offers profiling"
        );
    }

    Ok(if need_more {
        ConvertResult::Partial
    } else {
        ConvertResult::Ok
    })
}

struct PoolExchange {
    pool_id: PoolId,
    send: i64,
    recv: i64,
}

fn compute_pool_exchange(
    send_asset: &Asset,
    max_send: i64,
    recv_asset: &Asset,
    max_recv: i64,
    round: RoundingType,
    state: &LedgerStateManager,
) -> Result<Option<PoolExchange>> {
    let pool_id = pool_id_for_assets(send_asset, recv_asset)?;
    let Some(pool) = state.get_liquidity_pool(&pool_id) else {
        return Ok(None);
    };
    let LiquidityPoolEntryBody::LiquidityPoolConstantProduct(cp) = &pool.body;

    if cp.reserve_a <= 0 || cp.reserve_b <= 0 {
        return Ok(None);
    }

    let fee_bps = LIQUIDITY_POOL_FEE_V18 as i64;
    let (reserves_to, reserves_from, max_send, max_recv) =
        if send_asset == &cp.params.asset_a && recv_asset == &cp.params.asset_b {
            (cp.reserve_a, cp.reserve_b, max_send, max_recv)
        } else if send_asset == &cp.params.asset_b && recv_asset == &cp.params.asset_a {
            (cp.reserve_b, cp.reserve_a, max_send, max_recv)
        } else {
            return Ok(None);
        };

    let mut to_pool = 0i64;
    let mut from_pool = 0i64;
    let ok = exchange_with_pool(PoolExchangeRequest {
        reserves_to_pool: reserves_to,
        max_send_to_pool: max_send,
        to_pool: &mut to_pool,
        reserves_from_pool: reserves_from,
        max_receive_from_pool: max_recv,
        from_pool: &mut from_pool,
        fee_bps,
        round,
    })?;
    if !ok {
        return Ok(None);
    }

    Ok(Some(PoolExchange {
        pool_id,
        send: to_pool,
        recv: from_pool,
    }))
}

fn apply_pool_exchange(
    send_asset: &Asset,
    recv_asset: &Asset,
    to_pool: i64,
    from_pool: i64,
    state: &mut LedgerStateManager,
) -> Result<bool> {
    let pool_id = pool_id_for_assets(send_asset, recv_asset)?;
    let Some(pool) = state.get_liquidity_pool_mut(&pool_id) else {
        return Ok(false);
    };
    let LiquidityPoolEntryBody::LiquidityPoolConstantProduct(cp) = &mut pool.body;

    if send_asset == &cp.params.asset_a && recv_asset == &cp.params.asset_b {
        cp.reserve_a = cp
            .reserve_a
            .checked_add(to_pool)
            .ok_or_else(|| TxError::Internal("pool reserve overflow".into()))?;
        cp.reserve_b = cp
            .reserve_b
            .checked_sub(from_pool)
            .ok_or_else(|| TxError::Internal("pool reserve underflow".into()))?;
    } else if send_asset == &cp.params.asset_b && recv_asset == &cp.params.asset_a {
        cp.reserve_b = cp
            .reserve_b
            .checked_add(to_pool)
            .ok_or_else(|| TxError::Internal("pool reserve overflow".into()))?;
        cp.reserve_a = cp
            .reserve_a
            .checked_sub(from_pool)
            .ok_or_else(|| TxError::Internal("pool reserve underflow".into()))?;
    } else {
        return Ok(false);
    }

    Ok(true)
}

fn pool_id_for_assets(send_asset: &Asset, recv_asset: &Asset) -> Result<PoolId> {
    let params = LiquidityPoolParameters::LiquidityPoolConstantProduct(
        stellar_xdr::curr::LiquidityPoolConstantProductParameters {
            asset_a: std::cmp::min(send_asset.clone(), recv_asset.clone()),
            asset_b: std::cmp::max(send_asset.clone(), recv_asset.clone()),
            fee: LIQUIDITY_POOL_FEE_V18 as i32,
        },
    );
    let xdr = params.to_xdr(Limits::none())?;
    let mut hasher = Sha256::new();
    hasher.update(&xdr);
    Ok(PoolId(Hash(hasher.finalize().into())))
}

struct PoolExchangeRequest<'a> {
    reserves_to_pool: i64,
    max_send_to_pool: i64,
    to_pool: &'a mut i64,
    reserves_from_pool: i64,
    max_receive_from_pool: i64,
    from_pool: &'a mut i64,
    fee_bps: i64,
    round: RoundingType,
}

fn exchange_with_pool(request: PoolExchangeRequest<'_>) -> Result<bool> {
    let PoolExchangeRequest {
        reserves_to_pool,
        max_send_to_pool,
        to_pool,
        reserves_from_pool,
        max_receive_from_pool,
        from_pool,
        fee_bps,
        round,
    } = request;
    const MAX_BPS: i64 = 10_000;
    if !(0..MAX_BPS).contains(&fee_bps) {
        return Err(TxError::Internal("pool fee out of range".into()));
    }
    if reserves_to_pool <= 0 || reserves_from_pool <= 0 {
        return Err(TxError::Internal("non-positive pool reserves".into()));
    }

    match round {
        RoundingType::PathPaymentStrictSend => {
            if max_receive_from_pool != i64::MAX {
                return Err(TxError::Internal("strict send with bounded receive".into()));
            }
            let max_receive_from_pool = reserves_from_pool;

            if max_send_to_pool > i64::MAX - reserves_to_pool {
                return Ok(false);
            }
            *to_pool = max_send_to_pool;

            let denominator = u128::from(MAX_BPS as u64) * u128::from(reserves_to_pool as u64)
                + u128::from((MAX_BPS - fee_bps) as u64) * u128::from(*to_pool as u64);

            // Use hugeDivide algorithm matching stellar-core exactly:
            // result = floor(A * B / C) where A = (MAX_BPS - fee_bps), B and C are u128
            // stellar-core computes: A*Q + floor(A*R/C) where Q = B/C, R = B%C
            // This avoids overflow that would occur computing A*B directly in u128.
            let a = (MAX_BPS - fee_bps) as u128;
            let b = u128::from(reserves_from_pool as u64) * u128::from(*to_pool as u64);
            let c = denominator;

            if c == 0 {
                return Ok(false);
            }

            let q = b / c;
            let r = b % c;

            // result = A*Q + floor(A*R/C)
            let value = a * q + (a * r) / c;
            if value > i64::MAX as u128 {
                // stellar-core hugeDivide returns false on overflow; exchange is not possible
                return Ok(false);
            }
            *from_pool = value as i64;

            if *from_pool > max_receive_from_pool || *from_pool == 0 {
                return Ok(false);
            }
            Ok(true)
        }
        RoundingType::PathPaymentStrictReceive => {
            if max_send_to_pool != i64::MAX {
                return Err(TxError::Internal("strict receive with bounded send".into()));
            }
            let max_send_to_pool = i64::MAX - reserves_to_pool;
            if max_receive_from_pool >= reserves_from_pool {
                return Ok(false);
            }
            *from_pool = max_receive_from_pool;

            // Use hugeDivide algorithm matching stellar-core exactly:
            // result = ceil(A * B / C) where A = MAX_BPS (int32), B and C are u128
            // stellar-core computes: A*Q + ceil(A*R/C) where Q = B/C, R = B%C
            let a = MAX_BPS as u128;
            let b = u128::from(reserves_to_pool as u64) * u128::from(*from_pool as u64);
            let c = u128::from((reserves_from_pool - *from_pool) as u64)
                * u128::from((MAX_BPS - fee_bps) as u64);

            if c == 0 {
                return Ok(false);
            }

            let q = b / c;
            let r = b % c;

            // result = A*Q + ceil(A*R/C)
            let value = a * q + (a * r).div_ceil(c);

            if value > i64::MAX as u128 {
                // stellar-core hugeDivide returns false on overflow; exchange is not possible
                return Ok(false);
            }
            *to_pool = value as i64;

            Ok(*to_pool <= max_send_to_pool)
        }
        RoundingType::Normal => Ok(false),
    }
}

/// Convert PathPaymentStrictReceiveResultCode to PathPaymentStrictSendResultCode.
fn convert_receive_to_send_code(
    code: PathPaymentStrictReceiveResultCode,
) -> PathPaymentStrictSendResultCode {
    match code {
        PathPaymentStrictReceiveResultCode::Success => PathPaymentStrictSendResultCode::Success,
        PathPaymentStrictReceiveResultCode::Malformed => PathPaymentStrictSendResultCode::Malformed,
        PathPaymentStrictReceiveResultCode::Underfunded => {
            PathPaymentStrictSendResultCode::Underfunded
        }
        PathPaymentStrictReceiveResultCode::SrcNoTrust => {
            PathPaymentStrictSendResultCode::SrcNoTrust
        }
        PathPaymentStrictReceiveResultCode::SrcNotAuthorized => {
            PathPaymentStrictSendResultCode::SrcNotAuthorized
        }
        PathPaymentStrictReceiveResultCode::NoDestination => {
            PathPaymentStrictSendResultCode::NoDestination
        }
        PathPaymentStrictReceiveResultCode::NoTrust => PathPaymentStrictSendResultCode::NoTrust,
        PathPaymentStrictReceiveResultCode::NotAuthorized => {
            PathPaymentStrictSendResultCode::NotAuthorized
        }
        PathPaymentStrictReceiveResultCode::LineFull => PathPaymentStrictSendResultCode::LineFull,
        PathPaymentStrictReceiveResultCode::NoIssuer => PathPaymentStrictSendResultCode::NoIssuer,
        PathPaymentStrictReceiveResultCode::TooFewOffers => {
            PathPaymentStrictSendResultCode::TooFewOffers
        }
        PathPaymentStrictReceiveResultCode::OfferCrossSelf => {
            PathPaymentStrictSendResultCode::OfferCrossSelf
        }
        PathPaymentStrictReceiveResultCode::OverSendmax => {
            // No direct equivalent, use TooFewOffers
            PathPaymentStrictSendResultCode::TooFewOffers
        }
    }
}

/// Create a PathPaymentStrictReceive result.
fn make_strict_receive_result(
    code: PathPaymentStrictReceiveResultCode,
    success: Option<PathPaymentStrictReceiveResultSuccess>,
) -> OperationResult {
    make_strict_receive_result_with_asset(code, None, success)
}

fn make_strict_receive_result_with_asset(
    code: PathPaymentStrictReceiveResultCode,
    no_issuer_asset: Option<Asset>,
    success: Option<PathPaymentStrictReceiveResultSuccess>,
) -> OperationResult {
    let result = match code {
        PathPaymentStrictReceiveResultCode::Success => {
            PathPaymentStrictReceiveResult::Success(success.unwrap())
        }
        PathPaymentStrictReceiveResultCode::Malformed => PathPaymentStrictReceiveResult::Malformed,
        PathPaymentStrictReceiveResultCode::Underfunded => {
            PathPaymentStrictReceiveResult::Underfunded
        }
        PathPaymentStrictReceiveResultCode::SrcNoTrust => {
            PathPaymentStrictReceiveResult::SrcNoTrust
        }
        PathPaymentStrictReceiveResultCode::SrcNotAuthorized => {
            PathPaymentStrictReceiveResult::SrcNotAuthorized
        }
        PathPaymentStrictReceiveResultCode::NoDestination => {
            PathPaymentStrictReceiveResult::NoDestination
        }
        PathPaymentStrictReceiveResultCode::NoTrust => PathPaymentStrictReceiveResult::NoTrust,
        PathPaymentStrictReceiveResultCode::NotAuthorized => {
            PathPaymentStrictReceiveResult::NotAuthorized
        }
        PathPaymentStrictReceiveResultCode::LineFull => PathPaymentStrictReceiveResult::LineFull,
        PathPaymentStrictReceiveResultCode::NoIssuer => {
            // NoIssuer takes an Asset parameter
            PathPaymentStrictReceiveResult::NoIssuer(no_issuer_asset.unwrap_or(Asset::Native))
        }
        PathPaymentStrictReceiveResultCode::TooFewOffers => {
            PathPaymentStrictReceiveResult::TooFewOffers
        }
        PathPaymentStrictReceiveResultCode::OfferCrossSelf => {
            PathPaymentStrictReceiveResult::OfferCrossSelf
        }
        PathPaymentStrictReceiveResultCode::OverSendmax => {
            PathPaymentStrictReceiveResult::OverSendmax
        }
    };

    OperationResult::OpInner(OperationResultTr::PathPaymentStrictReceive(result))
}

/// Create a PathPaymentStrictSend result.
fn make_strict_send_result(
    code: PathPaymentStrictSendResultCode,
    success: Option<PathPaymentStrictSendResultSuccess>,
) -> OperationResult {
    make_strict_send_result_with_asset(code, None, success)
}

fn make_strict_send_result_with_asset(
    code: PathPaymentStrictSendResultCode,
    no_issuer_asset: Option<Asset>,
    success: Option<PathPaymentStrictSendResultSuccess>,
) -> OperationResult {
    let result = match code {
        PathPaymentStrictSendResultCode::Success => {
            PathPaymentStrictSendResult::Success(success.unwrap())
        }
        PathPaymentStrictSendResultCode::Malformed => PathPaymentStrictSendResult::Malformed,
        PathPaymentStrictSendResultCode::Underfunded => PathPaymentStrictSendResult::Underfunded,
        PathPaymentStrictSendResultCode::SrcNoTrust => PathPaymentStrictSendResult::SrcNoTrust,
        PathPaymentStrictSendResultCode::SrcNotAuthorized => {
            PathPaymentStrictSendResult::SrcNotAuthorized
        }
        PathPaymentStrictSendResultCode::NoDestination => {
            PathPaymentStrictSendResult::NoDestination
        }
        PathPaymentStrictSendResultCode::NoTrust => PathPaymentStrictSendResult::NoTrust,
        PathPaymentStrictSendResultCode::NotAuthorized => {
            PathPaymentStrictSendResult::NotAuthorized
        }
        PathPaymentStrictSendResultCode::LineFull => PathPaymentStrictSendResult::LineFull,
        PathPaymentStrictSendResultCode::NoIssuer => {
            PathPaymentStrictSendResult::NoIssuer(no_issuer_asset.unwrap_or(Asset::Native))
        }
        PathPaymentStrictSendResultCode::TooFewOffers => PathPaymentStrictSendResult::TooFewOffers,
        PathPaymentStrictSendResultCode::OfferCrossSelf => {
            PathPaymentStrictSendResult::OfferCrossSelf
        }
        PathPaymentStrictSendResultCode::UnderDestmin => PathPaymentStrictSendResult::UnderDestmin,
    };

    OperationResult::OpInner(OperationResultTr::PathPaymentStrictSend(result))
}

#[cfg(test)]
mod tests {
    use super::super::AUTHORIZED_FLAG;
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

    fn create_test_context() -> LedgerContext {
        LedgerContext::testnet(1, 1000)
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

    fn create_test_trustline_with_liabilities(
        account_id: AccountId,
        asset: TrustLineAsset,
        balance: i64,
        limit: i64,
        flags: u32,
        buying_liab: i64,
        selling_liab: i64,
    ) -> TrustLineEntry {
        TrustLineEntry {
            account_id,
            asset,
            balance,
            limit,
            flags,
            ext: TrustLineEntryExt::V1(TrustLineEntryV1 {
                liabilities: Liabilities {
                    buying: buying_liab,
                    selling: selling_liab,
                },
                ext: TrustLineEntryV1Ext::V0,
            }),
        }
    }

    fn create_asset(issuer: &AccountId) -> Asset {
        Asset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4([b'U', b'S', b'D', b'C']),
            issuer: issuer.clone(),
        })
    }

    #[test]
    fn test_path_payment_strict_receive_native() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(0);
        let dest_id = create_test_account_id(1);

        state.create_account(create_test_account(source_id.clone(), 100_000_000));
        state.create_account(create_test_account(dest_id.clone(), 50_000_000));

        let op = PathPaymentStrictReceiveOp {
            send_asset: Asset::Native,
            send_max: 20_000_000,
            destination: create_test_muxed_account(1),
            dest_asset: Asset::Native,
            dest_amount: 10_000_000,
            path: vec![].try_into().unwrap(),
        };

        let result = execute_path_payment_strict_receive(&op, &source_id, &mut state, &context);
        assert!(result.is_ok());

        // Verify balances
        assert_eq!(state.get_account(&source_id).unwrap().balance, 90_000_000);
        assert_eq!(state.get_account(&dest_id).unwrap().balance, 60_000_000);
    }

    #[test]
    fn test_path_payment_strict_send_native() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(0);
        let dest_id = create_test_account_id(1);

        state.create_account(create_test_account(source_id.clone(), 100_000_000));
        state.create_account(create_test_account(dest_id.clone(), 50_000_000));

        let op = PathPaymentStrictSendOp {
            send_asset: Asset::Native,
            send_amount: 10_000_000,
            destination: create_test_muxed_account(1),
            dest_asset: Asset::Native,
            dest_min: 5_000_000,
            path: vec![].try_into().unwrap(),
        };

        let result = execute_path_payment_strict_send(&op, &source_id, &mut state, &context);
        assert!(result.is_ok());

        // Verify balances
        assert_eq!(state.get_account(&source_id).unwrap().balance, 90_000_000);
        assert_eq!(state.get_account(&dest_id).unwrap().balance, 60_000_000);
    }

    #[test]
    fn test_path_payment_no_destination() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(0);
        state.create_account(create_test_account(source_id.clone(), 100_000_000));

        let op = PathPaymentStrictReceiveOp {
            send_asset: Asset::Native,
            send_max: 20_000_000,
            destination: create_test_muxed_account(1), // Non-existent
            dest_asset: Asset::Native,
            dest_amount: 10_000_000,
            path: vec![].try_into().unwrap(),
        };

        let result = execute_path_payment_strict_receive(&op, &source_id, &mut state, &context);
        assert!(result.is_ok());

        match result.unwrap() {
            OperationResult::OpInner(OperationResultTr::PathPaymentStrictReceive(r)) => {
                assert!(matches!(r, PathPaymentStrictReceiveResult::NoDestination));
            }
            _ => panic!("Unexpected result type"),
        }
    }

    #[test]
    fn test_path_payment_credit_src_not_authorized() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let issuer_id = create_test_account_id(9);
        let source_id = create_test_account_id(0);
        let dest_id = create_test_account_id(1);
        state.create_account(create_test_account(issuer_id.clone(), 100_000_000));
        state.create_account(create_test_account(source_id.clone(), 100_000_000));
        state.create_account(create_test_account(dest_id.clone(), 100_000_000));
        state.get_account_mut(&issuer_id).unwrap().flags = AccountFlags::RequiredFlag as u32;

        let asset = create_asset(&issuer_id);
        state.create_trustline(create_test_trustline(
            source_id.clone(),
            TrustLineAsset::CreditAlphanum4(AlphaNum4 {
                asset_code: AssetCode4([b'U', b'S', b'D', b'C']),
                issuer: issuer_id.clone(),
            }),
            100,
            1_000_000,
            0,
        ));
        state.create_trustline(create_test_trustline(
            dest_id.clone(),
            TrustLineAsset::CreditAlphanum4(AlphaNum4 {
                asset_code: AssetCode4([b'U', b'S', b'D', b'C']),
                issuer: issuer_id.clone(),
            }),
            0,
            1_000_000,
            AUTHORIZED_FLAG,
        ));

        let op = PathPaymentStrictReceiveOp {
            send_asset: asset.clone(),
            send_max: 100,
            destination: create_test_muxed_account(1),
            dest_asset: asset,
            dest_amount: 10,
            path: vec![].try_into().unwrap(),
        };

        let result =
            execute_path_payment_strict_receive(&op, &source_id, &mut state, &context).unwrap();
        match result {
            OperationResult::OpInner(OperationResultTr::PathPaymentStrictReceive(r)) => {
                assert!(matches!(
                    r,
                    PathPaymentStrictReceiveResult::SrcNotAuthorized
                ));
            }
            _ => panic!("Unexpected result type"),
        }
    }

    #[test]
    fn test_path_payment_credit_not_authorized_dest() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let issuer_id = create_test_account_id(9);
        let source_id = create_test_account_id(0);
        let dest_id = create_test_account_id(1);
        state.create_account(create_test_account(issuer_id.clone(), 100_000_000));
        state.create_account(create_test_account(source_id.clone(), 100_000_000));
        state.create_account(create_test_account(dest_id.clone(), 100_000_000));
        state.get_account_mut(&issuer_id).unwrap().flags = AccountFlags::RequiredFlag as u32;

        let asset = create_asset(&issuer_id);
        state.create_trustline(create_test_trustline(
            source_id.clone(),
            TrustLineAsset::CreditAlphanum4(AlphaNum4 {
                asset_code: AssetCode4([b'U', b'S', b'D', b'C']),
                issuer: issuer_id.clone(),
            }),
            100,
            1_000_000,
            AUTHORIZED_FLAG,
        ));
        state.create_trustline(create_test_trustline(
            dest_id.clone(),
            TrustLineAsset::CreditAlphanum4(AlphaNum4 {
                asset_code: AssetCode4([b'U', b'S', b'D', b'C']),
                issuer: issuer_id.clone(),
            }),
            0,
            1_000_000,
            0,
        ));

        let op = PathPaymentStrictReceiveOp {
            send_asset: asset.clone(),
            send_max: 100,
            destination: create_test_muxed_account(1),
            dest_asset: asset,
            dest_amount: 10,
            path: vec![].try_into().unwrap(),
        };

        let result =
            execute_path_payment_strict_receive(&op, &source_id, &mut state, &context).unwrap();
        match result {
            OperationResult::OpInner(OperationResultTr::PathPaymentStrictReceive(r)) => {
                assert!(matches!(r, PathPaymentStrictReceiveResult::NotAuthorized));
            }
            _ => panic!("Unexpected result type"),
        }
    }

    #[test]
    fn test_path_payment_credit_line_full_with_liabilities() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let issuer_id = create_test_account_id(9);
        let source_id = create_test_account_id(0);
        let dest_id = create_test_account_id(1);
        state.create_account(create_test_account(issuer_id.clone(), 100_000_000));
        state.create_account(create_test_account(source_id.clone(), 100_000_000));
        state.create_account(create_test_account(dest_id.clone(), 100_000_000));

        let asset = create_asset(&issuer_id);
        state.create_trustline(create_test_trustline(
            source_id.clone(),
            TrustLineAsset::CreditAlphanum4(AlphaNum4 {
                asset_code: AssetCode4([b'U', b'S', b'D', b'C']),
                issuer: issuer_id.clone(),
            }),
            100,
            1_000_000,
            AUTHORIZED_FLAG,
        ));
        state.create_trustline(create_test_trustline_with_liabilities(
            dest_id.clone(),
            TrustLineAsset::CreditAlphanum4(AlphaNum4 {
                asset_code: AssetCode4([b'U', b'S', b'D', b'C']),
                issuer: issuer_id.clone(),
            }),
            90,
            100,
            AUTHORIZED_FLAG,
            10,
            0,
        ));

        let op = PathPaymentStrictReceiveOp {
            send_asset: asset.clone(),
            send_max: 100,
            destination: create_test_muxed_account(1),
            dest_asset: asset,
            dest_amount: 1,
            path: vec![].try_into().unwrap(),
        };

        let result =
            execute_path_payment_strict_receive(&op, &source_id, &mut state, &context).unwrap();
        match result {
            OperationResult::OpInner(OperationResultTr::PathPaymentStrictReceive(r)) => {
                assert!(matches!(r, PathPaymentStrictReceiveResult::LineFull));
            }
            _ => panic!("Unexpected result type"),
        }
    }

    /// Test PathPaymentStrictReceive with malformed negative amount.
    ///
    /// C++ Reference: PathPaymentTests.cpp - "malformed negative amount" test section
    #[test]
    fn test_path_payment_malformed_negative_amount() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(40);
        let dest_id = create_test_account_id(41);
        state.create_account(create_test_account(source_id.clone(), 100_000_000));
        state.create_account(create_test_account(dest_id.clone(), 100_000_000));

        let op = PathPaymentStrictReceiveOp {
            send_asset: Asset::Native,
            send_max: 100,
            destination: create_test_muxed_account(41),
            dest_asset: Asset::Native,
            dest_amount: -1, // Negative
            path: vec![].try_into().unwrap(),
        };

        let result =
            execute_path_payment_strict_receive(&op, &source_id, &mut state, &context).unwrap();
        match result {
            OperationResult::OpInner(OperationResultTr::PathPaymentStrictReceive(r)) => {
                assert!(
                    matches!(r, PathPaymentStrictReceiveResult::Malformed),
                    "Expected Malformed for negative amount, got {:?}",
                    r
                );
            }
            _ => panic!("Unexpected result type"),
        }
    }

    /// Test PathPaymentStrictSend with malformed negative amount.
    ///
    /// C++ Reference: PathPaymentStrictSendTests.cpp - "malformed negative amount" test section
    #[test]
    fn test_path_payment_strict_send_malformed_negative_amount() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(42);
        let dest_id = create_test_account_id(43);
        state.create_account(create_test_account(source_id.clone(), 100_000_000));
        state.create_account(create_test_account(dest_id.clone(), 100_000_000));

        let op = PathPaymentStrictSendOp {
            send_asset: Asset::Native,
            send_amount: -1, // Negative
            destination: create_test_muxed_account(43),
            dest_asset: Asset::Native,
            dest_min: 100,
            path: vec![].try_into().unwrap(),
        };

        let result =
            execute_path_payment_strict_send(&op, &source_id, &mut state, &context).unwrap();
        match result {
            OperationResult::OpInner(OperationResultTr::PathPaymentStrictSend(r)) => {
                assert!(
                    matches!(r, PathPaymentStrictSendResult::Malformed),
                    "Expected Malformed for negative amount, got {:?}",
                    r
                );
            }
            _ => panic!("Unexpected result type"),
        }
    }

    /// Test PathPaymentStrictReceive underfunded.
    ///
    /// C++ Reference: PathPaymentTests.cpp - "underfunded" test section
    #[test]
    fn test_path_payment_underfunded() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(44);
        let dest_id = create_test_account_id(45);

        // Source with minimum balance
        let min_balance = state
            .minimum_balance_with_counts(context.protocol_version, 0, 0, 0)
            .unwrap();
        state.create_account(create_test_account(source_id.clone(), min_balance));
        state.create_account(create_test_account(dest_id.clone(), 100_000_000));

        let op = PathPaymentStrictReceiveOp {
            send_asset: Asset::Native,
            send_max: 1000,
            destination: create_test_muxed_account(45),
            dest_asset: Asset::Native,
            dest_amount: 1000,
            path: vec![].try_into().unwrap(),
        };

        let result =
            execute_path_payment_strict_receive(&op, &source_id, &mut state, &context).unwrap();
        match result {
            OperationResult::OpInner(OperationResultTr::PathPaymentStrictReceive(r)) => {
                assert!(
                    matches!(r, PathPaymentStrictReceiveResult::Underfunded),
                    "Expected Underfunded, got {:?}",
                    r
                );
            }
            _ => panic!("Unexpected result type"),
        }
    }

    /// Test PathPaymentStrictReceive source no trust.
    ///
    /// C++ Reference: PathPaymentTests.cpp - "src no trust" test section
    #[test]
    fn test_path_payment_src_no_trust() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(46);
        let dest_id = create_test_account_id(47);
        let issuer_id = create_test_account_id(48);

        state.create_account(create_test_account(source_id.clone(), 100_000_000));
        state.create_account(create_test_account(dest_id.clone(), 100_000_000));
        state.create_account(create_test_account(issuer_id.clone(), 100_000_000));

        let asset = create_asset(&issuer_id);

        // Destination has trustline, but source doesn't
        state.create_trustline(create_test_trustline(
            dest_id.clone(),
            TrustLineAsset::CreditAlphanum4(AlphaNum4 {
                asset_code: AssetCode4([b'U', b'S', b'D', b'C']),
                issuer: issuer_id.clone(),
            }),
            0,
            1_000_000,
            AUTHORIZED_FLAG,
        ));

        let op = PathPaymentStrictReceiveOp {
            send_asset: asset.clone(),
            send_max: 100,
            destination: create_test_muxed_account(47),
            dest_asset: asset,
            dest_amount: 50,
            path: vec![].try_into().unwrap(),
        };

        let result =
            execute_path_payment_strict_receive(&op, &source_id, &mut state, &context).unwrap();
        match result {
            OperationResult::OpInner(OperationResultTr::PathPaymentStrictReceive(r)) => {
                assert!(
                    matches!(r, PathPaymentStrictReceiveResult::SrcNoTrust),
                    "Expected SrcNoTrust, got {:?}",
                    r
                );
            }
            _ => panic!("Unexpected result type"),
        }
    }

    /// Test PathPaymentStrictReceive destination no trust.
    ///
    /// C++ Reference: PathPaymentTests.cpp - "no trust dest" test section
    #[test]
    fn test_path_payment_no_trust_dest() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(49);
        let dest_id = create_test_account_id(50);
        let issuer_id = create_test_account_id(51);

        state.create_account(create_test_account(source_id.clone(), 100_000_000));
        state.create_account(create_test_account(dest_id.clone(), 100_000_000));
        state.create_account(create_test_account(issuer_id.clone(), 100_000_000));

        let asset = create_asset(&issuer_id);

        // Source has trustline with balance, but destination doesn't have trustline
        state.create_trustline(create_test_trustline(
            source_id.clone(),
            TrustLineAsset::CreditAlphanum4(AlphaNum4 {
                asset_code: AssetCode4([b'U', b'S', b'D', b'C']),
                issuer: issuer_id.clone(),
            }),
            1000,
            1_000_000,
            AUTHORIZED_FLAG,
        ));

        let op = PathPaymentStrictReceiveOp {
            send_asset: asset.clone(),
            send_max: 100,
            destination: create_test_muxed_account(50),
            dest_asset: asset,
            dest_amount: 50,
            path: vec![].try_into().unwrap(),
        };

        let result =
            execute_path_payment_strict_receive(&op, &source_id, &mut state, &context).unwrap();
        match result {
            OperationResult::OpInner(OperationResultTr::PathPaymentStrictReceive(r)) => {
                assert!(
                    matches!(r, PathPaymentStrictReceiveResult::NoTrust),
                    "Expected NoTrust, got {:?}",
                    r
                );
            }
            _ => panic!("Unexpected result type"),
        }
    }

    /// Test PathPaymentStrictReceive to self (same source and destination).
    ///
    /// C++ Reference: PathPaymentTests.cpp - "self native to native" test section
    #[test]
    fn test_path_payment_self_native() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(52);
        state.create_account(create_test_account(source_id.clone(), 100_000_000));

        let op = PathPaymentStrictReceiveOp {
            send_asset: Asset::Native,
            send_max: 100,
            destination: create_test_muxed_account(52), // Same as source
            dest_asset: Asset::Native,
            dest_amount: 100,
            path: vec![].try_into().unwrap(),
        };

        let result =
            execute_path_payment_strict_receive(&op, &source_id, &mut state, &context).unwrap();
        match result {
            OperationResult::OpInner(OperationResultTr::PathPaymentStrictReceive(r)) => {
                // Self path payment should succeed
                assert!(
                    matches!(r, PathPaymentStrictReceiveResult::Success(_)),
                    "Expected Success for self path payment, got {:?}",
                    r
                );
            }
            _ => panic!("Unexpected result type"),
        }
    }

    /// Test PathPaymentStrictReceive with zero send_max.
    ///
    /// C++ Reference: PathPaymentTests.cpp - "send max zero" test section
    #[test]
    fn test_path_payment_send_max_zero() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(53);
        let dest_id = create_test_account_id(54);
        state.create_account(create_test_account(source_id.clone(), 100_000_000));
        state.create_account(create_test_account(dest_id.clone(), 100_000_000));

        let op = PathPaymentStrictReceiveOp {
            send_asset: Asset::Native,
            send_max: 0, // Zero
            destination: create_test_muxed_account(54),
            dest_asset: Asset::Native,
            dest_amount: 100,
            path: vec![].try_into().unwrap(),
        };

        let result =
            execute_path_payment_strict_receive(&op, &source_id, &mut state, &context).unwrap();
        match result {
            OperationResult::OpInner(OperationResultTr::PathPaymentStrictReceive(r)) => {
                assert!(
                    matches!(r, PathPaymentStrictReceiveResult::Malformed),
                    "Expected Malformed for zero send_max, got {:?}",
                    r
                );
            }
            _ => panic!("Unexpected result type"),
        }
    }

    /// Test PathPaymentStrictReceive with dest_amount of zero returns Malformed.
    ///
    /// C++ Reference: PathPaymentTests.cpp - "dest amount zero" test section
    #[test]
    fn test_path_payment_dest_amount_zero() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(60);
        let dest_id = create_test_account_id(61);
        state.create_account(create_test_account(source_id.clone(), 100_000_000));
        state.create_account(create_test_account(dest_id.clone(), 100_000_000));

        let op = PathPaymentStrictReceiveOp {
            send_asset: Asset::Native,
            send_max: 1000,
            destination: create_test_muxed_account(61),
            dest_asset: Asset::Native,
            dest_amount: 0, // Zero dest amount
            path: vec![].try_into().unwrap(),
        };

        let result =
            execute_path_payment_strict_receive(&op, &source_id, &mut state, &context).unwrap();
        match result {
            OperationResult::OpInner(OperationResultTr::PathPaymentStrictReceive(r)) => {
                assert!(
                    matches!(r, PathPaymentStrictReceiveResult::Malformed),
                    "Expected Malformed for zero dest_amount, got {:?}",
                    r
                );
            }
            _ => panic!("Unexpected result type"),
        }
    }

    /// Test PathPaymentStrictSend with dest_min of zero returns Malformed.
    /// Zero dest_min is not allowed - must be positive.
    ///
    /// C++ Reference: PathPaymentStrictSendTests.cpp - "dest min zero" test section
    #[test]
    fn test_path_payment_strict_send_dest_min_zero() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(62);
        let dest_id = create_test_account_id(63);
        state.create_account(create_test_account(source_id.clone(), 100_000_000));
        state.create_account(create_test_account(dest_id.clone(), 100_000_000));

        let op = PathPaymentStrictSendOp {
            send_asset: Asset::Native,
            send_amount: 1000,
            destination: create_test_muxed_account(63),
            dest_asset: Asset::Native,
            dest_min: 0, // Zero dest_min is malformed
            path: vec![].try_into().unwrap(),
        };

        let result =
            execute_path_payment_strict_send(&op, &source_id, &mut state, &context).unwrap();
        match result {
            OperationResult::OpInner(OperationResultTr::PathPaymentStrictSend(r)) => {
                assert!(
                    matches!(r, PathPaymentStrictSendResult::Malformed),
                    "Expected Malformed for zero dest_min, got {:?}",
                    r
                );
            }
            _ => panic!("Unexpected result type"),
        }
    }

    /// Test PathPaymentStrictReceive with negative dest_amount returns Malformed.
    ///
    /// C++ Reference: PathPaymentTests.cpp - "negative dest amount" test section
    #[test]
    fn test_path_payment_negative_dest_amount() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(64);
        let dest_id = create_test_account_id(65);
        state.create_account(create_test_account(source_id.clone(), 100_000_000));
        state.create_account(create_test_account(dest_id.clone(), 100_000_000));

        let op = PathPaymentStrictReceiveOp {
            send_asset: Asset::Native,
            send_max: 1000,
            destination: create_test_muxed_account(65),
            dest_asset: Asset::Native,
            dest_amount: -100, // Negative
            path: vec![].try_into().unwrap(),
        };

        let result =
            execute_path_payment_strict_receive(&op, &source_id, &mut state, &context).unwrap();
        match result {
            OperationResult::OpInner(OperationResultTr::PathPaymentStrictReceive(r)) => {
                assert!(
                    matches!(r, PathPaymentStrictReceiveResult::Malformed),
                    "Expected Malformed for negative dest_amount, got {:?}",
                    r
                );
            }
            _ => panic!("Unexpected result type"),
        }
    }

    /// Test PathPaymentStrictReceive line full when destination trustline is at limit.
    ///
    /// C++ Reference: PathPaymentTests.cpp - "line full" test section
    #[test]
    fn test_path_payment_line_full() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(70);
        let dest_id = create_test_account_id(71);
        let issuer_id = create_test_account_id(72);

        state.create_account(create_test_account(source_id.clone(), 100_000_000));
        state.create_account(create_test_account(dest_id.clone(), 100_000_000));
        state.create_account(create_test_account(issuer_id.clone(), 100_000_000));

        let asset = create_asset(&issuer_id);

        // Source trustline with balance
        state.create_trustline(create_test_trustline(
            source_id.clone(),
            TrustLineAsset::CreditAlphanum4(AlphaNum4 {
                asset_code: AssetCode4([b'U', b'S', b'D', b'C']),
                issuer: issuer_id.clone(),
            }),
            10_000,
            100_000,
            TrustLineFlags::AuthorizedFlag as u32,
        ));
        state.get_account_mut(&source_id).unwrap().num_sub_entries += 1;

        // Destination trustline already at limit
        state.create_trustline(create_test_trustline(
            dest_id.clone(),
            TrustLineAsset::CreditAlphanum4(AlphaNum4 {
                asset_code: AssetCode4([b'U', b'S', b'D', b'C']),
                issuer: issuer_id.clone(),
            }),
            1000,
            1000, // At limit!
            TrustLineFlags::AuthorizedFlag as u32,
        ));
        state.get_account_mut(&dest_id).unwrap().num_sub_entries += 1;

        let op = PathPaymentStrictReceiveOp {
            send_asset: asset.clone(),
            send_max: 100,
            destination: create_test_muxed_account(71),
            dest_asset: asset,
            dest_amount: 100,
            path: vec![].try_into().unwrap(),
        };

        let result =
            execute_path_payment_strict_receive(&op, &source_id, &mut state, &context).unwrap();
        match result {
            OperationResult::OpInner(OperationResultTr::PathPaymentStrictReceive(r)) => {
                assert!(
                    matches!(r, PathPaymentStrictReceiveResult::LineFull),
                    "Expected LineFull when dest trustline at limit, got {:?}",
                    r
                );
            }
            _ => panic!("Unexpected result type"),
        }
    }

    #[test]
    fn test_exchange_with_pool_strict_send_large_reserves_no_overflow() {
        // Regression test: when reserves_from_pool * to_pool * (MAX_BPS - fee_bps)
        // exceeds u128::MAX, the naive multiplication overflows silently in release
        // mode. The hugeDivide decomposition avoids this.
        //
        // Found at mainnet ledger 59501889: PathPaymentStrictSend through pools
        // with very large intermediate amounts (e.g., 7e16 stroops).
        let reserves_to_pool: i64 = 500_000_000_000_000_000; // 5e17
        let reserves_from_pool: i64 = 400_000_000_000_000_000; // 4e17
        let send_amount: i64 = 70_000_000_000_000_000; // 7e16

        // Verify that reserves_from * to_pool * 9970 would overflow u128
        let a = 9970u128;
        let _b = reserves_from_pool as u128 * send_amount as u128;
        // a * b = 9970 * 4e17 * 7e16 = 9970 * 2.8e34 ≈ 2.79e38
        // u128::MAX ≈ 3.4e38, so this is close to the boundary.
        // With slightly larger reserves, it WILL overflow.
        let reserves_from_large: i64 = 900_000_000_000_000_000; // 9e17
        let b_large = reserves_from_large as u128 * send_amount as u128;
        assert!(
            a.checked_mul(b_large).is_none(),
            "Should overflow u128 with large reserves: a*b = {} * {} would overflow",
            a,
            b_large
        );

        // The hugeDivide decomposition should still produce a correct result
        let mut to_pool = 0i64;
        let mut from_pool = 0i64;
        let ok = exchange_with_pool(PoolExchangeRequest {
            reserves_to_pool,
            max_send_to_pool: send_amount,
            to_pool: &mut to_pool,
            reserves_from_pool: reserves_from_large,
            max_receive_from_pool: i64::MAX,
            from_pool: &mut from_pool,
            fee_bps: 30,
            round: RoundingType::PathPaymentStrictSend,
        })
        .unwrap();

        assert!(ok, "Exchange should succeed");
        assert_eq!(to_pool, send_amount);
        assert!(from_pool > 0, "Should receive positive amount");

        // Verify the result matches manual calculation using the decomposition
        let denominator = 10_000u128 * reserves_to_pool as u128 + 9970u128 * send_amount as u128;
        let q = b_large / denominator;
        let r = b_large % denominator;
        let expected = a * q + (a * r) / denominator;
        assert_eq!(from_pool, expected as i64);
    }

    /// Regression test for pool exchange overflow returning Ok(false) instead of Err.
    /// Found at mainnet ledger 61170102: PathPaymentStrictReceive hit overflow in
    /// hugeDivide, causing TxNotSupported instead of TooFewOffers.
    /// stellar-core hugeDivide returns false on overflow; the exchange is simply not possible.
    #[test]
    fn test_pool_exchange_overflow_returns_false_strict_receive() {
        // Set up values that cause the hugeDivide result to overflow i64:
        // Large reserves_to_pool and from_pool, small denominator (reserves_from_pool - from_pool)
        let reserves_to_pool: i64 = i64::MAX / 2;
        let reserves_from_pool: i64 = i64::MAX / 2;
        let max_receive_from_pool: i64 = reserves_from_pool - 1; // just under reserves
        let fee_bps: i64 = 30;
        let mut to_pool: i64 = 0;
        let mut from_pool: i64 = 0;

        let result = exchange_with_pool(PoolExchangeRequest {
            reserves_to_pool,
            max_send_to_pool: i64::MAX,
            to_pool: &mut to_pool,
            reserves_from_pool,
            max_receive_from_pool,
            from_pool: &mut from_pool,
            fee_bps,
            round: RoundingType::PathPaymentStrictReceive,
        });

        // Must return Ok(false) — exchange not possible — NOT Err
        assert!(
            result.is_ok(),
            "Overflow must not return Err: {:?}",
            result.err()
        );
        assert!(!result.unwrap(), "Overflow exchange should return false");
    }

    #[test]
    fn test_pool_exchange_overflow_returns_false_strict_send() {
        // Similarly for strict send: values that cause overflow in the result
        let reserves_to_pool: i64 = 1; // tiny reserves
        let reserves_from_pool: i64 = i64::MAX;
        let max_send_to_pool: i64 = i64::MAX - 1; // near-overflow reserves
        let fee_bps: i64 = 30;
        let mut to_pool: i64 = 0;
        let mut from_pool: i64 = 0;

        let result = exchange_with_pool(PoolExchangeRequest {
            reserves_to_pool,
            max_send_to_pool,
            to_pool: &mut to_pool,
            reserves_from_pool,
            max_receive_from_pool: i64::MAX,
            from_pool: &mut from_pool,
            fee_bps,
            round: RoundingType::PathPaymentStrictSend,
        });

        // Must return Ok — exchange result (true or false) — NOT Err
        assert!(
            result.is_ok(),
            "Overflow must not return Err: {:?}",
            result.err()
        );
    }

    /// Regression test: convert_with_offers must return CrossedTooMany when
    /// max_offers_to_cross is 0 and there is still demand (fast-fail path).
    #[test]
    fn test_convert_with_offers_crossed_too_many_fast_fail() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();
        let source_id = create_test_account_id(0);

        let mut amount_send = 0;
        let mut amount_recv = 0;
        let mut offer_trail = Vec::new();

        let result = convert_with_offers(
            &mut ConversionParams {
                source: &source_id,
                selling: &Asset::Native,
                buying: &Asset::Native,
                max_send: i64::MAX,
                max_receive: 1_000_000,
                round: RoundingType::Normal,
                offer_trail: &mut offer_trail,
                state: &mut state,
                context: &context,
                frozen_key_config: &context.frozen_key_config,
            },
            &mut amount_send,
            &mut amount_recv,
            0, // max_offers_to_cross = 0 → fast-fail
        );

        assert_eq!(result.unwrap(), ConvertResult::CrossedTooMany);
    }
}
