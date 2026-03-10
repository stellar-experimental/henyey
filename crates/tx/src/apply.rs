//! Transaction application for replay/catchup mode.
//!
//! This module handles applying transactions during catchup/replay by trusting
//! the historical results from the archive rather than re-executing transactions.
//!
//! # Overview
//!
//! During catchup, we have access to:
//! - The original transaction envelope
//! - The recorded result (success/failure, fee charged, operation results)
//! - The transaction metadata (state changes: creates, updates, deletes)
//!
//! Instead of re-executing the transaction (which would require the exact
//! ledger state and could differ due to protocol changes), we apply the
//! recorded state changes directly. This is both faster and guaranteed to
//! produce the same ledger hash as the original execution.
//!
//! # Why Replay Instead of Re-execute?
//!
//! Re-executing historical transactions is problematic for several reasons:
//!
//! 1. **Protocol Evolution**: Older transactions may have been validated under
//!    different rules. Re-execution with current code could reject valid
//!    historical transactions or produce different results.
//!
//! 2. **State Dependencies**: Full execution requires the exact ledger state
//!    at the time of original execution, which may not be available.
//!
//! 3. **Soroban Determinism**: Smart contract execution depends on PRNG seeds
//!    and network configuration that must match exactly.
//!
//! 4. **Performance**: Replaying metadata is significantly faster than
//!    re-executing complex operations like path payments or contract calls.
//!
//! # Key Types
//!
//! - [`LedgerDelta`]: Accumulates state changes during transaction application.
//!   Tracks creates, updates, and deletes with their pre-states for proper
//!   metadata generation.
//!
//! - [`ApplyContext`]: Provides ledger context (sequence, close time, protocol
//!   version, network ID) needed for transaction application.
//!
//! - [`ChangeRef`]: References a change by type and index, preserving the exact
//!   order of state modifications for correct metadata construction.
//!
//! # Change Ordering
//!
//! The order of changes in transaction metadata is significant:
//!
//! ```text
//! Transaction Meta Structure:
//! +---------------------------+
//! | tx_changes_before         |  <- Fee deduction, sequence bump
//! +---------------------------+
//! | operation[0].changes      |  <- First operation's state changes
//! | operation[1].changes      |  <- Second operation's state changes
//! | ...                       |
//! +---------------------------+
//! | tx_changes_after          |  <- Post-operation adjustments
//! +---------------------------+
//! ```
//!
//! [`LedgerDelta`] preserves this ordering through `change_order`, allowing
//! metadata to be reconstructed exactly as recorded.
//!
//! # Usage Example
//!
//! ```ignore
//! use henyey_tx::{apply_from_history, LedgerDelta, TransactionFrame};
//!
//! let frame = TransactionFrame::new(envelope);
//! let mut delta = LedgerDelta::new(ledger_seq);
//!
//! // Apply the historical transaction
//! let result = apply_from_history(&frame, &tx_result, &tx_meta, &mut delta)?;
//!
//! // Delta now contains all state changes in execution order
//! for entry in delta.created_entries() {
//!     bucket_list.add(entry)?;
//! }
//! for entry in delta.updated_entries() {
//!     bucket_list.update(entry)?;
//! }
//! for key in delta.deleted_keys() {
//!     bucket_list.delete(key)?;
//! }
//! ```

use stellar_xdr::curr::{
    AccountEntry, AccountId, LedgerEntry, LedgerEntryChange, LedgerEntryChanges, LedgerEntryData,
    LedgerKey, LedgerKeyAccount, LedgerKeyClaimableBalance, LedgerKeyContractCode,
    LedgerKeyContractData, LedgerKeyData, LedgerKeyLiquidityPool, LedgerKeyOffer,
    LedgerKeyTrustLine, LedgerKeyTtl, TransactionMeta, TransactionResult,
};

use crate::frame::TransactionFrame;
use crate::result::{TxApplyResult, TxResultWrapper};
use crate::Result;

/// Reference to a change in a [`LedgerDelta`], preserving execution order.
///
/// During transaction execution, changes (creates, updates, deletes) can be
/// interleaved. This enum tracks the order so that changes can be replayed
/// in the correct sequence when building transaction metadata.
#[derive(Clone, Copy, Debug)]
pub enum ChangeRef {
    /// Index into the delta's created entries vector.
    Created(usize),
    /// Index into the delta's updated entries vector.
    Updated(usize),
    /// Index into the delta's deleted entries vector.
    Deleted(usize),
}

/// Accumulator for ledger state changes during transaction execution.
///
/// `LedgerDelta` collects all creates, updates, and deletes that occur during
/// transaction execution. It maintains both the new state (for updates) and
/// the pre-state (for building proper transaction metadata).
///
/// # Structure
///
/// - **Created**: New entries that didn't exist before
/// - **Updated**: Modified entries (stores both pre-state and post-state)
/// - **Deleted**: Removed entries (stores key and pre-state)
///
/// # Order Preservation
///
/// The `change_order` field tracks the sequence of changes for metadata construction.
///
/// # Example
///
/// ```ignore
/// let mut delta = LedgerDelta::new(ledger_seq);
///
/// // Record changes during execution
/// delta.record_create(new_account_entry);
/// delta.record_update(old_balance, new_balance);
/// delta.record_delete(trustline_key, trustline_entry);
///
/// // Access changes for bucket list updates
/// for entry in delta.created_entries() {
///     bucket_list.add(entry)?;
/// }
/// ```
/// Captured vector lengths for savepoint support.
#[derive(Clone)]
pub struct DeltaLengths {
    pub created: usize,
    pub updated: usize,
    pub deleted: usize,
    pub change_order: usize,
}

