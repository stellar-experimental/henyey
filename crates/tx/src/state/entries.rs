use super::*;

impl LedgerStateManager {
    /// Insert an offer into the (account, asset) secondary index.
    pub(super) fn aa_index_insert(&mut self, offer: &OfferEntry) {
        let seller = account_id_to_bytes(&offer.seller_id);
        let selling_key = AssetKey::from_asset(&offer.selling);
        let buying_key = AssetKey::from_asset(&offer.buying);
        self.account_asset_offers
            .entry((seller, selling_key))
            .or_default()
            .insert(offer.offer_id);
        self.account_asset_offers
            .entry((seller, buying_key))
            .or_default()
            .insert(offer.offer_id);
    }

    /// Remove an offer from the (account, asset) secondary index.
    pub(super) fn aa_index_remove(&mut self, offer: &OfferEntry) {
        let seller = account_id_to_bytes(&offer.seller_id);
        let selling_key = AssetKey::from_asset(&offer.selling);
        let buying_key = AssetKey::from_asset(&offer.buying);
        if let Some(set) = self.account_asset_offers.get_mut(&(seller, selling_key)) {
            set.remove(&offer.offer_id);
        }
        if let Some(set) = self.account_asset_offers.get_mut(&(seller, buying_key)) {
            set.remove(&offer.offer_id);
        }
    }

    pub(super) fn last_modified_for_key(&self, key: &LedgerKey) -> u32 {
        self.entry_last_modified
            .get(key)
            .copied()
            .unwrap_or(self.ledger_seq)
    }

    pub(super) fn last_modified_snapshot_for_key(&self, key: &LedgerKey) -> Option<u32> {
        self.entry_last_modified_snapshots
            .get(key)
            .copied()
            .flatten()
    }

    pub(super) fn snapshot_last_modified_key(&mut self, key: &LedgerKey) {
        if !self.entry_last_modified_snapshots.contains_key(key) {
            let snapshot = self.entry_last_modified.get(key).copied();
            self.entry_last_modified_snapshots
                .insert(key.clone(), snapshot);
        }
    }

    pub(super) fn set_last_modified_key(&mut self, key: LedgerKey, seq: u32) {
        self.entry_last_modified.insert(key, seq);
    }

    pub(super) fn remove_last_modified_key(&mut self, key: &LedgerKey) {
        self.entry_last_modified.remove(key);
    }

