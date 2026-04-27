//! Sponsorship tracking and management for ledger entry reserves.

use super::signers::SignerSet;
use super::*;

impl LedgerStateManager {
    /// Clear active sponsorship state (start of a new transaction).
    pub fn clear_sponsorship_stack(&mut self) {
        self.sponsorship_stack.clear();
    }

    /// Check if there is any pending sponsorship.
    pub fn has_pending_sponsorship(&self) -> bool {
        !self.sponsorship_stack.is_empty()
    }

    /// Return the active sponsor for a sponsored account, if any.
    pub fn active_sponsor_for(&self, sponsored: &AccountId) -> Option<AccountId> {
        self.sponsorship_stack
            .iter()
            .rev()
            .find(|ctx| ctx.sponsored == *sponsored)
            .map(|ctx| ctx.sponsoring.clone())
    }

    /// Return true if an account is currently being sponsored.
    pub fn is_sponsored(&self, account_id: &AccountId) -> bool {
        self.sponsorship_stack
            .iter()
            .any(|ctx| ctx.sponsored == *account_id)
    }

    /// Return true if an account is currently sponsoring someone else.
    pub fn is_sponsoring(&self, account_id: &AccountId) -> bool {
        self.sponsorship_stack
            .iter()
            .any(|ctx| ctx.sponsoring == *account_id)
    }

    /// Push a new sponsorship context onto the stack.
    pub fn push_sponsorship(&mut self, sponsoring: AccountId, sponsored: AccountId) {
        self.sponsorship_stack.push(SponsorshipContext {
            sponsoring,
            sponsored,
        });
    }

    /// Pop the latest sponsorship context.
    pub fn pop_sponsorship(&mut self) -> Option<SponsorshipContext> {
        self.sponsorship_stack.pop()
    }

    /// Remove the most recent sponsorship for a sponsored account.
    pub fn remove_sponsorship_for(&mut self, sponsored: &AccountId) -> Option<SponsorshipContext> {
        if let Some(pos) = self
            .sponsorship_stack
            .iter()
            .rposition(|ctx| &ctx.sponsored == sponsored)
        {
            return Some(self.sponsorship_stack.remove(pos));
        }
        None
    }

    /// Return the sponsor for a ledger entry, if any.
    pub fn entry_sponsor(&self, key: &LedgerKey) -> Option<AccountId> {
        self.get_entry_sponsorship(key)
    }

    fn snapshot_entry_sponsorship_ext(&mut self, key: &LedgerKey) {
        if !self.entry_sponsorship_ext_snapshots.contains_key(key) {
            self.entry_sponsorship_ext_snapshots
                .insert(key.clone(), self.contains_sponsorship_ext(key));
        }
    }

    fn snapshot_entry_sponsorship_metadata(&mut self, key: &LedgerKey) {
        if !self.entry_sponsorship_snapshots.contains_key(key) {
            self.entry_sponsorship_snapshots
                .insert(key.clone(), self.get_entry_sponsorship(key));
        }
        self.snapshot_entry_sponsorship_ext(key);
    }

    pub(super) fn clear_entry_sponsorship_metadata(&mut self, key: &LedgerKey) {
        self.snapshot_entry_sponsorship_metadata(key);
        self.remove_entry_sponsorship(key);
        self.remove_sponsorship_ext(key);
    }

    /// Set the sponsor for a ledger entry.
    pub fn set_entry_sponsor(&mut self, key: LedgerKey, sponsor: AccountId) {
        self.snapshot_entry_sponsorship_metadata(&key);
        self.capture_op_snapshot_for_key(&key);
        self.insert_entry_sponsorship(key.clone(), sponsor);
        self.insert_sponsorship_ext(key);
    }

    /// Remove and return the sponsor for a ledger entry, if any.
    pub fn remove_entry_sponsor(&mut self, key: &LedgerKey) -> Option<AccountId> {
        self.snapshot_entry_sponsorship_metadata(key);
        self.capture_op_snapshot_for_key(key);
        self.insert_sponsorship_ext(key.clone());
        self.remove_entry_sponsorship(key)
    }

