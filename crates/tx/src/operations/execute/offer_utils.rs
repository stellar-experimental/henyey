//! Shared DEX offer helpers used by manage_offer, path_payment, and trust_flags.
//!
//! These helpers encapsulate the common patterns for:
//! - Computing sell-offer liabilities
//! - Determining available selling capacity
//! - Crossing an offer (the v10 exchange path)
//! - Deleting an offer with sponsorship/sub-entry cleanup

use stellar_xdr::curr::{
    AccountId, Asset, ClaimAtom, ClaimOfferAtom, LedgerKey, LedgerKeyOffer, OfferEntry, Price,
};

use super::offer_exchange::{
    adjust_offer_amount, exchange_v10, exchange_v10_without_price_error_thresholds, RoundingType,
};
use super::{
    account_liabilities, apply_balance_delta, apply_liabilities_delta, can_buy_at_most,
    dec_sub_entries, is_authorized_to_maintain_liabilities, issuer_for_asset, map_exchange_error,
    trustline_liabilities,
};
use crate::state::LedgerStateManager;
use crate::validation::LedgerContext;
use crate::Result;

/// Compute the (selling_liability, buying_liability) for a sell-side offer.
///
/// A sell offer of `amount` at `price` creates:
/// - selling_liability = amount (in the selling asset)
/// - buying_liability  = amount converted at price (in the buying asset)
pub(super) fn offer_liabilities_sell(amount: i64, price: &Price) -> Result<(i64, i64)> {
    let res = exchange_v10_without_price_error_thresholds(
        price.clone(),
        amount,
        i64::MAX,
        i64::MAX,
        i64::MAX,
        RoundingType::Normal,
    )
    .map_err(map_exchange_error)?;
    Ok((res.num_wheat_received, res.num_sheep_send))
}

/// Compute how much of `asset` the source account can sell, considering:
/// - Native: balance − minimum reserve − selling liabilities
/// - Issued by source: i64::MAX
/// - Non-native: trustline balance − selling liabilities (if authorized)
///
/// When `reserve_subentry` is true, the minimum balance calculation includes
/// one additional sub-entry (used when creating a new offer alongside the sell).
pub(super) fn can_sell_at_most(
    source: &AccountId,
    asset: &Asset,
    state: &LedgerStateManager,
    context: &LedgerContext,
    reserve_subentry: bool,
) -> Result<i64> {
    if matches!(asset, Asset::Native) {
        let Some(account) = state.get_account(source) else {
            return Ok(0);
        };
        let additional_subentries = if reserve_subentry { 1 } else { 0 };
        let min_balance = state.minimum_balance_for_account(
            account,
            context.protocol_version,
            additional_subentries,
        )?;
        let available = account.balance - min_balance - account_liabilities(account).selling;
        return Ok(available.max(0));
    }

    if issuer_for_asset(asset) == Some(source) {
        return Ok(i64::MAX);
    }

    let Some(trustline) = state.get_trustline(source, asset) else {
        return Ok(0);
    };
    if !is_authorized_to_maintain_liabilities(trustline.flags) {
        return Ok(0);
    }
    let available = trustline.balance - trustline_liabilities(trustline).selling;
    Ok(available.max(0))
}

/// Delete an offer and handle sponsorship/sub-entry cleanup.
///
/// Uses a validate-then-mutate pattern matching stellar-core's
/// `removeEntryWithPossibleSponsorship` (SponsorshipUtils.cpp:809-847):
///
/// Phase 1 — Validate: ensure accounts exist, counts won't underflow.
/// Phase 2 — Mutate: delete offer, update sponsorship counts, decrement sub-entries.
///
/// Data integrity violations (missing accounts, insufficient counts) return
/// `TxError::Internal`, matching stellar-core's `std::runtime_error` behavior.
pub(super) fn delete_offer_with_sponsorship(
    seller: &AccountId,
    offer_id: i64,
    state: &mut LedgerStateManager,
) -> Result<()> {
    use crate::error::TxError;

    let ledger_key = LedgerKey::Offer(LedgerKeyOffer {
        seller_id: seller.clone(),
        offer_id,
    });

    // Phase 1: Validate — no state mutations
    let sponsor = state.entry_sponsor(&ledger_key);
    if let Some(ref sponsor) = sponsor {
        state.ensure_account_loaded(sponsor)?;
        let (num_sponsoring, _) = state
            .sponsorship_counts_for_account(sponsor)
            .ok_or_else(|| TxError::Internal("sponsor account missing".into()))?;
        if num_sponsoring < 1 {
            return Err(TxError::Internal("insufficient numSponsoring".into()));
        }
        state.ensure_account_loaded(seller)?;
        let (_, num_sponsored) = state
            .sponsorship_counts_for_account(seller)
            .ok_or_else(|| TxError::Internal("sponsored account missing".into()))?;
        if num_sponsored < 1 {
            return Err(TxError::Internal("insufficient numSponsored".into()));
        }
    }
    state.ensure_account_loaded(seller)?;
    let seller_account = state
        .get_account(seller)
        .ok_or_else(|| TxError::Internal(format!("seller account missing for offer {offer_id}")))?;
    if seller_account.num_sub_entries < 1 {
        return Err(TxError::Internal(format!(
            "seller num_sub_entries is 0 for offer {offer_id}"
        )));
    }

    // Phase 2: Mutate — all preconditions verified
    state.delete_offer(seller, offer_id);
    if let Some(sponsor) = sponsor {
        state.update_num_sponsoring(&sponsor, -1)?;
        state.update_num_sponsored(seller, -1)?;
    }
    let account = state
        .get_account_mut(seller)
        .expect("seller account validated above");
    dec_sub_entries(account, 1);
    Ok(())
}

