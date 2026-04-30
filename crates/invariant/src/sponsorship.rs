//! SponsorshipCountIsValid invariant.
//!
//! Verifies that changes to `num_sponsoring` and `num_sponsored` on each account
//! are consistent with the sponsorship extensions on changed ledger entries.
//!
//! # Parity
//!
//! stellar-core: `src/invariant/SponsorshipCountIsValid.cpp`
//! Strictness: non-strict (`Invariant(false)`)

use std::collections::HashMap;

use stellar_xdr::curr::{
    AccountEntry, AccountEntryExt, AccountEntryExtensionV1Ext, AccountId, ContractEvent,
    LedgerEntry, LedgerEntryData, LedgerEntryExt, Operation, OperationResult, TrustLineAsset,
};

use crate::{Invariant, OperationDelta};

pub struct SponsorshipCountIsValid;

impl SponsorshipCountIsValid {
    pub fn new() -> Self {
        Self
    }
}

/// Get the multiplier for sponsorship counting.
/// Accounts count as 2 (account + base reserve), pool share trustlines as 2,
/// claimable balances as claimant count, others as 1.
fn get_mult(entry: &LedgerEntry) -> i64 {
    match &entry.data {
        LedgerEntryData::Account(_) => 2,
        LedgerEntryData::Trustline(tl) => {
            if matches!(tl.asset, TrustLineAsset::PoolShare(_)) {
                2
            } else {
                1
            }
        }
        LedgerEntryData::Offer(_) | LedgerEntryData::Data(_) => 1,
        LedgerEntryData::ClaimableBalance(cb) => cb.claimants.len() as i64,
        // Contract data, contract code, config settings, TTL, liquidity pool
        // are not sponsorable in the same way.
        _ => 0,
    }
}

/// Get the owning account ID for sponsorship purposes.
fn get_account_id(entry: &LedgerEntry) -> Option<&AccountId> {
    match &entry.data {
        LedgerEntryData::Account(acc) => Some(&acc.account_id),
        LedgerEntryData::Trustline(tl) => Some(&tl.account_id),
        LedgerEntryData::Offer(offer) => Some(&offer.seller_id),
        LedgerEntryData::Data(data) => Some(&data.account_id),
        _ => None,
    }
}

/// Check if an account entry has a V2 extension.
fn has_account_ext_v2(acc: &AccountEntry) -> bool {
    matches!(
        &acc.ext,
        AccountEntryExt::V1(v1) if matches!(&v1.ext, AccountEntryExtensionV1Ext::V2(_))
    )
}

/// Update sponsorship counters for a single entry.
fn update_counters(
    entry: &LedgerEntry,
    num_sponsoring: &mut HashMap<AccountId, i64>,
    num_sponsored: &mut HashMap<AccountId, i64>,
    claimable_balance_reserve: &mut i64,
    sign: i64,
) {
    // Check for sponsoring extension on the entry itself.
    if let LedgerEntryExt::V1(v1) = &entry.ext {
        if let Some(ref sponsor) = v1.sponsoring_id.0 {
            let mult = sign * get_mult(entry);
            *num_sponsoring.entry(sponsor.clone()).or_default() += mult;
            if !matches!(&entry.data, LedgerEntryData::ClaimableBalance(_)) {
                if let Some(account_id) = get_account_id(entry) {
                    *num_sponsored.entry(account_id.clone()).or_default() += mult;
                }
            } else {
                *claimable_balance_reserve += mult;
            }
        }
    }

    // For accounts, also check signer sponsoring IDs.
    if let LedgerEntryData::Account(acc) = &entry.data {
        if has_account_ext_v2(acc) {
            if let AccountEntryExt::V1(v1) = &acc.ext {
                if let AccountEntryExtensionV1Ext::V2(v2) = &v1.ext {
                    for sponsor_opt in v2.signer_sponsoring_i_ds.iter() {
                        if let Some(ref sponsor) = sponsor_opt.0 {
                            *num_sponsoring.entry(sponsor.clone()).or_default() += sign;
                            *num_sponsored.entry(acc.account_id.clone()).or_default() += sign;
                        }
                    }
                }
            }
        }
    }
}

/// Get the delta in num_sponsoring and num_sponsored from an account entry.
fn get_delta_sponsoring_and_sponsored(
    entry: Option<&LedgerEntry>,
    num_sponsoring: &mut i64,
    num_sponsored: &mut i64,
    sign: i64,
) {
    if let Some(entry) = entry {
        if let LedgerEntryData::Account(acc) = &entry.data {
            if has_account_ext_v2(acc) {
                if let AccountEntryExt::V1(v1) = &acc.ext {
                    if let AccountEntryExtensionV1Ext::V2(v2) = &v1.ext {
                        *num_sponsoring += sign * v2.num_sponsoring as i64;
                        *num_sponsored += sign * v2.num_sponsored as i64;
                    }
                }
            }
        }
    }
}

impl Invariant for SponsorshipCountIsValid {
    fn name(&self) -> &str {
        "SponsorshipCountIsValid"
    }

    fn is_strict(&self) -> bool {
        false
    }

