//! Change tracking for ledger close operations.
//!
//! This module provides [`LedgerDelta`], which accumulates all state changes
//! during the processing of a single ledger. The delta serves as the
//! transaction log for the ledger close, enabling:
//!
//! - **Bucket list updates**: Changes are applied to the Merkle tree
//! - **Transaction metadata**: Change history for each transaction
//! - **Database updates**: Persistent storage synchronization
//! - **Invariant checking**: Validation of state consistency
//!
//! # Change Semantics
//!
//! The delta tracks three types of changes via [`EntryChange`]:
//!
//! - **Created**: New entries that didn't exist before
//! - **Updated**: Existing entries with modified values
//! - **Deleted**: Entries that have been removed
//!
//! # Change Coalescing
//!
//! When multiple operations affect the same entry within a ledger, changes
//! are coalesced to produce the minimal final diff:
//!
//! - Create + Update = Create (with final value)
//! - Create + Delete = No change (entry never existed in previous state)
//! - Update + Update = Update (original previous, final current)
//! - Update + Delete = Delete (original previous)

use crate::{LedgerError, Result};
use std::collections::HashMap;
use stellar_xdr::curr::{
    AccountId, LedgerEntry, LedgerEntryChange, LedgerEntryChanges, LedgerEntryData, LedgerKey,
    LedgerKeyAccount, VecM,
};

/// Represents a single change to a ledger entry.
///
/// Each change captures enough information to:
/// - Apply the change to the bucket list (forward)
/// - Reconstruct the previous state (backward)
/// - Generate transaction metadata
///
/// # Bucket List Categories
///
/// Changes map to bucket list update categories:
/// - `Created` entries go to the "init" batch
/// - `Updated` entries go to the "live" batch
/// - `Deleted` entries go to the "dead" batch
#[derive(Debug, Clone)]
/// Result of categorizing delta entries for bucket list update in a single pass.
pub struct DeltaCategorization {
    pub init_entries: Vec<LedgerEntry>,
    pub live_entries: Vec<LedgerEntry>,
    pub dead_keys: Vec<LedgerKey>,
    pub created_count: usize,
    pub updated_count: usize,
    pub deleted_count: usize,
    pub has_offers: bool,
    pub has_pool_share_trustlines: bool,
    /// Offer and pool share trustline changes, extracted for commit_close processing.
    /// Only populated when has_offers or has_pool_share_trustlines is true.
    pub offer_pool_changes: Vec<EntryChange>,
}

#[derive(Debug, Clone)]
pub enum EntryChange {
    /// A new entry was created (did not exist in previous ledger state).
    Created(LedgerEntry),
    /// An existing entry was modified.
    Updated {
        /// The entry value before the update (for rollback/metadata).
        previous: LedgerEntry,
        /// The entry value after the update.
        current: Box<LedgerEntry>,
    },
    /// An entry was deleted (existed in previous state, now gone).
    Deleted {
        /// The entry that was deleted (for rollback/metadata).
        previous: LedgerEntry,
    },
}

impl EntryChange {
    /// Get the ledger key for this change.
    pub fn key(&self) -> LedgerKey {
        match self {
            EntryChange::Created(entry) => henyey_common::entry_to_key(entry),
            EntryChange::Updated { current, .. } => henyey_common::entry_to_key(current),
            EntryChange::Deleted { previous } => henyey_common::entry_to_key(previous),
        }
    }

    /// Get the current entry value, if any.
    pub fn current_entry(&self) -> Option<&LedgerEntry> {
        match self {
            EntryChange::Created(entry) => Some(entry),
            EntryChange::Updated { current, .. } => Some(current.as_ref()),
            EntryChange::Deleted { .. } => None,
        }
    }

    /// Get the previous entry value, if any.
    pub fn previous_entry(&self) -> Option<&LedgerEntry> {
        match self {
            EntryChange::Created(_) => None,
            EntryChange::Updated { previous, .. } => Some(previous),
            EntryChange::Deleted { previous } => Some(previous),
        }
    }

    /// Check if this is a creation.
    pub fn is_created(&self) -> bool {
        matches!(self, EntryChange::Created(_))
    }

    /// Check if this is an update.
    pub fn is_updated(&self) -> bool {
        matches!(self, EntryChange::Updated { .. })
    }

    /// Check if this is a deletion.
    pub fn is_deleted(&self) -> bool {
        matches!(self, EntryChange::Deleted { .. })
    }
}


/// Accumulator for all ledger entry changes during a single ledger close.
///
/// `LedgerDelta` provides a transactional view of state changes, allowing
/// multiple operations to modify entries with automatic change coalescing.
/// The final delta represents the minimal diff between the previous and
/// new ledger state.
///
/// # Usage
///
/// ```ignore
/// let mut delta = LedgerDelta::new(ledger_seq);
///
/// // Record changes during transaction processing
/// delta.record_create(new_account)?;
/// delta.record_update(old_trustline, new_trustline)?;
/// delta.record_delete(expired_offer)?;
///
/// // Get categorized changes for bucket list update
/// let init_entries = delta.init_entries();   // Created
/// let live_entries = delta.live_entries();   // Updated
/// let dead_entries = delta.dead_entries();   // Deleted
/// ```
///
/// # Deterministic Ordering
///
/// Changes are tracked in insertion order to ensure deterministic iteration.
/// This is critical for producing consistent bucket list updates across nodes.
#[derive(Debug)]
pub struct LedgerDelta {
    /// The ledger sequence this delta applies to.
    ledger_seq: u32,

    /// All entry changes, keyed by LedgerKey directly.
    ///
    /// Using LedgerKey as the HashMap key eliminates XDR serialization overhead
    /// on every lookup (~1µs per call), which saves ~50ms per 50K-TX ledger in
    /// the fee pre-deduction path alone.
    changes: HashMap<LedgerKey, EntryChange>,

    /// Keys in the order changes were first recorded (for deterministic iteration).
    change_order: Vec<LedgerKey>,

    /// Net change to the fee pool (positive = fees collected).
    fee_pool_delta: i64,

    /// Net change to total coins in circulation.
    ///
    /// Typically zero, but can change due to inflation or fee burns.
    total_coins_delta: i64,
}

impl LedgerDelta {
    /// Create a new empty LedgerDelta.
    pub fn new(ledger_seq: u32) -> Self {
        Self {
            ledger_seq,
            changes: HashMap::new(),
            change_order: Vec::new(),
            fee_pool_delta: 0,
            total_coins_delta: 0,
        }
    }

    /// Get the ledger sequence this delta is for.
    pub fn ledger_seq(&self) -> u32 {
        self.ledger_seq
    }

