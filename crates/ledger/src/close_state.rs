//! Close-time ledger state abstraction.
//!
//! [`CloseLedgerState`] is a flat merged-read wrapper over a base snapshot and a
//! mutable delta. Every read resolves: current delta → base snapshot, making
//! stale reads structurally impossible during ledger close orchestration.
//!
//! The upgrade and prepare-liabilities pipelines use `CloseLedgerState` for
//! consistent read-after-write visibility. The execution layer (parallel
//! Soroban, fee deduction) continues to operate on the decomposed
//! `(SnapshotHandle, LedgerDelta)` primitives via escape hatches — this is an
//! intentional architectural boundary, not a bug.
//!
//! # Checkpoint API
//!
//! Per-upgrade entry change extraction uses explicit checkpoints rather than
//! nested transaction scopes:
//!
//! ```text
//! let cp = state.change_checkpoint();
//! // ... apply upgrade ...
//! let changes = state.entry_changes_since(cp);
//! ```

use crate::delta::{DeltaCategorization, EntryChange, LedgerDelta};
use crate::snapshot::SnapshotHandle;
use crate::Result;
use std::collections::HashMap;
use stellar_xdr::curr::{
    AccountEntry, AccountId, LedgerEntry, LedgerEntryChange, LedgerEntryChanges, LedgerEntryData,
    LedgerHeader, LedgerKey,
};

/// A checkpoint of delta state, used to extract per-upgrade entry changes.
///
/// Created by [`CloseLedgerState::change_checkpoint`], consumed by
/// [`CloseLedgerState::entry_changes_since`].
///
/// Stores a snapshot of the delta's entries at checkpoint time so that
/// `entry_changes_since` can diff the current state against the checkpoint
/// and detect all modifications — including in-place updates to pre-existing
/// entries. This mirrors stellar-core's child LedgerTxn + getChanges() pattern.
pub struct ChangeCheckpoint {
    /// Snapshot of delta entry values at checkpoint time.
    entries: HashMap<LedgerKey, EntryChange>,
    /// Keys in insertion order at checkpoint time, for deterministic
    /// iteration over keys that disappeared from the delta after checkpoint.
    key_order: Vec<LedgerKey>,
}

/// A merged-read view of ledger state during close.
///
/// All reads resolve: current delta → base snapshot.
/// All writes accumulate in the current delta.
pub struct CloseLedgerState {
    /// Frozen base state.
    snapshot: SnapshotHandle,

    /// This level's changes.
    current: LedgerDelta,

    /// Post-upgrade header, updated as upgrades are applied.
    header: LedgerHeader,

    /// Hash of the previous ledger header.
    header_hash: henyey_common::Hash256,

    /// Ledger sequence for this close.
    ledger_seq: u32,
}

impl CloseLedgerState {
    // ------------------------------------------------------------------
    // Construction
    // ------------------------------------------------------------------

