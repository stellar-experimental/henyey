//! Transactional ledger state for close operations.
//!
//! [`LedgerTxn`] is a nested, transactional abstraction over the ledger state
//! that mirrors stellar-core's `AbstractLedgerTxn` / `LedgerTxn` hierarchy.
//! Every read walks: current delta → committed chain → base snapshot, making
//! stale reads structurally impossible.
//!
//! # Nesting
//!
//! Nesting is achieved via move semantics:
//!
//! ```text
//! let (child, restore) = ltx.child();  // parent consumed
//! // ... reads/writes through child ...
//! ltx = child.commit(restore);         // or LedgerTxn::rollback(restore)
//! ```
//!
//! The parent is moved into an opaque [`LedgerTxnRestore`] token, enforcing
//! the one-active-child invariant at compile time.
//!
//! # Parity
//!
//! Direct structural correspondence with stellar-core's `LedgerTxn`:
//!
//! | stellar-core            | Henyey                          |
//! |-------------------------|---------------------------------|
//! | `LedgerTxnRoot`         | `LedgerTxn::begin()`           |
//! | `LedgerTxn(parent)`     | `ltx.child()`                  |
//! | `commit()`              | `child.commit(restore)`         |
//! | `rollback()`            | `LedgerTxn::rollback(restore)` |
//! | `load(key)`             | `ltx.get_entry(key)`           |

use crate::delta::{DeltaCategorization, EntryChange, LedgerDelta};
use crate::snapshot::SnapshotHandle;
use crate::{LedgerError, Result};
use stellar_xdr::curr::{
    AccountEntry, AccountId, ConfigSettingEntry, ConfigSettingId, LedgerEntry, LedgerEntryChange,
    LedgerEntryChanges, LedgerEntryData, LedgerHeader, LedgerKey, LedgerKeyConfigSetting, VecM,
};

/// A transactional view of ledger state during close.
///
/// All reads resolve: current delta → committed chain → base snapshot.
/// All writes accumulate in the current delta.
pub struct LedgerTxn {
    /// Frozen base state (shared across all nesting levels).
    snapshot: SnapshotHandle,

    /// Deltas committed by outer/prior nesting levels (newest last).
    /// On child creation, parent's current delta is pushed here.
    committed: Vec<LedgerDelta>,

    /// This level's uncommitted changes.
    current: LedgerDelta,

    /// Post-upgrade header, updated as upgrades are applied.
    header: LedgerHeader,

    /// Hash of the previous ledger header (for parity with stellar-core).
    header_hash: henyey_common::Hash256,

    /// Ledger sequence for this close.
    ledger_seq: u32,
}

/// Opaque token holding the parent's frozen state.
///
/// Must be consumed by exactly one of [`LedgerTxn::commit`] or
/// [`LedgerTxn::rollback`]. Enforced by Rust's ownership system.
pub struct LedgerTxnRestore {
    snapshot: SnapshotHandle,
    committed: Vec<LedgerDelta>,
    parent_delta: LedgerDelta,
    header: LedgerHeader,
    header_hash: henyey_common::Hash256,
    ledger_seq: u32,
}

/// Post-drain state for the remaining commit operations.
///
/// Created by [`LedgerTxn::drain_for_bucket_update`] after flattening all
/// committed deltas into a single delta and draining entry changes for the
/// bucket list.
pub struct LedgerTxnFinal {
    pub header: LedgerHeader,
    pub header_hash: henyey_common::Hash256,
    pub snapshot: SnapshotHandle,
    pub fee_pool_delta: i64,
    pub total_coins_delta: i64,
}

impl LedgerTxn {
    // ------------------------------------------------------------------
    // Construction & nesting
    // ------------------------------------------------------------------

    /// Create a root transaction for ledger close.
    pub fn begin(
        snapshot: SnapshotHandle,
        header: LedgerHeader,
        header_hash: henyey_common::Hash256,
        ledger_seq: u32,
    ) -> Self {
        Self {
            snapshot,
            committed: Vec::new(),
            current: LedgerDelta::new(ledger_seq),
            header,
            header_hash,
            ledger_seq,
        }
    }

