//! Ledger entry CRUD operations for [`LedgerStateManager`].

use super::*;

impl LedgerStateManager {
    pub(super) fn last_modified_for_key(&self, key: &LedgerKey) -> u32 {
        self.get_last_modified(key).unwrap_or(self.ledger_seq)
    }

    pub(super) fn last_modified_snapshot_for_key(&self, key: &LedgerKey) -> Option<u32> {
        self.entry_last_modified_snapshots
            .get(key)
            .copied()
            .flatten()
    }

    pub(super) fn snapshot_last_modified_key(&mut self, key: &LedgerKey) {
        if !self.entry_last_modified_snapshots.contains_key(key) {
            let snapshot = self.get_last_modified(key);
            self.entry_last_modified_snapshots
                .insert(key.clone(), snapshot);
        }
    }

    pub(super) fn set_last_modified_key(&mut self, key: LedgerKey, seq: u32) {
        self.insert_last_modified(key, seq);
    }

    pub(super) fn remove_last_modified_key(&mut self, key: &LedgerKey) {
        self.remove_last_modified(key);
    }

    pub(super) fn ledger_entry_ext_for_snapshot(&self, key: &LedgerKey) -> LedgerEntryExt {
        let ext_present = self
            .entry_sponsorship_ext_snapshots
            .get(key)
            .copied()
            .unwrap_or_else(|| self.contains_sponsorship_ext(key));
        let sponsor_snapshot = if let Some(snapshot) = self.entry_sponsorship_snapshots.get(key) {
            snapshot.clone()
        } else {
            self.get_entry_sponsorship(key)
        };

        if ext_present || sponsor_snapshot.is_some() {
            LedgerEntryExt::V1(LedgerEntryExtensionV1 {
                sponsoring_id: SponsorshipDescriptor(sponsor_snapshot),
                ext: LedgerEntryExtensionV1Ext::V0,
            })
        } else {
            LedgerEntryExt::V0
        }
    }

    pub(super) fn capture_op_snapshot_for_key(&mut self, key: &LedgerKey) {
        if !self.op_snapshots_active || self.op_entry_snapshots.contains_key(key) {
            return;
        }
        if let Some(entry) = self.get_entry(key) {
            self.op_entry_snapshots.insert(key.clone(), entry);
        }
    }

    pub(super) fn ledger_entry_ext_for(&self, key: &LedgerKey) -> LedgerEntryExt {
        let sponsor = self.get_entry_sponsorship(key);
        if self.contains_sponsorship_ext(key) || sponsor.is_some() {
            LedgerEntryExt::V1(LedgerEntryExtensionV1 {
                sponsoring_id: SponsorshipDescriptor(sponsor),
                ext: LedgerEntryExtensionV1Ext::V0,
            })
        } else {
            LedgerEntryExt::V0
        }
    }

    /// Record entry metadata (last_modified, sponsorship ext, sponsor) for a ledger key.
    ///
    /// This is shared by `load_entry` and `load_entry_without_snapshot` to avoid
    /// duplicating the metadata bookkeeping in every match arm.
    fn record_entry_metadata(
        &mut self,
        ledger_key: LedgerKey,
        last_modified: u32,
        has_sponsorship_ext: bool,
        sponsor: Option<AccountId>,
    ) {
        if has_sponsorship_ext {
            self.insert_sponsorship_ext(ledger_key.clone());
        }
        if let Some(sponsor) = sponsor {
            self.insert_last_modified(ledger_key.clone(), last_modified);
            self.insert_entry_sponsorship(ledger_key, sponsor);
        } else {
            self.insert_last_modified(ledger_key, last_modified);
        }
    }

    /// Load a single entry into the state manager.
    pub fn load_entry(&mut self, entry: LedgerEntry) {
        // Offers go directly into the canonical OfferStore (includes metadata).
        if matches!(entry.data, LedgerEntryData::Offer(_)) {
            let mut store = self.offer_store_lock();
            store.insert_from_ledger_entry(&entry);
            return;
        }

        let sponsor = sponsorship_from_entry_ext(&entry);
        let has_sponsorship_ext = matches!(entry.ext, LedgerEntryExt::V1(_));
        let last_modified = entry.last_modified_ledger_seq;
        match entry.data {
            LedgerEntryData::Account(account) => {
                let account_id = account.account_id.clone();
                self.accounts.insert(account_id.clone(), account);
                let ledger_key = LedgerKey::Account(LedgerKeyAccount { account_id });
                self.record_entry_metadata(ledger_key, last_modified, has_sponsorship_ext, sponsor);
            }
            LedgerEntryData::Trustline(trustline) => {
                let account_id = trustline.account_id.clone();
                let asset = trustline.asset.clone();
                self.trustlines
                    .insert((account_id.clone(), asset.clone()), trustline);
                let ledger_key = LedgerKey::Trustline(LedgerKeyTrustLine { account_id, asset });
                self.record_entry_metadata(ledger_key, last_modified, has_sponsorship_ext, sponsor);
            }
            LedgerEntryData::Offer(_) => unreachable!("handled above"),
            LedgerEntryData::Data(data) => {
                let name = data_name_to_string(&data.data_name);
                let account_id = data.account_id.clone();
                let data_name = data.data_name.clone();
                self.data_entries
                    .entries_mut()
                    .insert((account_id.clone(), name), data);
                let ledger_key = LedgerKey::Data(LedgerKeyData {
                    account_id,
                    data_name,
                });
                self.record_entry_metadata(ledger_key, last_modified, has_sponsorship_ext, sponsor);
            }
            LedgerEntryData::ContractData(contract_data) => {
                let contract = contract_data.contract.clone();
                let cd_key = contract_data.key.clone();
                let durability = contract_data.durability;
                let key = StorageKey::new(contract.clone(), cd_key.clone(), durability);
                self.contract_data.entries_mut().insert(key, contract_data);
                let ledger_key = LedgerKey::ContractData(LedgerKeyContractData {
                    contract,
                    key: cd_key,
                    durability,
                });
                self.record_entry_metadata(ledger_key, last_modified, has_sponsorship_ext, sponsor);
            }
            LedgerEntryData::ContractCode(contract_code) => {
                let hash = contract_code.hash.clone();
                self.contract_code
                    .entries_mut()
                    .insert(hash.clone(), contract_code);
                let ledger_key = LedgerKey::ContractCode(LedgerKeyContractCode { hash });
                self.record_entry_metadata(ledger_key, last_modified, has_sponsorship_ext, sponsor);
            }
            LedgerEntryData::Ttl(ttl) => {
                let key_hash = ttl.key_hash.clone();
                // Capture the bucket list TTL value for Soroban.
                // Only capture if not already present - this ensures we keep the original
                // bucket list value even if the entry is reloaded later.
                self.ttl_bucket_list_snapshot
                    .entry(key_hash.clone())
                    .or_insert(ttl.live_until_ledger_seq);
                self.ttl_entries.insert(key_hash.clone(), ttl);
                let ledger_key = LedgerKey::Ttl(LedgerKeyTtl { key_hash });
                self.record_entry_metadata(ledger_key, last_modified, has_sponsorship_ext, sponsor);
            }
            LedgerEntryData::ClaimableBalance(cb) => {
                let balance_id = cb.balance_id.clone();
                self.claimable_balances
                    .entries_mut()
                    .insert(balance_id.clone(), cb);
                let ledger_key =
                    LedgerKey::ClaimableBalance(LedgerKeyClaimableBalance { balance_id });
                self.record_entry_metadata(ledger_key, last_modified, has_sponsorship_ext, sponsor);
            }
            LedgerEntryData::LiquidityPool(lp) => {
                let liquidity_pool_id = lp.liquidity_pool_id.clone();
                self.liquidity_pools
                    .entries_mut()
                    .insert(liquidity_pool_id.clone(), lp);
                let ledger_key =
                    LedgerKey::LiquidityPool(LedgerKeyLiquidityPool { liquidity_pool_id });
                self.record_entry_metadata(ledger_key, last_modified, has_sponsorship_ext, sponsor);
            }
            _ => {}
        }
    }

