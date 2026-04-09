//! Liability preparation for protocol upgrades and reserve increases.
//!
//! This module implements the `prepareLiabilities` function from stellar-core
//! (Upgrades.cpp:949-1127). It is called when:
//!
//! 1. The protocol version is upgraded to V10 (one-time migration to introduce
//!    liability tracking on offers)
//! 2. The base reserve increases and protocol >= V10 (offers that can no longer
//!    be supported by accounts' new minimum balances must be cleaned up)
//!
//! # Algorithm
//!
//! For every account that has offers:
//! 1. Calculate total liabilities for each asset across all offers
//! 2. For every asset with excess buying/selling liabilities, erase all offers
//!    for that side
//! 3. Update liabilities to reflect offers remaining in the book
//!
//! Parity: Upgrades.cpp:949-1127 `prepareLiabilities`

use crate::close_state::CloseLedgerState;
use crate::{reserves, trustlines, LedgerError, Result};
use henyey_common::asset::{add_balance, is_issuer};
use henyey_common::protocol::{protocol_version_starts_from, ProtocolVersion};
use henyey_tx::operations::execute::{
    adjust_offer_amount, exchange_v10_without_price_error_thresholds, RoundingType,
};
use std::collections::{BTreeMap, HashMap, HashSet};
use stellar_xdr::curr::{
    AccountEntry, AccountEntryExt, AccountEntryExtensionV1, AccountEntryExtensionV1Ext,
    AccountEntryExtensionV2, AccountEntryExtensionV2Ext, AccountId, Asset, LedgerEntry,
    LedgerEntryData, LedgerEntryExt, LedgerKey, LedgerKeyAccount, LedgerKeyTrustLine, Liabilities,
    OfferEntry, TrustLineEntry, TrustLineEntryExt, TrustLineEntryV1, TrustLineEntryV1Ext, VecM,
};

/// Result of updating a single offer during liability preparation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum UpdateOfferResult {
    /// Offer unchanged.
    Unchanged,
    /// Offer amount was adjusted to satisfy price thresholds.
    Adjusted,
    /// Offer was adjusted but the result was zero, so it will be erased.
    AdjustedToZero,
    /// Offer must be erased because liabilities exceed available balance/limit.
    Erase,
}

/// Accumulate liabilities for an asset, using `None` as an overflow sentinel.
///
/// Parity: Upgrades.cpp:712-730 `addLiabilities`
fn add_liabilities(
    liabilities: &mut BTreeMap<Asset, Option<i64>>,
    account_id: &AccountId,
    asset: &Asset,
    delta: i64,
) {
    let entry = liabilities.entry(asset.clone()).or_insert(Some(0));

    // Issuer of a non-native asset has no liabilities for that asset.
    if !matches!(asset, Asset::Native) && is_issuer(account_id, asset) {
        return;
    }

    if let Some(ref mut val) = entry {
        if let Some(new_val) = add_balance(*val, delta, i64::MAX) {
            *val = new_val;
        } else {
            // Overflow → mark as overflow sentinel (None).
            *entry = None;
        }
    }
    // If already None (overflow), stay None.
}

/// Compute the buying liabilities implied by an offer.
///
/// Parity: TransactionUtils.cpp:908-923 `getOfferBuyingLiabilities`
fn get_offer_buying_liabilities(offer: &OfferEntry) -> Result<i64> {
    let res = exchange_v10_without_price_error_thresholds(
        offer.price.clone(),
        offer.amount,
        i64::MAX,
        i64::MAX,
        i64::MAX,
        RoundingType::Normal,
    );
    match res {
        Ok(r) => Ok(r.num_sheep_send),
        Err(e) => Err(LedgerError::Internal(format!(
            "getOfferBuyingLiabilities failed for offer {}: {:?}",
            offer.offer_id, e
        ))),
    }
}

/// Compute the selling liabilities implied by an offer.
///
/// Parity: TransactionUtils.cpp:932-947 `getOfferSellingLiabilities`
fn get_offer_selling_liabilities(offer: &OfferEntry) -> Result<i64> {
    let res = exchange_v10_without_price_error_thresholds(
        offer.price.clone(),
        offer.amount,
        i64::MAX,
        i64::MAX,
        i64::MAX,
        RoundingType::Normal,
    );
    match res {
        Ok(r) => Ok(r.num_wheat_received),
        Err(e) => Err(LedgerError::Internal(format!(
            "getOfferSellingLiabilities failed for offer {}: {:?}",
            offer.offer_id, e
        ))),
    }
}