    /// Record the creation of a new entry.
    ///
    /// If the entry already exists in the delta:
    /// - If it was created, update with the new value
    /// - If it was updated, keep original previous and update current
    /// - If it was deleted, return error (can't create a deleted entry)
    pub fn record_create(&mut self, entry: LedgerEntry) -> Result<()> {
        let key = henyey_common::entry_to_key(&entry);

        if let Some(existing) = self.changes.get(&key) {
            match existing {
                EntryChange::Created(_) => {
                    // Entry was already created, update with new value
                    self.changes.insert(key, EntryChange::Created(entry));
                }
                EntryChange::Updated { previous, .. } => {
                    // Entry was updated, keep original previous and update current
                    self.changes.insert(
                        key,
                        EntryChange::Updated {
                            previous: previous.clone(),
                            current: Box::new(entry),
                        },
                    );
                }
                EntryChange::Deleted { previous } => {
                    // Deleted then created = update (entry existed before the ledger,
                    // was deleted, then recreated - net effect is an update).
                    self.changes.insert(
                        key,
                        EntryChange::Updated {
                            previous: previous.clone(),
                            current: Box::new(entry),
                        },
                    );
                }
            }
        } else {
            self.change_order.push(key.clone());
            self.changes.insert(key, EntryChange::Created(entry));
        }
        Ok(())
    }

    /// Record an update to an existing entry.
    pub fn record_update(&mut self, previous: LedgerEntry, current: LedgerEntry) -> Result<()> {
        let key = henyey_common::entry_to_key(&current);

        // Check if we already have a change for this entry
        if let Some(existing) = self.changes.get(&key) {
            match existing {
                EntryChange::Created(_) => {
                    // If we created and then updated, just record as created with new value
                    self.changes
                        .insert(key, EntryChange::Created(current));
                }
                EntryChange::Updated { previous: orig, .. } => {
                    // Update the current value, keep original previous
                    self.changes.insert(
                        key,
                        EntryChange::Updated {
                            previous: orig.clone(),
                            current: Box::new(current),
                        },
                    );
                }
                EntryChange::Deleted { previous: orig } => {
                    // Deleted then updated = entry was deleted then came back
                    // (e.g., via fee refund restore). Keep original previous.
                    self.changes.insert(
                        key,
                        EntryChange::Updated {
                            previous: orig.clone(),
                            current: Box::new(current),
                        },
                    );
                }
            }
        } else {
            self.change_order.push(key.clone());
            self.changes.insert(
                key,
                EntryChange::Updated {
                    previous,
                    current: Box::new(current),
                },
            );
        }

        Ok(())
    }

    /// Record the deletion of an entry.
    ///
    /// If the entry already exists in the delta:
    /// - If it was created, remove from delta entirely (create + delete = no-op)
    /// - If it was updated, record as deleted with original previous
    /// - If it was already deleted, skip (idempotent delete)
    ///
    /// # Errors
    ///
    /// Returns an error if the entry is a `ConfigSetting`. Config settings are
    /// network-wide parameters that cannot be deleted, only updated via upgrades.
    /// Parity: LedgerTxnTests.cpp:853 "fails for configuration"
    pub fn record_delete(&mut self, entry: LedgerEntry) -> Result<()> {
        // ConfigSetting entries cannot be erased (parity: stellar-core LedgerTxn::erase)
        if matches!(
            entry.data,
            stellar_xdr::curr::LedgerEntryData::ConfigSetting(_)
        ) {
            return Err(LedgerError::InvalidEntry(
                "cannot delete ConfigSetting entries".to_string(),
            ));
        }

        let key = henyey_common::entry_to_key(&entry);

        // Check if we already have a change for this entry
        if let Some(existing) = self.changes.get(&key) {
            match existing {
                EntryChange::Created(_) => {
                    // If we created and then deleted, remove from delta entirely
                    self.changes.remove(&key);
                    self.change_order.retain(|k| k != &key);
                }
                EntryChange::Updated { previous, .. } => {
                    // If we updated and then deleted, record as deleted with original previous
                    self.changes.insert(
                        key,
                        EntryChange::Deleted {
                            previous: previous.clone(),
                        },
                    );
                }
                EntryChange::Deleted { .. } => {
                    // Entry already deleted, this is a no-op (idempotent delete)
                    // This can happen during replay when entries are processed multiple times
                }
            }
        } else {
            self.change_order.push(key.clone());
            self.changes
                .insert(key, EntryChange::Deleted { previous: entry });
        }

        Ok(())
    }

    /// Record a fee pool change.
    pub fn record_fee_pool_delta(&mut self, delta: i64) {
        self.fee_pool_delta += delta;
    }

    /// Record a total coins change (e.g., from inflation).
    pub fn record_total_coins_delta(&mut self, delta: i64) {
        self.total_coins_delta += delta;
    }

    /// Pre-deduct a fee from an account entry in the delta, or create/update the
    /// entry from the snapshot if not yet present.
    ///
    /// Returns `(charged_fee, fee_changes)` where `charged_fee = min(balance, fee)`.
    /// `fee_changes` contains the `[State(before), Updated(after)]` LedgerEntryChanges
    /// needed for the transaction result metadata.
    ///
    /// This is used by parallel Soroban execution to pre-deduct all fees before
    /// cluster execution, matching stellar-core's `processFeesSeqNums` behavior.
    pub fn deduct_fee_from_account(
        &mut self,
        account_id: &AccountId,
        fee: i64,
        snapshot: &crate::snapshot::SnapshotHandle,
        ledger_seq: u32,
    ) -> Result<(i64, LedgerEntryChanges)> {
        let key = LedgerKey::Account(LedgerKeyAccount {
            account_id: account_id.clone(),
        });

        // Get the current entry from the delta, or load from snapshot.
        let (mut entry, is_new) = if let Some(change) = self.changes.get(&key) {
            if let Some(current) = change.current_entry() {
                (current.clone(), false)
            } else {
                // Entry was deleted — shouldn't happen for fee accounts, but handle gracefully.
                return Ok((0, LedgerEntryChanges(VecM::default())));
            }
        } else if let Some(entry) = snapshot
            .get_entry(&key)
            .map_err(|e| LedgerError::Internal(format!("snapshot lookup failed: {}", e)))?
        {
            (entry, true)
        } else {
            // Account not found at all — no fee to deduct.
            return Ok((0, LedgerEntryChanges(VecM::default())));
        };

        // Build STATE entry (before fee deduction).
        let state_entry = entry.clone();

        // Deduct fee from balance.
        let balance = if let LedgerEntryData::Account(ref acc) = entry.data {
            acc.balance
        } else {
            return Ok((0, LedgerEntryChanges(VecM::default())));
        };
        let charged_fee = std::cmp::min(balance, fee);
        if let LedgerEntryData::Account(ref mut acc) = entry.data {
            acc.balance -= charged_fee;
        }
        // Stamp last_modified_ledger_seq to match stellar-core LedgerTxn behavior.
        if charged_fee > 0 {
            entry.last_modified_ledger_seq = ledger_seq;
        }

        // Build fee_changes: [State(before), Updated(after)].
        let fee_changes = if charged_fee > 0 {
            let changes_vec = vec![
                LedgerEntryChange::State(state_entry.clone()),
                LedgerEntryChange::Updated(entry.clone()),
            ];
            LedgerEntryChanges(changes_vec.try_into().map_err(|_| {
                LedgerError::Internal("fee changes vec conversion failed".to_string())
            })?)
        } else {
            LedgerEntryChanges(VecM::default())
        };

        // Update the delta with the post-fee-deduction entry.
        if is_new {
            // First time this account appears in the delta — record as Update.
            self.change_order.push(key.clone());
            self.changes.insert(
                key,
                EntryChange::Updated {
                    previous: state_entry,
                    current: Box::new(entry),
                },
            );
        } else {
            // Already in delta — update the current value in place.
            if let Some(change) = self.changes.get_mut(&key) {
                match change {
                    EntryChange::Created(ref mut e) => {
                        if let LedgerEntryData::Account(ref mut acc) = e.data {
                            acc.balance -= charged_fee;
                            // Re-add charged_fee to state_entry balance since Created
                            // doesn't have a previous.
                        }
                        // Actually, we need to just replace the entry data entirely
                        *e = entry;
                    }
                    EntryChange::Updated {
                        ref mut current, ..
                    } => {
                        **current = entry;
                    }
                    EntryChange::Deleted { .. } => {
                        // Shouldn't happen — already handled above.
                    }
                }
            }
        }

        Ok((charged_fee, fee_changes))
    }