    /// Load a single entry into state WITHOUT setting up change tracking.
    /// This matches stellar-core's `loadWithoutRecord()` behavior.
    /// Use this for entries that only need existence checks, not modification tracking.
    ///
    /// IMPORTANT: Entries loaded this way will NOT appear in transaction meta changes
    /// unless they are subsequently accessed via `get_*_mut()` or `record_*_access()`.
    pub fn load_entry_without_snapshot(&mut self, entry: LedgerEntry) {
        let sponsor = sponsorship_from_entry_ext(&entry);
        let has_sponsorship_ext = matches!(entry.ext, LedgerEntryExt::V1(_));
        let last_modified = entry.last_modified_ledger_seq;
        match entry.data {
            LedgerEntryData::Account(account) => {
                let account_id = account.account_id.clone();
                // Insert account but do NOT save snapshot or mark as modified
                self.accounts.insert(account_id.clone(), account);
                let ledger_key = LedgerKey::Account(LedgerKeyAccount { account_id });
                self.record_entry_metadata(ledger_key, last_modified, has_sponsorship_ext, sponsor);
            }
            // For other entry types, delegate to regular load_entry since they don't
            // have the same snapshotting concern
            other => {
                let entry = LedgerEntry {
                    last_modified_ledger_seq: last_modified,
                    data: other,
                    ext: if has_sponsorship_ext {
                        LedgerEntryExt::V1(stellar_xdr::curr::LedgerEntryExtensionV1 {
                            sponsoring_id: SponsorshipDescriptor(sponsor),
                            ext: stellar_xdr::curr::LedgerEntryExtensionV1Ext::V0,
                        })
                    } else {
                        LedgerEntryExt::V0
                    },
                };
                self.load_entry(entry);
            }
        }
    }

    /// Load an account by ID and return a reference to it.
    ///
    /// This method is useful when you need to load an account from external storage
    /// and then access it.
    pub fn load_account(&mut self, account_id: &AccountId) -> Option<&AccountEntry> {
        self.accounts.get(account_id)
    }

    /// Get an account by ID (read-only).
    pub fn get_account(&self, account_id: &AccountId) -> Option<&AccountEntry> {
        self.accounts.get(account_id)
    }

    /// Get a mutable reference to an account by ID.
    ///
    /// This automatically tracks the modification for the delta.
    pub fn get_account_mut(&mut self, account_id: &AccountId) -> Option<&mut AccountEntry> {
        if self.accounts.contains_key(account_id) {
            // Save snapshot if not already saved or if it's None (for newly created entries).
            // For newly created entries, we update the snapshot to the current value so
            // subsequent operations can track changes with STATE/UPDATED pairs.
            // Rollback correctness is ensured by the created_accounts set.
            if !self
                .account_snapshots
                .get(account_id)
                .is_some_and(|s| s.is_some())
            {
                let snapshot = self.accounts.get(account_id).cloned();
                self.account_snapshots.insert(account_id.clone(), snapshot);
            }
            let ledger_key = LedgerKey::Account(LedgerKeyAccount {
                account_id: account_id.clone(),
            });
            self.capture_op_snapshot_for_key(&ledger_key);
            self.snapshot_last_modified_key(&ledger_key);
            // Track modification
            if !self.modified_accounts.contains(account_id) {
                self.modified_accounts.push(account_id.clone());
            }
            self.accounts.get_mut(account_id)
        } else {
            None
        }
    }

    /// Record that an account was accessed during operation execution.
    ///
    /// This captures an op snapshot for the account so it appears in the delta
    /// even if only read (not modified). This matches stellar-core behavior
    /// where `load()` records entries vs `loadWithoutRecord()` which doesn't.
    ///
    /// Use this when an operation loads an account that must appear in the
    /// transaction meta (e.g., issuer account in AllowTrust/SetTrustLineFlags).
    pub fn record_account_access(&mut self, account_id: &AccountId) {
        // Only record if account exists in state
        if !self.accounts.contains_key(account_id) {
            return;
        }
        // Save snapshot if not already saved (same as get_account_mut)
        if !self
            .account_snapshots
            .get(account_id)
            .is_some_and(|s| s.is_some())
        {
            let snapshot = self.accounts.get(account_id).cloned();
            self.account_snapshots.insert(account_id.clone(), snapshot);
        }
        let ledger_key = LedgerKey::Account(LedgerKeyAccount {
            account_id: account_id.clone(),
        });
        self.capture_op_snapshot_for_key(&ledger_key);
        self.snapshot_last_modified_key(&ledger_key);
        // Track as "modified" so it gets flushed to delta
        if !self.modified_accounts.contains(account_id) {
            self.modified_accounts.push(account_id.clone());
        }
    }

    /// Record that an entry was accessed during operation execution.
    ///
    /// This matches stellar-core's load-with-record behavior for operations that
    /// only read an entry but still commit an updated last-modified ledger.
    pub fn record_entry_access(&mut self, key: &LedgerKey) {
        match key {
            LedgerKey::Account(k) => {
                let _ = self.get_account_mut(&k.account_id);
            }
            LedgerKey::Trustline(k) => {
                let _ = self.get_trustline_by_trustline_asset_mut(&k.account_id, &k.asset);
            }
            LedgerKey::Offer(k) => {
                let _ = self.get_offer_mut(&k.seller_id, k.offer_id);
            }
            LedgerKey::Data(k) => {
                let name = data_name_to_string(&k.data_name);
                let _ = self.get_data_mut(&k.account_id, &name);
            }
            LedgerKey::ClaimableBalance(k) => {
                let _ = self.get_claimable_balance_mut(&k.balance_id);
            }
            LedgerKey::LiquidityPool(k) => {
                let _ = self.get_liquidity_pool_mut(&k.liquidity_pool_id);
            }
            _ => {}
        }
    }

    /// Create a new account entry.
    pub fn create_account(&mut self, entry: AccountEntry) {
        let account_id = entry.account_id.clone();
        let ledger_key = LedgerKey::Account(LedgerKeyAccount {
            account_id: account_id.clone(),
        });

        // Save snapshot (None because it didn't exist)
        self.account_snapshots
            .entry(account_id.clone())
            .or_insert(None);
        self.snapshot_last_modified_key(&ledger_key);
        self.set_last_modified_key(ledger_key, self.ledger_seq);

        // Record in delta
        let ledger_entry = self.account_to_ledger_entry(&entry);
        self.delta.record_create(ledger_entry);

        // Insert into state
        self.accounts.insert(account_id.clone(), entry);

        // Track that this entry was created in this transaction (for rollback)
        self.created_accounts.insert(account_id.clone());

        // Track modification
        if !self.modified_accounts.contains(&account_id) {
            self.modified_accounts.push(account_id);
        }
    }

    /// Update an existing account entry.
    pub fn update_account(&mut self, entry: AccountEntry) {
        let account_id = entry.account_id.clone();
        let ledger_key = LedgerKey::Account(LedgerKeyAccount {
            account_id: account_id.clone(),
        });

        // Save snapshot if not already saved (preserves original state from start of tx)
        if !self.account_snapshots.contains_key(&account_id) {
            let snapshot = self.accounts.get(&account_id).cloned();
            self.account_snapshots.insert(account_id.clone(), snapshot);
        }
        self.capture_op_snapshot_for_key(&ledger_key);
        self.snapshot_last_modified_key(&ledger_key);

        // Get pre-state (current value BEFORE this update)
        let pre_state = self
            .accounts
            .get(&account_id)
            .map(|acc| self.account_to_ledger_entry(acc));

        self.set_last_modified_key(ledger_key, self.ledger_seq);

        // Record in delta with pre-state
        let post_state = self.account_to_ledger_entry(&entry);
        if let Some(pre) = pre_state {
            self.delta.record_update(pre, post_state);
        }

        // Update state
        self.accounts.insert(account_id, entry);
    }

    /// Set an account entry directly without delta tracking.
    ///
    /// This is used during verification to sync state with CDP without
    /// affecting the delta computation for subsequent transactions.
    pub fn set_account_no_tracking(&mut self, entry: AccountEntry) {
        self.accounts.insert(entry.account_id.clone(), entry);
    }

    /// Add or update an account entry (convenience alias for set_account_no_tracking).
    ///
    /// Use this for setting up test state or initializing accounts.
    pub fn put_account(&mut self, entry: AccountEntry) {
        self.set_account_no_tracking(entry);
    }