/// Get the available balance for an asset excluding liabilities.
///
/// Parity: Upgrades.cpp:732-758 `getAvailableBalanceExcludingLiabilities`
fn get_available_balance_excluding_liabilities(
    account_id: &AccountId,
    asset: &Asset,
    balance_above_reserve: i64,
    ltx: &CloseLedgerState,
) -> Result<i64> {
    if matches!(asset, Asset::Native) {
        return Ok(balance_above_reserve);
    }

    if is_issuer(account_id, asset) {
        return Ok(i64::MAX);
    }

    // Load the trustline to check authorization.
    let tl_key = make_trustline_key(account_id, asset);
    if let Some(entry) = ltx.get_entry(&tl_key)? {
        if let LedgerEntryData::Trustline(ref tl) = entry.data {
            if is_authorized_to_maintain_liabilities_tl(tl) {
                return Ok(tl.balance);
            }
        }
    }
    Ok(0)
}

/// Get the available limit for an asset excluding liabilities.
///
/// Parity: Upgrades.cpp:761-791 `getAvailableLimitExcludingLiabilities`
fn get_available_limit_excluding_liabilities(
    account_id: &AccountId,
    asset: &Asset,
    balance: i64,
    ltx: &CloseLedgerState,
) -> Result<i64> {
    if matches!(asset, Asset::Native) {
        return Ok(i64::MAX - balance);
    }

    if is_issuer(account_id, asset) {
        return Ok(i64::MAX);
    }

    let tl_key = make_trustline_key(account_id, asset);
    if let Some(entry) = ltx.get_entry(&tl_key)? {
        if let LedgerEntryData::Trustline(ref tl) = entry.data {
            if is_authorized_to_maintain_liabilities_tl(tl) {
                return Ok(tl.limit - tl.balance);
            }
        }
    }
    Ok(0)
}

/// Check if an offer should be deleted because its total liabilities exceed
/// the available cap.
///
/// Parity: Upgrades.cpp:793-807 `shouldDeleteOffer`
fn should_delete_offer(
    asset: &Asset,
    effective_balance: i64,
    liabilities: &BTreeMap<Asset, Option<i64>>,
    get_cap: impl Fn(&Asset, i64) -> Result<i64>,
) -> Result<bool> {
    let entry = liabilities
        .get(asset)
        .ok_or_else(|| LedgerError::Internal("liabilities were not calculated".to_string()))?;

    match entry {
        Some(liab) => {
            let cap = get_cap(asset, effective_balance)?;
            Ok(*liab > cap)
        }
        // None means overflow → always delete.
        None => Ok(true),
    }
}

