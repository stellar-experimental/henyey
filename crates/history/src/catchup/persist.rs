//! Persistence logic for catchup: storing ledger history, SCP data, and bucket list snapshots.

use crate::{HistoryError, Result};
use henyey_bucket::BucketList;
use henyey_common::{Hash256, NetworkId};

use henyey_tx::TransactionFrame;
use stellar_xdr::curr::{LedgerHeader, ScpHistoryEntry, WriteXdr};
use tracing::warn;

use super::{CatchupManager, LedgerData};

impl CatchupManager {
    pub(super) fn persist_ledger_history(
        &self,
        ledger_data: &[LedgerData],
        network_id: &NetworkId,
    ) -> Result<()> {
        if ledger_data.is_empty() {
            return Ok(());
        }

        self.db
            .transaction(|conn| {
                use henyey_db::error::DbError;
                use henyey_db::queries::{HistoryQueries, LedgerQueries};

                for data in ledger_data {
                    let header = data.header();
                    let header_xdr = header.to_xdr(stellar_xdr::curr::Limits::none())?;
                    conn.store_ledger_header(header, &header_xdr)?;

                    // Only store tx/result entries for Present data.
                    // For Absent (empty-tx) ledgers, no DB rows are written —
                    // matching the archive sparsity model.
                    if let (Some(tx_entry), Some(result_entry)) =
                        (data.tx_history_entry(), data.tx_result_entry())
                    {
                        conn.store_tx_history_entry(header.ledger_seq, tx_entry)?;
                        conn.store_tx_result_entry(header.ledger_seq, result_entry)?;
                    }

                    // tx/result count consistency is guaranteed by LedgerData::new()
                    let tx_set = data.tx_set();
                    let transactions = tx_set
                        .transactions_with_base_fee()
                        .into_iter()
                        .map(|(tx, _)| tx)
                        .collect::<Vec<_>>();
                    let tx_results = data.tx_results();

                    for (idx, tx) in transactions.iter().enumerate() {
                        let tx_result = &tx_results[idx];

                        let frame = TransactionFrame::with_network(tx.clone(), *network_id);
                        let tx_hash = frame
                            .hash(network_id)
                            .map_err(|e| DbError::Integrity(e.to_string()))?;
                        let tx_id = tx_hash.to_hex();

                        let tx_body = tx.to_xdr(stellar_xdr::curr::Limits::none())?;
                        let tx_result_xdr = tx_result.to_xdr(stellar_xdr::curr::Limits::none())?;

                        // Compute status from result code
                        let status = {
                            use stellar_xdr::curr::TransactionResultCode;
                            let code = tx_result.result.result.discriminant();
                            if code == TransactionResultCode::TxSuccess
                                || code == TransactionResultCode::TxFeeBumpInnerSuccess
                            {
                                henyey_db::TxStatus::Success
                            } else {
                                henyey_db::TxStatus::Failed
                            }
                        };

                        conn.store_transaction(&henyey_db::StoreTxParams {
                            ledger_seq: header.ledger_seq,
                            tx_index: idx as u32,
                            tx_id: &tx_id,
                            body: &tx_body,
                            result: &tx_result_xdr,
                            meta: None,
                            status,
                        })?;
                    }
                }

                Ok(())
            })
            .map_err(|err| match err {
                henyey_db::DbError::Integrity(msg) => {
                    HistoryError::VerificationFailed(format!("persist integrity: {msg}"))
                }
                other => HistoryError::CatchupFailed(format!("failed to persist history: {other}")),
            })?;

        Ok(())
    }

    pub(super) fn persist_scp_history_entries(&self, entries: &[ScpHistoryEntry]) -> Result<()> {
        if entries.is_empty() {
            return Ok(());
        }

        self.db
            .transaction(|conn| {
                use henyey_db::queries::ScpQueries;

                for entry in entries {
                    let ScpHistoryEntry::V0(v0) = entry;
                    let ledger_seq = v0.ledger_messages.ledger_seq;
                    let envelopes: Vec<_> = v0.ledger_messages.messages.iter().cloned().collect();

                    conn.store_scp_history(ledger_seq, &envelopes)?;

                    for qset in v0.quorum_sets.iter() {
                        let hash = Hash256::hash_xdr(qset);
                        conn.store_scp_quorum_set(&hash, ledger_seq, qset)?;
                    }
                }

                Ok(())
            })
            .map_err(|err| {
                HistoryError::CatchupFailed(format!("failed to persist scp history: {}", err))
            })?;

        Ok(())
    }

