//! Savepoint and rollback support for [`LedgerStateManager`].
//!
//! Provides the ability to create savepoints (snapshots of current state)
//! and roll back to them. Used for:
//! - Per-operation rollback within multi-op transactions
//! - Speculative orderbook exchange in path payment operations
//! - Full transaction rollback on failure

use super::*;

/// Restore entries from snapshots created after the savepoint.
///
/// For each key present in `current_snapshots` but not in `savepoint_snapshots`,
/// the snapshot value is applied to `live_map` (inserted if Some, removed if None).
fn rollback_new_snapshots<K, V>(
    live_map: &mut HashMap<K, V>,
    current_snapshots: &HashMap<K, Option<V>>,
    savepoint_snapshots: &HashMap<K, Option<V>>,
) where
    K: Eq + std::hash::Hash + Clone,
    V: Clone,
{
    for (key, snapshot) in current_snapshots {
        if !savepoint_snapshots.contains_key(key) {
            match snapshot {
                Some(entry) => {
                    live_map.insert(key.clone(), entry.clone());
                }
                None => {
                    live_map.remove(key);
                }
            }
        }
    }
}

/// Restore pre-savepoint values for entries modified before the savepoint.
fn apply_pre_values<K, V>(live_map: &mut HashMap<K, V>, pre_values: Vec<(K, Option<V>)>)
where
    K: Eq + std::hash::Hash,
{
    for (key, value) in pre_values {
        match value {
            Some(entry) => {
                live_map.insert(key, entry);
            }
            None => {
                live_map.remove(&key);
            }
        }
    }
}

/// Rollback a set of entries from their snapshot map.
///
/// For each snapshotted key: if the key is in `created`, remove the entry
/// from the live map (it was created during this transaction). Otherwise
/// restore the pre-transaction value from the snapshot.
pub(super) fn rollback_entries<K, V>(
    live_map: &mut HashMap<K, V>,
    snapshots: &mut HashMap<K, Option<V>>,
    created: &mut HashSet<K>,
) where
    K: Eq + std::hash::Hash,
{
    for (key, snapshot) in snapshots.drain() {
        if created.contains(&key) {
            live_map.remove(&key);
        } else if let Some(entry) = snapshot {
            live_map.insert(key, entry);
        }
    }
    created.clear();
}

/// Savepoint for rolling back state modifications within a transaction.
///
/// Used for per-operation rollback (failed operations have their state changes
/// undone so subsequent operations see clean state) and by
/// `convert_with_offers_and_pools` for speculative orderbook exchange.
///
/// The savepoint captures:
/// - Snapshot maps (to restore snapshot tracking state)
/// - Current entry values for snapshot'd entries (pre-savepoint values)
/// - Delta vector lengths (for truncation)
/// - Modified tracking vec lengths
/// - Entry metadata snapshot state
/// - Created entry sets
/// - ID pool value
pub struct Savepoint {
    // Snapshot maps clones (small: only entries modified earlier in TX)
    pub(super) offer_snapshots: HashMap<OfferKey, Option<OfferRecord>>,
    pub(super) account_snapshots: HashMap<AccountId, Option<AccountEntry>>,
    pub(super) trustline_snapshots: HashMap<TrustlineKey, Option<TrustLineEntry>>,
    pub(super) ttl_snapshots: HashMap<Hash, Option<TtlEntry>>,
    // Pre-savepoint values of entries in snapshot maps.
    pub(super) offer_pre_values: Vec<(OfferKey, Option<OfferRecord>)>,
    pub(super) account_pre_values: Vec<(AccountId, Option<AccountEntry>)>,
    pub(super) trustline_pre_values: Vec<(TrustlineKey, Option<TrustLineEntry>)>,
    pub(super) ttl_pre_values: Vec<(Hash, Option<TtlEntry>)>,

    // EntryStore-based savepoints
    pub(super) claimable_balances: EntryStoreSavepoint<ClaimableBalanceId, ClaimableBalanceEntry>,
    pub(super) liquidity_pools: EntryStoreSavepoint<PoolId, LiquidityPoolEntry>,
    pub(super) contract_code: EntryStoreSavepoint<Hash, ContractCodeEntry>,
    pub(super) contract_data: EntryStoreSavepoint<StorageKey, ContractDataEntry>,
    pub(super) data_entries: EntryStoreSavepoint<DataKey, DataEntry>,