/// Update a single offer, determining whether it should be kept, adjusted, or erased.
///
/// Parity: Upgrades.cpp:817-892 `updateOffer`
fn update_offer(
    offer: &mut OfferEntry,
    balance: i64,
    balance_above_reserve: i64,
    liabilities: &mut BTreeMap<Asset, Liabilities>,
    initial_buying_liabilities: &BTreeMap<Asset, Option<i64>>,
    initial_selling_liabilities: &BTreeMap<Asset, Option<i64>>,
    ltx: &CloseLedgerState,
) -> Result<UpdateOfferResult> {
    let seller_id = offer.seller_id.clone();

    // Check if selling side should cause deletion.
    let erase_sell = should_delete_offer(
        &offer.selling,
        balance_above_reserve,
        initial_selling_liabilities,
        |asset, eff_bal| {
            get_available_balance_excluding_liabilities(&seller_id, asset, eff_bal, ltx)
        },
    )?;

    // Check if buying side should cause deletion.
    let erase_buy = should_delete_offer(
        &offer.buying,
        balance,
        initial_buying_liabilities,
        |asset, eff_bal| get_available_limit_excluding_liabilities(&seller_id, asset, eff_bal, ltx),
    )?;

    let mut erase = erase_sell || erase_buy;
    let mut res = if erase {
        UpdateOfferResult::Erase
    } else {
        UpdateOfferResult::Unchanged
    };

    // If not erased, check that the offer passes the price threshold via adjustOffer.
    if !erase {
        match adjust_offer_amount(offer.price.clone(), offer.amount, i64::MAX) {
            Ok(0) => {
                erase = true;
                res = UpdateOfferResult::AdjustedToZero;
            }
            Err(_) => {
                erase = true;
                res = UpdateOfferResult::AdjustedToZero;
            }
            _ => {}
        }
    }

    if !erase {
        // Actually adjust the offer amount.
        let adj_amount =
            adjust_offer_amount(offer.price.clone(), offer.amount, i64::MAX).unwrap_or(0);
        if adj_amount != offer.amount {
            offer.amount = adj_amount;
            res = UpdateOfferResult::Adjusted;
        }

        // Accumulate new liabilities for surviving offers.
        if matches!(offer.buying, Asset::Native) || !is_issuer(&seller_id, &offer.buying) {
            let buying_liab = get_offer_buying_liabilities(offer)?;
            let entry = liabilities
                .entry(offer.buying.clone())
                .or_insert(Liabilities {
                    buying: 0,
                    selling: 0,
                });
            entry.buying = add_balance(entry.buying, buying_liab, i64::MAX).ok_or_else(|| {
                LedgerError::Internal("could not add buying liabilities".to_string())
            })?;
        }

        if matches!(offer.selling, Asset::Native) || !is_issuer(&seller_id, &offer.selling) {
            let selling_liab = get_offer_selling_liabilities(offer)?;
            let entry = liabilities
                .entry(offer.selling.clone())
                .or_insert(Liabilities {
                    buying: 0,
                    selling: 0,
                });
            entry.selling =
                add_balance(entry.selling, selling_liab, i64::MAX).ok_or_else(|| {
                    LedgerError::Internal("could not add selling liabilities".to_string())
                })?;
        }
    }

    Ok(res)
}

/// Erase an offer entry, handling sponsorship count adjustments.
///
/// Parity: Upgrades.cpp:917-947 `eraseOfferWithPossibleSponsorship`
///
/// This modifies the account entry in place (decrementing numSubEntries) and,
/// if the offer is sponsored, adjusts numSponsoring on the sponsor and
/// numSponsored on the offer owner.
///
/// Returns the sponsor AccountId if the offer was sponsored, so the caller
/// can track which accounts changed.
fn erase_offer_with_possible_sponsorship(
    offer_entry: &LedgerEntry,
    account: &mut AccountEntry,
    ltx: &mut CloseLedgerState,
    ledger_seq: u32,
) -> Result<Option<AccountId>> {
    let is_sponsored =
        matches!(&offer_entry.ext, LedgerEntryExt::V1(v1) if v1.sponsoring_id.0.is_some());

    let sponsor_id = if is_sponsored {
        if let LedgerEntryExt::V1(ref v1) = offer_entry.ext {
            v1.sponsoring_id.0.clone()
        } else {
            None
        }
    } else {
        None
    };

    if let Some(ref sponsor) = sponsor_id {
        // Parity: stellar-core throws if sponsoringID == sourceAccount for
        // non-claimable-balance entries (SponsorshipUtils.cpp:810-813).
        if *sponsor == account.account_id {
            return Err(LedgerError::Internal(
                "sponsoringID == sourceAccount for offer entry".to_string(),
            ));
        }

        // Sponsored offer: decrement numSubEntries on the owner account,
        // decrement numSponsored on the owner, decrement numSponsoring on sponsor.

        // Multiplier for offers is always 1.
        account.num_sub_entries = account.num_sub_entries.saturating_sub(1);

        // Decrement numSponsored on the owner account (the offer owner).
        let ext_v2 = ensure_account_ext_v2(account);
        ext_v2.num_sponsored = ext_v2.num_sponsored.saturating_sub(1);

        // Decrement numSponsoring on the sponsor account.
        // The sponsor may be different from the offer owner.
        update_sponsor_num_sponsoring(sponsor, -1, ltx, ledger_seq)?;
    } else {
        // Not sponsored: just decrement numSubEntries.
        account.num_sub_entries = account.num_sub_entries.saturating_sub(1);
    }

    Ok(sponsor_id)
}

