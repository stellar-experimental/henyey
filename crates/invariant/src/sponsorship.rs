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

/// Per-entry sponsorship contributions, as unsigned magnitudes.
///
/// The sign (+1 for entries that come into existence / post-states, -1 for
/// entries that go away / pre-states) is applied by the caller via [`merge`],
/// which keeps this a pure query over the entry and moves the reverse-direction
/// arithmetic to the call site (mirroring stellar-core's `updateCounters` with
/// the `sign` factor lifted out).
///
/// `sponsoring` is keyed by the **sponsor** account; `sponsored` is keyed by the
/// **owner/sponsored** account. These are distinct keys, and a single entry can
/// contribute to multiple sponsor keys (its own `sponsoringID` plus one per
/// signer-sponsoring-ID), so a flat scalar pair cannot represent them — hence the
/// per-key contribution lists.
#[derive(Default)]
struct SponsorshipContributions {
    /// (sponsor account, unsigned magnitude) pairs to add to `num_sponsoring`.
    sponsoring: Vec<(AccountId, i64)>,
    /// (owner account, unsigned magnitude) pairs to add to `num_sponsored`.
    sponsored: Vec<(AccountId, i64)>,
    /// Unsigned magnitude to add to the claimable-balance reserve accumulator.
    claimable_balance_reserve: i64,
}

/// Compute the sponsorship contributions for a single entry (sign-free).
fn sponsorship_contributions(entry: &LedgerEntry) -> SponsorshipContributions {
    let mut contribs = SponsorshipContributions::default();

    // Check for sponsoring extension on the entry itself.
    if let LedgerEntryExt::V1(v1) = &entry.ext {
        if let Some(ref sponsor) = v1.sponsoring_id.0 {
            let mult = get_mult(entry);
            contribs.sponsoring.push((sponsor.clone(), mult));
            if !matches!(&entry.data, LedgerEntryData::ClaimableBalance(_)) {
                if let Some(account_id) = get_account_id(entry) {
                    contribs.sponsored.push((account_id.clone(), mult));
                }
            } else {
                contribs.claimable_balance_reserve += mult;
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
                            contribs.sponsoring.push((sponsor.clone(), 1));
                            contribs.sponsored.push((acc.account_id.clone(), 1));
                        }
                    }
                }
            }
        }
    }

    contribs
}

/// Apply signed contributions into a counter map.
///
/// `*dst.entry(k).or_default() += sign * v` for each contribution. HashMap `+=`
/// accumulation is order-independent and `sign ∈ {+1, -1}` is pure multiplication,
/// so this is bit-for-bit identical to the old in-place `update_counters` math.
fn merge(dst: &mut HashMap<AccountId, i64>, contribs: &[(AccountId, i64)], sign: i64) {
    for (k, v) in contribs {
        *dst.entry(k.clone()).or_default() += sign * v;
    }
}

/// The change in an account's own `numSponsoring` / `numSponsored` counters,
/// read from its V2 extension. Both are zero when the entry is absent, is not an
/// account, or has no V2 extension.
#[derive(Default)]
struct AccountSponsorshipDelta {
    sponsoring: i64,
    sponsored: i64,
}

/// Read the `numSponsoring` / `numSponsored` counters from an account entry.
fn get_delta_sponsoring_and_sponsored(entry: Option<&LedgerEntry>) -> AccountSponsorshipDelta {
    let mut delta = AccountSponsorshipDelta::default();
    if let Some(entry) = entry {
        if let LedgerEntryData::Account(acc) = &entry.data {
            if has_account_ext_v2(acc) {
                if let AccountEntryExt::V1(v1) = &acc.ext {
                    if let AccountEntryExtensionV1Ext::V2(v2) = &v1.ext {
                        delta.sponsoring = v2.num_sponsoring as i64;
                        delta.sponsored = v2.num_sponsored as i64;
                    }
                }
            }
        }
    }
    delta
}

impl Invariant for SponsorshipCountIsValid {
    fn name(&self) -> &str {
        "SponsorshipCountIsValid"
    }

    fn is_strict(&self) -> bool {
        false
    }

    // `_claimable_balance_reserve` is a write-only accumulator (see below); the
    // `#[allow]` covers the dead-store lint on its per-iteration assignments,
    // which the previous `&mut` out-param form incidentally hid.
    #[allow(unused_assignments)]
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
        // Write-only accumulator, mirroring stellar-core's `claimableBalanceReserve`
        // in `SponsorshipCountIsValid::checkOnOperationApply`: it is accumulated but
        // never read back (the invariant never checks it). Kept for parity so the
        // claimable-balance branch of `sponsorship_contributions` has a faithful sink
        // and does NOT leak into `num_sponsored`.
        let mut _claimable_balance_reserve: i64 = 0;

