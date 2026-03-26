//! Ledger metadata normalization utilities.
//!
//! This module provides functions for normalizing ledger close metadata to
//! enable deterministic hashing. Different validators may produce transaction
//! metadata with ledger entry changes in different orders, but the final
//! ledger state should be identical. Normalizing the metadata allows for
//! consistent hashing across nodes.
//!
//! # Why Normalization?
//!
//! When a ledger closes, the transaction metadata includes all ledger entry
//! changes (creates, updates, deletes). The order of these changes may vary
//! between validators due to implementation differences in map iteration order
//! or parallel execution. Normalization sorts these changes deterministically
//! so that the metadata hash is consistent.
//!
//! # Sorting Order
//!
//! Changes are sorted by:
//! 1. Ledger entry key (XDR-encoded bytes)
//! 2. Change type (State, Created, Updated, Removed, Restored)
//! 3. Full change hash (for stability when keys and types are equal)
//!
//! # Example
//!
//! ```rust,ignore
//! use henyey_common::meta::normalize_transaction_meta;
//!
//! let mut meta = fetch_transaction_meta();
//! normalize_transaction_meta(&mut meta).unwrap();
//! // meta is now in canonical sorted order
//! ```

use crate::types::entry_to_key;
use crate::Hash256;
use stellar_xdr::curr::{
    LedgerEntryChange, LedgerEntryChanges, LedgerKey, Limits, TransactionMeta, WriteXdr,
};

/// Extracts the ledger key from a ledger entry change.
fn change_key(change: &LedgerEntryChange) -> LedgerKey {
    match change {
        LedgerEntryChange::State(entry)
        | LedgerEntryChange::Created(entry)
        | LedgerEntryChange::Updated(entry)
        | LedgerEntryChange::Restored(entry) => entry_to_key(entry),
        LedgerEntryChange::Removed(key) => key.clone(),
    }
}

/// Returns a numeric order value for change types to ensure consistent sorting.
///
/// Order: State(0) < Created(1) < Updated(2) < Removed(3) < Restored(4)
fn change_type_order(change: &LedgerEntryChange) -> u8 {
    match change {
        LedgerEntryChange::State(_) => 0,
        LedgerEntryChange::Created(_) => 1,
        LedgerEntryChange::Updated(_) => 2,
        LedgerEntryChange::Removed(_) => 3,
        LedgerEntryChange::Restored(_) => 4,
    }
}

/// Sorts a list of ledger entry changes into canonical order.
///
/// Changes are sorted by (key_bytes, change_type, change_hash) to ensure
/// deterministic ordering regardless of the original order.
fn sort_changes(changes: &mut LedgerEntryChanges) -> Result<(), stellar_xdr::curr::Error> {
    let mut entries: Vec<(Vec<u8>, u8, [u8; 32], LedgerEntryChange)> =
        Vec::with_capacity(changes.0.len());

    for change in changes.0.iter().cloned() {
        let key = change_key(&change);
        let key_bytes = key.to_xdr(Limits::none())?;
        let change_hash = Hash256::hash_xdr(&change)?.0;
        let order = change_type_order(&change);
        entries.push((key_bytes, order, change_hash, change));
    }

    entries.sort_by(|a, b| {
        a.0.cmp(&b.0)
            .then_with(|| a.1.cmp(&b.1))
            .then_with(|| a.2.cmp(&b.2))
    });

    let sorted: Vec<LedgerEntryChange> = entries.into_iter().map(|(_, _, _, c)| c).collect();
    changes.0 = sorted
        .try_into()
        .map_err(|_| stellar_xdr::curr::Error::Invalid)?;
    Ok(())
}

fn normalize_ops<T>(
    ops: &mut stellar_xdr::curr::VecM<T>,
    mut changes: impl FnMut(&mut T) -> &mut LedgerEntryChanges,
) {
    for op in ops.iter_mut() {
        let _ = sort_changes(changes(op));
    }
}

/// Normalizes transaction metadata for deterministic hashing.
///
/// This sorts all ledger entry changes within the transaction metadata
/// into canonical order. This ensures that the same transaction will
/// produce the same metadata hash regardless of execution order.
///
/// # Errors
///
/// Returns an error if XDR serialization fails during sorting.
pub fn normalize_transaction_meta(
    meta: &mut TransactionMeta,
) -> Result<(), stellar_xdr::curr::Error> {
    match meta {
        TransactionMeta::V0(ops) => {
            normalize_ops(ops, |op| &mut op.changes);
        }
        TransactionMeta::V1(v1) => {
            sort_changes(&mut v1.tx_changes)?;
            normalize_ops(&mut v1.operations, |op| &mut op.changes);
        }
        TransactionMeta::V2(v2) => {
            sort_changes(&mut v2.tx_changes_before)?;
            sort_changes(&mut v2.tx_changes_after)?;
            normalize_ops(&mut v2.operations, |op| &mut op.changes);
        }
        TransactionMeta::V3(v3) => {
            sort_changes(&mut v3.tx_changes_before)?;
            sort_changes(&mut v3.tx_changes_after)?;
            normalize_ops(&mut v3.operations, |op| &mut op.changes);
        }
        TransactionMeta::V4(v4) => {
            sort_changes(&mut v4.tx_changes_before)?;
            sort_changes(&mut v4.tx_changes_after)?;
            normalize_ops(&mut v4.operations, |op| &mut op.changes);
        }
    }
    Ok(())
}