/// Update `numSponsoring` on a sponsor account, loading from the CloseLedgerState read path.
///
/// This handles the case where the sponsor account may not be the same as the
/// offer owner, and may have already been modified in the delta.
fn update_sponsor_num_sponsoring(
    sponsor_id: &AccountId,
    delta_val: i64,
    ltx: &mut CloseLedgerState,
    ledger_seq: u32,
) -> Result<()> {
    let key = LedgerKey::Account(LedgerKeyAccount {
        account_id: sponsor_id.clone(),
    });

    // Load the current version of the sponsor account through the CloseLedgerState
    // read path (current delta → base snapshot).
    let entry = ltx.get_entry(&key)?.ok_or_else(|| {
        LedgerError::Internal(format!("sponsor account not found: {:?}", sponsor_id))
    })?;

    let previous = entry.clone();
    let mut updated = entry;

    if let LedgerEntryData::Account(ref mut acc) = updated.data {
        let ext_v2 = ensure_account_ext_v2(acc);
        let new_val = ext_v2.num_sponsoring as i64 + delta_val;
        if new_val < 0 || new_val > u32::MAX as i64 {
            return Err(LedgerError::Internal(
                "numSponsoring out of range".to_string(),
            ));
        }
        ext_v2.num_sponsoring = new_val as u32;
    }

    updated.last_modified_ledger_seq = ledger_seq;
    ltx.record_update(previous, updated)?;

    Ok(())
}

/// Ensure an AccountEntry has a V2 extension, creating intermediate
/// extensions as needed.
///
/// This is a local copy since the henyey-tx version is `pub(crate)`.
fn ensure_account_ext_v2(account: &mut AccountEntry) -> &mut AccountEntryExtensionV2 {
    let liabilities = match &account.ext {
        AccountEntryExt::V1(v1) => v1.liabilities.clone(),
        AccountEntryExt::V0 => Liabilities {
            buying: 0,
            selling: 0,
        },
    };

    match &account.ext {
        AccountEntryExt::V0 => {
            let signer_count = account.signers.len();
            account.ext = AccountEntryExt::V1(AccountEntryExtensionV1 {
                liabilities,
                ext: AccountEntryExtensionV1Ext::V2(AccountEntryExtensionV2 {
                    num_sponsored: 0,
                    num_sponsoring: 0,
                    signer_sponsoring_i_ds: build_signer_sponsoring_ids(signer_count),
                    ext: AccountEntryExtensionV2Ext::V0,
                }),
            });
        }
        AccountEntryExt::V1(v1) => {
            if matches!(v1.ext, AccountEntryExtensionV1Ext::V0) {
                let signer_count = account.signers.len();
                account.ext = AccountEntryExt::V1(AccountEntryExtensionV1 {
                    liabilities,
                    ext: AccountEntryExtensionV1Ext::V2(AccountEntryExtensionV2 {
                        num_sponsored: 0,
                        num_sponsoring: 0,
                        signer_sponsoring_i_ds: build_signer_sponsoring_ids(signer_count),
                        ext: AccountEntryExtensionV2Ext::V0,
                    }),
                });
            }
        }
    }

    if let AccountEntryExt::V1(v1) = &mut account.ext {
        if let AccountEntryExtensionV1Ext::V2(v2) = &mut v1.ext {
            return v2;
        }
    }
    unreachable!()
}

/// Ensure an AccountEntry has a V1 extension (for liabilities).
fn ensure_account_liabilities(account: &mut AccountEntry) -> &mut Liabilities {
    if matches!(account.ext, AccountEntryExt::V0) {
        account.ext = AccountEntryExt::V1(AccountEntryExtensionV1 {
            liabilities: Liabilities {
                buying: 0,
                selling: 0,
            },
            ext: AccountEntryExtensionV1Ext::V0,
        });
    }
    match &mut account.ext {
        AccountEntryExt::V1(v1) => &mut v1.liabilities,
        _ => unreachable!(),
    }
}