    /// Get the current entry value for a given key from the delta's changes, if it exists.
    ///
    /// Returns `Some(entry)` if the key has a Created or Updated change.
    /// Returns `None` if the key is Deleted or not present in the delta.
    pub fn get_current_entry(&self, key: &LedgerKey) -> Option<LedgerEntry> {
        if let Some(change) = self.changes.get(key) {
            change.current_entry().cloned()
        } else {
            None
        }
    }

    /// Apply a fee refund to an account entry already in the delta.
    ///
    /// This is used by parallel Soroban execution where cluster deltas have been merged
    /// into the main delta, and fee refunds need to be applied post-execution.
    /// Modifies the account's balance in-place within the existing change entry.
    pub fn apply_refund_to_account(&mut self, account_id: &AccountId, refund: i64) -> Result<()> {
        let key = LedgerKey::Account(LedgerKeyAccount {
            account_id: account_id.clone(),
        });

        if let Some(change) = self.changes.get_mut(&key) {
            match change {
                EntryChange::Created(ref mut entry) => {
                    if let LedgerEntryData::Account(ref mut acc) = entry.data {
                        acc.balance += refund;
                    }
                }
                EntryChange::Updated {
                    ref mut current, ..
                } => {
                    if let LedgerEntryData::Account(ref mut acc) = current.data {
                        acc.balance += refund;
                    }
                }
                EntryChange::Deleted { .. } => {}
            }
        }
        Ok(())
    }

    /// Get the fee pool delta.
    pub fn fee_pool_delta(&self) -> i64 {
        self.fee_pool_delta
    }

    /// Get the total coins delta.
    pub fn total_coins_delta(&self) -> i64 {
        self.total_coins_delta
    }

    /// Get all entry changes in the order they were recorded.
    pub fn changes(&self) -> impl Iterator<Item = &EntryChange> {
        self.change_order.iter().filter_map(|k| self.changes.get(k))
    }

    /// Get the number of changes.
    pub fn num_changes(&self) -> usize {
        self.changes.len()
    }

    /// Check if there are any changes.
    pub fn is_empty(&self) -> bool {
        self.changes.is_empty()
    }

    /// Get all init entries (created) for bucket list update.
    pub fn init_entries(&self) -> Vec<LedgerEntry> {
        self.changes()
            .filter(|change| change.is_created())
            .filter_map(|change| change.current_entry().cloned())
            .collect()
    }

    /// Get all live entries (updated) for bucket list update.
    pub fn live_entries(&self) -> Vec<LedgerEntry> {
        self.changes()
            .filter(|change| change.is_updated())
            .filter_map(|change| change.current_entry().cloned())
            .collect()
    }

    /// Get all current entry values (created + updated).
    ///
    /// Used to propagate prior-stage entries to subsequent stages in parallel
    /// Soroban execution, matching stellar-core `collectClusterFootprintEntriesFromGlobal`.
    pub fn current_entries(&self) -> Vec<LedgerEntry> {
        self.changes()
            .filter_map(|change| change.current_entry().cloned())
            .collect()
    }

    /// Get all dead entries (deleted keys) for bucket list update.
    pub fn dead_entries(&self) -> Vec<LedgerKey> {
        self.changes()
            .filter(|change| change.is_deleted())
            .map(|change| change.key())
            .collect()
    }

    /// Result of categorizing delta entries for bucket list update.
    pub fn categorize_for_bucket_update(&self) -> DeltaCategorization {
        let mut init = Vec::new();
        let mut live = Vec::new();
        let mut dead = Vec::new();
        let mut created = 0usize;
        let mut updated = 0usize;
        let mut deleted = 0usize;
        let mut has_offers = false;
        let mut has_pool_share_trustlines = false;
        for change in self.changes() {
            // Check entry data type for fast-path in commit_close
            let entry_ref = match change {
                EntryChange::Created(e) | EntryChange::Deleted { previous: e } => e,
                EntryChange::Updated { current, .. } => current,
            };
            match &entry_ref.data {
                stellar_xdr::curr::LedgerEntryData::Offer(_) => has_offers = true,
                stellar_xdr::curr::LedgerEntryData::Trustline(tl) if matches!(tl.asset, stellar_xdr::curr::TrustLineAsset::PoolShare(_)) => {
                    has_pool_share_trustlines = true;
                }
                _ => {}
            }
            match change {
                EntryChange::Created(entry) => {
                    created += 1;
                    init.push(entry.clone());
                }
                EntryChange::Updated { current, .. } => {
                    updated += 1;
                    live.push((**current).clone());
                }
                EntryChange::Deleted { previous } => {
                    deleted += 1;
                    dead.push(henyey_common::entry_to_key(previous));
                }
            }
        }
        DeltaCategorization {
            init_entries: init,
            live_entries: live,
            dead_keys: dead,
            created_count: created,
            updated_count: updated,
            deleted_count: deleted,
            has_offers,
            has_pool_share_trustlines,
            offer_pool_changes: Vec::new(),
        }
    }

    /// Drains entries from the delta, categorizing them for bucket list update.
    /// Moves entries instead of cloning, saving ~50K clone operations.
    /// Metadata (fee_pool_delta, total_coins_delta) is preserved.
    /// Offer and pool share trustline changes are collected separately for commit_close.
    pub fn drain_categorization_for_bucket_update(&mut self) -> DeltaCategorization {
        let mut init = Vec::new();
        let mut live = Vec::new();
        let mut dead = Vec::new();
        let mut created = 0usize;
        let mut updated = 0usize;
        let mut deleted = 0usize;
        let mut has_offers = false;
        let mut has_pool_share_trustlines = false;
        let mut offer_pool_changes = Vec::new();
        for (_key, change) in self.changes.drain() {
            let entry_ref = match &change {
                EntryChange::Created(e) | EntryChange::Deleted { previous: e } => e,
                EntryChange::Updated { current, .. } => current,
            };
            let is_offer_or_pool = match &entry_ref.data {
                stellar_xdr::curr::LedgerEntryData::Offer(_) => {
                    has_offers = true;
                    true
                }
                stellar_xdr::curr::LedgerEntryData::Trustline(tl) if matches!(tl.asset, stellar_xdr::curr::TrustLineAsset::PoolShare(_)) => {
                    has_pool_share_trustlines = true;
                    true
                }
                _ => false,
            };
            if is_offer_or_pool {
                offer_pool_changes.push(change.clone());
            }
            match change {
                EntryChange::Created(entry) => {
                    created += 1;
                    init.push(entry);
                }
                EntryChange::Updated { current, .. } => {
                    updated += 1;
                    live.push(*current);
                }
                EntryChange::Deleted { previous } => {
                    deleted += 1;
                    dead.push(henyey_common::entry_to_key(&previous));
                }
            }
        }
        DeltaCategorization {
            init_entries: init,
            live_entries: live,
            dead_keys: dead,
            created_count: created,
            updated_count: updated,
            deleted_count: deleted,
            has_offers,
            has_pool_share_trustlines,
            offer_pool_changes,
        }
    }

