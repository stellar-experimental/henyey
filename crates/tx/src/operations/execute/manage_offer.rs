//! Manage Offer operation execution.
//!
//! This module implements the execution logic for ManageSellOffer, ManageBuyOffer,
//! and CreatePassiveSellOffer operations for the Stellar DEX.

use std::cmp::Ordering;

use stellar_xdr::curr::{
    AccountId, Asset, CreatePassiveSellOfferOp, LedgerKey, LedgerKeyOffer, ManageBuyOfferOp,
    ManageOfferSuccessResult, ManageOfferSuccessResultOffer, ManageSellOfferOp,
    ManageSellOfferResult, ManageSellOfferResultCode, OfferEntry, OfferEntryExt, OfferEntryFlags,
    OperationResult, OperationResultTr, Price,
};

use super::offer_exchange::{
    adjust_offer_amount, exchange_v10_without_price_error_thresholds, ConversionParams,
    RoundingType,
};
use super::offer_utils::{
    can_sell_at_most, cross_offer_v10, delete_offer_with_sponsorship, offer_liabilities_sell,
};
use super::{
    account_balance_after_liabilities, account_liabilities, apply_balance_delta,
    apply_liabilities_delta, can_buy_at_most, dec_sub_entries, inc_sub_entries, is_asset_valid,
    is_trustline_authorized, issuer_for_asset, map_exchange_error, require_source_account,
    trustline_liabilities, ACCOUNT_SUBENTRY_LIMIT,
};
use crate::frozen_keys::offer_accesses_frozen_key;
use crate::state::LedgerStateManager;
use crate::validation::LedgerContext;
use crate::{Result, TxError};

/// Maximum number of offers that can be crossed in a single manage offer operation.
/// Matches stellar-core's MAX_OFFERS_TO_CROSS (protocol 11+).
const MAX_OFFERS_TO_CROSS: i64 = 1000;

/// Execute a ManageSellOffer operation.
///
/// This operation creates, updates, or deletes an offer to sell one asset for another.
pub(crate) fn execute_manage_sell_offer(
    op: &ManageSellOfferOp,
    source: &AccountId,
    state: &mut LedgerStateManager,
    context: &LedgerContext,
) -> Result<OperationResult> {
    execute_manage_offer(
        ManageOfferRequest {
            source,
            selling: &op.selling,
            buying: &op.buying,
            offer_id: op.offer_id,
            price: &op.price,
            offer_kind: OfferKind::Sell { amount: op.amount },
            passive: false,
        },
        state,
        context,
    )
}

struct ManageOfferRequest<'a> {
    source: &'a AccountId,
    selling: &'a Asset,
    buying: &'a Asset,
    offer_id: i64,
    price: &'a Price,
    offer_kind: OfferKind,
    passive: bool,
}

fn execute_manage_offer(
    request: ManageOfferRequest<'_>,
    state: &mut LedgerStateManager,
    context: &LedgerContext,
) -> Result<OperationResult> {
    let ManageOfferRequest {
        source,
        selling,
        buying,
        offer_id,
        price,
        offer_kind,
        passive,
    } = request;
    let offer_amount = offer_kind.amount();

    // Validate the offer parameters
    if let Err(code) = validate_offer(selling, buying, offer_amount, price) {
        return Ok(make_sell_offer_result(code, None));
    }

    // Check if this is a delete operation (amount = 0 with existing offer_id)
    if offer_amount == 0 {
        if offer_id == 0 {
            return Ok(make_sell_offer_result(
                ManageSellOfferResultCode::Malformed,
                None,
            ));
        }
        return delete_offer(source, offer_id, state);
    }

    // Check source account exists
    if state.get_account(source).is_none() {
        return Ok(make_sell_offer_result(
            ManageSellOfferResultCode::Underfunded,
            None,
        ));
    }

    // stellar-core checks trustlines BEFORE checking offer existence.
    // This is done in checkOfferValid() which is called before loadOffer().
    // We need to match this order to produce identical error codes.

    // For selling non-native assets, check trustline exists and has balance
    // This must happen BEFORE we check if the offer exists (NotFound).
    if !matches!(selling, Asset::Native) {
        if issuer_for_asset(selling) == Some(source) {
            // Issuer can always sell its own asset.
        } else {
            let trustline = match state.get_trustline(source, selling) {
                Some(tl) => tl,
                None => {
                    return Ok(make_sell_offer_result(
                        ManageSellOfferResultCode::SellNoTrust,
                        None,
                    ));
                }
            };

            // Check balance first (before authorization check)
            if trustline.balance == 0 {
                return Ok(make_sell_offer_result(
                    ManageSellOfferResultCode::Underfunded,
                    None,
                ));
            }

            if !is_trustline_authorized(trustline.flags) {
                return Ok(make_sell_offer_result(
                    ManageSellOfferResultCode::SellNotAuthorized,
                    None,
                ));
            }
        }
    }

    // For buying non-native assets, check trustline exists
    // This must happen BEFORE we check if the offer exists (NotFound).
    if !matches!(buying, Asset::Native) {
        if issuer_for_asset(buying) == Some(source) {
            // Issuer can always receive its own asset.
        } else {
            let trustline = match state.get_trustline(source, buying) {
                Some(tl) => tl,
                None => {
                    return Ok(make_sell_offer_result(
                        ManageSellOfferResultCode::BuyNoTrust,
                        None,
                    ));
                }
            };

            if !is_trustline_authorized(trustline.flags) {
                return Ok(make_sell_offer_result(
                    ManageSellOfferResultCode::BuyNotAuthorized,
                    None,
                ));
            }
        }
    }

    // NOW check if the offer exists (after trustline checks)
    let old_offer = if offer_id != 0 {
        state.get_offer(source, offer_id)
    } else {
        None
    };
    if offer_id != 0 && old_offer.is_none() {
        return Ok(make_sell_offer_result(
            ManageSellOfferResultCode::NotFound,
            None,
        ));
    }
    // For new offers, check subentry limit FIRST (before any balance/reserve checks).
    // This matches stellar-core's check ordering.
    if old_offer.is_none() {
        let source_account = require_source_account(state, source)?;
        if source_account.num_sub_entries >= ACCOUNT_SUBENTRY_LIMIT {
            return Ok(OperationResult::OpTooManySubentries);
        }
        // Protocol 18+ combined-cap: num_sub_entries + num_sponsoring + 1 must fit u32.
        // Mirrors stellar-core isSponsoringSubentrySumIncreaseValid().
        let (num_sponsoring, _) = state
            .sponsorship_counts_for_account(source)
            .unwrap_or((0, 0));
        let total = source_account.num_sub_entries as u64 + num_sponsoring as u64 + 1;
        if total > u32::MAX as u64 {
            return Ok(OperationResult::OpTooManySubentries);
        }
    }

    let sponsor = if old_offer.is_none() {
        state.active_sponsor_for(source)
    } else {
        None
    };
    let reserve_subentry = old_offer.is_none() && sponsor.is_none();

    // For native selling asset with existing offers, check account has sufficient balance.
    // For new offers, skip this check: stellar-core checks LowReserve (via
    // createEntryWithPossibleSponsorship) BEFORE Underfunded (via
    // computeOfferExchangeParameters). The later has_selling_capacity check
    // handles the Underfunded case for new offers after the LowReserve check.
    if old_offer.is_some() && matches!(selling, Asset::Native) {
        let account = state.get_account(source).unwrap();
        let min_balance =
            state.minimum_balance_for_account(account, context.protocol_version, 0)?;
        if account.balance < min_balance {
            return Ok(make_sell_offer_result(
                ManageSellOfferResultCode::Underfunded,
                None,
            ));
        }
    }

    if old_offer.is_none() {
        if let Some(sponsor) = &sponsor {
            let sponsor_account = state
                .get_account(sponsor)
                .ok_or(TxError::SourceAccountNotFound)?;
            let min_balance = state.minimum_balance_for_account_with_deltas(
                sponsor_account,
                context.protocol_version,
                0,
                1,
                0,
            )?;
            let available = account_balance_after_liabilities(sponsor_account);
            if available < min_balance {
                return Ok(make_sell_offer_result(
                    ManageSellOfferResultCode::LowReserve,
                    None,
                ));
            }
        } else if let Some(account) = state.get_account(source) {
            let min_balance =
                state.minimum_balance_for_account(account, context.protocol_version, 1)?;
            let available = account_balance_after_liabilities(account);
            if available < min_balance {
                return Ok(make_sell_offer_result(
                    ManageSellOfferResultCode::LowReserve,
                    None,
                ));
            }
        }
    }

    let (mut offer_flags, was_passive) = if let Some(old) = &old_offer {
        (
            old.flags,
            (old.flags & (OfferEntryFlags::PassiveFlag as u32)) != 0,
        )
    } else {
        (0, false)
    };
    let passive = if old_offer.is_some() {
        was_passive
    } else {
        passive
    };

    if passive {
        offer_flags |= OfferEntryFlags::PassiveFlag as u32;
    }

    if let Some(old) = &old_offer {
        // Ensure trustlines for the old offer's assets are loaded before updating liabilities.
        // The old offer might have different assets than the new offer being created/updated.
        if !matches!(&old.selling, Asset::Native) && issuer_for_asset(&old.selling) != Some(source)
        {
            state.ensure_trustline_loaded(source, &old.selling)?;
        }
        if !matches!(&old.buying, Asset::Native) && issuer_for_asset(&old.buying) != Some(source) {
            state.ensure_trustline_loaded(source, &old.buying)?;
        }

        let (old_selling, old_buying) = offer_liabilities_sell(old.amount, &old.price)?;
        apply_liabilities_delta(
            source,
            &old.selling,
            &old.buying,
            -old_selling,
            -old_buying,
            state,
        )?;
    }

    let (selling_liab, buying_liab) = offer_kind.offer_liabilities(price)?;
    // Check LineFull before Underfunded to match stellar-core ordering
    // (ManageOfferOpFrameBase.cpp:173 checks availableLimit before line 198 checks availableBalance).
    // When both conditions fail, the first check determines the result code.
    if !has_buying_capacity(source, buying, buying_liab, 0, state) {
        return Ok(make_sell_offer_result(
            ManageSellOfferResultCode::LineFull,
            None,
        ));
    }

    if !has_selling_capacity(
        source,
        selling,
        selling_liab,
        0,
        reserve_subentry,
        state,
        context,
    )? {
        return Ok(make_sell_offer_result(
            ManageSellOfferResultCode::Underfunded,
            None,
        ));
    }

    let mut max_sheep_send = can_sell_at_most(source, selling, state, context, reserve_subentry)?;
    let mut max_wheat_receive = can_buy_at_most(source, buying, state);
    offer_kind.apply_limits(&mut max_sheep_send, 0, &mut max_wheat_receive, 0);
    if max_wheat_receive == 0 {
        return Ok(make_sell_offer_result(
            ManageSellOfferResultCode::LineFull,
            None,
        ));
    }

    let mut offer_trail = Vec::new();
    let mut sheep_sent = 0;
    let mut wheat_received = 0;
    let max_wheat_price = Price {
        n: price.d,
        d: price.n,
    };

    let convert_res = convert_with_offers(
        &mut ConversionParams {
            source,
            selling,
            buying,
            max_send: max_sheep_send,
            max_receive: max_wheat_receive,
            round: RoundingType::Normal,
            offer_trail: &mut offer_trail,
            state,
            context,
            frozen_key_config: &context.frozen_key_config,
        },
        &mut sheep_sent,
        &mut wheat_received,
        offer_id,
        passive,
        &max_wheat_price,
        MAX_OFFERS_TO_CROSS,
    )?;

    let sheep_stays = match convert_res {
        ConvertResult::Ok => false,
        ConvertResult::Partial => true,
        ConvertResult::FilterStopBadPrice => true,
        ConvertResult::FilterStopCrossSelf => {
            return Ok(make_sell_offer_result(
                ManageSellOfferResultCode::CrossSelf,
                None,
            ));
        }
        ConvertResult::CrossedTooMany => {
            return Ok(OperationResult::OpExceededWorkLimit);
        }
    };

    if wheat_received > 0 {
        apply_balance_delta(source, buying, wheat_received, state)?;
        apply_balance_delta(source, selling, -sheep_sent, state)?;
    }

    let amount = if sheep_stays {
        let mut sheep_limit = can_sell_at_most(source, selling, state, context, reserve_subentry)?;
        let mut wheat_limit = can_buy_at_most(source, buying, state);
        offer_kind.apply_limits(
            &mut sheep_limit,
            sheep_sent,
            &mut wheat_limit,
            wheat_received,
        );
        adjust_offer_amount(price.clone(), sheep_limit, wheat_limit).map_err(map_exchange_error)?
    } else {
        0
    };

    let result = if amount > 0 {
        let offer_id = if old_offer.is_none() {
            generate_offer_id(state)
        } else {
            offer_id
        };
        let offer = OfferEntry {
            seller_id: source.clone(),
            offer_id,
            selling: selling.clone(),
            buying: buying.clone(),
            amount,
            price: price.clone(),
            flags: offer_flags,
            ext: OfferEntryExt::V0,
        };

        if old_offer.is_none() {
            if let Some(sponsor) = sponsor {
                let ledger_key = LedgerKey::Offer(LedgerKeyOffer {
                    seller_id: source.clone(),
                    offer_id,
                });
                state.apply_entry_sponsorship_with_sponsor(
                    ledger_key,
                    &sponsor,
                    Some(source),
                    1,
                )?;
            }
            state.create_offer(offer);
            if let Some(account) = state.get_account_mut(source) {
                inc_sub_entries(account, 1);
            }
            ManageOfferSuccessResultOffer::Created(create_offer_entry(
                source,
                offer_id,
                selling,
                buying,
                amount,
                price,
                offer_flags,
            ))
        } else {
            state.update_offer(offer);
            ManageOfferSuccessResultOffer::Updated(create_offer_entry(
                source,
                offer_id,
                selling,
                buying,
                amount,
                price,
                offer_flags,
            ))
        }
    } else {
        if old_offer.is_some() {
            let ledger_key = LedgerKey::Offer(LedgerKeyOffer {
                seller_id: source.clone(),
                offer_id,
            });
            if state.entry_sponsor(&ledger_key).is_some() {
                state.remove_entry_sponsorship_and_update_counts(&ledger_key, source, 1)?;
            }
            state.delete_offer(source, offer_id);
            // stellar-core: panics on underflow (invalid account state)
            if let Some(account) = state.get_account_mut(source) {
                dec_sub_entries(account, 1);
            }
        } else {
            // New offer that was fully consumed during matching. Mirrors
            // stellar-core's createEntryWithPossibleSponsorship +
            // removeEntryWithPossibleSponsorship pair (V14+,
            // ManageOfferOpFrameBase.cpp:282-315 and 530-537):
            //   - Sponsor numSponsoring overflow check (TOO_MANY_SPONSORING).
            //   - lastModified bumps on source and sponsor (counts net to zero).
            //
            // Order is observable: record_account_access pushes onto
            // modified_accounts, and flush_all_accounts_except iterates that
            // Vec in push order to emit tx-meta deltas. Source-then-sponsor
            // matches stellar-core's loadAccount(source) →
            // createEntryWithPossibleSponsorship → loadAccount(sponsor)
            // sequence. The overflow check runs first so on failure neither
            // account is stamped — matches stellar-core, which discards the
            // LedgerTxn on TOO_MANY_SPONSORING (TransactionFrame.cpp:2275-2322).
            //
            // The check fires AFTER convert_with_offers in henyey (vs BEFORE
            // in stellar-core); the per-op savepoint rolls back intermediate
            // mutations on failure, making the observable behavior identical.
            if let Some(ref sponsor) = sponsor {
                state.check_can_increase_num_sponsoring(sponsor, 1)?;
            }
            state.record_account_access(source);
            if let Some(ref sponsor) = sponsor {
                state.record_account_access(sponsor);
            }
        }
        ManageOfferSuccessResultOffer::Deleted
    };

    if amount > 0 {
        let (new_selling, new_buying) = offer_liabilities_sell(amount, price)?;
        apply_liabilities_delta(source, selling, buying, new_selling, new_buying, state)?;
    }

    let success = ManageOfferSuccessResult {
        offers_claimed: offer_trail.try_into().unwrap(),
        offer: result,
    };

    Ok(make_sell_offer_result(
        ManageSellOfferResultCode::Success,
        Some(success),
    ))
}

/// Execute a ManageBuyOffer operation.
///
/// This operation creates, updates, or deletes an offer to buy one asset with another.
pub(crate) fn execute_manage_buy_offer(
    op: &ManageBuyOfferOp,
    source: &AccountId,
    state: &mut LedgerStateManager,
    context: &LedgerContext,
) -> Result<OperationResult> {
    let effective_price = invert_price(&op.price);

    let result = execute_manage_offer(
        ManageOfferRequest {
            source,
            selling: &op.selling,
            buying: &op.buying,
            offer_id: op.offer_id,
            price: &effective_price,
            offer_kind: OfferKind::Buy {
                buy_amount: op.buy_amount,
            },
            passive: false,
        },
        state,
        context,
    )?;

    match result {
        OperationResult::OpInner(OperationResultTr::ManageSellOffer(r)) => {
            Ok(OperationResult::OpInner(OperationResultTr::ManageBuyOffer(
                convert_sell_to_buy_result(r),
            )))
        }
        other => Ok(other),
    }
}

/// Execute a CreatePassiveSellOffer operation.
///
/// This operation creates a passive sell offer that doesn't cross existing offers.
pub(crate) fn execute_create_passive_sell_offer(
    op: &CreatePassiveSellOfferOp,
    source: &AccountId,
    state: &mut LedgerStateManager,
    context: &LedgerContext,
) -> Result<OperationResult> {
    execute_manage_offer(
        ManageOfferRequest {
            source,
            selling: &op.selling,
            buying: &op.buying,
            offer_id: 0,
            price: &op.price,
            offer_kind: OfferKind::Sell { amount: op.amount },
            passive: true,
        },
        state,
        context,
    )
}

#[derive(Clone, Copy, Debug)]
enum OfferKind {
    Sell { amount: i64 },
    Buy { buy_amount: i64 },
}

impl OfferKind {
    fn amount(self) -> i64 {
        match self {
            OfferKind::Sell { amount } => amount,
            OfferKind::Buy { buy_amount } => buy_amount,
        }
    }

    fn offer_liabilities(self, price: &Price) -> Result<(i64, i64)> {
        match self {
            OfferKind::Sell { amount } => offer_liabilities_sell(amount, price),
            OfferKind::Buy { buy_amount } => offer_liabilities_buy(buy_amount, price),
        }
    }