    /// Spawn a nested child. Moves `self` into the restore token.
    ///
    /// The child sees all of the parent's committed + current changes through
    /// its read path (current delta is pushed onto the committed chain).
    pub fn child(self) -> (LedgerTxn, LedgerTxnRestore) {
        let LedgerTxn {
            snapshot,
            committed,
            current,
            header,
            header_hash,
            ledger_seq,
        } = self;

        // Save the parent's committed chain and current delta for restore.
        let parent_committed = committed;
        let parent_delta = current;

        // The child's committed chain is parent's committed + parent's current
        // delta (so the child can read parent's uncommitted changes).
        let mut child_committed = parent_committed.clone();
        child_committed.push(parent_delta.clone());

        let restore = LedgerTxnRestore {
            snapshot: snapshot.clone(),
            committed: parent_committed,
            parent_delta,
            header: header.clone(),
            header_hash,
            ledger_seq,
        };

        let child = LedgerTxn {
            snapshot,
            committed: child_committed,
            current: LedgerDelta::new(ledger_seq),
            header,
            header_hash,
            ledger_seq,
        };

        (child, restore)
    }

    /// Commit child's changes: merge `child.current` into parent's current delta.
    ///
    /// The parent is reconstituted from the restore token with its current delta
    /// updated to include all of the child's changes.
    pub fn commit(self, restore: LedgerTxnRestore) -> LedgerTxn {
        let child_current = self.current;
        let child_header = self.header;

        let LedgerTxnRestore {
            snapshot,
            committed,
            mut parent_delta,
            header_hash,
            ledger_seq,
            ..
        } = restore;

        // Merge child's changes into the parent's current delta.
        parent_delta
            .merge(child_current)
            .expect("LedgerTxn::commit: merge failed");

        // Merge fee/coins deltas are handled by LedgerDelta::merge.

        LedgerTxn {
            snapshot,
            committed,
            current: parent_delta,
            header: child_header,
            header_hash,
            ledger_seq,
        }
    }

    /// Rollback: discard child's changes, reconstitute parent unchanged.
    pub fn rollback(restore: LedgerTxnRestore) -> LedgerTxn {
        let LedgerTxnRestore {
            snapshot,
            committed,
            parent_delta,
            header,
            header_hash,
            ledger_seq,
        } = restore;

        LedgerTxn {
            snapshot,
            committed,
            current: parent_delta,
            header,
            header_hash,
            ledger_seq,
        }
    }

    // ------------------------------------------------------------------
    // Read path
    // ------------------------------------------------------------------

    /// The primary way to read ledger entries during close.
    ///
    /// Resolves: current → committed (newest first) → snapshot.
    pub fn get_entry(&self, key: &LedgerKey) -> Result<Option<LedgerEntry>> {
        // 1. Check current (uncommitted) delta
        if let Some(change) = self.current.get_change(key) {
            return Ok(change.current_entry().cloned());
        }
        // 2. Walk committed chain newest-first
        for delta in self.committed.iter().rev() {
            if let Some(change) = delta.get_change(key) {
                return Ok(change.current_entry().cloned());
            }
        }
        // 3. Fall back to base snapshot
        self.snapshot.get_entry(key)
    }

    /// Load an account by ID.
    pub fn get_account(&self, account_id: &AccountId) -> Result<Option<AccountEntry>> {
        let key = LedgerKey::Account(stellar_xdr::curr::LedgerKeyAccount {
            account_id: account_id.clone(),
        });
        match self.get_entry(&key)? {
            Some(entry) => {
                if let LedgerEntryData::Account(acc) = entry.data {
                    Ok(Some(acc))
                } else {
                    Ok(None)
                }
            }
            None => Ok(None),
        }
    }