    // Created entry sets
    pub(super) created_offers: HashSet<OfferKey>,
    pub(super) created_accounts: HashSet<AccountId>,
    pub(super) created_trustlines: HashSet<TrustlineKey>,
    pub(super) created_ttl: HashSet<Hash>,
    // Delta vector lengths for truncation
    pub(super) delta_lengths: ChangeLogLengths,

    // Modified tracking vec lengths
    pub(super) modified_accounts_len: usize,
    pub(super) modified_trustlines_len: usize,
    pub(super) modified_offers_len: usize,
    pub(super) modified_ttl_len: usize,
    // Entry metadata snapshots
    pub(super) entry_last_modified_snapshots: HashMap<LedgerKey, Option<u32>>,
    pub(super) entry_last_modified_pre_values: Vec<(LedgerKey, Option<u32>)>,
    pub(super) entry_sponsorship_snapshots: HashMap<LedgerKey, Option<AccountId>>,
    pub(super) entry_sponsorship_ext_snapshots: HashMap<LedgerKey, bool>,
    pub(super) entry_sponsorship_pre_values: Vec<(LedgerKey, Option<AccountId>)>,
    pub(super) entry_sponsorship_ext_pre_values: Vec<(LedgerKey, bool)>,

    // Op entry snapshot keys (to remove entries added during speculation)
    pub(super) op_entry_snapshot_keys: HashSet<LedgerKey>,

    // ID pool value for rollback
    pub(super) id_pool: u64,
}

// ==================== Savepoint & Rollback Methods ====================

impl LedgerStateManager {
    /// Create a savepoint capturing current state for potential rollback.
    ///
    /// Used for two purposes:
    /// 1. **Per-operation rollback**: Each operation in a multi-op transaction gets
    ///    a savepoint. If the operation fails, `rollback_to_savepoint()` undoes all
    ///    state changes so subsequent operations see clean state (matching stellar-core nested
    ///    `LedgerTxn` behavior).
    /// 2. **Path payment speculation**: `convert_with_offers_and_pools` runs the
    ///    orderbook path speculatively, rolling back if the pool provides a better rate.
    ///
    /// The savepoint records the current values of all modified entries so
    /// they can be restored if the operation fails or the speculative path is abandoned.
    pub fn create_savepoint(&self) -> Savepoint {
        Savepoint {
            // Clone snapshot maps (small: only entries modified in current TX)
            offer_snapshots: self.offer_snapshots.clone(),
            account_snapshots: self.account_snapshots.clone(),
            trustline_snapshots: self.trustline_snapshots.clone(),
            ttl_snapshots: self.ttl_snapshots.clone(),

            // Save current values of entries in snapshot maps (pre-savepoint values)
            offer_pre_values: {
                let store = self.offer_store_lock();
                self.offer_snapshots
                    .keys()
                    .map(|k| (k.clone(), store.get(k).cloned()))
                    .collect()
            },
            account_pre_values: self
                .account_snapshots
                .keys()
                .map(|k| (k.clone(), self.accounts.get(k).cloned()))
                .collect(),
            trustline_pre_values: self
                .trustline_snapshots
                .keys()
                .map(|k| (k.clone(), self.trustlines.get(k).cloned()))
                .collect(),
            ttl_pre_values: self
                .ttl_snapshots
                .keys()
                .map(|k| (k.clone(), self.ttl_entries.get(k).cloned()))
                .collect(),

            // EntryStore-based savepoints
            claimable_balances: self.claimable_balances.create_savepoint(),
            liquidity_pools: self.liquidity_pools.create_savepoint(),
            contract_code: self.contract_code.create_savepoint(),
            contract_data: self.contract_data.create_savepoint(),
            data_entries: self.data_entries.create_savepoint(),

            // Created entry sets
            created_offers: self.created_offers.clone(),
            created_accounts: self.created_accounts.clone(),
            created_trustlines: self.created_trustlines.clone(),
            created_ttl: self.created_ttl.clone(),

            // Delta and modified vec lengths
            delta_lengths: self.delta.snapshot_lengths(),
            modified_accounts_len: self.modified_accounts.len(),
            modified_trustlines_len: self.modified_trustlines.len(),
            modified_offers_len: self.modified_offers.len(),
            // modified_data_len is handled internally by data_entries.create_savepoint()
            modified_ttl_len: self.modified_ttl.len(),

            // Entry metadata
            entry_last_modified_snapshots: self.entry_last_modified_snapshots.clone(),
            entry_last_modified_pre_values: self
                .entry_last_modified_snapshots
                .keys()
                .map(|k| (k.clone(), self.get_last_modified(k)))
                .collect(),
            entry_sponsorship_snapshots: self.entry_sponsorship_snapshots.clone(),
            entry_sponsorship_ext_snapshots: self.entry_sponsorship_ext_snapshots.clone(),
            entry_sponsorship_pre_values: self
                .entry_sponsorship_snapshots
                .keys()
                .map(|k| (k.clone(), self.get_entry_sponsorship(k)))
                .collect(),
            entry_sponsorship_ext_pre_values: self
                .entry_sponsorship_ext_snapshots
                .keys()
                .map(|k| (k.clone(), self.contains_sponsorship_ext(k)))
                .collect(),

            op_entry_snapshot_keys: self.op_entry_snapshots.keys().cloned().collect(),
            id_pool: self.id_pool,
        }
    }