    /// Apply sponsorship to a newly created ledger entry owned by `sponsored`.
    pub fn apply_entry_sponsorship(
        &mut self,
        key: LedgerKey,
        sponsored: &AccountId,
        multiplier: i64,
    ) -> Result<Option<AccountId>> {
        let Some(sponsor) = self.active_sponsor_for(sponsored) else {
            return Ok(None);
        };
        self.apply_entry_sponsorship_with_sponsor(key, &sponsor, Some(sponsored), multiplier)?;
        Ok(Some(sponsor))
    }

    /// Apply sponsorship for a ledger entry with a known sponsor.
    pub fn apply_entry_sponsorship_with_sponsor(
        &mut self,
        key: LedgerKey,
        sponsor: &AccountId,
        sponsored: Option<&AccountId>,
        multiplier: i64,
    ) -> Result<()> {
        if multiplier < 0 {
            return Err(TxError::Internal(
                "negative sponsorship multiplier".to_string(),
            ));
        }
        self.set_entry_sponsor(key, sponsor.clone());
        self.update_num_sponsoring(sponsor, multiplier)?;
        if let Some(sponsored) = sponsored {
            self.update_num_sponsored(sponsored, multiplier)?;
        }
        Ok(())
    }

    /// Apply sponsorship to a newly created account entry (account not yet in state).
    pub fn apply_account_entry_sponsorship(
        &mut self,
        account: &mut AccountEntry,
        sponsor: &AccountId,
        multiplier: i64,
    ) -> Result<()> {
        if multiplier < 0 {
            return Err(TxError::Internal(
                "negative sponsorship multiplier".to_string(),
            ));
        }
        let ext = ensure_account_ext_v2(account);
        let updated = ext.num_sponsored as i64 + multiplier;
        if updated < 0 || updated > u32::MAX as i64 {
            return Err(TxError::TooManySponsoring);
        }
        ext.num_sponsored = updated as u32;
        self.update_num_sponsoring(sponsor, multiplier)?;
        Ok(())
    }

    /// Remove sponsorship for a ledger entry and update account counts.
    pub fn remove_entry_sponsorship_and_update_counts(
        &mut self,
        key: &LedgerKey,
        sponsored: &AccountId,
        multiplier: i64,
    ) -> Result<Option<AccountId>> {
        let Some(sponsor) = self.remove_entry_sponsor(key) else {
            return Ok(None);
        };
        if multiplier < 0 {
            return Err(TxError::Internal(
                "negative sponsorship multiplier".to_string(),
            ));
        }
        self.update_num_sponsoring(&sponsor, -multiplier)?;
        self.update_num_sponsored(sponsored, -multiplier)?;
        Ok(Some(sponsor))
    }

    /// Remove sponsorship for a ledger entry with optional sponsored account.
    pub fn remove_entry_sponsorship_with_sponsor_counts(
        &mut self,
        key: &LedgerKey,
        sponsored: Option<&AccountId>,
        multiplier: i64,
    ) -> Result<Option<AccountId>> {
        let Some(sponsor) = self.remove_entry_sponsor(key) else {
            return Ok(None);
        };
        if multiplier < 0 {
            return Err(TxError::Internal(
                "negative sponsorship multiplier".to_string(),
            ));
        }
        self.update_num_sponsoring(&sponsor, -multiplier)?;
        if let Some(sponsored) = sponsored {
            self.update_num_sponsored(sponsored, -multiplier)?;
        }
        Ok(Some(sponsor))
    }