    /// Load a config setting by ID.
    pub fn get_config_setting(&self, id: ConfigSettingId) -> Result<Option<ConfigSettingEntry>> {
        let key = LedgerKey::ConfigSetting(LedgerKeyConfigSetting {
            config_setting_id: id,
        });
        match self.get_entry(&key)? {
            Some(entry) => {
                if let LedgerEntryData::ConfigSetting(config) = entry.data {
                    Ok(Some(config))
                } else {
                    Ok(None)
                }
            }
            None => Ok(None),
        }
    }

    /// Access the ledger header (may have been updated by upgrades).
    pub fn header(&self) -> &LedgerHeader {
        &self.header
    }

    /// Protocol version from the header.
    pub fn protocol_version(&self) -> u32 {
        self.header.ledger_version
    }

    /// Ledger sequence number.
    pub fn ledger_seq(&self) -> u32 {
        self.ledger_seq
    }

    /// Previous ledger header hash.
    pub fn header_hash(&self) -> &henyey_common::Hash256 {
        &self.header_hash
    }

    /// Access the underlying snapshot for parallel execution paths.
    pub fn snapshot(&self) -> &SnapshotHandle {
        &self.snapshot
    }

    /// Batch load entries from the snapshot (pass-through for prefetching).
    pub fn load_entries(&self, keys: &[LedgerKey]) -> Result<Vec<LedgerEntry>> {
        self.snapshot.load_entries(keys)
    }

    /// Prefetch entries into the snapshot cache (pass-through).
    pub fn prefetch(&self, keys: &[LedgerKey]) -> Result<crate::snapshot::PrefetchStats> {
        self.snapshot.prefetch(keys)
    }

    /// Merged enumeration of all offers: snapshot offers overlaid with delta changes.
    ///
    /// Returns all live offer entries visible through the full read path:
    /// current delta + committed chain + snapshot.
    pub fn all_offers(&self) -> Result<Vec<LedgerEntry>> {
        // Start with all offers from the base snapshot
        let snapshot_entries = self.snapshot.all_entries()?;

        // Collect all changes from committed chain + current into a single view
        let mut overrides: std::collections::HashMap<LedgerKey, Option<LedgerEntry>> =
            std::collections::HashMap::new();

        for delta in &self.committed {
            for change in delta.changes() {
                let key = change.key();
                if is_offer_key(&key) {
                    overrides.insert(key, change.current_entry().cloned());
                }
            }
        }
        for change in self.current.changes() {
            let key = change.key();
            if is_offer_key(&key) {
                overrides.insert(key, change.current_entry().cloned());
            }
        }

        if overrides.is_empty() {
            return Ok(snapshot_entries);
        }

        // Build merged result
        let mut result: Vec<LedgerEntry> = Vec::with_capacity(snapshot_entries.len());

        // Add/replace snapshot entries
        for entry in snapshot_entries {
            let key = henyey_common::entry_to_key(&entry);
            if let Some(override_val) = overrides.remove(&key) {
                if let Some(live_entry) = override_val {
                    result.push(live_entry);
                }
                // If None (deleted), skip this entry
            } else {
                result.push(entry);
            }
        }

        // Add any newly created offers not in the snapshot
        for (_, entry_opt) in overrides {
            if let Some(entry) = entry_opt {
                result.push(entry);
            }
        }

        Ok(result)
    }

    /// Offers filtered by account and asset (pass-through to snapshot, then overlay deltas).
    pub fn offers_by_account_and_asset(
        &self,
        account_id: &AccountId,
        asset: &stellar_xdr::curr::Asset,
    ) -> Result<Vec<LedgerEntry>> {
        // For now, delegate to the snapshot and overlay delta changes.
        // This is less common than all_offers() and the index is snapshot-only.
        let mut offers = self
            .snapshot
            .offers_by_account_and_asset(account_id, asset)?;

        // Apply delta changes
        let mut i = 0;
        while i < offers.len() {
            let key = henyey_common::entry_to_key(&offers[i]);
            if let Some(change) = self.get_delta_change(&key) {
                match change.current_entry() {
                    Some(entry) => {
                        offers[i] = entry.clone();
                        i += 1;
                    }
                    None => {
                        // Deleted
                        offers.swap_remove(i);
                    }
                }
            } else {
                i += 1;
            }
        }

        Ok(offers)
    }