    /// Apply a ledger entry directly without delta tracking.
    ///
    /// This is used during verification to sync state with CDP without
    /// affecting the delta computation for subsequent transactions.
    pub fn apply_entry_no_tracking(&mut self, entry: &stellar_xdr::curr::LedgerEntry) {
        use stellar_xdr::curr::LedgerEntryData;
        match &entry.data {
            LedgerEntryData::Account(acc) => {
                self.accounts.insert(acc.account_id.clone(), acc.clone());
            }
            LedgerEntryData::Trustline(tl) => {
                let key = (tl.account_id.clone(), tl.asset.clone());
                self.trustlines.insert(key, tl.clone());
            }
            LedgerEntryData::Offer(_) => {
                let mut store = self.offer_store_lock();
                store.insert_from_ledger_entry(entry);
            }
            LedgerEntryData::Data(data) => {
                let name = data_name_to_string(&data.data_name);
                let key = (data.account_id.clone(), name);
                self.data_entries.entries_mut().insert(key, data.clone());
            }
            LedgerEntryData::ClaimableBalance(cb) => {
                let key = cb.balance_id.clone();
                self.claimable_balances
                    .entries_mut()
                    .insert(key, cb.clone());
            }
            LedgerEntryData::LiquidityPool(lp) => {
                let key = lp.liquidity_pool_id.clone();
                self.liquidity_pools.entries_mut().insert(key, lp.clone());
            }
            LedgerEntryData::ContractData(cd) => {
                let key = StorageKey::new(cd.contract.clone(), cd.key.clone(), cd.durability);
                self.contract_data.entries_mut().insert(key, cd.clone());
            }
            LedgerEntryData::ContractCode(cc) => {
                let key = cc.hash.clone();
                self.contract_code.entries_mut().insert(key, cc.clone());
            }
            LedgerEntryData::Ttl(ttl) => {
                let key = ttl.key_hash.clone();
                // Capture the bucket list TTL value for Soroban.
                // Only capture if not already present - this ensures we keep the original
                // bucket list value even if the entry is reloaded later.
                self.ttl_bucket_list_snapshot
                    .entry(key.clone())
                    .or_insert(ttl.live_until_ledger_seq);
                self.ttl_entries.insert(key, ttl.clone());
            }
            LedgerEntryData::ConfigSetting(_) => {
                // Config settings not tracked
            }
        }
    }

    /// Delete a ledger entry directly without delta tracking.
    ///
    /// This is used during verification to sync state with CDP without
    /// affecting the delta computation for subsequent transactions.
    pub fn delete_entry_no_tracking(&mut self, key: &stellar_xdr::curr::LedgerKey) {
        use stellar_xdr::curr::LedgerKey;
        match key {
            LedgerKey::Account(k) => {
                self.accounts.remove(&k.account_id);
            }
            LedgerKey::Trustline(k) => {
                let key = (k.account_id.clone(), k.asset.clone());
                self.trustlines.remove(&key);
            }
            LedgerKey::Offer(k) => {
                let offer_key = OfferKey::new(k.seller_id.clone(), k.offer_id);
                let mut store = self.offer_store_lock();
                store.remove(&offer_key);
            }
            LedgerKey::Data(k) => {
                let name = data_name_to_string(&k.data_name);
                let data_key = (k.account_id.clone(), name);
                self.data_entries.entries_mut().remove(&data_key);
            }
            LedgerKey::ClaimableBalance(k) => {
                self.claimable_balances.entries_mut().remove(&k.balance_id);
            }
            LedgerKey::LiquidityPool(k) => {
                self.liquidity_pools
                    .entries_mut()
                    .remove(&k.liquidity_pool_id);
            }
            LedgerKey::ContractData(k) => {
                let cd_key = StorageKey::new(k.contract.clone(), k.key.clone(), k.durability);
                self.contract_data.entries_mut().remove(&cd_key);
            }
            LedgerKey::ContractCode(k) => {
                self.contract_code.entries_mut().remove(&k.hash);
            }
            LedgerKey::Ttl(k) => {
                self.ttl_entries.remove(&k.key_hash);
            }
            LedgerKey::ConfigSetting(_) => {
                // Config settings not tracked
            }
        }

        self.remove_entry_sponsorship(key);
        self.remove_sponsorship_ext(key);
        self.remove_last_modified(key);
    }

    /// Delete an account entry.
    pub fn delete_account(&mut self, account_id: &AccountId) {
        let ledger_key = LedgerKey::Account(LedgerKeyAccount {
            account_id: account_id.clone(),
        });

        // Save snapshot if not already saved
        if !self.account_snapshots.contains_key(account_id) {
            let snapshot = self.accounts.get(account_id).cloned();
            self.account_snapshots.insert(account_id.clone(), snapshot);
        }
        self.capture_op_snapshot_for_key(&ledger_key);
        self.snapshot_last_modified_key(&ledger_key);

        // Record delete with snapshot pre-state (not current mutated state)
        self.record_snapshot_delete(&ledger_key);

        // Remove from state
        self.clear_entry_sponsorship_metadata(&ledger_key);
        self.accounts.remove(account_id);
        self.remove_last_modified_key(&ledger_key);
    }

    /// Get a trustline by account and asset (read-only).
    pub fn get_trustline(&self, account_id: &AccountId, asset: &Asset) -> Option<&TrustLineEntry> {
        let asset_key = asset_to_trustline_asset(asset);
        self.trustlines.get(&(account_id.clone(), asset_key))
    }

    /// Get a trustline by account and trustline asset (read-only).
    pub fn get_trustline_by_trustline_asset(
        &self,
        account_id: &AccountId,
        asset: &TrustLineAsset,
    ) -> Option<&TrustLineEntry> {
        self.trustlines.get(&(account_id.clone(), asset.clone()))
    }

    /// Check if a trustline was already loaded during this transaction.
    pub fn is_trustline_tracked(&self, account_id: &AccountId, asset: &TrustLineAsset) -> bool {
        self.trustline_snapshots
            .contains_key(&(account_id.clone(), asset.clone()))
    }

    /// Iterate over all trustlines as (key, entry) pairs.
    ///
    /// WARNING: This only iterates over trustlines currently loaded in memory.
    /// For pool share trustline discovery, use `ensure_pool_share_trustlines_loaded`
    /// first to guarantee completeness.
    pub(crate) fn trustlines_iter(&self) -> impl Iterator<Item = (&TrustlineKey, &TrustLineEntry)> {
        self.trustlines.iter()
    }

    /// Ensure all pool share trustlines for an account are loaded into memory.
    ///
    /// Uses the `pool_share_tls_by_account_loader` to discover pool IDs from the
    /// secondary index, then loads the pool share trustlines and their associated
    /// liquidity pools via the `entry_loader`.  This mirrors the defense-in-depth
    /// pattern used by `remove_offers_by_account_and_asset`, which loads all
    /// matching offers from the authoritative store before iterating.
    ///
    /// After this call, `trustlines_iter()` is guaranteed to contain all pool share
    /// trustlines for the account (not just those loaded during prior TX execution).
    pub fn ensure_pool_share_trustlines_loaded(&mut self, account_id: &AccountId) -> Result<()> {
        // Query the secondary index for pool IDs.
        let pool_ids = if let Some(loader) = self.pool_share_tls_by_account_loader.take() {
            let result = loader(account_id);
            self.pool_share_tls_by_account_loader = Some(loader);
            result?
        } else {
            return Ok(());
        };

        if pool_ids.is_empty() {
            return Ok(());
        }

        let account_bytes = account_id.clone();

        // Load each pool share trustline and its liquidity pool if not already in memory.
        for pool_id in &pool_ids {
            let pool_share_asset_key = TrustLineAsset::PoolShare(pool_id.clone());

            // Load pool share trustline if not already tracked.
            if !self
                .trustlines
                .contains_key(&(account_bytes.clone(), pool_share_asset_key))
            {
                let tl_key = LedgerKey::Trustline(LedgerKeyTrustLine {
                    account_id: account_id.clone(),
                    asset: TrustLineAsset::PoolShare(pool_id.clone()),
                });
                if let Some(loader) = self.entry_loader.take() {
                    let result = loader(&tl_key);
                    self.entry_loader = Some(loader);
                    if let Some(entry) = result? {
                        self.load_entry(entry);
                    }
                }
            }

            // Load the liquidity pool if not already tracked.
            let pool_id_key = pool_id.clone();
            if !self.liquidity_pools.contains(&pool_id_key) {
                let pool_key = LedgerKey::LiquidityPool(LedgerKeyLiquidityPool {
                    liquidity_pool_id: pool_id.clone(),
                });
                if let Some(loader) = self.entry_loader.take() {
                    let result = loader(&pool_key);
                    self.entry_loader = Some(loader);
                    if let Some(entry) = result? {
                        self.load_entry(entry);
                    }
                }
            }
        }

        Ok(())
    }