#[derive(Clone)]
pub struct LedgerDelta {
    /// Ledger sequence this delta applies to.
    ledger_seq: u32,
    /// Entries created.
    created: Vec<LedgerEntry>,
    /// Entries updated (post-state after modification).
    updated: Vec<LedgerEntry>,
    /// Pre-state for each updated entry (state before modification).
    /// Parallel to `updated` - same length, same indices.
    update_states: Vec<LedgerEntry>,
    /// Entries deleted.
    deleted: Vec<LedgerKey>,
    /// Pre-state for each deleted entry (state before deletion).
    /// Parallel to `deleted` - same length, same indices.
    delete_states: Vec<LedgerEntry>,
    /// Fee charged.
    fee_charged: i64,
    /// Order in which changes were recorded (for preserving execution order in meta).
    change_order: Vec<ChangeRef>,
}

impl LedgerDelta {
    /// Create a new delta for the given ledger sequence.
    pub fn new(ledger_seq: u32) -> Self {
        Self {
            ledger_seq,
            created: Vec::new(),
            updated: Vec::new(),
            update_states: Vec::new(),
            deleted: Vec::new(),
            delete_states: Vec::new(),
            fee_charged: 0,
            change_order: Vec::new(),
        }
    }

    /// Get the ledger sequence.
    pub fn ledger_seq(&self) -> u32 {
        self.ledger_seq
    }

    /// Record a created entry.
    pub fn record_create(&mut self, entry: LedgerEntry) {
        if let stellar_xdr::curr::LedgerEntryData::Ttl(ttl) = &entry.data {
            tracing::debug!(
                key_hash = ?ttl.key_hash,
                "LedgerDelta::record_create for Ttl"
            );
        }
        let idx = self.created.len();
        self.created.push(entry);
        self.change_order.push(ChangeRef::Created(idx));
    }

    /// Record an updated entry with its pre-state.
    ///
    /// `pre_state` is the entry value BEFORE the modification.
    /// `post_state` is the entry value AFTER the modification.
    pub fn record_update(&mut self, pre_state: LedgerEntry, post_state: LedgerEntry) {
        if let stellar_xdr::curr::LedgerEntryData::Ttl(ttl) = &post_state.data {
            tracing::debug!(
                key_hash = ?ttl.key_hash,
                "LedgerDelta::record_update for Ttl"
            );
        }
        let idx = self.updated.len();
        self.update_states.push(pre_state);
        self.updated.push(post_state);
        self.change_order.push(ChangeRef::Updated(idx));
    }

    /// Update a TTL entry that was previously created in the same transaction.
    ///
    /// This is used when a TTL is extended after creation - we want the CREATED
    /// entry to reflect the final value, not the initial value, without emitting
    /// a separate STATE+UPDATED pair.
    pub fn update_created_ttl(
        &mut self,
        key_hash: &stellar_xdr::curr::Hash,
        ttl_entry: &stellar_xdr::curr::TtlEntry,
    ) {
        use stellar_xdr::curr::LedgerEntryData;

        // Find the TTL entry in created with matching key_hash
        for entry in &mut self.created {
            if let LedgerEntryData::Ttl(ttl) = &entry.data {
                if ttl.key_hash == *key_hash {
                    // Update the TTL value in the created entry
                    entry.data = LedgerEntryData::Ttl(stellar_xdr::curr::TtlEntry {
                        key_hash: key_hash.clone(),
                        live_until_ledger_seq: ttl_entry.live_until_ledger_seq,
                    });
                    return;
                }
            }
        }
    }

    /// Record a deleted entry with its pre-state.
    ///
    /// `pre_state` is the entry value BEFORE deletion.
    pub fn record_delete(&mut self, key: LedgerKey, pre_state: LedgerEntry) {
        let idx = self.deleted.len();
        self.delete_states.push(pre_state);
        self.deleted.push(key);
        self.change_order.push(ChangeRef::Deleted(idx));
    }

    /// Add fee charged.
    pub fn add_fee(&mut self, fee: i64) {
        self.fee_charged += fee;
    }

    /// Set fee_charged to an exact value (used by delta snapshot rollback).
    pub fn set_fee_charged(&mut self, fee: i64) {
        self.fee_charged = fee;
    }

    /// Get all created entries.
    pub fn created_entries(&self) -> &[LedgerEntry] {
        &self.created
    }

    /// Get all updated entries (post-state after modification).
    pub fn updated_entries(&self) -> &[LedgerEntry] {
        &self.updated
    }

    /// Get all update pre-states (state before modification).
    /// Parallel to `updated_entries()` - same length, same indices.
    pub fn update_states(&self) -> &[LedgerEntry] {
        &self.update_states
    }