/// Ensure a TrustLineEntry has a V1 extension (for liabilities).
fn ensure_trustline_liabilities(trustline: &mut TrustLineEntry) -> &mut Liabilities {
    if matches!(trustline.ext, TrustLineEntryExt::V0) {
        trustline.ext = TrustLineEntryExt::V1(TrustLineEntryV1 {
            liabilities: Liabilities {
                buying: 0,
                selling: 0,
            },
            ext: TrustLineEntryV1Ext::V0,
        });
    }
    match &mut trustline.ext {
        TrustLineEntryExt::V1(v1) => &mut v1.liabilities,
        _ => unreachable!(),
    }
}

/// Build a default signer_sponsoring_ids vector of None values.
fn build_signer_sponsoring_ids(count: usize) -> VecM<stellar_xdr::curr::SponsorshipDescriptor, 20> {
    let ids: Vec<stellar_xdr::curr::SponsorshipDescriptor> = (0..count)
        .map(|_| stellar_xdr::curr::SponsorshipDescriptor(None))
        .collect();
    ids.try_into().unwrap_or_default()
}

/// Build a `LedgerKey::Trustline` for an account and asset.
fn make_trustline_key(account_id: &AccountId, asset: &Asset) -> LedgerKey {
    LedgerKey::Trustline(LedgerKeyTrustLine {
        account_id: account_id.clone(),
        asset: henyey_common::asset::asset_to_trustline_asset(asset),
    })
}

/// Check if a trustline is authorized to maintain liabilities.
///
/// Matches stellar-core's `isAuthorizedToMaintainLiabilities` check.
fn is_authorized_to_maintain_liabilities_tl(tl: &TrustLineEntry) -> bool {
    const AUTHORIZED_FLAG: u32 = stellar_xdr::curr::TrustLineFlags::AuthorizedFlag as u32;
    const AUTH_LIAB_FLAG: u32 =
        stellar_xdr::curr::TrustLineFlags::AuthorizedToMaintainLiabilitiesFlag as u32;
    tl.flags & (AUTHORIZED_FLAG | AUTH_LIAB_FLAG) != 0
}