    /// Get a specific change by key.
    pub fn get_change(&self, key: &LedgerKey) -> Option<&EntryChange> {
        self.changes.get(key)
    }

    /// Merge another delta into this one.
    ///
    /// This is useful when combining changes from multiple operations.
    pub fn merge(&mut self, other: LedgerDelta) -> Result<()> {
        // Consume `other` by value, iterating in insertion order to preserve
        // deterministic ordering. Using LedgerKey directly avoids XDR
        // serialization during the merge (~1µs per key eliminated).
        for key in other.change_order {
            if let Some(change) = other.changes.get(&key) {
                match change {
                    EntryChange::Created(entry) => {
                        if let Some(existing) = self.changes.get(&key) {
                            match existing {
                                EntryChange::Deleted { previous } => {
                                    // Deleted then created = update
                                    self.changes.insert(
                                        key,
                                        EntryChange::Updated {
                                            previous: previous.clone(),
                                            current: Box::new(entry.clone()),
                                        },
                                    );
                                }
                                EntryChange::Created(_) => {
                                    // Created + Created: a later stage re-creates the
                                    // same entry that an earlier stage already restored
                                    // from the hot archive.  Keep the later value.
                                    self.changes
                                        .insert(key, EntryChange::Created(entry.clone()));
                                }
                                EntryChange::Updated { .. } => {
                                    return Err(LedgerError::Internal(
                                        "invalid merge: create on updated entry".to_string(),
                                    ));
                                }
                            }
                        } else {
                            self.change_order.push(key.clone());
                            self.changes
                                .insert(key, EntryChange::Created(entry.clone()));
                        }
                    }
                    EntryChange::Updated { previous, current } => {
                        if let Some(existing) = self.changes.get(&key) {
                            match existing {
                                EntryChange::Created(_) => {
                                    self.changes.insert(
                                        key,
                                        EntryChange::Created(current.as_ref().clone()),
                                    );
                                }
                                EntryChange::Updated { previous: orig, .. } => {
                                    self.changes.insert(
                                        key,
                                        EntryChange::Updated {
                                            previous: orig.clone(),
                                            current: current.clone(),
                                        },
                                    );
                                }
                                EntryChange::Deleted { .. } => {
                                    return Err(LedgerError::Internal(
                                        "invalid merge: update on deleted entry".to_string(),
                                    ));
                                }
                            }
                        } else {
                            // Entry not in target delta — insert the full update.
                            // This occurs when merging independent deltas (e.g. parallel
                            // cluster execution) where each delta carries its own
                            // previous/current state.
                            self.change_order.push(key.clone());
                            self.changes.insert(
                                key,
                                EntryChange::Updated {
                                    previous: previous.clone(),
                                    current: current.clone(),
                                },
                            );
                        }
                    }
                    EntryChange::Deleted { previous } => {
                        if let Some(existing) = self.changes.get(&key) {
                            match existing {
                                EntryChange::Created(_) => {
                                    // Created then deleted = no change
                                    self.changes.remove(&key);
                                    self.change_order.retain(|k| k != &key);
                                }
                                EntryChange::Updated { previous: orig, .. } => {
                                    self.changes.insert(
                                        key,
                                        EntryChange::Deleted {
                                            previous: orig.clone(),
                                        },
                                    );
                                }
                                EntryChange::Deleted {
                                    previous: existing_prev,
                                } => {
                                    // Idempotent: both deltas deleted the same entry.
                                    // This is valid when parallel clusters independently
                                    // delete an entry that appears in multiple footprints
                                    // (e.g. TTL keys during Soroban execution).  Keep
                                    // the existing deletion.
                                    //
                                    // Both previous values should be identical since both
                                    // clusters loaded from the same snapshot.
                                    debug_assert!(
                                        existing_prev == previous,
                                        "merge: Deleted+Deleted previous values differ for same key"
                                    );
                                }
                            }
                        } else {
                            self.change_order.push(key.clone());
                            self.changes.insert(
                                key,
                                EntryChange::Deleted {
                                    previous: previous.clone(),
                                },
                            );
                        }
                    }
                }
            }
        }

        self.fee_pool_delta += other.fee_pool_delta;
        self.total_coins_delta += other.total_coins_delta;

        Ok(())
    }

