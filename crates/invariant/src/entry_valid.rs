//! LedgerEntryIsValid invariant.
//!
//! Validates field constraints on all created and updated ledger entries.
//!
//! # Parity
//!
//! stellar-core: `src/invariant/LedgerEntryIsValid.cpp`
//! Strictness: non-strict (`Invariant(false)`)
//!
//! This is a partial port covering the most impactful checks. Full parity
//! (all Soroban entry forms, asset contract validation, etc.) is tracked
//! as follow-up work.

use stellar_xdr::curr::{
    AccountEntry, AccountEntryExt, AccountEntryExtensionV1Ext, ContractEvent, DataEntry,
    LedgerEntry, LedgerEntryData, OfferEntry, Operation, OperationResult, TrustLineAsset,
    TrustLineEntry, TrustLineEntryExt, TrustLineEntryV1Ext,
};

use crate::{Invariant, OperationDelta};

pub struct LedgerEntryIsValid;

impl LedgerEntryIsValid {
    pub fn new() -> Self {
        Self
    }
}

impl Invariant for LedgerEntryIsValid {
    fn name(&self) -> &str {
        "LedgerEntryIsValid"
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
        let curr_ledger_seq = delta.ledger_seq;

        if curr_ledger_seq > i32::MAX as u32 {
            return Err(format!(
                "LedgerHeader ledgerSeq ({}) exceeds limits ({})",
                curr_ledger_seq,
                i32::MAX
            ));
        }

        // Check all created entries.
        for entry in delta.created {
            check_entry(entry, None, curr_ledger_seq)?;
        }

        // Check all updated entries (current + previous pair).
        for (current, previous) in delta.updated.iter().zip(delta.update_states.iter()) {
            check_entry(current, Some(previous), curr_ledger_seq)?;
        }

        Ok(())
    }
}

fn check_entry(
    entry: &LedgerEntry,
    previous: Option<&LedgerEntry>,
    ledger_seq: u32,
) -> Result<(), String> {
    // lastModifiedLedgerSeq must equal current ledger seq.
    if entry.last_modified_ledger_seq != ledger_seq {
        return Err(format!(
            "LedgerEntry lastModifiedLedgerSeq ({}) does not equal LedgerHeader ledgerSeq ({})",
            entry.last_modified_ledger_seq, ledger_seq
        ));
    }

    match &entry.data {
        LedgerEntryData::Account(acc) => check_account(acc),
        LedgerEntryData::Trustline(tl) => check_trustline(tl, previous),
        LedgerEntryData::Offer(offer) => check_offer(offer),
        LedgerEntryData::Data(data) => check_data(data),
        LedgerEntryData::ClaimableBalance(_) => {
            // ClaimableBalance must be sponsored.
            match &entry.ext {
                stellar_xdr::curr::LedgerEntryExt::V1(v1) => {
                    if v1.sponsoring_id.0.is_none() {
                        return Err("ClaimableBalance is not sponsored".to_string());
                    }
                }
                _ => {
                    return Err("ClaimableBalance is not sponsored".to_string());
                }
            }
            Ok(())
        }
        LedgerEntryData::LiquidityPool(_) => {
            // LiquidityPool must not be sponsored.
            if !matches!(&entry.ext, stellar_xdr::curr::LedgerEntryExt::V0) {
                return Err("LiquidityPool is sponsored".to_string());
            }
            Ok(())
        }
        // Contract data, contract code, config setting, TTL — basic validity
        // checks are deferred to follow-up (full parity requires Soroban-specific
        // validation).
        _ => Ok(()),
    }
}