    /// Get all deleted keys.
    pub fn deleted_keys(&self) -> &[LedgerKey] {
        &self.deleted
    }

    /// Get all delete pre-states (state before deletion).
    /// Parallel to `deleted_keys()` - same length, same indices.
    pub fn delete_states(&self) -> &[LedgerEntry] {
        &self.delete_states
    }

    /// Get total fee charged.
    pub fn fee_charged(&self) -> i64 {
        self.fee_charged
    }

    /// Get the change order (for preserving execution order in meta).
    pub fn change_order(&self) -> &[ChangeRef] {
        &self.change_order
    }

    /// Get the total number of changes.
    pub fn change_count(&self) -> usize {
        self.created.len() + self.updated.len() + self.deleted.len()
    }

    /// Check if this delta has any changes.
    pub fn has_changes(&self) -> bool {
        !self.created.is_empty() || !self.updated.is_empty() || !self.deleted.is_empty()
    }

    /// Apply a fee refund to the most recent update of a specific account.
    ///
    /// This modifies the balance in the post-state of the most recent account update.
    /// If no update exists for this account, the refund is not applied.
    pub fn apply_refund_to_account(&mut self, account_id: &AccountId, refund: i64) {
        use stellar_xdr::curr::LedgerEntryData;

        // Find the last update for this account and modify its balance
        for entry in self.updated.iter_mut().rev() {
            if let LedgerEntryData::Account(acc) = &mut entry.data {
                if &acc.account_id == account_id {
                    acc.balance += refund;
                    return;
                }
            }
        }
    }

    /// Capture the current vector lengths for savepoint support.
    pub fn snapshot_lengths(&self) -> DeltaLengths {
        DeltaLengths {
            created: self.created.len(),
            updated: self.updated.len(),
            deleted: self.deleted.len(),
            change_order: self.change_order.len(),
        }
    }

    /// Truncate all vectors back to the given lengths.
    /// Used by savepoint rollback to undo speculative delta entries.
    pub fn truncate_to(&mut self, lengths: &DeltaLengths) {
        self.created.truncate(lengths.created);
        self.updated.truncate(lengths.updated);
        self.update_states.truncate(lengths.updated);
        self.deleted.truncate(lengths.deleted);
        self.delete_states.truncate(lengths.deleted);
        self.change_order.truncate(lengths.change_order);
    }

    /// Merge another delta into this one.
    pub fn merge(&mut self, other: LedgerDelta) {
        // Track offsets for adjusting indices in change_order
        let created_offset = self.created.len();
        let updated_offset = self.updated.len();
        let deleted_offset = self.deleted.len();

        self.created.extend(other.created);
        self.updated.extend(other.updated);
        self.update_states.extend(other.update_states);
        self.deleted.extend(other.deleted);
        self.delete_states.extend(other.delete_states);
        self.fee_charged += other.fee_charged;

        // Merge change order with adjusted indices
        for change_ref in other.change_order {
            let adjusted = match change_ref {
                ChangeRef::Created(idx) => ChangeRef::Created(idx + created_offset),
                ChangeRef::Updated(idx) => ChangeRef::Updated(idx + updated_offset),
                ChangeRef::Deleted(idx) => ChangeRef::Deleted(idx + deleted_offset),
            };
            self.change_order.push(adjusted);
        }
    }
}

/// Context for applying transactions during catchup.
pub struct ApplyContext {
    /// Current ledger sequence.
    pub ledger_seq: u32,
    /// Ledger close time.
    pub close_time: u64,
    /// Protocol version.
    pub protocol_version: u32,
    /// Base fee.
    pub base_fee: u32,
    /// Base reserve.
    pub base_reserve: u32,
    /// Network ID bytes.
    pub network_id: [u8; 32],
}

impl ApplyContext {
    /// Create a new apply context.
    pub fn new(
        ledger_seq: u32,
        close_time: u64,
        protocol_version: u32,
        base_fee: u32,
        base_reserve: u32,
        network_id: [u8; 32],
    ) -> Self {
        Self {
            ledger_seq,
            close_time,
            protocol_version,
            base_fee,
            base_reserve,
            network_id,
        }
    }
}

/// Apply a transaction from history.
///
/// This is the main entry point for catchup mode. We trust the historical
/// results and just apply the state changes from the transaction meta.
pub fn apply_from_history(
    _frame: &TransactionFrame,
    result: &TransactionResult,
    meta: &TransactionMeta,
    delta: &mut LedgerDelta,
) -> Result<TxApplyResult> {
    // Add fee to delta
    delta.add_fee(result.fee_charged);

    // Apply state changes from meta
    apply_meta_changes(meta, delta)?;

    // Create result wrapper
    let wrapper = TxResultWrapper::from_xdr(result.clone());
    let success = wrapper.is_success();

    Ok(TxApplyResult {
        success,
        fee_charged: result.fee_charged,
        result: wrapper,
    })
}