    /// Update num_sponsoring for an account.
    ///
    /// Lazily loads the account from the bucket list if not already in state.
    /// This is necessary because sponsored entries may reference a sponsor
    /// account that hasn't been loaded yet (e.g., during offer crossing when
    /// a sponsored offer is fully consumed and deleted).
    pub fn update_num_sponsoring(&mut self, account_id: &AccountId, delta: i64) -> Result<()> {
        self.ensure_account_loaded(account_id)?;
        let account = self
            .get_account_mut(account_id)
            .ok_or(TxError::SourceAccountNotFound)?;
        let num_sub_entries = account.num_sub_entries as u64;
        let ext = ensure_account_ext_v2(account);
        let updated = ext.num_sponsoring as i64 + delta;
        if updated < 0 || updated > u32::MAX as i64 {
            return Err(TxError::TooManySponsoring);
        }
        // Combined cap: numSponsoring + numSubEntries must not exceed UINT32_MAX.
        // stellar-core: SponsorshipUtils.cpp:21-28 (isSponsoringSubentrySumIncreaseValid)
        if updated as u64 + num_sub_entries > u32::MAX as u64 {
            return Err(TxError::TooManySponsoring);
        }
        ext.num_sponsoring = updated as u32;
        Ok(())
    }

    /// Update num_sponsored for an account.
    ///
    /// Lazily loads the account from the bucket list if not already in state.
    pub fn update_num_sponsored(&mut self, account_id: &AccountId, delta: i64) -> Result<()> {
        self.ensure_account_loaded(account_id)?;
        let account = self
            .get_account_mut(account_id)
            .ok_or(TxError::SourceAccountNotFound)?;
        let ext = ensure_account_ext_v2(account);
        let updated = ext.num_sponsored as i64 + delta;
        if updated < 0 || updated > u32::MAX as i64 {
            return Err(TxError::TooManySponsoring);
        }
        ext.num_sponsored = updated as u32;
        Ok(())
    }

    /// Get sponsorship counts (num_sponsoring, num_sponsored) for an account.
    pub fn sponsorship_counts_for_account(&self, account_id: &AccountId) -> Option<(i64, i64)> {
        self.get_account(account_id).map(sponsorship_counts)
    }

    /// Remove a one-time (pre-auth TX) signer from all source accounts in a transaction.
    ///
    /// Pre-auth TX signers are automatically consumed when a transaction they
    /// authorized is applied. This method removes the signer from all accounts
    /// that participated in the transaction.
    ///
    /// # Arguments
    ///
    /// * `tx_hash` - The transaction hash (used to create the signer key)
    /// * `source_accounts` - All source account IDs in the transaction
    /// * `protocol_version` - Current protocol version
    ///
    /// # Note
    ///
    /// This is a no-op for protocol version 7 (matches stellar-core behavior).
    pub fn remove_one_time_signers_from_all_sources(
        &mut self,
        tx_hash: &henyey_common::Hash256,
        source_accounts: &[AccountId],
        protocol_version: u32,
    ) -> Result<()> {
        // Protocol 7 bypass (matches stellar-core behavior)
        if protocol_version == 7 {
            return Ok(());
        }

        // Create the pre-auth TX signer key from the transaction hash
        let signer_key =
            stellar_xdr::curr::SignerKey::PreAuthTx(stellar_xdr::curr::Uint256(tx_hash.0));

        // Remove from each source account
        for account_id in source_accounts {
            self.remove_account_signer(account_id, &signer_key)?;
        }
        Ok(())
    }