fn check_account(acc: &AccountEntry) -> Result<(), String> {
    if acc.balance < 0 {
        return Err(format!("Account balance ({}) is negative", acc.balance));
    }
    if acc.seq_num.0 < 0 {
        return Err(format!("Account seqNum ({}) is negative", acc.seq_num.0));
    }
    if acc.num_sub_entries > i32::MAX as u32 {
        return Err(format!(
            "Account numSubEntries ({}) exceeds limit ({})",
            acc.num_sub_entries,
            i32::MAX
        ));
    }

    // Signers must be strictly increasing (by key).
    let signers = acc.signers.as_slice();
    for i in 1..signers.len() {
        if signers[i - 1].key >= signers[i].key {
            return Err("Account signers are not strictly increasing".to_string());
        }
    }

    // Signer weights must be non-zero and <= 255 (protocol 10+, but henyey is P24+).
    for s in signers {
        if s.weight == 0 || s.weight > u8::MAX as u32 {
            return Err("Account signers have invalid weights".to_string());
        }
    }

    // Account v2 extension checks (protocol 14+, always applies for P24+).
    if let AccountEntryExt::V1(v1) = &acc.ext {
        if let AccountEntryExtensionV1Ext::V2(v2) = &v1.ext {
            if acc.signers.len() as u32 != v2.signer_sponsoring_i_ds.len() as u32 {
                return Err("Account signers not paired with signerSponsoringIDs".to_string());
            }
            // numSubEntries + numSponsoring must not overflow (protocol 18+).
            if acc.num_sub_entries > u32::MAX - v2.num_sponsoring {
                return Err("Account numSubEntries + numSponsoring is > UINT32_MAX".to_string());
            }
        }
    }

    Ok(())
}

fn check_trustline(tl: &TrustLineEntry, previous: Option<&LedgerEntry>) -> Result<(), String> {
    // TrustLine asset must not be native.
    if matches!(&tl.asset, TrustLineAsset::Native) {
        return Err("TrustLine asset is native".to_string());
    }

    // Pool share trustlines must not have liabilities.
    if matches!(&tl.asset, TrustLineAsset::PoolShare(_)) {
        if let TrustLineEntryExt::V1(v1) = &tl.ext {
            if v1.liabilities.buying != 0 || v1.liabilities.selling != 0 {
                return Err("Pool share TrustLine has liabilities".to_string());
            }
        }
    }

    // V2 extension (protocol 18+).
    if let TrustLineEntryExt::V1(v1) = &tl.ext {
        if let TrustLineEntryV1Ext::V2(v2) = &v1.ext {
            if v2.liquidity_pool_use_count < 0 {
                return Err("TrustLine liquidityPoolUseCount is negative".to_string());
            }
        }
    }

    if tl.balance < 0 {
        return Err(format!("TrustLine balance ({}) is negative", tl.balance));
    }
    if tl.limit <= 0 {
        return Err(format!("TrustLine limit ({}) is not positive", tl.limit));
    }
    if tl.balance > tl.limit {
        return Err(format!(
            "TrustLine balance ({}) exceeds limit ({})",
            tl.balance, tl.limit
        ));
    }

    // Clawback flag must not be enabled if it wasn't enabled before.
    if let Some(prev) = previous {
        if let LedgerEntryData::Trustline(prev_tl) = &prev.data {
            let prev_clawback = (prev_tl.flags & 0x4) != 0; // AUTHORIZED_TO_MAINTAIN_LIABILITIES clawback bit
            let cur_clawback = (tl.flags & 0x4) != 0;
            if !prev_clawback && cur_clawback {
                return Err("TrustLine clawback flag was enabled".to_string());
            }
        }
    }

    Ok(())
}

fn check_offer(offer: &OfferEntry) -> Result<(), String> {
    if offer.offer_id <= 0 {
        return Err(format!(
            "Offer offerID ({}) must be positive",
            offer.offer_id
        ));
    }
    if offer.amount <= 0 {
        return Err("Offer amount is not positive".to_string());
    }
    if offer.price.n <= 0 || offer.price.d < 1 {
        return Err(format!(
            "Offer price ({} / {}) is invalid",
            offer.price.n, offer.price.d
        ));
    }
    Ok(())
}

fn check_data(data: &DataEntry) -> Result<(), String> {
    if data.data_name.as_vec().is_empty() {
        return Err("Data dataName is empty".to_string());
    }
    Ok(())
}