/// Apply state changes from transaction meta.
fn apply_meta_changes(meta: &TransactionMeta, delta: &mut LedgerDelta) -> Result<()> {
    match meta {
        TransactionMeta::V0(changes) => {
            for op_meta in changes.iter() {
                apply_ledger_entry_changes(&op_meta.changes, delta)?;
            }
        }
        TransactionMeta::V1(v1) => {
            apply_ledger_entry_changes(&v1.tx_changes, delta)?;
            for op_meta in v1.operations.iter() {
                apply_ledger_entry_changes(&op_meta.changes, delta)?;
            }
        }
        TransactionMeta::V2(v2) => {
            apply_ledger_entry_changes(&v2.tx_changes_before, delta)?;
            for op_meta in v2.operations.iter() {
                apply_ledger_entry_changes(&op_meta.changes, delta)?;
            }
            apply_ledger_entry_changes(&v2.tx_changes_after, delta)?;
        }
        TransactionMeta::V3(v3) => {
            apply_ledger_entry_changes(&v3.tx_changes_before, delta)?;
            for op_meta in v3.operations.iter() {
                apply_ledger_entry_changes(&op_meta.changes, delta)?;
            }
            apply_ledger_entry_changes(&v3.tx_changes_after, delta)?;
        }
        TransactionMeta::V4(v4) => {
            apply_ledger_entry_changes(&v4.tx_changes_before, delta)?;
            for op_meta in v4.operations.iter() {
                apply_ledger_entry_changes(&op_meta.changes, delta)?;
            }
            apply_ledger_entry_changes(&v4.tx_changes_after, delta)?;
        }
    }

    Ok(())
}

/// Apply a set of ledger entry changes to the delta.
///
/// This is used during catchup mode where we replay historical changes.
/// For updates and deletes, we track the preceding STATE entry as the pre-state.
fn apply_ledger_entry_changes(changes: &LedgerEntryChanges, delta: &mut LedgerDelta) -> Result<()> {
    let mut pending_state: Option<LedgerEntry> = None;

    for change in changes.iter() {
        match change {
            LedgerEntryChange::Created(entry) => {
                pending_state = None;
                delta.record_create(entry.clone());
            }
            LedgerEntryChange::Updated(entry) => {
                // Use the preceding STATE as pre_state, or the entry itself as fallback
                let pre_state = pending_state.take().unwrap_or_else(|| entry.clone());
                delta.record_update(pre_state, entry.clone());
            }
            LedgerEntryChange::Removed(key) => {
                // Use the preceding STATE as pre_state, or create a placeholder if missing
                if let Some(pre_state) = pending_state.take() {
                    delta.record_delete(key.clone(), pre_state);
                }
                // If no STATE preceded this REMOVED, skip recording (shouldn't happen in valid meta)
            }
            LedgerEntryChange::State(entry) => {
                // Store STATE for the next UPDATED/REMOVED
                pending_state = Some(entry.clone());
            }
            LedgerEntryChange::Restored(entry) => {
                pending_state = None;
                delta.record_create(entry.clone());
            }
        }
    }

    Ok(())
}

/// Apply fee-only for a failed transaction.
pub fn apply_fee_only(
    frame: &TransactionFrame,
    delta: &mut LedgerDelta,
    _source_account: &AccountEntry,
) -> Result<()> {
    let fee = frame.total_fee();
    delta.add_fee(fee);
    Ok(())
}

/// Extract the ledger key from a ledger entry.
pub fn entry_to_key(entry: &LedgerEntry) -> LedgerKey {
    match &entry.data {
        LedgerEntryData::Account(a) => LedgerKey::Account(LedgerKeyAccount {
            account_id: a.account_id.clone(),
        }),
        LedgerEntryData::Trustline(t) => LedgerKey::Trustline(LedgerKeyTrustLine {
            account_id: t.account_id.clone(),
            asset: t.asset.clone(),
        }),
        LedgerEntryData::Offer(o) => LedgerKey::Offer(LedgerKeyOffer {
            seller_id: o.seller_id.clone(),
            offer_id: o.offer_id,
        }),
        LedgerEntryData::Data(d) => LedgerKey::Data(LedgerKeyData {
            account_id: d.account_id.clone(),
            data_name: d.data_name.clone(),
        }),
        LedgerEntryData::ClaimableBalance(c) => {
            LedgerKey::ClaimableBalance(LedgerKeyClaimableBalance {
                balance_id: c.balance_id.clone(),
            })
        }
        LedgerEntryData::LiquidityPool(l) => LedgerKey::LiquidityPool(LedgerKeyLiquidityPool {
            liquidity_pool_id: l.liquidity_pool_id.clone(),
        }),
        LedgerEntryData::ContractData(c) => LedgerKey::ContractData(LedgerKeyContractData {
            contract: c.contract.clone(),
            key: c.key.clone(),
            durability: c.durability,
        }),
        LedgerEntryData::ContractCode(c) => LedgerKey::ContractCode(LedgerKeyContractCode {
            hash: c.hash.clone(),
        }),
        LedgerEntryData::ConfigSetting(c) => {
            LedgerKey::ConfigSetting(stellar_xdr::curr::LedgerKeyConfigSetting {
                config_setting_id: c.discriminant(),
            })
        }
        LedgerEntryData::Ttl(t) => LedgerKey::Ttl(LedgerKeyTtl {
            key_hash: t.key_hash.clone(),
        }),
    }
}