    /// Remove a specific signer from an account.
    ///
    /// Uses a validate-then-mutate pattern to ensure atomicity: all fallible
    /// checks (signer lookup, descriptor validation, sponsor account existence,
    /// count preconditions) happen before any state mutations. This matches
    /// stellar-core's `canRemoveSignerWith[out]Sponsorship` + `removeSignerWith[out]Sponsorship`
    /// two-phase approach (`SponsorshipUtils.cpp:623-740`).
    ///
    /// # Returns
    ///
    /// `Ok(true)` if the signer was found and removed, `Ok(false)` if the
    /// signer was not found (or account doesn't exist), `Err` if the signer
    /// exists but removal would leave inconsistent state (missing sponsor,
    /// corrupt counts, descriptor mismatch).
    pub fn remove_account_signer(
        &mut self,
        account_id: &AccountId,
        signer_key: &stellar_xdr::curr::SignerKey,
    ) -> Result<bool> {
        // ── Phase 1: Validate (fallible, no observable mutations) ────────

        let Some(account) = self.get_account(account_id) else {
            return Ok(false); // Account may have been removed (e.g., by merge)
        };

        // Find the signer index
        let signer_idx = account.signers.iter().position(|s| &s.key == signer_key);
        let Some(idx) = signer_idx else {
            return Ok(false); // Signer not found
        };

        // Strictly validate the paired signer/descriptor invariant before any
        // state changes. This mirrors stellar-core's account-entry invariant.
        let mut signer_set = SignerSet::strict_from_account(account)?;
        let sponsor_id = signer_set.sponsor_at(idx)?;

        // Validate sponsorship preconditions if the signer is sponsored.
        // Matches stellar-core's `canRemoveSignerWithSponsorship`
        // (SponsorshipUtils.cpp:633-652).
        if let Some(ref sponsor) = sponsor_id {
            // Pre-load sponsor account (idempotent lazy load from bucket list)
            self.ensure_account_loaded(sponsor)?;

            // Validate sponsor account exists
            let sponsor_account = self.get_account(sponsor).ok_or_else(|| {
                TxError::Internal("sponsor account missing during signer removal".to_string())
            })?;

            // Validate sponsor's num_sponsoring >= 1
            let sponsor_num_sponsoring = match &sponsor_account.ext {
                AccountEntryExt::V1(v1) => match &v1.ext {
                    AccountEntryExtensionV1Ext::V2(v2) => v2.num_sponsoring,
                    AccountEntryExtensionV1Ext::V0 => 0,
                },
                AccountEntryExt::V0 => 0,
            };
            if sponsor_num_sponsoring < 1 {
                return Err(TxError::Internal(
                    "invalid sponsoring account state: num_sponsoring < 1".to_string(),
                ));
            }

            // Re-read the sponsored account (immutable borrow after ensure_account_loaded)
            let account = self.get_account(account_id).ok_or_else(|| {
                TxError::Internal("sponsored account disappeared during validation".to_string())
            })?;

            // Validate sponsored account's num_sub_entries >= 1 and num_sponsored >= 1
            if account.num_sub_entries < 1 {
                return Err(TxError::Internal(
                    "invalid sponsored account state: num_sub_entries < 1".to_string(),
                ));
            }
            let sponsored_num_sponsored = match &account.ext {
                AccountEntryExt::V1(v1) => match &v1.ext {
                    AccountEntryExtensionV1Ext::V2(v2) => v2.num_sponsored,
                    AccountEntryExtensionV1Ext::V0 => 0,
                },
                AccountEntryExt::V0 => 0,
            };
            if sponsored_num_sponsored < 1 {
                return Err(TxError::Internal(
                    "invalid sponsored account state: num_sponsored < 1".to_string(),
                ));
            }
        } else {
            // Unsponsored signer: validate num_sub_entries >= 1
            // Matches stellar-core's `canRemoveSignerWithoutSponsorship`
            // (SponsorshipUtils.cpp:623-630).
            let account = self.get_account(account_id).ok_or_else(|| {
                TxError::Internal("account disappeared during validation".to_string())
            })?;
            if account.num_sub_entries < 1 {
                return Err(TxError::Internal(
                    "invalid account state: num_sub_entries < 1".to_string(),
                ));
            }
        }

        // ── Phase 2: Mutate (infallible — all preconditions verified) ────

        // Update sponsorship counts if sponsored.
        // Direct field writes — preconditions guarantee no underflow.
        if let Some(ref sponsor) = sponsor_id {
            // Decrement sponsor's num_sponsoring
            if let Some(sponsor_account) = self.get_account_mut(sponsor) {
                let ext = ensure_account_ext_v2(sponsor_account);
                ext.num_sponsoring -= 1;
            }

            // Decrement account's num_sponsored
            if let Some(account) = self.get_account_mut(account_id) {
                let ext = ensure_account_ext_v2(account);
                ext.num_sponsored -= 1;
            }
        }

        signer_set.remove(idx)?;
        let prepared = signer_set.prepare_write()?;

        // Remove the signer and paired descriptor from the account.
        if let Some(account) = self.get_account_mut(account_id) {
            prepared.apply(account);

            henyey_common::checked_types::dec_sub_entries(account, 1);
        }

        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::{create_test_account_id, create_test_account_with_sponsorship};
    use stellar_xdr::curr::{
        AccountEntryExt, AccountEntryExtensionV1Ext, Signer, SignerKey, SponsorshipDescriptor,
        Uint256,
    };

    /// Helper: build an account with ext v2, custom signers and descriptor slots.
    fn create_account_with_signers_and_descriptors(
        account_id: AccountId,
        signers: Vec<Signer>,
        descriptors: Vec<SponsorshipDescriptor>,
        num_sub_entries: u32,
        num_sponsored: u32,
        num_sponsoring: u32,
    ) -> AccountEntry {
        let mut account = create_test_account_with_sponsorship(
            account_id,
            100_000_000,
            num_sub_entries,
            num_sponsored,
            num_sponsoring,
        );
        account.signers = signers.try_into().unwrap();
        if let AccountEntryExt::V1(v1) = &mut account.ext {
            if let AccountEntryExtensionV1Ext::V2(v2) = &mut v1.ext {
                v2.signer_sponsoring_i_ds = descriptors.try_into().unwrap();
            }
        }
        account
    }

    fn make_signer(seed: u8, weight: u32) -> Signer {
        Signer {
            key: SignerKey::Ed25519(Uint256([seed; 32])),
            weight,
        }
    }

    fn get_ext_v2(account: &AccountEntry) -> &AccountEntryExtensionV2 {
        match &account.ext {
            AccountEntryExt::V1(v1) => match &v1.ext {
                AccountEntryExtensionV1Ext::V2(v2) => v2,
                _ => panic!("expected V2"),
            },
            _ => panic!("expected V1"),
        }
    }

    // ========================================================================
    // Validation parity test (issue #1510)
    // ========================================================================

    /// Regression test for #1499 — combined sponsoring cap check.
    #[test]
    fn test_update_num_sponsoring_rejects_combined_cap_exceeded() {
        let mut state = LedgerStateManager::new(5_000_000, 100);

        let account_id = create_test_account_id(1);

        // Account with numSubEntries near UINT32_MAX and some numSponsoring.
        // Combined: numSponsoring + numSubEntries should not exceed UINT32_MAX.
        let account = create_test_account_with_sponsorship(
            account_id.clone(),
            100_000_000,
            u32::MAX - 10, // num_sub_entries: near max
            0,             // num_sponsored
            5,             // num_sponsoring: combined = (MAX-10) + 5 = MAX-5
        );
        state.create_account(account);

        // Increasing numSponsoring by 10 would exceed combined cap:
        // (MAX-10) + 5 + 10 = MAX+5 > MAX
        let result = state.update_num_sponsoring(&account_id, 10);
        assert!(
            result.is_err(),
            "update_num_sponsoring should reject when combined numSponsoring + numSubEntries + delta exceeds UINT32_MAX"
        );
    }

    // ========================================================================
    // Signer descriptor cleanup tests (issue #1955)
    // ========================================================================

    /// Removing an unsponsored signer must also remove the None descriptor slot.
    #[test]
    fn test_remove_unsponsored_signer_cleans_up_descriptor() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let account_id = create_test_account_id(10);

        let signer = make_signer(1, 1);
        let account = create_account_with_signers_and_descriptors(
            account_id.clone(),
            vec![signer.clone()],
            vec![SponsorshipDescriptor(None)],
            1, // num_sub_entries (1 signer)
            0,
            0,
        );
        state.create_account(account);

        let removed = state
            .remove_account_signer(&account_id, &signer.key)
            .unwrap();
        assert!(removed);

        let account = state.get_account(&account_id).unwrap();
        assert_eq!(account.signers.len(), 0);
        let v2 = get_ext_v2(account);
        assert_eq!(
            v2.signer_sponsoring_i_ds.len(),
            0,
            "descriptor slot must be removed even when unsponsored"
        );
        assert_eq!(account.num_sub_entries, 0);
    }