    pub(super) fn persist_bucket_list_snapshot(
        &self,
        ledger_seq: u32,
        bucket_list: &BucketList,
    ) -> Result<()> {
        let levels = bucket_list
            .levels()
            .iter()
            .map(|level| (level.curr.hash(), level.snap.hash()))
            .collect::<Vec<_>>();
        self.db
            .with_connection(|conn| {
                use henyey_db::queries::BucketListQueries;
                conn.store_bucket_list(ledger_seq, &levels)?;
                Ok(())
            })
            .map_err(|err| {
                HistoryError::CatchupFailed(format!(
                    "failed to persist bucket list for ledger {}: {}",
                    ledger_seq, err
                ))
            })?;
        Ok(())
    }

    pub(super) fn persist_header_only(&self, header: &LedgerHeader) -> Result<()> {
        self.db
            .with_connection(|conn| {
                use henyey_db::queries::LedgerQueries;
                let header_xdr = header.to_xdr(stellar_xdr::curr::Limits::none())?;
                conn.store_ledger_header(header, &header_xdr)?;
                Ok(())
            })
            .map_err(|err| {
                HistoryError::CatchupFailed(format!("failed to persist header: {}", err))
            })?;
        Ok(())
    }

    /// Emit `LedgerCloseMeta` to both the streaming callback and SQLite.
    ///
    /// During catchup replay, meta must be persisted to the `ledger_close_meta`
    /// table so that consumers querying SQLite (e.g., RPC `getTransactions`,
    /// `getLedgers`) see entries for the replayed range. The streaming callback
    /// (if configured) writes the meta to the fd:3 pipe for captive core
    /// consumers like stellar-rpc and horizon.
    pub(super) fn emit_meta(&self, ledger_seq: u32, meta: stellar_xdr::curr::LedgerCloseMeta) {
        // Persist to SQLite first (to_xdr borrows &self on meta, no clone needed).
        match meta.to_xdr(stellar_xdr::curr::Limits::none()) {
            Ok(meta_xdr) => {
                if let Err(err) = self.db.store_ledger_close_meta(ledger_seq, &meta_xdr) {
                    warn!(
                        error = %err,
                        ledger_seq,
                        "Failed to persist LedgerCloseMeta during catchup"
                    );
                }
            }
            Err(err) => {
                warn!(
                    error = %err,
                    ledger_seq,
                    "Failed to serialize LedgerCloseMeta during catchup"
                );
            }
        }

        // Stream to external consumers (fd:3 pipe).
        if let Some(ref callback) = self.meta_callback {
            callback(meta);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catchup::make_empty_tx_set_for_header;
    use henyey_bucket::BucketManager;
    use henyey_db::Database;
    use stellar_xdr::curr::{
        Hash, LedgerHeader, Memo, MuxedAccount, Preconditions, SequenceNumber, Transaction,
        TransactionEnvelope, TransactionExt, TransactionHistoryEntry, TransactionHistoryEntryExt,
        TransactionHistoryResultEntry, TransactionHistoryResultEntryExt, TransactionResult,
        TransactionResultExt, TransactionResultPair, TransactionResultResult, TransactionResultSet,
        TransactionSet, TransactionV1Envelope, Uint256,
    };

    fn test_network_id() -> NetworkId {
        NetworkId::from_passphrase("Test SDF Network ; September 2015")
    }

    fn make_test_catchup_manager() -> CatchupManager {
        let db = Database::open_in_memory().expect("in-memory db");
        let tmp_dir = tempfile::tempdir().expect("temp dir");
        let bucket_manager = BucketManager::new(tmp_dir.keep()).expect("bucket manager");
        let archive = crate::HistoryArchive::new("https://example.com").expect("archive");
        CatchupManager::new(vec![archive], bucket_manager, db)
    }

    fn make_test_envelope() -> TransactionEnvelope {
        TransactionEnvelope::Tx(TransactionV1Envelope {
            tx: Transaction {
                source_account: MuxedAccount::Ed25519(Uint256([0u8; 32])),
                fee: 100,
                seq_num: SequenceNumber(1),
                cond: Preconditions::None,
                memo: Memo::None,
                operations: vec![].try_into().unwrap(),
                ext: TransactionExt::V0,
            },
            signatures: vec![].try_into().unwrap(),
        })
    }

    fn make_success_result() -> TransactionResultPair {
        TransactionResultPair {
            transaction_hash: Hash([0u8; 32]),
            result: TransactionResult {
                fee_charged: 100,
                result: TransactionResultResult::TxSuccess(vec![].try_into().unwrap()),
                ext: TransactionResultExt::V0,
            },
        }
    }

    fn make_header(ledger_seq: u32) -> LedgerHeader {
        LedgerHeader {
            ledger_seq,
            ..Default::default()
        }
    }

    /// Helper to construct a classic TransactionHistoryEntry with the given txs.
    fn make_tx_history_entry(
        ledger_seq: u32,
        txs: Vec<TransactionEnvelope>,
    ) -> TransactionHistoryEntry {
        TransactionHistoryEntry {
            ledger_seq,
            tx_set: TransactionSet {
                previous_ledger_hash: Hash([0u8; 32]),
                txs: txs.try_into().unwrap(),
            },
            ext: TransactionHistoryEntryExt::V0,
        }
    }

    fn make_tx_result_entry(
        ledger_seq: u32,
        results: Vec<TransactionResultPair>,
    ) -> TransactionHistoryResultEntry {
        TransactionHistoryResultEntry {
            ledger_seq,
            tx_result_set: TransactionResultSet {
                results: results.try_into().unwrap(),
            },
            ext: TransactionHistoryResultEntryExt::default(),
        }
    }

    // --- Constructor validation tests ---

    #[test]
    fn test_ledger_data_new_happy_path() {
        let header = make_header(100);
        let tx_history = make_tx_history_entry(100, vec![make_test_envelope()]);
        let tx_result = make_tx_result_entry(100, vec![make_success_result()]);
        let data = LedgerData::new(header, Some(tx_history), Some(tx_result));
        assert!(data.is_ok(), "valid LedgerData should succeed");
    }

    #[test]
    fn test_ledger_data_new_empty_ledger() {
        let header = make_header(200);
        let tx_history = make_tx_history_entry(200, vec![]);
        let tx_result = make_tx_result_entry(200, vec![]);
        let data = LedgerData::new(header, Some(tx_history), Some(tx_result));
        assert!(data.is_ok(), "empty ledger should succeed");
    }

    #[test]
    fn test_ledger_data_new_validates_tx_history_ledger_seq() {
        let header = make_header(100);
        let tx_history = make_tx_history_entry(999, vec![]); // wrong seq
        let tx_result = make_tx_result_entry(100, vec![]);
        let result = LedgerData::new(header, Some(tx_history), Some(tx_result));
        assert!(result.is_err(), "mismatched tx_history seq should fail");
        let err = result.unwrap_err();
        assert!(
            matches!(&err, HistoryError::VerificationFailed(_)),
            "expected VerificationFailed, got: {err}"
        );
    }

    #[test]
    fn test_ledger_data_new_validates_tx_result_ledger_seq() {
        let header = make_header(100);
        let tx_history = make_tx_history_entry(100, vec![]);
        let tx_result = make_tx_result_entry(999, vec![]); // wrong seq
        let result = LedgerData::new(header, Some(tx_history), Some(tx_result));
        assert!(result.is_err(), "mismatched tx_result seq should fail");
        let err = result.unwrap_err();
        assert!(
            matches!(&err, HistoryError::VerificationFailed(_)),
            "expected VerificationFailed, got: {err}"
        );
    }

    #[test]
    fn test_ledger_data_new_validates_tx_result_count() {
        let header = make_header(100);
        let tx_history = make_tx_history_entry(100, vec![make_test_envelope()]);
        let tx_result = make_tx_result_entry(100, vec![]); // 0 results for 1 tx
        let result = LedgerData::new(header, Some(tx_history), Some(tx_result));
        assert!(result.is_err(), "count mismatch should fail");
        let err = result.unwrap_err();
        assert!(
            err.is_fatal_catchup_failure(),
            "count mismatch should be fatal: {err}"
        );
        assert!(
            matches!(&err, HistoryError::VerificationFailed(_)),
            "expected VerificationFailed, got: {err}"
        );
    }

    // --- Empty tx set synthesis tests ---

    #[test]
    fn test_make_empty_tx_set_for_header_pre_v20() {
        use henyey_ledger::TransactionSetVariant;

        let header = LedgerHeader {
            ledger_seq: 50,
            ledger_version: 19,
            previous_ledger_hash: Hash([42u8; 32]),
            ..Default::default()
        };
        let tx_set = make_empty_tx_set_for_header(&header);
        match tx_set {
            TransactionSetVariant::Classic(set) => {
                assert_eq!(set.txs.len(), 0);
                assert_eq!(set.previous_ledger_hash, Hash([42u8; 32]));
            }
            _ => panic!("pre-v20 should produce Classic tx set"),
        }
    }

    #[test]
    fn test_make_empty_tx_set_for_header_v20_sequential() {
        use henyey_ledger::TransactionSetVariant;
        use stellar_xdr::curr::{GeneralizedTransactionSet, TransactionPhase};

        let header = LedgerHeader {
            ledger_seq: 100,
            ledger_version: 20,
            previous_ledger_hash: Hash([7u8; 32]),
            ..Default::default()
        };
        let tx_set = make_empty_tx_set_for_header(&header);
        match tx_set {
            TransactionSetVariant::Generalized(GeneralizedTransactionSet::V1(set)) => {
                assert_eq!(set.previous_ledger_hash, Hash([7u8; 32]));
                assert_eq!(set.phases.len(), 2);
                // Both phases should be V0 (sequential) for protocol 20-22
                assert!(
                    matches!(&set.phases[0], TransactionPhase::V0(_)),
                    "classic phase should be V0"
                );
                assert!(
                    matches!(&set.phases[1], TransactionPhase::V0(_)),
                    "soroban phase should be V0 (sequential) for protocol 20"
                );
            }
            _ => panic!("v20 should produce Generalized tx set"),
        }
    }

    #[test]
    fn test_make_empty_tx_set_for_header_v22_sequential() {
        use henyey_ledger::TransactionSetVariant;
        use stellar_xdr::curr::{GeneralizedTransactionSet, TransactionPhase};

        let header = LedgerHeader {
            ledger_seq: 100,
            ledger_version: 22,
            previous_ledger_hash: Hash([5u8; 32]),
            ..Default::default()
        };
        let tx_set = make_empty_tx_set_for_header(&header);
        match tx_set {
            TransactionSetVariant::Generalized(GeneralizedTransactionSet::V1(set)) => {
                assert_eq!(set.previous_ledger_hash, Hash([5u8; 32]));
                assert_eq!(set.phases.len(), 2);
                assert!(
                    matches!(&set.phases[1], TransactionPhase::V0(_)),
                    "soroban phase should be V0 (sequential) for protocol 22"
                );
            }
            _ => panic!("v22 should produce Generalized tx set"),
        }
    }

    #[test]
    fn test_make_empty_tx_set_for_header_v23_parallel() {
        use henyey_ledger::TransactionSetVariant;
        use stellar_xdr::curr::{GeneralizedTransactionSet, TransactionPhase};

        let header = LedgerHeader {
            ledger_seq: 100,
            ledger_version: 23,
            previous_ledger_hash: Hash([9u8; 32]),
            ..Default::default()
        };
        let tx_set = make_empty_tx_set_for_header(&header);
        match tx_set {
            TransactionSetVariant::Generalized(GeneralizedTransactionSet::V1(set)) => {
                assert_eq!(set.previous_ledger_hash, Hash([9u8; 32]));
                assert_eq!(set.phases.len(), 2);
                assert!(
                    matches!(&set.phases[0], TransactionPhase::V0(_)),
                    "classic phase should be V0"
                );
                assert!(
                    matches!(&set.phases[1], TransactionPhase::V1(_)),
                    "soroban phase should be V1 (parallel) for protocol 23+"
                );
            }
            _ => panic!("v23 should produce Generalized tx set"),
        }
    }

    #[test]
    fn test_ledger_data_new_absent_both_none() {
        let header = make_header(300);
        let data = LedgerData::new(header, None, None);
        assert!(data.is_ok(), "both None should produce Absent LedgerData");
        let data = data.unwrap();
        assert!(!data.has_transactions());
        assert!(data.tx_history_entry().is_none());
        assert!(data.tx_result_entry().is_none());
        assert_eq!(data.tx_results().len(), 0);
    }

    #[test]
    fn test_ledger_data_new_asymmetric_some_none_error() {
        let header = make_header(400);
        let tx_history = make_tx_history_entry(400, vec![]);
        let result = LedgerData::new(header, Some(tx_history), None);
        assert!(result.is_err(), "(Some, None) should fail");
        let err = result.unwrap_err();
        assert!(
            matches!(&err, HistoryError::VerificationFailed(_)),
            "expected VerificationFailed, got: {err}"
        );
    }

    #[test]
    fn test_ledger_data_new_asymmetric_none_some_error() {
        let header = make_header(400);
        let tx_result = make_tx_result_entry(400, vec![]);
        let result = LedgerData::new(header, None, Some(tx_result));
        assert!(result.is_err(), "(None, Some) should fail");
        let err = result.unwrap_err();
        assert!(
            matches!(&err, HistoryError::VerificationFailed(_)),
            "expected VerificationFailed, got: {err}"
        );
    }

    // --- Persist path tests ---

    #[test]
    fn test_persist_ledger_history_happy_path() {
        let manager = make_test_catchup_manager();
        let network_id = test_network_id();

        let envelope = make_test_envelope();
        let result_pair = make_success_result();

        let data = LedgerData::new(
            make_header(100),
            Some(make_tx_history_entry(100, vec![envelope])),
            Some(make_tx_result_entry(100, vec![result_pair])),
        )
        .expect("valid LedgerData");

        let result = manager.persist_ledger_history(&[data], &network_id);
        assert!(
            result.is_ok(),
            "happy path should succeed: {:?}",
            result.err()
        );
    }

    #[test]
    fn test_persist_ledger_history_empty_ledger() {
        let manager = make_test_catchup_manager();
        let network_id = test_network_id();

        let data = LedgerData::new(
            make_header(200),
            Some(make_tx_history_entry(200, vec![])),
            Some(make_tx_result_entry(200, vec![])),
        )
        .expect("valid empty LedgerData");

        let result = manager.persist_ledger_history(&[data], &network_id);
        assert!(
            result.is_ok(),
            "empty ledger should succeed: {:?}",
            result.err()
        );
    }

    #[test]
    fn test_persist_ledger_history_with_absent_entries() {
        let manager = make_test_catchup_manager();
        let network_id = test_network_id();

        let header = make_header(300);
        let data = LedgerData::new(header.clone(), None, None).expect("valid absent LedgerData");

        let result = manager.persist_ledger_history(&[data], &network_id);
        assert!(
            result.is_ok(),
            "absent entries should persist: {:?}",
            result.err()
        );
    }

    #[test]
    fn test_ledger_data_count_mismatch_caught_at_construction() {
        // Previously this was caught at persist time; now it's caught by LedgerData::new()
        let result = LedgerData::new(
            make_header(400),
            Some(make_tx_history_entry(400, vec![make_test_envelope()])),
            Some(make_tx_result_entry(400, vec![])), // 0 results for 1 tx
        );
        assert!(
            result.is_err(),
            "count mismatch should be caught at construction"
        );
        let err = result.unwrap_err();
        assert!(
            err.is_fatal_catchup_failure(),
            "count mismatch should be fatal: {err}"
        );
    }

    #[test]
    fn test_persist_ledger_history_batch_rollback() {
        let manager = make_test_catchup_manager();
        let network_id = test_network_id();

        let envelope = make_test_envelope();
        let result_pair = make_success_result();

        // First ledger: valid
        let good_data = LedgerData::new(
            make_header(500),
            Some(make_tx_history_entry(500, vec![envelope.clone()])),
            Some(make_tx_result_entry(500, vec![result_pair])),
        )
        .expect("valid good LedgerData");

        // Second ledger: empty (valid on its own, but simulate a DB-level failure
        // by having two ledgers with the same seq — the second will fail to insert)
        let also_500 = LedgerData::new(
            make_header(500),
            Some(make_tx_history_entry(500, vec![])),
            Some(make_tx_result_entry(500, vec![])),
        )
        .expect("valid duplicate LedgerData");

        let result = manager.persist_ledger_history(&[good_data, also_500], &network_id);
        // The duplicate seq should cause a DB constraint error or be handled
        // Either way, the batch should roll back
        if result.is_err() {
            let header_check = manager.db.with_connection(|conn| {
                use henyey_db::queries::LedgerQueries;
                conn.load_ledger_header(500)
            });
            assert!(
                header_check.is_err() || header_check.unwrap().is_none(),
                "good ledger should be rolled back when batch fails"
            );
        }
    }

    // --- DB round-trip test ---

    #[test]
    fn test_persist_and_load_ledger_header_round_trip() {
        let manager = make_test_catchup_manager();
        let network_id = test_network_id();

        let header = make_header(600);
        let data = LedgerData::new(
            header.clone(),
            Some(make_tx_history_entry(600, vec![])),
            Some(make_tx_result_entry(600, vec![])),
        )
        .expect("valid LedgerData");

        manager
            .persist_ledger_history(&[data], &network_id)
            .expect("persist should succeed");

        let loaded = manager
            .db
            .with_connection(|conn| {
                use henyey_db::queries::LedgerQueries;
                conn.load_ledger_header(600)
            })
            .expect("load should succeed")
            .expect("header should exist");

        assert_eq!(loaded.ledger_seq, 600);
    }
}