    /// Get a mutable reference to a trustline by trustline asset.
    pub fn get_trustline_by_trustline_asset_mut(
        &mut self,
        account_id: &AccountId,
        asset: &TrustLineAsset,
    ) -> Option<&mut TrustLineEntry> {
        let key = (account_id.clone(), asset.clone());

        if self.trustlines.contains_key(&key) {
            // Save snapshot if not already saved or if it's None (for newly created entries).
            // For newly created entries, we update the snapshot to the current value so
            // subsequent operations can track changes with STATE/UPDATED pairs.
            // Rollback correctness is ensured by the created_trustlines set.
            if !self
                .trustline_snapshots
                .get(&key)
                .is_some_and(|s| s.is_some())
            {
                let snapshot = self.trustlines.get(&key).cloned();
                self.trustline_snapshots.insert(key.clone(), snapshot);
            }
            let ledger_key = LedgerKey::Trustline(LedgerKeyTrustLine {
                account_id: account_id.clone(),
                asset: asset.clone(),
            });
            self.capture_op_snapshot_for_key(&ledger_key);
            self.snapshot_last_modified_key(&ledger_key);
            // Track modification
            if !self.modified_trustlines.contains(&key) {
                self.modified_trustlines.push(key.clone());
            }
            self.trustlines.get_mut(&key)
        } else {
            None
        }
    }

    /// Get a mutable reference to a trustline.
    pub fn get_trustline_mut(
        &mut self,
        account_id: &AccountId,
        asset: &Asset,
    ) -> Option<&mut TrustLineEntry> {
        let asset_key = asset_to_trustline_asset(asset);
        let key = (account_id.clone(), asset_key);

        if self.trustlines.contains_key(&key) {
            // Save snapshot if not already saved or if it's None (for newly created entries).
            // For newly created entries, we update the snapshot to the current value so
            // subsequent operations can track changes with STATE/UPDATED pairs.
            // Rollback correctness is ensured by the created_trustlines set.
            if !self
                .trustline_snapshots
                .get(&key)
                .is_some_and(|s| s.is_some())
            {
                let snapshot = self.trustlines.get(&key).cloned();
                self.trustline_snapshots.insert(key.clone(), snapshot);
            }
            let ledger_key = LedgerKey::Trustline(LedgerKeyTrustLine {
                account_id: account_id.clone(),
                asset: asset_to_trustline_asset(asset),
            });
            self.capture_op_snapshot_for_key(&ledger_key);
            self.snapshot_last_modified_key(&ledger_key);
            // Track modification
            if !self.modified_trustlines.contains(&key) {
                self.modified_trustlines.push(key.clone());
            }
            self.trustlines.get_mut(&key)
        } else {
            None
        }
    }

    /// Create a new trustline entry.
    pub fn create_trustline(&mut self, entry: TrustLineEntry) {
        let key = (entry.account_id.clone(), entry.asset.clone());
        let ledger_key = LedgerKey::Trustline(LedgerKeyTrustLine {
            account_id: entry.account_id.clone(),
            asset: entry.asset.clone(),
        });

        // Save snapshot (None because it didn't exist)
        if !self.trustline_snapshots.contains_key(&key) {
            self.trustline_snapshots.insert(key.clone(), None);
        }
        self.snapshot_last_modified_key(&ledger_key);
        self.set_last_modified_key(ledger_key, self.ledger_seq);

        // Record in delta
        let ledger_entry = self.trustline_to_ledger_entry(&entry);
        self.delta.record_create(ledger_entry);

        // Insert into state
        self.trustlines.insert(key.clone(), entry);

        // Track that this entry was created in this transaction (for rollback)
        self.created_trustlines.insert(key.clone());

        // Track modification
        if !self.modified_trustlines.contains(&key) {
            self.modified_trustlines.push(key);
        }
    }

    /// Update an existing trustline entry.
    pub fn update_trustline(&mut self, entry: TrustLineEntry) {
        let key = (entry.account_id.clone(), entry.asset.clone());
        let ledger_key = LedgerKey::Trustline(LedgerKeyTrustLine {
            account_id: entry.account_id.clone(),
            asset: entry.asset.clone(),
        });

        // Save snapshot if not already saved
        if !self.trustline_snapshots.contains_key(&key) {
            let snapshot = self.trustlines.get(&key).cloned();
            self.trustline_snapshots.insert(key.clone(), snapshot);
        }
        self.capture_op_snapshot_for_key(&ledger_key);
        self.snapshot_last_modified_key(&ledger_key);

        // Get pre-state (current value BEFORE this update)
        let pre_state = self
            .trustlines
            .get(&key)
            .map(|tl| self.trustline_to_ledger_entry(tl));

        self.set_last_modified_key(ledger_key, self.ledger_seq);

        // Record in delta with pre-state
        let post_state = self.trustline_to_ledger_entry(&entry);
        if let Some(pre) = pre_state {
            self.delta.record_update(pre, post_state);
        }

        // Update state
        self.trustlines.insert(key, entry);

        // Do NOT add to modified_trustlines since we already recorded the update.
        // This prevents flush_modified_entries from recording a duplicate.
        // Classic operations use get_trustline_mut() which tracks modifications separately.
    }

    /// Delete a trustline entry.
    pub fn delete_trustline(&mut self, account_id: &AccountId, asset: &Asset) {
        let asset_key = asset_to_trustline_asset(asset);
        let key = (account_id.clone(), asset_key.clone());
        let ledger_key = LedgerKey::Trustline(LedgerKeyTrustLine {
            account_id: account_id.clone(),
            asset: asset_key,
        });

        // Save snapshot if not already saved
        if !self.trustline_snapshots.contains_key(&key) {
            let snapshot = self.trustlines.get(&key).cloned();
            self.trustline_snapshots.insert(key.clone(), snapshot);
        }
        self.capture_op_snapshot_for_key(&ledger_key);
        self.snapshot_last_modified_key(&ledger_key);

        // Record delete with snapshot pre-state (not current mutated state)
        self.record_snapshot_delete(&ledger_key);

        // Remove from state
        self.clear_entry_sponsorship_metadata(&ledger_key);
        self.trustlines.remove(&key);
        self.remove_last_modified_key(&ledger_key);
    }

    /// Delete a trustline entry by trustline asset.
    pub fn delete_trustline_by_trustline_asset(
        &mut self,
        account_id: &AccountId,
        asset: &TrustLineAsset,
    ) {
        let key = (account_id.clone(), asset.clone());
        let ledger_key = LedgerKey::Trustline(LedgerKeyTrustLine {
            account_id: account_id.clone(),
            asset: asset.clone(),
        });

        // Save snapshot if not already saved
        if !self.trustline_snapshots.contains_key(&key) {
            let snapshot = self.trustlines.get(&key).cloned();
            self.trustline_snapshots.insert(key.clone(), snapshot);
        }
        self.capture_op_snapshot_for_key(&ledger_key);
        self.snapshot_last_modified_key(&ledger_key);

        // Record delete with snapshot pre-state (not current mutated state)
        self.record_snapshot_delete(&ledger_key);

        // Remove from state
        self.clear_entry_sponsorship_metadata(&ledger_key);
        self.trustlines.remove(&key);
        self.remove_last_modified_key(&ledger_key);
    }

    /// Get the top-N offer keys (cheapest first) for an asset pair.
    pub fn top_n_offer_keys(&self, buying: &Asset, selling: &Asset, n: usize) -> Vec<OfferKey> {
        let store = self.offer_store_lock();
        store.top_n_offer_keys(buying, selling, n)
    }

    /// Get an offer by its key (read-only).
    pub fn get_offer_by_key(&self, key: &OfferKey) -> Option<OfferEntry> {
        let store = self.offer_store_lock();
        store.get_offer(key).cloned()
    }

    /// Get an offer by seller and offer ID (read-only).
    pub fn get_offer(&self, seller_id: &AccountId, offer_id: i64) -> Option<OfferEntry> {
        let store = self.offer_store_lock();
        store
            .get_by_seller(seller_id, offer_id)
            .map(|r| r.entry.clone())
    }

    /// Check if an offer was already loaded during this transaction.
    pub fn is_offer_tracked(&self, seller_id: &AccountId, offer_id: i64) -> bool {
        self.offer_snapshots
            .contains_key(&OfferKey::new(seller_id.clone(), offer_id))
    }

    /// Get all offers for an account that buy or sell a specific asset.
    ///
    /// Reads from the canonical OfferStore which maintains its own secondary index.
    pub fn get_offers_by_account_and_asset(
        &self,
        account_id: &AccountId,
        asset: &Asset,
    ) -> Vec<OfferEntry> {
        let store = self.offer_store_lock();
        store.get_offers_by_account_and_asset(account_id, asset)
    }