    /// Rollback state to a previously created savepoint.
    ///
    /// Undoes all modifications made since the savepoint was created,
    /// restoring entries to their pre-speculation values. This is O(k)
    /// where k = entries modified during speculation (typically < 50),
    /// compared to O(n) for cloning 911K+ offers.
    pub fn rollback_to_savepoint(&mut self, sp: Savepoint) {
        // Phase 1: Restore entries newly snapshot'd since the savepoint.
        // These entries have snapshots added after the savepoint, so their
        // snapshot values ARE their pre-savepoint (= pre-TX) values.

        // Offers require special handling for aa_index and offer_index
        self.rollback_offer_snapshots(&sp);
        rollback_new_snapshots(
            &mut self.accounts,
            &self.account_snapshots,
            &sp.account_snapshots,
        );
        rollback_new_snapshots(
            &mut self.trustlines,
            &self.trustline_snapshots,
            &sp.trustline_snapshots,
        );
        // data_entries uses EntryStore — handled below
        rollback_new_snapshots(
            &mut self.ttl_entries,
            &self.ttl_snapshots,
            &sp.ttl_snapshots,
        );

        // Phase 2: Restore pre-savepoint values for entries already in snapshot maps.
        // These were modified before the savepoint AND potentially re-modified since.
        self.apply_offer_pre_values(sp.offer_pre_values);
        apply_pre_values(&mut self.accounts, sp.account_pre_values);
        apply_pre_values(&mut self.trustlines, sp.trustline_pre_values);
        // data_entries pre_values handled by EntryStore rollback below
        apply_pre_values(&mut self.ttl_entries, sp.ttl_pre_values);

        // EntryStore-based rollbacks (handles phases 1-3 + modified truncation internally)
        self.claimable_balances
            .rollback_to_savepoint(sp.claimable_balances);
        self.liquidity_pools
            .rollback_to_savepoint(sp.liquidity_pools);
        self.contract_code.rollback_to_savepoint(sp.contract_code);
        self.contract_data.rollback_to_savepoint(sp.contract_data);
        self.data_entries.rollback_to_savepoint(sp.data_entries);

        // Phase 3: Restore snapshot maps and created sets
        self.offer_snapshots = sp.offer_snapshots;
        self.account_snapshots = sp.account_snapshots;
        self.trustline_snapshots = sp.trustline_snapshots;
        self.ttl_snapshots = sp.ttl_snapshots;

        self.created_offers = sp.created_offers;
        self.created_accounts = sp.created_accounts;
        self.created_trustlines = sp.created_trustlines;
        self.created_ttl = sp.created_ttl;

        // Phase 4: Truncate delta
        self.delta.truncate_to(&sp.delta_lengths);

        // Phase 5: Truncate modified tracking vecs
        self.modified_accounts.truncate(sp.modified_accounts_len);
        self.modified_trustlines
            .truncate(sp.modified_trustlines_len);
        self.modified_offers.truncate(sp.modified_offers_len);
        // modified_data truncation handled by data_entries.rollback_to_savepoint() above
        self.modified_ttl.truncate(sp.modified_ttl_len);

        // Phase 6: Restore entry metadata.
        // For offer keys, metadata is restored via OfferRecord snapshots (in rollback_offer_snapshots).
        // For non-offer keys, use the standard rollback helpers.
        // Filter to only non-offer keys for the non-offer maps.
        let non_offer_lm_pre: Vec<_> = sp
            .entry_last_modified_pre_values
            .into_iter()
            .filter(|(k, _)| !Self::is_offer_key(k))
            .collect();
        rollback_new_snapshots(
            &mut self.entry_last_modified,
            &self.entry_last_modified_snapshots,
            &sp.entry_last_modified_snapshots,
        );
        apply_pre_values(&mut self.entry_last_modified, non_offer_lm_pre);
        self.entry_last_modified_snapshots = sp.entry_last_modified_snapshots;

        let non_offer_sp_pre: Vec<_> = sp
            .entry_sponsorship_pre_values
            .into_iter()
            .filter(|(k, _)| !Self::is_offer_key(k))
            .collect();
        rollback_new_snapshots(
            &mut self.entry_sponsorships,
            &self.entry_sponsorship_snapshots,
            &sp.entry_sponsorship_snapshots,
        );
        apply_pre_values(&mut self.entry_sponsorships, non_offer_sp_pre);
        self.entry_sponsorship_snapshots = sp.entry_sponsorship_snapshots;

        // For sponsorship ext (bool-based set)
        for (key, &was_present) in &self.entry_sponsorship_ext_snapshots {
            if !sp.entry_sponsorship_ext_snapshots.contains_key(key) && !Self::is_offer_key(key) {
                if was_present {
                    self.entry_sponsorship_ext.insert(key.clone());
                } else {
                    self.entry_sponsorship_ext.remove(key);
                }
            }
        }
        let non_offer_ext_pre: Vec<_> = sp
            .entry_sponsorship_ext_pre_values
            .into_iter()
            .filter(|(k, _)| !Self::is_offer_key(k))
            .collect();
        for (key, was_present) in non_offer_ext_pre {
            if was_present {
                self.entry_sponsorship_ext.insert(key);
            } else {
                self.entry_sponsorship_ext.remove(&key);
            }
        }
        self.entry_sponsorship_ext_snapshots = sp.entry_sponsorship_ext_snapshots;

        // Phase 7: Restore op entry snapshots and id_pool
        self.op_entry_snapshots
            .retain(|k, _| sp.op_entry_snapshot_keys.contains(k));
        self.id_pool = sp.id_pool;
    }