    pub(super) fn ledger_entry_ext_for_snapshot(&self, key: &LedgerKey) -> LedgerEntryExt {
        let ext_present = self
            .entry_sponsorship_ext_snapshots
            .get(key)
            .copied()
            .unwrap_or_else(|| self.entry_sponsorship_ext.contains(key));
        let sponsor_snapshot = if let Some(snapshot) = self.entry_sponsorship_snapshots.get(key) {
            snapshot.clone()
        } else {
            self.entry_sponsorships.get(key).cloned()
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
        let sponsor = self.entry_sponsorships.get(key).cloned();
        if self.entry_sponsorship_ext.contains(key) || sponsor.is_some() {
            LedgerEntryExt::V1(LedgerEntryExtensionV1 {
                sponsoring_id: SponsorshipDescriptor(sponsor),
                ext: LedgerEntryExtensionV1Ext::V0,
            })
        } else {
            LedgerEntryExt::V0
        }
    }

    /// Load initial state from a ledger reader.
    pub fn load_from_reader<R: LedgerReader>(&mut self, reader: &R, keys: &[LedgerKey]) {
        for key in keys {
            if let Some(entry) = reader.get_entry(key) {
                self.load_entry(entry);
            }
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
        self.entry_last_modified
            .insert(ledger_key.clone(), last_modified);
        if has_sponsorship_ext {
            self.entry_sponsorship_ext.insert(ledger_key.clone());
        }
        if let Some(sponsor) = sponsor {
            self.entry_sponsorships.insert(ledger_key, sponsor);
        }
    }

    /// Load a single entry into the state manager.
    pub fn load_entry(&mut self, entry: LedgerEntry) {
        let sponsor = sponsorship_from_entry_ext(&entry);
        let has_sponsorship_ext = matches!(entry.ext, LedgerEntryExt::V1(_));
        let last_modified = entry.last_modified_ledger_seq;
        match entry.data {
            LedgerEntryData::Account(account) => {
                let key = account_id_to_bytes(&account.account_id);
                let ledger_key = LedgerKey::Account(LedgerKeyAccount {
                    account_id: account.account_id.clone(),
                });
                self.accounts.insert(key, account);
                self.record_entry_metadata(ledger_key, last_modified, has_sponsorship_ext, sponsor);
            }
            LedgerEntryData::Trustline(trustline) => {
                let account_key = account_id_to_bytes(&trustline.account_id);
                let asset_key = AssetKey::from_trustline_asset(&trustline.asset);
                let ledger_key = LedgerKey::Trustline(LedgerKeyTrustLine {
                    account_id: trustline.account_id.clone(),
                    asset: trustline.asset.clone(),
                });
                self.trustlines.insert((account_key, asset_key), trustline);
                self.record_entry_metadata(ledger_key, last_modified, has_sponsorship_ext, sponsor);
            }
            LedgerEntryData::Offer(offer) => {
                let seller_key = account_id_to_bytes(&offer.seller_id);
                let ledger_key = LedgerKey::Offer(LedgerKeyOffer {
                    seller_id: offer.seller_id.clone(),
                    offer_id: offer.offer_id,
                });
                // Add to offer index for efficient best-offer lookups
                self.offer_index.add_offer(&offer);
                self.aa_index_insert(&offer);
                self.offers
                    .insert(OfferKey::new(seller_key, offer.offer_id), offer);
                self.record_entry_metadata(ledger_key, last_modified, has_sponsorship_ext, sponsor);
            }
            LedgerEntryData::Data(data) => {
                let account_key = account_id_to_bytes(&data.account_id);
                let name = data_name_to_string(&data.data_name);
                let ledger_key = LedgerKey::Data(LedgerKeyData {
                    account_id: data.account_id.clone(),
                    data_name: data.data_name.clone(),
                });
                self.data_entries.insert((account_key, name), data);
                self.record_entry_metadata(ledger_key, last_modified, has_sponsorship_ext, sponsor);
            }
            LedgerEntryData::ContractData(contract_data) => {
                let key = ContractDataKey::new(
                    contract_data.contract.clone(),
                    contract_data.key.clone(),
                    contract_data.durability,
                );
                let ledger_key = LedgerKey::ContractData(LedgerKeyContractData {
                    contract: contract_data.contract.clone(),
                    key: contract_data.key.clone(),
                    durability: contract_data.durability,
                });
                self.contract_data.insert(key, contract_data);
                self.record_entry_metadata(ledger_key, last_modified, has_sponsorship_ext, sponsor);
            }
            LedgerEntryData::ContractCode(contract_code) => {
                let key = contract_code.hash.0;
                let ledger_key = LedgerKey::ContractCode(LedgerKeyContractCode {
                    hash: contract_code.hash.clone(),
                });
                self.contract_code.insert(key, contract_code);
                self.record_entry_metadata(ledger_key, last_modified, has_sponsorship_ext, sponsor);
            }
            LedgerEntryData::Ttl(ttl) => {
                let key = ttl.key_hash.0;
                let ledger_key = LedgerKey::Ttl(LedgerKeyTtl {
                    key_hash: ttl.key_hash.clone(),
                });
                // Capture the bucket list TTL value for Soroban.
                // Only capture if not already present - this ensures we keep the original
                // bucket list value even if the entry is reloaded later.
                self.ttl_bucket_list_snapshot
                    .entry(key)
                    .or_insert(ttl.live_until_ledger_seq);
                self.ttl_entries.insert(key, ttl);
                self.record_entry_metadata(ledger_key, last_modified, has_sponsorship_ext, sponsor);
            }
            LedgerEntryData::ClaimableBalance(cb) => {
                let key = claimable_balance_id_to_bytes(&cb.balance_id);
                let ledger_key = LedgerKey::ClaimableBalance(LedgerKeyClaimableBalance {
                    balance_id: cb.balance_id.clone(),
                });
                self.claimable_balances.insert(key, cb);
                self.record_entry_metadata(ledger_key, last_modified, has_sponsorship_ext, sponsor);
            }
            LedgerEntryData::LiquidityPool(lp) => {
                let key = pool_id_to_bytes(&lp.liquidity_pool_id);
                let ledger_key = LedgerKey::LiquidityPool(LedgerKeyLiquidityPool {
                    liquidity_pool_id: lp.liquidity_pool_id.clone(),
                });
                self.liquidity_pools.insert(key, lp);
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
                let key = account_id_to_bytes(&account.account_id);
                let ledger_key = LedgerKey::Account(LedgerKeyAccount {
                    account_id: account.account_id.clone(),
                });
                // Insert account but do NOT save snapshot or mark as modified
                self.accounts.insert(key, account);
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
        let key = account_id_to_bytes(account_id);
        self.accounts.get(&key)
    }

    /// Get an account by ID (read-only).
    pub fn get_account(&self, account_id: &AccountId) -> Option<&AccountEntry> {
        let key = account_id_to_bytes(account_id);
        self.accounts.get(&key)
    }

    /// Get a mutable reference to an account by ID.
    ///
    /// This automatically tracks the modification for the delta.
    pub fn get_account_mut(&mut self, account_id: &AccountId) -> Option<&mut AccountEntry> {
        let key = account_id_to_bytes(account_id);
        if self.accounts.contains_key(&key) {
            // Save snapshot if not already saved or if it's None (for newly created entries).
            // For newly created entries, we update the snapshot to the current value so
            // subsequent operations can track changes with STATE/UPDATED pairs.
            // Rollback correctness is ensured by the created_accounts set.
            if !self
                .account_snapshots
                .get(&key)
                .is_some_and(|s| s.is_some())
            {
                let snapshot = self.accounts.get(&key).cloned();
                self.account_snapshots.insert(key, snapshot);
            }
            let ledger_key = LedgerKey::Account(LedgerKeyAccount {
                account_id: account_id.clone(),
            });
            self.capture_op_snapshot_for_key(&ledger_key);
            self.snapshot_last_modified_key(&ledger_key);
            // Track modification
            if !self.modified_accounts.contains(&key) {
                self.modified_accounts.push(key);
            }
            self.accounts.get_mut(&key)
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
        let key = account_id_to_bytes(account_id);
        // Only record if account exists in state
        if !self.accounts.contains_key(&key) {
            return;
        }
        // Save snapshot if not already saved (same as get_account_mut)
        if !self
            .account_snapshots
            .get(&key)
            .is_some_and(|s| s.is_some())
        {
            let snapshot = self.accounts.get(&key).cloned();
            self.account_snapshots.insert(key, snapshot);
        }
        let ledger_key = LedgerKey::Account(LedgerKeyAccount {
            account_id: account_id.clone(),
        });
        self.capture_op_snapshot_for_key(&ledger_key);
        self.snapshot_last_modified_key(&ledger_key);
        // Track as "modified" so it gets flushed to delta
        if !self.modified_accounts.contains(&key) {
            self.modified_accounts.push(key);
        }
    }

    /// Create a new account entry.
    pub fn create_account(&mut self, entry: AccountEntry) {
        let key = account_id_to_bytes(&entry.account_id);
        let ledger_key = LedgerKey::Account(LedgerKeyAccount {
            account_id: entry.account_id.clone(),
        });

        // Save snapshot (None because it didn't exist)
        self.account_snapshots.entry(key).or_insert(None);
        self.snapshot_last_modified_key(&ledger_key);
        self.set_last_modified_key(ledger_key.clone(), self.ledger_seq);

        // Record in delta
        let ledger_entry = self.account_to_ledger_entry(&entry);
        self.delta.record_create(ledger_entry);

        // Insert into state
        self.accounts.insert(key, entry);

        // Track that this entry was created in this transaction (for rollback)
        self.created_accounts.insert(key);

        // Track modification
        if !self.modified_accounts.contains(&key) {
            self.modified_accounts.push(key);
        }
    }

    /// Update an existing account entry.
    pub fn update_account(&mut self, entry: AccountEntry) {
        let key = account_id_to_bytes(&entry.account_id);
        let ledger_key = LedgerKey::Account(LedgerKeyAccount {
            account_id: entry.account_id.clone(),
        });

        // Save snapshot if not already saved (preserves original state from start of tx)
        if !self.account_snapshots.contains_key(&key) {
            let snapshot = self.accounts.get(&key).cloned();
            self.account_snapshots.insert(key, snapshot);
        }
        self.capture_op_snapshot_for_key(&ledger_key);
        self.snapshot_last_modified_key(&ledger_key);

        // Get pre-state (current value BEFORE this update)
        let pre_state = self
            .accounts
            .get(&key)
            .map(|acc| self.account_to_ledger_entry(acc));

        self.set_last_modified_key(ledger_key.clone(), self.ledger_seq);

        // Record in delta with pre-state
        let post_state = self.account_to_ledger_entry(&entry);
        if let Some(pre) = pre_state {
            self.delta.record_update(pre, post_state);
        }

        // Update state
        self.accounts.insert(key, entry);
    }

    /// Set an account entry directly without delta tracking.
    ///
    /// This is used during verification to sync state with CDP without
    /// affecting the delta computation for subsequent transactions.
    pub fn set_account_no_tracking(&mut self, entry: AccountEntry) {
        let key = account_id_to_bytes(&entry.account_id);
        self.accounts.insert(key, entry);
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
                let key = account_id_to_bytes(&acc.account_id);
                self.accounts.insert(key, acc.clone());
            }
            LedgerEntryData::Trustline(tl) => {
                let account_key = account_id_to_bytes(&tl.account_id);
                let asset_key = AssetKey::from_trustline_asset(&tl.asset);
                let key = (account_key, asset_key);
                self.trustlines.insert(key, tl.clone());
            }
            LedgerEntryData::Offer(offer) => {
                let key = OfferKey::new(account_id_to_bytes(&offer.seller_id), offer.offer_id);
                self.offers.insert(key, offer.clone());
            }
            LedgerEntryData::Data(data) => {
                let name = data_name_to_string(&data.data_name);
                let key = (account_id_to_bytes(&data.account_id), name);
                self.data_entries.insert(key, data.clone());
            }
            LedgerEntryData::ClaimableBalance(cb) => {
                let key = claimable_balance_id_to_bytes(&cb.balance_id);
                self.claimable_balances.insert(key, cb.clone());
            }
            LedgerEntryData::LiquidityPool(lp) => {
                let key = pool_id_to_bytes(&lp.liquidity_pool_id);
                self.liquidity_pools.insert(key, lp.clone());
            }
            LedgerEntryData::ContractData(cd) => {
                let key = ContractDataKey::new(cd.contract.clone(), cd.key.clone(), cd.durability);
                self.contract_data.insert(key, cd.clone());
            }
            LedgerEntryData::ContractCode(cc) => {
                let key = cc.hash.0;
                self.contract_code.insert(key, cc.clone());
            }
            LedgerEntryData::Ttl(ttl) => {
                let key = ttl.key_hash.0;
                // Capture the bucket list TTL value for Soroban.
                // Only capture if not already present - this ensures we keep the original
                // bucket list value even if the entry is reloaded later.
                self.ttl_bucket_list_snapshot
                    .entry(key)
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
                let account_key = account_id_to_bytes(&k.account_id);
                self.accounts.remove(&account_key);
            }
            LedgerKey::Trustline(k) => {
                let account_key = account_id_to_bytes(&k.account_id);
                let asset_key = AssetKey::from_trustline_asset(&k.asset);
                self.trustlines.remove(&(account_key, asset_key));
            }
            LedgerKey::Offer(k) => {
                let offer_key = OfferKey::new(account_id_to_bytes(&k.seller_id), k.offer_id);
                self.offers.remove(&offer_key);
            }
            LedgerKey::Data(k) => {
                let name = data_name_to_string(&k.data_name);
                let data_key = (account_id_to_bytes(&k.account_id), name);
                self.data_entries.remove(&data_key);
            }
            LedgerKey::ClaimableBalance(k) => {
                let cb_key = claimable_balance_id_to_bytes(&k.balance_id);
                self.claimable_balances.remove(&cb_key);
            }
            LedgerKey::LiquidityPool(k) => {
                let pool_key = pool_id_to_bytes(&k.liquidity_pool_id);
                self.liquidity_pools.remove(&pool_key);
            }
            LedgerKey::ContractData(k) => {
                let cd_key = ContractDataKey::new(k.contract.clone(), k.key.clone(), k.durability);
                self.contract_data.remove(&cd_key);
            }
            LedgerKey::ContractCode(k) => {
                let code_key = k.hash.0;
                self.contract_code.remove(&code_key);
            }
            LedgerKey::Ttl(k) => {
                let ttl_key = k.key_hash.0;
                self.ttl_entries.remove(&ttl_key);
            }
            LedgerKey::ConfigSetting(_) => {
                // Config settings not tracked
            }
        }

        self.entry_sponsorships.remove(key);
        self.entry_sponsorship_ext.remove(key);
        self.entry_last_modified.remove(key);
    }

    /// Delete an account entry.
    pub fn delete_account(&mut self, account_id: &AccountId) {
        let key = account_id_to_bytes(account_id);
        let ledger_key = LedgerKey::Account(LedgerKeyAccount {
            account_id: account_id.clone(),
        });

        // Save snapshot if not already saved
        if !self.account_snapshots.contains_key(&key) {
            let snapshot = self.accounts.get(&key).cloned();
            self.account_snapshots.insert(key, snapshot);
        }
        self.capture_op_snapshot_for_key(&ledger_key);
        self.snapshot_last_modified_key(&ledger_key);

        // Get pre-state (current value BEFORE deletion)
        let pre_state = self
            .accounts
            .get(&key)
            .map(|acc| self.account_to_ledger_entry(acc));

        // Record in delta with pre-state
        if let Some(pre) = pre_state {
            self.delta.record_delete(ledger_key.clone(), pre);
        }

        // Remove from state
        self.clear_entry_sponsorship_metadata(&ledger_key);
        self.accounts.remove(&key);
        self.remove_last_modified_key(&ledger_key);
    }

    /// Get a trustline by account and asset (read-only).
    pub fn get_trustline(&self, account_id: &AccountId, asset: &Asset) -> Option<&TrustLineEntry> {
        let account_key = account_id_to_bytes(account_id);
        let asset_key = AssetKey::from_asset(asset);
        self.trustlines.get(&(account_key, asset_key))
    }

    /// Get a trustline by account and trustline asset (read-only).
    pub fn get_trustline_by_trustline_asset(
        &self,
        account_id: &AccountId,
        asset: &TrustLineAsset,
    ) -> Option<&TrustLineEntry> {
        let account_key = account_id_to_bytes(account_id);
        let asset_key = AssetKey::from_trustline_asset(asset);
        self.trustlines.get(&(account_key, asset_key))
    }

    /// Check if a trustline was already loaded during this transaction.
    pub fn is_trustline_tracked(&self, account_id: &AccountId, asset: &TrustLineAsset) -> bool {
        let account_key = account_id_to_bytes(account_id);
        let asset_key = AssetKey::from_trustline_asset(asset);
        self.trustline_snapshots
            .contains_key(&(account_key, asset_key))
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

        let account_bytes = account_id_to_bytes(account_id);

        // Load each pool share trustline and its liquidity pool if not already in memory.
        for pool_id in &pool_ids {
            let pool_share_asset_key = AssetKey::PoolShare(pool_id.0 .0);

            // Load pool share trustline if not already tracked.
            if !self
                .trustlines
                .contains_key(&(account_bytes, pool_share_asset_key))
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
            let pool_id_bytes = pool_id.0 .0;
            if !self.liquidity_pools.contains_key(&pool_id_bytes) {
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
        let account_key = account_id_to_bytes(account_id);
        let asset_key = AssetKey::from_trustline_asset(asset);
        let key = (account_key, asset_key.clone());

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
        let account_key = account_id_to_bytes(account_id);
        let asset_key = AssetKey::from_asset(asset);
        let key = (account_key, asset_key.clone());

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
        let account_key = account_id_to_bytes(&entry.account_id);
        let asset_key = AssetKey::from_trustline_asset(&entry.asset);
        let key = (account_key, asset_key.clone());
        let ledger_key = LedgerKey::Trustline(LedgerKeyTrustLine {
            account_id: entry.account_id.clone(),
            asset: entry.asset.clone(),
        });

        // Save snapshot (None because it didn't exist)
        if !self.trustline_snapshots.contains_key(&key) {
            self.trustline_snapshots.insert(key.clone(), None);
        }
        self.snapshot_last_modified_key(&ledger_key);
        self.set_last_modified_key(ledger_key.clone(), self.ledger_seq);

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
        let account_key = account_id_to_bytes(&entry.account_id);
        let asset_key = AssetKey::from_trustline_asset(&entry.asset);
        let key = (account_key, asset_key.clone());
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

        self.set_last_modified_key(ledger_key.clone(), self.ledger_seq);

        // Record in delta with pre-state
        let post_state = self.trustline_to_ledger_entry(&entry);
        if let Some(pre) = pre_state {
            self.delta.record_update(pre, post_state);
        }

        // Update state
        self.trustlines.insert(key.clone(), entry.clone());

        // Do NOT add to modified_trustlines since we already recorded the update.
        // This prevents flush_modified_entries from recording a duplicate.
        // Classic operations use get_trustline_mut() which tracks modifications separately.
    }

    /// Delete a trustline entry.
    pub fn delete_trustline(&mut self, account_id: &AccountId, asset: &Asset) {
        let account_key = account_id_to_bytes(account_id);
        let asset_key = AssetKey::from_asset(asset);
        let key = (account_key, asset_key.clone());
        let ledger_key = LedgerKey::Trustline(LedgerKeyTrustLine {
            account_id: account_id.clone(),
            asset: asset_to_trustline_asset(asset),
        });

        // Save snapshot if not already saved
        if !self.trustline_snapshots.contains_key(&key) {
            let snapshot = self.trustlines.get(&key).cloned();
            self.trustline_snapshots.insert(key.clone(), snapshot);
        }
        self.capture_op_snapshot_for_key(&ledger_key);
        self.snapshot_last_modified_key(&ledger_key);

        // Get pre-state (current value BEFORE deletion)
        let pre_state = self
            .trustlines
            .get(&key)
            .map(|tl| self.trustline_to_ledger_entry(tl));

        // Record in delta with pre-state
        if let Some(pre) = pre_state {
            self.delta.record_delete(ledger_key.clone(), pre);
        }

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
        let account_key = account_id_to_bytes(account_id);
        let asset_key = AssetKey::from_trustline_asset(asset);
        let key = (account_key, asset_key.clone());
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

        // Get pre-state (current value BEFORE deletion)
        let pre_state = self
            .trustlines
            .get(&key)
            .map(|tl| self.trustline_to_ledger_entry(tl));

        // Record in delta with pre-state
        if let Some(pre) = pre_state {
            self.delta.record_delete(ledger_key.clone(), pre);
        }

        // Remove from state
        self.clear_entry_sponsorship_metadata(&ledger_key);
        self.trustlines.remove(&key);
        self.remove_last_modified_key(&ledger_key);
    }

    /// Iterate over all offers currently in state.
    pub fn iter_offers(&self) -> impl Iterator<Item = &OfferEntry> {
        self.offers.values()
    }

    /// Get an offer by seller and offer ID (read-only).
    pub fn get_offer(&self, seller_id: &AccountId, offer_id: i64) -> Option<&OfferEntry> {
        let seller_key = account_id_to_bytes(seller_id);
        self.offers.get(&OfferKey::new(seller_key, offer_id))
    }

    /// Check if an offer was already loaded during this transaction.
    pub fn is_offer_tracked(&self, seller_id: &AccountId, offer_id: i64) -> bool {
        let seller_key = account_id_to_bytes(seller_id);
        self.offer_snapshots
            .contains_key(&OfferKey::new(seller_key, offer_id))
    }

    /// Get all offers for an account that buy or sell a specific asset.
    ///
    /// Uses the state's own `account_asset_offers` secondary index, which is
    /// maintained as offers are loaded, created, modified, and deleted. This is
    /// more reliable than the manager's index (which may be stale across ledgers).
    pub fn get_offers_by_account_and_asset(
        &self,
        account_id: &AccountId,
        asset: &Asset,
    ) -> Vec<OfferEntry> {
        let account_key = account_id_to_bytes(account_id);
        let asset_key = AssetKey::from_asset(asset);
        self.account_asset_offers
            .get(&(account_key, asset_key))
            .map(|ids| {
                ids.iter()
                    .filter_map(|&id| self.offers.get(&OfferKey::new(account_key, id)).cloned())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Get a mutable reference to an offer.
    pub fn get_offer_mut(
        &mut self,
        seller_id: &AccountId,
        offer_id: i64,
    ) -> Option<&mut OfferEntry> {
        let seller_key = account_id_to_bytes(seller_id);
        let key = OfferKey::new(seller_key, offer_id);

        if self.offers.contains_key(&key) {
            // Save snapshot if not already saved or if it's None (for newly created entries).
            // For newly created entries, we update the snapshot to the current value so
            // subsequent operations can track changes with STATE/UPDATED pairs.
            // Rollback correctness is ensured by the created_offers set.
            if !self.offer_snapshots.get(&key).is_some_and(|s| s.is_some()) {
                let snapshot = self.offers.get(&key).cloned();
                self.offer_snapshots.insert(key, snapshot);
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
            self.offers.get_mut(&key)
        } else {
            None
        }
    }

    /// Create a new offer entry.
    pub fn create_offer(&mut self, entry: OfferEntry) {
        let seller_key = account_id_to_bytes(&entry.seller_id);
        let key = OfferKey::new(seller_key, entry.offer_id);
        let ledger_key = LedgerKey::Offer(LedgerKeyOffer {
            seller_id: entry.seller_id.clone(),
            offer_id: entry.offer_id,
        });

        // Save snapshot (None because it didn't exist)
        self.offer_snapshots.entry(key).or_insert(None);
        self.snapshot_last_modified_key(&ledger_key);
        self.set_last_modified_key(ledger_key.clone(), self.ledger_seq);

        // Record in delta
        let ledger_entry = self.offer_to_ledger_entry(&entry);
        self.delta.record_create(ledger_entry);

        // Add to offer index for efficient best-offer lookups
        self.offer_index.add_offer(&entry);
        self.aa_index_insert(&entry);

        // Insert into state
        self.offers.insert(key, entry);

        // Track that this entry was created in this transaction (for rollback)
        self.created_offers.insert(key);

        // Track modification
        if !self.modified_offers.contains(&key) {
            self.modified_offers.push(key);
        }
    }

    /// Update an existing offer entry.
    pub fn update_offer(&mut self, entry: OfferEntry) {
        let seller_key = account_id_to_bytes(&entry.seller_id);
        let key = OfferKey::new(seller_key, entry.offer_id);
        let ledger_key = LedgerKey::Offer(LedgerKeyOffer {
            seller_id: entry.seller_id.clone(),
            offer_id: entry.offer_id,
        });

        // Save snapshot if not already saved (for rollback purposes)
        if !self.offer_snapshots.contains_key(&key) {
            let snapshot = self.offers.get(&key).cloned();
            self.offer_snapshots.insert(key, snapshot);
        }
        self.capture_op_snapshot_for_key(&ledger_key);
        self.snapshot_last_modified_key(&ledger_key);

        // Get pre-state from current state (value BEFORE this specific update)
        let pre_state = self
            .offers
            .get(&key)
            .map(|offer| self.offer_to_ledger_entry(offer));

        self.set_last_modified_key(ledger_key.clone(), self.ledger_seq);

        // Record in delta - each update gets its own STATE/UPDATED pair
        let post_state = self.offer_to_ledger_entry(&entry);
        if let Some(pre) = pre_state {
            self.delta.record_update(pre, post_state);
        }

        // Update offer index (handles price/asset changes)
        self.offer_index.update_offer(&entry);

        // Update (account, asset) secondary index: remove old, insert new
        let old_offer_clone = self.offers.get(&key).cloned();
        if let Some(ref old_offer) = old_offer_clone {
            self.aa_index_remove(old_offer);
        }
        self.aa_index_insert(&entry);

        // Update state
        self.offers.insert(key, entry.clone());

        // Do NOT track in modified_offers since we already recorded the update
        // This prevents flush_modified_entries from recording a duplicate
    }

    /// Delete an offer entry.
    pub fn delete_offer(&mut self, seller_id: &AccountId, offer_id: i64) {
        let seller_key = account_id_to_bytes(seller_id);
        let key = OfferKey::new(seller_key, offer_id);
        let ledger_key = LedgerKey::Offer(LedgerKeyOffer {
            seller_id: seller_id.clone(),
            offer_id,
        });

        // Save snapshot if not already saved
        if !self.offer_snapshots.contains_key(&key) {
            let snapshot = self.offers.get(&key).cloned();
            self.offer_snapshots.insert(key, snapshot);
        }
        self.capture_op_snapshot_for_key(&ledger_key);
        self.snapshot_last_modified_key(&ledger_key);

        // Get pre-state (current value BEFORE deletion)
        let pre_state = self
            .offers
            .get(&key)
            .map(|offer| self.offer_to_ledger_entry(offer));

        // Record in delta with pre-state
        if let Some(pre) = pre_state {
            self.delta.record_delete(ledger_key.clone(), pre);
        }

        // Remove from offer index
        self.offer_index.remove_offer(seller_id, offer_id);

        // Remove from (account, asset) secondary index
        if let Some(offer) = self.offers.get(&key) {
            let offer_clone = offer.clone();
            self.aa_index_remove(&offer_clone);
        }

        // Remove from state
        self.clear_entry_sponsorship_metadata(&ledger_key);
        self.offers.remove(&key);
        self.remove_last_modified_key(&ledger_key);
    }

    /// Get the best offer for a buying/selling pair (lowest price, then offer ID).
    ///
    /// Uses the offer index for O(log n) lookup instead of scanning all offers.
    pub fn best_offer(&self, buying: &Asset, selling: &Asset) -> Option<OfferEntry> {
        // Use the offer index for efficient lookup
        if let Some(key) = self.offer_index.best_offer_key(buying, selling) {
            return self.offers.get(&key).cloned();
        }
        None
    }

    /// Get the best offer for a buying/selling pair with an additional filter.
    ///
    /// Uses the offer index for efficient traversal in price order.
    pub fn best_offer_filtered<F>(
        &self,
        buying: &Asset,
        selling: &Asset,
        mut keep: F,
    ) -> Option<OfferEntry>
    where
        F: FnMut(&OfferEntry) -> bool,
    {
        // Use the offer index to iterate in price order
        for offer_key in self.offer_index.offers_for_pair(buying, selling) {
            if let Some(offer) = self.offers.get(&offer_key) {
                if keep(offer) {
                    return Some(offer.clone());
                }
            }
        }
        None
    }

    /// Check if offers exist for a specific asset pair.
    pub fn has_offers_for_pair(&self, buying: &Asset, selling: &Asset) -> bool {
        self.offer_index.has_offers(buying, selling)
    }

    /// Get all offers for a specific buying/selling asset pair.
    ///
    /// Returns cloned OfferEntry values for each offer in the pair's order book.
    pub fn offers_for_asset_pair(&self, buying: &Asset, selling: &Asset) -> Vec<OfferEntry> {
        self.offer_index
            .offers_for_pair(buying, selling)
            .filter_map(|key| self.offers.get(&key).cloned())
            .collect()
    }

    /// Get the number of offers in the index.
    pub fn offer_index_size(&self) -> usize {
        self.offer_index.len()
    }

    /// Get the number of unique asset pairs with offers.
    pub fn offer_index_num_pairs(&self) -> usize {
        self.offer_index.num_asset_pairs()
    }

    /// Remove all offers owned by an account that are buying or selling a specific asset.
    /// This is used when revoking authorization on a trustline.
    /// Returns the list of OfferEntry that were removed (before deletion) so callers can
    /// handle liability release, subentry updates, and sponsorship adjustments.
    ///
    /// Mirrors stellar-core `removeOffersByAccountAndAsset` which calls
    /// `loadOffersByAccountAndAsset` to query the SQL database for ALL
    /// matching offers.  We first load all matching offers from the
    /// authoritative offer store so the in-memory index is complete.
    pub fn remove_offers_by_account_and_asset(
        &mut self,
        account_id: &AccountId,
        asset: &Asset,
    ) -> Vec<OfferEntry> {
        // Load all matching offers from the authoritative source so the
        // in-memory index has every offer that exists, not just those that
        // happened to be loaded during prior TX execution.
        if let Some(loader) = self.offers_by_account_asset_loader.take() {
            match loader(account_id, asset) {
                Ok(entries) => {
                    tracing::debug!(
                        ledger_seq = self.ledger_seq,
                        account = ?account_id,
                        loader_entries = entries.len(),
                        existing_offers = self.offers.len(),
                        "remove_offers_by_account_and_asset: loader returned entries"
                    );
                    for entry in entries {
                        if let LedgerEntryData::Offer(ref offer) = entry.data {
                            let seller_key = account_id_to_bytes(&offer.seller_id);
                            let key = OfferKey::new(seller_key, offer.offer_id);
                            // Skip offers already deleted in this ledger (by a previous TX).
                            // The loader returns offers from the bucket list snapshot which
                            // doesn't reflect in-ledger deletions.
                            let ledger_key = LedgerKey::Offer(LedgerKeyOffer {
                                seller_id: offer.seller_id.clone(),
                                offer_id: offer.offer_id,
                            });
                            if self.delta.deleted_keys().contains(&ledger_key) {
                                continue;
                            }
                            // Only load offers not already tracked in state.
                            if !self.offers.contains_key(&key) {
                                tracing::info!(
                                    ledger_seq = self.ledger_seq,
                                    offer_id = offer.offer_id,
                                    "remove_offers_by_account_and_asset: loading NEW offer from authoritative store"
                                );
                                self.load_entry(entry);
                            }
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        "remove_offers_by_account_and_asset: loader failed"
                    );
                }
            }
            self.offers_by_account_asset_loader = Some(loader);
        } else {
            tracing::debug!(
                ledger_seq = self.ledger_seq,
                "remove_offers_by_account_and_asset: NO loader available"
            );
        }

        let account_key = account_id_to_bytes(account_id);
        let asset_key = AssetKey::from_asset(asset);

        // Look up offer IDs from secondary index
        let offer_ids: Vec<i64> = self
            .account_asset_offers
            .get(&(account_key, asset_key))
            .map(|ids| ids.iter().copied().collect())
            .unwrap_or_default();

        // Collect matching offers (verify they still match before removing)
        let offers_to_remove: Vec<OfferEntry> = offer_ids
            .iter()
            .filter_map(|&offer_id| {
                self.offers
                    .get(&OfferKey::new(account_key, offer_id))
                    .cloned()
                    .filter(|offer| offer.buying == *asset || offer.selling == *asset)
            })
            .collect();

        // Remove each offer
        for offer in &offers_to_remove {
            self.delete_offer(&offer.seller_id, offer.offer_id);
        }

        offers_to_remove
    }

    /// Get a data entry by account and name (read-only).
    pub fn get_data(&self, account_id: &AccountId, name: &str) -> Option<&DataEntry> {
        let account_key = account_id_to_bytes(account_id);
        self.data_entries.get(&(account_key, name.to_string()))
    }

    /// Check if a data entry was already loaded during this transaction.
    pub fn is_data_tracked(&self, account_id: &AccountId, name: &str) -> bool {
        let account_key = account_id_to_bytes(account_id);
        self.data_snapshots
            .contains_key(&(account_key, name.to_string()))
    }

    /// Get a mutable reference to a data entry.
    pub fn get_data_mut(&mut self, account_id: &AccountId, name: &str) -> Option<&mut DataEntry> {
        let account_key = account_id_to_bytes(account_id);
        let key = (account_key, name.to_string());

        if self.data_entries.contains_key(&key) {
            // Save snapshot if not already saved or if it's None (for newly created entries).
            // For newly created entries, we update the snapshot to the current value so
            // subsequent operations can track changes with STATE/UPDATED pairs.
            // Rollback correctness is ensured by the created_data set.
            if !self.data_snapshots.get(&key).is_some_and(|s| s.is_some()) {
                let snapshot = self.data_entries.get(&key).cloned();
                self.data_snapshots.insert(key.clone(), snapshot);
            }
            if let Some(entry) = self.data_entries.get(&key) {
                let ledger_key = LedgerKey::Data(LedgerKeyData {
                    account_id: entry.account_id.clone(),
                    data_name: entry.data_name.clone(),
                });
                self.capture_op_snapshot_for_key(&ledger_key);
                self.snapshot_last_modified_key(&ledger_key);
            }
            // Track modification
            if !self.modified_data.contains(&key) {
                self.modified_data.push(key.clone());
            }
            self.data_entries.get_mut(&key)
        } else {
            None
        }
    }

    /// Create a new data entry.
    pub fn create_data(&mut self, entry: DataEntry) {
        let account_key = account_id_to_bytes(&entry.account_id);
        let name = data_name_to_string(&entry.data_name);
        tracing::debug!(
            "create_data: account_key={:02x?}, name={:?}, name_bytes={:?}",
            &account_key[..4],
            name,
            entry.data_name.as_vec()
        );
        let key = (account_key, name.clone());
        let ledger_key = LedgerKey::Data(LedgerKeyData {
            account_id: entry.account_id.clone(),
            data_name: entry.data_name.clone(),
        });

        // Save snapshot (None because it didn't exist)
        if !self.data_snapshots.contains_key(&key) {
            self.data_snapshots.insert(key.clone(), None);
        }
        self.snapshot_last_modified_key(&ledger_key);
        self.set_last_modified_key(ledger_key.clone(), self.ledger_seq);

        // Record in delta
        let ledger_entry = self.data_to_ledger_entry(&entry);
        self.delta.record_create(ledger_entry);

        // Insert into state
        self.data_entries.insert(key.clone(), entry);

        // Track that this entry was created in this transaction (for rollback)
        self.created_data.insert(key.clone());

        // Track modification
        if !self.modified_data.contains(&key) {
            self.modified_data.push(key);
        }
    }

    /// Update an existing data entry.
    pub fn update_data(&mut self, entry: DataEntry) {
        let account_key = account_id_to_bytes(&entry.account_id);
        let name = data_name_to_string(&entry.data_name);
        let key = (account_key, name.clone());
        let ledger_key = LedgerKey::Data(LedgerKeyData {
            account_id: entry.account_id.clone(),
            data_name: entry.data_name.clone(),
        });

        // Save snapshot if not already saved
        if !self.data_snapshots.contains_key(&key) {
            let snapshot = self.data_entries.get(&key).cloned();
            self.data_snapshots.insert(key.clone(), snapshot);
        }
        self.capture_op_snapshot_for_key(&ledger_key);
        self.snapshot_last_modified_key(&ledger_key);

        // Get pre-state (current value BEFORE this update)
        let pre_state = self
            .data_entries
            .get(&key)
            .map(|data| self.data_to_ledger_entry(data));

        self.set_last_modified_key(ledger_key.clone(), self.ledger_seq);

        // Record in delta with pre-state
        let post_state = self.data_to_ledger_entry(&entry);
        if let Some(pre) = pre_state {
            self.delta.record_update(pre, post_state);
        }

        // Update state
        self.data_entries.insert(key, entry);
    }

    /// Delete a data entry.
    pub fn delete_data(&mut self, account_id: &AccountId, name: &str) {
        let account_key = account_id_to_bytes(account_id);
        let key = (account_key, name.to_string());

        // Save snapshot if not already saved
        if !self.data_snapshots.contains_key(&key) {
            let snapshot = self.data_entries.get(&key).cloned();
            self.data_snapshots.insert(key.clone(), snapshot);
        }

        // Record in delta - we need to get the data_name from the entry
        // Clone the entry first to avoid borrow checker issues
        if let Some(entry) = self.data_entries.get(&key).cloned() {
            let ledger_key = LedgerKey::Data(LedgerKeyData {
                account_id: account_id.clone(),
                data_name: entry.data_name.clone(),
            });
            self.capture_op_snapshot_for_key(&ledger_key);
            self.snapshot_last_modified_key(&ledger_key);

            // Get pre-state (current value BEFORE deletion)
            let pre_state = self.data_to_ledger_entry(&entry);
            self.delta.record_delete(ledger_key.clone(), pre_state);
            self.clear_entry_sponsorship_metadata(&ledger_key);
            self.remove_last_modified_key(&ledger_key);
        }

        // Remove from state
        self.data_entries.remove(&key);
    }

    /// Get a contract data entry by key (read-only).
    pub fn get_contract_data(
        &self,
        contract: &ScAddress,
        key: &ScVal,
        durability: ContractDataDurability,
    ) -> Option<&ContractDataEntry> {
        let lookup_key = ContractDataKey::new(contract.clone(), key.clone(), durability);
        self.contract_data.get(&lookup_key)
    }

    /// Get a mutable reference to a contract data entry.
    pub fn get_contract_data_mut(
        &mut self,
        contract: &ScAddress,
        key: &ScVal,
        durability: ContractDataDurability,
    ) -> Option<&mut ContractDataEntry> {
        let lookup_key = ContractDataKey::new(contract.clone(), key.clone(), durability);

        if self.contract_data.contains_key(&lookup_key) {
            // Save snapshot if not already saved or if it's None (for newly created entries).
            // For newly created entries, we update the snapshot to the current value so
            // subsequent operations can track changes with STATE/UPDATED pairs.
            // Rollback correctness is ensured by the created_contract_data set.
            if !self
                .contract_data_snapshots
                .get(&lookup_key)
                .is_some_and(|s| s.is_some())
            {
                let snapshot = self.contract_data.get(&lookup_key).cloned();
                self.contract_data_snapshots
                    .insert(lookup_key.clone(), snapshot);
            }
            let ledger_key = LedgerKey::ContractData(LedgerKeyContractData {
                contract: contract.clone(),
                key: key.clone(),
                durability,
            });
            self.capture_op_snapshot_for_key(&ledger_key);
            self.snapshot_last_modified_key(&ledger_key);
            // Track modification
            if !self.modified_contract_data.contains(&lookup_key) {
                self.modified_contract_data.push(lookup_key.clone());
            }
            self.contract_data.get_mut(&lookup_key)
        } else {
            None
        }
    }

    /// Create a new contract data entry.
    pub fn create_contract_data(&mut self, entry: ContractDataEntry) {
        let key = ContractDataKey::new(entry.contract.clone(), entry.key.clone(), entry.durability);
        let ledger_key = LedgerKey::ContractData(LedgerKeyContractData {
            contract: entry.contract.clone(),
            key: entry.key.clone(),
            durability: entry.durability,
        });

        // Save snapshot (None because it didn't exist)
        if !self.contract_data_snapshots.contains_key(&key) {
            self.contract_data_snapshots.insert(key.clone(), None);
        }
        self.snapshot_last_modified_key(&ledger_key);
        self.set_last_modified_key(ledger_key.clone(), self.ledger_seq);

        // Record in delta
        let ledger_entry = self.contract_data_to_ledger_entry(&entry);
        self.delta.record_create(ledger_entry);

        // Insert into state
        self.contract_data.insert(key.clone(), entry);

        // Track that this entry was created in this transaction (for rollback)
        self.created_contract_data.insert(key.clone());

        // Track modification
        if !self.modified_contract_data.contains(&key) {
            self.modified_contract_data.push(key);
        }
    }

    /// Update an existing contract data entry.
    pub fn update_contract_data(&mut self, entry: ContractDataEntry) {
        let key = ContractDataKey::new(entry.contract.clone(), entry.key.clone(), entry.durability);
        let ledger_key = LedgerKey::ContractData(LedgerKeyContractData {
            contract: entry.contract.clone(),
            key: entry.key.clone(),
            durability: entry.durability,
        });

        // Save snapshot if not already saved
        if !self.contract_data_snapshots.contains_key(&key) {
            let snapshot = self.contract_data.get(&key).cloned();
            self.contract_data_snapshots.insert(key.clone(), snapshot);
        }
        self.capture_op_snapshot_for_key(&ledger_key);
        self.snapshot_last_modified_key(&ledger_key);

        // Get pre-state (current value BEFORE this update)
        let pre_state = self
            .contract_data
            .get(&key)
            .map(|cd| self.contract_data_to_ledger_entry(cd));

        self.set_last_modified_key(ledger_key.clone(), self.ledger_seq);

        // Record in delta with pre-state
        let post_state = self.contract_data_to_ledger_entry(&entry);
        if let Some(pre) = pre_state {
            self.delta.record_update(pre, post_state);
        }

        // Update state
        self.contract_data.insert(key, entry);
    }

    /// Delete a contract data entry.
    pub fn delete_contract_data(
        &mut self,
        contract: &ScAddress,
        key: &ScVal,
        durability: ContractDataDurability,
    ) {
        let lookup_key = ContractDataKey::new(contract.clone(), key.clone(), durability);
        let ledger_key = LedgerKey::ContractData(LedgerKeyContractData {
            contract: contract.clone(),
            key: key.clone(),
            durability,
        });

        // Save snapshot if not already saved
        if !self.contract_data_snapshots.contains_key(&lookup_key) {
            let snapshot = self.contract_data.get(&lookup_key).cloned();
            self.contract_data_snapshots
                .insert(lookup_key.clone(), snapshot);
        }
        self.capture_op_snapshot_for_key(&ledger_key);
        self.snapshot_last_modified_key(&ledger_key);

        // Get pre-state (current value BEFORE deletion)
        let pre_state = self
            .contract_data
            .get(&lookup_key)
            .map(|cd| self.contract_data_to_ledger_entry(cd));

        // Record in delta with pre-state
        if let Some(pre) = pre_state {
            self.delta.record_delete(ledger_key.clone(), pre);
        }

        // Remove from state and track deletion
        self.clear_entry_sponsorship_metadata(&ledger_key);
        self.contract_data.remove(&lookup_key);
        self.remove_last_modified_key(&ledger_key);
        // Track this deletion to prevent reloading from bucket list
        self.deleted_contract_data.insert(lookup_key);
    }

    /// Get a contract code entry by hash (read-only).
    pub fn get_contract_code(&self, hash: &Hash) -> Option<&ContractCodeEntry> {
        self.contract_code.get(&hash.0)
    }

    /// Get a mutable reference to a contract code entry.
    pub fn get_contract_code_mut(&mut self, hash: &Hash) -> Option<&mut ContractCodeEntry> {
        let key = hash.0;

        if self.contract_code.contains_key(&key) {
            // Save snapshot if not already saved or if it's None (for newly created entries).
            // For newly created entries, we update the snapshot to the current value so
            // subsequent operations can track changes with STATE/UPDATED pairs.
            // Rollback correctness is ensured by the created_contract_code set.
            if !self
                .contract_code_snapshots
                .get(&key)
                .is_some_and(|s| s.is_some())
            {
                let snapshot = self.contract_code.get(&key).cloned();
                self.contract_code_snapshots.insert(key, snapshot);
            }
            let ledger_key = LedgerKey::ContractCode(LedgerKeyContractCode { hash: hash.clone() });
            self.capture_op_snapshot_for_key(&ledger_key);
            self.snapshot_last_modified_key(&ledger_key);
            // Track modification
            if !self.modified_contract_code.contains(&key) {
                self.modified_contract_code.push(key);
            }
            self.contract_code.get_mut(&key)
        } else {
            None
        }
    }

    /// Create a new contract code entry.
    pub fn create_contract_code(&mut self, entry: ContractCodeEntry) {
        let key = entry.hash.0;
        let ledger_key = LedgerKey::ContractCode(LedgerKeyContractCode {
            hash: entry.hash.clone(),
        });

        // Save snapshot (None because it didn't exist)
        self.contract_code_snapshots.entry(key).or_insert(None);
        self.snapshot_last_modified_key(&ledger_key);
        self.set_last_modified_key(ledger_key.clone(), self.ledger_seq);

        // Record in delta
        let ledger_entry = self.contract_code_to_ledger_entry(&entry);
        self.delta.record_create(ledger_entry);

        // Insert into state
        self.contract_code.insert(key, entry);

        // Track that this entry was created in this transaction (for rollback)
        self.created_contract_code.insert(key);

        // Track modification
        if !self.modified_contract_code.contains(&key) {
            self.modified_contract_code.push(key);
        }
    }

    /// Update an existing contract code entry.
    pub fn update_contract_code(&mut self, entry: ContractCodeEntry) {
        let key = entry.hash.0;
        let ledger_key = LedgerKey::ContractCode(LedgerKeyContractCode {
            hash: entry.hash.clone(),
        });

        // Save snapshot if not already saved
        if !self.contract_code_snapshots.contains_key(&key) {
            let snapshot = self.contract_code.get(&key).cloned();
            self.contract_code_snapshots.insert(key, snapshot);
        }
        self.capture_op_snapshot_for_key(&ledger_key);
        self.snapshot_last_modified_key(&ledger_key);

        // Get pre-state (current value BEFORE this update)
        let pre_state = self
            .contract_code
            .get(&key)
            .map(|cc| self.contract_code_to_ledger_entry(cc));

        self.set_last_modified_key(ledger_key.clone(), self.ledger_seq);

        // Record in delta with pre-state
        let post_state = self.contract_code_to_ledger_entry(&entry);
        if let Some(pre) = pre_state {
            self.delta.record_update(pre, post_state);
        }

        // Update state
        self.contract_code.insert(key, entry);
    }

    /// Delete a contract code entry.
    pub fn delete_contract_code(&mut self, hash: &Hash) {
        let key = hash.0;
        let ledger_key = LedgerKey::ContractCode(LedgerKeyContractCode { hash: hash.clone() });

        // Save snapshot if not already saved
        if !self.contract_code_snapshots.contains_key(&key) {
            let snapshot = self.contract_code.get(&key).cloned();
            self.contract_code_snapshots.insert(key, snapshot);
        }
        self.capture_op_snapshot_for_key(&ledger_key);
        self.snapshot_last_modified_key(&ledger_key);

        // Get pre-state (current value BEFORE deletion)
        let pre_state = self
            .contract_code
            .get(&key)
            .map(|cc| self.contract_code_to_ledger_entry(cc));

        // Record in delta with pre-state
        if let Some(pre) = pre_state {
            self.delta.record_delete(ledger_key.clone(), pre);
        }

        // Remove from state and track deletion
        self.clear_entry_sponsorship_metadata(&ledger_key);
        self.contract_code.remove(&key);
        self.remove_last_modified_key(&ledger_key);
        // Track this deletion to prevent reloading from bucket list
        self.deleted_contract_code.insert(key);
    }

    /// Get a claimable balance by ID (read-only).
    pub fn get_claimable_balance(
        &self,
        balance_id: &ClaimableBalanceId,
    ) -> Option<&ClaimableBalanceEntry> {
        let key = claimable_balance_id_to_bytes(balance_id);
        self.claimable_balances.get(&key)
    }

    /// Check if a claimable balance was already loaded during this transaction.
    /// Returns true if the entry exists in snapshots, meaning it was loaded
    /// (even if subsequently deleted). Used to prevent reloading deleted entries
    /// from the database during per-operation preloading.
    pub fn is_claimable_balance_tracked(&self, balance_id: &ClaimableBalanceId) -> bool {
        let key = claimable_balance_id_to_bytes(balance_id);
        self.claimable_balance_snapshots.contains_key(&key)
    }

    /// Get a mutable reference to a claimable balance entry.
    pub fn get_claimable_balance_mut(
        &mut self,
        balance_id: &ClaimableBalanceId,
    ) -> Option<&mut ClaimableBalanceEntry> {
        let key = claimable_balance_id_to_bytes(balance_id);

        if self.claimable_balances.contains_key(&key) {
            if !self
                .claimable_balance_snapshots
                .get(&key)
                .is_some_and(|s| s.is_some())
            {
                let snapshot = self.claimable_balances.get(&key).cloned();
                self.claimable_balance_snapshots.insert(key, snapshot);
            }
            let ledger_key = LedgerKey::ClaimableBalance(LedgerKeyClaimableBalance {
                balance_id: balance_id.clone(),
            });
            self.capture_op_snapshot_for_key(&ledger_key);
            self.snapshot_last_modified_key(&ledger_key);
            if !self.modified_claimable_balances.contains(&key) {
                self.modified_claimable_balances.push(key);
            }
            self.claimable_balances.get_mut(&key)
        } else {
            None
        }
    }

    /// Create a new claimable balance entry.
    pub fn create_claimable_balance(&mut self, entry: ClaimableBalanceEntry) {
        let key = claimable_balance_id_to_bytes(&entry.balance_id);
        let ledger_key = LedgerKey::ClaimableBalance(LedgerKeyClaimableBalance {
            balance_id: entry.balance_id.clone(),
        });

        // Save snapshot (None because it didn't exist)
        self.claimable_balance_snapshots.entry(key).or_insert(None);
        self.snapshot_last_modified_key(&ledger_key);
        self.set_last_modified_key(ledger_key.clone(), self.ledger_seq);

        // Record in delta
        let ledger_entry = self.claimable_balance_to_ledger_entry(&entry);
        self.delta.record_create(ledger_entry);

        // Insert into state
        self.claimable_balances.insert(key, entry);

        // Track that this entry was created in this transaction (for rollback)
        self.created_claimable_balances.insert(key);

        // Track modification
        if !self.modified_claimable_balances.contains(&key) {
            self.modified_claimable_balances.push(key);
        }
    }

    /// Delete a claimable balance entry (when claimed).
    pub fn delete_claimable_balance(&mut self, balance_id: &ClaimableBalanceId) {
        let key = claimable_balance_id_to_bytes(balance_id);
        let ledger_key = LedgerKey::ClaimableBalance(LedgerKeyClaimableBalance {
            balance_id: balance_id.clone(),
        });

        // Save snapshot if not already saved
        if !self.claimable_balance_snapshots.contains_key(&key) {
            let snapshot = self.claimable_balances.get(&key).cloned();
            self.claimable_balance_snapshots.insert(key, snapshot);
        }
        self.capture_op_snapshot_for_key(&ledger_key);
        self.snapshot_last_modified_key(&ledger_key);

        // Get pre-state (current value BEFORE deletion)
        let pre_state = self
            .claimable_balances
            .get(&key)
            .map(|e| self.claimable_balance_to_ledger_entry(e));

        // Record in delta with pre-state
        if let Some(pre) = pre_state {
            self.delta.record_delete(ledger_key.clone(), pre);
        }

        // Remove from state
        self.clear_entry_sponsorship_metadata(&ledger_key);
        self.claimable_balances.remove(&key);
        self.remove_last_modified_key(&ledger_key);
    }

    /// Update an existing claimable balance entry.
    pub fn update_claimable_balance(&mut self, entry: ClaimableBalanceEntry) {
        let key = claimable_balance_id_to_bytes(&entry.balance_id);
        let ledger_key = LedgerKey::ClaimableBalance(LedgerKeyClaimableBalance {
            balance_id: entry.balance_id.clone(),
        });

        // Save snapshot if not already saved
        if !self.claimable_balance_snapshots.contains_key(&key) {
            let snapshot = self.claimable_balances.get(&key).cloned();
            self.claimable_balance_snapshots.insert(key, snapshot);
        }
        self.capture_op_snapshot_for_key(&ledger_key);
        self.snapshot_last_modified_key(&ledger_key);

        // Get pre-state (current value BEFORE this update)
        let pre_state = self
            .claimable_balances
            .get(&key)
            .map(|e| self.claimable_balance_to_ledger_entry(e));

        self.set_last_modified_key(ledger_key.clone(), self.ledger_seq);

        // Record in delta with pre-state
        let post_state = self.claimable_balance_to_ledger_entry(&entry);
        if let Some(pre) = pre_state {
            self.delta.record_update(pre, post_state);
        }

        // Update state
        self.claimable_balances.insert(key, entry);
    }

    /// Get a liquidity pool by ID (read-only).
    pub fn get_liquidity_pool(&self, pool_id: &PoolId) -> Option<&LiquidityPoolEntry> {
        let key = pool_id_to_bytes(pool_id);
        self.liquidity_pools.get(&key)
    }

    /// Check if a liquidity pool was already loaded during this transaction.
    pub fn is_liquidity_pool_tracked(&self, pool_id: &PoolId) -> bool {
        let key = pool_id_to_bytes(pool_id);
        self.liquidity_pool_snapshots.contains_key(&key)
    }

    /// Get a mutable reference to a liquidity pool.
    pub fn get_liquidity_pool_mut(&mut self, pool_id: &PoolId) -> Option<&mut LiquidityPoolEntry> {
        let key = pool_id_to_bytes(pool_id);
        if self.liquidity_pools.contains_key(&key) {
            // Save snapshot if not already saved or if it's None (for newly created entries).
            // For newly created entries, we update the snapshot to the current value so
            // subsequent operations can track changes with STATE/UPDATED pairs.
            // Rollback correctness is ensured by the created_liquidity_pools set.
            if !self
                .liquidity_pool_snapshots
                .get(&key)
                .is_some_and(|s| s.is_some())
            {
                let snapshot = self.liquidity_pools.get(&key).cloned();
                self.liquidity_pool_snapshots.insert(key, snapshot);
            }
            let ledger_key = LedgerKey::LiquidityPool(LedgerKeyLiquidityPool {
                liquidity_pool_id: pool_id.clone(),
            });
            self.capture_op_snapshot_for_key(&ledger_key);
            self.snapshot_last_modified_key(&ledger_key);
            // Track modification
            if !self.modified_liquidity_pools.contains(&key) {
                self.modified_liquidity_pools.push(key);
            }
            self.liquidity_pools.get_mut(&key)
        } else {
            None
        }
    }

    /// Create a new liquidity pool entry.
    pub fn create_liquidity_pool(&mut self, entry: LiquidityPoolEntry) {
        let key = pool_id_to_bytes(&entry.liquidity_pool_id);
        let ledger_key = LedgerKey::LiquidityPool(LedgerKeyLiquidityPool {
            liquidity_pool_id: entry.liquidity_pool_id.clone(),
        });

        // Save snapshot (None because it didn't exist)
        self.liquidity_pool_snapshots.entry(key).or_insert(None);
        self.snapshot_last_modified_key(&ledger_key);
        self.set_last_modified_key(ledger_key.clone(), self.ledger_seq);

        // Record in delta
        let ledger_entry = self.liquidity_pool_to_ledger_entry(&entry);
        self.delta.record_create(ledger_entry);

        // Insert into state
        self.liquidity_pools.insert(key, entry);

        // Track that this entry was created in this transaction (for rollback)
        self.created_liquidity_pools.insert(key);

        // Track modification
        if !self.modified_liquidity_pools.contains(&key) {
            self.modified_liquidity_pools.push(key);
        }
    }

    /// Update an existing liquidity pool entry.
    pub fn update_liquidity_pool(&mut self, entry: LiquidityPoolEntry) {
        let key = pool_id_to_bytes(&entry.liquidity_pool_id);
        let ledger_key = LedgerKey::LiquidityPool(LedgerKeyLiquidityPool {
            liquidity_pool_id: entry.liquidity_pool_id.clone(),
        });

        // Save snapshot if not already saved
        if !self.liquidity_pool_snapshots.contains_key(&key) {
            let snapshot = self.liquidity_pools.get(&key).cloned();
            self.liquidity_pool_snapshots.insert(key, snapshot);
        }
        self.capture_op_snapshot_for_key(&ledger_key);
        self.snapshot_last_modified_key(&ledger_key);

        // Get pre-state (current value BEFORE this update)
        let pre_state = self
            .liquidity_pools
            .get(&key)
            .map(|e| self.liquidity_pool_to_ledger_entry(e));

        self.set_last_modified_key(ledger_key.clone(), self.ledger_seq);

        // Record in delta with pre-state
        let post_state = self.liquidity_pool_to_ledger_entry(&entry);
        if let Some(pre) = pre_state {
            self.delta.record_update(pre, post_state);
        }

        // Update state
        self.liquidity_pools.insert(key, entry);

        // Track modification
        if !self.modified_liquidity_pools.contains(&key) {
            self.modified_liquidity_pools.push(key);
        }
    }

    /// Delete a liquidity pool entry (when pool_shares_trust_line_count reaches 0).
    pub fn delete_liquidity_pool(&mut self, pool_id: &PoolId) {
        let key = pool_id_to_bytes(pool_id);
        let ledger_key = LedgerKey::LiquidityPool(LedgerKeyLiquidityPool {
            liquidity_pool_id: pool_id.clone(),
        });

        // Save snapshot if not already saved
        if !self.liquidity_pool_snapshots.contains_key(&key) {
            let snapshot = self.liquidity_pools.get(&key).cloned();
            self.liquidity_pool_snapshots.insert(key, snapshot);
        }
        self.capture_op_snapshot_for_key(&ledger_key);
        self.snapshot_last_modified_key(&ledger_key);

        // Get pre-state (current value BEFORE deletion)
        let pre_state = self
            .liquidity_pools
            .get(&key)
            .map(|e| self.liquidity_pool_to_ledger_entry(e));

        // Record in delta with pre-state
        if let Some(pre) = pre_state {
            self.delta.record_delete(ledger_key.clone(), pre);
        }

        // Remove from state
        self.clear_entry_sponsorship_metadata(&ledger_key);
        self.liquidity_pools.remove(&key);
        self.remove_last_modified_key(&ledger_key);

        // Track modification (for proper rollback handling)
        if !self.modified_liquidity_pools.contains(&key) {
            self.modified_liquidity_pools.push(key);
        }
    }

    /// Get an entry by LedgerKey (read-only).
    pub fn get_entry(&self, key: &LedgerKey) -> Option<LedgerEntry> {
        match key {
            LedgerKey::Account(k) => self
                .get_account(&k.account_id)
                .map(|e| self.account_to_ledger_entry(e)),
            LedgerKey::Trustline(k) => {
                let account_key = account_id_to_bytes(&k.account_id);
                let asset_key = AssetKey::from_trustline_asset(&k.asset);
                self.trustlines
                    .get(&(account_key, asset_key))
                    .map(|e| self.trustline_to_ledger_entry(e))
            }
            LedgerKey::Offer(k) => self
                .get_offer(&k.seller_id, k.offer_id)
                .map(|e| self.offer_to_ledger_entry(e)),
            LedgerKey::Data(k) => {
                let name = data_name_to_string(&k.data_name);
                let account_key = account_id_to_bytes(&k.account_id);
                let result = self.get_data(&k.account_id, &name);
                tracing::debug!(
                    "get_entry for Data: account={:02x?}, name={:?}, name_bytes={:?}, found={}",
                    &account_key[..4],
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
                let lookup_key =
                    ContractDataKey::new(k.contract.clone(), k.key.clone(), k.durability);
                self.deleted_contract_data.contains(&lookup_key)
            }
            LedgerKey::ContractCode(k) => self.deleted_contract_code.contains(&k.hash.0),
            LedgerKey::Ttl(k) => self.deleted_ttl.contains(&k.key_hash.0),
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
    pub fn mark_entry_deleted(&mut self, key: &LedgerKey) {
        match key {
            LedgerKey::ContractData(k) => {
                let lookup_key =
                    ContractDataKey::new(k.contract.clone(), k.key.clone(), k.durability);
                self.deleted_contract_data.insert(lookup_key);
            }
            LedgerKey::ContractCode(k) => {
                self.deleted_contract_code.insert(k.hash.0);
            }
            LedgerKey::Ttl(k) => {
                self.deleted_ttl.insert(k.key_hash.0);
            }
            _ => {}
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