/// Convert AccountId to lookup key
pub fn account_id_to_key(account_id: &stellar_xdr::curr::AccountId) -> [u8; 32] {
    match &account_id.0 {
        stellar_xdr::curr::PublicKey::PublicKeyTypeEd25519(key) => key.0,
    }
}

/// Asset key for lookups
#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub enum AssetKey {
    Native,
    CreditAlphanum4([u8; 4], [u8; 32]),
    CreditAlphanum12([u8; 12], [u8; 32]),
}

impl AssetKey {
    pub fn from_asset(asset: &stellar_xdr::curr::Asset) -> Self {
        match asset {
            stellar_xdr::curr::Asset::Native => AssetKey::Native,
            stellar_xdr::curr::Asset::CreditAlphanum4(a) => {
                let mut code = [0u8; 4];
                code.copy_from_slice(&a.asset_code.0);
                let issuer = account_id_to_key(&a.issuer);
                AssetKey::CreditAlphanum4(code, issuer)
            }
            stellar_xdr::curr::Asset::CreditAlphanum12(a) => {
                let mut code = [0u8; 12];
                code.copy_from_slice(&a.asset_code.0);
                let issuer = account_id_to_key(&a.issuer);
                AssetKey::CreditAlphanum12(code, issuer)
            }
        }
    }
}

/// Batch apply multiple transactions from history.
pub fn apply_transaction_set_from_history(
    transactions: &[(TransactionFrame, TransactionResult, TransactionMeta)],
    delta: &mut LedgerDelta,
) -> Result<Vec<TxApplyResult>> {
    let mut results = Vec::with_capacity(transactions.len());

    for (frame, result, meta) in transactions {
        let apply_result = apply_from_history(frame, result, meta, delta)?;
        results.push(apply_result);
    }

    Ok(results)
}

#[cfg(test)]
mod tests {
    use super::*;
    use stellar_xdr::curr::*;

    #[test]
    fn test_ledger_delta_creation() {
        let delta = LedgerDelta::new(100);
        assert_eq!(delta.ledger_seq(), 100);
        assert!(!delta.has_changes());
        assert_eq!(delta.change_count(), 0);
    }

