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

    /// Validate that a sponsored entry can be removed without violating
    /// sponsorship invariants. Does NOT mutate any state.
    ///
    /// Mirrors stellar-core's `canRemoveEntryWithSponsorship`
    /// (SponsorshipUtils.cpp:558-584): validates that the sponsor's
    /// `num_sponsoring` is sufficient, and (for subentries) that the
    /// sponsored account's `num_sponsored` and `num_sub_entries` are
    /// sufficient. Corrupt state is an internal error, not a user-facing
    /// operation result.
    ///
    /// `sponsored` is `None` for claimable balances (they have no owner
    /// subentry relationship) and `Some` for all other entry types.
    pub fn validate_can_remove_sponsorship(
        &mut self,
        key: &LedgerKey,
        sponsored: Option<&AccountId>,
        multiplier: i64,
    ) -> Result<()> {
        if multiplier < 0 {
            return Err(TxError::Internal(
                "negative sponsorship multiplier".to_string(),
            ));
        }
        let Some(sponsor) = self.entry_sponsor(key) else {
            return Ok(());
        };
        // Lazily load the sponsor account so we can read its counts.
        self.ensure_account_loaded(&sponsor)?;
        let (num_sponsoring, _) =
            self.sponsorship_counts_for_account(&sponsor)
                .ok_or_else(|| {
                    TxError::Internal(format!("sponsor account missing for entry {:?}", key))
                })?;
        if num_sponsoring < multiplier {
            return Err(TxError::Internal(format!(
                "invalid sponsoring account state: num_sponsoring {} < multiplier {}",
                num_sponsoring, multiplier
            )));
        }
        if let Some(sponsored_id) = sponsored {
            self.ensure_account_loaded(sponsored_id)?;
            let account = self.get_account(sponsored_id).ok_or_else(|| {
                TxError::Internal(format!("sponsored account missing for entry {:?}", key))
            })?;
            let (_, num_sponsored) = sponsorship_counts(account);
            if num_sponsored < multiplier {
                return Err(TxError::Internal(format!(
                    "invalid sponsored account state: num_sponsored {} < multiplier {}",
                    num_sponsored, multiplier
                )));
            }
            // For non-account entries, also check num_sub_entries.
            // Account entries (account-merge) don't require this check —
            // stellar-core skips the subentry check when le.data.type() == ACCOUNT.
            if !matches!(key, LedgerKey::Account(_)) {
                if (account.num_sub_entries as i64) < multiplier {
                    return Err(TxError::Internal(format!(
                        "invalid sponsored account state: num_sub_entries {} < multiplier {}",
                        account.num_sub_entries, multiplier
                    )));
                }
            }
        }
        Ok(())
    }

    /// Remove sponsorship for a ledger entry and update account counts.
    ///
    /// Validates sponsorship invariants before mutating any state.
    pub fn remove_entry_sponsorship_and_update_counts(
        &mut self,
        key: &LedgerKey,
        sponsored: &AccountId,
        multiplier: i64,
    ) -> Result<Option<AccountId>> {
        if self.entry_sponsor(key).is_none() {
            return Ok(None);
        }
        if multiplier < 0 {
            return Err(TxError::Internal(
                "negative sponsorship multiplier".to_string(),
            ));
        }
        // Validate before any mutation.
        self.validate_can_remove_sponsorship(key, Some(sponsored), multiplier)?;
        let sponsor = self
            .remove_entry_sponsor(key)
            .expect("sponsor verified present");
        self.update_num_sponsoring(&sponsor, -multiplier)?;
        self.update_num_sponsored(sponsored, -multiplier)?;
        Ok(Some(sponsor))
    }

    /// Remove sponsorship for a ledger entry with optional sponsored account.
    ///
    /// Validates sponsorship invariants before mutating any state.
    pub fn remove_entry_sponsorship_with_sponsor_counts(
        &mut self,
        key: &LedgerKey,
        sponsored: Option<&AccountId>,
        multiplier: i64,
    ) -> Result<Option<AccountId>> {
        if self.entry_sponsor(key).is_none() {
            return Ok(None);
        }
        if multiplier < 0 {
            return Err(TxError::Internal(
                "negative sponsorship multiplier".to_string(),
            ));
        }
        // Validate before any mutation.
        self.validate_can_remove_sponsorship(key, sponsored, multiplier)?;
        let sponsor = self
            .remove_entry_sponsor(key)
            .expect("sponsor verified present");
        self.update_num_sponsoring(&sponsor, -multiplier)?;
        if let Some(sponsored) = sponsored {
            self.update_num_sponsored(sponsored, -multiplier)?;
        }
        Ok(Some(sponsor))
    }

    /// Atomically remove a sponsored subentry: validate sponsor counts and
    /// `num_sub_entries`, decrement them all, and clear sponsorship metadata.
    ///
    /// This is the entry-keyed analogue of stellar-core's
    /// `removeEntryWithPossibleSponsorship` (`SponsorshipUtils.cpp:808-847`).
    /// It centralizes the validate-then-mutate pattern previously duplicated
    /// across offer/trustline/pool-share trustline removal sites (issues
    /// #1976, #1995, #1996).
    ///
    /// The caller is responsible for the type-specific entry deletion
    /// (`delete_offer` / `delete_trustline_by_trustline_asset`) AFTER calling
    /// this helper. Some callers must read the entry's body before deletion
    /// to compute withdrawal amounts, which is why deletion is left to the
    /// caller.
    ///
    /// # Behavior (matches stellar-core's order)
    ///
    /// Phase 1 (validate, may return errors):
    ///   - Loads the sponsored account (so Phase 2 can decrement
    ///     `num_sub_entries` unconditionally — the unsponsored case still
    ///     needs the account loaded, which `validate_can_remove_sponsorship`
    ///     does NOT cover when there is no sponsor).
    ///   - Calls `validate_can_remove_sponsorship` which verifies
    ///     `num_sub_entries >= multiplier`, `num_sponsored >= multiplier`,
    ///     and (if sponsored) sponsor existence + `num_sponsoring >= multiplier`.
    ///
    /// Phase 2 (mutate, infallible by construction — direct field writes):
    ///   - Decrements sponsored account's `num_sub_entries` by `multiplier`
    ///     (matches stellar-core: `removeEntryWithoutSponsorship` runs first).
    ///   - If sponsored: clears entry sponsorship metadata, decrements
    ///     sponsor's `num_sponsoring` and sponsored's `num_sponsored` by
    ///     `multiplier` via `checked_sub().expect(...)`.
    ///
    /// The `expect(...)` panics fire only on a Phase 1/Phase 2 desync (logic
    /// bug). This matches stellar-core's `std::runtime_error` behavior on
    /// invariant violation.
    ///
    /// # Note on semantic sharpening vs. legacy callers
    ///
    /// Some legacy call sites used `if let Some(account) = state.get_account_mut(source)`
    /// to defensively skip `dec_sub_entries` if the account was missing.
    /// This helper instead requires the account to be loadable (Phase 1's
    /// `ensure_account_loaded`) and panics in Phase 2 if it's somehow gone.
    /// Rationale: the legacy "silent skip" was warn-and-continue padding for
    /// a state that should never occur. Failing loud on corrupt state is the
    /// pattern issues #1976/#1995 just established.
    pub fn remove_sponsored_subentry(
        &mut self,
        key: &LedgerKey,
        sponsored: &AccountId,
        multiplier: i64,
    ) -> Result<()> {
        if multiplier < 0 {
            return Err(TxError::Internal(
                "negative sponsorship multiplier".to_string(),
            ));
        }

        // ── Phase 1: validate ────────────────────────────────────────
        //
        // Load the sponsored account explicitly. `validate_can_remove_sponsorship`
        // only loads it when there *is* a sponsor; we need it loaded
        // unconditionally because Phase 2 unconditionally decrements
        // `num_sub_entries` on it.
        self.ensure_account_loaded(sponsored)?;
        self.validate_can_remove_sponsorship(key, Some(sponsored), multiplier)?;

        // ── Phase 2: mutate (infallible — direct field writes) ───────
        //
        // (a) Decrement num_sub_entries first (matches stellar-core order:
        //     `removeEntryWithoutSponsorship` then `removeEntrySponsorship`).
        let acc = self
            .get_account_mut(sponsored)
            .expect("sponsored account loaded in Phase 1");
        henyey_common::checked_types::dec_sub_entries(acc, multiplier as u32);

        // (b) If sponsored, clear metadata and decrement counts.
        if self.entry_sponsor(key).is_some() {
            let sponsor = self
                .remove_entry_sponsor(key)
                .expect("sponsor verified present in Phase 1");

            let sponsor_acc = self
                .get_account_mut(&sponsor)
                .expect("sponsor account loaded by validate_can_remove_sponsorship");
            let ext = ensure_account_ext_v2(sponsor_acc);
            ext.num_sponsoring = ext
                .num_sponsoring
                .checked_sub(multiplier as u32)
                .expect("Phase 1 verified num_sponsoring >= multiplier");

            let sponsored_acc = self
                .get_account_mut(sponsored)
                .expect("sponsored account loaded in Phase 1");
            let ext = ensure_account_ext_v2(sponsored_acc);
            ext.num_sponsored = ext
                .num_sponsored
                .checked_sub(multiplier as u32)
                .expect("Phase 1 verified num_sponsored >= multiplier");
        }

        Ok(())
    }

    /// Atomically clear sponsorship for the source account ledger entry
    /// during account-merge: validate sponsor counts and `num_sponsored`,
    /// then decrement them.
    ///
    /// Account-merge has special semantics versus generic
    /// [`remove_sponsored_subentry`](Self::remove_sponsored_subentry):
    ///
    /// - `num_sub_entries` is **not** decremented. This matches stellar-core's
    ///   `SponsorshipUtils.cpp:685` (`if (le.data.type() != ACCOUNT)` guard
    ///   in `removeEntryWithoutSponsorship`) and is irrelevant anyway because
    ///   the caller will delete the account immediately after.
    /// - `num_sponsored` IS decremented on the source, matching stellar-core's
    ///   `removeEntrySponsorship` (`SponsorshipUtils.cpp:391-397`). The
    ///   decrement is technically wasted (the account is about to be
    ///   deleted), but eliding it would change observable transaction meta
    ///   and is therefore deferred to a separate parity investigation.
    /// - `num_sponsoring` IS decremented on the sponsor.
    /// - The entry-sponsorship metadata is cleared.
    /// - If the source is its own sponsor (corrupt state — impossible by
    ///   sponsorship invariants), returns `TxError::Internal` matching
    ///   stellar-core's `SponsorshipUtils.cpp:822-824` `runtime_error`.
    ///
    /// If the source key has no sponsor, this is a no-op (matching the
    /// pre-existing `if entry_sponsor(...).is_some()` guard at the
    /// account-merge call site).
    ///
    /// Mirrors stellar-core's `MergeOpFrame.cpp:180,262` →
    /// `removeEntryWithPossibleSponsorship` flow for an ACCOUNT entry.
    ///
    /// # Errors
    ///
    /// - `TxError::Internal` if `multiplier < 0`.
    /// - `TxError::Internal` if Phase-1 validation fails (sponsor's
    ///   `num_sponsoring < multiplier`, or source's
    ///   `num_sponsored < multiplier`).
    /// - `TxError::Internal` if source is listed as its own sponsor.
    ///
    /// State is **not** mutated on any error path.
    pub fn remove_account_merge_sponsorship(
        &mut self,
        source: &AccountId,
        multiplier: i64,
    ) -> Result<()> {
        if multiplier < 0 {
            return Err(TxError::Internal(
                "negative sponsorship multiplier".to_string(),
            ));
        }
        let key = LedgerKey::Account(LedgerKeyAccount {
            account_id: source.clone(),
        });
        let Some(sponsor) = self.entry_sponsor(&key) else {
            return Ok(());
        };
        // Fail-loud on self-sponsor (impossible by invariant; matches
        // stellar-core SponsorshipUtils.cpp:822-824 runtime_error).
        if sponsor == *source {
            return Err(TxError::Internal(
                "source account is listed as its own sponsor".to_string(),
            ));
        }

        // Phase 1: validate. validate_can_remove_sponsorship checks the
        // sponsor's num_sponsoring and (with sponsored=Some) the source's
        // num_sponsored. It explicitly skips the num_sub_entries check for
        // ACCOUNT keys, which is exactly what we need here.
        self.validate_can_remove_sponsorship(&key, Some(source), multiplier)?;

        // Phase 2: mutate (direct field writes — preconditions verified).
        let sponsor_removed = self
            .remove_entry_sponsor(&key)
            .expect("sponsor verified present in Phase 1");
        debug_assert_eq!(sponsor_removed, sponsor);

        let sponsor_acc = self
            .get_account_mut(&sponsor)
            .expect("sponsor account loaded by validate_can_remove_sponsorship");
        let ext = ensure_account_ext_v2(sponsor_acc);
        ext.num_sponsoring = ext
            .num_sponsoring
            .checked_sub(multiplier as u32)
            .expect("Phase 1 verified num_sponsoring >= multiplier");

        let source_acc = self
            .get_account_mut(source)
            .expect("source account loaded by validate_can_remove_sponsorship");
        let ext = ensure_account_ext_v2(source_acc);
        ext.num_sponsored = ext
            .num_sponsored
            .checked_sub(multiplier as u32)
            .expect("Phase 1 verified num_sponsored >= multiplier");

        Ok(())
    }

    /// Atomically remove a sponsored claimable balance entry.
    ///
    /// Like `remove_sponsored_subentry` but for entries that are NOT
    /// subentries (claimable balances): only the sponsor's `num_sponsoring`
    /// is decremented; the source account has no associated subentry.
    ///
    /// As with `remove_sponsored_subentry`, the caller deletes the entry
    /// itself (`delete_claimable_balance`) AFTER this call.
    ///
    /// If the claimable balance has no sponsor, this is a no-op (matching
    /// stellar-core's `removeEntryWithPossibleSponsorship` early return for
    /// the non-sponsored claimable-balance case).
    pub fn remove_sponsored_claimable_balance(
        &mut self,
        key: &LedgerKey,
        multiplier: i64,
    ) -> Result<()> {
        if multiplier < 0 {
            return Err(TxError::Internal(
                "negative sponsorship multiplier".to_string(),
            ));
        }
        if self.entry_sponsor(key).is_none() {
            return Ok(()); // No sponsor — nothing to validate or decrement.
        }
        // Phase 1: validate (loads sponsor, verifies num_sponsoring >= multiplier).
        self.validate_can_remove_sponsorship(key, None, multiplier)?;

        // Phase 2: mutate (direct field writes — preconditions verified).
        let sponsor = self
            .remove_entry_sponsor(key)
            .expect("sponsor verified present in Phase 1");
        let sponsor_acc = self
            .get_account_mut(&sponsor)
            .expect("sponsor account loaded by validate_can_remove_sponsorship");
        let ext = ensure_account_ext_v2(sponsor_acc);
        ext.num_sponsoring = ext
            .num_sponsoring
            .checked_sub(multiplier as u32)
            .expect("Phase 1 verified num_sponsoring >= multiplier");
        Ok(())
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

    // ========================================================================
    // validate_can_remove_sponsorship tests (issue #2005)
    // ========================================================================

    #[test]
    fn test_validate_can_remove_sponsorship_no_sponsor() {
        // Entry with no sponsor should pass validation (nothing to check).
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let mut hash = [0u8; 32];
        hash[0] = 99;
        let key = LedgerKey::ClaimableBalance(stellar_xdr::curr::LedgerKeyClaimableBalance {
            balance_id: stellar_xdr::curr::ClaimableBalanceId::ClaimableBalanceIdTypeV0(
                stellar_xdr::curr::Hash(hash),
            ),
        });
        // No sponsor set for this key
        let result = state.validate_can_remove_sponsorship(&key, None, 1);
        assert!(result.is_ok());
    }

    #[test]
    fn test_validate_can_remove_sponsorship_subentry_underflow() {
        // Sponsored trustline entry where the sponsored account has
        // num_sub_entries = 0 — should fail validation.
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let owner_id = create_test_account_id(0);
        let sponsor_id = create_test_account_id(1);

        // Owner has num_sub_entries = 0 but num_sponsored = 1
        let owner = create_test_account_with_sponsorship(
            owner_id.clone(),
            100_000_000,
            0, // num_sub_entries = 0 — corrupt for a subentry
            1,
            0,
        );
        state.create_account(owner);

        // Sponsor has num_sponsoring = 1
        state.create_account(create_test_account_with_sponsorship(
            sponsor_id.clone(),
            100_000_000,
            0,
            0,
            1,
        ));

        // Use a trustline key (a subentry type that requires num_sub_entries check)
        let key = LedgerKey::Trustline(stellar_xdr::curr::LedgerKeyTrustLine {
            account_id: owner_id.clone(),
            asset: stellar_xdr::curr::TrustLineAsset::Native,
        });
        state.set_entry_sponsor(key.clone(), sponsor_id.clone());

        let result = state.validate_can_remove_sponsorship(&key, Some(&owner_id), 1);
        assert!(result.is_err());
        match result.unwrap_err() {
            TxError::Internal(msg) => {
                assert!(msg.contains("num_sub_entries"));
            }
            other => panic!("expected Internal error, got {:?}", other),
        }
    }

    // ========================================================================
    // remove_sponsored_subentry / remove_sponsored_claimable_balance tests
    // (issue #1996 — centralized atomic sponsorship-removal helpers)
    // ========================================================================

    /// Helper: build a trustline ledger key for tests.
    fn trustline_key(owner: &AccountId, asset_seed: u8) -> LedgerKey {
        LedgerKey::Trustline(stellar_xdr::curr::LedgerKeyTrustLine {
            account_id: owner.clone(),
            asset: stellar_xdr::curr::TrustLineAsset::CreditAlphanum4(
                stellar_xdr::curr::AlphaNum4 {
                    asset_code: stellar_xdr::curr::AssetCode4([asset_seed, 0, 0, 0]),
                    issuer: create_test_account_id(200),
                },
            ),
        })
    }

    /// Helper: build a claimable balance ledger key for tests.
    fn cb_key(seed: u8) -> LedgerKey {
        LedgerKey::ClaimableBalance(stellar_xdr::curr::LedgerKeyClaimableBalance {
            balance_id: stellar_xdr::curr::ClaimableBalanceId::ClaimableBalanceIdTypeV0(
                stellar_xdr::curr::Hash([seed; 32]),
            ),
        })
    }

    /// remove_sponsored_subentry decrements all three counts for a sponsored entry.
    #[test]
    fn test_remove_sponsored_subentry_offer_decrements_all_three_counts() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let owner_id = create_test_account_id(40);
        let sponsor_id = create_test_account_id(41);

        // Owner has 1 subentry, 1 sponsored.
        state.create_account(create_test_account_with_sponsorship(
            owner_id.clone(),
            100_000_000,
            1, // num_sub_entries
            1, // num_sponsored
            0, // num_sponsoring
        ));
        // Sponsor has num_sponsoring = 1.
        state.create_account(create_test_account_with_sponsorship(
            sponsor_id.clone(),
            100_000_000,
            0,
            0,
            1,
        ));

        let key = trustline_key(&owner_id, 1);
        state.set_entry_sponsor(key.clone(), sponsor_id.clone());

        state.remove_sponsored_subentry(&key, &owner_id, 1).unwrap();

        let owner = state.get_account(&owner_id).unwrap();
        assert_eq!(owner.num_sub_entries, 0);
        let v2 = get_ext_v2(owner);
        assert_eq!(v2.num_sponsored, 0);

        let sponsor = state.get_account(&sponsor_id).unwrap();
        let sv2 = get_ext_v2(sponsor);
        assert_eq!(sv2.num_sponsoring, 0);

        assert!(
            state.entry_sponsor(&key).is_none(),
            "sponsorship metadata must be cleared"
        );
    }

    /// remove_sponsored_subentry decrements only num_sub_entries when unsponsored.
    /// Critically: the sponsored account must be loaded by the helper itself
    /// (verifies the explicit ensure_account_loaded call in Phase 1).
    #[test]
    fn test_remove_sponsored_subentry_unsponsored_only_decrements_subentries() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let owner_id = create_test_account_id(50);

        state.create_account(create_test_account_with_sponsorship(
            owner_id.clone(),
            100_000_000,
            1, // num_sub_entries
            0,
            0,
        ));

        let key = trustline_key(&owner_id, 2);
        // No set_entry_sponsor call — entry is unsponsored.

        state.remove_sponsored_subentry(&key, &owner_id, 1).unwrap();

        let owner = state.get_account(&owner_id).unwrap();
        assert_eq!(owner.num_sub_entries, 0);
    }

    /// remove_sponsored_claimable_balance decrements only num_sponsoring.
    #[test]
    fn test_remove_sponsored_claimable_balance_decrements_num_sponsoring() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let sponsor_id = create_test_account_id(60);

        state.create_account(create_test_account_with_sponsorship(
            sponsor_id.clone(),
            100_000_000,
            0,
            0,
            3, // claimable balance with 3 claimants → num_sponsoring = 3
        ));

        let key = cb_key(1);
        state.set_entry_sponsor(key.clone(), sponsor_id.clone());

        state.remove_sponsored_claimable_balance(&key, 3).unwrap();

        let sponsor = state.get_account(&sponsor_id).unwrap();
        let sv2 = get_ext_v2(sponsor);
        assert_eq!(sv2.num_sponsoring, 0);
        assert!(state.entry_sponsor(&key).is_none());
    }

    /// remove_sponsored_claimable_balance is a no-op for unsponsored CBs.
    #[test]
    fn test_remove_sponsored_claimable_balance_no_sponsor_is_noop() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let unrelated_id = create_test_account_id(70);

        // Unrelated account in state — verify nothing about it changes.
        state.create_account(create_test_account_with_sponsorship(
            unrelated_id.clone(),
            100_000_000,
            5, // num_sub_entries
            2, // num_sponsored
            7, // num_sponsoring
        ));

        let key = cb_key(2);
        // No set_entry_sponsor call.

        state.remove_sponsored_claimable_balance(&key, 2).unwrap();

        let acc = state.get_account(&unrelated_id).unwrap();
        assert_eq!(acc.num_sub_entries, 5);
        let v2 = get_ext_v2(acc);
        assert_eq!(v2.num_sponsored, 2);
        assert_eq!(v2.num_sponsoring, 7);
    }

    /// remove_sponsored_subentry errors before any count mutation when sponsor missing.
    #[test]
    fn test_remove_sponsored_subentry_missing_sponsor_errors_before_mutation() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let owner_id = create_test_account_id(80);
        let phantom_sponsor_id = create_test_account_id(81); // never created

        state.create_account(create_test_account_with_sponsorship(
            owner_id.clone(),
            100_000_000,
            1,
            1,
            0,
        ));

        let key = trustline_key(&owner_id, 3);
        // Manually corrupt: register a sponsor that doesn't exist.
        state.set_entry_sponsor(key.clone(), phantom_sponsor_id.clone());

        let result = state.remove_sponsored_subentry(&key, &owner_id, 1);
        assert!(result.is_err());

        // Count fields on owner must be unchanged (atomic Phase 1 failure).
        let owner = state.get_account(&owner_id).unwrap();
        assert_eq!(owner.num_sub_entries, 1);
        let v2 = get_ext_v2(owner);
        assert_eq!(v2.num_sponsored, 1);
    }

    /// remove_sponsored_subentry errors before mutation when sponsor.num_sponsoring < multiplier.
    #[test]
    fn test_remove_sponsored_subentry_low_num_sponsoring_errors_before_mutation() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let owner_id = create_test_account_id(90);
        let sponsor_id = create_test_account_id(91);

        state.create_account(create_test_account_with_sponsorship(
            owner_id.clone(),
            100_000_000,
            1,
            1,
            0,
        ));
        // Sponsor has num_sponsoring = 0 (corrupt: should be at least 1).
        state.create_account(create_test_account_with_sponsorship(
            sponsor_id.clone(),
            100_000_000,
            0,
            0,
            0, // ← invalid for the sponsorship we register below
        ));

        let key = trustline_key(&owner_id, 4);
        state.set_entry_sponsor(key.clone(), sponsor_id.clone());

        let result = state.remove_sponsored_subentry(&key, &owner_id, 1);
        assert!(result.is_err());

        let owner = state.get_account(&owner_id).unwrap();
        assert_eq!(owner.num_sub_entries, 1);
        let v2 = get_ext_v2(owner);
        assert_eq!(v2.num_sponsored, 1);
        let sponsor = state.get_account(&sponsor_id).unwrap();
        let sv2 = get_ext_v2(sponsor);
        assert_eq!(sv2.num_sponsoring, 0);
    }

    /// Multiplier 2 (pool-share trustline) decrements all counts by 2.
    #[test]
    fn test_remove_sponsored_subentry_pool_share_multiplier_2() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let owner_id = create_test_account_id(110);
        let sponsor_id = create_test_account_id(111);

        state.create_account(create_test_account_with_sponsorship(
            owner_id.clone(),
            100_000_000,
            5, // num_sub_entries
            2, // num_sponsored
            0,
        ));
        state.create_account(create_test_account_with_sponsorship(
            sponsor_id.clone(),
            100_000_000,
            0,
            0,
            2, // num_sponsoring (pool-share trustline counts as 2)
        ));

        let key = trustline_key(&owner_id, 5);
        state.set_entry_sponsor(key.clone(), sponsor_id.clone());

        state.remove_sponsored_subentry(&key, &owner_id, 2).unwrap();

        let owner = state.get_account(&owner_id).unwrap();
        assert_eq!(owner.num_sub_entries, 3);
        let v2 = get_ext_v2(owner);
        assert_eq!(v2.num_sponsored, 0);

        let sponsor = state.get_account(&sponsor_id).unwrap();
        let sv2 = get_ext_v2(sponsor);
        assert_eq!(sv2.num_sponsoring, 0);
    }

    /// remove_sponsored_subentry errors before mutation when num_sub_entries < multiplier.
    #[test]
    fn test_remove_sponsored_subentry_low_num_sub_entries_errors() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let owner_id = create_test_account_id(120);
        let sponsor_id = create_test_account_id(121);

        // Owner has num_sub_entries = 0 (corrupt — should be at least 1 for a sponsored sub).
        state.create_account(create_test_account_with_sponsorship(
            owner_id.clone(),
            100_000_000,
            0, // ← invalid
            1,
            0,
        ));
        state.create_account(create_test_account_with_sponsorship(
            sponsor_id.clone(),
            100_000_000,
            0,
            0,
            1,
        ));

        let key = trustline_key(&owner_id, 6);
        state.set_entry_sponsor(key.clone(), sponsor_id.clone());

        let result = state.remove_sponsored_subentry(&key, &owner_id, 1);
        assert!(result.is_err());

        let owner = state.get_account(&owner_id).unwrap();
        assert_eq!(owner.num_sub_entries, 0);
        let v2 = get_ext_v2(owner);
        assert_eq!(v2.num_sponsored, 1);
        let sponsor = state.get_account(&sponsor_id).unwrap();
        let sv2 = get_ext_v2(sponsor);
        assert_eq!(sv2.num_sponsoring, 1);
    }

    // ========================================================================
    // remove_account_merge_sponsorship tests (issue #2048 — account-merge-
    // specific sponsorship removal helper)
    // ========================================================================

    /// Helper: build the source account ledger key for tests.
    fn source_account_key(source: &AccountId) -> LedgerKey {
        LedgerKey::Account(LedgerKeyAccount {
            account_id: source.clone(),
        })
    }

    /// remove_account_merge_sponsorship decrements sponsor's num_sponsoring AND
    /// source's num_sponsored, leaves num_sub_entries untouched, and clears
    /// sponsorship metadata.
    #[test]
    fn test_remove_account_merge_sponsorship_decrements_both_counts() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let source_id = create_test_account_id(150);
        let sponsor_id = create_test_account_id(151);

        // Source: num_sub_entries = 0 (account-merge precondition: no
        // non-signer subentries), num_sponsored = 2 (account = 2 reserve units).
        state.create_account(create_test_account_with_sponsorship(
            source_id.clone(),
            100_000_000,
            0, // num_sub_entries
            2, // num_sponsored
            0,
        ));
        // Sponsor: num_sponsoring = 2 to match the sponsored source account.
        state.create_account(create_test_account_with_sponsorship(
            sponsor_id.clone(),
            100_000_000,
            0,
            0,
            2, // num_sponsoring
        ));

        let key = source_account_key(&source_id);
        state.set_entry_sponsor(key.clone(), sponsor_id.clone());

        state
            .remove_account_merge_sponsorship(&source_id, 2)
            .unwrap();

        let source = state.get_account(&source_id).unwrap();
        assert_eq!(
            source.num_sub_entries, 0,
            "num_sub_entries must not be touched"
        );
        let v2 = get_ext_v2(source);
        assert_eq!(v2.num_sponsored, 0);

        let sponsor = state.get_account(&sponsor_id).unwrap();
        let sv2 = get_ext_v2(sponsor);
        assert_eq!(sv2.num_sponsoring, 0);

        assert!(
            state.entry_sponsor(&key).is_none(),
            "sponsorship metadata must be cleared"
        );
    }

    /// remove_account_merge_sponsorship is a no-op when the source has no
    /// entry-sponsorship metadata (matches the legacy call-site `if` guard).
    #[test]
    fn test_remove_account_merge_sponsorship_no_sponsor_is_noop() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let source_id = create_test_account_id(160);

        state.create_account(create_test_account_with_sponsorship(
            source_id.clone(),
            100_000_000,
            3, // unrelated num_sub_entries — must be unchanged
            5, // unrelated num_sponsored — must be unchanged
            0,
        ));

        // No set_entry_sponsor call.
        state
            .remove_account_merge_sponsorship(&source_id, 2)
            .unwrap();

        let source = state.get_account(&source_id).unwrap();
        assert_eq!(source.num_sub_entries, 3);
        let v2 = get_ext_v2(source);
        assert_eq!(v2.num_sponsored, 5);
        assert!(state
            .entry_sponsor(&source_account_key(&source_id))
            .is_none());
    }

    /// Negative multiplier returns Internal error and does not mutate state.
    #[test]
    fn test_remove_account_merge_sponsorship_negative_multiplier_errors() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let source_id = create_test_account_id(170);
        let sponsor_id = create_test_account_id(171);

        state.create_account(create_test_account_with_sponsorship(
            source_id.clone(),
            100_000_000,
            0,
            2,
            0,
        ));
        state.create_account(create_test_account_with_sponsorship(
            sponsor_id.clone(),
            100_000_000,
            0,
            0,
            2,
        ));
        let key = source_account_key(&source_id);
        state.set_entry_sponsor(key.clone(), sponsor_id.clone());

        let result = state.remove_account_merge_sponsorship(&source_id, -1);
        assert!(result.is_err());
        match result.unwrap_err() {
            TxError::Internal(msg) => assert!(msg.contains("negative")),
            other => panic!("expected Internal, got {:?}", other),
        }

        // State must be unchanged.
        let v2 = get_ext_v2(state.get_account(&source_id).unwrap());
        assert_eq!(v2.num_sponsored, 2);
        let sv2 = get_ext_v2(state.get_account(&sponsor_id).unwrap());
        assert_eq!(sv2.num_sponsoring, 2);
        assert!(state.entry_sponsor(&key).is_some());
    }

    /// Sponsor with insufficient num_sponsoring errors before any mutation.
    #[test]
    fn test_remove_account_merge_sponsorship_low_num_sponsoring_errors_before_mutation() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let source_id = create_test_account_id(180);
        let sponsor_id = create_test_account_id(181);

        state.create_account(create_test_account_with_sponsorship(
            source_id.clone(),
            100_000_000,
            0,
            2, // num_sponsored
            0,
        ));
        // Sponsor has num_sponsoring = 1, less than the multiplier of 2.
        state.create_account(create_test_account_with_sponsorship(
            sponsor_id.clone(),
            100_000_000,
            0,
            0,
            1, // ← invalid for multiplier 2
        ));

        let key = source_account_key(&source_id);
        state.set_entry_sponsor(key.clone(), sponsor_id.clone());

        let result = state.remove_account_merge_sponsorship(&source_id, 2);
        assert!(result.is_err());

        // Phase-1 atomic failure: nothing mutated.
        let v2 = get_ext_v2(state.get_account(&source_id).unwrap());
        assert_eq!(v2.num_sponsored, 2);
        let sv2 = get_ext_v2(state.get_account(&sponsor_id).unwrap());
        assert_eq!(sv2.num_sponsoring, 1);
        assert!(state.entry_sponsor(&key).is_some());
    }

    /// Source with insufficient num_sponsored errors before any mutation.
    #[test]
    fn test_remove_account_merge_sponsorship_low_num_sponsored_errors_before_mutation() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let source_id = create_test_account_id(190);
        let sponsor_id = create_test_account_id(191);

        // Source has num_sponsored = 1, less than the multiplier of 2.
        state.create_account(create_test_account_with_sponsorship(
            source_id.clone(),
            100_000_000,
            0,
            1, // ← invalid for multiplier 2
            0,
        ));
        state.create_account(create_test_account_with_sponsorship(
            sponsor_id.clone(),
            100_000_000,
            0,
            0,
            2,
        ));

        let key = source_account_key(&source_id);
        state.set_entry_sponsor(key.clone(), sponsor_id.clone());

        let result = state.remove_account_merge_sponsorship(&source_id, 2);
        assert!(result.is_err());

        // Phase-1 atomic failure: nothing mutated.
        let v2 = get_ext_v2(state.get_account(&source_id).unwrap());
        assert_eq!(v2.num_sponsored, 1);
        let sv2 = get_ext_v2(state.get_account(&sponsor_id).unwrap());
        assert_eq!(sv2.num_sponsoring, 2);
        assert!(state.entry_sponsor(&key).is_some());
    }

    /// remove_account_merge_sponsorship must not touch source's num_sub_entries
    /// regardless of its value. Matches stellar-core's
    /// `removeEntryWithoutSponsorship` no-op for ACCOUNT entries
    /// (SponsorshipUtils.cpp:685 guard).
    #[test]
    fn test_remove_account_merge_sponsorship_does_not_touch_num_sub_entries() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let source_id = create_test_account_id(200);
        let sponsor_id = create_test_account_id(201);

        // Set num_sub_entries = 5 directly. The helper must be agnostic to
        // why it's set — it must never touch this field.
        state.create_account(create_test_account_with_sponsorship(
            source_id.clone(),
            100_000_000,
            5, // num_sub_entries — to be left untouched
            2,
            0,
        ));
        state.create_account(create_test_account_with_sponsorship(
            sponsor_id.clone(),
            100_000_000,
            0,
            0,
            2,
        ));

        let key = source_account_key(&source_id);
        state.set_entry_sponsor(key.clone(), sponsor_id.clone());

        state
            .remove_account_merge_sponsorship(&source_id, 2)
            .unwrap();

        let source = state.get_account(&source_id).unwrap();
        assert_eq!(
            source.num_sub_entries, 5,
            "num_sub_entries must remain 5 — helper must not touch it"
        );
    }

    /// Source listed as its own sponsor is corrupt state. Helper must fail
    /// loud, matching stellar-core's SponsorshipUtils.cpp:822-824 runtime_error.
    #[test]
    fn test_remove_account_merge_sponsorship_self_sponsor_errors() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let source_id = create_test_account_id(210);

        state.create_account(create_test_account_with_sponsorship(
            source_id.clone(),
            100_000_000,
            0,
            2,
            2, // self-sponsor scenario: source has its own num_sponsoring too
        ));

        let key = source_account_key(&source_id);
        // Manually corrupt: source listed as its own sponsor.
        state.set_entry_sponsor(key.clone(), source_id.clone());

        let result = state.remove_account_merge_sponsorship(&source_id, 2);
        assert!(result.is_err());
        match result.unwrap_err() {
            TxError::Internal(msg) => {
                assert!(
                    msg.contains("source") && msg.contains("own sponsor"),
                    "expected self-sponsor error, got: {}",
                    msg
                );
            }
            other => panic!("expected Internal error, got {:?}", other),
        }

        // State unchanged.
        let v2 = get_ext_v2(state.get_account(&source_id).unwrap());
        assert_eq!(v2.num_sponsored, 2);
        assert_eq!(v2.num_sponsoring, 2);
        assert!(state.entry_sponsor(&key).is_some());
    }
}
