//! Persistence logic for catchup: storing ledger history, SCP data, and bucket list snapshots.

use crate::{HistoryError, Result};
use henyey_bucket::BucketList;
use henyey_common::{Hash256, NetworkId};

use henyey_tx::TransactionFrame;
use stellar_xdr::{LedgerHeader, ScpHistoryEntry, WriteXdr};
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
                    let header_xdr = header.to_xdr(stellar_xdr::Limits::none())?;
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

                        let tx_body = tx.to_xdr(stellar_xdr::Limits::none())?;
                        let tx_result_xdr = tx_result.to_xdr(stellar_xdr::Limits::none())?;

                        // Compute status from result code
                        let status = {
                            use stellar_xdr::TransactionResultCode;
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
                let header_xdr = header.to_xdr(stellar_xdr::Limits::none())?;
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
    pub(super) fn emit_meta(&self, ledger_seq: u32, meta: stellar_xdr::LedgerCloseMeta) {
        // Persist to SQLite first (to_xdr borrows &self on meta, no clone needed).
        match meta.to_xdr(stellar_xdr::Limits::none()) {
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
    use crate::catchup::make_empty_tx_set;
    use henyey_bucket::BucketManager;
    use henyey_common::protocol::LclContext;
    use henyey_db::Database;
    use stellar_xdr::{
        Hash, LedgerHeader, Memo, MuxedAccount, Preconditions, SequenceNumber, Transaction,
        TransactionEnvelope, TransactionExt, TransactionHistoryEntry, TransactionHistoryEntryExt,
        TransactionHistoryResultEntry, TransactionHistoryResultEntryExt, TransactionResult,
        TransactionResultExt, TransactionResultPair, TransactionResultResult, TransactionResultSet,
        TransactionSet, TransactionV1Envelope, Uint256,
    };

    fn test_network_id() -> NetworkId {
        NetworkId::from_passphrase("Test SDF Network ; September 2015")
    }

    /// Returns the `TempDir` guard backing the `BucketManager` together
    /// with the manager. The `TempDir` is returned **first** so callers
    /// destructure as `let (_tmp_dir, manager) = ...`: Rust drops local
    /// bindings in reverse declaration order, so binding `manager`
    /// second guarantees it (and any file handles its `BucketManager`
    /// holds) is dropped before the `TempDir` removes the directory.
    /// The caller must keep `_tmp_dir` alive for the duration of the
    /// test so the bucket directory is deleted on drop rather than
    /// leaked under the system temp location.
    fn make_test_catchup_manager() -> (tempfile::TempDir, CatchupManager) {
        let db = Database::open_in_memory().expect("in-memory db");
        let tmp_dir = tempfile::tempdir().expect("temp dir");
        let bucket_manager =
            BucketManager::new(tmp_dir.path().to_path_buf()).expect("bucket manager");
        let archive = crate::HistoryArchive::new("https://example.com").expect("archive");
        (
            tmp_dir,
            CatchupManager::new(vec![archive], bucket_manager, db),
        )
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
        let data = LedgerData::new(
            header,
            Some(tx_history),
            Some(tx_result),
            &LclContext::pre_genesis(),
        );
        assert!(data.is_ok(), "valid LedgerData should succeed");
    }

    #[test]
    fn test_ledger_data_new_empty_ledger() {
        let header = make_header(200);
        let tx_history = make_tx_history_entry(200, vec![]);
        let tx_result = make_tx_result_entry(200, vec![]);
        let data = LedgerData::new(
            header,
            Some(tx_history),
            Some(tx_result),
            &LclContext::pre_genesis(),
        );
        assert!(data.is_ok(), "empty ledger should succeed");
    }

    #[test]
    fn test_ledger_data_new_validates_tx_history_ledger_seq() {
        let header = make_header(100);
        let tx_history = make_tx_history_entry(999, vec![]); // wrong seq
        let tx_result = make_tx_result_entry(100, vec![]);
        let result = LedgerData::new(
            header,
            Some(tx_history),
            Some(tx_result),
            &LclContext::pre_genesis(),
        );
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
        let result = LedgerData::new(
            header,
            Some(tx_history),
            Some(tx_result),
            &LclContext::pre_genesis(),
        );
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
        let result = LedgerData::new(
            header,
            Some(tx_history),
            Some(tx_result),
            &LclContext::pre_genesis(),
        );
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
    fn test_make_empty_tx_set_pre_v20() {
        use henyey_ledger::TransactionSetVariant;

        let lcl = LclContext::new(19, henyey_common::Hash256([42u8; 32]));
        let tx_set = make_empty_tx_set(&lcl);
        match tx_set {
            TransactionSetVariant::Classic(set) => {
                assert_eq!(set.txs.len(), 0);
                assert_eq!(set.previous_ledger_hash, Hash([42u8; 32]));
            }
            _ => panic!("pre-v20 should produce Classic tx set"),
        }
    }

    #[test]
    fn test_make_empty_tx_set_v20_sequential() {
        use henyey_ledger::TransactionSetVariant;
        use stellar_xdr::{GeneralizedTransactionSet, TransactionPhase};

        let lcl = LclContext::new(20, henyey_common::Hash256([7u8; 32]));
        let tx_set = make_empty_tx_set(&lcl);
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
    fn test_make_empty_tx_set_v22_sequential() {
        use henyey_ledger::TransactionSetVariant;
        use stellar_xdr::{GeneralizedTransactionSet, TransactionPhase};

        let lcl = LclContext::new(22, henyey_common::Hash256([5u8; 32]));
        let tx_set = make_empty_tx_set(&lcl);
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
    fn test_make_empty_tx_set_v23_parallel() {
        use henyey_ledger::TransactionSetVariant;
        use stellar_xdr::{GeneralizedTransactionSet, TransactionPhase};

        let lcl = LclContext::new(23, henyey_common::Hash256([9u8; 32]));
        let tx_set = make_empty_tx_set(&lcl);
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
        let lcl = LclContext::pre_genesis();
        let data = LedgerData::new(header, None, None, &lcl);
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
        let result = LedgerData::new(header, Some(tx_history), None, &LclContext::pre_genesis());
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
        let result = LedgerData::new(header, None, Some(tx_result), &LclContext::pre_genesis());
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
        let (_tmp_dir, manager) = make_test_catchup_manager();
        let network_id = test_network_id();

        let envelope = make_test_envelope();
        let result_pair = make_success_result();

        let data = LedgerData::new(
            make_header(100),
            Some(make_tx_history_entry(100, vec![envelope])),
            Some(make_tx_result_entry(100, vec![result_pair])),
            &LclContext::pre_genesis(),
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
    fn test_ledger_data_new_lcl_stitching_mismatch_returns_error() {
        // Regression test: LCL stitching check must be a hard error in release builds.
        // When (None, None) arms are hit, previous_ledger_hash must match lcl_hash.
        let mut header = make_header(300);
        // Set previous_ledger_hash to something non-zero
        header.previous_ledger_hash = Hash([0xAB; 32]);
        // LCL hash is all zeros (pre_genesis), which differs from 0xAB
        let lcl = LclContext::new(20, henyey_common::Hash256::from_bytes([0x00; 32]));
        let result = LedgerData::new(header, None, None, &lcl);
        assert!(result.is_err(), "LCL hash mismatch should return Err");
        let err_msg = format!("{}", result.unwrap_err());
        assert!(
            err_msg.contains("LCL stitching failed"),
            "error should mention LCL stitching: {}",
            err_msg
        );
    }

    #[test]
    fn test_persist_ledger_history_empty_ledger() {
        let (_tmp_dir, manager) = make_test_catchup_manager();
        let network_id = test_network_id();

        let data = LedgerData::new(
            make_header(200),
            Some(make_tx_history_entry(200, vec![])),
            Some(make_tx_result_entry(200, vec![])),
            &LclContext::pre_genesis(),
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
        let (_tmp_dir, manager) = make_test_catchup_manager();
        let network_id = test_network_id();

        let header = make_header(300);
        let data = LedgerData::new(header.clone(), None, None, &LclContext::pre_genesis())
            .expect("valid absent LedgerData");

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
            &LclContext::pre_genesis(),
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
        let (_tmp_dir, manager) = make_test_catchup_manager();
        let network_id = test_network_id();

        let envelope = make_test_envelope();
        let result_pair = make_success_result();

        // First ledger: valid
        let good_data = LedgerData::new(
            make_header(500),
            Some(make_tx_history_entry(500, vec![envelope.clone()])),
            Some(make_tx_result_entry(500, vec![result_pair])),
            &LclContext::pre_genesis(),
        )
        .expect("valid good LedgerData");

        // Second ledger: empty (valid on its own, but simulate a DB-level failure
        // by having two ledgers with the same seq — the second will fail to insert)
        let also_500 = LedgerData::new(
            make_header(500),
            Some(make_tx_history_entry(500, vec![])),
            Some(make_tx_result_entry(500, vec![])),
            &LclContext::pre_genesis(),
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
        let (_tmp_dir, manager) = make_test_catchup_manager();
        let network_id = test_network_id();

        let header = make_header(600);
        let data = LedgerData::new(
            header.clone(),
            Some(make_tx_history_entry(600, vec![])),
            Some(make_tx_result_entry(600, vec![])),
            &LclContext::pre_genesis(),
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

    // --- Protocol-boundary hash verification regression tests ---
    //
    // These tests verify that `make_empty_tx_set` produces the correct format
    // for each protocol boundary, and that `verify_tx_set` accepts the
    // synthesized set when the header's scp_value.tx_set_hash is computed
    // from the same format.

    /// Helper: build a header whose scp_value.tx_set_hash matches the empty tx
    /// set synthesized with the given LCL protocol version.
    fn make_header_with_empty_tx_set_hash(
        ledger_seq: u32,
        current_protocol: u32,
        lcl_protocol: u32,
    ) -> LedgerHeader {
        use crate::verify::compute_tx_set_hash;

        let previous_ledger_hash = Hash([0xAB; 32]);
        let lcl = LclContext::new(lcl_protocol, henyey_common::Hash256([0xAB; 32]));
        let tx_set = make_empty_tx_set(&lcl);
        let tx_set_hash = compute_tx_set_hash(&tx_set).expect("hash computation");

        LedgerHeader {
            ledger_seq,
            ledger_version: current_protocol,
            previous_ledger_hash,
            scp_value: stellar_xdr::StellarValue {
                tx_set_hash: stellar_xdr::Hash(tx_set_hash.0),
                close_time: stellar_xdr::TimePoint(0),
                upgrades: Default::default(),
                ext: stellar_xdr::StellarValueExt::Basic,
            },
            ..Default::default()
        }
    }

    #[test]
    fn test_empty_tx_set_hash_protocol_0_to_22_regression() {
        // The failing quickstart galexie case: LCL is genesis (protocol 0),
        // current ledger upgrades to protocol 22. Must use Classic format.
        use crate::verify::verify_tx_set;
        use henyey_ledger::TransactionSetVariant;

        let header = make_header_with_empty_tx_set_hash(2, 22, 0);
        let lcl = LclContext::new(0, henyey_common::Hash256(header.previous_ledger_hash.0));
        let tx_set = make_empty_tx_set(&lcl);

        // Must be Classic format (protocol 0 < 20)
        assert!(
            matches!(tx_set, TransactionSetVariant::Classic(_)),
            "LCL protocol 0 should produce Classic tx set, got Generalized"
        );

        // verify_tx_set must accept this
        verify_tx_set(&header, &tx_set).expect("hash should match for protocol 0→22 upgrade");
    }

    #[test]
    fn test_empty_tx_set_hash_protocol_19_to_20_regression() {
        // Protocol boundary: LCL at 19 (Classic), current upgrades to 20.
        use crate::verify::verify_tx_set;
        use henyey_ledger::TransactionSetVariant;

        let header = make_header_with_empty_tx_set_hash(100, 20, 19);
        let lcl = LclContext::new(19, henyey_common::Hash256(header.previous_ledger_hash.0));
        let tx_set = make_empty_tx_set(&lcl);

        assert!(
            matches!(tx_set, TransactionSetVariant::Classic(_)),
            "LCL protocol 19 should produce Classic tx set"
        );

        verify_tx_set(&header, &tx_set).expect("hash should match for protocol 19→20 upgrade");
    }

    #[test]
    fn test_empty_tx_set_hash_protocol_20_steady_state() {
        // Steady state: LCL at 20, current at 20. Generalized V0+V0.
        use crate::verify::verify_tx_set;
        use henyey_ledger::TransactionSetVariant;
        use stellar_xdr::{GeneralizedTransactionSet, TransactionPhase};

        let header = make_header_with_empty_tx_set_hash(200, 20, 20);
        let lcl = LclContext::new(20, henyey_common::Hash256(header.previous_ledger_hash.0));
        let tx_set = make_empty_tx_set(&lcl);

        match &tx_set {
            TransactionSetVariant::Generalized(GeneralizedTransactionSet::V1(set)) => {
                assert!(matches!(&set.phases[0], TransactionPhase::V0(_)));
                assert!(matches!(&set.phases[1], TransactionPhase::V0(_)));
            }
            _ => panic!("LCL protocol 20 should produce Generalized V0+V0"),
        }

        verify_tx_set(&header, &tx_set).expect("hash should match for protocol 20→20");
    }

    #[test]
    fn test_empty_tx_set_hash_protocol_22_to_23_regression() {
        // Protocol boundary: LCL at 22 (Generalized V0+V0), current upgrades to 23.
        use crate::verify::verify_tx_set;
        use henyey_ledger::TransactionSetVariant;
        use stellar_xdr::{GeneralizedTransactionSet, TransactionPhase};

        let header = make_header_with_empty_tx_set_hash(300, 23, 22);
        let lcl = LclContext::new(22, henyey_common::Hash256(header.previous_ledger_hash.0));
        let tx_set = make_empty_tx_set(&lcl);

        match &tx_set {
            TransactionSetVariant::Generalized(GeneralizedTransactionSet::V1(set)) => {
                assert!(matches!(&set.phases[0], TransactionPhase::V0(_)));
                // LCL protocol 22 < 23, so soroban phase is still V0 (sequential)
                assert!(
                    matches!(&set.phases[1], TransactionPhase::V0(_)),
                    "LCL protocol 22 should produce sequential soroban phase"
                );
            }
            _ => panic!("LCL protocol 22 should produce Generalized V0+V0"),
        }

        verify_tx_set(&header, &tx_set).expect("hash should match for protocol 22→23 upgrade");
    }

    #[test]
    fn test_empty_tx_set_hash_protocol_23_steady_state() {
        // Steady state: LCL at 23, current at 23. Generalized V0+V1 (parallel).
        use crate::verify::verify_tx_set;
        use henyey_ledger::TransactionSetVariant;
        use stellar_xdr::{GeneralizedTransactionSet, TransactionPhase};

        let header = make_header_with_empty_tx_set_hash(400, 23, 23);
        let lcl = LclContext::new(23, henyey_common::Hash256(header.previous_ledger_hash.0));
        let tx_set = make_empty_tx_set(&lcl);

        match &tx_set {
            TransactionSetVariant::Generalized(GeneralizedTransactionSet::V1(set)) => {
                assert!(matches!(&set.phases[0], TransactionPhase::V0(_)));
                assert!(
                    matches!(&set.phases[1], TransactionPhase::V1(_)),
                    "LCL protocol 23 should produce parallel soroban phase"
                );
            }
            _ => panic!("LCL protocol 23 should produce Generalized V0+V1"),
        }

        verify_tx_set(&header, &tx_set).expect("hash should match for protocol 23→23");
    }

    #[test]
    fn test_empty_tx_set_hash_mismatch_when_using_wrong_protocol() {
        // Demonstrates the original bug: using current protocol (22) instead of
        // LCL protocol (0) produces a different hash that fails verification.
        use crate::verify::verify_tx_set;
        use crate::HistoryError;
        use henyey_common::Hash256;

        // Header with tx_set_hash computed from LCL protocol 0 (Classic format)
        let header = make_header_with_empty_tx_set_hash(2, 22, 0);

        // BUG reproduction: synthesize using the *wrong* protocol (current = 22)
        let wrong_lcl = LclContext::new(22, henyey_common::Hash256(header.previous_ledger_hash.0));
        let wrong_tx_set = make_empty_tx_set(&wrong_lcl);

        // This should fail — the hash won't match
        let result = verify_tx_set(&header, &wrong_tx_set);
        let err = result.expect_err(
            "using current protocol (22) instead of LCL protocol (0) should produce hash mismatch",
        );

        // Verify the diagnostic payload is correctly populated
        match err {
            HistoryError::InvalidTxSetHash { ledger, info } => {
                assert_eq!(ledger, 2);
                assert_eq!(info.header_ledger_version, 22);
                // Wrong protocol (22) produces generalized format
                assert_eq!(info.tx_set_format, "generalized_v1");
                assert_ne!(
                    info.expected, info.actual,
                    "hashes must differ for the test to be valid"
                );
                // Both should use the same previous_ledger_hash since we passed the same one
                assert_eq!(
                    info.header_prev_hash, info.tx_set_prev_hash,
                    "prev hash should match — the bug is format, not prev hash"
                );
                // Verify hashes are non-zero (sanity check)
                assert_ne!(info.expected, Hash256::ZERO);
                assert_ne!(info.actual, Hash256::ZERO);
            }
            other => panic!("expected InvalidTxSetHash, got: {}", other),
        }
    }

    #[test]
    fn test_empty_tx_set_hash_ci_regression_genesis_v23() {
        // Regression test for #2292: quickstart galexie CI failure.
        // The actual network genesis is at protocol 23, but henyey's synthetic
        // genesis was at protocol 0. Verify that make_empty_tx_set with protocol
        // 23 produces the Generalized v23+ format and that using protocol 0
        // instead produces a Classic format with a different hash (the bug).
        use crate::verify::{compute_tx_set_hash, verify_tx_set};
        use henyey_ledger::TransactionSetVariant;

        let prev_hash = Hash([0x42; 32]);

        // With protocol 23, should produce Generalized tx set
        let lcl_v23 = LclContext::new(23, henyey_common::Hash256([0x42; 32]));
        let tx_set_v23 = make_empty_tx_set(&lcl_v23);
        assert!(
            matches!(tx_set_v23, TransactionSetVariant::Generalized(_)),
            "protocol 23 should produce Generalized tx set"
        );
        let hash_v23 = compute_tx_set_hash(&tx_set_v23).expect("hash computation");

        // With protocol 0, should produce Classic tx set (the bug)
        let lcl_v0 = LclContext::new(0, henyey_common::Hash256([0x42; 32]));
        let tx_set_v0 = make_empty_tx_set(&lcl_v0);
        assert!(
            matches!(tx_set_v0, TransactionSetVariant::Classic(_)),
            "protocol 0 should produce Classic tx set"
        );
        let hash_v0 = compute_tx_set_hash(&tx_set_v0).expect("hash computation");

        // They must differ — this is the exact mismatch from #2292
        assert_ne!(
            hash_v23, hash_v0,
            "protocol 23 and 0 must produce different hashes for same prev_hash"
        );

        // Build a header that expects the v23 hash (simulating stellar-core's output)
        let header = LedgerHeader {
            ledger_seq: 2,
            ledger_version: 23,
            previous_ledger_hash: prev_hash,
            scp_value: stellar_xdr::StellarValue {
                tx_set_hash: stellar_xdr::Hash(hash_v23.0),
                close_time: stellar_xdr::TimePoint(0),
                upgrades: Default::default(),
                ext: stellar_xdr::StellarValueExt::Basic,
            },
            ..Default::default()
        };

        // Correct protocol → verify passes
        verify_tx_set(&header, &tx_set_v23)
            .expect("v23 tx set should pass verification against v23 header");

        // Wrong protocol → verify fails (reproduces #2292)
        verify_tx_set(&header, &tx_set_v0)
            .expect_err("v0 tx set should fail verification against v23 header");
    }

    // --- #3801: transient-busy retry around the catchup meta write ---

    /// Build a `DbError::Sqlite(SqliteFailure(ffi::Error { code, .. }, msg))`,
    /// mirroring how rusqlite materializes a SQLite error on a contended write.
    fn db_sqlite_error(code: rusqlite::ffi::ErrorCode, msg: &str) -> henyey_db::DbError {
        henyey_db::DbError::Sqlite(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error {
                code,
                extended_code: 0,
            },
            Some(msg.to_string()),
        ))
    }

    fn busy_error() -> henyey_db::DbError {
        db_sqlite_error(rusqlite::ffi::ErrorCode::DatabaseBusy, "database is locked")
    }

    /// A transient busy on the first attempt must be retried, not dropped.
    #[test]
    fn test_store_meta_with_busy_retry_retries_transient_busy_then_stores() {
        let calls = std::cell::Cell::new(0u32);
        let outcome = store_meta_with_busy_retry(|| {
            calls.set(calls.get() + 1);
            if calls.get() == 1 {
                Err(busy_error())
            } else {
                Ok(())
            }
        });

        assert_eq!(calls.get(), 2, "the store must be re-issued after a busy");
        assert!(
            matches!(outcome, MetaStoreOutcome::Stored),
            "the retried write succeeded, so the outcome must be Stored"
        );
    }

    /// A persistently busy DB must stop at the retry bound (no spin), report
    /// `DroppedBusy`, and emit exactly the #3802 telemetry: one drop and
    /// `MAX_ATTEMPTS - 1` retried attempts.
    #[test]
    fn test_store_meta_with_busy_retry_gives_up_after_max_attempts_and_counts_drop() {
        let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();

        let calls = std::cell::Cell::new(0u32);
        let outcome = metrics::with_local_recorder(&recorder, || {
            store_meta_with_busy_retry(|| {
                calls.set(calls.get() + 1);
                Err(busy_error())
            })
        });

        assert_eq!(
            calls.get(),
            CATCHUP_META_MAX_ATTEMPTS,
            "the retry must be bounded at CATCHUP_META_MAX_ATTEMPTS"
        );
        assert!(
            matches!(outcome, MetaStoreOutcome::DroppedBusy(_)),
            "exhausting the budget on a busy error must report DroppedBusy"
        );

        let output = handle.render();
        assert!(
            output.contains("henyey_db_busy_write_dropped_total{site=\"catchup_meta\"} 1"),
            "the abandoned write must increment the drop series.\nOutput:\n{output}"
        );
        assert!(
            output.contains(&format!(
                "henyey_db_busy_retry_attempts_total{{site=\"catchup_meta\"}} {}",
                CATCHUP_META_MAX_ATTEMPTS - 1
            )),
            "one retry-attempt increment per retried attempt.\nOutput:\n{output}"
        );
    }

    /// Contract point 4 of #3802: non-transient errors are neither retried nor
    /// counted against the busy-loss series.
    #[test]
    fn test_store_meta_with_busy_retry_does_not_retry_non_transient() {
        let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();

        let calls = std::cell::Cell::new(0u32);
        let outcome = metrics::with_local_recorder(&recorder, || {
            store_meta_with_busy_retry(|| {
                calls.set(calls.get() + 1);
                Err(henyey_db::DbError::Integrity("corrupt row".to_string()))
            })
        });

        assert_eq!(calls.get(), 1, "a non-transient error must not be retried");
        assert!(
            matches!(outcome, MetaStoreOutcome::Failed(_)),
            "a non-transient error must report Failed"
        );

        let output = handle.render();
        assert!(
            !output.contains("henyey_db_busy_write_dropped_total"),
            "non-transient losses must not inflate the busy-drop series.\nOutput:\n{output}"
        );
        assert!(
            !output.contains("henyey_db_busy_retry_attempts_total"),
            "non-transient losses must not inflate the retry series.\nOutput:\n{output}"
        );
    }

    /// End-to-end wiring: `emit_meta` must route its store through the retry
    /// helper, count the loss, and still stream the frame to the fd:3 callback.
    /// Uses a REAL DB fault (the table is dropped) rather than the closure seam,
    /// so a correct-but-uncalled helper cannot pass the suite.
    #[test]
    fn test_emit_meta_routes_store_through_retry_helper() {
        let (_tmp_dir, mut manager) = make_test_catchup_manager();

        let fired = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let fired_cb = fired.clone();
        manager.set_meta_callback(Box::new(move |_meta| {
            fired_cb.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }));

        // Deterministic, non-transient DB fault on the meta write path.
        manager
            .db
            .with_connection(|conn| {
                conn.execute("DROP TABLE ledger_close_meta", [])?;
                Ok(())
            })
            .expect("drop ledger_close_meta");

        let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        let header = make_header(42);
        let meta = crate::catchup::build_bucket_apply_meta(&header, Hash256::ZERO, false);
        metrics::with_local_recorder(&recorder, || manager.emit_meta(42, meta));

        assert_eq!(
            manager.meta_rows_dropped(),
            1,
            "the lost meta row must be reported to the caller"
        );
        assert_eq!(
            fired.load(std::sync::atomic::Ordering::Relaxed),
            1,
            "the fd:3 callback must stay unconditional on every path"
        );
        let output = handle.render();
        assert!(
            !output.contains("henyey_db_busy_write_dropped_total"),
            "a non-transient DB fault must not be counted as a busy drop.\nOutput:\n{output}"
        );
    }

    /// Happy path: the refactor must still persist the row AND stream the frame.
    #[test]
    fn test_emit_meta_persists_row_and_invokes_callback() {
        let (_tmp_dir, mut manager) = make_test_catchup_manager();

        let fired = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let fired_cb = fired.clone();
        manager.set_meta_callback(Box::new(move |_meta| {
            fired_cb.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }));

        let header = make_header(7);
        let meta = crate::catchup::build_bucket_apply_meta(&header, Hash256::ZERO, false);
        let expected = meta.to_xdr(stellar_xdr::Limits::none()).expect("serialize");
        manager.emit_meta(7, meta);

        let stored = manager
            .db
            .with_connection(|conn| {
                use henyey_db::queries::LedgerCloseMetaQueries;
                conn.load_ledger_close_meta(7)
            })
            .expect("load meta");
        assert_eq!(
            stored,
            Some(expected),
            "the happy path must still write the ledger_close_meta row"
        );
        assert_eq!(
            fired.load(std::sync::atomic::Ordering::Relaxed),
            1,
            "the fd:3 callback must fire exactly once"
        );
        assert_eq!(
            manager.meta_rows_dropped(),
            0,
            "no drop on the happy path"
        );
    }
}