    /// Create a new close-state wrapper for ledger close.
    pub fn begin(
        snapshot: SnapshotHandle,
        header: LedgerHeader,
        header_hash: henyey_common::Hash256,
        ledger_seq: u32,
    ) -> Self {
        Self {
            snapshot,
            current: LedgerDelta::new(ledger_seq),
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
    /// Resolves: current delta → base snapshot.
    pub fn get_entry(&self, key: &LedgerKey) -> Result<Option<LedgerEntry>> {
        // 1. Check current delta
        if let Some(change) = self.current.get_change(key) {
            return Ok(change.current_entry().cloned());
        }
        // 2. Fall back to base snapshot
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

    /// Merged enumeration of all offers: snapshot offers overlaid with delta changes.
    ///
    /// Returns all live offer entries visible through the full read path:
    /// current delta + snapshot.
    pub fn all_offers(&self) -> Result<Vec<LedgerEntry>> {
        // Start with all offers from the base snapshot
        let snapshot_entries = self.snapshot.all_entries()?;

        // Collect all changes from current delta
        let mut overrides: std::collections::HashMap<LedgerKey, Option<LedgerEntry>> =
            std::collections::HashMap::new();

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

    /// Accumulate fee pool change.
    pub fn record_fee_pool_delta(&mut self, amount: i64) {
        self.current.record_fee_pool_delta(amount);
    }

    // ------------------------------------------------------------------
    // Accessors (replacing escape hatches)
    // ------------------------------------------------------------------

    /// Current fee pool delta accumulated across all operations.
    pub fn fee_pool_delta(&self) -> i64 {
        self.current.fee_pool_delta()
    }

    /// Current total coins delta accumulated across all operations.
    pub fn total_coins_delta(&self) -> i64 {
        self.current.total_coins_delta()
    }

    /// Number of entry changes in the current delta.
    pub fn num_changes(&self) -> usize {
        self.current.num_changes()
    }

    // ------------------------------------------------------------------
    // Checkpoint API for upgrade meta capture
    // ------------------------------------------------------------------

    /// Capture the current delta state as a checkpoint.
    ///
    /// Used before an upgrade sub-phase to snapshot the "before" state, so that
    /// [`entry_changes_since`] can diff the current state against it and detect
    /// all modifications — including in-place updates to pre-existing entries.
    ///
    /// Mirrors stellar-core's child LedgerTxn creation before each upgrade scope.
    pub fn change_checkpoint(&self) -> ChangeCheckpoint {
        ChangeCheckpoint {
            entries: self.current.snapshot_changes(),
            key_order: self.current.key_order(),
        }
    }

    /// Extract all entry changes made since the given checkpoint.
    ///
    /// Performs a snapshot-and-diff: compares the current delta state against
    /// the checkpoint snapshot to detect new entries, modified pre-existing
    /// entries, and deleted entries. This matches stellar-core's
    /// `LedgerTxn::getChanges()` parent-vs-child diff semantics.
    ///
    /// Returns an XDR `LedgerEntryChanges` suitable for upgrade metadata.
    pub fn entry_changes_since(&self, cp: ChangeCheckpoint) -> LedgerEntryChanges {
        let mut changes: Vec<LedgerEntryChange> = Vec::new();

        // Phase 1: Process all keys currently in the delta (insertion order).
        for (key, current_change) in self.current.keyed_changes() {
            if let Some(cp_change) = cp.entries.get(key) {
                // Key existed at checkpoint — compare visible states.
                let before = visible_entry(cp_change);
                let after = visible_entry(current_change);
                emit_diff(&mut changes, key, before, after);
            } else {
                // Key is new to the delta since checkpoint.
                // The EntryChange variant encodes the correct emission:
                // - Created = genuinely new entry
                // - Updated = snapshot entry was modified (previous is pre-mod value)
                // - Deleted = snapshot entry was deleted (previous is pre-del value)
                emit_new_entry(&mut changes, current_change);
            }
        }

        // Phase 2: Keys in checkpoint that disappeared from the current delta.
        // This only happens when record_delete removes a Created entry entirely
        // (delta.rs: Created + Delete = remove from delta). If the entry was
        // visible at checkpoint time, emit STATE + REMOVED.
        for key in &cp.key_order {
            if self.current.has_key(key) {
                continue; // Already handled in Phase 1
            }
            if let Some(cp_change) = cp.entries.get(key) {
                if let Some(v1) = visible_entry(cp_change) {
                    changes.push(LedgerEntryChange::State(v1.clone()));
                    changes.push(LedgerEntryChange::Removed(henyey_common::entry_to_key(v1)));
                }
            }
        }

        LedgerEntryChanges(changes.try_into().unwrap_or_default())
    }

    // ------------------------------------------------------------------
    // Terminal operations
    // ------------------------------------------------------------------

    /// Drain the delta for bucket list update.
    ///
    /// This is the proper terminal operation — consumes the delta's entry
    /// changes for the bucket list, leaving the `CloseLedgerState` in a
    /// drained state.
    pub fn drain_for_bucket_update(&mut self) -> DeltaCategorization {
        self.current.drain_categorization_for_bucket_update()
    }

    /// Release the snapshot's lookup closures to drop captured Arc references.
    ///
    /// Called after `drain_for_bucket_update()` during commit, before the
    /// soroban state update phase. This drops the Arc references to the
    /// soroban state snapshot held by the lookup closures, allowing
    /// `Arc::make_mut` on the live soroban state to mutate in-place instead
    /// of deep-cloning the entire HashMap.
    pub fn release_snapshot_lookups(&mut self) {
        self.snapshot.release_lookups();
    }

    // ------------------------------------------------------------------
    // Escape hatches for the execution layer
    // ------------------------------------------------------------------

    /// Mutable access to the current delta for the execution layer.
    ///
    /// The execution layer (`tx_set.rs`, `mod.rs`) uses `LedgerDelta` directly
    /// for recording per-TX results and fee pool changes. This provides access
    /// to the current delta at the execution-layer boundary.
    pub(crate) fn current_delta_mut(&mut self) -> &mut LedgerDelta {
        &mut self.current
    }

    /// Immutable access to the current delta.
    ///
    /// Used for iterating current entries (e.g., executor seeding) and other
    /// execution-layer operations that need the raw delta.
    pub(crate) fn current_delta(&self) -> &LedgerDelta {
        &self.current
    }
}

impl crate::EntryReader for CloseLedgerState {
    fn get_entry(&self, key: &LedgerKey) -> crate::Result<Option<LedgerEntry>> {
        CloseLedgerState::get_entry(self, key)
    }
}

/// Check if a ledger key is for an offer entry.
fn is_offer_key(key: &LedgerKey) -> bool {
    matches!(key, LedgerKey::Offer(_))
}

/// Extract the visible LedgerEntry from an EntryChange.
///
/// "Visible" means the entry value that would be seen by readers:
/// - Created(v) → Some(v)
/// - Updated{_, curr} → Some(curr)
/// - Deleted{_} → None (entry is absent)
fn visible_entry(change: &EntryChange) -> Option<&LedgerEntry> {
    match change {
        EntryChange::Created(entry) => Some(entry),
        EntryChange::Updated { current, .. } => Some(current.as_ref()),
        EntryChange::Deleted { .. } => None,
    }
}

/// Emit LedgerEntryChanges for a diff between before/after visible states.
///
/// Matches stellar-core's parent-vs-child comparison in getChanges().
fn emit_diff(
    changes: &mut Vec<LedgerEntryChange>,
    key: &LedgerKey,
    before: Option<&LedgerEntry>,
    after: Option<&LedgerEntry>,
) {
    match (before, after) {
        (Some(v1), Some(v2)) if v1 != v2 => {
            changes.push(LedgerEntryChange::State(v1.clone()));
            changes.push(LedgerEntryChange::Updated(v2.clone()));
        }
        (Some(v1), None) => {
            changes.push(LedgerEntryChange::State(v1.clone()));
            changes.push(LedgerEntryChange::Removed(key.clone()));
        }
        (None, Some(v2)) => {
            changes.push(LedgerEntryChange::Created(v2.clone()));
        }
        _ => {} // same or both absent — no emission
    }
}

/// Emit LedgerEntryChanges for a key new to the delta since checkpoint.
///
/// The EntryChange variant directly determines the emission type:
/// - Created = genuinely new entry (CREATED)
/// - Updated = snapshot entry was modified (STATE + UPDATED)
/// - Deleted = snapshot entry was deleted (STATE + REMOVED)
fn emit_new_entry(changes: &mut Vec<LedgerEntryChange>, change: &EntryChange) {
    match change {
        EntryChange::Created(entry) => {
            changes.push(LedgerEntryChange::Created(entry.clone()));
        }
        EntryChange::Updated { previous, current } => {
            changes.push(LedgerEntryChange::State(previous.clone()));
            changes.push(LedgerEntryChange::Updated(current.as_ref().clone()));
        }
        EntryChange::Deleted { previous } => {
            changes.push(LedgerEntryChange::State(previous.clone()));
            changes.push(LedgerEntryChange::Removed(henyey_common::entry_to_key(
                previous,
            )));
        }
    }
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
        let mut state =
            CloseLedgerState::begin(snapshot, header, henyey_common::Hash256::ZERO, 101);

        // Initially empty
        let key = make_account_key(1);
        assert!(state.get_entry(&key).unwrap().is_none());

        // Create an entry
        let entry = make_test_account_entry(1, 1000, 101);
        state.record_create(entry.clone()).unwrap();

        // Now visible
        let loaded = state.get_entry(&key).unwrap().unwrap();
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
        let state = CloseLedgerState::begin(snapshot, header, henyey_common::Hash256::ZERO, 101);

        let key = make_account_key(1);
        let loaded = state.get_entry(&key).unwrap().unwrap();
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
        let mut state =
            CloseLedgerState::begin(snapshot, header, henyey_common::Hash256::ZERO, 101);

        // Update overrides snapshot
        let new_entry = make_test_account_entry(1, 3000, 101);
        state.record_update(old_entry, new_entry.clone()).unwrap();

        let key = make_account_key(1);
        let loaded = state.get_entry(&key).unwrap().unwrap();
        assert_eq!(loaded, new_entry);
    }

    #[test]
    fn test_change_checkpoint() {
        let snapshot = make_empty_snapshot(100);
        let header = LedgerHeader {
            ledger_version: 25,
            ledger_seq: 100,
            ..Default::default()
        };
        let mut state =
            CloseLedgerState::begin(snapshot, header, henyey_common::Hash256::ZERO, 101);

        // Create an entry before the checkpoint
        let entry1 = make_test_account_entry(1, 1000, 101);
        state.record_create(entry1).unwrap();

        // Take checkpoint
        let cp = state.change_checkpoint();

        // Create another entry after the checkpoint
        let entry2 = make_test_account_entry(2, 2000, 101);
        state.record_create(entry2).unwrap();

        // Only the second entry should appear in changes since checkpoint
        let changes = state.entry_changes_since(cp);
        assert_eq!(changes.0.len(), 1);
        assert!(matches!(&changes.0[0], LedgerEntryChange::Created(_)));
    }

    #[test]
    fn test_change_checkpoint_empty() {
        let snapshot = make_empty_snapshot(100);
        let header = LedgerHeader {
            ledger_version: 25,
            ledger_seq: 100,
            ..Default::default()
        };
        let state = CloseLedgerState::begin(snapshot, header, henyey_common::Hash256::ZERO, 101);

        let cp = state.change_checkpoint();
        let changes = state.entry_changes_since(cp);
        assert_eq!(changes.0.len(), 0);
    }

    #[test]
    fn test_fee_pool_and_coins_delta() {
        let snapshot = make_empty_snapshot(100);
        let header = LedgerHeader {
            ledger_version: 25,
            ledger_seq: 100,
            ..Default::default()
        };
        let mut state =
            CloseLedgerState::begin(snapshot, header, henyey_common::Hash256::ZERO, 101);

        state.record_fee_pool_delta(500);
        assert_eq!(state.fee_pool_delta(), 500);

        // total_coins_delta is exposed via accessor (write goes through delta)
        state.current_delta_mut().record_total_coins_delta(-100);
        assert_eq!(state.total_coins_delta(), -100);
    }

    #[test]
    fn test_drain_preserves_metadata_deltas() {
        let snapshot = make_empty_snapshot(100);
        let header = LedgerHeader {
            ledger_version: 25,
            ledger_seq: 100,
            ..Default::default()
        };
        let mut state =
            CloseLedgerState::begin(snapshot, header, henyey_common::Hash256::ZERO, 101);

        // Accumulate some entry changes plus metadata deltas
        let entry = make_test_account_entry(1, 1000, 101);
        state.record_create(entry).unwrap();
        state.record_fee_pool_delta(750);
        state.current_delta_mut().record_total_coins_delta(-200);

        assert_eq!(state.num_changes(), 1);
        assert_eq!(state.fee_pool_delta(), 750);
        assert_eq!(state.total_coins_delta(), -200);

        // Drain entry changes for bucket update
        let categorization = state.drain_for_bucket_update();
        assert_eq!(categorization.created_count, 1);

        // After drain, entry changes are gone but metadata deltas are preserved.
        // This is critical: manager.rs reads fee_pool_delta() and total_coins_delta()
        // after draining to construct the final ledger header.
        assert_eq!(state.num_changes(), 0);
        assert_eq!(state.fee_pool_delta(), 750);
        assert_eq!(state.total_coins_delta(), -200);
    }

    // ------------------------------------------------------------------
    // Checkpoint snapshot-and-diff tests
    // ------------------------------------------------------------------

    #[test]
    fn test_entry_changes_since_detects_pre_existing_modification() {
        // Bug scenario: entry modified in delta before checkpoint, then modified
        // again after checkpoint. The old skip-based approach missed this.
        let old_entry = make_test_account_entry(1, 5000, 99);
        let snapshot = make_snapshot_with_entry(100, old_entry.clone());
        let header = LedgerHeader {
            ledger_version: 25,
            ledger_seq: 100,
            ..Default::default()
        };
        let mut state =
            CloseLedgerState::begin(snapshot, header, henyey_common::Hash256::ZERO, 101);

        // Modify entry (e.g. fee processing) — adds to delta as Updated
        let mid_entry = make_test_account_entry(1, 4000, 101);
        state.record_update(old_entry, mid_entry.clone()).unwrap();

        // Take checkpoint
        let cp = state.change_checkpoint();

        // Modify same entry again (e.g. prepareLiabilities during upgrade)
        let final_entry = make_test_account_entry(1, 3000, 101);
        state
            .record_update(mid_entry.clone(), final_entry.clone())
            .unwrap();

        // Should detect the modification: STATE(mid) + UPDATED(final)
        let changes = state.entry_changes_since(cp);
        assert_eq!(changes.0.len(), 2, "Expected STATE + UPDATED pair");
        assert!(matches!(&changes.0[0], LedgerEntryChange::State(e) if *e == mid_entry));
        assert!(matches!(&changes.0[1], LedgerEntryChange::Updated(e) if *e == final_entry));
    }

    #[test]
    fn test_entry_changes_since_created_then_deleted() {
        // Entry created before checkpoint, then deleted after.
        // record_delete on Created removes from delta entirely → Phase 2 catches it.
        let snapshot = make_empty_snapshot(100);
        let header = LedgerHeader {
            ledger_version: 25,
            ledger_seq: 100,
            ..Default::default()
        };
        let mut state =
            CloseLedgerState::begin(snapshot, header, henyey_common::Hash256::ZERO, 101);

        // Create entry
        let entry = make_test_account_entry(1, 1000, 101);
        state.record_create(entry.clone()).unwrap();

        // Take checkpoint (entry is visible as Created)
        let cp = state.change_checkpoint();

        // Delete the entry — removes Created from delta entirely
        state.record_delete(entry.clone()).unwrap();

        // Should emit STATE(entry) + REMOVED(key) via Phase 2
        let changes = state.entry_changes_since(cp);
        assert_eq!(changes.0.len(), 2, "Expected STATE + REMOVED pair");
        assert!(matches!(&changes.0[0], LedgerEntryChange::State(e) if *e == entry));
        assert!(matches!(&changes.0[1], LedgerEntryChange::Removed(_)));
    }

    #[test]
    fn test_entry_changes_since_created_then_modified() {
        // Entry created before checkpoint, then modified after (via record_create on existing).
        let snapshot = make_empty_snapshot(100);
        let header = LedgerHeader {
            ledger_version: 25,
            ledger_seq: 100,
            ..Default::default()
        };
        let mut state =
            CloseLedgerState::begin(snapshot, header, henyey_common::Hash256::ZERO, 101);

        // Create entry
        let entry_v1 = make_test_account_entry(1, 1000, 101);
        state.record_create(entry_v1.clone()).unwrap();

        // Take checkpoint
        let cp = state.change_checkpoint();

        // Modify via record_create on existing key (updates in-place)
        let entry_v2 = make_test_account_entry(1, 2000, 101);
        state.record_create(entry_v2.clone()).unwrap();

        // Should emit STATE(v1) + UPDATED(v2)
        let changes = state.entry_changes_since(cp);
        assert_eq!(changes.0.len(), 2, "Expected STATE + UPDATED pair");
        assert!(matches!(&changes.0[0], LedgerEntryChange::State(e) if *e == entry_v1));
        assert!(matches!(&changes.0[1], LedgerEntryChange::Updated(e) if *e == entry_v2));
    }

    #[test]
    fn test_entry_changes_since_snapshot_entry_updated() {
        // Empty delta at checkpoint, then snapshot entry modified → STATE + UPDATED.
        // This is the config upgrade scenario (config_upgrade.rs tests).
        let old_entry = make_test_account_entry(1, 5000, 99);
        let snapshot = make_snapshot_with_entry(100, old_entry.clone());
        let header = LedgerHeader {
            ledger_version: 25,
            ledger_seq: 100,
            ..Default::default()
        };
        let mut state =
            CloseLedgerState::begin(snapshot, header, henyey_common::Hash256::ZERO, 101);

        // Checkpoint with empty delta
        let cp = state.change_checkpoint();

        // Modify snapshot entry (new to delta as Updated{prev, curr})
        let new_entry = make_test_account_entry(1, 3000, 101);
        state
            .record_update(old_entry.clone(), new_entry.clone())
            .unwrap();

        // Should emit STATE(old) + UPDATED(new) via emit_new_entry
        let changes = state.entry_changes_since(cp);
        assert_eq!(changes.0.len(), 2, "Expected STATE + UPDATED pair");
        assert!(matches!(&changes.0[0], LedgerEntryChange::State(e) if *e == old_entry));
        assert!(matches!(&changes.0[1], LedgerEntryChange::Updated(e) if *e == new_entry));
    }

    #[test]
    fn test_entry_changes_since_snapshot_entry_deleted() {
        // Empty delta at checkpoint, then snapshot entry deleted → STATE + REMOVED.
        let old_entry = make_test_account_entry(1, 5000, 99);
        let snapshot = make_snapshot_with_entry(100, old_entry.clone());
        let header = LedgerHeader {
            ledger_version: 25,
            ledger_seq: 100,
            ..Default::default()
        };
        let mut state =
            CloseLedgerState::begin(snapshot, header, henyey_common::Hash256::ZERO, 101);

        // Checkpoint with empty delta
        let cp = state.change_checkpoint();

        // Delete snapshot entry (new to delta as Deleted{prev})
        state.record_delete(old_entry.clone()).unwrap();

        // Should emit STATE(old) + REMOVED(key) via emit_new_entry
        let changes = state.entry_changes_since(cp);
        assert_eq!(changes.0.len(), 2, "Expected STATE + REMOVED pair");
        assert!(matches!(&changes.0[0], LedgerEntryChange::State(e) if *e == old_entry));
        assert!(matches!(&changes.0[1], LedgerEntryChange::Removed(_)));
    }

    #[test]
    fn test_entry_changes_since_deleted_then_recreated() {
        // Entry deleted before checkpoint (Deleted in delta), then recreated after.
        // Visible state: absent → present = CREATED.
        let old_entry = make_test_account_entry(1, 5000, 99);
        let snapshot = make_snapshot_with_entry(100, old_entry.clone());
        let header = LedgerHeader {
            ledger_version: 25,
            ledger_seq: 100,
            ..Default::default()
        };
        let mut state =
            CloseLedgerState::begin(snapshot, header, henyey_common::Hash256::ZERO, 101);

        // Delete entry (adds Deleted{prev} to delta)
        state.record_delete(old_entry.clone()).unwrap();

        // Take checkpoint (entry visible state = absent/Deleted)
        let cp = state.change_checkpoint();

        // Recreate entry (record_create on Deleted = Updated with new value)
        let new_entry = make_test_account_entry(1, 8000, 101);
        state.record_create(new_entry.clone()).unwrap();

        // Visible state went from absent (Deleted) to present → CREATED
        let changes = state.entry_changes_since(cp);
        assert_eq!(changes.0.len(), 1, "Expected CREATED");
        assert!(matches!(&changes.0[0], LedgerEntryChange::Created(e) if *e == new_entry));
    }

    #[test]
    fn test_entry_changes_since_mixed_scenarios() {
        // Multiple entries with different before/after transitions.
        let snapshot_entry = make_test_account_entry(3, 9000, 99);
        let snapshot = make_snapshot_with_entry(100, snapshot_entry.clone());
        let header = LedgerHeader {
            ledger_version: 25,
            ledger_seq: 100,
            ..Default::default()
        };
        let mut state =
            CloseLedgerState::begin(snapshot, header, henyey_common::Hash256::ZERO, 101);

        // Entry 1: created before checkpoint, unchanged after → no emission
        let entry1 = make_test_account_entry(1, 1000, 101);
        state.record_create(entry1.clone()).unwrap();

        // Entry 2: created before checkpoint, modified after → STATE + UPDATED
        let entry2_v1 = make_test_account_entry(2, 2000, 101);
        state.record_create(entry2_v1.clone()).unwrap();

        // Take checkpoint
        let cp = state.change_checkpoint();

        // Modify entry 2
        let entry2_v2 = make_test_account_entry(2, 2500, 101);
        state.record_create(entry2_v2.clone()).unwrap();

        // Entry 4: new after checkpoint → CREATED
        let entry4 = make_test_account_entry(4, 4000, 101);
        state.record_create(entry4.clone()).unwrap();

        // Entry 3 (snapshot): modify after checkpoint → STATE + UPDATED (new to delta)
        let entry3_new = make_test_account_entry(3, 7000, 101);
        state
            .record_update(snapshot_entry.clone(), entry3_new.clone())
            .unwrap();

        let changes = state.entry_changes_since(cp);
        // Expected:
        // - Entry 1: unchanged → skip
        // - Entry 2: STATE(v1) + UPDATED(v2) [pre-existing, modified]
        // - Entry 4: CREATED [new]
        // - Entry 3: STATE(snapshot) + UPDATED(new) [new to delta, Updated variant]
        assert_eq!(
            changes.0.len(),
            5,
            "Expected 5 change entries: {:?}",
            changes
                .0
                .iter()
                .map(std::mem::discriminant)
                .collect::<Vec<_>>()
        );

        // Entry 2: STATE + UPDATED
        assert!(matches!(&changes.0[0], LedgerEntryChange::State(e) if *e == entry2_v1));
        assert!(matches!(&changes.0[1], LedgerEntryChange::Updated(e) if *e == entry2_v2));
        // Entry 4: CREATED
        assert!(matches!(&changes.0[2], LedgerEntryChange::Created(e) if *e == entry4));
        // Entry 3: STATE + UPDATED (from snapshot)
        assert!(matches!(&changes.0[3], LedgerEntryChange::State(e) if *e == snapshot_entry));
        assert!(matches!(&changes.0[4], LedgerEntryChange::Updated(e) if *e == entry3_new));
    }

    #[test]
    fn test_entry_changes_since_unchanged_entry_skipped() {
        // Entry in delta before and after checkpoint but not modified → no emission.
        let old_entry = make_test_account_entry(1, 5000, 99);
        let snapshot = make_snapshot_with_entry(100, old_entry.clone());
        let header = LedgerHeader {
            ledger_version: 25,
            ledger_seq: 100,
            ..Default::default()
        };
        let mut state =
            CloseLedgerState::begin(snapshot, header, henyey_common::Hash256::ZERO, 101);

        // Modify entry (adds to delta)
        let new_entry = make_test_account_entry(1, 4000, 101);
        state.record_update(old_entry, new_entry).unwrap();

        // Take checkpoint
        let cp = state.change_checkpoint();

        // Don't modify the entry after checkpoint
        // Should produce no changes
        let changes = state.entry_changes_since(cp);
        assert_eq!(
            changes.0.len(),
            0,
            "Expected no changes for unmodified entry"
        );
    }
}