    /// Removing a sponsored signer updates counts AND removes the descriptor.
    #[test]
    fn test_remove_sponsored_signer_updates_counts_and_cleans_descriptor() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let account_id = create_test_account_id(20);
        let sponsor_id = create_test_account_id(21);

        let signer = make_signer(2, 1);
        let account = create_account_with_signers_and_descriptors(
            account_id.clone(),
            vec![signer.clone()],
            vec![SponsorshipDescriptor(Some(sponsor_id.clone()))],
            1, // num_sub_entries
            1, // num_sponsored (signer is sponsored)
            0,
        );
        state.create_account(account);

        // Sponsor account with num_sponsoring = 1
        let sponsor = create_test_account_with_sponsorship(
            sponsor_id.clone(),
            100_000_000,
            0,
            0,
            1, // num_sponsoring
        );
        state.create_account(sponsor);

        let removed = state
            .remove_account_signer(&account_id, &signer.key)
            .unwrap();
        assert!(removed);

        let account = state.get_account(&account_id).unwrap();
        assert_eq!(account.signers.len(), 0);
        let v2 = get_ext_v2(account);
        assert_eq!(v2.signer_sponsoring_i_ds.len(), 0);
        assert_eq!(v2.num_sponsored, 0, "num_sponsored must be decremented");

