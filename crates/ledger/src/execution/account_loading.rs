//! Operation account loading.
//!
//! Pre-loads accounts, trustlines, and other entries needed by each operation
//! type before execution. Extracted from the main executor module for readability.

use stellar_xdr::curr::{
    AccountId, LedgerKey, LedgerKeyClaimableBalance, LedgerKeyLiquidityPool, LedgerKeyTrustLine,
    Limits, OperationBody, PoolId, TrustLineAsset, WriteXdr,
};

use crate::snapshot::SnapshotHandle;
use crate::{LedgerError, Result};

use super::meta::{
    allow_trust_asset, asset_issuer_id, asset_to_trustline_asset, make_account_key,
    make_trustline_key,
};
use super::TransactionExecutor;

impl TransactionExecutor {
    /// Load accounts needed for an operation.
    pub(super) fn load_operation_accounts(
        &mut self,
        snapshot: &SnapshotHandle,
        op: &stellar_xdr::curr::Operation,
        source_id: &AccountId,
    ) -> Result<()> {
        let op_source = op
            .source_account
            .as_ref()
            .map(henyey_tx::muxed_to_account_id)
            .unwrap_or_else(|| source_id.clone());

        // Load operation source if different from transaction source
        if let Some(ref muxed) = op.source_account {
            let op_source = henyey_tx::muxed_to_account_id(muxed);
            self.load_account(snapshot, &op_source)?;
        }

        // Phase 1: Batch-load statically-known keys (shared with per-ledger prefetch).
        // When the prefetch cache is populated, these lookups are cache hits.
        // When called without prefetch (e.g., in tests), this provides the
        // same batch-loading benefit as the per-ledger prefetch.
        {
            let mut static_keys = std::collections::HashSet::new();
            henyey_tx::collect_prefetch_keys(&op.body, &op_source, &mut static_keys);
            if !static_keys.is_empty() {
                let keys_vec: Vec<LedgerKey> = static_keys.into_iter().collect();
                self.batch_load_keys(snapshot, &keys_vec)?;
            }
        }

        // Phase 2: Conditional/secondary loading that depends on loaded state
        // or requires special semantics (e.g., load_account_without_record).
        match &op.body {
            OperationBody::CreateAccount(op_data) => {
                self.load_account(snapshot, &op_data.destination)?;
            }
            OperationBody::BeginSponsoringFutureReserves(op_data) => {
                self.load_account(snapshot, &op_data.sponsored_id)?;
            }
            OperationBody::AllowTrust(op_data) => {
                let asset = allow_trust_asset(op_data, &op_source);
                let mut keys = vec![make_account_key(&op_data.trustor)];
                if let Some(tl_asset) = asset_to_trustline_asset(&asset) {
                    keys.push(make_trustline_key(&op_data.trustor, &tl_asset));
                }
                self.batch_load_keys(snapshot, &keys)?;
                // Load offers by account/asset so they can be removed if authorization is revoked
                self.load_offers_by_account_and_asset(snapshot, &op_data.trustor, &asset)?;
                // Load pool share trustlines so they can be redeemed if authorization is revoked
                self.load_pool_share_trustlines_for_account_and_asset(
                    snapshot,
                    &op_data.trustor,
                    &asset,
                )?;
            }
            OperationBody::Payment(op_data) => {
                let dest = henyey_tx::muxed_to_account_id(&op_data.destination);
                let mut keys = vec![make_account_key(&dest)];
                if let Some(tl_asset) = asset_to_trustline_asset(&op_data.asset) {
                    keys.push(make_trustline_key(&op_source, &tl_asset));
                    keys.push(make_trustline_key(&dest, &tl_asset));
                }
                if let Some(issuer) = asset_issuer_id(&op_data.asset) {
                    keys.push(make_account_key(&issuer));
                }
                self.batch_load_keys(snapshot, &keys)?;
            }
            OperationBody::AccountMerge(dest) => {
                let dest = henyey_tx::muxed_to_account_id(dest);
                self.load_account(snapshot, &dest)?;
            }
            OperationBody::ClaimClaimableBalance(op_data) => {
                self.load_claimable_balance(snapshot, &op_data.balance_id)?;
                let key = LedgerKey::ClaimableBalance(LedgerKeyClaimableBalance {
                    balance_id: op_data.balance_id.clone(),
                });
                if let Some(sponsor) = self.state.entry_sponsor(&key) {
                    self.load_account(snapshot, &sponsor)?;
                }
                if let Some(entry) = self.state.get_claimable_balance(&op_data.balance_id) {
                    let asset = entry.asset.clone();
                    if let Some(tl_asset) = asset_to_trustline_asset(&asset) {
                        self.load_trustline(snapshot, &op_source, &tl_asset)?;
                        self.load_asset_issuer(snapshot, &asset)?;
                    }
                }
            }
            OperationBody::ClawbackClaimableBalance(op_data) => {
                self.load_claimable_balance(snapshot, &op_data.balance_id)?;
                let key = LedgerKey::ClaimableBalance(LedgerKeyClaimableBalance {
                    balance_id: op_data.balance_id.clone(),
                });
                if let Some(sponsor) = self.state.entry_sponsor(&key) {
                    self.load_account(snapshot, &sponsor)?;
                }
            }
            OperationBody::CreateClaimableBalance(op_data) => {
                if let Some(tl_asset) = asset_to_trustline_asset(&op_data.asset) {
                    self.load_trustline(snapshot, &op_source, &tl_asset)?;
                }
            }
            OperationBody::SetTrustLineFlags(op_data) => {
                let mut keys = vec![make_account_key(&op_data.trustor)];
                if let Some(tl_asset) = asset_to_trustline_asset(&op_data.asset) {
                    keys.push(make_trustline_key(&op_data.trustor, &tl_asset));
                }
                self.batch_load_keys(snapshot, &keys)?;
                // Load offers by account/asset so they can be removed if authorization is revoked
                self.load_offers_by_account_and_asset(snapshot, &op_data.trustor, &op_data.asset)?;
                // Load pool share trustlines so they can be redeemed if authorization is revoked
                self.load_pool_share_trustlines_for_account_and_asset(
                    snapshot,
                    &op_data.trustor,
                    &op_data.asset,
                )?;
            }
            OperationBody::Clawback(op_data) => {
                let from_account = henyey_tx::muxed_to_account_id(&op_data.from);
                if let Some(tl_asset) = asset_to_trustline_asset(&op_data.asset) {
                    self.load_trustline(snapshot, &from_account, &tl_asset)?;
                }
            }
            OperationBody::ManageSellOffer(op_data) => {
                let mut keys = Vec::new();
                for asset in [&op_data.selling, &op_data.buying] {
                    if let Some(tl_asset) = asset_to_trustline_asset(asset) {
                        keys.push(make_trustline_key(&op_source, &tl_asset));
                    }
                    if let Some(issuer) = asset_issuer_id(asset) {
                        keys.push(make_account_key(&issuer));
                    }
                }
                if op_data.offer_id != 0 {
                    keys.push(LedgerKey::Offer(stellar_xdr::curr::LedgerKeyOffer {
                        seller_id: op_source.clone(),
                        offer_id: op_data.offer_id,
                    }));
                }
                self.batch_load_keys(snapshot, &keys)?;
                if op_data.offer_id != 0 {
                    self.load_offer_sponsor(snapshot, &op_source, op_data.offer_id)?;
                }
            }
            OperationBody::CreatePassiveSellOffer(op_data) => {
                let mut keys = Vec::new();
                for asset in [&op_data.selling, &op_data.buying] {
                    if let Some(tl_asset) = asset_to_trustline_asset(asset) {
                        keys.push(make_trustline_key(&op_source, &tl_asset));
                    }
                    if let Some(issuer) = asset_issuer_id(asset) {
                        keys.push(make_account_key(&issuer));
                    }
                }
                self.batch_load_keys(snapshot, &keys)?;
            }
            OperationBody::ManageBuyOffer(op_data) => {
                let mut keys = Vec::new();
                for asset in [&op_data.selling, &op_data.buying] {
                    if let Some(tl_asset) = asset_to_trustline_asset(asset) {
                        keys.push(make_trustline_key(&op_source, &tl_asset));
                    }
                    if let Some(issuer) = asset_issuer_id(asset) {
                        keys.push(make_account_key(&issuer));
                    }
                }
                if op_data.offer_id != 0 {
                    keys.push(LedgerKey::Offer(stellar_xdr::curr::LedgerKeyOffer {
                        seller_id: op_source.clone(),
                        offer_id: op_data.offer_id,
                    }));
                }
                self.batch_load_keys(snapshot, &keys)?;
                if op_data.offer_id != 0 {
                    self.load_offer_sponsor(snapshot, &op_source, op_data.offer_id)?;
                }
            }
            OperationBody::PathPaymentStrictSend(op_data) => {
                let dest = henyey_tx::muxed_to_account_id(&op_data.destination);
                let mut keys = vec![make_account_key(&dest)];
                if let Some(tl_asset) = asset_to_trustline_asset(&op_data.send_asset) {
                    keys.push(make_trustline_key(&op_source, &tl_asset));
                }
                if let Some(tl_asset) = asset_to_trustline_asset(&op_data.dest_asset) {
                    keys.push(make_trustline_key(&dest, &tl_asset));
                }
                if let Some(issuer) = asset_issuer_id(&op_data.send_asset) {
                    keys.push(make_account_key(&issuer));
                }
                if let Some(issuer) = asset_issuer_id(&op_data.dest_asset) {
                    keys.push(make_account_key(&issuer));
                }
                self.batch_load_keys(snapshot, &keys)?;
                self.load_path_payment_pools(
                    snapshot,
                    &op_data.send_asset,
                    &op_data.dest_asset,
                    op_data.path.as_slice(),
                )?;
            }
            OperationBody::PathPaymentStrictReceive(op_data) => {
                let dest = henyey_tx::muxed_to_account_id(&op_data.destination);
                let mut keys = vec![make_account_key(&dest)];
                if let Some(tl_asset) = asset_to_trustline_asset(&op_data.send_asset) {
                    keys.push(make_trustline_key(&op_source, &tl_asset));
                }
                if let Some(tl_asset) = asset_to_trustline_asset(&op_data.dest_asset) {
                    keys.push(make_trustline_key(&dest, &tl_asset));
                }
                if let Some(issuer) = asset_issuer_id(&op_data.send_asset) {
                    keys.push(make_account_key(&issuer));
                }
                if let Some(issuer) = asset_issuer_id(&op_data.dest_asset) {
                    keys.push(make_account_key(&issuer));
                }
                self.batch_load_keys(snapshot, &keys)?;
                self.load_path_payment_pools(
                    snapshot,
                    &op_data.send_asset,
                    &op_data.dest_asset,
                    op_data.path.as_slice(),
                )?;
            }
            OperationBody::LiquidityPoolDeposit(op_data) => {
                self.load_liquidity_pool_dependencies(
                    snapshot,
                    &op_source,
                    &op_data.liquidity_pool_id,
                )?;
            }
            OperationBody::LiquidityPoolWithdraw(op_data) => {
                self.load_liquidity_pool_dependencies(
                    snapshot,
                    &op_source,
                    &op_data.liquidity_pool_id,
                )?;
            }
            OperationBody::ChangeTrust(op_data) => {
                // Load existing trustline if any
                let tl_asset = match &op_data.line {
                    stellar_xdr::curr::ChangeTrustAsset::Native => None,
                    stellar_xdr::curr::ChangeTrustAsset::CreditAlphanum4(a) => {
                        Some(TrustLineAsset::CreditAlphanum4(a.clone()))
                    }
                    stellar_xdr::curr::ChangeTrustAsset::CreditAlphanum12(a) => {
                        Some(TrustLineAsset::CreditAlphanum12(a.clone()))
                    }
                    stellar_xdr::curr::ChangeTrustAsset::PoolShare(params) => {
                        // Compute pool ID from params
                        use sha2::{Digest, Sha256};
                        let xdr = params
                            .to_xdr(Limits::none())
                            .map_err(|e| LedgerError::Serialization(e.to_string()))?;
                        let pool_id = PoolId(stellar_xdr::curr::Hash(Sha256::digest(&xdr).into()));
                        Some(TrustLineAsset::PoolShare(pool_id))
                    }
                };
                if let Some(ref tl_asset) = tl_asset {
                    self.load_trustline(snapshot, &op_source, tl_asset)?;
                    // If deleting a trustline (limit=0), load the sponsor account if it has one.
                    // The sponsor's num_sponsoring needs to be decremented.
                    if op_data.limit == 0 {
                        let tl_key = LedgerKey::Trustline(LedgerKeyTrustLine {
                            account_id: op_source.clone(),
                            asset: tl_asset.clone(),
                        });
                        if let Some(sponsor) = self.state.entry_sponsor(&tl_key) {
                            self.load_account(snapshot, &sponsor)?;
                        }
                    }
                }
                // Load issuer account for non-pool-share assets WITHOUT recording.
                // stellar-core uses loadAccountWithoutRecord() for ChangeTrust issuer check
                // which doesn't record the access in transaction changes.
                // We still need to load the account into state so the existence check works.
                match &op_data.line {
                    stellar_xdr::curr::ChangeTrustAsset::CreditAlphanum4(a) => {
                        let asset_code = String::from_utf8_lossy(a.asset_code.as_slice());
                        tracing::debug!(
                            asset_code = %asset_code,
                            issuer = ?a.issuer,
                            "ChangeTrust: loading issuer for CreditAlphanum4 (without record)"
                        );
                        self.load_account_without_record(snapshot, &a.issuer)?;
                    }
                    stellar_xdr::curr::ChangeTrustAsset::CreditAlphanum12(a) => {
                        let asset_code = String::from_utf8_lossy(a.asset_code.as_slice());
                        tracing::debug!(
                            asset_code = %asset_code,
                            issuer = ?a.issuer,
                            "ChangeTrust: loading issuer for CreditAlphanum12 (without record)"
                        );
                        self.load_account_without_record(snapshot, &a.issuer)?;
                    }
                    stellar_xdr::curr::ChangeTrustAsset::PoolShare(params) => {
                        use sha2::{Digest, Sha256};
                        let xdr = params
                            .to_xdr(Limits::none())
                            .map_err(|e| LedgerError::Serialization(e.to_string()))?;
                        let pool_id = PoolId(stellar_xdr::curr::Hash(Sha256::digest(&xdr).into()));
                        let stellar_xdr::curr::LiquidityPoolParameters::LiquidityPoolConstantProduct(cp) = params;
                        let mut keys = vec![LedgerKey::LiquidityPool(LedgerKeyLiquidityPool {
                            liquidity_pool_id: pool_id,
                        })];
                        if let Some(tl_asset) = asset_to_trustline_asset(&cp.asset_a) {
                            keys.push(make_trustline_key(&op_source, &tl_asset));
                        }
                        if let Some(tl_asset) = asset_to_trustline_asset(&cp.asset_b) {
                            keys.push(make_trustline_key(&op_source, &tl_asset));
                        }
                        self.batch_load_keys(snapshot, &keys)?;
                    }
                    _ => {}
                }
            }
            OperationBody::ManageData(op_data) => {
                // Load existing data entry if any (needed for updates and deletes)
                self.load_data_raw(snapshot, &op_source, &op_data.data_name)?;
            }
            OperationBody::RevokeSponsorship(op_data) => {
                // Load the target entry that sponsorship is being revoked from
                use stellar_xdr::curr::RevokeSponsorshipOp;
                match op_data {
                    RevokeSponsorshipOp::LedgerEntry(ledger_key) => {
                        // Load the entry directly by its key
                        self.load_entry(snapshot, ledger_key)?;
                        self.state.record_entry_access(ledger_key);
                        // Also load owner/sponsor accounts that may be modified
                        match ledger_key {
                            LedgerKey::Account(k) => {
                                self.load_account(snapshot, &k.account_id)?;
                            }
                            LedgerKey::Trustline(k) => {
                                self.load_account(snapshot, &k.account_id)?;
                            }
                            LedgerKey::Offer(k) => {
                                self.load_account(snapshot, &k.seller_id)?;
                            }
                            LedgerKey::Data(k) => {
                                self.load_account(snapshot, &k.account_id)?;
                            }
                            LedgerKey::ClaimableBalance(k) => {
                                // Load the claimable balance and its sponsor
                                self.load_claimable_balance(snapshot, &k.balance_id)?;
                            }
                            _ => {}
                        }
                    }
                    RevokeSponsorshipOp::Signer(signer_key) => {
                        // Load the account that has the signer
                        self.load_account(snapshot, &signer_key.account_id)?;
                    }
                }
            }
            OperationBody::SetOptions(op_data) => {
                // If SetOptions sets an inflation_dest that differs from the source,
                // we need to load that account to validate it exists.
                // This matches stellar-core's loadAccountWithoutRecord() call.
                if let Some(ref inflation_dest) = op_data.inflation_dest {
                    if inflation_dest != &op_source {
                        self.load_account_without_record(snapshot, inflation_dest)?;
                    }
                }

                // If SetOptions modifies signers and the source account has sponsored signers,
                // we need to load those sponsor accounts so we can update their num_sponsoring.
                if op_data.signer.is_some() {
                    // Collect sponsor IDs from paired signer slots. Strict
                    // validation prevents loading sponsors for stale
                    // descriptors that do not correspond to real signers.
                    let sponsor_ids: Vec<AccountId> = self
                        .state
                        .get_account(&op_source)
                        .map(henyey_tx::state::signers::strict_sponsored_signers)
                        .transpose()
                        .map_err(|e| LedgerError::Internal(e.to_string()))?
                        .unwrap_or_default();

                    // Load each sponsor account
                    for sponsor_id in &sponsor_ids {
                        self.load_account(snapshot, sponsor_id)?;
                    }
                }
            }
            _ => {
                // Other operations typically work on source account
            }
        }

        Ok(())
    }
}