    /// Pool share trustlines by account (pass-through to snapshot).
    pub fn pool_share_tls_by_account(
        &self,
        account_id: &AccountId,
    ) -> Result<Vec<stellar_xdr::curr::PoolId>> {
        self.snapshot.pool_share_tls_by_account(account_id)
    }

    /// Get all current entries (Created + Updated) from the full delta chain.
    ///
    /// Used to build `PriorStageState` for parallel Soroban execution.
    pub fn current_entries(&self) -> Vec<LedgerEntry> {
        let mut entries = Vec::new();
        // Gather from committed chain
        for delta in &self.committed {
            entries.extend(delta.current_entries());
        }
        // Gather from current
        entries.extend(self.current.current_entries());
        entries
    }

    /// Build [`PriorStageState`] from the current delta state.
    pub fn prior_stage_state(&self) -> PriorStageState {
        // Need to flatten all deltas for a coherent view
        let mut entries = Vec::new();
        let mut deleted_keys = Vec::new();

        for delta in &self.committed {
            for change in delta.changes() {
                match change {
                    EntryChange::Created(e) => entries.push(e.clone()),
                    EntryChange::Updated { current, .. } => {
                        entries.push(current.as_ref().clone());
                    }
                    EntryChange::Deleted { .. } => {
                        deleted_keys.push(change.key());
                    }
                }
            }
        }
        for change in self.current.changes() {
            match change {
                EntryChange::Created(e) => entries.push(e.clone()),
                EntryChange::Updated { current, .. } => entries.push(current.as_ref().clone()),
                EntryChange::Deleted { .. } => deleted_keys.push(change.key()),
            }
        }

        PriorStageState {
            entries,
            deleted_keys,
        }
    }

    // ------------------------------------------------------------------
    // Write path
    // ------------------------------------------------------------------

    /// Record a new entry creation.
    pub fn record_create(&mut self, entry: LedgerEntry) -> Result<()> {
        self.current.record_create(entry)
    }

    /// Record an entry update.
    pub fn record_update(&mut self, previous: LedgerEntry, current: LedgerEntry) -> Result<()> {
        self.current.record_update(previous, current)
    }

    /// Record an entry deletion.
    pub fn record_delete(&mut self, entry: LedgerEntry) -> Result<()> {
        self.current.record_delete(entry)
    }

    /// Update the header via a closure.
    pub fn update_header(&mut self, f: impl FnOnce(&mut LedgerHeader)) {
        f(&mut self.header);
    }

    /// Accumulate fee pool change.
    pub fn record_fee_pool_delta(&mut self, amount: i64) {
        self.current.record_fee_pool_delta(amount);
    }

    /// Accumulate total coins change.
    pub fn record_total_coins_delta(&mut self, amount: i64) {
        self.current.record_total_coins_delta(amount);
    }