        let sponsor = state.get_account(&sponsor_id).unwrap();
        let sv2 = get_ext_v2(sponsor);
        assert_eq!(sv2.num_sponsoring, 0, "num_sponsoring must be decremented");
    }

    /// Removing a leading unsponsored signer reindexes remaining descriptors.
    #[test]
    fn test_remove_leading_unsponsored_signer_reindexes_descriptors() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let account_id = create_test_account_id(30);
        let sponsor_id = create_test_account_id(31);

        // signer_a sorts before signer_b (seed 1 < seed 2)
        let signer_a = make_signer(1, 1); // unsponsored, index 0
        let signer_b = make_signer(2, 1); // sponsored, index 1
        let account = create_account_with_signers_and_descriptors(
            account_id.clone(),
            vec![signer_a.clone(), signer_b.clone()],
            vec![
                SponsorshipDescriptor(None),
                SponsorshipDescriptor(Some(sponsor_id.clone())),
            ],
            2, // num_sub_entries
            1, // num_sponsored (signer_b)
            0,
        );
        state.create_account(account);

        let sponsor =
            create_test_account_with_sponsorship(sponsor_id.clone(), 100_000_000, 0, 0, 1);
        state.create_account(sponsor);

        // Remove the leading unsponsored signer
        let removed = state
            .remove_account_signer(&account_id, &signer_a.key)
            .unwrap();
        assert!(removed);

        let account = state.get_account(&account_id).unwrap();
        assert_eq!(account.signers.len(), 1);
        assert_eq!(account.signers[0].key, signer_b.key);

        let v2 = get_ext_v2(account);
        assert_eq!(
            v2.signer_sponsoring_i_ds.len(),
            1,
            "descriptors must stay aligned with signers"
        );
        assert_eq!(
            v2.signer_sponsoring_i_ds[0].0,
            Some(sponsor_id.clone()),
            "remaining descriptor must be the sponsored one (reindexed from pos 1 to 0)"
        );
        // Sponsorship counts unchanged — only unsponsored signer was removed
        assert_eq!(v2.num_sponsored, 1);
        let sponsor = state.get_account(&sponsor_id).unwrap();
        let sv2 = get_ext_v2(sponsor);
        assert_eq!(sv2.num_sponsoring, 1);
    }

    /// Production entry point: remove_one_time_signers_from_all_sources
    /// cleans up pre-auth TX signer descriptors.
    #[test]
    fn test_remove_one_time_signers_cleans_descriptors() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let account_id = create_test_account_id(40);

        let tx_hash = henyey_common::Hash256([0xAB; 32]);
        let preauth_key = SignerKey::PreAuthTx(Uint256(tx_hash.0));
        let signer = Signer {
            key: preauth_key,
            weight: 1,
        };

        let account = create_account_with_signers_and_descriptors(
            account_id.clone(),
            vec![signer],
            vec![SponsorshipDescriptor(None)],
            1,
            0,
            0,
        );
        state.create_account(account);

        state
            .remove_one_time_signers_from_all_sources(&tx_hash, &[account_id.clone()], 25)
            .unwrap();

        let account = state.get_account(&account_id).unwrap();
        assert_eq!(account.signers.len(), 0);
        let v2 = get_ext_v2(account);
        assert_eq!(
            v2.signer_sponsoring_i_ds.len(),
            0,
            "production path must clean up descriptors"
        );
    }

    /// Already-corrupted state (desynchronized vectors) must error atomically.
    #[test]
    fn test_remove_signer_with_corrupted_descriptor_length() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let account_id = create_test_account_id(50);

        let signer = make_signer(1, 1);
        // Intentionally create a mismatch: 1 signer but 2 descriptor slots
        let account = create_account_with_signers_and_descriptors(
            account_id.clone(),
            vec![signer.clone()],
            vec![
                SponsorshipDescriptor(None),
                SponsorshipDescriptor(None), // extra stale slot
            ],
            1,
            0,
            0,
        );
        state.create_account(account);

        let result = state.remove_account_signer(&account_id, &signer.key);
        assert!(
            result.is_err(),
            "strict signer descriptors must reject extra stale slots"
        );

        let account = state.get_account(&account_id).unwrap();
        assert_eq!(account.signers.len(), 1);
        assert_eq!(account.num_sub_entries, 1);
        let v2 = get_ext_v2(account);
        assert_eq!(v2.signer_sponsoring_i_ds.len(), 2);
    }

    // ========================================================================
    // Hard-error regression tests (issue #1976)
    // ========================================================================

    /// Removing a sponsored signer when sponsor account is missing must error,
    /// and leave state completely unchanged (atomicity).
    #[test]
    fn test_remove_sponsored_signer_missing_sponsor_errors() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let account_id = create_test_account_id(60);
        let sponsor_id = create_test_account_id(61);

        let signer = make_signer(3, 1);
        let account = create_account_with_signers_and_descriptors(
            account_id.clone(),
            vec![signer.clone()],
            vec![SponsorshipDescriptor(Some(sponsor_id.clone()))],
            1, // num_sub_entries
            1, // num_sponsored
            0,
        );
        state.create_account(account);
        // Intentionally do NOT create the sponsor account

        let result = state.remove_account_signer(&account_id, &signer.key);
        assert!(
            result.is_err(),
            "must error when sponsor account is missing"
        );

        // Verify atomicity: nothing changed
        let account = state.get_account(&account_id).unwrap();
        assert_eq!(
            account.signers.len(),
            1,
            "signer must not be removed on error"
        );
        assert_eq!(
            account.num_sub_entries, 1,
            "num_sub_entries must not change on error"
        );
        let v2 = get_ext_v2(account);
        assert_eq!(
            v2.num_sponsored, 1,
            "num_sponsored must not change on error"
        );
        assert_eq!(
            v2.signer_sponsoring_i_ds.len(),
            1,
            "descriptors must not change on error"
        );
    }

    /// Descriptor out-of-bounds (signer_index >= descriptors.len()) must error.
    #[test]
    fn test_remove_signer_descriptor_out_of_bounds_errors() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let account_id = create_test_account_id(70);

        let signer = make_signer(4, 1);
        // Create account with ext v2 but EMPTY descriptor list — mismatch
        let account = create_account_with_signers_and_descriptors(
            account_id.clone(),
            vec![signer.clone()],
            vec![], // no descriptors — index 0 is out of bounds
            1,
            0,
            0,
        );
        state.create_account(account);

        let result = state.remove_account_signer(&account_id, &signer.key);
        assert!(
            result.is_err(),
            "must error when descriptor index is out of bounds"
        );

        // Verify atomicity: nothing changed
        let account = state.get_account(&account_id).unwrap();
        assert_eq!(
            account.signers.len(),
            1,
            "signer must not be removed on error"
        );
        assert_eq!(account.num_sub_entries, 1);
    }

    /// Zero num_sponsoring on sponsor when removing a sponsored signer must error.
    #[test]
    fn test_remove_sponsored_signer_zero_num_sponsoring_errors() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let account_id = create_test_account_id(80);
        let sponsor_id = create_test_account_id(81);

        let signer = make_signer(5, 1);
        let account = create_account_with_signers_and_descriptors(
            account_id.clone(),
            vec![signer.clone()],
            vec![SponsorshipDescriptor(Some(sponsor_id.clone()))],
            1, // num_sub_entries
            1, // num_sponsored
            0,
        );
        state.create_account(account);

        // Sponsor account exists but with num_sponsoring = 0 (corrupted state)
        let sponsor = create_test_account_with_sponsorship(
            sponsor_id.clone(),
            100_000_000,
            0,
            0,
            0, // num_sponsoring = 0 — corrupt
        );
        state.create_account(sponsor);

        let result = state.remove_account_signer(&account_id, &signer.key);
        assert!(
            result.is_err(),
            "must error when sponsor num_sponsoring < 1"
        );

        // Verify atomicity: nothing changed
        let account = state.get_account(&account_id).unwrap();
        assert_eq!(account.signers.len(), 1);
        assert_eq!(account.num_sub_entries, 1);
        let v2 = get_ext_v2(account);
        assert_eq!(v2.num_sponsored, 1);

        let sponsor = state.get_account(&sponsor_id).unwrap();
        let sv2 = get_ext_v2(sponsor);
        assert_eq!(
            sv2.num_sponsoring, 0,
            "sponsor num_sponsoring must not change"
        );
    }

    /// Zero num_sponsored on account when removing a sponsored signer must error.
    #[test]
    fn test_remove_sponsored_signer_zero_num_sponsored_errors() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let account_id = create_test_account_id(90);
        let sponsor_id = create_test_account_id(91);

        let signer = make_signer(6, 1);
        let account = create_account_with_signers_and_descriptors(
            account_id.clone(),
            vec![signer.clone()],
            vec![SponsorshipDescriptor(Some(sponsor_id.clone()))],
            1, // num_sub_entries
            0, // num_sponsored = 0 — corrupt
            0,
        );
        state.create_account(account);

        let sponsor = create_test_account_with_sponsorship(
            sponsor_id.clone(),
            100_000_000,
            0,
            0,
            1, // num_sponsoring = 1
        );
        state.create_account(sponsor);

        let result = state.remove_account_signer(&account_id, &signer.key);
        assert!(result.is_err(), "must error when account num_sponsored < 1");

        // Verify atomicity: nothing changed
        let account = state.get_account(&account_id).unwrap();
        assert_eq!(account.signers.len(), 1);
        let sponsor = state.get_account(&sponsor_id).unwrap();
        let sv2 = get_ext_v2(sponsor);
        assert_eq!(
            sv2.num_sponsoring, 1,
            "sponsor must not be decremented on error"
        );
    }

    /// Error propagation through remove_one_time_signers_from_all_sources.
    #[test]
    fn test_remove_one_time_signers_propagates_error() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let account_id = create_test_account_id(100);
        let sponsor_id = create_test_account_id(101);

        let tx_hash = henyey_common::Hash256([0xCC; 32]);
        let preauth_key = SignerKey::PreAuthTx(Uint256(tx_hash.0));
        let signer = Signer {
            key: preauth_key,
            weight: 1,
        };

        let account = create_account_with_signers_and_descriptors(
            account_id.clone(),
            vec![signer],
            vec![SponsorshipDescriptor(Some(sponsor_id.clone()))],
            1, // num_sub_entries
            1, // num_sponsored
            0,
        );
        state.create_account(account);
        // No sponsor account → error must propagate through the production entry point

        let result =
            state.remove_one_time_signers_from_all_sources(&tx_hash, &[account_id.clone()], 25);
        assert!(
            result.is_err(),
            "error must propagate through remove_one_time_signers_from_all_sources"
        );
    }
}