    fn check_on_operation_apply(
        &self,
        _operation: &Operation,
        _op_result: &OperationResult,
        delta: &OperationDelta<'_>,
        _events: &[ContractEvent],
    ) -> Result<(), String> {
        // Sponsorships only exist from protocol 14+.
        // henyey is P24+ so this always applies, but keep the guard for clarity.
        if delta.ledger_version < 14 {
            return Ok(());
        }

        let mut num_sponsoring: HashMap<AccountId, i64> = HashMap::new();
        let mut num_sponsored: HashMap<AccountId, i64> = HashMap::new();
        let mut claimable_balance_reserve: i64 = 0;

        // Process created entries.
        for entry in delta.created {
            update_counters(
                entry,
                &mut num_sponsoring,
                &mut num_sponsored,
                &mut claimable_balance_reserve,
                1,
            );
        }

        // Process updated entries (current - previous).
        for (current, previous) in delta.updated.iter().zip(delta.update_states.iter()) {
            update_counters(
                current,
                &mut num_sponsoring,
                &mut num_sponsored,
                &mut claimable_balance_reserve,
                1,
            );
            update_counters(
                previous,
                &mut num_sponsoring,
                &mut num_sponsored,
                &mut claimable_balance_reserve,
                -1,
            );
        }

        // Process deleted entries.
        for entry in delta.delete_states {
            update_counters(
                entry,
                &mut num_sponsoring,
                &mut num_sponsored,
                &mut claimable_balance_reserve,
                -1,
            );
        }

        // Check accounts that appear in the delta.
        // For each account entry in the delta, verify that the change in
        // num_sponsoring/num_sponsored matches the calculated change.

        // Collect account entries from updated (both current and previous).
        for (current, previous) in delta.updated.iter().zip(delta.update_states.iter()) {
            if let LedgerEntryData::Account(acc) = &current.data {
                let account_id = &acc.account_id;

                let mut delta_sponsoring: i64 = 0;
                let mut delta_sponsored: i64 = 0;
                get_delta_sponsoring_and_sponsored(
                    Some(current),
                    &mut delta_sponsoring,
                    &mut delta_sponsored,
                    1,
                );
                get_delta_sponsoring_and_sponsored(
                    Some(previous),
                    &mut delta_sponsoring,
                    &mut delta_sponsored,
                    -1,
                );

                let expected_sponsoring = num_sponsoring.get(account_id).copied().unwrap_or(0);
                if expected_sponsoring != delta_sponsoring {
                    return Err(format!(
                        "Change in Account {:?} numSponsoring ({}) does not \
                         match change in number of sponsored entries ({})",
                        account_id, delta_sponsoring, expected_sponsoring
                    ));
                }

                let expected_sponsored = num_sponsored.get(account_id).copied().unwrap_or(0);
                if expected_sponsored != delta_sponsored {
                    return Err(format!(
                        "Change in Account {:?} numSponsored ({}) does not \
                         match change in number of sponsored entries ({})",
                        account_id, delta_sponsored, expected_sponsored
                    ));
                }

                // Remove from maps so we can check for unmatched changes.
                num_sponsoring.remove(account_id);
                num_sponsored.remove(account_id);
            }
        }

        // Also check created accounts.
        for entry in delta.created {
            if let LedgerEntryData::Account(acc) = &entry.data {
                let account_id = &acc.account_id;

                let mut delta_sponsoring: i64 = 0;
                let mut delta_sponsored: i64 = 0;
                get_delta_sponsoring_and_sponsored(
                    Some(entry),
                    &mut delta_sponsoring,
                    &mut delta_sponsored,
                    1,
                );

                let expected_sponsoring = num_sponsoring.get(account_id).copied().unwrap_or(0);
                if expected_sponsoring != delta_sponsoring {
                    return Err(format!(
                        "Change in Account {:?} numSponsoring ({}) does not \
                         match change in number of sponsored entries ({})",
                        account_id, delta_sponsoring, expected_sponsoring
                    ));
                }

                let expected_sponsored = num_sponsored.get(account_id).copied().unwrap_or(0);
                if expected_sponsored != delta_sponsored {
                    return Err(format!(
                        "Change in Account {:?} numSponsored ({}) does not \
                         match change in number of sponsored entries ({})",
                        account_id, delta_sponsored, expected_sponsored
                    ));
                }

                num_sponsoring.remove(account_id);
                num_sponsored.remove(account_id);
            }
        }

        // Check deleted accounts.
        for entry in delta.delete_states {
            if let LedgerEntryData::Account(acc) = &entry.data {
                let account_id = &acc.account_id;

                let mut delta_sponsoring: i64 = 0;
                let mut delta_sponsored: i64 = 0;
                get_delta_sponsoring_and_sponsored(
                    Some(entry),
                    &mut delta_sponsoring,
                    &mut delta_sponsored,
                    -1,
                );

                let expected_sponsoring = num_sponsoring.get(account_id).copied().unwrap_or(0);
                if expected_sponsoring != delta_sponsoring {
                    return Err(format!(
                        "Change in Account {:?} numSponsoring ({}) does not \
                         match change in number of sponsored entries ({})",
                        account_id, delta_sponsoring, expected_sponsoring
                    ));
                }

                let expected_sponsored = num_sponsored.get(account_id).copied().unwrap_or(0);
                if expected_sponsored != delta_sponsored {
                    return Err(format!(
                        "Change in Account {:?} numSponsored ({}) does not \
                         match change in number of sponsored entries ({})",
                        account_id, delta_sponsored, expected_sponsored
                    ));
                }

                num_sponsoring.remove(account_id);
                num_sponsored.remove(account_id);
            }
        }

        // Check for unmatched changes (accounts that had sponsorship changes
        // but were not in the delta as account entries).
        for (account_id, count) in &num_sponsoring {
            if *count != 0 {
                return Err(format!(
                    "Change in Account {:?} numSponsoring (0) does not \
                     match change in number of sponsored entries ({})",
                    account_id, count
                ));
            }
        }
        for (account_id, count) in &num_sponsored {
            if *count != 0 {
                return Err(format!(
                    "Change in Account {:?} numSponsored (0) does not \
                     match change in number of sponsored entries ({})",
                    account_id, count
                ));
            }
        }

        Ok(())
    }
}