        // Process created entries.
        for entry in delta.created {
            let c = sponsorship_contributions(entry);
            merge(&mut num_sponsoring, &c.sponsoring, 1);
            merge(&mut num_sponsored, &c.sponsored, 1);
            _claimable_balance_reserve += c.claimable_balance_reserve;
        }

        // Process updated entries (current - previous).
        for (current, previous) in delta.updated.iter().zip(delta.update_states.iter()) {
            let c = sponsorship_contributions(current);
            merge(&mut num_sponsoring, &c.sponsoring, 1);
            merge(&mut num_sponsored, &c.sponsored, 1);
            _claimable_balance_reserve += c.claimable_balance_reserve;

            let p = sponsorship_contributions(previous);
            merge(&mut num_sponsoring, &p.sponsoring, -1);
            merge(&mut num_sponsored, &p.sponsored, -1);
            _claimable_balance_reserve -= p.claimable_balance_reserve;
        }

        // Process deleted entries.
        for entry in delta.delete_states {
            let c = sponsorship_contributions(entry);
            merge(&mut num_sponsoring, &c.sponsoring, -1);
            merge(&mut num_sponsored, &c.sponsored, -1);
            _claimable_balance_reserve -= c.claimable_balance_reserve;
        }

        // Check accounts that appear in the delta.
        // For each account entry in the delta, verify that the change in
        // num_sponsoring/num_sponsored matches the calculated change.