    /// Get a clone of an offer for mutation. Caller must call `update_offer()` to write back.
    ///
    /// This also snapshots the offer for rollback and tracks modification.
    pub fn get_offer_mut(&mut self, seller_id: &AccountId, offer_id: i64) -> Option<OfferEntry> {
        let key = OfferKey::new(seller_id.clone(), offer_id);

        let record = {
            let store = self.offer_store_lock();
            store.get(&key)?.clone()
        };

        // Save snapshot if not already saved or if it's None (for newly created entries).
        if !self.offer_snapshots.get(&key).is_some_and(|s| s.is_some()) {
            self.offer_snapshots
                .insert(key.clone(), Some(record.clone()));
        }
        let ledger_key = LedgerKey::Offer(LedgerKeyOffer {
            seller_id: seller_id.clone(),
            offer_id,
        });
        self.capture_op_snapshot_for_key(&ledger_key);
        self.snapshot_last_modified_key(&ledger_key);
        // Track modification
        if !self.modified_offers.contains(&key) {
            self.modified_offers.push(key);
        }

        Some(record.entry)
    }

    /// Create a new offer entry.
    pub fn create_offer(&mut self, entry: OfferEntry) {
        let key = OfferKey::from_offer(&entry);
        let ledger_key = LedgerKey::Offer(LedgerKeyOffer {
            seller_id: entry.seller_id.clone(),
            offer_id: entry.offer_id,
        });

        // Save snapshot (None because it didn't exist)
        self.offer_snapshots.entry(key.clone()).or_insert(None);
        self.snapshot_last_modified_key(&ledger_key);

        // Pick up any pre-set sponsorship (set before create_offer via
        // apply_entry_sponsorship_with_sponsor, which falls back to
        // entry_sponsorships when the offer doesn't exist yet).
        let sponsor = self.entry_sponsorships.remove(&ledger_key);
        let has_ext = sponsor.is_some() || self.entry_sponsorship_ext.remove(&ledger_key);

        // Build record and insert into canonical offer store
        let record = OfferRecord {
            entry,
            last_modified: self.ledger_seq,
            sponsor,
            has_ext,
        };
        let ledger_entry = record.to_ledger_entry();
        self.delta.record_create(ledger_entry);

        {
            let mut store = self.offer_store_lock();
            store.insert_record(record);
        }

        // Track that this entry was created in this transaction (for rollback)
        self.created_offers.insert(key.clone());

        // Track modification
        if !self.modified_offers.contains(&key) {
            self.modified_offers.push(key);
        }
    }

    /// Update an existing offer entry.
    pub fn update_offer(&mut self, entry: OfferEntry) {
        let key = OfferKey::from_offer(&entry);
        let ledger_key = LedgerKey::Offer(LedgerKeyOffer {
            seller_id: entry.seller_id.clone(),
            offer_id: entry.offer_id,
        });

        // Save snapshot if not already saved (for rollback purposes)
        if !self.offer_snapshots.contains_key(&key) {
            let snapshot = self.offer_store_lock().get(&key).cloned();
            self.offer_snapshots.insert(key.clone(), snapshot);
        }
        self.capture_op_snapshot_for_key(&ledger_key);
        self.snapshot_last_modified_key(&ledger_key);

        // Get pre-state and existing metadata from canonical store
        let (pre_state, mut new_record) = {
            let store = self.offer_store_lock();
            let pre = store.get(&key).map(|r| r.to_ledger_entry());
            let rec = store.get(&key).cloned().unwrap_or(OfferRecord {
                entry: entry.clone(),
                last_modified: self.ledger_seq,
                sponsor: None,
                has_ext: false,
            });
            (pre, rec)
        };

        new_record.entry = entry;
        new_record.last_modified = self.ledger_seq;
        let post_state = new_record.to_ledger_entry();

        // Record in delta - each update gets its own STATE/UPDATED pair
        if let Some(pre) = pre_state {
            self.delta.record_update(pre, post_state);
        }

        // Update canonical store (handles index maintenance internally)
        {
            let mut store = self.offer_store_lock();
            store.insert_record(new_record);
        }

        // Do NOT track in modified_offers since we already recorded the update
        // This prevents flush_modified_entries from recording a duplicate
    }

    /// Delete an offer entry.
    pub fn delete_offer(&mut self, seller_id: &AccountId, offer_id: i64) {
        let key = OfferKey::new(seller_id.clone(), offer_id);
        let ledger_key = LedgerKey::Offer(LedgerKeyOffer {
            seller_id: seller_id.clone(),
            offer_id,
        });

        // Save snapshot if not already saved
        if !self.offer_snapshots.contains_key(&key) {
            let snapshot = self.offer_store_lock().get(&key).cloned();
            self.offer_snapshots.insert(key.clone(), snapshot);
        }
        self.capture_op_snapshot_for_key(&ledger_key);
        self.snapshot_last_modified_key(&ledger_key);

        // Record delete with snapshot pre-state (not current mutated state)
        self.record_snapshot_delete(&ledger_key);

        // Remove from canonical store (handles index cleanup internally)
        {
            let mut store = self.offer_store_lock();
            store.remove(&key);
        }
        // Note: metadata is inline in OfferRecord — removed with the offer.
    }

    /// Get the best offer for a buying/selling pair (lowest price, then offer ID).
    ///
    /// Uses the OfferStore's order book index for O(log n) lookup.
    pub fn best_offer(&self, buying: &Asset, selling: &Asset) -> Option<OfferEntry> {
        let store = self.offer_store_lock();
        store.best_offer(buying, selling).cloned()
    }

    /// Get the best offer for a buying/selling pair with an additional filter.
    ///
    /// Uses the OfferStore's order book index for efficient traversal in price order.
    pub fn best_offer_filtered<F>(
        &self,
        buying: &Asset,
        selling: &Asset,
        keep: F,
    ) -> Option<OfferEntry>
    where
        F: FnMut(&OfferEntry) -> bool,
    {
        let store = self.offer_store_lock();
        store.best_offer_filtered(buying, selling, keep)
    }

    /// Check if offers exist for a specific asset pair.
    pub fn has_offers_for_pair(&self, buying: &Asset, selling: &Asset) -> bool {
        let store = self.offer_store_lock();
        store.has_offers_for_pair(buying, selling)
    }

    /// Get all offers for a specific buying/selling asset pair.
    ///
    /// Returns cloned OfferEntry values for each offer in the pair's order book.
    pub fn offers_for_asset_pair(&self, buying: &Asset, selling: &Asset) -> Vec<OfferEntry> {
        let store = self.offer_store_lock();
        store.offers_for_asset_pair(buying, selling)
    }

    /// Get the number of offers in the index.
    pub fn offer_index_size(&self) -> usize {
        let store = self.offer_store_lock();
        store.offer_index_size()
    }

    /// Get the number of unique asset pairs with offers.
    pub fn offer_index_num_pairs(&self) -> usize {
        let store = self.offer_store_lock();
        store.num_asset_pairs()
    }

    /// Remove all offers owned by an account that are buying or selling a specific asset.
    /// This is used when revoking authorization on a trustline.
    /// Returns the list of OfferEntry that were removed (before deletion) so callers can
    /// handle liability release, subentry updates, and sponsorship adjustments.
    ///
    /// The canonical OfferStore contains ALL offers, so no authoritative loader is needed.
    pub fn remove_offers_by_account_and_asset(
        &mut self,
        account_id: &AccountId,
        asset: &Asset,
    ) -> Vec<OfferEntry> {
        // Get matching offers from the canonical store
        let offers_to_remove: Vec<OfferEntry> = {
            let store = self.offer_store_lock();
            store
                .get_offers_by_account_and_asset(account_id, asset)
                .into_iter()
                .filter(|offer| {
                    // Skip offers already deleted in this ledger
                    let ledger_key = LedgerKey::Offer(LedgerKeyOffer {
                        seller_id: offer.seller_id.clone(),
                        offer_id: offer.offer_id,
                    });
                    !self.delta.deleted_keys().contains(&ledger_key)
                })
                .collect()
        };

        // Remove each offer
        for offer in &offers_to_remove {
            self.delete_offer(&offer.seller_id, offer.offer_id);
        }

        offers_to_remove
    }

    /// Get a data entry by account and name (read-only).
    pub fn get_data(&self, account_id: &AccountId, name: &str) -> Option<&DataEntry> {
        self.data_entries
            .get(&(account_id.clone(), name.to_string()))
    }

    /// Check if a data entry was already loaded during this transaction.
    pub fn is_data_tracked(&self, account_id: &AccountId, name: &str) -> bool {
        self.data_entries
            .is_tracked(&(account_id.clone(), name.to_string()))
    }

