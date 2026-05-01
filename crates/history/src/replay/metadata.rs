//! Metadata-based ledger replay.
//!
//! Applies `TransactionMeta` directly from archives to reconstruct ledger
//! entry changes. This produces identical results to the original execution
//! and is used for testing and specialized replay scenarios.

use crate::{verify, HistoryError, Result};
use henyey_ledger::TransactionSetVariant;
use stellar_xdr::curr::{
    LedgerEntry, LedgerHeader, LedgerKey, TransactionMeta, TransactionResultPair,
    TransactionResultSet, WriteXdr,
};

use super::execution::soroban_entry_size;
use super::{LedgerReplayResult, ReplayConfig};

/// Replays a single ledger from history data.
///
/// This applies the transaction results to extract ledger entry changes,
/// which can then be applied to the bucket list.
///
/// # Arguments
///
/// * `header` - The ledger header
/// * `tx_set` - The transaction set for this ledger
/// * `tx_results` - The transaction results from history
/// * `tx_metas` - Transaction metadata containing ledger entry changes
/// * `config` - Replay configuration
///
/// # Returns
///
/// A `LedgerReplayResult` containing the changes to apply to ledger state.
pub(crate) fn replay_ledger(
    header: &LedgerHeader,
    tx_set: &TransactionSetVariant,
    tx_results: &[TransactionResultPair],
    tx_metas: &[TransactionMeta],
    config: &ReplayConfig,
) -> Result<LedgerReplayResult> {
    // Verify the transaction set hash matches the header
    if config.verify_results {
        verify::verify_tx_set(header, tx_set)?;

        let result_set = TransactionResultSet {
            results: tx_results
                .to_vec()
                .try_into()
                .map_err(|_| HistoryError::CatchupFailed("tx result set too large".to_string()))?,
        };
        let xdr = result_set
            .to_xdr(stellar_xdr::curr::Limits::none())
            .map_err(|e| {
                HistoryError::CatchupFailed(format!("failed to encode tx result set: {}", e))
            })?;
        verify::verify_tx_result_set(header, &xdr)?;
    }

    // Extract ledger entry changes from transaction metadata
    let (init_entries, live_entries, dead_entries) = extract_ledger_changes(tx_metas)?;

    // Count transactions and operations
    let tx_count = tx_set.num_transactions() as u32;
    let op_count = count_operations(tx_set);

    // Compute the ledger hash
    let ledger_hash = verify::compute_header_hash(header)?;

    // Compute soroban state size delta for metadata-based replay.
    // NOTE: This is an approximation since we don't have pre-update/pre-delete states.
    // We add size for INIT entries, but can't accurately track LIVE (updates) or DEAD (deletes).
    // For accurate tracking, use execution-based replay.
    let mut soroban_state_size_delta: i64 = 0;
    for entry in &init_entries {
        soroban_state_size_delta += soroban_entry_size(entry, header.ledger_version, None);
    }

    Ok(LedgerReplayResult {
        sequence: header.ledger_seq,
        protocol_version: header.ledger_version,
        ledger_hash,
        tx_count,
        op_count,
        fee_pool_delta: 0,
        total_coins_delta: 0,
        init_entries,
        live_entries,
        dead_entries,
        changes: Vec::new(),
        eviction_iterator: None, // Metadata-based replay doesn't track eviction
        soroban_state_size_delta,
    })
}

/// Extract ledger entry changes from transaction metadata.
///
/// Returns (init_entries, live_entries, dead_entries) where:
/// - init_entries: Entries that were created
/// - live_entries: Entries that were updated or restored
/// - dead_entries: Keys of entries that were deleted
/// Accumulated ledger entry changes from transaction meta.
struct LedgerChanges {
    init: Vec<LedgerEntry>,
    live: Vec<LedgerEntry>,
    dead: Vec<LedgerKey>,
}