/// Prepare liabilities: bring all offers and their associated account/trustline
/// liabilities into a valid state.
///
/// This is called during protocol V10 upgrades and when the base reserve increases
/// (protocol >= V10).
///
/// Parity: Upgrades.cpp:949-1127 `prepareLiabilities`
// SECURITY: liability clearing only runs on offers that have corresponding liabilities
// INVARIANT: offers always have corresponding liabilities after creation
pub fn prepare_liabilities(
    ltx: &mut CloseLedgerState,
    protocol_version: u32,
    base_reserve: u32,
    ledger_seq: u32,
) -> Result<()> {
    tracing::info!("Starting prepareLiabilities");

    // Step 1: Load all offers through the CloseLedgerState merged read path
    // (current delta → base snapshot) and group by account.
    // Parity: ltx.loadAllOffers()
    let all_entries = ltx.all_offers()?;
    let mut offers_by_account: BTreeMap<AccountId, Vec<(LedgerEntry, OfferEntry)>> =
        BTreeMap::new();

    for entry in &all_entries {
        if let LedgerEntryData::Offer(ref offer) = entry.data {
            offers_by_account
                .entry(offer.seller_id.clone())
                .or_default()
                .push((entry.clone(), offer.clone()));
        }
    }

    let mut changed_accounts: HashSet<LedgerKey> = HashSet::new();
    let mut n_changed_trustlines: u64 = 0;
    let mut n_updated_offers: HashMap<UpdateOfferResult, u64> = HashMap::new();

    // Step 2: Pre-compute min balances for all accounts with offers.
    // Parity: getOfferAccountMinBalances
    let mut min_balance_map: HashMap<AccountId, i64> = HashMap::new();
    for account_id in offers_by_account.keys() {
        let account = ltx
            .get_account(account_id)?
            .ok_or_else(|| LedgerError::Internal("account does not exist".to_string()))?;
        let min_balance = reserves::minimum_balance(&account, base_reserve);
        min_balance_map.insert(account_id.clone(), min_balance);
    }

    // Step 3: Process each account's offers.
    for (account_id, account_offers) in &offers_by_account {
        // 3a: Calculate initial liabilities across all offers for this account.
        let mut initial_buying_liabilities: BTreeMap<Asset, Option<i64>> = BTreeMap::new();
        let mut initial_selling_liabilities: BTreeMap<Asset, Option<i64>> = BTreeMap::new();

        for (_entry, offer) in account_offers {
            add_liabilities(
                &mut initial_buying_liabilities,
                &offer.seller_id,
                &offer.buying,
                get_offer_buying_liabilities(offer)?,
            );
            add_liabilities(
                &mut initial_selling_liabilities,
                &offer.seller_id,
                &offer.selling,
                get_offer_selling_liabilities(offer)?,
            );
        }

        // 3b: Load the account through the CloseLedgerState read path.
        // This automatically sees prior delta changes (e.g., sponsor adjustments
        // from processing a previous account's offers).
        let account_key = LedgerKey::Account(LedgerKeyAccount {
            account_id: account_id.clone(),
        });
        let account_entry = ltx
            .get_entry(&account_key)?
            .ok_or_else(|| LedgerError::Internal("account does not exist".to_string()))?;
        let mut account_entry_current = account_entry.clone();
        let account_before = if let LedgerEntryData::Account(ref acc) = account_entry.data {
            acc.clone()
        } else {
            return Err(LedgerError::Internal("expected account entry".to_string()));
        };

        let balance = account_before.balance;
        let min_balance = *min_balance_map
            .get(account_id)
            .ok_or_else(|| LedgerError::Internal("min balance missing from map".to_string()))?;
        let balance_above_reserve = balance - min_balance;

        // 3c: Process each offer.
        let mut new_liabilities: BTreeMap<Asset, Liabilities> = BTreeMap::new();
        let mut offers_to_erase: Vec<LedgerEntry> = Vec::new();
        let mut offers_to_update: Vec<(LedgerEntry, OfferEntry)> = Vec::new();

        for (entry, offer) in account_offers {
            let mut offer_copy = offer.clone();
            let offer_id = offer.offer_id;

            let res = update_offer(
                &mut offer_copy,
                balance,
                balance_above_reserve,
                &mut new_liabilities,
                &initial_buying_liabilities,
                &initial_selling_liabilities,
                ltx,
            )?;

            match res {
                UpdateOfferResult::AdjustedToZero | UpdateOfferResult::Erase => {
                    offers_to_erase.push(entry.clone());
                }
                UpdateOfferResult::Adjusted => {
                    offers_to_update.push((entry.clone(), offer_copy));
                }
                UpdateOfferResult::Unchanged => {}
            }

            *n_updated_offers.entry(res).or_insert(0) += 1;

            if res != UpdateOfferResult::Unchanged {
                let message = match res {
                    UpdateOfferResult::Adjusted => "was adjusted",
                    UpdateOfferResult::AdjustedToZero => "was adjusted to zero",
                    UpdateOfferResult::Erase => "was erased",
                    UpdateOfferResult::Unchanged => unreachable!(),
                };
                tracing::debug!(offer_id = offer_id, "{}", message);
            }
        }

        // 3d: Get a mutable reference to the account in our working copy.
        let account_mut = if let LedgerEntryData::Account(ref mut acc) = account_entry_current.data
        {
            acc
        } else {
            return Err(LedgerError::Internal("expected account entry".to_string()));
        };

        // 3e: Erase offers (handling sponsorship).
        for offer_entry in &offers_to_erase {
            let sponsor =
                erase_offer_with_possible_sponsorship(offer_entry, account_mut, ltx, ledger_seq)?;

            if let Some(ref sponsor_id) = sponsor {
                // Track changed sponsor account.
                let sponsor_key = LedgerKey::Account(LedgerKeyAccount {
                    account_id: sponsor_id.clone(),
                });
                changed_accounts.insert(sponsor_key);
            }

            // Record the offer deletion.
            ltx.record_delete(offer_entry.clone())?;
        }

        // 3f: Record updated offers.
        for (original_entry, updated_offer) in &offers_to_update {
            let mut new_entry = original_entry.clone();
            new_entry.data = LedgerEntryData::Offer(updated_offer.clone());
            new_entry.last_modified_ledger_seq = ledger_seq;
            ltx.record_update(original_entry.clone(), new_entry)?;
        }

        // 3g: Update liabilities on account and trustlines.
        // Parity: Upgrades.cpp:1055-1111
        for (asset, liab) in &new_liabilities {
            if matches!(asset, Asset::Native) {
                // Update account native liabilities.
                let current_selling = reserves::selling_liabilities(account_mut);
                let current_buying = reserves::buying_liabilities(account_mut);
                let delta_selling = liab.selling - current_selling;
                let delta_buying = liab.buying - current_buying;

                let acc_liab = ensure_account_liabilities(account_mut);
                let new_selling = acc_liab.selling.checked_add(delta_selling).ok_or_else(|| {
                    LedgerError::Internal("invalid selling liabilities during upgrade".to_string())
                })?;
                if new_selling < 0 {
                    return Err(LedgerError::Internal(
                        "invalid selling liabilities during upgrade".to_string(),
                    ));
                }
                acc_liab.selling = new_selling;

                let new_buying = acc_liab.buying.checked_add(delta_buying).ok_or_else(|| {
                    LedgerError::Internal("invalid buying liabilities during upgrade".to_string())
                })?;
                if new_buying < 0 {
                    return Err(LedgerError::Internal(
                        "invalid buying liabilities during upgrade".to_string(),
                    ));
                }
                acc_liab.buying = new_buying;
            } else {
                // Update trustline liabilities for non-native assets.
                let tl_key = make_trustline_key(account_id, asset);
                let tl_entry = ltx
                    .get_entry(&tl_key)?
                    .ok_or_else(|| LedgerError::Internal("trustline not found".to_string()))?;

                let mut tl_entry_new = tl_entry.clone();
                if let LedgerEntryData::Trustline(ref mut tl) = tl_entry_new.data {
                    let current_selling = trustlines::selling_liabilities(tl);
                    let current_buying = trustlines::buying_liabilities(tl);
                    let delta_selling = liab.selling - current_selling;
                    let delta_buying = liab.buying - current_buying;

                    if delta_selling != 0 || delta_buying != 0 {
                        n_changed_trustlines += 1;
                    }

                    // Parity: V11+ deltas should not be positive.
                    if protocol_version_starts_from(protocol_version, ProtocolVersion::V11)
                        && (delta_selling > 0 || delta_buying > 0)
                    {
                        return Err(LedgerError::Internal(
                            "invalid liabilities delta".to_string(),
                        ));
                    }

                    let tl_liab = ensure_trustline_liabilities(tl);
                    let new_selling =
                        tl_liab.selling.checked_add(delta_selling).ok_or_else(|| {
                            LedgerError::Internal(
                                "invalid selling liabilities during upgrade".to_string(),
                            )
                        })?;
                    if new_selling < 0 {
                        return Err(LedgerError::Internal(
                            "invalid selling liabilities during upgrade".to_string(),
                        ));
                    }
                    tl_liab.selling = new_selling;

                    let new_buying = tl_liab.buying.checked_add(delta_buying).ok_or_else(|| {
                        LedgerError::Internal(
                            "invalid buying liabilities during upgrade".to_string(),
                        )
                    })?;
                    if new_buying < 0 {
                        return Err(LedgerError::Internal(
                            "invalid buying liabilities during upgrade".to_string(),
                        ));
                    }
                    tl_liab.buying = new_buying;

                    if delta_selling != 0 || delta_buying != 0 {
                        tl_entry_new.last_modified_ledger_seq = ledger_seq;
                        ltx.record_update(tl_entry.clone(), tl_entry_new)?;
                    }
                }
            }
        }

        // 3h: If the account changed, record the update.
        // The CloseLedgerState delta's coalescing logic keeps the original previous
        // and replaces the current with our updated entry.
        if account_mut != &account_before {
            changed_accounts.insert(account_key.clone());
            account_entry_current.last_modified_ledger_seq = ledger_seq;
            ltx.record_update(account_entry.clone(), account_entry_current)?;
        }
    }

    tracing::info!(
        changed_accounts = changed_accounts.len(),
        changed_trustlines = n_changed_trustlines,
        adjusted = n_updated_offers
            .get(&UpdateOfferResult::Adjusted)
            .copied()
            .unwrap_or(0),
        adjusted_to_zero = n_updated_offers
            .get(&UpdateOfferResult::AdjustedToZero)
            .copied()
            .unwrap_or(0),
        erased = n_updated_offers
            .get(&UpdateOfferResult::Erase)
            .copied()
            .unwrap_or(0),
        "prepareLiabilities completed"
    );

    Ok(())
}