    /// Get a mutable reference to a data entry.
    pub fn get_data_mut(&mut self, account_id: &AccountId, name: &str) -> Option<&mut DataEntry> {
        let key = (account_id.clone(), name.to_string());
        if !self.data_entries.contains(&key) {
            return None;
        }
        self.data_entries.ensure_snapshot(&key);
        // Build ledger key from stored entry (Data uses name from entry, not from caller).
        if let Some(entry) = self.data_entries.get(&key) {
            let ledger_key = LedgerKey::Data(LedgerKeyData {
                account_id: entry.account_id.clone(),
                data_name: entry.data_name.clone(),
            });
            self.capture_op_snapshot_for_key(&ledger_key);
            self.snapshot_last_modified_key(&ledger_key);
        }
        self.data_entries.get_mut_tracked(&key)
    }

    /// Create a new data entry.
    pub fn create_data(&mut self, entry: DataEntry) {
        let name = data_name_to_string(&entry.data_name);
        tracing::debug!(
            "create_data: account_id={:?}, name={:?}, name_bytes={:?}",
            &entry.account_id,
            name,
            entry.data_name.as_vec()
        );
        let key = (entry.account_id.clone(), name);
        let ledger_key = LedgerKey::Data(LedgerKeyData {
            account_id: entry.account_id.clone(),
            data_name: entry.data_name.clone(),
        });
        self.snapshot_last_modified_key(&ledger_key);
        self.set_last_modified_key(ledger_key, self.ledger_seq);
        let ledger_entry = self.data_to_ledger_entry(&entry);
        self.delta.record_create(ledger_entry);
        self.data_entries.insert_created(key, entry);
    }

    /// Update an existing data entry.
    pub fn update_data(&mut self, entry: DataEntry) {
        let name = data_name_to_string(&entry.data_name);
        let key = (entry.account_id.clone(), name);
        let ledger_key = LedgerKey::Data(LedgerKeyData {
            account_id: entry.account_id.clone(),
            data_name: entry.data_name.clone(),
        });
        self.data_entries.ensure_snapshot_on_first(&key);
        self.capture_op_snapshot_for_key(&ledger_key);
        self.snapshot_last_modified_key(&ledger_key);
        let pre_state = self
            .data_entries
            .get(&key)
            .map(|data| self.data_to_ledger_entry(data));
        self.set_last_modified_key(ledger_key, self.ledger_seq);
        let post_state = self.data_to_ledger_entry(&entry);
        if let Some(pre) = pre_state {
            self.delta.record_update(pre, post_state);
        }
        self.data_entries.insert_updated(key, entry, false);
    }

    /// Delete a data entry.
    pub fn delete_data(&mut self, account_id: &AccountId, name: &str) {
        let key = (account_id.clone(), name.to_string());
        self.data_entries.ensure_snapshot_on_first(&key);

        // Build ledger key from stored entry (Data constructs LedgerKey from entry fields).
        // Clone the entry to avoid borrow checker issues.
        if let Some(entry) = self.data_entries.get(&key).cloned() {
            let ledger_key = LedgerKey::Data(LedgerKeyData {
                account_id: account_id.clone(),
                data_name: entry.data_name.clone(),
            });
            self.capture_op_snapshot_for_key(&ledger_key);
            self.snapshot_last_modified_key(&ledger_key);
            self.record_snapshot_delete(&ledger_key);
            self.clear_entry_sponsorship_metadata(&ledger_key);
            self.data_entries.remove_deleted(&key, false, false);
            self.remove_last_modified_key(&ledger_key);
        }
    }

    /// Get a contract data entry by key (read-only).
    pub fn get_contract_data(
        &self,
        contract: &ScAddress,
        key: &ScVal,
        durability: ContractDataDurability,
    ) -> Option<&ContractDataEntry> {
        let lookup_key = StorageKey::new(contract.clone(), key.clone(), durability);
        self.contract_data.get(&lookup_key)
    }

    /// Get a mutable reference to a contract data entry.
    pub fn get_contract_data_mut(
        &mut self,
        contract: &ScAddress,
        key: &ScVal,
        durability: ContractDataDurability,
    ) -> Option<&mut ContractDataEntry> {
        let lookup_key = StorageKey::new(contract.clone(), key.clone(), durability);
        if !self.contract_data.contains(&lookup_key) {
            return None;
        }
        self.contract_data.ensure_snapshot(&lookup_key);
        let ledger_key = LedgerKey::ContractData(LedgerKeyContractData {
            contract: contract.clone(),
            key: key.clone(),
            durability,
        });
        self.capture_op_snapshot_for_key(&ledger_key);
        self.snapshot_last_modified_key(&ledger_key);
        self.contract_data.get_mut_tracked(&lookup_key)
    }

    /// Create a new contract data entry.
    pub fn create_contract_data(&mut self, entry: ContractDataEntry) {
        let key = StorageKey::new(entry.contract.clone(), entry.key.clone(), entry.durability);
        let ledger_key = LedgerKey::ContractData(LedgerKeyContractData {
            contract: entry.contract.clone(),
            key: entry.key.clone(),
            durability: entry.durability,
        });
        self.snapshot_last_modified_key(&ledger_key);
        self.set_last_modified_key(ledger_key, self.ledger_seq);
        let ledger_entry = self.contract_data_to_ledger_entry(&entry);
        self.delta.record_create(ledger_entry);
        self.contract_data.insert_created(key, entry);
    }

    /// Update an existing contract data entry.
    pub fn update_contract_data(&mut self, entry: ContractDataEntry) {
        let key = StorageKey::new(entry.contract.clone(), entry.key.clone(), entry.durability);
        let ledger_key = LedgerKey::ContractData(LedgerKeyContractData {
            contract: entry.contract.clone(),
            key: entry.key.clone(),
            durability: entry.durability,
        });
        self.contract_data.ensure_snapshot_on_first(&key);
        self.capture_op_snapshot_for_key(&ledger_key);
        self.snapshot_last_modified_key(&ledger_key);
        let pre_state = self
            .contract_data
            .get(&key)
            .map(|cd| self.contract_data_to_ledger_entry(cd));
        self.set_last_modified_key(ledger_key, self.ledger_seq);
        let post_state = self.contract_data_to_ledger_entry(&entry);
        if let Some(pre) = pre_state {
            self.delta.record_update(pre, post_state);
        }
        self.contract_data.insert_updated(key, entry, false);
    }

    /// Delete a contract data entry.
    pub fn delete_contract_data(
        &mut self,
        contract: &ScAddress,
        key: &ScVal,
        durability: ContractDataDurability,
    ) {
        let lookup_key = StorageKey::new(contract.clone(), key.clone(), durability);
        let ledger_key = LedgerKey::ContractData(LedgerKeyContractData {
            contract: contract.clone(),
            key: key.clone(),
            durability,
        });
        self.contract_data.ensure_snapshot_on_first(&lookup_key);
        self.capture_op_snapshot_for_key(&ledger_key);
        self.snapshot_last_modified_key(&ledger_key);
        self.record_snapshot_delete(&ledger_key);
        self.clear_entry_sponsorship_metadata(&ledger_key);
        self.contract_data.remove_deleted(&lookup_key, true, false);
        self.remove_last_modified_key(&ledger_key);
    }

    /// Get a contract code entry by hash (read-only).
    pub fn get_contract_code(&self, hash: &Hash) -> Option<&ContractCodeEntry> {
        self.contract_code.get(hash)
    }

    /// Get a mutable reference to a contract code entry.
    pub fn get_contract_code_mut(&mut self, hash: &Hash) -> Option<&mut ContractCodeEntry> {
        let key = hash.clone();
        if !self.contract_code.contains(&key) {
            return None;
        }
        self.contract_code.ensure_snapshot(&key);
        let ledger_key = LedgerKey::ContractCode(LedgerKeyContractCode { hash: hash.clone() });
        self.capture_op_snapshot_for_key(&ledger_key);
        self.snapshot_last_modified_key(&ledger_key);
        self.contract_code.get_mut_tracked(&key)
    }

    /// Create a new contract code entry.
    pub fn create_contract_code(&mut self, entry: ContractCodeEntry) {
        let key = entry.hash.clone();
        let ledger_key = LedgerKey::ContractCode(LedgerKeyContractCode {
            hash: entry.hash.clone(),
        });
        self.snapshot_last_modified_key(&ledger_key);
        self.set_last_modified_key(ledger_key, self.ledger_seq);
        let ledger_entry = self.contract_code_to_ledger_entry(&entry);
        self.delta.record_create(ledger_entry);
        self.contract_code.insert_created(key, entry);
    }