    fn apply_limits(
        self,
        max_sheep_send: &mut i64,
        sheep_sent: i64,
        max_wheat_receive: &mut i64,
        wheat_received: i64,
    ) {
        match self {
            OfferKind::Sell { amount } => {
                *max_sheep_send = (*max_sheep_send).min(amount.saturating_sub(sheep_sent));
            }
            OfferKind::Buy { buy_amount } => {
                *max_wheat_receive =
                    (*max_wheat_receive).min(buy_amount.saturating_sub(wheat_received));
            }
        }
    }
}

/// Validate offer parameters.
fn validate_offer(
    selling: &Asset,
    buying: &Asset,
    amount: i64,
    price: &Price,
) -> std::result::Result<(), ManageSellOfferResultCode> {
    // Validate asset codes (stellar-core: isAssetValid in doCheckValid)
    if !is_asset_valid(selling) || !is_asset_valid(buying) {
        return Err(ManageSellOfferResultCode::Malformed);
    }

    // Cannot trade an asset for itself
    if selling == buying {
        return Err(ManageSellOfferResultCode::Malformed);
    }

    // Amount must be non-negative (0 is valid for deleting)
    if amount < 0 {
        return Err(ManageSellOfferResultCode::Malformed);
    }

    // Price must be positive
    if price.n <= 0 || price.d <= 0 {
        return Err(ManageSellOfferResultCode::Malformed);
    }

    Ok(())
}

/// Delete an existing offer.
fn delete_offer(
    source: &AccountId,
    offer_id: i64,
    state: &mut LedgerStateManager,
) -> Result<OperationResult> {
    // Check offer exists and belongs to source
    let offer = match state.get_offer(source, offer_id) {
        Some(offer) => offer,
        None => {
            return Ok(make_sell_offer_result(
                ManageSellOfferResultCode::NotFound,
                None,
            ));
        }
    };

    // Ensure trustlines for the offer's assets are loaded before updating liabilities.
    // The offer might reference assets that weren't loaded in the current transaction.
    if !matches!(&offer.selling, Asset::Native) && issuer_for_asset(&offer.selling) != Some(source)
    {
        state.ensure_trustline_loaded(source, &offer.selling)?;
    }
    if !matches!(&offer.buying, Asset::Native) && issuer_for_asset(&offer.buying) != Some(source) {
        state.ensure_trustline_loaded(source, &offer.buying)?;
    }

    let (selling_liab, buying_liab) = offer_liabilities_sell(offer.amount, &offer.price)?;
    apply_liabilities_delta(
        source,
        &offer.selling,
        &offer.buying,
        -selling_liab,
        -buying_liab,
        state,
    )?;

    delete_offer_with_sponsorship(source, offer_id, state)?;

    let success = ManageOfferSuccessResult {
        offers_claimed: vec![].try_into().unwrap(),
        offer: ManageOfferSuccessResultOffer::Deleted,
    };

    Ok(make_sell_offer_result(
        ManageSellOfferResultCode::Success,
        Some(success),
    ))
}

/// Generate a new offer ID.
fn generate_offer_id(state: &mut LedgerStateManager) -> i64 {
    state.next_id()
}

fn invert_price(price: &Price) -> Price {
    Price {
        n: price.d,
        d: price.n,
    }
}

fn offer_liabilities_buy(buy_amount: i64, price: &Price) -> Result<(i64, i64)> {
    let res = exchange_v10_without_price_error_thresholds(
        price.clone(),
        i64::MAX,
        i64::MAX,
        i64::MAX,
        buy_amount,
        RoundingType::Normal,
    )
    .map_err(map_exchange_error)?;
    Ok((res.num_wheat_received, res.num_sheep_send))
}

fn has_selling_capacity(
    source: &AccountId,
    selling: &Asset,
    selling_liab: i64,
    old_selling_liab: i64,
    reserve_subentry: bool,
    state: &LedgerStateManager,
    context: &LedgerContext,
) -> Result<bool> {
    if matches!(selling, Asset::Native) {
        let Some(account) = state.get_account(source) else {
            return Ok(false);
        };
        let min_balance = state.minimum_balance_for_account(
            account,
            context.protocol_version,
            if reserve_subentry { 1 } else { 0 },
        )?;
        let current_liab = account_liabilities(account).selling;
        let effective_liab = current_liab.saturating_sub(old_selling_liab);
        let available = account.balance - min_balance - effective_liab;
        return Ok(available >= selling_liab);
    }

    if issuer_for_asset(selling) == Some(source) {
        return Ok(true);
    }

    let Some(trustline) = state.get_trustline(source, selling) else {
        return Ok(false);
    };
    let current_liab = trustline_liabilities(trustline).selling;
    let effective_liab = current_liab.saturating_sub(old_selling_liab);
    let available = trustline.balance - effective_liab;
    Ok(available >= selling_liab)
}

