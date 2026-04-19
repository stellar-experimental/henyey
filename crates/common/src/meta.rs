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
//! 1. Ledger entry key (`LedgerKey::cmp`, matching stellar-core's xdrpp ordering)
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
use stellar_xdr::curr::{LedgerEntryChange, LedgerEntryChanges, LedgerKey, TransactionMeta};

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
/// Changes are sorted by (key, change_type, change_hash) to ensure
/// deterministic ordering regardless of the original order. Uses
/// `LedgerKey::cmp` (derived `Ord`) which matches stellar-core's xdrpp
/// `operator<` ordering — see `crates/bucket/src/entry.rs:143-151`.
fn sort_changes(changes: &mut LedgerEntryChanges) -> Result<(), stellar_xdr::curr::Error> {
    let mut entries: Vec<(LedgerKey, u8, [u8; 32], LedgerEntryChange)> =
        Vec::with_capacity(changes.0.len());

    for change in changes.0.iter().cloned() {
        let key = change_key(&change);
        let change_hash = Hash256::hash_xdr(&change).0;
        let order = change_type_order(&change);
        entries.push((key, order, change_hash, change));
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
) -> Result<(), stellar_xdr::curr::Error> {
    for op in ops.iter_mut() {
        sort_changes(changes(op))?;
    }
    Ok(())
}

/// Normalizes the before/after changes and per-operation changes shared by V2, V3, and V4
/// transaction metadata formats.
fn normalize_v2_style<T>(
    tx_changes_before: &mut LedgerEntryChanges,
    tx_changes_after: &mut LedgerEntryChanges,
    operations: &mut stellar_xdr::curr::VecM<T>,
    changes: impl FnMut(&mut T) -> &mut LedgerEntryChanges,
) -> Result<(), stellar_xdr::curr::Error> {
    sort_changes(tx_changes_before)?;
    sort_changes(tx_changes_after)?;
    normalize_ops(operations, changes)?;
    Ok(())
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
            normalize_ops(ops, |op| &mut op.changes)?;
        }
        TransactionMeta::V1(v1) => {
            sort_changes(&mut v1.tx_changes)?;
            normalize_ops(&mut v1.operations, |op| &mut op.changes)?;
        }
        TransactionMeta::V2(v2) => normalize_v2_style(
            &mut v2.tx_changes_before,
            &mut v2.tx_changes_after,
            &mut v2.operations,
            |op| &mut op.changes,
        )?,
        TransactionMeta::V3(v3) => normalize_v2_style(
            &mut v3.tx_changes_before,
            &mut v3.tx_changes_after,
            &mut v3.operations,
            |op| &mut op.changes,
        )?,
        TransactionMeta::V4(v4) => normalize_v2_style(
            &mut v4.tx_changes_before,
            &mut v4.tx_changes_after,
            &mut v4.operations,
            |op| &mut op.changes,
        )?,
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use stellar_xdr::curr::*;

    /// Verify that sort_changes uses LedgerKey::cmp (struct ordering), not XDR
    /// byte ordering. For ContractData keys with variable-length ScVal fields,
    /// these orderings can differ because XDR length-prefixes variable fields
    /// (making byte comparison length-first) while LedgerKey::cmp compares
    /// element-by-element.
    #[test]
    fn test_sort_changes_uses_struct_ordering() {
        let contract = ScAddress::Contract(ContractId(Hash([0xAA; 32])));

        // Key A: Bytes(short) — XDR: length=2, then 2 bytes
        let key_a = LedgerKey::ContractData(LedgerKeyContractData {
            contract: contract.clone(),
            key: ScVal::Bytes(ScBytes(vec![0xFF, 0xFF].try_into().unwrap())),
            durability: ContractDataDurability::Persistent,
        });

        // Key B: Bytes(longer) — XDR: length=3, then 3 bytes (all 0x00)
        let key_b = LedgerKey::ContractData(LedgerKeyContractData {
            contract: contract.clone(),
            key: ScVal::Bytes(ScBytes(vec![0x00, 0x00, 0x00].try_into().unwrap())),
            durability: ContractDataDurability::Persistent,
        });

        // Struct ordering: Bytes([0x00,0x00,0x00]) < Bytes([0xFF,0xFF])
        // (element-by-element: 0x00 < 0xFF)
        assert!(key_b < key_a, "struct ordering: key_b < key_a");

        // XDR byte ordering: length 2 < length 3, so key_a < key_b
        let a_bytes = key_a.to_xdr(Limits::none()).unwrap();
        let b_bytes = key_b.to_xdr(Limits::none()).unwrap();
        assert!(
            a_bytes < b_bytes,
            "byte ordering: key_a_bytes < key_b_bytes"
        );

        // The two orderings disagree — verify sort_changes uses struct ordering
        let entry_a = LedgerEntry {
            last_modified_ledger_seq: 1,
            data: LedgerEntryData::ContractData(ContractDataEntry {
                ext: ExtensionPoint::V0,
                contract: contract.clone(),
                key: ScVal::Bytes(ScBytes(vec![0xFF, 0xFF].try_into().unwrap())),
                durability: ContractDataDurability::Persistent,
                val: ScVal::U32(1),
            }),
            ext: LedgerEntryExt::V0,
        };
        let entry_b = LedgerEntry {
            last_modified_ledger_seq: 1,
            data: LedgerEntryData::ContractData(ContractDataEntry {
                ext: ExtensionPoint::V0,
                contract: contract.clone(),
                key: ScVal::Bytes(ScBytes(vec![0x00, 0x00, 0x00].try_into().unwrap())),
                durability: ContractDataDurability::Persistent,
                val: ScVal::U32(2),
            }),
            ext: LedgerEntryExt::V0,
        };

        // Put them in byte order (key_a first) — struct ordering should reverse
        let mut changes = LedgerEntryChanges(
            vec![
                LedgerEntryChange::Updated(entry_a.clone()),
                LedgerEntryChange::Updated(entry_b.clone()),
            ]
            .try_into()
            .unwrap(),
        );

        sort_changes(&mut changes).unwrap();

        // After sort, key_b (Bytes([0x00,...])) should come first (struct order)
        let first_key = change_key(&changes.0[0]);
        assert_eq!(
            first_key, key_b,
            "sort should use struct ordering: key_b first"
        );
    }

    #[test]
    fn test_sort_changes_by_type_order() {
        let key = LedgerKey::Account(LedgerKeyAccount {
            account_id: AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([1; 32]))),
        });
        let entry = LedgerEntry {
            last_modified_ledger_seq: 1,
            data: LedgerEntryData::Account(AccountEntry {
                account_id: AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([1; 32]))),
                balance: 100,
                seq_num: SequenceNumber(1),
                num_sub_entries: 0,
                inflation_dest: None,
                flags: 0,
                home_domain: Default::default(),
                thresholds: Thresholds([1, 0, 0, 0]),
                signers: Default::default(),
                ext: AccountEntryExt::V0,
            }),
            ext: LedgerEntryExt::V0,
        };

        // Sorted order: State, Created, Updated, Removed
        let mut changes = LedgerEntryChanges(
            vec![
                LedgerEntryChange::Updated(entry.clone()),
                LedgerEntryChange::State(entry.clone()),
                LedgerEntryChange::Removed(key.clone()),
                LedgerEntryChange::Created(entry.clone()),
            ]
            .try_into()
            .unwrap(),
        );

        sort_changes(&mut changes).unwrap();

        assert!(matches!(changes.0[0], LedgerEntryChange::State(_)));
        assert!(matches!(changes.0[1], LedgerEntryChange::Created(_)));
        assert!(matches!(changes.0[2], LedgerEntryChange::Updated(_)));
        assert!(matches!(changes.0[3], LedgerEntryChange::Removed(_)));
    }
}