    /// Deduct a fee from an account entry.
    ///
    /// Reads the account from the full chain (current → committed → snapshot),
    /// deducts `min(balance, fee)`, and records the update in the current delta.
    ///
    /// Returns `(charged_fee, fee_changes)` for metadata generation.
    pub fn deduct_fee_from_account(
        &mut self,
        account_id: &AccountId,
        fee: i64,
    ) -> Result<(i64, LedgerEntryChanges)> {
        let key = LedgerKey::Account(stellar_xdr::curr::LedgerKeyAccount {
            account_id: account_id.clone(),
        });

        // Load the account from the full chain.
        let entry = match self.get_entry(&key)? {
            Some(e) => e,
            None => return Ok((0, LedgerEntryChanges(VecM::default()))),
        };

        let balance = if let LedgerEntryData::Account(ref acc) = entry.data {
            acc.balance
        } else {
            return Ok((0, LedgerEntryChanges(VecM::default())));
        };

        let charged_fee = std::cmp::min(balance, fee);
        if charged_fee == 0 {
            return Ok((0, LedgerEntryChanges(VecM::default())));
        }

        let state_entry = entry.clone();
        let mut updated = entry;

        if let LedgerEntryData::Account(ref mut acc) = updated.data {
            henyey_common::checked_types::sub_account_balance(acc, charged_fee)
                .expect("fee underflow after capping to balance");
        }
        updated.last_modified_ledger_seq = self.ledger_seq;

        let fee_changes = {
            let changes_vec = vec![
                LedgerEntryChange::State(state_entry.clone()),
                LedgerEntryChange::Updated(updated.clone()),
            ];
            LedgerEntryChanges(changes_vec.try_into().map_err(|_| {
                LedgerError::Internal("fee changes vec conversion failed".to_string())
            })?)
        };

        // Record in the current delta. We need to determine the "previous" for
        // the delta's coalescing logic: if the account already exists in the
        // current delta or committed chain, the previous is the state_entry we
        // loaded. If it's new to the delta, it's an Update from snapshot state.
        self.current.record_update(state_entry, updated)?;

        Ok((charged_fee, fee_changes))
    }

    /// Apply a fee refund to an account already in the delta.
    pub fn apply_refund_to_account(&mut self, account_id: &AccountId, refund: i64) -> Result<()> {
        self.current.apply_refund_to_account(account_id, refund)
    }

    // ------------------------------------------------------------------
    // Meta helpers
    // ------------------------------------------------------------------

    /// Convert the current delta's changes to XDR `LedgerEntryChanges`.
    ///
    /// Used to capture per-upgrade metadata before `commit()`.
    pub fn current_changes_as_entry_changes(&self) -> LedgerEntryChanges {
        let mut changes: Vec<LedgerEntryChange> = Vec::new();
        for change in self.current.changes() {
            match change {
                EntryChange::Created(entry) => {
                    changes.push(LedgerEntryChange::Created(entry.clone()));
                }
                EntryChange::Updated { previous, current } => {
                    changes.push(LedgerEntryChange::State(previous.clone()));
                    changes.push(LedgerEntryChange::Updated(current.as_ref().clone()));
                }
                EntryChange::Deleted { previous } => {
                    let key = henyey_common::entry_to_key(previous);
                    changes.push(LedgerEntryChange::State(previous.clone()));
                    changes.push(LedgerEntryChange::Removed(key));
                }
            }
        }
        LedgerEntryChanges(changes.try_into().unwrap_or_default())
    }

    // ------------------------------------------------------------------
    // Terminal operations
    // ------------------------------------------------------------------

    /// Flatten all committed deltas + current into a single delta,
    /// then drain for bucket list update.
    ///
    /// This consumes the `LedgerTxn` and returns the categorized entries for
    /// bucket list update plus a `LedgerTxnFinal` with the remaining metadata.
    pub fn drain_for_bucket_update(self) -> (DeltaCategorization, LedgerTxnFinal) {
        let LedgerTxn {
            snapshot,
            committed,
            current,
            header,
            header_hash,
            ..
        } = self;

        // Merge all committed deltas into current (oldest first).
        // We merge in reverse order: start with a fresh delta, merge committed
        // oldest-first, then merge current on top.
        let mut flat = LedgerDelta::new(current.ledger_seq());
        for delta in committed {
            flat.merge(delta)
                .expect("LedgerTxn::drain_for_bucket_update: merge committed failed");
        }
        flat.merge(current)
            .expect("LedgerTxn::drain_for_bucket_update: merge current failed");

        let fee_pool_delta = flat.fee_pool_delta();
        let total_coins_delta = flat.total_coins_delta();

        let cat = flat.drain_categorization_for_bucket_update();

        let final_state = LedgerTxnFinal {
            header,
            header_hash,
            snapshot,
            fee_pool_delta,
            total_coins_delta,
        };

        (cat, final_state)
    }

