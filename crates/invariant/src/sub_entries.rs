//! AccountSubEntriesCountIsValid invariant.
//!
//! Verifies that the change in `num_sub_entries` on each account matches the
//! calculated change in the number of sub-entries (trustlines, offers, data
//! entries). Pool share trustlines count as 2 sub-entries.
//!
//! # Parity
//!
//! stellar-core: `src/invariant/AccountSubEntriesCountIsValid.cpp`
//! Strictness: non-strict (`Invariant(false)`)

use std::collections::HashMap;

use stellar_xdr::curr::{
    AccountId, ContractEvent, LedgerEntry, LedgerEntryData, Operation, OperationResult,
    TrustLineAsset,
};

use crate::{Invariant, OperationDelta};

pub struct AccountSubEntriesCountIsValid;

impl AccountSubEntriesCountIsValid {
    pub fn new() -> Self {
        Self
    }
}

/// Tracks the delta for a single account.
#[derive(Default)]
struct SubEntriesChange {
    /// Change in the account's `num_sub_entries` field.
    num_sub_entries: i32,
    /// Change in signer count.
    signers: i32,
    /// Calculated change in sub-entries from non-account entry changes.
    calculated_sub_entries: i32,
}

fn is_pool_share_trustline(entry: &LedgerEntry) -> bool {
    matches!(
        &entry.data,
        LedgerEntryData::Trustline(tl) if matches!(tl.asset, TrustLineAsset::PoolShare(_))
    )
}

/// Calculate the sub-entry delta for an entry appearing/disappearing.
/// Pool share trustlines count as 2; all other sub-entry types count as 1.
fn calculate_delta(current: Option<&LedgerEntry>, previous: Option<&LedgerEntry>) -> i32 {
    let mut delta = 0i32;
    if let Some(entry) = current {
        if is_pool_share_trustline(entry) {
            delta += 2;
        } else {
            delta += 1;
        }
    }
    if let Some(entry) = previous {
        if is_pool_share_trustline(entry) {
            delta -= 2;
        } else {
            delta -= 1;
        }
    }
    delta
}

/// Get the owning account ID for a sub-entry type, or None for non-sub-entry types.
fn get_sub_entry_account(entry: &LedgerEntry) -> Option<&AccountId> {
    match &entry.data {
        LedgerEntryData::Trustline(tl) => Some(&tl.account_id),
        LedgerEntryData::Offer(offer) => Some(&offer.seller_id),
        LedgerEntryData::Data(data) => Some(&data.account_id),
        _ => None,
    }
}

/// Update the sub-entries change map for a single entry delta.
fn update_changed_sub_entries(
    changes: &mut HashMap<AccountId, SubEntriesChange>,
    current: Option<&LedgerEntry>,
    previous: Option<&LedgerEntry>,
) {
    let valid = current.or(previous).expect("at least one entry must exist");

    match &valid.data {
        LedgerEntryData::Account(acc) => {
            let account_id = acc.account_id.clone();
            let change = changes.entry(account_id).or_default();
            change.num_sub_entries += current
                .map(|e| {
                    if let LedgerEntryData::Account(a) = &e.data {
                        a.num_sub_entries as i32
                    } else {
                        0
                    }
                })
                .unwrap_or(0)
                - previous
                    .map(|e| {
                        if let LedgerEntryData::Account(a) = &e.data {
                            a.num_sub_entries as i32
                        } else {
                            0
                        }
                    })
                    .unwrap_or(0);

            let cur_signers = current
                .map(|e| {
                    if let LedgerEntryData::Account(a) = &e.data {
                        a.signers.len() as i32
                    } else {
                        0
                    }
                })
                .unwrap_or(0);
            let prev_signers = previous
                .map(|e| {
                    if let LedgerEntryData::Account(a) = &e.data {
                        a.signers.len() as i32
                    } else {
                        0
                    }
                })
                .unwrap_or(0);
            change.signers += cur_signers - prev_signers;
            change.calculated_sub_entries += cur_signers - prev_signers;
        }
        LedgerEntryData::Trustline(_) | LedgerEntryData::Offer(_) | LedgerEntryData::Data(_) => {
            let account_id = get_sub_entry_account(valid).unwrap().clone();
            let change = changes.entry(account_id).or_default();
            change.calculated_sub_entries += calculate_delta(current, previous);
        }
        // Claimable balances, liquidity pools, contract data/code, config settings, TTL
        // are not sub-entries.
        _ => {}
    }
}

impl Invariant for AccountSubEntriesCountIsValid {
    fn name(&self) -> &str {
        "AccountSubEntriesCountIsValid"
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
        let mut changes: HashMap<AccountId, SubEntriesChange> = HashMap::new();

        // Process created entries (current exists, no previous).
        for entry in delta.created {
            update_changed_sub_entries(&mut changes, Some(entry), None);
        }

        // Process updated entries (both current and previous exist).
        for (current, previous) in delta.updated.iter().zip(delta.update_states.iter()) {
            update_changed_sub_entries(&mut changes, Some(current), Some(previous));
        }

        // Process deleted entries (no current, previous exists).
        for previous in delta.delete_states {
            update_changed_sub_entries(&mut changes, None, Some(previous));
        }

        // Check: for each account, num_sub_entries delta == calculated sub-entries delta.
        for (account_id, change) in &changes {
            if change.num_sub_entries != change.calculated_sub_entries {
                return Err(format!(
                    "Change in Account {:?} numSubEntries ({}) does not match \
                     change in number of subentries ({})",
                    account_id, change.num_sub_entries, change.calculated_sub_entries
                ));
            }
        }

        // Check deleted accounts: when an account is deleted (no current), verify
        // that its remaining sub-entries are only signers (matching stellar-core
        // AccountSubEntriesCountIsValid.cpp:172-205).
        for previous in delta.delete_states {
            if let LedgerEntryData::Account(account) = &previous.data {
                let change = changes
                    .get(&account.account_id)
                    .map(|c| c.clone_counts())
                    .unwrap_or_default();
                let num_signers =
                    account.num_sub_entries as i32 + change.num_sub_entries - change.signers;
                if num_signers != account.signers.len() as i32 {
                    let other_sub_entries =
                        account.num_sub_entries as i32 - account.signers.len() as i32;
                    return Err(format!(
                        "Deleted Account {:?} has {} subentries other than signers",
                        account.account_id, other_sub_entries
                    ));
                }
            }
        }

        Ok(())
    }
}

impl SubEntriesChange {
    fn clone_counts(&self) -> SubEntriesChange {
        SubEntriesChange {
            num_sub_entries: self.num_sub_entries,
            signers: self.signers,
            calculated_sub_entries: self.calculated_sub_entries,
        }
    }
}