    #[test]
    fn test_ledger_delta_changes() {
        let mut delta = LedgerDelta::new(100);

        let account_id = AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([0u8; 32])));
        let entry = LedgerEntry {
            last_modified_ledger_seq: 100,
            data: LedgerEntryData::Account(AccountEntry {
                account_id: account_id.clone(),
                balance: 1000000000,
                seq_num: SequenceNumber(1),
                num_sub_entries: 0,
                inflation_dest: None,
                flags: 0,
                home_domain: String32::default(),
                thresholds: Thresholds([1, 0, 0, 0]),
                signers: vec![].try_into().unwrap(),
                ext: AccountEntryExt::V0,
            }),
            ext: LedgerEntryExt::V0,
        };

        delta.record_create(entry.clone());
        assert!(delta.has_changes());
        assert_eq!(delta.change_count(), 1);
        assert_eq!(delta.created_entries().len(), 1);

        // record_update requires pre_state and post_state
        let mut updated_entry = entry.clone();
        if let LedgerEntryData::Account(ref mut acc) = updated_entry.data {
            acc.balance = 2000000000;
        }
        delta.record_update(entry.clone(), updated_entry.clone());
        assert_eq!(delta.change_count(), 2);
        assert_eq!(delta.updated_entries().len(), 1);

        let key = LedgerKey::Account(LedgerKeyAccount { account_id });
        // record_delete requires key and pre_state
        delta.record_delete(key, updated_entry);
        assert_eq!(delta.change_count(), 3);
        assert_eq!(delta.deleted_keys().len(), 1);
    }

    #[test]
    fn test_ledger_delta_fee() {
        let mut delta = LedgerDelta::new(100);
        assert_eq!(delta.fee_charged(), 0);

        delta.add_fee(100);
        assert_eq!(delta.fee_charged(), 100);

        delta.add_fee(50);
        assert_eq!(delta.fee_charged(), 150);
    }

    #[test]
    fn test_ledger_delta_merge() {
        let mut delta1 = LedgerDelta::new(100);
        delta1.add_fee(100);

        let mut delta2 = LedgerDelta::new(100);
        delta2.add_fee(200);

        delta1.merge(delta2);
        assert_eq!(delta1.fee_charged(), 300);
    }

    #[test]
    fn test_entry_to_key() {
        let account_id = AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([0u8; 32])));
        let entry = LedgerEntry {
            last_modified_ledger_seq: 100,
            data: LedgerEntryData::Account(AccountEntry {
                account_id: account_id.clone(),
                balance: 1000000000,
                seq_num: SequenceNumber(1),
                num_sub_entries: 0,
                inflation_dest: None,
                flags: 0,
                home_domain: String32::default(),
                thresholds: Thresholds([1, 0, 0, 0]),
                signers: vec![].try_into().unwrap(),
                ext: AccountEntryExt::V0,
            }),
            ext: LedgerEntryExt::V0,
        };

        let key = entry_to_key(&entry);
        match key {
            LedgerKey::Account(k) => {
                assert_eq!(k.account_id, account_id);
            }
            _ => panic!("Expected Account key"),
        }
    }

    /// Test LedgerDelta snapshot and truncate for savepoint support.
    #[test]
    fn test_ledger_delta_snapshot_and_truncate() {
        let mut delta = LedgerDelta::new(100);

        let account_id1 = AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([1u8; 32])));
        let entry1 = LedgerEntry {
            last_modified_ledger_seq: 100,
            data: LedgerEntryData::Account(AccountEntry {
                account_id: account_id1.clone(),
                balance: 1000000000,
                seq_num: SequenceNumber(1),
                num_sub_entries: 0,
                inflation_dest: None,
                flags: 0,
                home_domain: String32::default(),
                thresholds: Thresholds([1, 0, 0, 0]),
                signers: vec![].try_into().unwrap(),
                ext: AccountEntryExt::V0,
            }),
            ext: LedgerEntryExt::V0,
        };

        // Add first entry
        delta.record_create(entry1.clone());
        assert_eq!(delta.change_count(), 1);

        // Take snapshot
        let snapshot = delta.snapshot_lengths();

        // Add more entries
        let account_id2 = AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([2u8; 32])));
        let entry2 = LedgerEntry {
            last_modified_ledger_seq: 100,
            data: LedgerEntryData::Account(AccountEntry {
                account_id: account_id2.clone(),
                balance: 2000000000,
                seq_num: SequenceNumber(1),
                num_sub_entries: 0,
                inflation_dest: None,
                flags: 0,
                home_domain: String32::default(),
                thresholds: Thresholds([1, 0, 0, 0]),
                signers: vec![].try_into().unwrap(),
                ext: AccountEntryExt::V0,
            }),
            ext: LedgerEntryExt::V0,
        };
        delta.record_create(entry2.clone());
        assert_eq!(delta.change_count(), 2);

        // Truncate back to snapshot
        delta.truncate_to(&snapshot);
        assert_eq!(delta.change_count(), 1);
        assert_eq!(delta.created_entries().len(), 1);
    }

    /// Test LedgerDelta apply_refund_to_account modifies the correct account.
    #[test]
    fn test_ledger_delta_apply_refund() {
        let mut delta = LedgerDelta::new(100);

        let account_id = AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([3u8; 32])));
        let entry = LedgerEntry {
            last_modified_ledger_seq: 100,
            data: LedgerEntryData::Account(AccountEntry {
                account_id: account_id.clone(),
                balance: 1000000000,
                seq_num: SequenceNumber(1),
                num_sub_entries: 0,
                inflation_dest: None,
                flags: 0,
                home_domain: String32::default(),
                thresholds: Thresholds([1, 0, 0, 0]),
                signers: vec![].try_into().unwrap(),
                ext: AccountEntryExt::V0,
            }),
            ext: LedgerEntryExt::V0,
        };

        let pre_state = entry.clone();
        let mut post_state = entry.clone();
        if let LedgerEntryData::Account(ref mut acc) = post_state.data {
            acc.balance = 900000000; // Reduced by fee
        }
        delta.record_update(pre_state, post_state);

        // Apply refund
        delta.apply_refund_to_account(&account_id, 50000000);

        // Check the balance was updated
        let updated = &delta.updated_entries()[0];
        if let LedgerEntryData::Account(acc) = &updated.data {
            assert_eq!(acc.balance, 950000000); // 900000000 + 50000000 refund
        } else {
            panic!("Expected account entry");
        }
    }

    /// Test change_order preserves execution order.
    #[test]
    fn test_ledger_delta_change_order() {
        let mut delta = LedgerDelta::new(100);

        let account_id = AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([4u8; 32])));
        let entry = LedgerEntry {
            last_modified_ledger_seq: 100,
            data: LedgerEntryData::Account(AccountEntry {
                account_id: account_id.clone(),
                balance: 1000000000,
                seq_num: SequenceNumber(1),
                num_sub_entries: 0,
                inflation_dest: None,
                flags: 0,
                home_domain: String32::default(),
                thresholds: Thresholds([1, 0, 0, 0]),
                signers: vec![].try_into().unwrap(),
                ext: AccountEntryExt::V0,
            }),
            ext: LedgerEntryExt::V0,
        };

        // Create, then update, then delete
        delta.record_create(entry.clone());
        delta.record_update(entry.clone(), entry.clone());
        let key = LedgerKey::Account(LedgerKeyAccount {
            account_id: account_id.clone(),
        });
        delta.record_delete(key, entry);

        let order = delta.change_order();
        assert_eq!(order.len(), 3);
        assert!(matches!(order[0], ChangeRef::Created(0)));
        assert!(matches!(order[1], ChangeRef::Updated(0)));
        assert!(matches!(order[2], ChangeRef::Deleted(0)));
    }

    /// Test AssetKey from different asset types.
    #[test]
    fn test_asset_key_variants() {
        // Native
        let native = stellar_xdr::curr::Asset::Native;
        let key = AssetKey::from_asset(&native);
        assert!(matches!(key, AssetKey::Native));

        // CreditAlphanum4
        let issuer = AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([5u8; 32])));
        let alpha4 = stellar_xdr::curr::Asset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4(*b"USD\0"),
            issuer: issuer.clone(),
        });
        let key = AssetKey::from_asset(&alpha4);
        match key {
            AssetKey::CreditAlphanum4(code, _) => {
                assert_eq!(&code, b"USD\0");
            }
            _ => panic!("Expected CreditAlphanum4"),
        }

        // CreditAlphanum12
        let alpha12 = stellar_xdr::curr::Asset::CreditAlphanum12(AlphaNum12 {
            asset_code: AssetCode12(*b"LONGASSET123"),
            issuer: issuer.clone(),
        });
        let key = AssetKey::from_asset(&alpha12);
        match key {
            AssetKey::CreditAlphanum12(code, _) => {
                assert_eq!(&code, b"LONGASSET123");
            }
            _ => panic!("Expected CreditAlphanum12"),
        }
    }

    /// Test LedgerDelta merge preserves change order with correct offsets.
    #[test]
    fn test_ledger_delta_merge_change_order() {
        let mut delta1 = LedgerDelta::new(100);
        let mut delta2 = LedgerDelta::new(100);

        let account_id1 = AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([6u8; 32])));
        let entry1 = LedgerEntry {
            last_modified_ledger_seq: 100,
            data: LedgerEntryData::Account(AccountEntry {
                account_id: account_id1.clone(),
                balance: 1000000000,
                seq_num: SequenceNumber(1),
                num_sub_entries: 0,
                inflation_dest: None,
                flags: 0,
                home_domain: String32::default(),
                thresholds: Thresholds([1, 0, 0, 0]),
                signers: vec![].try_into().unwrap(),
                ext: AccountEntryExt::V0,
            }),
            ext: LedgerEntryExt::V0,
        };

        let account_id2 = AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([7u8; 32])));
        let entry2 = LedgerEntry {
            last_modified_ledger_seq: 100,
            data: LedgerEntryData::Account(AccountEntry {
                account_id: account_id2.clone(),
                balance: 2000000000,
                seq_num: SequenceNumber(1),
                num_sub_entries: 0,
                inflation_dest: None,
                flags: 0,
                home_domain: String32::default(),
                thresholds: Thresholds([1, 0, 0, 0]),
                signers: vec![].try_into().unwrap(),
                ext: AccountEntryExt::V0,
            }),
            ext: LedgerEntryExt::V0,
        };

        delta1.record_create(entry1.clone());
        delta2.record_create(entry2.clone());

        delta1.merge(delta2);

        assert_eq!(delta1.created_entries().len(), 2);
        let order = delta1.change_order();
        assert_eq!(order.len(), 2);
        // First entry at index 0, second at index 1 (offset applied)
        assert!(matches!(order[0], ChangeRef::Created(0)));
        assert!(matches!(order[1], ChangeRef::Created(1)));
    }

    // === Additional LedgerDelta tests ===

    #[test]
    fn test_change_ref_debug() {
        let ref1 = ChangeRef::Created(0);
        let ref2 = ChangeRef::Updated(5);
        let ref3 = ChangeRef::Deleted(10);

        let debug1 = format!("{:?}", ref1);
        let debug2 = format!("{:?}", ref2);
        let debug3 = format!("{:?}", ref3);

        assert!(debug1.contains("Created"));
        assert!(debug2.contains("Updated"));
        assert!(debug3.contains("Deleted"));
    }

    #[test]
    fn test_delta_lengths_struct() {
        let lengths = DeltaLengths {
            created: 5,
            updated: 3,
            deleted: 2,
            change_order: 10,
        };

        assert_eq!(lengths.created, 5);
        assert_eq!(lengths.updated, 3);
        assert_eq!(lengths.deleted, 2);
        assert_eq!(lengths.change_order, 10);
    }

    #[test]
    fn test_ledger_delta_update_states() {
        let mut delta = LedgerDelta::new(100);

        let account_id = AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([8u8; 32])));
        let entry = LedgerEntry {
            last_modified_ledger_seq: 100,
            data: LedgerEntryData::Account(AccountEntry {
                account_id: account_id.clone(),
                balance: 1000000000,
                seq_num: SequenceNumber(1),
                num_sub_entries: 0,
                inflation_dest: None,
                flags: 0,
                home_domain: String32::default(),
                thresholds: Thresholds([1, 0, 0, 0]),
                signers: vec![].try_into().unwrap(),
                ext: AccountEntryExt::V0,
            }),
            ext: LedgerEntryExt::V0,
        };

        let pre_state = entry.clone();
        let mut post_state = entry.clone();
        if let LedgerEntryData::Account(ref mut acc) = post_state.data {
            acc.balance = 500000000;
        }

        delta.record_update(pre_state, post_state);

        assert_eq!(delta.update_states().len(), 1);
        assert_eq!(delta.updated_entries().len(), 1);

        // Verify pre_state is preserved
        if let LedgerEntryData::Account(acc) = &delta.update_states()[0].data {
            assert_eq!(acc.balance, 1000000000);
        }
        // Verify post_state is correct
        if let LedgerEntryData::Account(acc) = &delta.updated_entries()[0].data {
            assert_eq!(acc.balance, 500000000);
        }
    }

    #[test]
    fn test_ledger_delta_delete_states() {
        let mut delta = LedgerDelta::new(100);

        let account_id = AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([9u8; 32])));
        let entry = LedgerEntry {
            last_modified_ledger_seq: 100,
            data: LedgerEntryData::Account(AccountEntry {
                account_id: account_id.clone(),
                balance: 2000000000,
                seq_num: SequenceNumber(5),
                num_sub_entries: 0,
                inflation_dest: None,
                flags: 0,
                home_domain: String32::default(),
                thresholds: Thresholds([1, 0, 0, 0]),
                signers: vec![].try_into().unwrap(),
                ext: AccountEntryExt::V0,
            }),
            ext: LedgerEntryExt::V0,
        };

        let key = LedgerKey::Account(LedgerKeyAccount {
            account_id: account_id.clone(),
        });

        delta.record_delete(key, entry);

        assert_eq!(delta.deleted_keys().len(), 1);
        assert_eq!(delta.delete_states().len(), 1);

        // Verify pre_state is preserved
        if let LedgerEntryData::Account(acc) = &delta.delete_states()[0].data {
            assert_eq!(acc.balance, 2000000000);
            assert_eq!(acc.seq_num.0, 5);
        }
    }

    #[test]
    fn test_ledger_delta_has_changes_false() {
        let delta = LedgerDelta::new(200);
        assert!(!delta.has_changes());
    }

    #[test]
    fn test_ledger_delta_has_changes_after_create() {
        let mut delta = LedgerDelta::new(200);

        let account_id = AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([10u8; 32])));
        let entry = LedgerEntry {
            last_modified_ledger_seq: 200,
            data: LedgerEntryData::Account(AccountEntry {
                account_id: account_id.clone(),
                balance: 1000000000,
                seq_num: SequenceNumber(1),
                num_sub_entries: 0,
                inflation_dest: None,
                flags: 0,
                home_domain: String32::default(),
                thresholds: Thresholds([1, 0, 0, 0]),
                signers: vec![].try_into().unwrap(),
                ext: AccountEntryExt::V0,
            }),
            ext: LedgerEntryExt::V0,
        };

        delta.record_create(entry);
        assert!(delta.has_changes());
    }

    #[test]
    fn test_ledger_delta_clone() {
        let mut delta = LedgerDelta::new(300);
        delta.add_fee(500);

        let cloned = delta.clone();

        assert_eq!(cloned.ledger_seq(), 300);
        assert_eq!(cloned.fee_charged(), 500);
    }

    #[test]
    fn test_entry_to_key_trustline() {
        let account_id = AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([11u8; 32])));
        let issuer = AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([12u8; 32])));
        let asset = TrustLineAsset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4(*b"USD\0"),
            issuer,
        });

        let entry = LedgerEntry {
            last_modified_ledger_seq: 100,
            data: LedgerEntryData::Trustline(TrustLineEntry {
                account_id: account_id.clone(),
                asset: asset.clone(),
                balance: 5000000,
                limit: 10000000,
                flags: 0,
                ext: TrustLineEntryExt::V0,
            }),
            ext: LedgerEntryExt::V0,
        };

        let key = entry_to_key(&entry);
        match key {
            LedgerKey::Trustline(k) => {
                assert_eq!(k.account_id, account_id);
            }
            _ => panic!("Expected Trustline key"),
        }
    }

    #[test]
    fn test_entry_to_key_offer() {
        let seller = AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([13u8; 32])));
        let issuer = AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([14u8; 32])));

        let entry = LedgerEntry {
            last_modified_ledger_seq: 100,
            data: LedgerEntryData::Offer(OfferEntry {
                seller_id: seller.clone(),
                offer_id: 12345,
                selling: Asset::Native,
                buying: Asset::CreditAlphanum4(AlphaNum4 {
                    asset_code: AssetCode4(*b"USD\0"),
                    issuer,
                }),
                amount: 1000,
                price: Price { n: 1, d: 1 },
                flags: 0,
                ext: OfferEntryExt::V0,
            }),
            ext: LedgerEntryExt::V0,
        };

        let key = entry_to_key(&entry);
        match key {
            LedgerKey::Offer(k) => {
                assert_eq!(k.seller_id, seller);
                assert_eq!(k.offer_id, 12345);
            }
            _ => panic!("Expected Offer key"),
        }
    }

    #[test]
    fn test_entry_to_key_data() {
        let account_id = AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([15u8; 32])));

        let entry = LedgerEntry {
            last_modified_ledger_seq: 100,
            data: LedgerEntryData::Data(DataEntry {
                account_id: account_id.clone(),
                data_name: String64::try_from(b"mykey".to_vec()).unwrap(),
                data_value: DataValue(vec![1, 2, 3, 4].try_into().unwrap()),
                ext: DataEntryExt::V0,
            }),
            ext: LedgerEntryExt::V0,
        };

        let key = entry_to_key(&entry);
        match key {
            LedgerKey::Data(k) => {
                assert_eq!(k.account_id, account_id);
            }
            _ => panic!("Expected Data key"),
        }
    }
}