    /// Update an existing contract code entry.
    pub fn update_contract_code(&mut self, entry: ContractCodeEntry) {
        let key = entry.hash.clone();
        let ledger_key = LedgerKey::ContractCode(LedgerKeyContractCode {
            hash: entry.hash.clone(),
        });
        self.contract_code.ensure_snapshot_on_first(&key);
        self.capture_op_snapshot_for_key(&ledger_key);
        self.snapshot_last_modified_key(&ledger_key);
        let pre_state = self
            .contract_code
            .get(&key)
            .map(|cc| self.contract_code_to_ledger_entry(cc));
        self.set_last_modified_key(ledger_key, self.ledger_seq);
        let post_state = self.contract_code_to_ledger_entry(&entry);
        if let Some(pre) = pre_state {
            self.delta.record_update(pre, post_state);
        }
        self.contract_code.insert_updated(key, entry, false);
    }

    /// Delete a contract code entry.
    pub fn delete_contract_code(&mut self, hash: &Hash) {
        let key = hash.clone();
        let ledger_key = LedgerKey::ContractCode(LedgerKeyContractCode { hash: hash.clone() });
        self.contract_code.ensure_snapshot_on_first(&key);
        self.capture_op_snapshot_for_key(&ledger_key);
        self.snapshot_last_modified_key(&ledger_key);
        self.record_snapshot_delete(&ledger_key);
        self.clear_entry_sponsorship_metadata(&ledger_key);
        self.contract_code.remove_deleted(&key, true, false);
        self.remove_last_modified_key(&ledger_key);
    }

    /// Get a claimable balance by ID (read-only).
    pub fn get_claimable_balance(
        &self,
        balance_id: &ClaimableBalanceId,
    ) -> Option<&ClaimableBalanceEntry> {
        self.claimable_balances.get(balance_id)
    }

    /// Check if a claimable balance was already loaded during this transaction.
    pub fn is_claimable_balance_tracked(&self, balance_id: &ClaimableBalanceId) -> bool {
        self.claimable_balances.is_tracked(balance_id)
    }

    /// Get a mutable reference to a claimable balance entry.
    pub fn get_claimable_balance_mut(
        &mut self,
        balance_id: &ClaimableBalanceId,
    ) -> Option<&mut ClaimableBalanceEntry> {
        if !self.claimable_balances.contains(balance_id) {
            return None;
        }
        self.claimable_balances.ensure_snapshot(balance_id);
        let ledger_key = LedgerKey::ClaimableBalance(LedgerKeyClaimableBalance {
            balance_id: balance_id.clone(),
        });
        self.capture_op_snapshot_for_key(&ledger_key);
        self.snapshot_last_modified_key(&ledger_key);
        self.claimable_balances.get_mut_tracked(balance_id)
    }

    /// Create a new claimable balance entry.
    pub fn create_claimable_balance(&mut self, entry: ClaimableBalanceEntry) {
        let key = entry.balance_id.clone();
        let ledger_key = LedgerKey::ClaimableBalance(LedgerKeyClaimableBalance {
            balance_id: entry.balance_id.clone(),
        });
        self.snapshot_last_modified_key(&ledger_key);
        self.set_last_modified_key(ledger_key, self.ledger_seq);
        let ledger_entry = self.claimable_balance_to_ledger_entry(&entry);
        self.delta.record_create(ledger_entry);
        self.claimable_balances.insert_created(key, entry);
    }

    /// Delete a claimable balance entry (when claimed).
    pub fn delete_claimable_balance(&mut self, balance_id: &ClaimableBalanceId) {
        let key = balance_id.clone();
        let ledger_key = LedgerKey::ClaimableBalance(LedgerKeyClaimableBalance {
            balance_id: balance_id.clone(),
        });
        self.claimable_balances.ensure_snapshot_on_first(&key);
        self.capture_op_snapshot_for_key(&ledger_key);
        self.snapshot_last_modified_key(&ledger_key);
        self.record_snapshot_delete(&ledger_key);
        self.clear_entry_sponsorship_metadata(&ledger_key);
        self.claimable_balances.remove_deleted(&key, false, false);
        self.remove_last_modified_key(&ledger_key);
    }

    /// Update an existing claimable balance entry.
    pub fn update_claimable_balance(&mut self, entry: ClaimableBalanceEntry) {
        let key = entry.balance_id.clone();
        let ledger_key = LedgerKey::ClaimableBalance(LedgerKeyClaimableBalance {
            balance_id: entry.balance_id.clone(),
        });
        self.claimable_balances.ensure_snapshot_on_first(&key);
        self.capture_op_snapshot_for_key(&ledger_key);
        self.snapshot_last_modified_key(&ledger_key);
        let pre_state = self
            .claimable_balances
            .get(&key)
            .map(|e| self.claimable_balance_to_ledger_entry(e));
        self.set_last_modified_key(ledger_key, self.ledger_seq);
        let post_state = self.claimable_balance_to_ledger_entry(&entry);
        if let Some(pre) = pre_state {
            self.delta.record_update(pre, post_state);
        }
        self.claimable_balances.insert_updated(key, entry, false);
    }

    /// Get a liquidity pool by ID (read-only).
    pub fn get_liquidity_pool(&self, pool_id: &PoolId) -> Option<&LiquidityPoolEntry> {
        self.liquidity_pools.get(pool_id)
    }

    /// Check if a liquidity pool was already loaded during this transaction.
    pub fn is_liquidity_pool_tracked(&self, pool_id: &PoolId) -> bool {
        self.liquidity_pools.is_tracked(pool_id)
    }

    /// Get a mutable reference to a liquidity pool.
    pub fn get_liquidity_pool_mut(&mut self, pool_id: &PoolId) -> Option<&mut LiquidityPoolEntry> {
        if !self.liquidity_pools.contains(pool_id) {
            return None;
        }
        self.liquidity_pools.ensure_snapshot(pool_id);
        let ledger_key = LedgerKey::LiquidityPool(LedgerKeyLiquidityPool {
            liquidity_pool_id: pool_id.clone(),
        });
        self.capture_op_snapshot_for_key(&ledger_key);
        self.snapshot_last_modified_key(&ledger_key);
        self.liquidity_pools.get_mut_tracked(pool_id)
    }

    /// Create a new liquidity pool entry.
    pub fn create_liquidity_pool(&mut self, entry: LiquidityPoolEntry) {
        let key = entry.liquidity_pool_id.clone();
        let ledger_key = LedgerKey::LiquidityPool(LedgerKeyLiquidityPool {
            liquidity_pool_id: entry.liquidity_pool_id.clone(),
        });
        self.snapshot_last_modified_key(&ledger_key);
        self.set_last_modified_key(ledger_key, self.ledger_seq);
        let ledger_entry = self.liquidity_pool_to_ledger_entry(&entry);
        self.delta.record_create(ledger_entry);
        self.liquidity_pools.insert_created(key, entry);
    }

    /// Update an existing liquidity pool entry.
    pub fn update_liquidity_pool(&mut self, entry: LiquidityPoolEntry) {
        let key = entry.liquidity_pool_id.clone();
        let ledger_key = LedgerKey::LiquidityPool(LedgerKeyLiquidityPool {
            liquidity_pool_id: entry.liquidity_pool_id.clone(),
        });
        self.liquidity_pools.ensure_snapshot_on_first(&key);
        self.capture_op_snapshot_for_key(&ledger_key);
        self.snapshot_last_modified_key(&ledger_key);
        let pre_state = self
            .liquidity_pools
            .get(&key)
            .map(|e| self.liquidity_pool_to_ledger_entry(e));
        self.set_last_modified_key(ledger_key, self.ledger_seq);
        let post_state = self.liquidity_pool_to_ledger_entry(&entry);
        if let Some(pre) = pre_state {
            self.delta.record_update(pre, post_state);
        }
        self.liquidity_pools.insert_updated(key, entry, true);
    }

    /// Delete a liquidity pool entry (when pool_shares_trust_line_count reaches 0).
    pub fn delete_liquidity_pool(&mut self, pool_id: &PoolId) {
        let key = pool_id.clone();
        let ledger_key = LedgerKey::LiquidityPool(LedgerKeyLiquidityPool {
            liquidity_pool_id: pool_id.clone(),
        });
        self.liquidity_pools.ensure_snapshot_on_first(&key);
        self.capture_op_snapshot_for_key(&ledger_key);
        self.snapshot_last_modified_key(&ledger_key);
        self.record_snapshot_delete(&ledger_key);
        self.clear_entry_sponsorship_metadata(&ledger_key);
        self.liquidity_pools.remove_deleted(&key, false, true);
        self.remove_last_modified_key(&ledger_key);
    }