impl LedgerChanges {
    fn new() -> Self {
        Self {
            init: Vec::new(),
            live: Vec::new(),
            dead: Vec::new(),
        }
    }

    fn push(&mut self, change: &stellar_xdr::curr::LedgerEntryChange) {
        use stellar_xdr::curr::LedgerEntryChange;
        match change {
            LedgerEntryChange::Created(entry) => self.init.push(entry.clone()),
            LedgerEntryChange::Updated(entry) => self.live.push(entry.clone()),
            LedgerEntryChange::Removed(key) => self.dead.push(key.clone()),
            LedgerEntryChange::State(_) => {}
            LedgerEntryChange::Restored(entry) => self.live.push(entry.clone()),
        }
    }

    fn push_all(&mut self, changes: &stellar_xdr::curr::LedgerEntryChanges) {
        for change in changes.iter() {
            self.push(change);
        }
    }

    fn into_tuple(self) -> (Vec<LedgerEntry>, Vec<LedgerEntry>, Vec<LedgerKey>) {
        (self.init, self.live, self.dead)
    }
}

pub(crate) fn extract_ledger_changes(
    tx_metas: &[TransactionMeta],
) -> Result<(Vec<LedgerEntry>, Vec<LedgerEntry>, Vec<LedgerKey>)> {
    let mut changes = LedgerChanges::new();

    for meta in tx_metas {
        match meta {
            TransactionMeta::V0(operations) => {
                for op_meta in operations.iter() {
                    changes.push_all(&op_meta.changes);
                }
            }
            TransactionMeta::V1(v1) => {
                changes.push_all(&v1.tx_changes);
                for op_changes in v1.operations.iter() {
                    changes.push_all(&op_changes.changes);
                }
            }
            TransactionMeta::V2(v2) => {
                changes.push_all(&v2.tx_changes_before);
                for op in v2.operations.iter() {
                    changes.push_all(&op.changes);
                }
                changes.push_all(&v2.tx_changes_after);
            }
            TransactionMeta::V3(v3) => {
                changes.push_all(&v3.tx_changes_before);
                for op in v3.operations.iter() {
                    changes.push_all(&op.changes);
                }
                changes.push_all(&v3.tx_changes_after);
            }
            TransactionMeta::V4(v4) => {
                changes.push_all(&v4.tx_changes_before);
                for op in v4.operations.iter() {
                    changes.push_all(&op.changes);
                }
                changes.push_all(&v4.tx_changes_after);
            }
        }
    }

    Ok(changes.into_tuple())
}