    /// Rollback offer snapshots created since the savepoint.
    /// Restores full OfferRecords (including metadata) to the shared OfferStore.
    fn rollback_offer_snapshots(&mut self, sp: &Savepoint) {
        let new_offer_snapshots: Vec<_> = self
            .offer_snapshots
            .iter()
            .filter(|(k, _)| !sp.offer_snapshots.contains_key(k))
            .map(|(key, snap)| (key.clone(), snap.clone()))
            .collect();
        let mut store = self.offer_store_lock();
        for (key, snapshot) in new_offer_snapshots {
            match snapshot {
                Some(record) => {
                    store.insert_record(record);
                }
                None => {
                    store.remove(&key);
                }
            }
        }
    }

    /// Apply offer pre-savepoint values, restoring full OfferRecords to the shared OfferStore.
    fn apply_offer_pre_values(&mut self, pre_values: Vec<(OfferKey, Option<OfferRecord>)>) {
        let mut store = self.offer_store_lock();
        for (key, value) in pre_values {
            match value {
                Some(record) => {
                    store.insert_record(record);
                }
                None => {
                    store.remove(&key);
                }
            }
        }
    }

    // ==================== Rollback Support ====================

    /// Rollback all changes since the state manager was created.
    ///
    /// This restores all entries to their original state and clears the delta.
    pub fn rollback(&mut self) {
        // Restore id_pool snapshot if present (must be done before entry snapshots
        // since offer IDs need to be correct for subsequent transactions)
        if let Some(snapshot) = self.id_pool_snapshot.take() {
            self.id_pool = snapshot;
        }

        rollback_entries(
            &mut self.accounts,
            &mut self.account_snapshots,
            &mut self.created_accounts,
        );
        rollback_entries(
            &mut self.trustlines,
            &mut self.trustline_snapshots,
            &mut self.created_trustlines,
        );

        // Restore offer snapshots to the shared OfferStore.
        let offer_snapshots: Vec<_> = self.offer_snapshots.drain().collect();
        {
            let mut store = self.offer_store_lock();
            for (key, snapshot) in offer_snapshots {
                if self.created_offers.contains(&key) {
                    // Offer was created in this transaction — remove it.
                    store.remove(&key);
                } else if let Some(record) = snapshot {
                    // Offer existed before — restore the full record (entry + metadata).
                    store.insert_record(record);
                }
            }
        }
        self.created_offers.clear();

        self.data_entries.rollback();
        self.contract_data.rollback();
        self.contract_code.rollback();
        rollback_entries(
            &mut self.ttl_entries,
            &mut self.ttl_snapshots,
            &mut self.created_ttl,
        );

        // Restore deferred RO TTL bumps to pre-transaction state.
        // In stellar-core, commitChangesFromSuccessfulOp is only called for
        // successful TXs. Failed TXs do not commit RO TTL bumps to
        // mRoTTLBumps. We restore the snapshot to match this behavior.
        if let Some(snapshot) = self.deferred_ro_ttl_bumps_snapshot.take() {
            self.deferred_ro_ttl_bumps = snapshot;
        } else {
            self.deferred_ro_ttl_bumps.clear();
        }

        self.claimable_balances.rollback();
        self.liquidity_pools.rollback();

        // Restore entry sponsorship snapshots
        let sponsorship_snaps: Vec<_> = self.entry_sponsorship_snapshots.drain().collect();
        for (key, snapshot) in sponsorship_snaps {
            match snapshot {
                Some(entry) => {
                    self.insert_entry_sponsorship(key, entry);
                }
                None => {
                    self.remove_entry_sponsorship(&key);
                }
            }
        }

        // Restore sponsorship extension snapshots
        let ext_snaps: Vec<_> = self.entry_sponsorship_ext_snapshots.drain().collect();
        for (key, snapshot) in ext_snaps {
            if snapshot {
                self.insert_sponsorship_ext(key);
            } else {
                self.remove_sponsorship_ext(&key);
            }
        }

        // Restore last modified snapshots
        let lm_snaps: Vec<_> = self.entry_last_modified_snapshots.drain().collect();
        for (key, snapshot) in lm_snaps {
            match snapshot {
                Some(seq) => {
                    self.insert_last_modified(key, seq);
                }
                None => {
                    self.remove_last_modified(&key);
                }
            }
        }

        // Clear modification tracking
        self.modified_accounts.clear();
        self.modified_trustlines.clear();
        self.modified_offers.clear();
        // modified_data is cleared by data_entries.rollback() above
        self.modified_ttl.clear();

        // Restore delta from snapshot if available, otherwise reset it.
        // This preserves committed changes from previous transactions in this ledger.
        // The fee for the current transaction was already added during fee deduction
        // phase (before operations ran) and is restored via restore_delta_entries()
        // in execution.rs after rollback() returns.
        if let Some(snapshot) = self.delta_snapshot.take() {
            // Truncate delta vectors back to pre-TX lengths (O(1) instead of clone).
            self.delta.truncate_to(&snapshot.lengths);
            // Restore fee_charged to pre-TX value.
            self.delta.set_fee_charged(snapshot.fee_charged);
        } else {
            // No snapshot - reset delta but preserve fee_charged.
            let fee_charged = self.delta.fee_charged();
            self.delta = TxChangeLog::new(self.ledger_seq);
            if fee_charged != 0 {
                self.delta.add_fee(fee_charged);
            }
        }
    }
}