    /// Access the current delta's number of changes (for logging/debugging).
    pub fn num_changes(&self) -> usize {
        let mut total = self.current.num_changes();
        for delta in &self.committed {
            total += delta.num_changes();
        }
        total
    }

    /// Merge a cluster delta into the current level (used after parallel Soroban execution).
    pub fn merge_cluster_delta(&mut self, cluster_delta: LedgerDelta) -> Result<()> {
        self.current.merge(cluster_delta)
    }

    // ------------------------------------------------------------------
    // Escape hatches for the execution layer
    // ------------------------------------------------------------------

    /// Mutable access to the current delta for the execution layer.
    ///
    /// The execution layer (`tx_set.rs`, `mod.rs`) uses `LedgerDelta` directly
    /// for recording per-TX results and fee pool changes. This escape hatch
    /// provides access to the current delta without breaking the `LedgerTxn`
    /// abstraction for the upgrade and close paths.
    pub fn current_delta_mut(&mut self) -> &mut LedgerDelta {
        &mut self.current
    }

    /// Immutable access to the current delta (for logging, counts, etc.).
    pub fn current_delta(&self) -> &LedgerDelta {
        &self.current
    }

    // ------------------------------------------------------------------
    // Internal helpers
    // ------------------------------------------------------------------

    /// Look up a change in the delta chain (current → committed, newest first).
    /// Does NOT fall through to the snapshot.
    fn get_delta_change(&self, key: &LedgerKey) -> Option<&EntryChange> {
        if let Some(change) = self.current.get_change(key) {
            return Some(change);
        }
        for delta in self.committed.iter().rev() {
            if let Some(change) = delta.get_change(key) {
                return Some(change);
            }
        }
        None
    }
}

/// Check if a ledger key is for an offer entry.
fn is_offer_key(key: &LedgerKey) -> bool {
    matches!(key, LedgerKey::Offer(_))
}

/// State captured from prior stages for parallel Soroban execution.
///
/// This replaces the standalone `PriorStageState` struct in execution/mod.rs
/// when reads are done through `LedgerTxn`.
pub struct PriorStageState {
    pub entries: Vec<LedgerEntry>,
    pub deleted_keys: Vec<LedgerKey>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot::{LedgerSnapshot, SnapshotHandle};
    use stellar_xdr::curr::{
        AccountEntry, AccountId, LedgerEntry, LedgerEntryData, LedgerEntryExt, LedgerHeader,
        LedgerKey, LedgerKeyAccount, PublicKey, Thresholds, Uint256,
    };

    fn make_test_account_id(seed: u8) -> AccountId {
        let mut key_bytes = [0u8; 32];
        key_bytes[0] = seed;
        AccountId(PublicKey::PublicKeyTypeEd25519(Uint256(key_bytes)))
    }

    fn make_test_account_entry(seed: u8, balance: i64, seq: u32) -> LedgerEntry {
        let account_id = make_test_account_id(seed);
        LedgerEntry {
            last_modified_ledger_seq: seq,
            data: LedgerEntryData::Account(AccountEntry {
                account_id,
                balance,
                seq_num: stellar_xdr::curr::SequenceNumber(0),
                num_sub_entries: 0,
                inflation_dest: None,
                flags: 0,
                home_domain: stellar_xdr::curr::String32::default(),
                thresholds: Thresholds([1, 0, 0, 0]),
                signers: stellar_xdr::curr::VecM::default(),
                ext: stellar_xdr::curr::AccountEntryExt::V0,
            }),
            ext: LedgerEntryExt::V0,
        }
    }