/// Count the total number of operations in a transaction set.
fn count_operations(tx_set: &TransactionSetVariant) -> u32 {
    tx_set
        .transactions()
        .into_iter()
        .map(|tx_env| {
            use stellar_xdr::curr::TransactionEnvelope;
            match tx_env {
                TransactionEnvelope::TxV0(tx) => tx.tx.operations.len() as u32,
                TransactionEnvelope::Tx(tx) => tx.tx.operations.len() as u32,
                TransactionEnvelope::TxFeeBump(tx) => {
                    // Fee bump wraps an inner transaction; +1 for the wrapper itself
                    match &tx.tx.inner_tx {
                        stellar_xdr::curr::FeeBumpTransactionInnerTx::Tx(inner) => {
                            inner.tx.operations.len() as u32 + 1
                        }
                    }
                }
            }
        })
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use henyey_common::Hash256;
    use stellar_xdr::curr::{
        AccountEntry, AccountEntryExt, AccountId, ExtensionPoint, GeneralizedTransactionSet, Hash,
        LedgerEntryChange, LedgerEntryChanges, LedgerEntryData, LedgerEntryExt, OperationMeta,
        OperationMetaV2, PublicKey, SequenceNumber, StateArchivalSettings, String32, Thresholds,
        TransactionMetaV1, TransactionMetaV2, TransactionMetaV3, TransactionMetaV4,
        TransactionResultSet, TransactionSetV1, Uint256, VecM, WriteXdr,
    };

    use super::super::tests::{make_empty_tx_set, make_header_with_hashes, make_test_header};

    #[test]
    fn test_replay_empty_ledger() {
        let header = make_test_header(100);
        let tx_set = TransactionSetVariant::Classic(make_empty_tx_set());
        let tx_results = vec![];
        let tx_metas = vec![];

        let config = ReplayConfig {
            verify_results: false, // Skip verification for test
            verify_bucket_list: false,
            verify_header_hash: false,
            emit_classic_events: false,
            backfill_stellar_asset_events: false,
            run_eviction: false,
            eviction_settings: StateArchivalSettings::default(),
            wait_for_publish: false,
        };

        let result = replay_ledger(&header, &tx_set, &tx_results, &tx_metas, &config).unwrap();

        assert_eq!(result.sequence, 100);
        assert_eq!(result.tx_count, 0);
        assert_eq!(result.op_count, 0);
        assert!(result.init_entries.is_empty());
        assert!(result.live_entries.is_empty());
        assert!(result.dead_entries.is_empty());
    }

    #[test]
    fn test_count_operations_empty() {
        let tx_set = TransactionSetVariant::Classic(make_empty_tx_set());
        assert_eq!(count_operations(&tx_set), 0);
    }

    #[test]
    fn test_replay_ledger_rejects_tx_set_hash_mismatch() {
        let tx_set = TransactionSetVariant::Classic(make_empty_tx_set());
        let tx_results = vec![];
        let tx_metas = vec![];

        let tx_set_hash = verify::compute_tx_set_hash(&tx_set).expect("tx set hash");
        let header = make_header_with_hashes(100, Hash([1u8; 32]), Hash(*tx_set_hash.as_bytes()));

        // Must enable verify_results to test tx_set hash validation
        let config = ReplayConfig {
            verify_results: true,
            ..ReplayConfig::default()
        };
        let result = replay_ledger(&header, &tx_set, &tx_results, &tx_metas, &config);
        assert!(matches!(result, Err(HistoryError::InvalidTxSetHash { .. })));
    }

    #[test]
    fn test_replay_ledger_rejects_tx_result_hash_mismatch() {
        let tx_set = TransactionSetVariant::Classic(make_empty_tx_set());
        let tx_results = vec![];
        let tx_metas = vec![];

        let tx_set_hash = verify::compute_tx_set_hash(&tx_set).expect("tx set hash");

        let header = make_header_with_hashes(100, Hash(*tx_set_hash.as_bytes()), Hash([2u8; 32]));

        // Must enable verify_results to test tx_result hash validation
        let config = ReplayConfig {
            verify_results: true,
            ..ReplayConfig::default()
        };
        let result = replay_ledger(&header, &tx_set, &tx_results, &tx_metas, &config);
        assert!(matches!(result, Err(HistoryError::VerificationFailed(_))));
    }

    #[test]
    fn test_replay_ledger_accepts_generalized_tx_set() {
        let gen_set = GeneralizedTransactionSet::V1(TransactionSetV1 {
            previous_ledger_hash: Hash([0u8; 32]),
            phases: VecM::default(),
        });
        let tx_set = TransactionSetVariant::Generalized(gen_set);
        let tx_results = vec![];
        let tx_metas = vec![];

        let tx_set_hash = verify::compute_tx_set_hash(&tx_set).expect("tx set hash");

        let result_set = TransactionResultSet {
            results: VecM::default(),
        };
        let result_xdr = result_set
            .to_xdr(stellar_xdr::curr::Limits::none())
            .expect("tx result set xdr");
        let result_hash = Hash256::hash(&result_xdr);

        let header = make_header_with_hashes(
            100,
            Hash(*tx_set_hash.as_bytes()),
            Hash(*result_hash.as_bytes()),
        );

        let config = ReplayConfig::default();
        let result = replay_ledger(&header, &tx_set, &tx_results, &tx_metas, &config).unwrap();
        assert_eq!(result.tx_count, 0);
        assert_eq!(result.op_count, 0);
    }

    // --- Helpers for extract_ledger_changes tests ---

    fn make_account_entry(id_byte: u8) -> LedgerEntry {
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

    fn changes(entries: Vec<LedgerEntryChange>) -> LedgerEntryChanges {
        entries.try_into().unwrap()
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

    fn created(entry: LedgerEntry) -> LedgerEntryChange {
        LedgerEntryChange::Created(entry)
    }

    fn updated(entry: LedgerEntry) -> LedgerEntryChange {
        LedgerEntryChange::Updated(entry)
    }

    fn removed(key: LedgerKey) -> LedgerEntryChange {
        LedgerEntryChange::Removed(key)
    }

    fn restored(entry: LedgerEntry) -> LedgerEntryChange {
        LedgerEntryChange::Restored(entry)
    }

    fn state(entry: LedgerEntry) -> LedgerEntryChange {
        LedgerEntryChange::State(entry)
    }

    fn key_for(entry: &LedgerEntry) -> LedgerKey {
        henyey_common::entry_to_key(entry)
    }

    // --- Tests for extract_ledger_changes: categorization ---

    #[test]
    fn test_extract_ledger_changes_created_goes_to_init() {
        let entry = make_account_entry(1);
        let meta = make_v4_meta(vec![created(entry.clone())], vec![], vec![]);
        let (init, live, dead) = extract_ledger_changes(&[meta]).unwrap();
        assert_eq!(init, vec![entry]);
        assert!(live.is_empty());
        assert!(dead.is_empty());
    }

    #[test]
    fn test_extract_ledger_changes_updated_goes_to_live() {
        let entry = make_account_entry(1);
        let meta = make_v4_meta(vec![updated(entry.clone())], vec![], vec![]);
        let (init, live, dead) = extract_ledger_changes(&[meta]).unwrap();
        assert!(init.is_empty());
        assert_eq!(live, vec![entry]);
        assert!(dead.is_empty());
    }

    #[test]
    fn test_extract_ledger_changes_removed_goes_to_dead() {
        let entry = make_account_entry(1);
        let key = key_for(&entry);
        let meta = make_v4_meta(vec![removed(key.clone())], vec![], vec![]);
        let (init, live, dead) = extract_ledger_changes(&[meta]).unwrap();
        assert!(init.is_empty());
        assert!(live.is_empty());
        assert_eq!(dead, vec![key]);
    }

    #[test]
    fn test_extract_ledger_changes_restored_goes_to_live() {
        let entry = make_account_entry(1);
        let meta = make_v4_meta(vec![restored(entry.clone())], vec![], vec![]);
        let (init, live, dead) = extract_ledger_changes(&[meta]).unwrap();
        assert!(init.is_empty());
        assert_eq!(live, vec![entry]);
        assert!(dead.is_empty());
    }

    #[test]
    fn test_extract_ledger_changes_state_is_ignored() {
        let entry = make_account_entry(1);
        let meta = make_v4_meta(vec![state(entry)], vec![], vec![]);
        let (init, live, dead) = extract_ledger_changes(&[meta]).unwrap();
        assert!(init.is_empty());
        assert!(live.is_empty());
        assert!(dead.is_empty());
    }

    // --- Tests for extract_ledger_changes: all 5 variants ---

    #[test]
    fn test_extract_ledger_changes_v0_operations() {
        let entry_a = make_account_entry(1);
        let entry_b = make_account_entry(2);
        let meta = make_v0_meta(vec![
            vec![created(entry_a.clone())],
            vec![updated(entry_b.clone())],
        ]);
        let (init, live, dead) = extract_ledger_changes(&[meta]).unwrap();
        assert_eq!(init, vec![entry_a]);
        assert_eq!(live, vec![entry_b]);
        assert!(dead.is_empty());
    }

    #[test]
    fn test_extract_ledger_changes_v1_tx_changes_then_operations() {
        let entry_tx = make_account_entry(1);
        let entry_op = make_account_entry(2);
        let meta = make_v1_meta(
            vec![created(entry_tx.clone())],
            vec![vec![updated(entry_op.clone())]],
        );
        let (init, live, _) = extract_ledger_changes(&[meta]).unwrap();
        assert_eq!(init, vec![entry_tx]);
        assert_eq!(live, vec![entry_op]);
    }

    #[test]
    fn test_extract_ledger_changes_v2_before_ops_after() {
        let entry_before = make_account_entry(1);
        let entry_op = make_account_entry(2);
        let entry_after = make_account_entry(3);
        let meta = make_v2_meta(
            vec![created(entry_before.clone())],
            vec![vec![updated(entry_op.clone())]],
            vec![created(entry_after.clone())],
        );
        let (init, live, _) = extract_ledger_changes(&[meta]).unwrap();
        assert_eq!(init, vec![entry_before, entry_after]);
        assert_eq!(live, vec![entry_op]);
    }

    #[test]
    fn test_extract_ledger_changes_v3_before_ops_after() {
        let entry_before = make_account_entry(1);
        let entry_op = make_account_entry(2);
        let entry_after = make_account_entry(3);
        let meta = make_v3_meta(
            vec![created(entry_before.clone())],
            vec![vec![updated(entry_op.clone())]],
            vec![created(entry_after.clone())],
        );
        let (init, live, _) = extract_ledger_changes(&[meta]).unwrap();
        assert_eq!(init, vec![entry_before, entry_after]);
        assert_eq!(live, vec![entry_op]);
    }

    #[test]
    fn test_extract_ledger_changes_v4_before_ops_after() {
        let entry_before = make_account_entry(1);
        let entry_op = make_account_entry(2);
        let entry_after = make_account_entry(3);
        let meta = make_v4_meta(
            vec![created(entry_before.clone())],
            vec![vec![updated(entry_op.clone())]],
            vec![created(entry_after.clone())],
        );
        let (init, live, _) = extract_ledger_changes(&[meta]).unwrap();
        assert_eq!(init, vec![entry_before, entry_after]);
        assert_eq!(live, vec![entry_op]);
    }

    // --- Tests for ordering preservation ---

    #[test]
    fn test_extract_ledger_changes_all_meta_variants_ordered() {
        let entry_v0 = make_account_entry(0);
        let entry_v1 = make_account_entry(1);
        let entry_v2 = make_account_entry(2);
        let entry_v3 = make_account_entry(3);
        let entry_v4 = make_account_entry(4);

        let metas = vec![
            make_v0_meta(vec![vec![created(entry_v0.clone())]]),
            make_v1_meta(vec![created(entry_v1.clone())], vec![]),
            make_v2_meta(vec![created(entry_v2.clone())], vec![], vec![]),
            make_v3_meta(vec![created(entry_v3.clone())], vec![], vec![]),
            make_v4_meta(vec![created(entry_v4.clone())], vec![], vec![]),
        ];

        let (init, _, _) = extract_ledger_changes(&metas).unwrap();
        assert_eq!(init, vec![entry_v0, entry_v1, entry_v2, entry_v3, entry_v4]);
    }

    #[test]
    fn test_extract_ledger_changes_ordering_within_v4() {
        // V4: tx_changes_before → operations → tx_changes_after
        let entry_b = make_account_entry(1);
        let entry_op1 = make_account_entry(2);
        let entry_op2 = make_account_entry(3);
        let entry_a = make_account_entry(4);
        let meta = make_v4_meta(
            vec![created(entry_b.clone())],
            vec![
                vec![created(entry_op1.clone())],
                vec![created(entry_op2.clone())],
            ],
            vec![created(entry_a.clone())],
        );
        let (init, _, _) = extract_ledger_changes(&[meta]).unwrap();
        assert_eq!(init, vec![entry_b, entry_op1, entry_op2, entry_a]);
    }

    #[test]
    fn test_extract_ledger_changes_ordering_within_v1() {
        // V1: tx_changes → operations (in op order)
        let entry_tx = make_account_entry(1);
        let entry_op1 = make_account_entry(2);
        let entry_op2 = make_account_entry(3);
        let meta = make_v1_meta(
            vec![created(entry_tx.clone())],
            vec![
                vec![created(entry_op1.clone())],
                vec![created(entry_op2.clone())],
            ],
        );
        let (init, _, _) = extract_ledger_changes(&[meta]).unwrap();
        assert_eq!(init, vec![entry_tx, entry_op1, entry_op2]);
    }

    // --- Edge cases ---

    #[test]
    fn test_extract_ledger_changes_empty_input() {
        let (init, live, dead) = extract_ledger_changes(&[]).unwrap();
        assert!(init.is_empty());
        assert!(live.is_empty());
        assert!(dead.is_empty());
    }

    #[test]
    fn test_extract_ledger_changes_mixed_categories() {
        // All change types in one meta, verifying correct categorization
        let entry_created = make_account_entry(1);
        let entry_updated = make_account_entry(2);
        let entry_removed = make_account_entry(3);
        let entry_restored = make_account_entry(4);
        let entry_state = make_account_entry(5);

        let key_removed = key_for(&entry_removed);

        let meta = make_v4_meta(
            vec![
                created(entry_created.clone()),
                updated(entry_updated.clone()),
                removed(key_removed.clone()),
                restored(entry_restored.clone()),
                state(entry_state),
            ],
            vec![],
            vec![],
        );
        let (init, live, dead) = extract_ledger_changes(&[meta]).unwrap();
        assert_eq!(init, vec![entry_created]);
        assert_eq!(live, vec![entry_updated, entry_restored]);
        assert_eq!(dead, vec![key_removed]);
    }

    #[test]
    fn test_extract_ledger_changes_multiple_metas_accumulate() {
        // Changes from multiple tx metas are accumulated in order
        let entry_a = make_account_entry(1);
        let entry_b = make_account_entry(2);
        let entry_c = make_account_entry(3);

        let metas = vec![
            make_v4_meta(vec![created(entry_a.clone())], vec![], vec![]),
            make_v4_meta(vec![created(entry_b.clone())], vec![], vec![]),
            make_v4_meta(vec![created(entry_c.clone())], vec![], vec![]),
        ];
        let (init, _, _) = extract_ledger_changes(&metas).unwrap();
        assert_eq!(init, vec![entry_a, entry_b, entry_c]);
    }

    #[test]
    fn test_extract_ledger_changes_v0_multiple_changes_per_operation() {
        let entry_a = make_account_entry(1);
        let entry_b = make_account_entry(2);
        let meta = make_v0_meta(vec![vec![
            created(entry_a.clone()),
            updated(entry_b.clone()),
        ]]);
        let (init, live, _) = extract_ledger_changes(&[meta]).unwrap();
        assert_eq!(init, vec![entry_a]);
        assert_eq!(live, vec![entry_b]);
    }

    #[test]
    fn test_extract_ledger_changes_v2_ordering_before_ops_after() {
        // V2: tx_changes_before → operations → tx_changes_after
        let entry_b = make_account_entry(1);
        let entry_op1 = make_account_entry(2);
        let entry_op2 = make_account_entry(3);
        let entry_a = make_account_entry(4);
        let meta = make_v2_meta(
            vec![created(entry_b.clone())],
            vec![
                vec![created(entry_op1.clone())],
                vec![created(entry_op2.clone())],
            ],
            vec![created(entry_a.clone())],
        );
        let (init, _, _) = extract_ledger_changes(&[meta]).unwrap();
        assert_eq!(init, vec![entry_b, entry_op1, entry_op2, entry_a]);
    }
}