    /// Get an entry by LedgerKey (read-only).
    pub fn get_entry(&self, key: &LedgerKey) -> Option<LedgerEntry> {
        match key {
            LedgerKey::Account(k) => self
                .get_account(&k.account_id)
                .map(|e| self.account_to_ledger_entry(e)),
            LedgerKey::Trustline(k) => self
                .trustlines
                .get(&(k.account_id.clone(), k.asset.clone()))
                .map(|e| self.trustline_to_ledger_entry(e)),
            LedgerKey::Offer(k) => self
                .get_offer(&k.seller_id, k.offer_id)
                .map(|e| self.offer_to_ledger_entry(&e)),
            LedgerKey::Data(k) => {
                let name = data_name_to_string(&k.data_name);
                let result = self.get_data(&k.account_id, &name);
                tracing::debug!(
                    "get_entry for Data: account={:?}, name={:?}, name_bytes={:?}, found={}",
                    &k.account_id,
                    name,
                    k.data_name.as_vec(),
                    result.is_some()
                );
                result.map(|e| self.data_to_ledger_entry(e))
            }
            LedgerKey::ContractData(k) => self
                .get_contract_data(&k.contract, &k.key, k.durability)
                .map(|e| self.contract_data_to_ledger_entry(e)),
            LedgerKey::ContractCode(k) => self
                .get_contract_code(&k.hash)
                .map(|e| self.contract_code_to_ledger_entry(e)),
            LedgerKey::Ttl(k) => self
                .get_ttl(&k.key_hash)
                .map(|e| self.ttl_to_ledger_entry(e)),
            LedgerKey::ClaimableBalance(k) => self
                .get_claimable_balance(&k.balance_id)
                .map(|e| self.claimable_balance_to_ledger_entry(e)),
            LedgerKey::LiquidityPool(k) => self
                .get_liquidity_pool(&k.liquidity_pool_id)
                .map(|e| self.liquidity_pool_to_ledger_entry(e)),
            _ => None,
        }
    }

    /// Check if an entry was deleted during this ledger (for Soroban entries).
    ///
    /// This is used to prevent reloading deleted entries from the bucket list.
    /// In stellar-core, deleted entries are tracked in mThreadEntryMap as nullopt,
    /// which prevents subsequent transactions from seeing them. This method provides
    /// equivalent functionality.
    pub fn is_entry_deleted(&self, key: &LedgerKey) -> bool {
        match key {
            LedgerKey::ContractData(k) => {
                let lookup_key = StorageKey::new(k.contract.clone(), k.key.clone(), k.durability);
                self.contract_data.is_deleted(&lookup_key)
            }
            LedgerKey::ContractCode(k) => self.contract_code.is_deleted(&k.hash),
            LedgerKey::Ttl(k) => self.deleted_ttl.contains(&k.key_hash),
            _ => false,
        }
    }

    /// Mark an entry as deleted without requiring it to be in the state.
    ///
    /// Used to propagate deletion information from prior stages in parallel
    /// Soroban execution. In stellar-core, deleted entries are stored in the
    /// global entry map as `cleanEmpty` and loaded by
    /// `collectClusterFootprintEntriesFromGlobal`, which blocks BL fallthrough.
    /// This method provides the same blocking behavior for our code.
    ///
    /// Only Soroban key types (ContractData, ContractCode, Ttl) are supported.
    /// Callers must filter to Soroban keys before calling this method.
    pub fn mark_entry_deleted(&mut self, key: &LedgerKey) {
        match key {
            LedgerKey::ContractData(k) => {
                let lookup_key = StorageKey::new(k.contract.clone(), k.key.clone(), k.durability);
                self.contract_data.mark_deleted(lookup_key);
            }
            LedgerKey::ContractCode(k) => {
                self.contract_code.mark_deleted(k.hash.clone());
            }
            LedgerKey::Ttl(k) => {
                self.deleted_ttl.insert(k.key_hash.clone());
            }
            other => {
                // Non-Soroban keys should be filtered by the caller.
                // If this fires, the caller is passing unfiltered dead_entries.
                debug_assert!(
                    false,
                    "mark_entry_deleted called with non-Soroban key: {:?}",
                    other.discriminant()
                );
            }
        }
    }

    /// Convert an account entry into a ledger entry using current metadata.
    pub fn ledger_entry_for_account(&self, entry: &AccountEntry) -> LedgerEntry {
        self.account_to_ledger_entry(entry)
    }

    /// Build a LedgerEntry from a LedgerKey and LedgerEntryData.
    ///
    /// Shared helper for all typed `*_to_ledger_entry` methods. Looks up
    /// `last_modified` and sponsorship metadata from the key.
    pub(super) fn build_ledger_entry(
        &self,
        ledger_key: &LedgerKey,
        data: LedgerEntryData,
    ) -> LedgerEntry {
        LedgerEntry {
            last_modified_ledger_seq: self.last_modified_for_key(ledger_key),
            data,
            ext: self.ledger_entry_ext_for(ledger_key),
        }
    }

    pub(super) fn account_to_ledger_entry(&self, entry: &AccountEntry) -> LedgerEntry {
        let key = LedgerKey::Account(LedgerKeyAccount {
            account_id: entry.account_id.clone(),
        });
        self.build_ledger_entry(&key, LedgerEntryData::Account(entry.clone()))
    }

    pub(super) fn trustline_to_ledger_entry(&self, entry: &TrustLineEntry) -> LedgerEntry {
        let key = LedgerKey::Trustline(LedgerKeyTrustLine {
            account_id: entry.account_id.clone(),
            asset: entry.asset.clone(),
        });
        self.build_ledger_entry(&key, LedgerEntryData::Trustline(entry.clone()))
    }

    pub(super) fn offer_to_ledger_entry(&self, entry: &OfferEntry) -> LedgerEntry {
        let key = LedgerKey::Offer(LedgerKeyOffer {
            seller_id: entry.seller_id.clone(),
            offer_id: entry.offer_id,
        });
        self.build_ledger_entry(&key, LedgerEntryData::Offer(entry.clone()))
    }

    pub(super) fn data_to_ledger_entry(&self, entry: &DataEntry) -> LedgerEntry {
        let key = LedgerKey::Data(LedgerKeyData {
            account_id: entry.account_id.clone(),
            data_name: entry.data_name.clone(),
        });
        self.build_ledger_entry(&key, LedgerEntryData::Data(entry.clone()))
    }

    pub(super) fn contract_data_to_ledger_entry(&self, entry: &ContractDataEntry) -> LedgerEntry {
        let key = LedgerKey::ContractData(LedgerKeyContractData {
            contract: entry.contract.clone(),
            key: entry.key.clone(),
            durability: entry.durability,
        });
        self.build_ledger_entry(&key, LedgerEntryData::ContractData(entry.clone()))
    }

    pub(super) fn contract_code_to_ledger_entry(&self, entry: &ContractCodeEntry) -> LedgerEntry {
        let key = LedgerKey::ContractCode(LedgerKeyContractCode {
            hash: entry.hash.clone(),
        });
        self.build_ledger_entry(&key, LedgerEntryData::ContractCode(entry.clone()))
    }

    pub(super) fn ttl_to_ledger_entry(&self, entry: &TtlEntry) -> LedgerEntry {
        let key = LedgerKey::Ttl(LedgerKeyTtl {
            key_hash: entry.key_hash.clone(),
        });
        self.build_ledger_entry(&key, LedgerEntryData::Ttl(entry.clone()))
    }

    pub(super) fn claimable_balance_to_ledger_entry(
        &self,
        entry: &ClaimableBalanceEntry,
    ) -> LedgerEntry {
        let key = LedgerKey::ClaimableBalance(LedgerKeyClaimableBalance {
            balance_id: entry.balance_id.clone(),
        });
        self.build_ledger_entry(&key, LedgerEntryData::ClaimableBalance(entry.clone()))
    }

    pub(super) fn liquidity_pool_to_ledger_entry(&self, entry: &LiquidityPoolEntry) -> LedgerEntry {
        let key = LedgerKey::LiquidityPool(LedgerKeyLiquidityPool {
            liquidity_pool_id: entry.liquidity_pool_id.clone(),
        });
        self.build_ledger_entry(&key, LedgerEntryData::LiquidityPool(entry.clone()))
    }
}