    fn make_account_key(seed: u8) -> LedgerKey {
        LedgerKey::Account(LedgerKeyAccount {
            account_id: make_test_account_id(seed),
        })
    }

    fn make_empty_snapshot(seq: u32) -> SnapshotHandle {
        let header = LedgerHeader {
            ledger_version: 25,
            ledger_seq: seq,
            ..Default::default()
        };
        let snapshot = LedgerSnapshot::new(
            header,
            henyey_common::Hash256::ZERO,
            std::collections::HashMap::new(),
        );
        SnapshotHandle::new(snapshot)
    }

    fn make_snapshot_with_entry(seq: u32, entry: LedgerEntry) -> SnapshotHandle {
        let header = LedgerHeader {
            ledger_version: 25,
            ledger_seq: seq,
            ..Default::default()
        };
        let key = henyey_common::entry_to_key(&entry);
        let mut entries = std::collections::HashMap::new();
        entries.insert(key, entry);
        let snapshot = LedgerSnapshot::new(header, henyey_common::Hash256::ZERO, entries);
        SnapshotHandle::new(snapshot)
    }

    #[test]
    fn test_basic_read_write() {
        let snapshot = make_empty_snapshot(100);
        let header = LedgerHeader {
            ledger_version: 25,
            ledger_seq: 100,
            ..Default::default()
        };
        let mut ltx = LedgerTxn::begin(snapshot, header, henyey_common::Hash256::ZERO, 101);

        // Initially empty
        let key = make_account_key(1);
        assert!(ltx.get_entry(&key).unwrap().is_none());

        // Create an entry
        let entry = make_test_account_entry(1, 1000, 101);
        ltx.record_create(entry.clone()).unwrap();

        // Now visible
        let loaded = ltx.get_entry(&key).unwrap().unwrap();
        assert_eq!(loaded, entry);
    }

    #[test]
    fn test_read_from_snapshot() {
        let entry = make_test_account_entry(1, 5000, 99);
        let snapshot = make_snapshot_with_entry(100, entry.clone());
        let header = LedgerHeader {
            ledger_version: 25,
            ledger_seq: 100,
            ..Default::default()
        };
        let ltx = LedgerTxn::begin(snapshot, header, henyey_common::Hash256::ZERO, 101);

        let key = make_account_key(1);
        let loaded = ltx.get_entry(&key).unwrap().unwrap();
        assert_eq!(loaded, entry);
    }

    #[test]
    fn test_delta_overrides_snapshot() {
        let old_entry = make_test_account_entry(1, 5000, 99);
        let snapshot = make_snapshot_with_entry(100, old_entry.clone());
        let header = LedgerHeader {
            ledger_version: 25,
            ledger_seq: 100,
            ..Default::default()
        };
        let mut ltx = LedgerTxn::begin(snapshot, header, henyey_common::Hash256::ZERO, 101);

        // Update overrides snapshot
        let new_entry = make_test_account_entry(1, 3000, 101);
        ltx.record_update(old_entry, new_entry.clone()).unwrap();

        let key = make_account_key(1);
        let loaded = ltx.get_entry(&key).unwrap().unwrap();
        assert_eq!(loaded, new_entry);
    }

    #[test]
    fn test_child_sees_parent_changes() {
        let snapshot = make_empty_snapshot(100);
        let header = LedgerHeader {
            ledger_version: 25,
            ledger_seq: 100,
            ..Default::default()
        };
        let mut ltx = LedgerTxn::begin(snapshot, header, henyey_common::Hash256::ZERO, 101);

        // Parent creates an entry
        let entry = make_test_account_entry(1, 1000, 101);
        ltx.record_create(entry.clone()).unwrap();

        // Child can see parent's entry
        let (child, _restore) = ltx.child();
        let key = make_account_key(1);
        let loaded = child.get_entry(&key).unwrap().unwrap();
        assert_eq!(loaded, entry);
    }