/// Cross a single sell offer (v10+ exchange path).
///
/// This is the core offer-crossing logic used by both manage_offer and path_payment:
/// 1. Release the offer's current liabilities
/// 2. Compute available selling/buying capacity
/// 3. Perform the exchange calculation
/// 4. Apply balance deltas to the seller
/// 5. Delete the offer (if fully consumed) or update with new amount
/// 6. Record a ClaimAtom in the offer trail
///
/// Returns `(wheat_received, sheep_sent, wheat_stays)`.
pub(super) fn cross_offer_v10(
    offer: &OfferEntry,
    max_wheat_receive: i64,
    max_sheep_send: i64,
    round: RoundingType,
    offer_trail: &mut Vec<ClaimAtom>,
    state: &mut LedgerStateManager,
    context: &LedgerContext,
) -> Result<(i64, i64, bool)> {
    let seller = offer.seller_id.clone();
    let selling = &offer.selling;
    let buying = &offer.buying;

    // Batch-load seller's account and trustlines in a single bucket list pass.
    state.ensure_offer_entries_loaded(&seller, selling, buying)?;

    // Release liabilities FIRST (matches stellar-core exactly).
    // The available balance calculation depends on liabilities being released first.
    let (selling_liab, buying_liab) = offer_liabilities_sell(offer.amount, &offer.price)?;
    apply_liabilities_delta(&seller, selling, buying, -selling_liab, -buying_liab, state)?;

    // Calculate available amounts AFTER liabilities are released.
    let max_wheat_send = offer
        .amount
        .min(can_sell_at_most(&seller, selling, state, context, false)?);
    let max_sheep_receive = can_buy_at_most(&seller, buying, state);

    // Adjust offer amount (stellar-core calls adjustOffer as "preventative measure").
    let adjusted_offer_amount =
        adjust_offer_amount(offer.price.clone(), max_wheat_send, max_sheep_receive)
            .map_err(map_exchange_error)?;

    // Perform the exchange calculation.
    let exchange = exchange_v10(
        offer.price.clone(),
        adjusted_offer_amount,
        max_wheat_receive,
        max_sheep_send,
        max_sheep_receive,
        round,
    )
    .map_err(map_exchange_error)?;

    let num_wheat_received = exchange.num_wheat_received;
    let num_sheep_send = exchange.num_sheep_send;
    let wheat_stays = exchange.wheat_stays;

    // Apply balance changes.
    if num_sheep_send != 0 {
        apply_balance_delta(&seller, buying, num_sheep_send, state)?;
    }
    if num_wheat_received != 0 {
        apply_balance_delta(&seller, selling, -num_wheat_received, state)?;
    }

    // Calculate new offer amount and handle offer update/deletion.
    let new_amount = if wheat_stays {
        let tentative = adjusted_offer_amount.saturating_sub(num_wheat_received);
        if tentative > 0 {
            // Re-adjust after balance changes.
            let post_wheat_send =
                tentative.min(can_sell_at_most(&seller, selling, state, context, false)?);
            let post_sheep_receive = can_buy_at_most(&seller, buying, state);
            adjust_offer_amount(offer.price.clone(), post_wheat_send, post_sheep_receive)
                .map_err(map_exchange_error)?
        } else {
            0
        }
    } else {
        0
    };

    if new_amount == 0 {
        delete_offer_with_sponsorship(&seller, offer.offer_id, state)?;
    } else {
        let updated = OfferEntry {
            amount: new_amount,
            ..offer.clone()
        };
        state.update_offer(updated);
        let (new_selling, new_buying) = offer_liabilities_sell(new_amount, &offer.price)?;
        apply_liabilities_delta(&seller, selling, buying, new_selling, new_buying, state)?;
    }

    offer_trail.push(ClaimAtom::OrderBook(ClaimOfferAtom {
        seller_id: seller,
        offer_id: offer.offer_id,
        asset_sold: offer.selling.clone(),
        amount_sold: num_wheat_received,
        asset_bought: offer.buying.clone(),
        amount_bought: num_sheep_send,
    }));

    Ok((num_wheat_received, num_sheep_send, wheat_stays))
}