    /// Clear all changes.
    pub fn clear(&mut self) {
        self.changes.clear();
        self.change_order.clear();
        self.fee_pool_delta = 0;
        self.total_coins_delta = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use stellar_xdr::curr::{
        AccountEntry, AccountEntryExt, AccountId, LedgerEntryData, LedgerEntryExt, PublicKey,
        SequenceNumber, Thresholds, Uint256,
    };

    fn create_test_account(seed: u8) -> LedgerEntry {
        let mut key = [0u8; 32];
        key[0] = seed;

        LedgerEntry {
            last_modified_ledger_seq: 1,
            data: LedgerEntryData::Account(AccountEntry {
                account_id: AccountId(PublicKey::PublicKeyTypeEd25519(Uint256(key))),
                balance: 1000000000,
                seq_num: SequenceNumber(1),
                num_sub_entries: 0,
                inflation_dest: None,
                flags: 0,
                home_domain: stellar_xdr::curr::String32::default(),
                thresholds: Thresholds([1, 0, 0, 0]),
                signers: stellar_xdr::curr::VecM::default(),
                ext: AccountEntryExt::V0,
            }),
            ext: LedgerEntryExt::V0,
        }
    }

    #[test]
    fn test_record_create() {
        let mut delta = LedgerDelta::new(1);
        let entry = create_test_account(1);

        delta.record_create(entry.clone()).unwrap();
        assert_eq!(delta.num_changes(), 1);

        let init = delta.init_entries();
        assert_eq!(init.len(), 1);
    }

    #[test]
    fn test_record_update() {
        let mut delta = LedgerDelta::new(1);
        let entry1 = create_test_account(1);
        let mut entry2 = entry1.clone();
        if let LedgerEntryData::Account(ref mut acc) = entry2.data {
            acc.balance = 2000000000;
        }

        delta.record_update(entry1, entry2).unwrap();
        assert_eq!(delta.num_changes(), 1);

        let changes: Vec<_> = delta.changes().collect();
        assert!(changes[0].is_updated());
    }

    #[test]
    fn test_record_delete() {
        let mut delta = LedgerDelta::new(1);
        let entry = create_test_account(1);

        delta.record_delete(entry).unwrap();
        assert_eq!(delta.num_changes(), 1);

        let dead = delta.dead_entries();
        assert_eq!(dead.len(), 1);
    }

    #[test]
    fn test_create_then_delete() {
        let mut delta = LedgerDelta::new(1);
        let entry = create_test_account(1);

        delta.record_create(entry.clone()).unwrap();
        delta.record_delete(entry).unwrap();

        // Should cancel out
        assert!(delta.is_empty());
    }

    #[test]
    fn test_create_then_update() {
        let mut delta = LedgerDelta::new(1);
        let entry1 = create_test_account(1);
        let mut entry2 = entry1.clone();
        if let LedgerEntryData::Account(ref mut acc) = entry2.data {
            acc.balance = 2000000000;
        }

        delta.record_create(entry1.clone()).unwrap();
        delta.record_update(entry1, entry2.clone()).unwrap();

        // Should be recorded as a create with the final value
        assert_eq!(delta.num_changes(), 1);
        let changes: Vec<_> = delta.changes().collect();
        assert!(changes[0].is_created());
    }

    #[test]
    fn test_delete_then_create() {
        // Scenario: TX1 deletes an entry that existed before the ledger,
        // TX2 recreates it. Net effect should be Updated (existed before,
        // still exists after with new value).
        let mut delta = LedgerDelta::new(1);
        let original = create_test_account(1);
        let mut recreated = original.clone();
        if let LedgerEntryData::Account(ref mut acc) = recreated.data {
            acc.balance = 5000000000;
        }

        delta.record_delete(original.clone()).unwrap();
        delta.record_create(recreated.clone()).unwrap();

        assert_eq!(delta.num_changes(), 1);
        let changes: Vec<_> = delta.changes().collect();
        assert!(changes[0].is_updated());

        // Current value should be the recreated entry
        let current = changes[0].current_entry().unwrap();
        if let LedgerEntryData::Account(ref acc) = current.data {
            assert_eq!(acc.balance, 5000000000);
        } else {
            panic!("expected account entry");
        }

        // Previous value should be the original
        let previous = changes[0].previous_entry().unwrap();
        if let LedgerEntryData::Account(ref acc) = previous.data {
            assert_eq!(acc.balance, 1000000000);
        } else {
            panic!("expected account entry");
        }

        // Should appear in live_entries (not init or dead)
        assert_eq!(delta.live_entries().len(), 1);
        assert_eq!(delta.init_entries().len(), 0);
        assert_eq!(delta.dead_entries().len(), 0);
    }

    // =========================================================================
    // P2-1: Delta round-trip stress test
    // Parity: LedgerTxnTests.cpp:464 "LedgerTxn round trip"
    // =========================================================================

    fn create_test_account_with_balance(seed: u8, balance: i64) -> LedgerEntry {
        let mut entry = create_test_account(seed);
        if let LedgerEntryData::Account(ref mut acc) = entry.data {
            acc.balance = balance;
        }
        entry
    }

    /// Stress test: multiple rounds of create/modify/erase operations.
    #[test]
    fn test_delta_round_trip_stress() {
        let mut delta = LedgerDelta::new(1);

        // Round 1: Create 10 entries
        for i in 0..10u8 {
            let entry = create_test_account(i);
            delta.record_create(entry).unwrap();
        }
        assert_eq!(delta.num_changes(), 10);
        assert_eq!(delta.init_entries().len(), 10);

        // Round 2: Update all 10 entries
        for i in 0..10u8 {
            let old = create_test_account(i);
            let mut new = old.clone();
            if let LedgerEntryData::Account(ref mut acc) = new.data {
                acc.balance = 2_000_000_000;
            }
            delta.record_update(old, new).unwrap();
        }
        // Created then updated = Created with final value
        assert_eq!(delta.num_changes(), 10);
        assert_eq!(delta.init_entries().len(), 10);
        assert_eq!(delta.live_entries().len(), 0);

        // Verify all entries have updated balance
        for entry in delta.init_entries() {
            if let LedgerEntryData::Account(ref acc) = entry.data {
                assert_eq!(acc.balance, 2_000_000_000);
            }
        }

        // Round 3: Delete half the entries
        for i in 0..5u8 {
            let entry = create_test_account(i);
            delta.record_delete(entry).unwrap();
        }
        // Created then deleted = removed from delta
        assert_eq!(delta.num_changes(), 5);
        assert_eq!(delta.init_entries().len(), 5);
        assert_eq!(delta.dead_entries().len(), 0);

        // Round 4: Recreate the deleted entries with new values
        for i in 0..5u8 {
            let mut entry = create_test_account(i);
            if let LedgerEntryData::Account(ref mut acc) = entry.data {
                acc.balance = 3_000_000_000;
            }
            delta.record_create(entry).unwrap();
        }
        // These are fresh creates since the entries were removed from delta
        assert_eq!(delta.num_changes(), 10);
        assert_eq!(delta.init_entries().len(), 10);
    }

    /// Stress test: interleaved operations on the same entries.
    #[test]
    fn test_delta_interleaved_operations() {
        let mut delta = LedgerDelta::new(1);

        // Create entry
        let e1 = create_test_account(1);
        delta.record_create(e1.clone()).unwrap();

        // Update it
        let e1_v2 = create_test_account_with_balance(1, 2_000_000_000);
        delta.record_update(e1.clone(), e1_v2.clone()).unwrap();

        // Update it again
        let e1_v3 = create_test_account_with_balance(1, 3_000_000_000);
        delta.record_update(e1_v2.clone(), e1_v3.clone()).unwrap();

        // Net: created with value v3
        assert_eq!(delta.num_changes(), 1);
        let changes: Vec<_> = delta.changes().collect();
        assert!(changes[0].is_created());
        if let LedgerEntryData::Account(ref acc) = changes[0].current_entry().unwrap().data {
            assert_eq!(acc.balance, 3_000_000_000);
        }

        // Delete it
        delta.record_delete(e1_v3.clone()).unwrap();
        // Created then deleted = empty delta
        assert!(delta.is_empty());
    }

    // =========================================================================
    // P2-2: Create entry when key exists
    // Parity: LedgerTxnTests.cpp:692 "LedgerTxn create"
    // =========================================================================

    /// Creating when key already exists as Created should overwrite.
    #[test]
    fn test_create_on_existing_created_overwrites() {
        let mut delta = LedgerDelta::new(1);
        let entry1 = create_test_account(1);
        delta.record_create(entry1).unwrap();

        // Create same key again with different value
        let entry2 = create_test_account_with_balance(1, 9_999);
        delta.record_create(entry2).unwrap();

        assert_eq!(delta.num_changes(), 1);
        let changes: Vec<_> = delta.changes().collect();
        assert!(changes[0].is_created());
        if let LedgerEntryData::Account(ref acc) = changes[0].current_entry().unwrap().data {
            assert_eq!(acc.balance, 9_999);
        }
    }

    /// Creating when key exists as Updated should update current value.
    #[test]
    fn test_create_on_existing_updated_keeps_original_previous() {
        let mut delta = LedgerDelta::new(1);
        let original = create_test_account(1);
        let updated = create_test_account_with_balance(1, 2_000);
        delta.record_update(original.clone(), updated).unwrap();

        // Now "create" on same key
        let recreated = create_test_account_with_balance(1, 3_000);
        delta.record_create(recreated).unwrap();

        // Should still be Updated with original previous
        assert_eq!(delta.num_changes(), 1);
        let changes: Vec<_> = delta.changes().collect();
        assert!(changes[0].is_updated());
        if let LedgerEntryData::Account(ref acc) = changes[0].previous_entry().unwrap().data {
            assert_eq!(acc.balance, 1_000_000_000); // original
        }
        if let LedgerEntryData::Account(ref acc) = changes[0].current_entry().unwrap().data {
            assert_eq!(acc.balance, 3_000);
        }
    }

    // =========================================================================
    // Delta merge tests
    // Parity: LedgerTxnTests.cpp commit/merge semantics
    // =========================================================================

    /// Merge two independent deltas.
    #[test]
    fn test_merge_independent_deltas() {
        let mut delta1 = LedgerDelta::new(1);
        let mut delta2 = LedgerDelta::new(1);

        delta1.record_create(create_test_account(1)).unwrap();
        delta2.record_create(create_test_account(2)).unwrap();

        delta1.merge(delta2).unwrap();
        assert_eq!(delta1.num_changes(), 2);
        assert_eq!(delta1.init_entries().len(), 2);
    }

    /// Merge delta with Deleted + Created = Updated.
    #[test]
    fn test_merge_deleted_then_created_becomes_updated() {
        let mut delta1 = LedgerDelta::new(1);
        let mut delta2 = LedgerDelta::new(1);

        let original = create_test_account(1);
        delta1.record_delete(original.clone()).unwrap();

        let recreated = create_test_account_with_balance(1, 5_000);
        delta2.record_create(recreated.clone()).unwrap();

        delta1.merge(delta2).unwrap();
        assert_eq!(delta1.num_changes(), 1);
        let changes: Vec<_> = delta1.changes().collect();
        assert!(changes[0].is_updated());
    }

    /// Merge delta with Created + Deleted = no-op.
    #[test]
    fn test_merge_created_then_deleted_becomes_noop() {
        let mut delta1 = LedgerDelta::new(1);
        let mut delta2 = LedgerDelta::new(1);

        let entry = create_test_account(1);
        delta1.record_create(entry.clone()).unwrap();
        delta2.record_delete(entry).unwrap();

        delta1.merge(delta2).unwrap();
        assert!(delta1.is_empty());
    }

    /// Merge delta: Updated + Updated = Updated (original previous, final current).
    #[test]
    fn test_merge_updated_then_updated_keeps_original_previous() {
        let mut delta1 = LedgerDelta::new(1);
        let mut delta2 = LedgerDelta::new(1);

        let v0 = create_test_account(1);
        let v1 = create_test_account_with_balance(1, 2_000);
        let v2 = create_test_account_with_balance(1, 3_000);

        delta1.record_update(v0.clone(), v1.clone()).unwrap();
        delta2.record_update(v1.clone(), v2.clone()).unwrap();

        delta1.merge(delta2).unwrap();
        assert_eq!(delta1.num_changes(), 1);
        let changes: Vec<_> = delta1.changes().collect();
        assert!(changes[0].is_updated());
        // Previous should be v0 (original)
        if let LedgerEntryData::Account(ref acc) = changes[0].previous_entry().unwrap().data {
            assert_eq!(acc.balance, 1_000_000_000); // v0
        }
        // Current should be v2 (final)
        if let LedgerEntryData::Account(ref acc) = changes[0].current_entry().unwrap().data {
            assert_eq!(acc.balance, 3_000); // v2
        }
    }

    /// Merge fee pool and total coins deltas.
    #[test]
    fn test_merge_fee_pool_and_total_coins() {
        let mut delta1 = LedgerDelta::new(1);
        let mut delta2 = LedgerDelta::new(1);

        delta1.record_fee_pool_delta(100);
        delta1.record_total_coins_delta(50);

        delta2.record_fee_pool_delta(200);
        delta2.record_total_coins_delta(-30);

        delta1.merge(delta2).unwrap();
        assert_eq!(delta1.fee_pool_delta(), 300);
        assert_eq!(delta1.total_coins_delta(), 20);
    }

    /// Merge Created + Created keeps the later value.
    ///
    /// This occurs when multiple Soroban stages restore the same entry from
    /// the hot archive.  Stage 0's cluster creates the entry; stage 1's cluster
    /// re-creates it.  The merge should succeed, keeping the later value.
    #[test]
    fn test_merge_created_then_created_keeps_later_value() {
        let mut delta1 = LedgerDelta::new(1);
        let mut delta2 = LedgerDelta::new(1);

        let entry_v1 = create_test_account_with_balance(1, 1_000);
        let entry_v2 = create_test_account_with_balance(1, 2_000);
        delta1.record_create(entry_v1).unwrap();
        delta2.record_create(entry_v2).unwrap();

        delta1.merge(delta2).unwrap();
        assert_eq!(delta1.num_changes(), 1);
        let changes: Vec<_> = delta1.changes().collect();
        assert!(changes[0].is_created());
        if let LedgerEntryData::Account(ref acc) = changes[0].current_entry().unwrap().data {
            assert_eq!(acc.balance, 2_000); // later value wins
        }
    }

    /// Merge error: create on existing updated entry.
    #[test]
    fn test_merge_create_on_existing_updated_fails() {
        let mut delta1 = LedgerDelta::new(1);
        let mut delta2 = LedgerDelta::new(1);

        let original = create_test_account(1);
        let updated = create_test_account_with_balance(1, 2_000);
        delta1.record_update(original, updated).unwrap();
        delta2.record_create(create_test_account(1)).unwrap();

        assert!(delta1.merge(delta2).is_err());
    }

    /// Merge: delete on already-deleted entry is idempotent.
    ///
    /// This occurs when parallel Soroban clusters independently delete the
    /// same entry (e.g. a TTL key present in multiple footprints).
    #[test]
    fn test_merge_delete_on_deleted_is_idempotent() {
        let mut delta1 = LedgerDelta::new(1);
        let mut delta2 = LedgerDelta::new(1);

        let entry = create_test_account(1);
        delta1.record_delete(entry.clone()).unwrap();
        delta2.record_delete(entry).unwrap();

        delta1.merge(delta2).unwrap();

        // Exactly one deletion should remain.
        assert_eq!(delta1.changes.len(), 1);
        assert!(matches!(
            delta1.changes.values().next().unwrap(),
            EntryChange::Deleted { .. }
        ));
    }

    /// Merge: Created + Updated = Created with the updated value.
    ///
    /// This occurs when one parallel cluster creates an entry and another
    /// cluster (or a later stage) updates it.  The merge should produce a
    /// single Created change with the final (updated) value, since the entry
    /// did not exist before the first delta.
    #[test]
    fn test_merge_created_then_updated_becomes_created_with_final_value() {
        let mut delta1 = LedgerDelta::new(1);
        let mut delta2 = LedgerDelta::new(1);

        let v0 = create_test_account(1);
        let v1 = create_test_account_with_balance(1, 2_000);
        let v2 = create_test_account_with_balance(1, 3_000);

        delta1.record_create(v0.clone()).unwrap();
        // delta2 sees v1 as the "previous" and updates to v2.
        delta2.record_update(v1.clone(), v2.clone()).unwrap();

        delta1.merge(delta2).unwrap();
        assert_eq!(delta1.num_changes(), 1);
        let changes: Vec<_> = delta1.changes().collect();
        // Should remain Created (not Updated) since the entry was new.
        assert!(changes[0].is_created());
        // Value should be v2 (final from the Update).
        if let LedgerEntryData::Account(ref acc) = changes[0].current_entry().unwrap().data {
            assert_eq!(acc.balance, 3_000);
        } else {
            panic!("expected Account entry");
        }
    }

    /// Merge: Updated + Deleted = Deleted with the original previous.
    ///
    /// This occurs when one delta updates an entry and a second delta deletes
    /// it. The merge should produce a Deleted change that preserves the
    /// original previous value (from before the first update).
    #[test]
    fn test_merge_updated_then_deleted_becomes_deleted_with_original_previous() {
        let mut delta1 = LedgerDelta::new(1);
        let mut delta2 = LedgerDelta::new(1);

        let v0 = create_test_account(1);
        let v1 = create_test_account_with_balance(1, 2_000);

        delta1.record_update(v0.clone(), v1.clone()).unwrap();
        delta2.record_delete(v1.clone()).unwrap();

        delta1.merge(delta2).unwrap();
        assert_eq!(delta1.num_changes(), 1);
        let changes: Vec<_> = delta1.changes().collect();
        // Should be Deleted.
        assert!(changes[0].is_deleted());
        // Previous should be v0 (the original value before the update).
        if let LedgerEntryData::Account(ref acc) = changes[0].previous_entry().unwrap().data {
            assert_eq!(acc.balance, 1_000_000_000); // v0 default balance
        } else {
            panic!("expected Account entry");
        }
    }

    // =========================================================================
    // Delta ordering test
    // =========================================================================

    /// Changes are returned in insertion order (deterministic).
    #[test]
    fn test_delta_deterministic_ordering() {
        let mut delta = LedgerDelta::new(1);

        // Insert in specific order
        for i in (0..10u8).rev() {
            delta.record_create(create_test_account(i)).unwrap();
        }

        // Verify changes come back in insertion order (9, 8, 7, ..., 0)
        let keys: Vec<u8> = delta
            .changes()
            .map(|c| {
                if let LedgerEntryData::Account(ref acc) = c.current_entry().unwrap().data {
                    match &acc.account_id.0 {
                        PublicKey::PublicKeyTypeEd25519(Uint256(bytes)) => bytes[0],
                    }
                } else {
                    panic!("expected account");
                }
            })
            .collect();
        assert_eq!(keys, vec![9, 8, 7, 6, 5, 4, 3, 2, 1, 0]);
    }

    /// Clear resets all state.
    #[test]
    fn test_delta_clear() {
        let mut delta = LedgerDelta::new(1);
        delta.record_create(create_test_account(1)).unwrap();
        delta.record_fee_pool_delta(100);
        delta.record_total_coins_delta(50);

        delta.clear();
        assert!(delta.is_empty());
        assert_eq!(delta.fee_pool_delta(), 0);
        assert_eq!(delta.total_coins_delta(), 0);
    }

    // =========================================================================
    // Idempotent delete test
    // =========================================================================

    /// Double delete on same entry is idempotent.
    #[test]
    fn test_double_delete_idempotent() {
        let mut delta = LedgerDelta::new(1);
        let entry = create_test_account(1);

        delta.record_delete(entry.clone()).unwrap();
        delta.record_delete(entry).unwrap(); // should be no-op

        assert_eq!(delta.num_changes(), 1);
        assert_eq!(delta.dead_entries().len(), 1);
    }

    /// Update then delete preserves original previous.
    #[test]
    fn test_update_then_delete_preserves_original() {
        let mut delta = LedgerDelta::new(1);
        let original = create_test_account(1);
        let updated = create_test_account_with_balance(1, 5_000);

        delta
            .record_update(original.clone(), updated.clone())
            .unwrap();
        delta.record_delete(updated).unwrap();

        assert_eq!(delta.num_changes(), 1);
        let changes: Vec<_> = delta.changes().collect();
        assert!(changes[0].is_deleted());
        // Previous should be the original, not the updated value
        if let LedgerEntryData::Account(ref acc) = changes[0].previous_entry().unwrap().data {
            assert_eq!(acc.balance, 1_000_000_000); // original
        }
    }

    #[test]
    fn test_delete_then_update() {
        // Scenario: TX1 deletes an entry, TX2 updates it (e.g., fee refund
        // restores the account). Net effect should be Updated.
        let mut delta = LedgerDelta::new(1);
        let original = create_test_account(1);
        let mut updated_entry = original.clone();
        if let LedgerEntryData::Account(ref mut acc) = updated_entry.data {
            acc.balance = 3000000000;
        }

        delta.record_delete(original.clone()).unwrap();
        delta
            .record_update(original.clone(), updated_entry.clone())
            .unwrap();

        assert_eq!(delta.num_changes(), 1);
        let changes: Vec<_> = delta.changes().collect();
        assert!(changes[0].is_updated());

        let current = changes[0].current_entry().unwrap();
        if let LedgerEntryData::Account(ref acc) = current.data {
            assert_eq!(acc.balance, 3000000000);
        } else {
            panic!("expected account entry");
        }

        // Should appear in live_entries (not init or dead)
        assert_eq!(delta.live_entries().len(), 1);
        assert_eq!(delta.init_entries().len(), 0);
        assert_eq!(delta.dead_entries().len(), 0);
    }

    // =========================================================================
    // P2-3: ConfigSetting deletion prevention
    // Parity: LedgerTxnTests.cpp:853 "fails for configuration"
    // =========================================================================

    fn create_config_setting_entry() -> LedgerEntry {
        use stellar_xdr::curr::ConfigSettingEntry;

        LedgerEntry {
            last_modified_ledger_seq: 1,
            data: LedgerEntryData::ConfigSetting(ConfigSettingEntry::ContractMaxSizeBytes(16384)),
            ext: LedgerEntryExt::V0,
        }
    }

    /// Parity: LedgerTxnTests.cpp:853 "fails for configuration"
    /// ConfigSetting entries cannot be erased.
    #[test]
    fn test_cannot_delete_config_setting() {
        let mut delta = LedgerDelta::new(1);
        let config = create_config_setting_entry();

        let result = delta.record_delete(config);
        assert!(result.is_err(), "should reject deletion of ConfigSetting");
        let err = result.unwrap_err();
        assert!(
            err.to_string().contains("cannot delete ConfigSetting"),
            "error should mention ConfigSetting, got: {}",
            err
        );

        // Delta should be empty - no change was recorded
        assert_eq!(delta.num_changes(), 0);
    }

    /// ConfigSetting entries can be created and updated (just not deleted).
    #[test]
    fn test_config_setting_create_and_update_allowed() {
        let mut delta = LedgerDelta::new(1);
        let config = create_config_setting_entry();

        // Create is allowed
        delta.record_create(config.clone()).unwrap();
        assert_eq!(delta.num_changes(), 1);

        // Update is allowed
        let mut updated = config.clone();
        if let LedgerEntryData::ConfigSetting(ref mut setting) = updated.data {
            *setting = stellar_xdr::curr::ConfigSettingEntry::ContractMaxSizeBytes(32768);
        }
        delta.record_update(config, updated).unwrap();
        assert_eq!(delta.num_changes(), 1);
        assert!(delta.changes().next().unwrap().is_created()); // create+update = created
    }

    // =========================================================================
    // P2-4: Load entry when erased
    // Parity: LedgerTxnTests.cpp:1509 "when key exists in grandparent, erased in parent"
    //
    // Tests that after deleting an entry, get_change shows it as deleted,
    // and that created-then-deleted entries vanish entirely.
    // =========================================================================

    /// After deleting an entry, get_change returns Deleted with the original.
    #[test]
    fn test_deleted_entry_shows_as_deleted_in_delta() {
        let mut delta = LedgerDelta::new(1);
        let entry = create_test_account(1);
        let key = henyey_common::entry_to_key(&entry);

        delta.record_delete(entry.clone()).unwrap();

        // get_change should return Deleted
        let change = delta.get_change(&key);
        assert!(change.is_some(), "deleted entry should be findable");
        assert!(change.unwrap().is_deleted());

        // dead_entries should contain the key
        let dead = delta.dead_entries();
        assert_eq!(dead.len(), 1);
    }

    /// Created then deleted = completely vanishes from delta (no-op).
    /// Parity: LedgerTxnTests.cpp "when key exists in grandparent, erased in parent"
    /// In stellar-core, erasing an entry erased by a parent throws. In Rust, created+deleted = removed.
    #[test]
    fn test_created_then_deleted_vanishes() {
        let mut delta = LedgerDelta::new(1);
        let entry = create_test_account(1);
        let key = henyey_common::entry_to_key(&entry);

        delta.record_create(entry.clone()).unwrap();
        assert_eq!(delta.num_changes(), 1);

        delta.record_delete(entry).unwrap();
        assert_eq!(delta.num_changes(), 0, "create+delete should cancel out");

        // Entry should not be findable
        let change = delta.get_change(&key);
        assert!(
            change.is_none(),
            "entry should have been removed from delta"
        );

        // No entries in any category
        assert!(delta.init_entries().is_empty());
        assert!(delta.live_entries().is_empty());
        assert!(delta.dead_entries().is_empty());
    }

    /// Entry deleted in delta cannot be loaded from delta (returns Deleted).
    /// The snapshot/state layer interprets Deleted as "entry does not exist".
    #[test]
    fn test_deleted_entry_previous_preserved() {
        let mut delta = LedgerDelta::new(1);
        let entry = create_test_account(1);
        let key = henyey_common::entry_to_key(&entry);

        // Set a specific balance so we can verify previous is preserved
        let mut custom = entry.clone();
        if let LedgerEntryData::Account(ref mut acc) = custom.data {
            acc.balance = 42_000_000;
        }

        delta.record_delete(custom.clone()).unwrap();

        let change = delta.get_change(&key).unwrap();
        assert!(change.is_deleted());

        // Previous entry should be the exact entry we deleted
        let prev = change.previous_entry().unwrap();
        if let LedgerEntryData::Account(ref acc) = prev.data {
            assert_eq!(acc.balance, 42_000_000);
        } else {
            panic!("expected account entry");
        }

        // Current entry should be None for deleted entries
        assert!(change.current_entry().is_none());
    }

    /// Applying a fee refund to an updated account modifies its balance.
    #[test]
    fn test_apply_refund_to_updated_account() {
        let mut delta = LedgerDelta::new(1);
        let original = create_test_account(1); // balance = 1_000_000_000
        let updated = create_test_account_with_balance(1, 900_000_000); // fee deducted

        delta
            .record_update(original.clone(), updated.clone())
            .unwrap();

        // Apply a refund of 50_000_000
        let account_id = if let LedgerEntryData::Account(ref acc) = original.data {
            acc.account_id.clone()
        } else {
            panic!("expected account");
        };
        delta
            .apply_refund_to_account(&account_id, 50_000_000)
            .unwrap();

        let changes: Vec<_> = delta.changes().collect();
        assert_eq!(changes.len(), 1);
        if let LedgerEntryData::Account(ref acc) = changes[0].current_entry().unwrap().data {
            assert_eq!(acc.balance, 950_000_000); // 900M + 50M refund
        } else {
            panic!("expected account entry");
        }
        // Previous should be unchanged
        if let LedgerEntryData::Account(ref acc) = changes[0].previous_entry().unwrap().data {
            assert_eq!(acc.balance, 1_000_000_000);
        }
    }

    /// Applying a fee refund to a created account modifies its balance.
    #[test]
    fn test_apply_refund_to_created_account() {
        let mut delta = LedgerDelta::new(1);
        let entry = create_test_account_with_balance(1, 500_000_000);

        delta.record_create(entry.clone()).unwrap();

        let account_id = if let LedgerEntryData::Account(ref acc) = entry.data {
            acc.account_id.clone()
        } else {
            panic!("expected account");
        };
        delta
            .apply_refund_to_account(&account_id, 25_000_000)
            .unwrap();

        let changes: Vec<_> = delta.changes().collect();
        if let LedgerEntryData::Account(ref acc) = changes[0].current_entry().unwrap().data {
            assert_eq!(acc.balance, 525_000_000);
        } else {
            panic!("expected account entry");
        }
    }

    /// Applying a refund to a nonexistent account is a no-op.
    #[test]
    fn test_apply_refund_to_missing_account() {
        let mut delta = LedgerDelta::new(1);
        let entry = create_test_account(1);
        let account_id = if let LedgerEntryData::Account(ref acc) = entry.data {
            acc.account_id.clone()
        } else {
            panic!("expected account");
        };
        // No entries in delta - refund is a no-op
        delta.apply_refund_to_account(&account_id, 100).unwrap();
        assert!(delta.is_empty());
    }

    // =========================================================================
    // current_entries() tests
    // =========================================================================

    /// current_entries returns created and updated entries but not deleted.
    #[test]
    fn test_current_entries_includes_created_and_updated() {
        let mut delta = LedgerDelta::new(1);

        let created = create_test_account(1);
        delta.record_create(created.clone()).unwrap();

        let prev = create_test_account(2);
        let updated = create_test_account_with_balance(2, 5_000);
        delta.record_update(prev, updated.clone()).unwrap();

        let deleted = create_test_account(3);
        delta.record_delete(deleted).unwrap();

        let entries = delta.current_entries();
        assert_eq!(entries.len(), 2); // created + updated, not deleted
    }

    /// current_entries on empty delta returns empty.
    #[test]
    fn test_current_entries_empty_delta() {
        let delta = LedgerDelta::new(1);
        assert!(delta.current_entries().is_empty());
    }
}