fn has_buying_capacity(
    source: &AccountId,
    buying: &Asset,
    buying_liab: i64,
    old_buying_liab: i64,
    state: &LedgerStateManager,
) -> bool {
    if matches!(buying, Asset::Native) {
        let Some(account) = state.get_account(source) else {
            return false;
        };
        let current_liab = account_liabilities(account).buying;
        let effective_liab = current_liab.saturating_sub(old_buying_liab);
        let available = i64::MAX - account.balance - effective_liab;
        return available >= buying_liab;
    }

    if issuer_for_asset(buying) == Some(source) {
        return true;
    }

    let Some(trustline) = state.get_trustline(source, buying) else {
        return false;
    };
    let current_liab = trustline_liabilities(trustline).buying;
    let effective_liab = current_liab.saturating_sub(old_buying_liab);
    let available = trustline.limit - trustline.balance - effective_liab;
    available >= buying_liab
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ConvertResult {
    Ok,
    Partial,
    FilterStopBadPrice,
    FilterStopCrossSelf,
    CrossedTooMany,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OfferFilterResult {
    Keep,
    StopBadPrice,
    StopCrossSelf,
}

fn convert_with_offers(
    params: &mut ConversionParams<'_>,
    sheep_sent: &mut i64,
    wheat_received: &mut i64,
    updating_offer_id: i64,
    passive: bool,
    max_wheat_price: &Price,
    max_offers_to_cross: i64,
) -> Result<ConvertResult> {
    params.offer_trail.clear();
    *sheep_sent = 0;
    *wheat_received = 0;

    let mut max_sheep_send = params.max_send;
    let mut max_wheat_receive = params.max_receive;
    let mut need_more = max_sheep_send > 0 && max_wheat_receive > 0;

    // Fast-fail if no crosses allowed
    if need_more && max_offers_to_cross == 0 {
        return Ok(ConvertResult::CrossedTooMany);
    }

    while need_more {
        let offer = params
            .state
            .best_offer_filtered(params.selling, params.buying, |offer| {
                offer.seller_id != *params.source || offer.offer_id != updating_offer_id
            });

        let Some(offer) = offer else {
            break;
        };

        match offer_filter(params.source, &offer, passive, max_wheat_price) {
            OfferFilterResult::Keep => {}
            OfferFilterResult::StopBadPrice => return Ok(ConvertResult::FilterStopBadPrice),
            OfferFilterResult::StopCrossSelf => return Ok(ConvertResult::FilterStopCrossSelf),
        }

        // CAP-77: Skip and delete offers that access frozen keys.
        // Parity: stellar-core checks frozen AFTER price/cross-self filters.
        // When an offer is both frozen and would trigger a stop condition,
        // the stop condition takes priority and the offer survives.
        if offer_accesses_frozen_key(&offer, params.frozen_key_config) {
            // Load seller's account and trustlines so liabilities can be released.
            params.state.ensure_offer_entries_loaded(
                &offer.seller_id,
                &offer.selling,
                &offer.buying,
            )?;
            // Release liabilities before deleting.
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

        // Check maxOffersToCross limit BEFORE crossing — matching stellar-core
        // which checks `offerTrail.size() >= maxOffersToCross` before calling
        // crossOffer. This allows exactly maxOffersToCross crossings.
        if params.offer_trail.len() as i64 >= max_offers_to_cross {
            return Ok(ConvertResult::CrossedTooMany);
        }

        let (num_wheat_received, num_sheep_send, wheat_stays) = cross_offer_v10(
            &offer,
            max_wheat_receive,
            max_sheep_send,
            params.round,
            params.offer_trail,
            params.state,
            params.context,
        )?;
        *sheep_sent += num_sheep_send;
        *wheat_received += num_wheat_received;
        max_sheep_send -= num_sheep_send;
        max_wheat_receive -= num_wheat_received;

        need_more = !wheat_stays && max_wheat_receive > 0 && max_sheep_send > 0;
        if !need_more {
            return Ok(ConvertResult::Ok);
        }
        if wheat_stays {
            return Ok(ConvertResult::Partial);
        }
    }

    Ok(if need_more {
        ConvertResult::Partial
    } else {
        ConvertResult::Ok
    })
}

fn offer_filter(
    source: &AccountId,
    offer: &OfferEntry,
    passive: bool,
    max_wheat_price: &Price,
) -> OfferFilterResult {
    // Check price first - if offer price is worse than our limit, stop looking
    // (offers are sorted by price, so all subsequent offers will be worse)
    let price_cmp = compare_price(&offer.price, max_wheat_price);
    if (passive && price_cmp != Ordering::Less) || (!passive && price_cmp == Ordering::Greater) {
        return OfferFilterResult::StopBadPrice;
    }

    // Only after confirming prices would cross, check if it's our own offer
    if offer.seller_id == *source {
        return OfferFilterResult::StopCrossSelf;
    }

    OfferFilterResult::Keep
}

fn compare_price(lhs: &Price, rhs: &Price) -> Ordering {
    let lhs_value = i128::from(lhs.n) * i128::from(rhs.d);
    let rhs_value = i128::from(rhs.n) * i128::from(lhs.d);
    lhs_value.cmp(&rhs_value)
}

/// Create an OfferEntry for result.
fn create_offer_entry(
    source: &AccountId,
    offer_id: i64,
    selling: &Asset,
    buying: &Asset,
    amount: i64,
    price: &Price,
    flags: u32,
) -> OfferEntry {
    OfferEntry {
        seller_id: source.clone(),
        offer_id,
        selling: selling.clone(),
        buying: buying.clone(),
        amount,
        price: price.clone(),
        flags,
        ext: OfferEntryExt::V0,
    }
}

/// Convert ManageSellOfferResult to ManageBuyOfferResult.
fn convert_sell_to_buy_result(
    result: ManageSellOfferResult,
) -> stellar_xdr::curr::ManageBuyOfferResult {
    use stellar_xdr::curr::ManageBuyOfferResult;

    match result {
        ManageSellOfferResult::Success(s) => ManageBuyOfferResult::Success(s),
        ManageSellOfferResult::Malformed => ManageBuyOfferResult::Malformed,
        ManageSellOfferResult::SellNoTrust => ManageBuyOfferResult::SellNoTrust,
        ManageSellOfferResult::BuyNoTrust => ManageBuyOfferResult::BuyNoTrust,
        ManageSellOfferResult::SellNotAuthorized => ManageBuyOfferResult::SellNotAuthorized,
        ManageSellOfferResult::BuyNotAuthorized => ManageBuyOfferResult::BuyNotAuthorized,
        ManageSellOfferResult::LineFull => ManageBuyOfferResult::LineFull,
        ManageSellOfferResult::Underfunded => ManageBuyOfferResult::Underfunded,
        ManageSellOfferResult::CrossSelf => ManageBuyOfferResult::CrossSelf,
        ManageSellOfferResult::SellNoIssuer => ManageBuyOfferResult::SellNoIssuer,
        ManageSellOfferResult::BuyNoIssuer => ManageBuyOfferResult::BuyNoIssuer,
        ManageSellOfferResult::NotFound => ManageBuyOfferResult::NotFound,
        ManageSellOfferResult::LowReserve => ManageBuyOfferResult::LowReserve,
    }
}

/// Create a ManageSellOffer result.
fn make_sell_offer_result(
    code: ManageSellOfferResultCode,
    success: Option<ManageOfferSuccessResult>,
) -> OperationResult {
    let result = match code {
        ManageSellOfferResultCode::Success => ManageSellOfferResult::Success(success.unwrap()),
        ManageSellOfferResultCode::Malformed => ManageSellOfferResult::Malformed,
        ManageSellOfferResultCode::SellNoTrust => ManageSellOfferResult::SellNoTrust,
        ManageSellOfferResultCode::BuyNoTrust => ManageSellOfferResult::BuyNoTrust,
        ManageSellOfferResultCode::SellNotAuthorized => ManageSellOfferResult::SellNotAuthorized,
        ManageSellOfferResultCode::BuyNotAuthorized => ManageSellOfferResult::BuyNotAuthorized,
        ManageSellOfferResultCode::LineFull => ManageSellOfferResult::LineFull,
        ManageSellOfferResultCode::Underfunded => ManageSellOfferResult::Underfunded,
        ManageSellOfferResultCode::CrossSelf => ManageSellOfferResult::CrossSelf,
        ManageSellOfferResultCode::SellNoIssuer => ManageSellOfferResult::SellNoIssuer,
        ManageSellOfferResultCode::BuyNoIssuer => ManageSellOfferResult::BuyNoIssuer,
        ManageSellOfferResultCode::NotFound => ManageSellOfferResult::NotFound,
        ManageSellOfferResultCode::LowReserve => ManageSellOfferResult::LowReserve,
    };

    OperationResult::OpInner(OperationResultTr::ManageSellOffer(result))
}

#[cfg(test)]
mod tests {
    use super::super::{AUTHORIZED_FLAG, AUTHORIZED_TO_MAINTAIN_LIABILITIES_FLAG};
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

    fn create_asset(issuer: &AccountId) -> Asset {
        Asset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4([b'U', b'S', b'D', b'C']),
            issuer: issuer.clone(),
        })
    }

    #[test]
    fn test_manage_sell_offer_malformed_same_asset() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(0);
        state.create_account(create_test_account(source_id.clone(), 100_000_000));

        let op = ManageSellOfferOp {
            selling: Asset::Native,
            buying: Asset::Native, // Same as selling
            amount: 10_000_000,
            price: Price { n: 1, d: 1 },
            offer_id: 0,
        };

        let result = execute_manage_sell_offer(&op, &source_id, &mut state, &context);
        assert!(result.is_ok());

        match result.unwrap() {
            OperationResult::OpInner(OperationResultTr::ManageSellOffer(r)) => {
                assert!(matches!(r, ManageSellOfferResult::Malformed));
            }
            _ => panic!("Unexpected result type"),
        }
    }

    #[test]
    fn test_manage_sell_offer_create() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(0);
        state.create_account(create_test_account(source_id.clone(), 100_000_000));

        // Create trustline for the buying asset
        let issuer_id = create_test_account_id(99);
        state.create_account(create_test_account(issuer_id.clone(), 100_000_000));
        let buying_asset = create_asset(&issuer_id);
        state.create_trustline(create_test_trustline(
            source_id.clone(),
            TrustLineAsset::CreditAlphanum4(AlphaNum4 {
                asset_code: AssetCode4([b'U', b'S', b'D', b'C']),
                issuer: issuer_id.clone(),
            }),
            0,
            i64::MAX,
            AUTHORIZED_FLAG,
        ));

        let op = ManageSellOfferOp {
            selling: Asset::Native,
            buying: buying_asset,
            amount: 10_000_000,
            price: Price { n: 1, d: 2 },
            offer_id: 0,
        };

        let result = execute_manage_sell_offer(&op, &source_id, &mut state, &context);
        assert!(result.is_ok());

        match result.unwrap() {
            OperationResult::OpInner(OperationResultTr::ManageSellOffer(r)) => {
                assert!(matches!(r, ManageSellOfferResult::Success(_)));
            }
            _ => panic!("Unexpected result type"),
        }
    }

    #[test]
    fn test_manage_sell_offer_issuer_can_sell_without_trustline() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let issuer_id = create_test_account_id(9);
        let min_balance = state
            .minimum_balance_with_counts(context.protocol_version, 0, 0, 0)
            .unwrap();
        state.create_account(create_test_account(
            issuer_id.clone(),
            min_balance + 10_000_000,
        ));

        let selling = Asset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4([b'U', b'S', b'D', b'C']),
            issuer: issuer_id.clone(),
        });

        let op = ManageSellOfferOp {
            selling,
            buying: Asset::Native,
            amount: 100,
            price: Price { n: 1, d: 1 },
            offer_id: 0,
        };

        let result = execute_manage_sell_offer(&op, &issuer_id, &mut state, &context).unwrap();
        match result {
            OperationResult::OpInner(OperationResultTr::ManageSellOffer(r)) => {
                assert!(matches!(r, ManageSellOfferResult::Success(_)));
            }
            _ => panic!("Unexpected result type"),
        }
    }

    #[test]
    fn test_manage_sell_offer_issuer_can_buy_without_trustline() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let issuer_id = create_test_account_id(9);
        let min_balance = state
            .minimum_balance_with_counts(context.protocol_version, 0, 0, 0)
            .unwrap();
        state.create_account(create_test_account(
            issuer_id.clone(),
            min_balance + 10_000_000,
        ));

        let buying = Asset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4([b'U', b'S', b'D', b'C']),
            issuer: issuer_id.clone(),
        });

        let op = ManageSellOfferOp {
            selling: Asset::Native,
            buying,
            amount: 100,
            price: Price { n: 1, d: 1 },
            offer_id: 0,
        };

        let result = execute_manage_sell_offer(&op, &issuer_id, &mut state, &context).unwrap();
        match result {
            OperationResult::OpInner(OperationResultTr::ManageSellOffer(r)) => {
                assert!(matches!(r, ManageSellOfferResult::Success(_)));
            }
            _ => panic!("Unexpected result type"),
        }
    }

    #[test]
    fn test_manage_sell_offer_sell_not_authorized() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(0);
        let issuer_id = create_test_account_id(1);
        state.create_account(create_test_account(source_id.clone(), 100_000_000));
        state.create_account(create_test_account(issuer_id.clone(), 100_000_000));
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

        let op = ManageSellOfferOp {
            selling: asset,
            buying: Asset::Native,
            amount: 10,
            price: Price { n: 1, d: 1 },
            offer_id: 0,
        };

        let result = execute_manage_sell_offer(&op, &source_id, &mut state, &context).unwrap();
        match result {
            OperationResult::OpInner(OperationResultTr::ManageSellOffer(r)) => {
                assert!(matches!(r, ManageSellOfferResult::SellNotAuthorized));
            }
            _ => panic!("Unexpected result type"),
        }
    }

    #[test]
    fn test_manage_sell_offer_sell_authorized_to_maintain_not_authorized() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let mut context = create_test_context();
        context.protocol_version = 13;

        let source_id = create_test_account_id(0);
        let issuer_id = create_test_account_id(1);
        state.create_account(create_test_account(source_id.clone(), 100_000_000));
        state.create_account(create_test_account(issuer_id.clone(), 100_000_000));
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
            AUTHORIZED_TO_MAINTAIN_LIABILITIES_FLAG,
        ));

        let op = ManageSellOfferOp {
            selling: asset,
            buying: Asset::Native,
            amount: 10,
            price: Price { n: 1, d: 1 },
            offer_id: 0,
        };

        let result = execute_manage_sell_offer(&op, &source_id, &mut state, &context).unwrap();
        match result {
            OperationResult::OpInner(OperationResultTr::ManageSellOffer(r)) => {
                assert!(matches!(r, ManageSellOfferResult::SellNotAuthorized));
            }
            _ => panic!("Unexpected result type"),
        }
    }

    #[test]
    fn test_manage_sell_offer_buy_not_authorized() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(0);
        let issuer_id = create_test_account_id(1);
        state.create_account(create_test_account(source_id.clone(), 100_000_000));
        state.create_account(create_test_account(issuer_id.clone(), 100_000_000));
        state.get_account_mut(&issuer_id).unwrap().flags = AccountFlags::RequiredFlag as u32;

        let asset = create_asset(&issuer_id);
        state.create_trustline(create_test_trustline(
            source_id.clone(),
            TrustLineAsset::CreditAlphanum4(AlphaNum4 {
                asset_code: AssetCode4([b'U', b'S', b'D', b'C']),
                issuer: issuer_id.clone(),
            }),
            0,
            1_000_000,
            0,
        ));

        let op = ManageSellOfferOp {
            selling: Asset::Native,
            buying: asset,
            amount: 10,
            price: Price { n: 1, d: 1 },
            offer_id: 0,
        };

        let result = execute_manage_sell_offer(&op, &source_id, &mut state, &context).unwrap();
        match result {
            OperationResult::OpInner(OperationResultTr::ManageSellOffer(r)) => {
                assert!(matches!(r, ManageSellOfferResult::BuyNotAuthorized));
            }
            _ => panic!("Unexpected result type"),
        }
    }

    #[test]
    fn test_manage_sell_offer_buy_authorized_to_maintain_not_authorized() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let mut context = create_test_context();
        context.protocol_version = 13;

        let source_id = create_test_account_id(0);
        let issuer_id = create_test_account_id(1);
        state.create_account(create_test_account(source_id.clone(), 100_000_000));
        state.create_account(create_test_account(issuer_id.clone(), 100_000_000));
        state.get_account_mut(&issuer_id).unwrap().flags = AccountFlags::RequiredFlag as u32;

        let asset = create_asset(&issuer_id);
        state.create_trustline(create_test_trustline(
            source_id.clone(),
            TrustLineAsset::CreditAlphanum4(AlphaNum4 {
                asset_code: AssetCode4([b'U', b'S', b'D', b'C']),
                issuer: issuer_id.clone(),
            }),
            0,
            1_000_000,
            AUTHORIZED_TO_MAINTAIN_LIABILITIES_FLAG,
        ));

        let op = ManageSellOfferOp {
            selling: Asset::Native,
            buying: asset,
            amount: 10,
            price: Price { n: 1, d: 1 },
            offer_id: 0,
        };

        let result = execute_manage_sell_offer(&op, &source_id, &mut state, &context).unwrap();
        match result {
            OperationResult::OpInner(OperationResultTr::ManageSellOffer(r)) => {
                assert!(matches!(r, ManageSellOfferResult::BuyNotAuthorized));
            }
            _ => panic!("Unexpected result type"),
        }
    }

    #[test]
    fn test_manage_sell_offer_update_denied_when_selling_maintain_only() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let mut context = create_test_context();
        context.protocol_version = 13;

        let source_id = create_test_account_id(0);
        let issuer_id = create_test_account_id(1);
        state.create_account(create_test_account(source_id.clone(), 100_000_000));
        state.create_account(create_test_account(issuer_id.clone(), 100_000_000));
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

        let create = ManageSellOfferOp {
            selling: asset.clone(),
            buying: Asset::Native,
            amount: 10,
            price: Price { n: 1, d: 1 },
            offer_id: 0,
        };
        let created = execute_manage_sell_offer(&create, &source_id, &mut state, &context).unwrap();
        let offer_id = match created {
            OperationResult::OpInner(OperationResultTr::ManageSellOffer(
                ManageSellOfferResult::Success(ManageOfferSuccessResult { offer, .. }),
            )) => match offer {
                ManageOfferSuccessResultOffer::Created(entry) => entry.offer_id,
                _ => panic!("expected created offer"),
            },
            _ => panic!("unexpected result type"),
        };

        let tl = state
            .get_trustline_by_trustline_asset_mut(
                &source_id,
                &TrustLineAsset::CreditAlphanum4(AlphaNum4 {
                    asset_code: AssetCode4([b'U', b'S', b'D', b'C']),
                    issuer: issuer_id.clone(),
                }),
            )
            .expect("trustline exists");
        tl.flags = AUTHORIZED_TO_MAINTAIN_LIABILITIES_FLAG;

        let update = ManageSellOfferOp {
            selling: asset,
            buying: Asset::Native,
            amount: 11,
            price: Price { n: 1, d: 1 },
            offer_id,
        };

        let result = execute_manage_sell_offer(&update, &source_id, &mut state, &context).unwrap();
        match result {
            OperationResult::OpInner(OperationResultTr::ManageSellOffer(r)) => {
                assert!(matches!(r, ManageSellOfferResult::SellNotAuthorized));
            }
            _ => panic!("Unexpected result type"),
        }
    }

    #[test]
    fn test_manage_sell_offer_update_denied_when_buying_maintain_only() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let mut context = create_test_context();
        context.protocol_version = 13;

        let source_id = create_test_account_id(0);
        let issuer_id = create_test_account_id(1);
        state.create_account(create_test_account(source_id.clone(), 100_000_000));
        state.create_account(create_test_account(issuer_id.clone(), 100_000_000));
        state.get_account_mut(&issuer_id).unwrap().flags = AccountFlags::RequiredFlag as u32;

        let asset = create_asset(&issuer_id);
        state.create_trustline(create_test_trustline(
            source_id.clone(),
            TrustLineAsset::CreditAlphanum4(AlphaNum4 {
                asset_code: AssetCode4([b'U', b'S', b'D', b'C']),
                issuer: issuer_id.clone(),
            }),
            0,
            1_000_000,
            AUTHORIZED_FLAG,
        ));

        let create = ManageSellOfferOp {
            selling: Asset::Native,
            buying: asset.clone(),
            amount: 10,
            price: Price { n: 1, d: 1 },
            offer_id: 0,
        };
        let created = execute_manage_sell_offer(&create, &source_id, &mut state, &context).unwrap();
        let offer_id = match created {
            OperationResult::OpInner(OperationResultTr::ManageSellOffer(
                ManageSellOfferResult::Success(ManageOfferSuccessResult { offer, .. }),
            )) => match offer {
                ManageOfferSuccessResultOffer::Created(entry) => entry.offer_id,
                _ => panic!("expected created offer"),
            },
            _ => panic!("unexpected result type"),
        };

        let tl = state
            .get_trustline_by_trustline_asset_mut(
                &source_id,
                &TrustLineAsset::CreditAlphanum4(AlphaNum4 {
                    asset_code: AssetCode4([b'U', b'S', b'D', b'C']),
                    issuer: issuer_id.clone(),
                }),
            )
            .expect("trustline exists");
        tl.flags = AUTHORIZED_TO_MAINTAIN_LIABILITIES_FLAG;

        let update = ManageSellOfferOp {
            selling: Asset::Native,
            buying: asset,
            amount: 11,
            price: Price { n: 1, d: 1 },
            offer_id,
        };

        let result = execute_manage_sell_offer(&update, &source_id, &mut state, &context).unwrap();
        match result {
            OperationResult::OpInner(OperationResultTr::ManageSellOffer(r)) => {
                assert!(matches!(r, ManageSellOfferResult::BuyNotAuthorized));
            }
            _ => panic!("Unexpected result type"),
        }
    }

    #[test]
    fn test_manage_sell_offer_delete_allowed_when_not_authorized() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let mut context = create_test_context();
        context.protocol_version = 13;

        let source_id = create_test_account_id(0);
        let issuer_id = create_test_account_id(1);
        state.create_account(create_test_account(source_id.clone(), 100_000_000));
        state.create_account(create_test_account(issuer_id.clone(), 100_000_000));
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

        let create = ManageSellOfferOp {
            selling: asset.clone(),
            buying: Asset::Native,
            amount: 10,
            price: Price { n: 1, d: 1 },
            offer_id: 0,
        };
        let created = execute_manage_sell_offer(&create, &source_id, &mut state, &context).unwrap();
        let offer_id = match created {
            OperationResult::OpInner(OperationResultTr::ManageSellOffer(
                ManageSellOfferResult::Success(ManageOfferSuccessResult { offer, .. }),
            )) => match offer {
                ManageOfferSuccessResultOffer::Created(entry) => entry.offer_id,
                _ => panic!("expected created offer"),
            },
            _ => panic!("unexpected result type"),
        };

        let tl = state
            .get_trustline_by_trustline_asset_mut(
                &source_id,
                &TrustLineAsset::CreditAlphanum4(AlphaNum4 {
                    asset_code: AssetCode4([b'U', b'S', b'D', b'C']),
                    issuer: issuer_id.clone(),
                }),
            )
            .expect("trustline exists");
        tl.flags = AUTHORIZED_TO_MAINTAIN_LIABILITIES_FLAG;

        let delete = ManageSellOfferOp {
            selling: asset,
            buying: Asset::Native,
            amount: 0,
            price: Price { n: 1, d: 1 },
            offer_id,
        };

        let result = execute_manage_sell_offer(&delete, &source_id, &mut state, &context).unwrap();
        match result {
            OperationResult::OpInner(OperationResultTr::ManageSellOffer(
                ManageSellOfferResult::Success(ManageOfferSuccessResult { offer, .. }),
            )) => assert!(matches!(offer, ManageOfferSuccessResultOffer::Deleted)),
            _ => panic!("Unexpected result type"),
        }
        assert!(state.get_offer(&source_id, offer_id).is_none());
    }

    #[test]
    fn test_manage_sell_offer_cross_self() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(0);
        let issuer_id = create_test_account_id(1);
        state.create_account(create_test_account(source_id.clone(), 100_000_000));
        state.create_account(create_test_account(issuer_id.clone(), 100_000_000));

        let usd = Asset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4([b'U', b'S', b'D', b'X']),
            issuer: issuer_id.clone(),
        });
        let idr = Asset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4([b'I', b'D', b'R', b'X']),
            issuer: issuer_id.clone(),
        });

        state.create_trustline(create_test_trustline(
            source_id.clone(),
            TrustLineAsset::CreditAlphanum4(AlphaNum4 {
                asset_code: AssetCode4([b'U', b'S', b'D', b'X']),
                issuer: issuer_id.clone(),
            }),
            1_000,
            1_000_000,
            AUTHORIZED_FLAG,
        ));
        state.create_trustline(create_test_trustline(
            source_id.clone(),
            TrustLineAsset::CreditAlphanum4(AlphaNum4 {
                asset_code: AssetCode4([b'I', b'D', b'R', b'X']),
                issuer: issuer_id.clone(),
            }),
            1_000,
            1_000_000,
            AUTHORIZED_FLAG,
        ));

        state.create_offer(OfferEntry {
            seller_id: source_id.clone(),
            offer_id: 1,
            selling: idr.clone(),
            buying: usd.clone(),
            amount: 100,
            price: Price { n: 1, d: 1 },
            flags: 0,
            ext: OfferEntryExt::V0,
        });

        let op = ManageSellOfferOp {
            selling: usd,
            buying: idr,
            amount: 10,
            price: Price { n: 1, d: 1 },
            offer_id: 0,
        };

        let result = execute_manage_sell_offer(&op, &source_id, &mut state, &context).unwrap();
        match result {
            OperationResult::OpInner(OperationResultTr::ManageSellOffer(r)) => {
                assert!(matches!(r, ManageSellOfferResult::CrossSelf), "{r:?}");
            }
            _ => panic!("Unexpected result type"),
        }
    }

    #[test]
    fn test_manage_buy_offer_cross_self() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(0);
        let issuer_id = create_test_account_id(1);
        state.create_account(create_test_account(source_id.clone(), 100_000_000));
        state.create_account(create_test_account(issuer_id.clone(), 100_000_000));

        let usd = Asset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4([b'U', b'S', b'D', b'X']),
            issuer: issuer_id.clone(),
        });
        let idr = Asset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4([b'I', b'D', b'R', b'X']),
            issuer: issuer_id.clone(),
        });

        state.create_trustline(create_test_trustline(
            source_id.clone(),
            TrustLineAsset::CreditAlphanum4(AlphaNum4 {
                asset_code: AssetCode4([b'U', b'S', b'D', b'X']),
                issuer: issuer_id.clone(),
            }),
            1_000,
            1_000_000,
            AUTHORIZED_FLAG,
        ));
        state.create_trustline(create_test_trustline(
            source_id.clone(),
            TrustLineAsset::CreditAlphanum4(AlphaNum4 {
                asset_code: AssetCode4([b'I', b'D', b'R', b'X']),
                issuer: issuer_id.clone(),
            }),
            1_000,
            1_000_000,
            AUTHORIZED_FLAG,
        ));

        state.create_offer(OfferEntry {
            seller_id: source_id.clone(),
            offer_id: 1,
            selling: idr.clone(),
            buying: usd.clone(),
            amount: 100,
            price: Price { n: 1, d: 1 },
            flags: 0,
            ext: OfferEntryExt::V0,
        });

        let op = ManageBuyOfferOp {
            selling: usd,
            buying: idr,
            buy_amount: 10,
            price: Price { n: 1, d: 1 },
            offer_id: 0,
        };

        let result = execute_manage_buy_offer(&op, &source_id, &mut state, &context).unwrap();
        match result {
            OperationResult::OpInner(OperationResultTr::ManageBuyOffer(r)) => {
                assert!(matches!(r, ManageBuyOfferResult::CrossSelf));
            }
            _ => panic!("Unexpected result type"),
        }
    }

    fn manage_buy_offer_amount(price: Price, buy_amount: i64) -> i64 {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(0);
        let issuer_id = create_test_account_id(1);
        state.create_account(create_test_account(source_id.clone(), 100_000_000));
        state.create_account(create_test_account(issuer_id.clone(), 100_000_000));

        let selling = Asset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4([b'C', b'U', b'R', b'1']),
            issuer: issuer_id.clone(),
        });
        let buying = Asset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4([b'C', b'U', b'R', b'2']),
            issuer: issuer_id.clone(),
        });

        state.create_trustline(create_test_trustline(
            source_id.clone(),
            TrustLineAsset::CreditAlphanum4(AlphaNum4 {
                asset_code: AssetCode4([b'C', b'U', b'R', b'1']),
                issuer: issuer_id.clone(),
            }),
            100,
            100,
            AUTHORIZED_FLAG,
        ));
        state.create_trustline(create_test_trustline(
            source_id.clone(),
            TrustLineAsset::CreditAlphanum4(AlphaNum4 {
                asset_code: AssetCode4([b'C', b'U', b'R', b'2']),
                issuer: issuer_id.clone(),
            }),
            0,
            100,
            AUTHORIZED_FLAG,
        ));

        let op = ManageBuyOfferOp {
            selling,
            buying,
            buy_amount,
            price,
            offer_id: 0,
        };

        let result = execute_manage_buy_offer(&op, &source_id, &mut state, &context).unwrap();
        match result {
            OperationResult::OpInner(OperationResultTr::ManageBuyOffer(
                ManageBuyOfferResult::Success(ManageOfferSuccessResult { offer, .. }),
            )) => match offer {
                ManageOfferSuccessResultOffer::Created(entry) => entry.amount,
                _ => panic!("expected created offer"),
            },
            other => panic!("unexpected result: {:?}", other),
        }
    }

    fn manage_sell_offer_amount(price: Price, amount: i64, seed: u8) -> i64 {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(seed);
        let issuer_id = create_test_account_id(1);
        state.create_account(create_test_account(source_id.clone(), 100_000_000));
        state.create_account(create_test_account(issuer_id.clone(), 100_000_000));

        let selling = Asset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4([b'C', b'U', b'R', b'3']),
            issuer: issuer_id.clone(),
        });
        let buying = Asset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4([b'C', b'U', b'R', b'4']),
            issuer: issuer_id.clone(),
        });

        state.create_trustline(create_test_trustline(
            source_id.clone(),
            TrustLineAsset::CreditAlphanum4(AlphaNum4 {
                asset_code: AssetCode4([b'C', b'U', b'R', b'3']),
                issuer: issuer_id.clone(),
            }),
            100,
            100,
            AUTHORIZED_FLAG,
        ));
        state.create_trustline(create_test_trustline(
            source_id.clone(),
            TrustLineAsset::CreditAlphanum4(AlphaNum4 {
                asset_code: AssetCode4([b'C', b'U', b'R', b'4']),
                issuer: issuer_id.clone(),
            }),
            0,
            100,
            AUTHORIZED_FLAG,
        ));

        let op = ManageSellOfferOp {
            selling,
            buying,
            amount,
            price,
            offer_id: 0,
        };

        let result = execute_manage_sell_offer(&op, &source_id, &mut state, &context).unwrap();
        match result {
            OperationResult::OpInner(OperationResultTr::ManageSellOffer(
                ManageSellOfferResult::Success(ManageOfferSuccessResult { offer, .. }),
            )) => match offer {
                ManageOfferSuccessResultOffer::Created(entry) => entry.amount,
                _ => panic!("expected created offer"),
            },
            other => panic!("unexpected result: {:?}", other),
        }
    }

    #[test]
    fn test_manage_buy_offer_amount_matches_sell_offer() {
        let cases = [
            (Price { n: 2, d: 5 }, 20, Price { n: 5, d: 2 }, 8),
            (Price { n: 1, d: 2 }, 20, Price { n: 2, d: 1 }, 10),
            (Price { n: 1, d: 1 }, 20, Price { n: 1, d: 1 }, 20),
            (Price { n: 2, d: 1 }, 20, Price { n: 1, d: 2 }, 40),
            (Price { n: 5, d: 2 }, 20, Price { n: 2, d: 5 }, 50),
            (Price { n: 2, d: 5 }, 21, Price { n: 5, d: 2 }, 8),
            (Price { n: 1, d: 2 }, 21, Price { n: 2, d: 1 }, 10),
            (Price { n: 2, d: 1 }, 21, Price { n: 1, d: 2 }, 42),
            (Price { n: 5, d: 2 }, 21, Price { n: 2, d: 5 }, 53),
        ];

        for (buy_price, buy_amount, sell_price, sell_amount) in cases {
            let buy_offer = manage_buy_offer_amount(buy_price, buy_amount);
            let sell_offer = manage_sell_offer_amount(sell_price, sell_amount, 2);
            assert_eq!(buy_offer, sell_offer);
        }
    }

    /// Regression test: new offer selling native with insufficient balance must
    /// return LowReserve, not Underfunded. stellar-core checks LowReserve first (via
    /// createEntryWithPossibleSponsorship in doApply) before Underfunded (via
    /// computeOfferExchangeParameters). The early Underfunded check only applies
    /// to existing offers.
    #[test]
    fn test_manage_sell_offer_new_native_low_reserve_before_underfunded() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(200);
        let issuer_id = create_test_account_id(201);

        // Account has exactly min_balance: can't afford the subentry reserve
        // for a new offer AND also has insufficient native balance to sell.
        // stellar-core returns LowReserve (not Underfunded) because the reserve check
        // happens first.
        let min_balance = state
            .minimum_balance_with_counts(context.protocol_version, 0, 0, 0)
            .unwrap();
        state.create_account(create_test_account(source_id.clone(), min_balance));
        state.create_account(create_test_account(issuer_id.clone(), 100_000_000));

        // Create buying trustline (we're selling native, buying USD)
        let asset = create_asset(&issuer_id);
        state.create_trustline(create_test_trustline(
            source_id.clone(),
            TrustLineAsset::CreditAlphanum4(AlphaNum4 {
                asset_code: AssetCode4([b'U', b'S', b'D', b'C']),
                issuer: issuer_id.clone(),
            }),
            0,
            1_000_000,
            AUTHORIZED_FLAG,
        ));

        let op = ManageSellOfferOp {
            selling: Asset::Native,
            buying: asset,
            amount: 10_000, // selling native
            price: Price { n: 1, d: 1 },
            offer_id: 0, // new offer
        };

        let result = execute_manage_sell_offer(&op, &source_id, &mut state, &context).unwrap();
        match result {
            OperationResult::OpInner(OperationResultTr::ManageSellOffer(r)) => {
                assert!(
                    matches!(r, ManageSellOfferResult::LowReserve),
                    "New offer selling native should return LowReserve (not Underfunded) \
                     when account can't afford reserve for new subentry. Got {:?}",
                    r
                );
            }
            other => panic!("Unexpected result type: {:?}", other),
        }
    }

    #[test]
    fn test_manage_sell_offer_low_reserve() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(0);
        let issuer_id = create_test_account_id(1);
        let min_balance = state
            .minimum_balance_with_counts(context.protocol_version, 0, 0, 0)
            .unwrap();
        state.create_account(create_test_account(source_id.clone(), min_balance));
        state.create_account(create_test_account(issuer_id.clone(), 100_000_000));

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

        let op = ManageSellOfferOp {
            selling: asset.clone(),
            buying: Asset::Native,
            amount: 10,
            price: Price { n: 1, d: 1 },
            offer_id: 0,
        };

        let result = execute_manage_sell_offer(&op, &source_id, &mut state, &context).unwrap();
        match result {
            OperationResult::OpInner(OperationResultTr::ManageSellOffer(r)) => {
                assert!(matches!(r, ManageSellOfferResult::LowReserve));
            }
            _ => panic!("Unexpected result type"),
        }
    }

    #[test]
    fn test_manage_sell_offer_line_full_buying_limit() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(0);
        let issuer_id = create_test_account_id(1);
        state.create_account(create_test_account(source_id.clone(), 100_000_000));
        state.create_account(create_test_account(issuer_id.clone(), 100_000_000));

        let buying_asset = create_asset(&issuer_id);
        state.create_trustline(create_test_trustline(
            source_id.clone(),
            TrustLineAsset::CreditAlphanum4(AlphaNum4 {
                asset_code: AssetCode4([b'U', b'S', b'D', b'C']),
                issuer: issuer_id.clone(),
            }),
            1_000,
            1_000,
            AUTHORIZED_FLAG,
        ));

        let op = ManageSellOfferOp {
            selling: Asset::Native,
            buying: buying_asset,
            amount: 10,
            price: Price { n: 1, d: 1 },
            offer_id: 0,
        };

        let result = execute_manage_sell_offer(&op, &source_id, &mut state, &context).unwrap();
        match result {
            OperationResult::OpInner(OperationResultTr::ManageSellOffer(r)) => {
                assert!(matches!(r, ManageSellOfferResult::LineFull));
            }
            _ => panic!("Unexpected result type"),
        }
    }

    #[test]
    fn test_manage_sell_offer_low_reserve_with_selling_liabilities() {
        // Tests that selling liabilities are subtracted from balance when checking
        // if account can afford a new subentry reserve for a new offer.
        // This is a regression test for a bug where the LowReserve check only
        // compared balance to min_balance without considering liabilities.
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let mut context = create_test_context();
        context.protocol_version = 25;

        let source_id = create_test_account_id(0);
        let issuer_id = create_test_account_id(1);

        // Calculate the exact balance needed: min_balance + 1 subentry + selling liabilities
        // With selling liabilities, the available balance = balance - selling_liabilities
        // needs to be >= min_balance + 1 subentry reserve
        let min_balance = state
            .minimum_balance_with_counts(context.protocol_version, 0, 0, 0)
            .unwrap();
        let subentry_reserve = state
            .minimum_balance_with_counts(context.protocol_version, 1, 0, 0)
            .unwrap()
            - min_balance;

        let selling_liabilities = 1_000_000i64;
        // Set balance to exactly min_balance + subentry_reserve + selling_liabilities - 1
        // This means: available = balance - selling_liabilities = min_balance + subentry_reserve - 1
        // Which is less than the required min_balance + subentry_reserve, so LowReserve should trigger.
        let balance = min_balance + subentry_reserve + selling_liabilities - 1;

        let mut account = create_test_account(source_id.clone(), balance);
        // Add selling liabilities to the account
        account.ext = AccountEntryExt::V1(AccountEntryExtensionV1 {
            liabilities: Liabilities {
                buying: 0,
                selling: selling_liabilities,
            },
            ext: AccountEntryExtensionV1Ext::V0,
        });
        state.create_account(account);
        state.create_account(create_test_account(issuer_id.clone(), 100_000_000));

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

        let op = ManageSellOfferOp {
            selling: asset.clone(),
            buying: Asset::Native,
            amount: 10,
            price: Price { n: 1, d: 1 },
            offer_id: 0,
        };

        let result = execute_manage_sell_offer(&op, &source_id, &mut state, &context).unwrap();
        match result {
            OperationResult::OpInner(OperationResultTr::ManageSellOffer(r)) => {
                assert!(
                    matches!(r, ManageSellOfferResult::LowReserve),
                    "Expected LowReserve when selling liabilities make available balance insufficient, got {:?}",
                    r
                );
            }
            other => panic!("Unexpected result type: {:?}", other),
        }
    }

    /// Test that ManageSellOffer returns OpTooManySubentries when account has reached
    /// the maximum subentries limit (1000).
    ///
    /// C++ Reference: OfferTests.cpp - tooManySubentries tests via SponsorshipTestUtils
    #[test]
    fn test_manage_sell_offer_too_many_subentries() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(100);
        let issuer_id = create_test_account_id(101);

        // Create source account with max subentries (1000)
        let mut source_account = create_test_account(source_id.clone(), 100_000_000);
        source_account.num_sub_entries = ACCOUNT_SUBENTRY_LIMIT; // At the limit
        state.create_account(source_account);
        state.create_account(create_test_account(issuer_id.clone(), 100_000_000));

        // Set up trustline for the selling asset
        let asset = create_asset(&issuer_id);
        state.create_trustline(create_test_trustline(
            source_id.clone(),
            TrustLineAsset::CreditAlphanum4(AlphaNum4 {
                asset_code: AssetCode4([b'U', b'S', b'D', b'C']),
                issuer: issuer_id.clone(),
            }),
            1_000_000, // Has balance to sell
            10_000_000,
            AUTHORIZED_FLAG,
        ));

        // Try to create a new offer (offer_id=0 means new offer)
        let op = ManageSellOfferOp {
            selling: asset,
            buying: Asset::Native,
            amount: 10_000,
            price: Price { n: 1, d: 1 },
            offer_id: 0, // New offer
        };

        let result = execute_manage_sell_offer(&op, &source_id, &mut state, &context);
        match result.unwrap() {
            OperationResult::OpTooManySubentries => {
                // Expected - account has max subentries, can't create new offer
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

    /// Test that ManageBuyOffer returns OpTooManySubentries when account has reached
    /// the maximum subentries limit (1000).
    #[test]
    fn test_manage_buy_offer_too_many_subentries() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(102);
        let issuer_id = create_test_account_id(103);

        // Create source account with max subentries (1000)
        let mut source_account = create_test_account(source_id.clone(), 100_000_000);
        source_account.num_sub_entries = ACCOUNT_SUBENTRY_LIMIT; // At the limit
        state.create_account(source_account);
        state.create_account(create_test_account(issuer_id.clone(), 100_000_000));

        // Set up trustline for the buying asset
        let asset = create_asset(&issuer_id);
        state.create_trustline(create_test_trustline(
            source_id.clone(),
            TrustLineAsset::CreditAlphanum4(AlphaNum4 {
                asset_code: AssetCode4([b'U', b'S', b'D', b'C']),
                issuer: issuer_id.clone(),
            }),
            0,
            10_000_000,
            AUTHORIZED_FLAG,
        ));

        // Try to create a new buy offer (offer_id=0 means new offer)
        let op = ManageBuyOfferOp {
            selling: Asset::Native,
            buying: asset,
            buy_amount: 10_000,
            price: Price { n: 1, d: 1 },
            offer_id: 0, // New offer
        };

        let result = execute_manage_buy_offer(&op, &source_id, &mut state, &context);
        match result.unwrap() {
            OperationResult::OpTooManySubentries => {
                // Expected - account has max subentries, can't create new offer
            }
            other => panic!("expected OpTooManySubentries, got {:?}", other),
        }
    }

    /// Test that CreatePassiveSellOffer returns OpTooManySubentries when account has reached
    /// the maximum subentries limit (1000).
    #[test]
    fn test_create_passive_sell_offer_too_many_subentries() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(104);
        let issuer_id = create_test_account_id(105);

        // Create source account with max subentries (1000)
        let mut source_account = create_test_account(source_id.clone(), 100_000_000);
        source_account.num_sub_entries = ACCOUNT_SUBENTRY_LIMIT; // At the limit
        state.create_account(source_account);
        state.create_account(create_test_account(issuer_id.clone(), 100_000_000));

        // Set up trustline for the selling asset
        let asset = create_asset(&issuer_id);
        state.create_trustline(create_test_trustline(
            source_id.clone(),
            TrustLineAsset::CreditAlphanum4(AlphaNum4 {
                asset_code: AssetCode4([b'U', b'S', b'D', b'C']),
                issuer: issuer_id.clone(),
            }),
            1_000_000,
            10_000_000,
            AUTHORIZED_FLAG,
        ));

        // Try to create a passive offer
        let op = CreatePassiveSellOfferOp {
            selling: asset,
            buying: Asset::Native,
            amount: 10_000,
            price: Price { n: 1, d: 1 },
        };

        let result = execute_create_passive_sell_offer(&op, &source_id, &mut state, &context);
        match result.unwrap() {
            OperationResult::OpTooManySubentries => {
                // Expected - account has max subentries, can't create new offer
            }
            other => panic!("expected OpTooManySubentries, got {:?}", other),
        }
    }

    /// Test ManageSellOffer with negative amount returns Malformed.
    ///
    /// C++ Reference: OfferTests.cpp - "malformed negative amount" test section
    #[test]
    fn test_manage_sell_offer_malformed_negative_amount() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(110);
        state.create_account(create_test_account(source_id.clone(), 100_000_000));

        let op = ManageSellOfferOp {
            selling: Asset::Native,
            buying: Asset::Native,
            amount: -1,
            price: Price { n: 1, d: 1 },
            offer_id: 0,
        };

        let result = execute_manage_sell_offer(&op, &source_id, &mut state, &context);
        match result.unwrap() {
            OperationResult::OpInner(OperationResultTr::ManageSellOffer(r)) => {
                assert!(
                    matches!(r, ManageSellOfferResult::Malformed),
                    "Expected Malformed for negative amount, got {:?}",
                    r
                );
            }
            other => panic!("unexpected result: {:?}", other),
        }
    }

    /// Test ManageSellOffer with zero price returns Malformed.
    ///
    /// C++ Reference: OfferTests.cpp - "malformed zero price" test section
    #[test]
    fn test_manage_sell_offer_malformed_zero_price() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(111);
        let issuer_id = create_test_account_id(112);
        state.create_account(create_test_account(source_id.clone(), 100_000_000));
        state.create_account(create_test_account(issuer_id.clone(), 100_000_000));

        let asset = create_asset(&issuer_id);

        let op = ManageSellOfferOp {
            selling: Asset::Native,
            buying: asset,
            amount: 1000,
            price: Price { n: 0, d: 1 }, // Zero price
            offer_id: 0,
        };

        let result = execute_manage_sell_offer(&op, &source_id, &mut state, &context);
        match result.unwrap() {
            OperationResult::OpInner(OperationResultTr::ManageSellOffer(r)) => {
                assert!(
                    matches!(r, ManageSellOfferResult::Malformed),
                    "Expected Malformed for zero price, got {:?}",
                    r
                );
            }
            other => panic!("unexpected result: {:?}", other),
        }
    }

    /// Test ManageSellOffer with zero denominator returns Malformed.
    ///
    /// C++ Reference: OfferTests.cpp - "malformed invalid price" test section
    #[test]
    fn test_manage_sell_offer_malformed_zero_denominator() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(113);
        let issuer_id = create_test_account_id(114);
        state.create_account(create_test_account(source_id.clone(), 100_000_000));
        state.create_account(create_test_account(issuer_id.clone(), 100_000_000));

        let asset = create_asset(&issuer_id);

        let op = ManageSellOfferOp {
            selling: Asset::Native,
            buying: asset,
            amount: 1000,
            price: Price { n: 1, d: 0 }, // Zero denominator
            offer_id: 0,
        };

        let result = execute_manage_sell_offer(&op, &source_id, &mut state, &context);
        match result.unwrap() {
            OperationResult::OpInner(OperationResultTr::ManageSellOffer(r)) => {
                assert!(
                    matches!(r, ManageSellOfferResult::Malformed),
                    "Expected Malformed for zero denominator, got {:?}",
                    r
                );
            }
            other => panic!("unexpected result: {:?}", other),
        }
    }

    /// Test ManageSellOffer sell no trust - selling asset without trustline.
    ///
    /// C++ Reference: OfferTests.cpp - "sell no trust" test section
    #[test]
    fn test_manage_sell_offer_sell_no_trust() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(115);
        let issuer_id = create_test_account_id(116);
        state.create_account(create_test_account(source_id.clone(), 100_000_000));
        state.create_account(create_test_account(issuer_id.clone(), 100_000_000));

        let asset = create_asset(&issuer_id);

        // No trustline for the selling asset
        let op = ManageSellOfferOp {
            selling: asset,
            buying: Asset::Native,
            amount: 1000,
            price: Price { n: 1, d: 1 },
            offer_id: 0,
        };

        let result = execute_manage_sell_offer(&op, &source_id, &mut state, &context);
        match result.unwrap() {
            OperationResult::OpInner(OperationResultTr::ManageSellOffer(r)) => {
                assert!(
                    matches!(r, ManageSellOfferResult::SellNoTrust),
                    "Expected SellNoTrust, got {:?}",
                    r
                );
            }
            other => panic!("unexpected result: {:?}", other),
        }
    }

    /// Test ManageSellOffer buy no trust - buying asset without trustline.
    ///
    /// C++ Reference: OfferTests.cpp - "buy no trust" test section
    #[test]
    fn test_manage_sell_offer_buy_no_trust() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(117);
        let issuer_id = create_test_account_id(118);
        state.create_account(create_test_account(source_id.clone(), 100_000_000));
        state.create_account(create_test_account(issuer_id.clone(), 100_000_000));

        let asset = create_asset(&issuer_id);

        // No trustline for the buying asset
        let op = ManageSellOfferOp {
            selling: Asset::Native,
            buying: asset,
            amount: 1000,
            price: Price { n: 1, d: 1 },
            offer_id: 0,
        };

        let result = execute_manage_sell_offer(&op, &source_id, &mut state, &context);
        match result.unwrap() {
            OperationResult::OpInner(OperationResultTr::ManageSellOffer(r)) => {
                assert!(
                    matches!(r, ManageSellOfferResult::BuyNoTrust),
                    "Expected BuyNoTrust, got {:?}",
                    r
                );
            }
            other => panic!("unexpected result: {:?}", other),
        }
    }

    /// Test ManageSellOffer underfunded - selling credit asset with zero balance.
    ///
    /// C++ Reference: OfferTests.cpp - "underfunded" test section
    #[test]
    fn test_manage_sell_offer_underfunded_zero_balance() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(119);
        let issuer_id = create_test_account_id(120);
        state.create_account(create_test_account(source_id.clone(), 100_000_000));
        state.create_account(create_test_account(issuer_id.clone(), 100_000_000));

        let asset = create_asset(&issuer_id);

        // Create trustline with zero balance
        state.create_trustline(create_test_trustline(
            source_id.clone(),
            TrustLineAsset::CreditAlphanum4(AlphaNum4 {
                asset_code: AssetCode4([b'U', b'S', b'D', b'C']),
                issuer: issuer_id.clone(),
            }),
            0, // Zero balance
            10_000_000,
            AUTHORIZED_FLAG,
        ));

        let op = ManageSellOfferOp {
            selling: asset,
            buying: Asset::Native,
            amount: 1000,
            price: Price { n: 1, d: 1 },
            offer_id: 0,
        };

        let result = execute_manage_sell_offer(&op, &source_id, &mut state, &context);
        match result.unwrap() {
            OperationResult::OpInner(OperationResultTr::ManageSellOffer(r)) => {
                assert!(
                    matches!(r, ManageSellOfferResult::Underfunded),
                    "Expected Underfunded for zero balance, got {:?}",
                    r
                );
            }
            other => panic!("unexpected result: {:?}", other),
        }
    }

    /// Test ManageSellOffer not found - updating non-existent offer.
    ///
    /// C++ Reference: OfferTests.cpp - "not found" test section
    #[test]
    fn test_manage_sell_offer_not_found() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(121);
        let issuer_id = create_test_account_id(122);
        state.create_account(create_test_account(source_id.clone(), 100_000_000));
        state.create_account(create_test_account(issuer_id.clone(), 100_000_000));

        let asset = create_asset(&issuer_id);

        // Create trustline
        state.create_trustline(create_test_trustline(
            source_id.clone(),
            TrustLineAsset::CreditAlphanum4(AlphaNum4 {
                asset_code: AssetCode4([b'U', b'S', b'D', b'C']),
                issuer: issuer_id.clone(),
            }),
            1_000_000,
            10_000_000,
            AUTHORIZED_FLAG,
        ));

        // Try to update non-existent offer
        let op = ManageSellOfferOp {
            selling: asset,
            buying: Asset::Native,
            amount: 1000,
            price: Price { n: 1, d: 1 },
            offer_id: 999999, // Non-existent offer ID
        };

        let result = execute_manage_sell_offer(&op, &source_id, &mut state, &context);
        match result.unwrap() {
            OperationResult::OpInner(OperationResultTr::ManageSellOffer(r)) => {
                assert!(
                    matches!(r, ManageSellOfferResult::NotFound),
                    "Expected NotFound, got {:?}",
                    r
                );
            }
            other => panic!("unexpected result: {:?}", other),
        }
    }

    /// Test ManageSellOffer delete - deleting existing offer.
    ///
    /// C++ Reference: OfferTests.cpp - "delete offer" test section
    #[test]
    fn test_manage_sell_offer_delete() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(123);
        let issuer_id = create_test_account_id(124);
        state.create_account(create_test_account(source_id.clone(), 100_000_000));
        state.create_account(create_test_account(issuer_id.clone(), 100_000_000));

        let asset = create_asset(&issuer_id);

        // Create trustline
        state.create_trustline(create_test_trustline(
            source_id.clone(),
            TrustLineAsset::CreditAlphanum4(AlphaNum4 {
                asset_code: AssetCode4([b'U', b'S', b'D', b'C']),
                issuer: issuer_id.clone(),
            }),
            1_000_000,
            10_000_000,
            AUTHORIZED_FLAG,
        ));
        state.get_account_mut(&source_id).unwrap().num_sub_entries += 1;

        // First create an offer
        let create_op = ManageSellOfferOp {
            selling: asset.clone(),
            buying: Asset::Native,
            amount: 1000,
            price: Price { n: 1, d: 1 },
            offer_id: 0,
        };

        let create_result =
            execute_manage_sell_offer(&create_op, &source_id, &mut state, &context).unwrap();
        let offer_id = match create_result {
            OperationResult::OpInner(OperationResultTr::ManageSellOffer(
                ManageSellOfferResult::Success(success),
            )) => match success.offer {
                ManageOfferSuccessResultOffer::Created(offer)
                | ManageOfferSuccessResultOffer::Updated(offer) => offer.offer_id,
                ManageOfferSuccessResultOffer::Deleted => {
                    panic!("Expected offer to be created, not deleted")
                }
            },
            other => panic!("Expected success, got {:?}", other),
        };

        // Verify offer exists
        assert!(state.get_offer(&source_id, offer_id).is_some());

        // Now delete the offer
        let delete_op = ManageSellOfferOp {
            selling: asset,
            buying: Asset::Native,
            amount: 0, // Zero amount = delete
            price: Price { n: 1, d: 1 },
            offer_id,
        };

        let delete_result =
            execute_manage_sell_offer(&delete_op, &source_id, &mut state, &context).unwrap();
        match delete_result {
            OperationResult::OpInner(OperationResultTr::ManageSellOffer(
                ManageSellOfferResult::Success(success),
            )) => {
                assert!(
                    matches!(success.offer, ManageOfferSuccessResultOffer::Deleted),
                    "Expected offer to be deleted"
                );
            }
            other => panic!("Expected success with deleted offer, got {:?}", other),
        }

        // Verify offer is gone
        assert!(state.get_offer(&source_id, offer_id).is_none());
    }

    /// Test ManageSellOffer update - updating existing offer.
    ///
    /// C++ Reference: OfferTests.cpp - "update offer" test section
    #[test]
    fn test_manage_sell_offer_update() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(125);
        let issuer_id = create_test_account_id(126);
        state.create_account(create_test_account(source_id.clone(), 100_000_000));
        state.create_account(create_test_account(issuer_id.clone(), 100_000_000));

        let asset = create_asset(&issuer_id);

        // Create trustline
        state.create_trustline(create_test_trustline(
            source_id.clone(),
            TrustLineAsset::CreditAlphanum4(AlphaNum4 {
                asset_code: AssetCode4([b'U', b'S', b'D', b'C']),
                issuer: issuer_id.clone(),
            }),
            1_000_000,
            10_000_000,
            AUTHORIZED_FLAG,
        ));
        state.get_account_mut(&source_id).unwrap().num_sub_entries += 1;

        // First create an offer
        let create_op = ManageSellOfferOp {
            selling: asset.clone(),
            buying: Asset::Native,
            amount: 1000,
            price: Price { n: 1, d: 1 },
            offer_id: 0,
        };

        let create_result =
            execute_manage_sell_offer(&create_op, &source_id, &mut state, &context).unwrap();
        let offer_id = match create_result {
            OperationResult::OpInner(OperationResultTr::ManageSellOffer(
                ManageSellOfferResult::Success(success),
            )) => match success.offer {
                ManageOfferSuccessResultOffer::Created(offer)
                | ManageOfferSuccessResultOffer::Updated(offer) => offer.offer_id,
                ManageOfferSuccessResultOffer::Deleted => {
                    panic!("Expected offer to be created, not deleted")
                }
            },
            other => panic!("Expected success, got {:?}", other),
        };

        // Update the offer with new amount and price
        let update_op = ManageSellOfferOp {
            selling: asset,
            buying: Asset::Native,
            amount: 2000,
            price: Price { n: 2, d: 1 },
            offer_id,
        };

        let update_result =
            execute_manage_sell_offer(&update_op, &source_id, &mut state, &context).unwrap();
        match update_result {
            OperationResult::OpInner(OperationResultTr::ManageSellOffer(
                ManageSellOfferResult::Success(success),
            )) => match success.offer {
                ManageOfferSuccessResultOffer::Updated(offer) => {
                    assert_eq!(offer.amount, 2000);
                    assert_eq!(offer.price, Price { n: 2, d: 1 });
                }
                other => panic!("Expected Updated, got {:?}", other),
            },
            other => panic!("Expected success, got {:?}", other),
        }
    }

    /// Test CreatePassiveSellOffer basic success.
    ///
    /// C++ Reference: OfferTests.cpp - "passive offer basic" test section
    #[test]
    fn test_create_passive_sell_offer_basic() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(127);
        let issuer_id = create_test_account_id(128);
        state.create_account(create_test_account(source_id.clone(), 100_000_000));
        state.create_account(create_test_account(issuer_id.clone(), 100_000_000));

        let asset = create_asset(&issuer_id);

        // Create trustline
        state.create_trustline(create_test_trustline(
            source_id.clone(),
            TrustLineAsset::CreditAlphanum4(AlphaNum4 {
                asset_code: AssetCode4([b'U', b'S', b'D', b'C']),
                issuer: issuer_id.clone(),
            }),
            1_000_000,
            10_000_000,
            AUTHORIZED_FLAG,
        ));
        state.get_account_mut(&source_id).unwrap().num_sub_entries += 1;

        let op = CreatePassiveSellOfferOp {
            selling: asset,
            buying: Asset::Native,
            amount: 10_000,
            price: Price { n: 1, d: 1 },
        };

        let result = execute_create_passive_sell_offer(&op, &source_id, &mut state, &context);
        match result.unwrap() {
            OperationResult::OpInner(OperationResultTr::ManageSellOffer(
                ManageSellOfferResult::Success(success),
            )) => match success.offer {
                ManageOfferSuccessResultOffer::Created(offer) => {
                    assert_eq!(offer.amount, 10_000);
                    // Passive offers have the PASSIVE_FLAG set
                    assert!(offer.flags & (OfferEntryFlags::PassiveFlag as u32) != 0);
                }
                other => panic!("Expected Created, got {:?}", other),
            },
            other => panic!("Expected success, got {:?}", other),
        }
    }

    /// Test ManageBuyOffer basic success.
    ///
    /// C++ Reference: ManageBuyOfferTests.cpp - "basic" test section
    #[test]
    fn test_manage_buy_offer_basic() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(129);
        let issuer_id = create_test_account_id(130);
        state.create_account(create_test_account(source_id.clone(), 100_000_000));
        state.create_account(create_test_account(issuer_id.clone(), 100_000_000));

        let asset = create_asset(&issuer_id);

        // Create trustline for buying
        state.create_trustline(create_test_trustline(
            source_id.clone(),
            TrustLineAsset::CreditAlphanum4(AlphaNum4 {
                asset_code: AssetCode4([b'U', b'S', b'D', b'C']),
                issuer: issuer_id.clone(),
            }),
            0,
            10_000_000,
            AUTHORIZED_FLAG,
        ));
        state.get_account_mut(&source_id).unwrap().num_sub_entries += 1;

        let op = ManageBuyOfferOp {
            selling: Asset::Native,
            buying: asset,
            buy_amount: 10_000, // Amount to buy
            price: Price { n: 1, d: 1 },
            offer_id: 0,
        };

        let result = execute_manage_buy_offer(&op, &source_id, &mut state, &context);
        match result.unwrap() {
            OperationResult::OpInner(OperationResultTr::ManageBuyOffer(
                stellar_xdr::curr::ManageBuyOfferResult::Success(_),
            )) => {
                // Success
            }
            other => panic!("Expected success, got {:?}", other),
        }
    }

    /// Regression test for the "fully consumed new offer" bug.
    ///
    /// When a new ManageSellOffer (or ManageBuyOffer) is submitted and gets
    /// fully consumed during matching against existing offers, stellar-core still
    /// records the source account as accessed (via loadAccount before the
    /// exchange). Our code must also call record_account_access so the
    /// account appears in the delta with updated lastModified.
    #[test]
    fn test_new_offer_fully_consumed_records_source_account() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        // Create issuer for two non-native assets
        let issuer_id = create_test_account_id(99);
        state.create_account(create_test_account(issuer_id.clone(), 1_000_000_000));

        let asset_a = Asset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4(*b"USDC"),
            issuer: issuer_id.clone(),
        });
        let tl_asset_a = TrustLineAsset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4(*b"USDC"),
            issuer: issuer_id.clone(),
        });
        let asset_b = Asset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4(*b"EURX"),
            issuer: issuer_id.clone(),
        });
        let tl_asset_b = TrustLineAsset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4(*b"EURX"),
            issuer: issuer_id.clone(),
        });

        // Counterparty: has a sell offer selling USDC for EURX
        let counter_id = create_test_account_id(1);
        let min_balance = state
            .minimum_balance_with_counts(context.protocol_version, 3, 0, 0)
            .unwrap();
        state.create_account(create_test_account(counter_id.clone(), min_balance));

        // Counterparty trustlines (with V1 ext for liabilities)
        state.create_trustline(TrustLineEntry {
            account_id: counter_id.clone(),
            asset: tl_asset_a.clone(),
            balance: 500_000_000, // 500 USDC
            limit: i64::MAX,
            flags: AUTHORIZED_FLAG,
            ext: TrustLineEntryExt::V1(TrustLineEntryV1 {
                liabilities: Liabilities {
                    buying: 0,
                    selling: 100_000_000, // Matches the offer amount
                },
                ext: TrustLineEntryV1Ext::V0,
            }),
        });
        state.create_trustline(TrustLineEntry {
            account_id: counter_id.clone(),
            asset: tl_asset_b.clone(),
            balance: 0,
            limit: i64::MAX,
            flags: AUTHORIZED_FLAG,
            ext: TrustLineEntryExt::V1(TrustLineEntryV1 {
                liabilities: Liabilities {
                    buying: 100_000_000, // Matches what the offer buys
                    selling: 0,
                },
                ext: TrustLineEntryV1Ext::V0,
            }),
        });

        // Counterparty offer: selling 100 USDC for EURX at price 1:1
        let counter_offer = OfferEntry {
            seller_id: counter_id.clone(),
            offer_id: 1000,
            selling: asset_a.clone(),
            buying: asset_b.clone(),
            amount: 100_000_000, // 100 USDC
            price: Price { n: 1, d: 1 },
            flags: 0,
            ext: OfferEntryExt::V0,
        };
        // Load the offer as pre-existing (not created in this ledger)
        state.load_entry(LedgerEntry {
            last_modified_ledger_seq: 840000,
            data: LedgerEntryData::Offer(counter_offer),
            ext: LedgerEntryExt::V0,
        });

        // Bump counterparty's num_sub_entries to account for the offer + 2 trustlines
        if let Some(acct) = state.get_account_mut(&counter_id) {
            acct.num_sub_entries = 3;
        }

        // Source account: will submit a new offer that gets fully consumed
        let source_id = create_test_account_id(2);
        state.create_account(create_test_account(source_id.clone(), min_balance));

        // Source trustlines
        state.create_trustline(create_test_trustline(
            source_id.clone(),
            tl_asset_a.clone(),
            0,
            i64::MAX,
            AUTHORIZED_FLAG,
        ));
        state.create_trustline(create_test_trustline(
            source_id.clone(),
            tl_asset_b.clone(),
            500_000_000, // 500 EURX
            i64::MAX,
            AUTHORIZED_FLAG,
        ));

        // Record source's initial num_sub_entries (2 trustlines)
        if let Some(acct) = state.get_account_mut(&source_id) {
            acct.num_sub_entries = 2;
        }

        // Clear any snapshots from setup
        state.flush_modified_entries();
        state.begin_op_snapshot();

        // Source submits: sell 100 EURX for USDC at price 1:1
        // This should match the counterparty's offer exactly and be fully consumed.
        let op = ManageSellOfferOp {
            selling: asset_b.clone(),
            buying: asset_a.clone(),
            amount: 100_000_000, // Same as counterparty's offer
            price: Price { n: 1, d: 1 },
            offer_id: 0, // New offer
        };

        let result = execute_manage_sell_offer(&op, &source_id, &mut state, &context).unwrap();

        // Verify operation succeeded
        match &result {
            OperationResult::OpInner(OperationResultTr::ManageSellOffer(
                ManageSellOfferResult::Success(success),
            )) => {
                // Offer should be deleted (fully consumed)
                assert!(
                    matches!(success.offer, ManageOfferSuccessResultOffer::Deleted),
                    "Expected offer to be fully consumed (Deleted), got {:?}",
                    success.offer
                );
                // Should have claimed the counterparty's offer
                assert_eq!(success.offers_claimed.len(), 1, "Expected 1 offer claimed");
            }
            other => panic!("Expected success, got {:?}", other),
        }

        // Verify source account num_sub_entries is unchanged (no offer was created)
        let source_acct = state.get_account(&source_id).unwrap();
        assert_eq!(
            source_acct.num_sub_entries, 2,
            "num_sub_entries should be unchanged (offer was fully consumed)"
        );

        // Key assertion: the source account must appear in the op snapshot,
        // meaning it was recorded as accessed. This matches stellar-core behavior where
        // loadAccount is called before the exchange for new offers (V14+).
        let op_snapshots = state.end_op_snapshot();
        let source_account_key = LedgerKey::Account(stellar_xdr::curr::LedgerKeyAccount {
            account_id: source_id.clone(),
        });
        assert!(
            op_snapshots.contains_key(&source_account_key),
            "Source account must be recorded in op snapshot for fully consumed new offers"
        );
    }

    /// Test that when a sponsored new offer is fully consumed during matching,
    /// the sponsor account is recorded in the op snapshot (gets lm stamped).
    ///
    /// In stellar-core (V14+), createEntryWithPossibleSponsorship is called before the
    /// exchange (numSponsoring++ on sponsor), and removeEntryWithPossibleSponsorship
    /// is called after (numSponsoring-- on sponsor). The net effect on the sponsor
    /// is zero, but the account is loaded with tracking and gets lastModified updated.
    ///
    /// Found at mainnet ledger 61153284.
    #[test]
    fn test_sponsored_new_offer_fully_consumed_records_sponsor_account() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        // Create issuer for two non-native assets
        let issuer_id = create_test_account_id(99);
        state.create_account(create_test_account(issuer_id.clone(), 1_000_000_000));

        let asset_a = Asset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4(*b"USDC"),
            issuer: issuer_id.clone(),
        });
        let asset_b = Asset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4(*b"EURX"),
            issuer: issuer_id.clone(),
        });
        let tl_asset_a = TrustLineAsset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4(*b"USDC"),
            issuer: issuer_id.clone(),
        });
        let tl_asset_b = TrustLineAsset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4(*b"EURX"),
            issuer: issuer_id.clone(),
        });

        let min_balance = state
            .minimum_balance_with_counts(context.protocol_version, 10, 0, 0)
            .unwrap();

        // Counterparty with existing offer on the book
        let counter_id = create_test_account_id(1);
        state.create_account(create_test_account(counter_id.clone(), min_balance));
        state.create_trustline(TrustLineEntry {
            account_id: counter_id.clone(),
            asset: tl_asset_a.clone(),
            balance: 500_000_000,
            limit: i64::MAX,
            flags: AUTHORIZED_FLAG,
            ext: TrustLineEntryExt::V1(TrustLineEntryV1 {
                liabilities: Liabilities {
                    buying: 0,
                    selling: 100_000_000,
                },
                ext: TrustLineEntryV1Ext::V0,
            }),
        });
        state.create_trustline(TrustLineEntry {
            account_id: counter_id.clone(),
            asset: tl_asset_b.clone(),
            balance: 0,
            limit: i64::MAX,
            flags: AUTHORIZED_FLAG,
            ext: TrustLineEntryExt::V1(TrustLineEntryV1 {
                liabilities: Liabilities {
                    buying: 100_000_000,
                    selling: 0,
                },
                ext: TrustLineEntryV1Ext::V0,
            }),
        });

        let counter_offer = OfferEntry {
            seller_id: counter_id.clone(),
            offer_id: 1000,
            selling: asset_a.clone(),
            buying: asset_b.clone(),
            amount: 100_000_000,
            price: Price { n: 1, d: 1 },
            flags: 0,
            ext: OfferEntryExt::V0,
        };
        state.load_entry(LedgerEntry {
            last_modified_ledger_seq: 840000,
            data: LedgerEntryData::Offer(counter_offer),
            ext: LedgerEntryExt::V0,
        });

        if let Some(acct) = state.get_account_mut(&counter_id) {
            acct.num_sub_entries = 3;
        }

        // Source account: will submit a new offer that gets fully consumed
        let source_id = create_test_account_id(2);
        state.create_account(create_test_account(source_id.clone(), min_balance));
        state.create_trustline(create_test_trustline(
            source_id.clone(),
            tl_asset_a.clone(),
            0,
            i64::MAX,
            AUTHORIZED_FLAG,
        ));
        state.create_trustline(create_test_trustline(
            source_id.clone(),
            tl_asset_b.clone(),
            500_000_000,
            i64::MAX,
            AUTHORIZED_FLAG,
        ));

        if let Some(acct) = state.get_account_mut(&source_id) {
            acct.num_sub_entries = 2;
        }

        // Sponsor account: will sponsor the source's offer
        let sponsor_id = create_test_account_id(3);
        state.create_account(create_test_account(sponsor_id.clone(), min_balance * 10));

        // Set up sponsorship: sponsor_id sponsors source_id's entries
        state.push_sponsorship(sponsor_id.clone(), source_id.clone());

        // Clear any snapshots from setup
        state.flush_modified_entries();
        state.begin_op_snapshot();

        // Source submits: sell 100 EURX for USDC at price 1:1
        // This should match the counterparty's offer exactly and be fully consumed.
        let op = ManageSellOfferOp {
            selling: asset_b.clone(),
            buying: asset_a.clone(),
            amount: 100_000_000,
            price: Price { n: 1, d: 1 },
            offer_id: 0, // New offer
        };

        let result = execute_manage_sell_offer(&op, &source_id, &mut state, &context).unwrap();

        // Verify operation succeeded with offer fully consumed
        match &result {
            OperationResult::OpInner(OperationResultTr::ManageSellOffer(
                ManageSellOfferResult::Success(success),
            )) => {
                assert!(
                    matches!(success.offer, ManageOfferSuccessResultOffer::Deleted),
                    "Expected offer to be fully consumed (Deleted), got {:?}",
                    success.offer
                );
                assert_eq!(success.offers_claimed.len(), 1, "Expected 1 offer claimed");
            }
            other => panic!("Expected success, got {:?}", other),
        }

        let op_snapshots = state.end_op_snapshot();

        // Key assertion: the SPONSOR account must appear in the op snapshot.
        // In stellar-core, createEntryWithPossibleSponsorship loads the sponsor (numSponsoring++),
        // and removeEntryWithPossibleSponsorship loads it again (numSponsoring--).
        // Net effect is zero but the sponsor gets lm stamped.
        let sponsor_account_key = LedgerKey::Account(stellar_xdr::curr::LedgerKeyAccount {
            account_id: sponsor_id.clone(),
        });
        assert!(
            op_snapshots.contains_key(&sponsor_account_key),
            "Sponsor account must be recorded in op snapshot for fully consumed sponsored new offers"
        );

        // Source account should also be in op snapshot
        let source_account_key = LedgerKey::Account(stellar_xdr::curr::LedgerKeyAccount {
            account_id: source_id.clone(),
        });
        assert!(
            op_snapshots.contains_key(&source_account_key),
            "Source account must be recorded in op snapshot"
        );
    }

    /// Regression test: when both LineFull and Underfunded conditions are true,
    /// the result must be LineFull (not Underfunded) to match stellar-core's
    /// check ordering in ManageOfferOpFrameBase.cpp.
    ///
    /// Bug: L59549719 TX 36 returned ManageBuyOffer(Underfunded) instead of
    /// ManageBuyOffer(LineFull) because we checked Underfunded before LineFull.
    #[test]
    fn test_manage_offer_line_full_before_underfunded() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(0);
        // Give account very low balance: (2 + 2 sub_entries) * base_reserve = 4 * 5_000_000 = 20_000_000
        // This leaves 0 available balance for selling, triggering Underfunded.
        state.create_account(create_test_account(source_id.clone(), 20_000_000));

        let issuer_id = create_test_account_id(99);
        state.create_account(create_test_account(issuer_id.clone(), 100_000_000));

        let buying_asset = create_asset(&issuer_id);

        // Create buying trustline with balance == limit (LineFull condition)
        state.create_trustline(create_test_trustline(
            source_id.clone(),
            TrustLineAsset::CreditAlphanum4(AlphaNum4 {
                asset_code: AssetCode4([b'U', b'S', b'D', b'C']),
                issuer: issuer_id.clone(),
            }),
            1000, // balance
            1000, // limit == balance, so no room for buying
            AUTHORIZED_FLAG,
        ));

        // Sell native (Underfunded because balance is at minimum reserve)
        // Buy the asset (LineFull because trustline is at limit)
        // Both conditions fail, but LineFull should be returned first.
        let op = ManageSellOfferOp {
            selling: Asset::Native,
            buying: buying_asset,
            amount: 10_000_000,
            price: Price { n: 1, d: 1 },
            offer_id: 0,
        };

        let result = execute_manage_sell_offer(&op, &source_id, &mut state, &context);
        assert!(result.is_ok());

        match result.unwrap() {
            OperationResult::OpInner(OperationResultTr::ManageSellOffer(r)) => {
                assert!(
                    matches!(r, ManageSellOfferResult::LineFull),
                    "Must return LineFull (not Underfunded) when both conditions fail, \
                     matching stellar-core check ordering. Got: {:?}",
                    r
                );
            }
            other => panic!("Unexpected result type: {:?}", other),
        }
    }

    /// Regression test for AUDIT-011: CAP-77 frozen offer filter must run AFTER
    /// price/cross-self filters, matching stellar-core ordering.
    ///
    /// Scenario: seller has an offer selling IDR for USD at price 2:1. The seller's
    /// IDR trustline is frozen. Buyer places a sell offer USD→IDR at price 1:1
    /// (max_wheat_price = 1:1). The seller's offer has price 2:1 which exceeds the
    /// buyer's max_wheat_price — this should trigger StopBadPrice.
    ///
    /// Before the fix: frozen check ran first, deleting the offer and continuing.
    /// After the fix: price check runs first, returning CrossSelf or BadPrice.
    #[test]
    fn test_audit_011_frozen_offer_filter_order_vs_bad_price() {
        use crate::frozen_keys::{trustline_key, FrozenKeyConfig};

        let mut state = LedgerStateManager::new(5_000_000, 100);

        let buyer_id = create_test_account_id(0);
        let seller_id = create_test_account_id(1);
        let issuer_id = create_test_account_id(2);
        state.create_account(create_test_account(buyer_id.clone(), 100_000_000));
        state.create_account(create_test_account(seller_id.clone(), 100_000_000));
        state.create_account(create_test_account(issuer_id.clone(), 100_000_000));

        let usd = Asset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4([b'U', b'S', b'D', b'X']),
            issuer: issuer_id.clone(),
        });
        let idr = Asset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4([b'I', b'D', b'R', b'X']),
            issuer: issuer_id.clone(),
        });

        // Buyer trustlines
        state.create_trustline(create_test_trustline(
            buyer_id.clone(),
            TrustLineAsset::CreditAlphanum4(AlphaNum4 {
                asset_code: AssetCode4([b'U', b'S', b'D', b'X']),
                issuer: issuer_id.clone(),
            }),
            1_000_000,
            10_000_000,
            AUTHORIZED_FLAG,
        ));
        state.create_trustline(create_test_trustline(
            buyer_id.clone(),
            TrustLineAsset::CreditAlphanum4(AlphaNum4 {
                asset_code: AssetCode4([b'I', b'D', b'R', b'X']),
                issuer: issuer_id.clone(),
            }),
            0,
            10_000_000,
            AUTHORIZED_FLAG,
        ));

        // Seller trustlines
        state.create_trustline(create_test_trustline(
            seller_id.clone(),
            TrustLineAsset::CreditAlphanum4(AlphaNum4 {
                asset_code: AssetCode4([b'U', b'S', b'D', b'X']),
                issuer: issuer_id.clone(),
            }),
            0,
            10_000_000,
            AUTHORIZED_FLAG,
        ));
        state.create_trustline(create_test_trustline(
            seller_id.clone(),
            TrustLineAsset::CreditAlphanum4(AlphaNum4 {
                asset_code: AssetCode4([b'I', b'D', b'R', b'X']),
                issuer: issuer_id.clone(),
            }),
            1_000_000,
            10_000_000,
            AUTHORIZED_FLAG,
        ));

        // Seller's existing offer: selling IDR for USD at price 2/1
        // (2 IDR per 1 USD — expensive from buyer's perspective)
        state.create_offer(OfferEntry {
            seller_id: seller_id.clone(),
            offer_id: 1,
            selling: idr.clone(),
            buying: usd.clone(),
            amount: 1000,
            price: Price { n: 2, d: 1 },
            flags: 0,
            ext: OfferEntryExt::V0,
        });

        // Freeze the seller's IDR trustline via FrozenKeyConfig
        let frozen_key = trustline_key(&seller_id, &idr);
        let frozen_bytes = frozen_key.to_xdr(Limits::none()).unwrap();
        let frozen_config = FrozenKeyConfig::new(vec![frozen_bytes], vec![]);

        // Create context with frozen keys
        let mut context = create_test_context();
        context.frozen_key_config = frozen_config;

        // Buyer places offer: selling USD, buying IDR at price 1/1
        // The seller's offer at price 2/1 > buyer's max price 1/1 → StopBadPrice
        // The seller's offer is ALSO frozen → would be deleted if checked first
        //
        // stellar-core: price check first → StopBadPrice → offer survives
        // Bug (old code): frozen check first → offer deleted → no match → offer placed
        let op = ManageSellOfferOp {
            selling: usd.clone(),
            buying: idr.clone(),
            amount: 100,
            price: Price { n: 1, d: 1 },
            offer_id: 0,
        };

        let result = execute_manage_sell_offer(&op, &buyer_id, &mut state, &context).unwrap();

        // Verify the buyer's offer was created (no crossing happened due to bad price)
        match result {
            OperationResult::OpInner(OperationResultTr::ManageSellOffer(
                ManageSellOfferResult::Success(detail),
            )) => {
                // The offer should have been created since no crossing happened
                assert!(
                    detail.offers_claimed.is_empty(),
                    "No offers should have been crossed"
                );
            }
            other => panic!("Expected Success, got: {:?}", other),
        }

        // The seller's frozen offer must STILL exist (price filter stopped before frozen check)
        let offer = state.get_offer(&seller_id, 1);
        assert!(
            offer.is_some(),
            "AUDIT-011: Frozen offer must survive when price filter would stop first"
        );
    }

    /// SellNoIssuer is unreachable since protocol 13+ (CAP-0017 removed issuer checks).
    #[test]
    #[ignore = "SellNoIssuer is unreachable since protocol 13+ (CAP-0017)"]
    fn test_manage_sell_offer_sell_no_issuer() {}

    /// BuyNoIssuer is unreachable since protocol 13+ (CAP-0017 removed issuer checks).
    #[test]
    #[ignore = "BuyNoIssuer is unreachable since protocol 13+ (CAP-0017)"]
    fn test_manage_sell_offer_buy_no_issuer() {}

    /// Regression test for #1115: Invalid asset code in selling asset should be
    /// rejected as Malformed.
    #[test]
    fn test_manage_sell_offer_malformed_invalid_asset_code() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(0);
        state.create_account(create_test_account(source_id.clone(), 100_000_000));

        // Invalid asset: embedded NUL between non-NUL chars
        let bad_asset = Asset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4([b'U', 0, b'D', b'C']),
            issuer: source_id.clone(),
        });

        let op = ManageSellOfferOp {
            selling: bad_asset,
            buying: Asset::Native,
            amount: 100,
            price: Price { n: 1, d: 1 },
            offer_id: 0,
        };
        let result = execute_manage_sell_offer(&op, &source_id, &mut state, &context);
        match result.unwrap() {
            OperationResult::OpInner(OperationResultTr::ManageSellOffer(r)) => {
                assert!(
                    matches!(r, ManageSellOfferResult::Malformed),
                    "Expected Malformed for invalid asset code, got {:?}",
                    r
                );
            }
            other => panic!("unexpected: {:?}", other),
        }
    }

    // ── ManageBuyOffer result code parity tests ──
    // ManageBuyOffer delegates to execute_manage_offer and converts results
    // via convert_sell_to_buy_result. These tests verify the conversion layer.

    #[test]
    fn test_manage_buy_offer_malformed_same_asset() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(0);
        state.create_account(create_test_account(source_id.clone(), 100_000_000));

        let op = ManageBuyOfferOp {
            selling: Asset::Native,
            buying: Asset::Native, // Same as selling
            buy_amount: 10_000_000,
            price: Price { n: 1, d: 1 },
            offer_id: 0,
        };

        let result = execute_manage_buy_offer(&op, &source_id, &mut state, &context).unwrap();
        match result {
            OperationResult::OpInner(OperationResultTr::ManageBuyOffer(r)) => {
                assert!(
                    matches!(r, ManageBuyOfferResult::Malformed),
                    "Expected Malformed, got {:?}",
                    r
                );
            }
            other => panic!("unexpected: {:?}", other),
        }
    }

    #[test]
    fn test_manage_buy_offer_sell_no_trust() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(0);
        let issuer_id = create_test_account_id(1);
        state.create_account(create_test_account(source_id.clone(), 100_000_000));
        state.create_account(create_test_account(issuer_id.clone(), 100_000_000));

        let usd = create_asset(&issuer_id);

        // Source has no trustline for USD (selling side)
        let op = ManageBuyOfferOp {
            selling: usd,
            buying: Asset::Native,
            buy_amount: 1000,
            price: Price { n: 1, d: 1 },
            offer_id: 0,
        };

        let result = execute_manage_buy_offer(&op, &source_id, &mut state, &context).unwrap();
        match result {
            OperationResult::OpInner(OperationResultTr::ManageBuyOffer(r)) => {
                assert!(
                    matches!(r, ManageBuyOfferResult::SellNoTrust),
                    "Expected SellNoTrust, got {:?}",
                    r
                );
            }
            other => panic!("unexpected: {:?}", other),
        }
    }

    #[test]
    fn test_manage_buy_offer_not_found() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(0);
        let issuer_id = create_test_account_id(1);
        state.create_account(create_test_account(source_id.clone(), 100_000_000));
        state.create_account(create_test_account(issuer_id.clone(), 100_000_000));

        let usd = create_asset(&issuer_id);

        // Create trustlines for both sides
        state.create_trustline(create_test_trustline(
            source_id.clone(),
            TrustLineAsset::CreditAlphanum4(AlphaNum4 {
                asset_code: AssetCode4([b'U', b'S', b'D', b'C']),
                issuer: issuer_id.clone(),
            }),
            1_000_000,
            10_000_000,
            AUTHORIZED_FLAG,
        ));
        state.get_account_mut(&source_id).unwrap().num_sub_entries += 1;

        // Try to update a non-existent offer
        let op = ManageBuyOfferOp {
            selling: usd,
            buying: Asset::Native,
            buy_amount: 1000,
            price: Price { n: 1, d: 1 },
            offer_id: 999, // Non-existent
        };

        let result = execute_manage_buy_offer(&op, &source_id, &mut state, &context).unwrap();
        match result {
            OperationResult::OpInner(OperationResultTr::ManageBuyOffer(r)) => {
                assert!(
                    matches!(r, ManageBuyOfferResult::NotFound),
                    "Expected NotFound, got {:?}",
                    r
                );
            }
            other => panic!("unexpected: {:?}", other),
        }
    }

    #[test]
    fn test_manage_buy_offer_underfunded() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(0);
        let issuer_id = create_test_account_id(1);
        state.create_account(create_test_account(source_id.clone(), 100_000_000));
        state.create_account(create_test_account(issuer_id.clone(), 100_000_000));

        let usd = create_asset(&issuer_id);

        // Source has trustline but zero balance
        state.create_trustline(create_test_trustline(
            source_id.clone(),
            TrustLineAsset::CreditAlphanum4(AlphaNum4 {
                asset_code: AssetCode4([b'U', b'S', b'D', b'C']),
                issuer: issuer_id.clone(),
            }),
            0, // zero balance
            10_000_000,
            AUTHORIZED_FLAG,
        ));
        state.get_account_mut(&source_id).unwrap().num_sub_entries += 1;

        let op = ManageBuyOfferOp {
            selling: usd,
            buying: Asset::Native,
            buy_amount: 1000,
            price: Price { n: 1, d: 1 },
            offer_id: 0,
        };

        let result = execute_manage_buy_offer(&op, &source_id, &mut state, &context).unwrap();
        match result {
            OperationResult::OpInner(OperationResultTr::ManageBuyOffer(r)) => {
                assert!(
                    matches!(r, ManageBuyOfferResult::Underfunded),
                    "Expected Underfunded, got {:?}",
                    r
                );
            }
            other => panic!("unexpected: {:?}", other),
        }
    }

    // ── CreatePassiveSellOffer result code parity tests ──
    // CreatePassiveSellOffer also delegates to execute_manage_offer with
    // passive=true and amount >= 0 (no update/delete, always offer_id=0).

    #[test]
    fn test_create_passive_sell_offer_malformed_same_asset() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(0);
        state.create_account(create_test_account(source_id.clone(), 100_000_000));

        let op = CreatePassiveSellOfferOp {
            selling: Asset::Native,
            buying: Asset::Native,
            amount: 1000,
            price: Price { n: 1, d: 1 },
        };

        let result =
            execute_create_passive_sell_offer(&op, &source_id, &mut state, &context).unwrap();
        match result {
            OperationResult::OpInner(OperationResultTr::ManageSellOffer(r)) => {
                assert!(
                    matches!(r, ManageSellOfferResult::Malformed),
                    "Expected Malformed, got {:?}",
                    r
                );
            }
            other => panic!("unexpected: {:?}", other),
        }
    }

    #[test]
    fn test_create_passive_sell_offer_sell_no_trust() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(0);
        let issuer_id = create_test_account_id(1);
        state.create_account(create_test_account(source_id.clone(), 100_000_000));
        state.create_account(create_test_account(issuer_id.clone(), 100_000_000));

        let usd = create_asset(&issuer_id);

        // No trustline for USD
        let op = CreatePassiveSellOfferOp {
            selling: usd,
            buying: Asset::Native,
            amount: 1000,
            price: Price { n: 1, d: 1 },
        };

        let result =
            execute_create_passive_sell_offer(&op, &source_id, &mut state, &context).unwrap();
        match result {
            OperationResult::OpInner(OperationResultTr::ManageSellOffer(r)) => {
                assert!(
                    matches!(r, ManageSellOfferResult::SellNoTrust),
                    "Expected SellNoTrust, got {:?}",
                    r
                );
            }
            other => panic!("unexpected: {:?}", other),
        }
    }

    #[test]
    fn test_create_passive_sell_offer_underfunded() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(0);
        let issuer_id = create_test_account_id(1);
        state.create_account(create_test_account(source_id.clone(), 100_000_000));
        state.create_account(create_test_account(issuer_id.clone(), 100_000_000));

        let usd = create_asset(&issuer_id);

        // Zero-balance trustline
        state.create_trustline(create_test_trustline(
            source_id.clone(),
            TrustLineAsset::CreditAlphanum4(AlphaNum4 {
                asset_code: AssetCode4([b'U', b'S', b'D', b'C']),
                issuer: issuer_id.clone(),
            }),
            0,
            10_000_000,
            AUTHORIZED_FLAG,
        ));
        state.get_account_mut(&source_id).unwrap().num_sub_entries += 1;

        let op = CreatePassiveSellOfferOp {
            selling: usd,
            buying: Asset::Native,
            amount: 1000,
            price: Price { n: 1, d: 1 },
        };

        let result =
            execute_create_passive_sell_offer(&op, &source_id, &mut state, &context).unwrap();
        match result {
            OperationResult::OpInner(OperationResultTr::ManageSellOffer(r)) => {
                assert!(
                    matches!(r, ManageSellOfferResult::Underfunded),
                    "Expected Underfunded, got {:?}",
                    r
                );
            }
            other => panic!("unexpected: {:?}", other),
        }
    }

    // ── Remaining ManageBuyOffer result code parity tests ──

    #[test]
    fn test_manage_buy_offer_buy_no_trust() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(0);
        let issuer_id = create_test_account_id(1);
        state.create_account(create_test_account(source_id.clone(), 100_000_000));
        state.create_account(create_test_account(issuer_id.clone(), 100_000_000));

        let usd = create_asset(&issuer_id);

        // No trustline for buying asset (USD)
        let op = ManageBuyOfferOp {
            selling: Asset::Native,
            buying: usd,
            buy_amount: 1000,
            price: Price { n: 1, d: 1 },
            offer_id: 0,
        };

        let result = execute_manage_buy_offer(&op, &source_id, &mut state, &context).unwrap();
        match result {
            OperationResult::OpInner(OperationResultTr::ManageBuyOffer(r)) => {
                assert!(
                    matches!(r, ManageBuyOfferResult::BuyNoTrust),
                    "Expected BuyNoTrust, got {:?}",
                    r
                );
            }
            other => panic!("unexpected: {:?}", other),
        }
    }

    #[test]
    fn test_manage_buy_offer_sell_not_authorized() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(0);
        let issuer_id = create_test_account_id(1);
        state.create_account(create_test_account(source_id.clone(), 100_000_000));
        state.create_account(create_test_account(issuer_id.clone(), 100_000_000));
        state.get_account_mut(&issuer_id).unwrap().flags = AccountFlags::RequiredFlag as u32;

        let usd = create_asset(&issuer_id);
        // Trustline with flags=0 (not authorized)
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

        let op = ManageBuyOfferOp {
            selling: usd,
            buying: Asset::Native,
            buy_amount: 10,
            price: Price { n: 1, d: 1 },
            offer_id: 0,
        };

        let result = execute_manage_buy_offer(&op, &source_id, &mut state, &context).unwrap();
        match result {
            OperationResult::OpInner(OperationResultTr::ManageBuyOffer(r)) => {
                assert!(
                    matches!(r, ManageBuyOfferResult::SellNotAuthorized),
                    "Expected SellNotAuthorized, got {:?}",
                    r
                );
            }
            other => panic!("unexpected: {:?}", other),
        }
    }

    #[test]
    fn test_manage_buy_offer_buy_not_authorized() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(0);
        let issuer_id = create_test_account_id(1);
        state.create_account(create_test_account(source_id.clone(), 100_000_000));
        state.create_account(create_test_account(issuer_id.clone(), 100_000_000));
        state.get_account_mut(&issuer_id).unwrap().flags = AccountFlags::RequiredFlag as u32;

        let usd = create_asset(&issuer_id);
        // Trustline for buying side with flags=0 (not authorized)
        state.create_trustline(create_test_trustline(
            source_id.clone(),
            TrustLineAsset::CreditAlphanum4(AlphaNum4 {
                asset_code: AssetCode4([b'U', b'S', b'D', b'C']),
                issuer: issuer_id.clone(),
            }),
            0,
            1_000_000,
            0,
        ));

        let op = ManageBuyOfferOp {
            selling: Asset::Native,
            buying: usd,
            buy_amount: 10,
            price: Price { n: 1, d: 1 },
            offer_id: 0,
        };

        let result = execute_manage_buy_offer(&op, &source_id, &mut state, &context).unwrap();
        match result {
            OperationResult::OpInner(OperationResultTr::ManageBuyOffer(r)) => {
                assert!(
                    matches!(r, ManageBuyOfferResult::BuyNotAuthorized),
                    "Expected BuyNotAuthorized, got {:?}",
                    r
                );
            }
            other => panic!("unexpected: {:?}", other),
        }
    }

    #[test]
    fn test_manage_buy_offer_line_full() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(0);
        let issuer_id = create_test_account_id(1);
        state.create_account(create_test_account(source_id.clone(), 100_000_000));
        state.create_account(create_test_account(issuer_id.clone(), 100_000_000));

        let buying_asset = create_asset(&issuer_id);
        // Buying trustline is full (balance == limit)
        state.create_trustline(create_test_trustline(
            source_id.clone(),
            TrustLineAsset::CreditAlphanum4(AlphaNum4 {
                asset_code: AssetCode4([b'U', b'S', b'D', b'C']),
                issuer: issuer_id.clone(),
            }),
            1_000,
            1_000,
            AUTHORIZED_FLAG,
        ));

        let op = ManageBuyOfferOp {
            selling: Asset::Native,
            buying: buying_asset,
            buy_amount: 10,
            price: Price { n: 1, d: 1 },
            offer_id: 0,
        };

        let result = execute_manage_buy_offer(&op, &source_id, &mut state, &context).unwrap();
        match result {
            OperationResult::OpInner(OperationResultTr::ManageBuyOffer(r)) => {
                assert!(
                    matches!(r, ManageBuyOfferResult::LineFull),
                    "Expected LineFull, got {:?}",
                    r
                );
            }
            other => panic!("unexpected: {:?}", other),
        }
    }

    #[test]
    fn test_manage_buy_offer_low_reserve() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(0);
        let issuer_id = create_test_account_id(1);
        let min_balance = state
            .minimum_balance_with_counts(context.protocol_version, 0, 0, 0)
            .unwrap();
        state.create_account(create_test_account(source_id.clone(), min_balance));
        state.create_account(create_test_account(issuer_id.clone(), 100_000_000));

        let usd = create_asset(&issuer_id);
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

        let op = ManageBuyOfferOp {
            selling: usd,
            buying: Asset::Native,
            buy_amount: 10,
            price: Price { n: 1, d: 1 },
            offer_id: 0,
        };

        let result = execute_manage_buy_offer(&op, &source_id, &mut state, &context).unwrap();
        match result {
            OperationResult::OpInner(OperationResultTr::ManageBuyOffer(r)) => {
                assert!(
                    matches!(r, ManageBuyOfferResult::LowReserve),
                    "Expected LowReserve, got {:?}",
                    r
                );
            }
            other => panic!("unexpected: {:?}", other),
        }
    }

    // ── Remaining CreatePassiveSellOffer result code parity tests ──

    #[test]
    fn test_create_passive_sell_offer_buy_no_trust() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(0);
        let issuer_id = create_test_account_id(1);
        state.create_account(create_test_account(source_id.clone(), 100_000_000));
        state.create_account(create_test_account(issuer_id.clone(), 100_000_000));

        let usd = create_asset(&issuer_id);

        // No trustline for the buying asset (USD)
        let op = CreatePassiveSellOfferOp {
            selling: Asset::Native,
            buying: usd,
            amount: 1000,
            price: Price { n: 1, d: 1 },
        };

        let result =
            execute_create_passive_sell_offer(&op, &source_id, &mut state, &context).unwrap();
        match result {
            OperationResult::OpInner(OperationResultTr::ManageSellOffer(r)) => {
                assert!(
                    matches!(r, ManageSellOfferResult::BuyNoTrust),
                    "Expected BuyNoTrust, got {:?}",
                    r
                );
            }
            other => panic!("unexpected: {:?}", other),
        }
    }

    #[test]
    fn test_create_passive_sell_offer_sell_not_authorized() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(0);
        let issuer_id = create_test_account_id(1);
        state.create_account(create_test_account(source_id.clone(), 100_000_000));
        state.create_account(create_test_account(issuer_id.clone(), 100_000_000));
        state.get_account_mut(&issuer_id).unwrap().flags = AccountFlags::RequiredFlag as u32;

        let usd = create_asset(&issuer_id);
        // Trustline with flags=0 (not authorized)
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

        let op = CreatePassiveSellOfferOp {
            selling: usd,
            buying: Asset::Native,
            amount: 10,
            price: Price { n: 1, d: 1 },
        };

        let result =
            execute_create_passive_sell_offer(&op, &source_id, &mut state, &context).unwrap();
        match result {
            OperationResult::OpInner(OperationResultTr::ManageSellOffer(r)) => {
                assert!(
                    matches!(r, ManageSellOfferResult::SellNotAuthorized),
                    "Expected SellNotAuthorized, got {:?}",
                    r
                );
            }
            other => panic!("unexpected: {:?}", other),
        }
    }

    #[test]
    fn test_create_passive_sell_offer_buy_not_authorized() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(0);
        let issuer_id = create_test_account_id(1);
        state.create_account(create_test_account(source_id.clone(), 100_000_000));
        state.create_account(create_test_account(issuer_id.clone(), 100_000_000));
        state.get_account_mut(&issuer_id).unwrap().flags = AccountFlags::RequiredFlag as u32;

        let usd = create_asset(&issuer_id);
        // Trustline for buying side with flags=0 (not authorized)
        state.create_trustline(create_test_trustline(
            source_id.clone(),
            TrustLineAsset::CreditAlphanum4(AlphaNum4 {
                asset_code: AssetCode4([b'U', b'S', b'D', b'C']),
                issuer: issuer_id.clone(),
            }),
            0,
            1_000_000,
            0,
        ));

        let op = CreatePassiveSellOfferOp {
            selling: Asset::Native,
            buying: usd,
            amount: 10,
            price: Price { n: 1, d: 1 },
        };

        let result =
            execute_create_passive_sell_offer(&op, &source_id, &mut state, &context).unwrap();
        match result {
            OperationResult::OpInner(OperationResultTr::ManageSellOffer(r)) => {
                assert!(
                    matches!(r, ManageSellOfferResult::BuyNotAuthorized),
                    "Expected BuyNotAuthorized, got {:?}",
                    r
                );
            }
            other => panic!("unexpected: {:?}", other),
        }
    }

    #[test]
    fn test_create_passive_sell_offer_line_full() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(0);
        let issuer_id = create_test_account_id(1);
        state.create_account(create_test_account(source_id.clone(), 100_000_000));
        state.create_account(create_test_account(issuer_id.clone(), 100_000_000));

        let buying_asset = create_asset(&issuer_id);
        // Buying trustline is full (balance == limit)
        state.create_trustline(create_test_trustline(
            source_id.clone(),
            TrustLineAsset::CreditAlphanum4(AlphaNum4 {
                asset_code: AssetCode4([b'U', b'S', b'D', b'C']),
                issuer: issuer_id.clone(),
            }),
            1_000,
            1_000,
            AUTHORIZED_FLAG,
        ));

        let op = CreatePassiveSellOfferOp {
            selling: Asset::Native,
            buying: buying_asset,
            amount: 10,
            price: Price { n: 1, d: 1 },
        };

        let result =
            execute_create_passive_sell_offer(&op, &source_id, &mut state, &context).unwrap();
        match result {
            OperationResult::OpInner(OperationResultTr::ManageSellOffer(r)) => {
                assert!(
                    matches!(r, ManageSellOfferResult::LineFull),
                    "Expected LineFull, got {:?}",
                    r
                );
            }
            other => panic!("unexpected: {:?}", other),
        }
    }

    /// Test CreatePassiveSellOffer CrossSelf.
    ///
    /// The passive flag only changes the price threshold (strict less-than instead
    /// of less-than-or-equal for crossing). If the source's own opposite offer
    /// has a strictly better price, CrossSelf is returned.
    #[test]
    fn test_create_passive_sell_offer_cross_self() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(0);
        let issuer_id = create_test_account_id(1);
        state.create_account(create_test_account(source_id.clone(), 100_000_000));
        state.create_account(create_test_account(issuer_id.clone(), 100_000_000));

        let usd = Asset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4([b'U', b'S', b'D', b'X']),
            issuer: issuer_id.clone(),
        });
        let idr = Asset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4([b'I', b'D', b'R', b'X']),
            issuer: issuer_id.clone(),
        });

        state.create_trustline(create_test_trustline(
            source_id.clone(),
            TrustLineAsset::CreditAlphanum4(AlphaNum4 {
                asset_code: AssetCode4([b'U', b'S', b'D', b'X']),
                issuer: issuer_id.clone(),
            }),
            1_000,
            1_000_000,
            AUTHORIZED_FLAG,
        ));
        state.create_trustline(create_test_trustline(
            source_id.clone(),
            TrustLineAsset::CreditAlphanum4(AlphaNum4 {
                asset_code: AssetCode4([b'I', b'D', b'R', b'X']),
                issuer: issuer_id.clone(),
            }),
            1_000,
            1_000_000,
            AUTHORIZED_FLAG,
        ));

        // Existing offer: selling IDR, buying USD at price 1/2
        // (willing to sell 2 IDR per 1 USD)
        state.create_offer(OfferEntry {
            seller_id: source_id.clone(),
            offer_id: 1,
            selling: idr.clone(),
            buying: usd.clone(),
            amount: 100,
            price: Price { n: 1, d: 2 },
            flags: 0,
            ext: OfferEntryExt::V0,
        });

        // Passive offer: selling USD, buying IDR at price 1/1.
        // The existing offer's effective ask for USD→IDR is 2/1 = 2 IDR per USD.
        // This passive offer offers 1/1, meaning the existing offer has a strictly
        // better price, so CrossSelf triggers.
        let op = CreatePassiveSellOfferOp {
            selling: usd,
            buying: idr,
            amount: 10,
            price: Price { n: 1, d: 1 },
        };

        let result =
            execute_create_passive_sell_offer(&op, &source_id, &mut state, &context).unwrap();
        match result {
            OperationResult::OpInner(OperationResultTr::ManageSellOffer(r)) => {
                assert!(
                    matches!(r, ManageSellOfferResult::CrossSelf),
                    "Expected CrossSelf, got {:?}",
                    r
                );
            }
            other => panic!("unexpected: {:?}", other),
        }
    }

    #[test]
    fn test_create_passive_sell_offer_low_reserve() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source_id = create_test_account_id(0);
        let issuer_id = create_test_account_id(1);
        let min_balance = state
            .minimum_balance_with_counts(context.protocol_version, 0, 0, 0)
            .unwrap();
        state.create_account(create_test_account(source_id.clone(), min_balance));
        state.create_account(create_test_account(issuer_id.clone(), 100_000_000));

        let usd = create_asset(&issuer_id);
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

        let op = CreatePassiveSellOfferOp {
            selling: usd,
            buying: Asset::Native,
            amount: 10,
            price: Price { n: 1, d: 1 },
        };

        let result =
            execute_create_passive_sell_offer(&op, &source_id, &mut state, &context).unwrap();
        match result {
            OperationResult::OpInner(OperationResultTr::ManageSellOffer(r)) => {
                assert!(
                    matches!(r, ManageSellOfferResult::LowReserve),
                    "Expected LowReserve, got {:?}",
                    r
                );
            }
            other => panic!("unexpected: {:?}", other),
        }
    }

    /// Dead code: SellNoIssuer is unreachable in protocol 13+ (CAP-0017).
    #[test]
    #[ignore = "Dead code: CAP-0017 (protocol 13+) removed issuer existence checks"]
    fn test_manage_buy_offer_sell_no_issuer() {}

    /// Dead code: BuyNoIssuer is unreachable in protocol 13+ (CAP-0017).
    #[test]
    #[ignore = "Dead code: CAP-0017 (protocol 13+) removed issuer existence checks"]
    fn test_manage_buy_offer_buy_no_issuer() {}

    /// Regression test for AUDIT-014: convert_with_offers must check the crossing
    /// limit BEFORE performing the crossing (matching stellar-core), not after.
    /// This ensures exactly `max_offers_to_cross` crossings are allowed.
    #[test]
    fn test_audit_014_crossing_limit_boundary() {
        use super::super::offer_exchange::{ConversionParams, RoundingType};
        use crate::frozen_keys::FrozenKeyConfig;

        fn setup_state_with_offers(
            n_offers: u8,
        ) -> (LedgerStateManager, LedgerContext, AccountId, Asset) {
            let mut state = LedgerStateManager::new(5_000_000, 100);
            let context = create_test_context();
            let buyer_id = create_test_account_id(0);
            let issuer_id = create_test_account_id(99);
            state.create_account(create_test_account(buyer_id.clone(), 1_000_000_000));
            state.create_account(create_test_account(issuer_id.clone(), 1_000_000_000));

            let usd = Asset::CreditAlphanum4(AlphaNum4 {
                asset_code: AssetCode4([b'U', b'S', b'D', b'C']),
                issuer: issuer_id.clone(),
            });
            let usd_tla = TrustLineAsset::CreditAlphanum4(AlphaNum4 {
                asset_code: AssetCode4([b'U', b'S', b'D', b'C']),
                issuer: issuer_id.clone(),
            });

            // Buyer needs USD trustline to receive
            state.create_trustline(create_test_trustline(
                buyer_id.clone(),
                usd_tla.clone(),
                0,
                i64::MAX,
                AUTHORIZED_FLAG,
            ));

            // Create n sellers, each selling 100 USD for XLM at 1:1
            for i in 1..=n_offers {
                let seller_id = create_test_account_id(i);
                // Account with buying liabilities for native (100 buying)
                let mut acc = create_test_account(seller_id.clone(), 1_000_000_000);
                acc.num_sub_entries = 2; // 1 trustline + 1 offer
                acc.ext = AccountEntryExt::V1(AccountEntryExtensionV1 {
                    liabilities: Liabilities {
                        buying: 100,
                        selling: 0,
                    },
                    ext: AccountEntryExtensionV1Ext::V0,
                });
                state.create_account(acc);

                // Trustline with selling liabilities (100 selling)
                state.create_trustline(TrustLineEntry {
                    account_id: seller_id.clone(),
                    asset: usd_tla.clone(),
                    balance: 1_000,
                    limit: i64::MAX,
                    flags: AUTHORIZED_FLAG,
                    ext: TrustLineEntryExt::V1(TrustLineEntryV1 {
                        liabilities: Liabilities {
                            buying: 0,
                            selling: 100,
                        },
                        ext: TrustLineEntryV1Ext::V0,
                    }),
                });

                state.create_offer(OfferEntry {
                    seller_id,
                    offer_id: i as i64,
                    selling: usd.clone(),
                    buying: Asset::Native,
                    amount: 100,
                    price: Price { n: 1, d: 1 },
                    flags: 0,
                    ext: OfferEntryExt::V0,
                });
            }
            (state, context, buyer_id, usd)
        }

        let frozen = FrozenKeyConfig::default();

        // Test 1: With max_offers_to_cross=3, all 3 crossings should succeed
        {
            let (mut state, context, buyer_id, usd) = setup_state_with_offers(3);
            let mut trail = Vec::new();
            let mut sheep_sent = 0i64;
            let mut wheat_received = 0i64;
            let result = convert_with_offers(
                &mut ConversionParams {
                    source: &buyer_id,
                    selling: &Asset::Native,
                    buying: &usd,
                    max_send: 300,
                    max_receive: 300,
                    round: RoundingType::Normal,
                    offer_trail: &mut trail,
                    state: &mut state,
                    context: &context,
                    frozen_key_config: &frozen,
                },
                &mut sheep_sent,
                &mut wheat_received,
                0,
                false,
                &Price { n: i32::MAX, d: 1 },
                3, // exactly 3 allowed
            )
            .unwrap();
            assert_eq!(
                result,
                ConvertResult::Ok,
                "3 crossings with limit=3 should succeed"
            );
            assert_eq!(trail.len(), 3);
            assert_eq!(wheat_received, 300);
        }

        // Test 2: With max_offers_to_cross=2 and 3 offers, the 3rd should be rejected
        {
            let (mut state, context, buyer_id, usd) = setup_state_with_offers(3);
            let mut trail = Vec::new();
            let mut sheep_sent = 0i64;
            let mut wheat_received = 0i64;
            let result = convert_with_offers(
                &mut ConversionParams {
                    source: &buyer_id,
                    selling: &Asset::Native,
                    buying: &usd,
                    max_send: 300,
                    max_receive: 300,
                    round: RoundingType::Normal,
                    offer_trail: &mut trail,
                    state: &mut state,
                    context: &context,
                    frozen_key_config: &frozen,
                },
                &mut sheep_sent,
                &mut wheat_received,
                0,
                false,
                &Price { n: i32::MAX, d: 1 },
                2, // only 2 allowed
            )
            .unwrap();
            assert_eq!(
                result,
                ConvertResult::CrossedTooMany,
                "3rd crossing with limit=2 should fail"
            );
            // The first 2 crossings should have succeeded
            assert_eq!(trail.len(), 2);
            assert_eq!(wheat_received, 200);
        }
    }

    // ============================================================================
    // Tests for #2287 / AUDIT-249: TOO_MANY_SPONSORING check for fully-consumed
    // sponsored offers.
    //
    // Pattern: sponsor pre-arranged to sponsor source, source submits a new
    // sponsored offer that fully crosses an existing counter-offer, expect
    // OpTooManySponsoring on overflow. All five tests invoke `execute_operation`
    // (the dispatcher) so that `TxError::TooManySponsoring` is converted to
    // `Ok(OperationResult::OpTooManySponsoring)` (precedent:
    // test_audit_058_sponsored_manage_data_at_max_sponsoring at mod.rs:1862).
    // ============================================================================

    /// Setup helper: fully-consumed sponsored offer scenario.
    ///
    /// Returns `(state, context, source_id, sponsor_id, asset_a, asset_b)` where
    /// source has trustlines for both assets and a counter-offer is on the book
    /// matching exactly the offer source will submit. Sponsor's `num_sponsoring`
    /// and `num_sub_entries` are set per parameters.
    fn setup_fully_consumed_sponsored_offer(
        num_sponsoring: u32,
        sponsor_num_sub_entries: u32,
        base_reserve: i64,
    ) -> (
        LedgerStateManager,
        LedgerContext,
        AccountId,
        AccountId,
        Asset,
        Asset,
    ) {
        let mut state = LedgerStateManager::new(base_reserve, 100);
        let context = create_test_context();

        let issuer_id = create_test_account_id(99);
        state.create_account(create_test_account(issuer_id.clone(), i64::MAX));

        let asset_a = Asset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4(*b"USDC"),
            issuer: issuer_id.clone(),
        });
        let asset_b = Asset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4(*b"EURX"),
            issuer: issuer_id.clone(),
        });
        let tl_asset_a = TrustLineAsset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4(*b"USDC"),
            issuer: issuer_id.clone(),
        });
        let tl_asset_b = TrustLineAsset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4(*b"EURX"),
            issuer: issuer_id.clone(),
        });

        // Counterparty with existing offer on the book (selling asset_a for asset_b)
        let counter_id = create_test_account_id(1);
        state.create_account(create_test_account(counter_id.clone(), i64::MAX));
        state.create_trustline(TrustLineEntry {
            account_id: counter_id.clone(),
            asset: tl_asset_a.clone(),
            balance: 500_000_000,
            limit: i64::MAX,
            flags: AUTHORIZED_FLAG,
            ext: TrustLineEntryExt::V1(TrustLineEntryV1 {
                liabilities: Liabilities {
                    buying: 0,
                    selling: 100_000_000,
                },
                ext: TrustLineEntryV1Ext::V0,
            }),
        });
        state.create_trustline(TrustLineEntry {
            account_id: counter_id.clone(),
            asset: tl_asset_b.clone(),
            balance: 0,
            limit: i64::MAX,
            flags: AUTHORIZED_FLAG,
            ext: TrustLineEntryExt::V1(TrustLineEntryV1 {
                liabilities: Liabilities {
                    buying: 100_000_000,
                    selling: 0,
                },
                ext: TrustLineEntryV1Ext::V0,
            }),
        });
        let counter_offer = OfferEntry {
            seller_id: counter_id.clone(),
            offer_id: 1000,
            selling: asset_a.clone(),
            buying: asset_b.clone(),
            amount: 100_000_000,
            price: Price { n: 1, d: 1 },
            flags: 0,
            ext: OfferEntryExt::V0,
        };
        state.load_entry(LedgerEntry {
            last_modified_ledger_seq: 840000,
            data: LedgerEntryData::Offer(counter_offer),
            ext: LedgerEntryExt::V0,
        });
        if let Some(acct) = state.get_account_mut(&counter_id) {
            acct.num_sub_entries = 3;
        }

        // Source: submits a new offer that gets fully consumed
        let source_id = create_test_account_id(2);
        state.create_account(create_test_account(source_id.clone(), i64::MAX));
        state.create_trustline(create_test_trustline(
            source_id.clone(),
            tl_asset_a.clone(),
            0,
            i64::MAX,
            AUTHORIZED_FLAG,
        ));
        state.create_trustline(create_test_trustline(
            source_id.clone(),
            tl_asset_b.clone(),
            500_000_000,
            i64::MAX,
            AUTHORIZED_FLAG,
        ));
        if let Some(acct) = state.get_account_mut(&source_id) {
            acct.num_sub_entries = 2;
        }

        // Sponsor: balances large, num_sponsoring/num_sub_entries set per args
        let sponsor_id = create_test_account_id(3);
        let mut sponsor_acct = create_test_account(sponsor_id.clone(), i64::MAX);
        sponsor_acct.num_sub_entries = sponsor_num_sub_entries;
        sponsor_acct.ext = AccountEntryExt::V1(AccountEntryExtensionV1 {
            liabilities: Liabilities {
                buying: 0,
                selling: 0,
            },
            ext: AccountEntryExtensionV1Ext::V2(AccountEntryExtensionV2 {
                num_sponsoring,
                num_sponsored: 0,
                signer_sponsoring_i_ds: vec![].try_into().unwrap(),
                ext: AccountEntryExtensionV2Ext::V0,
            }),
        });
        state.create_account(sponsor_acct);

        // Set up sponsorship: sponsor sponsors source's future reserves
        state.push_sponsorship(sponsor_id.clone(), source_id.clone());

        // Clear scaffolding from modified_accounts so test ordering assertions
        // (and tx-meta delta assertions in general) reflect only the op itself.
        state.flush_modified_entries();

        (state, context, source_id, sponsor_id, asset_a, asset_b)
    }

    /// Build a `ManageSellOfferOp` that fully crosses the counter-offer set up
    /// by `setup_fully_consumed_sponsored_offer`.
    fn make_sell_op(asset_a: Asset, asset_b: Asset) -> Operation {
        Operation {
            source_account: None,
            body: OperationBody::ManageSellOffer(ManageSellOfferOp {
                selling: asset_b,
                buying: asset_a,
                amount: 100_000_000,
                price: Price { n: 1, d: 1 },
                offer_id: 0,
            }),
        }
    }

    /// Regression test for #2287 / AUDIT-249.
    ///
    /// ManageSellOffer with a new sponsored offer that fully crosses an
    /// existing counter-offer must return OpTooManySponsoring when the
    /// sponsor's num_sponsoring is already at u32::MAX. Pre-fix, henyey
    /// returned `Success(Deleted)` (consensus divergence vs stellar-core).
    #[test]
    fn test_manage_sell_offer_fully_consumed_sponsored_too_many_sponsoring() {
        let (mut state, context, source_id, sponsor_id, asset_a, asset_b) =
            setup_fully_consumed_sponsored_offer(u32::MAX, 0, 5_000_000);

        let op = make_sell_op(asset_a, asset_b);
        let result = super::super::execute_operation(&op, &source_id, &mut state, &context);
        match &result {
            Err(e) => panic!("Should not return Err: {:?}", e),
            Ok(r) => match &r.result {
                OperationResult::OpTooManySponsoring => {}
                other => panic!("Expected OpTooManySponsoring, got {:?}", other),
            },
        }

        let (num_sponsoring, _) = state.sponsorship_counts_for_account(&sponsor_id).unwrap();
        assert_eq!(
            num_sponsoring,
            u32::MAX as i64,
            "Sponsor's num_sponsoring must be unchanged on overflow"
        );
    }

    /// Regression test for #2287 / AUDIT-249 — ManageBuyOffer variant.
    #[test]
    fn test_manage_buy_offer_fully_consumed_sponsored_too_many_sponsoring() {
        let (mut state, context, source_id, sponsor_id, asset_a, asset_b) =
            setup_fully_consumed_sponsored_offer(u32::MAX, 0, 5_000_000);

        let op = Operation {
            source_account: None,
            body: OperationBody::ManageBuyOffer(ManageBuyOfferOp {
                selling: asset_b,
                buying: asset_a,
                buy_amount: 100_000_000,
                price: Price { n: 1, d: 1 },
                offer_id: 0,
            }),
        };
        let result = super::super::execute_operation(&op, &source_id, &mut state, &context);
        match &result {
            Err(e) => panic!("Should not return Err: {:?}", e),
            Ok(r) => match &r.result {
                OperationResult::OpTooManySponsoring => {}
                other => panic!("Expected OpTooManySponsoring, got {:?}", other),
            },
        }

        let (num_sponsoring, _) = state.sponsorship_counts_for_account(&sponsor_id).unwrap();
        assert_eq!(num_sponsoring, u32::MAX as i64);
    }

    /// Regression test for #2287 / AUDIT-249 — CreatePassiveSellOffer variant.
    ///
    /// Passive offers only cross when the counter's price is *strictly* better
    /// (offer_filter at manage_offer.rs:912-913: passive requires
    /// `price_cmp == Ordering::Less`). The new passive's price is therefore set
    /// worse than the counter's so the matching engine actually crosses it
    /// (otherwise the passive would trigger StopBadPrice and the offer would be
    /// kept, exercising the ALREADY-correct kept-offer code path with its
    /// existing overflow check rather than the fully-consumed branch this
    /// regression test targets).
    ///
    /// At new.price=1/2 the matching engine fills at counter's better price 1/1
    /// (maker price wins), so 100M asset_b sold gives the new offer 100M
    /// asset_a — fully consuming both sides.
    #[test]
    fn test_create_passive_sell_offer_fully_consumed_sponsored_too_many_sponsoring() {
        let (mut state, context, source_id, sponsor_id, asset_a, asset_b) =
            setup_fully_consumed_sponsored_offer(u32::MAX, 0, 5_000_000);

        let op = Operation {
            source_account: None,
            body: OperationBody::CreatePassiveSellOffer(CreatePassiveSellOfferOp {
                selling: asset_b,
                buying: asset_a,
                amount: 100_000_000,
                // Worse than counter's 1/1 so passive's max_wheat_price (2/1)
                // is strictly greater than counter's price (1/1) → crosses.
                price: Price { n: 1, d: 2 },
            }),
        };
        let result = super::super::execute_operation(&op, &source_id, &mut state, &context);
        match &result {
            Err(e) => panic!("Should not return Err: {:?}", e),
            Ok(r) => match &r.result {
                OperationResult::OpTooManySponsoring => {}
                other => panic!("Expected OpTooManySponsoring, got {:?}", other),
            },
        }

        let (num_sponsoring, _) = state.sponsorship_counts_for_account(&sponsor_id).unwrap();
        assert_eq!(num_sponsoring, u32::MAX as i64);
    }

    /// Combined-cap branch: protocol-18+ rule that
    /// `num_sponsoring + num_sub_entries <= u32::MAX` (stellar-core's
    /// `isSponsoringSubentrySumIncreaseValid`). Sponsor has small
    /// `num_sponsoring` but `num_sub_entries == u32::MAX - num_sponsoring`,
    /// so adding +1 to `num_sponsoring` would overflow the combined cap.
    ///
    /// Uses `base_reserve=1` so the LowReserve gate at manage_offer.rs:228-238
    /// does not short-circuit before our new check fires (precedent:
    /// sponsorship.rs::test_revoke_sponsorship_establish_too_many_sponsoring).
    #[test]
    fn test_manage_sell_offer_fully_consumed_sponsored_combined_cap_overflow() {
        let small = 100u32;
        let (mut state, context, source_id, sponsor_id, asset_a, asset_b) =
            setup_fully_consumed_sponsored_offer(small, u32::MAX - small, 1);

        let op = make_sell_op(asset_a, asset_b);
        let result = super::super::execute_operation(&op, &source_id, &mut state, &context);
        match &result {
            Err(e) => panic!("Should not return Err: {:?}", e),
            Ok(r) => match &r.result {
                OperationResult::OpTooManySponsoring => {}
                other => panic!(
                    "Expected OpTooManySponsoring (combined-cap overflow), got {:?}",
                    other
                ),
            },
        }

        let (num_sponsoring, _) = state.sponsorship_counts_for_account(&sponsor_id).unwrap();
        assert_eq!(num_sponsoring, small as i64);
    }

    /// No-spurious-failure sentinel: sponsor far below the caps, fully-consumed
    /// sponsored offer should succeed AND sponsor's num_sponsoring must be
    /// unchanged AND the sponsor must be pushed onto modified_accounts AFTER
    /// the source.
    ///
    /// (`record_account_access(source)` is itself a contains-skip no-op once
    /// source has been pushed by upstream `apply_balance_delta` during the
    /// matching exchange. The assertion guards against future regressions
    /// where a refactor might cause the sponsor to be pushed before source
    /// via some other path.)
    #[test]
    fn test_manage_sell_offer_fully_consumed_sponsored_no_spurious_failure() {
        let (mut state, context, source_id, sponsor_id, asset_a, asset_b) =
            setup_fully_consumed_sponsored_offer(100, 0, 5_000_000);

        let op = make_sell_op(asset_a, asset_b);
        let result = super::super::execute_operation(&op, &source_id, &mut state, &context);
        match &result {
            Err(e) => panic!("Should not return Err: {:?}", e),
            Ok(r) => match &r.result {
                OperationResult::OpInner(OperationResultTr::ManageSellOffer(
                    ManageSellOfferResult::Success(success),
                )) => {
                    assert!(
                        matches!(success.offer, ManageOfferSuccessResultOffer::Deleted),
                        "Expected Deleted, got {:?}",
                        success.offer
                    );
                    assert_eq!(success.offers_claimed.len(), 1);
                }
                other => panic!("Expected Success(Deleted), got {:?}", other),
            },
        }

        // Sponsor counts unchanged.
        let (num_sponsoring, _) = state.sponsorship_counts_for_account(&sponsor_id).unwrap();
        assert_eq!(
            num_sponsoring, 100i64,
            "sponsor num_sponsoring must be unchanged on success"
        );

        // Source must be pushed onto modified_accounts BEFORE sponsor.
        let modified = state.modified_accounts_for_test();
        let src_idx = modified
            .iter()
            .position(|a| a == &source_id)
            .expect("source must be in modified_accounts");
        let spn_idx = modified
            .iter()
            .position(|a| a == &sponsor_id)
            .expect("sponsor must be in modified_accounts");
        assert!(
            src_idx < spn_idx,
            "source (idx {}) must precede sponsor (idx {}) in modified_accounts: {:?}",
            src_idx,
            spn_idx,
            modified,
        );
    }
}