    #[test]
    fn test_commit_merges_child_changes() {
        let snapshot = make_empty_snapshot(100);
        let header = LedgerHeader {
            ledger_version: 25,
            ledger_seq: 100,
            ..Default::default()
        };
        let ltx = LedgerTxn::begin(snapshot, header, henyey_common::Hash256::ZERO, 101);

        // Create child and add an entry
        let (mut child, restore) = ltx.child();
        let entry = make_test_account_entry(1, 1000, 101);
        child.record_create(entry.clone()).unwrap();

        // Commit child back to parent
        let ltx = child.commit(restore);

        // Parent now sees the entry
        let key = make_account_key(1);
        let loaded = ltx.get_entry(&key).unwrap().unwrap();
        assert_eq!(loaded, entry);
    }

    #[test]
    fn test_rollback_discards_child_changes() {
        let snapshot = make_empty_snapshot(100);
        let header = LedgerHeader {
            ledger_version: 25,
            ledger_seq: 100,
            ..Default::default()
        };
        let ltx = LedgerTxn::begin(snapshot, header, henyey_common::Hash256::ZERO, 101);

        // Create child and add an entry
        let (mut child, restore) = ltx.child();
        let entry = make_test_account_entry(1, 1000, 101);
        child.record_create(entry).unwrap();

        // Rollback
        let ltx = LedgerTxn::rollback(restore);

        // Parent does NOT see the entry
        let key = make_account_key(1);
        assert!(ltx.get_entry(&key).unwrap().is_none());
    }

    #[test]
    fn test_nested_children() {
        let snapshot = make_empty_snapshot(100);
        let header = LedgerHeader {
            ledger_version: 25,
            ledger_seq: 100,
            ..Default::default()
        };
        let ltx = LedgerTxn::begin(snapshot, header, henyey_common::Hash256::ZERO, 101);

        // Child 1 creates entry A
        let (mut child1, restore1) = ltx.child();
        let entry_a = make_test_account_entry(1, 1000, 101);
        child1.record_create(entry_a.clone()).unwrap();
        let ltx = child1.commit(restore1);

        // Child 2 creates entry B and sees entry A
        let (mut child2, restore2) = ltx.child();
        let key_a = make_account_key(1);
        assert!(child2.get_entry(&key_a).unwrap().is_some());

        let entry_b = make_test_account_entry(2, 2000, 101);
        child2.record_create(entry_b.clone()).unwrap();
        let ltx = child2.commit(restore2);

        // Parent sees both
        let key_b = make_account_key(2);
        assert!(ltx.get_entry(&key_a).unwrap().is_some());
        assert!(ltx.get_entry(&key_b).unwrap().is_some());
    }

    #[test]
    fn test_current_changes_as_entry_changes() {
        let snapshot = make_empty_snapshot(100);
        let header = LedgerHeader {
            ledger_version: 25,
            ledger_seq: 100,
            ..Default::default()
        };
        let mut ltx = LedgerTxn::begin(snapshot, header, henyey_common::Hash256::ZERO, 101);

        let entry = make_test_account_entry(1, 1000, 101);
        ltx.record_create(entry).unwrap();

        let changes = ltx.current_changes_as_entry_changes();
        assert_eq!(changes.0.len(), 1);
        assert!(matches!(&changes.0[0], LedgerEntryChange::Created(_)));
    }

    #[test]
    fn test_fee_pool_and_coins_delta() {
        let snapshot = make_empty_snapshot(100);
        let header = LedgerHeader {
            ledger_version: 25,
            ledger_seq: 100,
            ..Default::default()
        };
        let mut ltx = LedgerTxn::begin(snapshot, header, henyey_common::Hash256::ZERO, 101);

        ltx.record_fee_pool_delta(500);
        ltx.record_total_coins_delta(-100);

        let (_cat, final_state) = ltx.drain_for_bucket_update();
        assert_eq!(final_state.fee_pool_delta, 500);
        assert_eq!(final_state.total_coins_delta, -100);
    }
}
