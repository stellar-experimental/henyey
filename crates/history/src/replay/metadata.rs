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
pub fn replay_ledger(
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
pub fn extract_ledger_changes(
    tx_metas: &[TransactionMeta],
) -> Result<(Vec<LedgerEntry>, Vec<LedgerEntry>, Vec<LedgerKey>)> {
    let mut init_entries = Vec::new();
    let mut live_entries = Vec::new();
    let mut dead_entries = Vec::new();

    for meta in tx_metas {
        match meta {
            TransactionMeta::V0(operations) => {
                // V0: VecM<OperationMeta> - each OperationMeta has a changes field
                for op_meta in operations.iter() {
                    for change in op_meta.changes.iter() {
                        process_ledger_entry_change(
                            change,
                            &mut init_entries,
                            &mut live_entries,
                            &mut dead_entries,
                        );
                    }
                }
            }
            TransactionMeta::V1(v1) => {
                // Process txChanges (before)
                for change in v1.tx_changes.iter() {
                    process_ledger_entry_change(
                        change,
                        &mut init_entries,
                        &mut live_entries,
                        &mut dead_entries,
                    );
                }
                // Process operation changes
                for op_changes in v1.operations.iter() {
                    for change in op_changes.changes.iter() {
                        process_ledger_entry_change(
                            change,
                            &mut init_entries,
                            &mut live_entries,
                            &mut dead_entries,
                        );
                    }
                }
            }
            // V2, V3, and V4 share the same before/operations/after structure
            TransactionMeta::V2(v2) => {
                extract_before_ops_after_changes(
                    &v2.tx_changes_before,
                    v2.operations.iter().map(|op| &op.changes),
                    &v2.tx_changes_after,
                    &mut init_entries,
                    &mut live_entries,
                    &mut dead_entries,
                );
            }
            TransactionMeta::V3(v3) => {
                extract_before_ops_after_changes(
                    &v3.tx_changes_before,
                    v3.operations.iter().map(|op| &op.changes),
                    &v3.tx_changes_after,
                    &mut init_entries,
                    &mut live_entries,
                    &mut dead_entries,
                );
            }
            TransactionMeta::V4(v4) => {
                extract_before_ops_after_changes(
                    &v4.tx_changes_before,
                    v4.operations.iter().map(|op| &op.changes),
                    &v4.tx_changes_after,
                    &mut init_entries,
                    &mut live_entries,
                    &mut dead_entries,
                );
            }
        }
    }

    Ok((init_entries, live_entries, dead_entries))
}

/// Extract changes from V2/V3/V4 meta which share tx_changes_before, operations, tx_changes_after.
///
/// The `op_changes` parameter accepts an iterator of `LedgerEntryChanges` references to handle
/// both `OperationMeta` (V2/V3) and `OperationMetaV2` (V4), which differ in type but both
/// have a `.changes` field of type `LedgerEntryChanges`.
fn extract_before_ops_after_changes<'a>(
    tx_changes_before: &stellar_xdr::curr::LedgerEntryChanges,
    op_changes: impl Iterator<Item = &'a stellar_xdr::curr::LedgerEntryChanges>,
    tx_changes_after: &stellar_xdr::curr::LedgerEntryChanges,
    init_entries: &mut Vec<LedgerEntry>,
    live_entries: &mut Vec<LedgerEntry>,
    dead_entries: &mut Vec<LedgerKey>,
) {
    for change in tx_changes_before.iter() {
        process_ledger_entry_change(change, init_entries, live_entries, dead_entries);
    }
    for changes in op_changes {
        for change in changes.iter() {
            process_ledger_entry_change(change, init_entries, live_entries, dead_entries);
        }
    }
    for change in tx_changes_after.iter() {
        process_ledger_entry_change(change, init_entries, live_entries, dead_entries);
    }
}

/// Process a single ledger entry change.
fn process_ledger_entry_change(
    change: &stellar_xdr::curr::LedgerEntryChange,
    init_entries: &mut Vec<LedgerEntry>,
    live_entries: &mut Vec<LedgerEntry>,
    dead_entries: &mut Vec<LedgerKey>,
) {
    use stellar_xdr::curr::LedgerEntryChange;

    match change {
        LedgerEntryChange::Created(entry) => {
            init_entries.push(entry.clone());
        }
        LedgerEntryChange::Updated(entry) => {
            live_entries.push(entry.clone());
        }
        LedgerEntryChange::Removed(key) => {
            dead_entries.push(key.clone());
        }
        LedgerEntryChange::State(_) => {
            // State entries represent the state before a change,
            // we don't need to process them for replay
        }
        LedgerEntryChange::Restored(entry) => {
            // Restored entries (from Soroban) are treated as live entries
            live_entries.push(entry.clone());
        }
    }
}

/// Count the total number of operations in a transaction set.
fn count_operations(tx_set: &TransactionSetVariant) -> u32 {
    let mut count = 0;

    for tx_env in tx_set.transactions().into_iter() {
        use stellar_xdr::curr::TransactionEnvelope;
        match tx_env {
            TransactionEnvelope::TxV0(tx) => {
                count += tx.tx.operations.len() as u32;
            }
            TransactionEnvelope::Tx(tx) => {
                count += tx.tx.operations.len() as u32;
            }
            TransactionEnvelope::TxFeeBump(tx) => {
                // Fee bump wraps an inner transaction
                match &tx.tx.inner_tx {
                    stellar_xdr::curr::FeeBumpTransactionInnerTx::Tx(inner) => {
                        count += inner.tx.operations.len() as u32;
                    }
                }
            }
        }
    }

    count
}

#[cfg(test)]
mod tests {
    use super::*;
    use henyey_common::Hash256;
    use stellar_xdr::curr::{
        GeneralizedTransactionSet, Hash, StateArchivalSettings, TransactionResultSet,
        TransactionSetV1, VecM, WriteXdr,
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
}