        // Collect account entries from updated (both current and previous).
        for (current, previous) in delta.updated.iter().zip(delta.update_states.iter()) {
            if let LedgerEntryData::Account(acc) = &current.data {
                let account_id = &acc.account_id;

                let current_delta = get_delta_sponsoring_and_sponsored(Some(current));
                let previous_delta = get_delta_sponsoring_and_sponsored(Some(previous));
                let delta_sponsoring = current_delta.sponsoring - previous_delta.sponsoring;
                let delta_sponsored = current_delta.sponsored - previous_delta.sponsored;

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

                let delta = get_delta_sponsoring_and_sponsored(Some(entry));
                let delta_sponsoring = delta.sponsoring;
                let delta_sponsored = delta.sponsored;

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

                let delta = get_delta_sponsoring_and_sponsored(Some(entry));
                let delta_sponsoring = -delta.sponsoring;
                let delta_sponsored = -delta.sponsored;

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

#[cfg(test)]
mod tests {
    //! Characterization guard test for the sponsorship-count invariant.
    //!
    //! These tests pin the *current* (and post-refactor identical) behavior of
    //! `SponsorshipCountIsValid` so a future subtle sign/merge regression in
    //! `sponsorship_contributions` / `merge` / `get_delta_sponsoring_and_sponsored`
    //! is caught. They are NOT regression tests for a bug — they pass on the
    //! pre-refactor code too. They cover, per the converged plan for #3361:
    //!   - created entry (sign +1) routing into num_sponsoring / num_sponsored,
    //!   - the reverse-direction updated path (current +1 / previous -1),
    //!   - signer-sponsoring-ID contributions (magnitude 1),
    //!   - claimable-balance reserve routing (magnitude must NOT leak into
    //!     num_sponsored),
    //!   - consistent delta -> Ok, inconsistent (V2 counter off by 1) -> Err with
    //!     the numSponsoring / numSponsored mismatch message,
    //!   - the trailing unmatched-residual `count != 0` error branch.

    use super::*;
    use stellar_xdr::curr::{
        AccountEntryExtensionV1, AccountEntryExtensionV2, AccountEntryExtensionV2Ext, AlphaNum4,
        Asset, AssetCode4, ClaimPredicate, ClaimableBalanceEntry, ClaimableBalanceEntryExt,
        ClaimableBalanceId, Claimant, ClaimantV0, Hash, InflationResult, LedgerEntryExtensionV1,
        LedgerEntryExtensionV1Ext, Liabilities, OperationBody, OperationResultTr, PublicKey,
        SequenceNumber, SponsorshipDescriptor, String32, Thresholds, TrustLineEntry,
        TrustLineEntryExt, Uint256,
    };

    fn account_id(seed: u8) -> AccountId {
        AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([seed; 32])))
    }

    fn dummy_operation() -> Operation {
        Operation {
            source_account: None,
            body: OperationBody::Inflation,
        }
    }

    fn dummy_op_result() -> OperationResult {
        OperationResult::OpInner(OperationResultTr::Inflation(InflationResult::NotTime))
    }

    fn network_id() -> [u8; 32] {
        [0u8; 32]
    }

    /// Build an account entry with an optional V2 ext (numSponsoring/numSponsored
    /// counters plus signer-sponsoring-ID descriptors).
    fn account_entry(
        id: u8,
        num_sub_entries: u32,
        v2: Option<(u32, u32, Vec<Option<AccountId>>)>,
    ) -> LedgerEntry {
        let ext = match v2 {
            None => AccountEntryExt::V0,
            Some((num_sponsoring, num_sponsored, signers)) => {
                let signer_sponsoring_i_ds: VecMOrVec = signers
                    .into_iter()
                    .map(SponsorshipDescriptor)
                    .collect::<Vec<_>>()
                    .try_into()
                    .unwrap();
                AccountEntryExt::V1(AccountEntryExtensionV1 {
                    liabilities: Liabilities {
                        buying: 0,
                        selling: 0,
                    },
                    ext: AccountEntryExtensionV1Ext::V2(AccountEntryExtensionV2 {
                        num_sponsored,
                        num_sponsoring,
                        signer_sponsoring_i_ds,
                        ext: AccountEntryExtensionV2Ext::V0,
                    }),
                })
            }
        };
        LedgerEntry {
            last_modified_ledger_seq: 1,
            data: LedgerEntryData::Account(AccountEntry {
                account_id: account_id(id),
                balance: 0,
                seq_num: SequenceNumber(0),
                num_sub_entries,
                inflation_dest: None,
                flags: 0,
                home_domain: String32::default(),
                thresholds: Thresholds([0; 4]),
                signers: vec![].try_into().unwrap(),
                ext,
            }),
            ext: LedgerEntryExt::V0,
        }
    }

    /// Build a (non-pool-share) trustline entry, optionally sponsored by `sponsor`.
    fn trustline_entry(owner: u8, sponsor: Option<AccountId>) -> LedgerEntry {
        let ext = match sponsor {
            None => LedgerEntryExt::V0,
            Some(s) => LedgerEntryExt::V1(LedgerEntryExtensionV1 {
                sponsoring_id: SponsorshipDescriptor(Some(s)),
                ext: LedgerEntryExtensionV1Ext::V0,
            }),
        };
        LedgerEntry {
            last_modified_ledger_seq: 1,
            data: LedgerEntryData::Trustline(TrustLineEntry {
                account_id: account_id(owner),
                asset: TrustLineAsset::CreditAlphanum4(AlphaNum4 {
                    asset_code: AssetCode4([b'A', b'B', b'C', 0]),
                    issuer: account_id(99),
                }),
                balance: 0,
                limit: 0,
                flags: 0,
                ext: TrustLineEntryExt::V0,
            }),
            ext,
        }
    }

    /// Build a claimable-balance entry with `n` claimants, sponsored by `sponsor`.
    fn claimable_balance_entry(n: usize, sponsor: AccountId) -> LedgerEntry {
        let claimants: Vec<Claimant> = (0..n)
            .map(|i| {
                Claimant::ClaimantTypeV0(ClaimantV0 {
                    destination: account_id(50 + i as u8),
                    predicate: ClaimPredicate::Unconditional,
                })
            })
            .collect();
        LedgerEntry {
            last_modified_ledger_seq: 1,
            data: LedgerEntryData::ClaimableBalance(ClaimableBalanceEntry {
                balance_id: ClaimableBalanceId::ClaimableBalanceIdTypeV0(Hash([7; 32])),
                claimants: claimants.try_into().unwrap(),
                asset: Asset::Native,
                amount: 0,
                ext: ClaimableBalanceEntryExt::V0,
            }),
            ext: LedgerEntryExt::V1(LedgerEntryExtensionV1 {
                sponsoring_id: SponsorshipDescriptor(Some(sponsor)),
                ext: LedgerEntryExtensionV1Ext::V0,
            }),
        }
    }

    type VecMOrVec = stellar_xdr::curr::VecM<SponsorshipDescriptor, 20>;

    fn run(
        created: &[LedgerEntry],
        updated: &[LedgerEntry],
        update_states: &[LedgerEntry],
        delete_states: &[LedgerEntry],
    ) -> Result<(), String> {
        let nid = network_id();
        let delta = OperationDelta {
            created,
            updated,
            update_states,
            deleted: &[],
            delete_states,
            ledger_seq: 100,
            ledger_version: 24,
            header_current: None,
            header_previous: None,
            network_id: &nid,
        };
        SponsorshipCountIsValid.check_on_operation_apply(
            &dummy_operation(),
            &dummy_op_result(),
            &delta,
            &[],
        )
    }

    /// Account B sponsors a newly-created trustline owned by A; A's V2 ext gains
    /// numSponsored=1, B's V2 ext gains numSponsoring=1. Consistent => Ok.
    /// Then flip B's numSponsoring to 0 (off by 1) => Err with the numSponsoring
    /// mismatch message.
    #[test]
    fn test_sponsorship_delta_signs() {
        let sponsor = account_id(2); // B
        let owner_a = 1u8; // A

        // --- Consistent: created sponsored trustline + both accounts created ---
        // A created with numSponsored = 1; B created with numSponsoring = 1.
        let tl = trustline_entry(owner_a, Some(sponsor.clone()));
        let acct_a = account_entry(owner_a, 1, Some((0, 1, vec![]))); // numSponsored=1
        let acct_b = account_entry(2, 0, Some((1, 0, vec![]))); // numSponsoring=1
        let created = vec![tl.clone(), acct_a.clone(), acct_b.clone()];
        assert!(
            run(&created, &[], &[], &[]).is_ok(),
            "consistent created sponsored trustline should pass"
        );

        // --- Inconsistent: B's numSponsoring off by 1 (0 instead of 1) ---
        let acct_b_bad = account_entry(2, 0, Some((0, 0, vec![])));
        let created_bad = vec![tl.clone(), acct_a.clone(), acct_b_bad];
        let err = run(&created_bad, &[], &[], &[]).unwrap_err();
        assert!(
            err.contains("numSponsoring"),
            "expected numSponsoring mismatch, got: {err}"
        );

        // --- Reverse-direction (updated): trustline sponsorship REMOVED ---
        // previous state had A sponsored (numSponsored=1) by B (numSponsoring=1);
        // current state drops the sponsorship. The current-minus-previous delta is
        // -1 for both. Build updated current = unsponsored A/B/TL, previous =
        // sponsored. Consistent => Ok.
        let tl_prev = trustline_entry(owner_a, Some(sponsor.clone()));
        let tl_cur = trustline_entry(owner_a, None);
        let a_prev = account_entry(owner_a, 1, Some((0, 1, vec![])));
        let a_cur = account_entry(owner_a, 1, Some((0, 0, vec![])));
        let b_prev = account_entry(2, 0, Some((1, 0, vec![])));
        let b_cur = account_entry(2, 0, Some((0, 0, vec![])));
        let updated = vec![tl_cur, a_cur, b_cur];
        let update_states = vec![tl_prev, a_prev, b_prev];
        assert!(
            run(&[], &updated, &update_states, &[]).is_ok(),
            "consistent reverse-direction sponsorship removal should pass"
        );

        // --- Signer-sponsoring-ID contribution (magnitude 1) ---
        // A created with one signer sponsored by B: A.numSponsored=1, B.numSponsoring=1.
        let a_signer = account_entry(owner_a, 0, Some((0, 1, vec![Some(sponsor.clone())])));
        let b_signer = account_entry(2, 0, Some((1, 0, vec![])));
        assert!(
            run(&[a_signer, b_signer], &[], &[], &[]).is_ok(),
            "consistent signer-sponsoring contribution should pass"
        );

        // --- Claimable-balance reserve routing: must NOT leak into num_sponsored ---
        // A CB with 3 claimants sponsored by B contributes mult=3 to B's
        // numSponsoring and routes 3 to the (unchecked) claimable_balance_reserve,
        // NOT to num_sponsored. So if B (created with numSponsoring=3) is the only
        // account in the delta, the check passes — proving the CB magnitude did not
        // create a num_sponsored entry for any account.
        let cb = claimable_balance_entry(3, sponsor.clone());
        let b_cb = account_entry(2, 0, Some((3, 0, vec![])));
        assert!(
            run(&[cb.clone(), b_cb], &[], &[], &[]).is_ok(),
            "claimable-balance reserve must route to numSponsoring only, not numSponsored"
        );

        // --- Unmatched residual branch ---
        // Sponsored trustline owned by A, sponsor B, but neither A nor B appears as
        // its own account entry in the delta. The accumulated numSponsoring[B]=1 and
        // numSponsored[A]=1 are never erased => trailing `count != 0` error fires.
        let lone_tl = trustline_entry(owner_a, Some(sponsor.clone()));
        let err = run(&[lone_tl], &[], &[], &[]).unwrap_err();
        assert!(
            err.contains("numSponsoring") || err.contains("numSponsored"),
            "expected unmatched-residual mismatch, got: {err}"
        );
    }
}
