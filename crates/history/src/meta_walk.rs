//! Shared traversal helper for `TransactionMeta` V0–V4.
//!
//! This module provides a single function that encapsulates the
//! V0–V4 `TransactionMeta` variant traversal pattern, invoking a
//! caller-supplied closure on each `LedgerEntryChange` in field
//! traversal order.

use stellar_xdr::curr::{LedgerEntryChange, TransactionMeta};

/// Walk all `LedgerEntryChange` entries in a slice of `TransactionMeta`,
/// invoking `f` for each change in field traversal order.
///
/// Traversal order preserves the current henyey/XDR field traversal order:
/// - V0: `operations[i].changes` (sequentially per operation)
/// - V1: `tx_changes`, then `operations[i].changes`
/// - V2/V3/V4: `tx_changes_before`, `operations[i].changes`, `tx_changes_after`
///
/// Multiple metas in the slice are processed sequentially in slice order.
pub(crate) fn for_each_change<F>(tx_metas: &[TransactionMeta], mut f: F)
where
    F: FnMut(&LedgerEntryChange),
{
    for meta in tx_metas {
        match meta {
            TransactionMeta::V0(operations) => {
                for op_meta in operations.iter() {
                    for change in op_meta.changes.iter() {
                        f(change);
                    }
                }
            }
            TransactionMeta::V1(v1) => {
                for change in v1.tx_changes.iter() {
                    f(change);
                }
                for op_changes in v1.operations.iter() {
                    for change in op_changes.changes.iter() {
                        f(change);
                    }
                }
            }
            TransactionMeta::V2(v2) => {
                for change in v2.tx_changes_before.iter() {
                    f(change);
                }
                for op in v2.operations.iter() {
                    for change in op.changes.iter() {
                        f(change);
                    }
                }
                for change in v2.tx_changes_after.iter() {
                    f(change);
                }
            }
            TransactionMeta::V3(v3) => {
                for change in v3.tx_changes_before.iter() {
                    f(change);
                }
                for op in v3.operations.iter() {
                    for change in op.changes.iter() {
                        f(change);
                    }
                }
                for change in v3.tx_changes_after.iter() {
                    f(change);
                }
            }
            TransactionMeta::V4(v4) => {
                for change in v4.tx_changes_before.iter() {
                    f(change);
                }
                for op in v4.operations.iter() {
                    for change in op.changes.iter() {
                        f(change);
                    }
                }
                for change in v4.tx_changes_after.iter() {
                    f(change);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use stellar_xdr::curr::{
        AccountEntry, AccountEntryExt, AccountId, ExtensionPoint, LedgerEntry, LedgerEntryChanges,
        LedgerEntryData, LedgerEntryExt, OperationMeta, OperationMetaV2, PublicKey, SequenceNumber,
        String32, Thresholds, TransactionMetaV1, TransactionMetaV2, TransactionMetaV3,
        TransactionMetaV4, Uint256,
    };

    /// Create a minimal `LedgerEntry` distinguishable by `id_byte`.
    fn make_entry(id_byte: u8) -> LedgerEntry {
        LedgerEntry {
            last_modified_ledger_seq: 0,
            data: LedgerEntryData::Account(AccountEntry {
                account_id: AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([id_byte; 32]))),
                balance: 0,
                seq_num: SequenceNumber(0),
                num_sub_entries: 0,
                inflation_dest: None,
                flags: 0,
                home_domain: String32::default(),
                thresholds: Thresholds([0; 4]),
                signers: vec![].try_into().unwrap(),
                ext: AccountEntryExt::V0,
            }),
            ext: LedgerEntryExt::V0,
        }
    }

    /// Create a `State` change as a lightweight traversal marker.
    fn marker(id: u8) -> LedgerEntryChange {
        LedgerEntryChange::State(make_entry(id))
    }

    fn changes(entries: Vec<LedgerEntryChange>) -> LedgerEntryChanges {
        entries.try_into().unwrap()
    }

    /// Extract the id_byte from a State marker for easy assertion.
    fn marker_id(change: &LedgerEntryChange) -> u8 {
        match change {
            LedgerEntryChange::State(entry) => match &entry.data {
                LedgerEntryData::Account(acc) => match &acc.account_id.0 {
                    PublicKey::PublicKeyTypeEd25519(Uint256(bytes)) => bytes[0],
                },
                _ => panic!("expected Account entry in marker"),
            },
            _ => panic!("expected State marker"),
        }
    }

    /// Collect all change marker IDs from `for_each_change`.
    fn collect_ids(metas: &[TransactionMeta]) -> Vec<u8> {
        let mut ids = Vec::new();
        for_each_change(metas, |change| ids.push(marker_id(change)));
        ids
    }

    fn make_v0_meta(ops: Vec<Vec<LedgerEntryChange>>) -> TransactionMeta {
        let op_metas: Vec<OperationMeta> = ops
            .into_iter()
            .map(|op_changes| OperationMeta {
                changes: changes(op_changes),
            })
            .collect();
        TransactionMeta::V0(op_metas.try_into().unwrap())
    }

    fn make_v1_meta(
        tx_changes: Vec<LedgerEntryChange>,
        ops: Vec<Vec<LedgerEntryChange>>,
    ) -> TransactionMeta {
        let op_metas: Vec<OperationMeta> = ops
            .into_iter()
            .map(|op_changes| OperationMeta {
                changes: changes(op_changes),
            })
            .collect();
        TransactionMeta::V1(TransactionMetaV1 {
            tx_changes: changes(tx_changes),
            operations: op_metas.try_into().unwrap(),
        })
    }

    fn make_v2_meta(
        before: Vec<LedgerEntryChange>,
        ops: Vec<Vec<LedgerEntryChange>>,
        after: Vec<LedgerEntryChange>,
    ) -> TransactionMeta {
        let op_metas: Vec<OperationMeta> = ops
            .into_iter()
            .map(|op_changes| OperationMeta {
                changes: changes(op_changes),
            })
            .collect();
        TransactionMeta::V2(TransactionMetaV2 {
            tx_changes_before: changes(before),
            operations: op_metas.try_into().unwrap(),
            tx_changes_after: changes(after),
        })
    }

    fn make_v3_meta(
        before: Vec<LedgerEntryChange>,
        ops: Vec<Vec<LedgerEntryChange>>,
        after: Vec<LedgerEntryChange>,
    ) -> TransactionMeta {
        let op_metas: Vec<OperationMeta> = ops
            .into_iter()
            .map(|op_changes| OperationMeta {
                changes: changes(op_changes),
            })
            .collect();
        TransactionMeta::V3(TransactionMetaV3 {
            ext: ExtensionPoint::V0,
            tx_changes_before: changes(before),
            operations: op_metas.try_into().unwrap(),
            tx_changes_after: changes(after),
            soroban_meta: None,
        })
    }

    fn make_v4_meta(
        before: Vec<LedgerEntryChange>,
        ops: Vec<Vec<LedgerEntryChange>>,
        after: Vec<LedgerEntryChange>,
    ) -> TransactionMeta {
        let op_metas: Vec<OperationMetaV2> = ops
            .into_iter()
            .map(|op_changes| OperationMetaV2 {
                ext: ExtensionPoint::V0,
                changes: changes(op_changes),
                events: vec![].try_into().unwrap(),
            })
            .collect();
        TransactionMeta::V4(TransactionMetaV4 {
            ext: ExtensionPoint::V0,
            tx_changes_before: changes(before),
            operations: op_metas.try_into().unwrap(),
            tx_changes_after: changes(after),
            soroban_meta: None,
            events: vec![].try_into().unwrap(),
            diagnostic_events: vec![].try_into().unwrap(),
        })
    }

    #[test]
    fn test_for_each_change_empty_slice() {
        let ids = collect_ids(&[]);
        assert!(ids.is_empty());
    }

    #[test]
    fn test_for_each_change_v0_order() {
        // V0: operations[0].changes then operations[1].changes
        let meta = make_v0_meta(vec![vec![marker(1), marker(2)], vec![marker(3)]]);
        assert_eq!(collect_ids(&[meta]), vec![1, 2, 3]);
    }

    #[test]
    fn test_for_each_change_v1_order() {
        // V1: tx_changes first, then operations
        let meta = make_v1_meta(
            vec![marker(10), marker(11)],
            vec![vec![marker(20)], vec![marker(21), marker(22)]],
        );
        assert_eq!(collect_ids(&[meta]), vec![10, 11, 20, 21, 22]);
    }

    #[test]
    fn test_for_each_change_v2_order() {
        // V2: before → ops → after
        let meta = make_v2_meta(
            vec![marker(1)],
            vec![vec![marker(2), marker(3)]],
            vec![marker(4)],
        );
        assert_eq!(collect_ids(&[meta]), vec![1, 2, 3, 4]);
    }

    #[test]
    fn test_for_each_change_v3_order() {
        // V3: before → ops → after
        let meta = make_v3_meta(
            vec![marker(5)],
            vec![vec![marker(6)], vec![marker(7)]],
            vec![marker(8), marker(9)],
        );
        assert_eq!(collect_ids(&[meta]), vec![5, 6, 7, 8, 9]);
    }

    #[test]
    fn test_for_each_change_v4_order() {
        // V4: before → ops → after
        let meta = make_v4_meta(
            vec![marker(10), marker(11)],
            vec![vec![marker(12)]],
            vec![marker(13)],
        );
        assert_eq!(collect_ids(&[meta]), vec![10, 11, 12, 13]);
    }

    #[test]
    fn test_for_each_change_multiple_metas() {
        // Multiple metas processed sequentially in slice order
        let m1 = make_v0_meta(vec![vec![marker(1)]]);
        let m2 = make_v2_meta(vec![marker(2)], vec![vec![marker(3)]], vec![marker(4)]);
        let m3 = make_v4_meta(vec![marker(5)], vec![], vec![marker(6)]);
        assert_eq!(collect_ids(&[m1, m2, m3]), vec![1, 2, 3, 4, 5, 6]);
    }

    #[test]
    fn test_for_each_change_empty_containers() {
        let empty: Vec<u8> = vec![];

        // V0 with no operations
        let m0 = make_v0_meta(vec![]);
        assert_eq!(collect_ids(&[m0]), empty);

        // V1 with empty tx_changes and empty ops
        let m1 = make_v1_meta(vec![], vec![]);
        assert_eq!(collect_ids(&[m1]), empty);

        // V2 with empty before/after but operations present
        let m2 = make_v2_meta(vec![], vec![vec![marker(1)]], vec![]);
        assert_eq!(collect_ids(&[m2]), vec![1]);

        // V3 with all empty
        let m3 = make_v3_meta(vec![], vec![], vec![]);
        assert_eq!(collect_ids(&[m3]), empty);

        // V4 with empty operations but before/after present
        let m4 = make_v4_meta(vec![marker(2)], vec![], vec![marker(3)]);
        assert_eq!(collect_ids(&[m4]), vec![2, 3]);
    }
}
