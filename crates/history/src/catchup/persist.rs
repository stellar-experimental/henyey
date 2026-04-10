//! Persistence logic for catchup: storing ledger history, SCP data, and bucket list snapshots.

use crate::{HistoryError, Result};
use henyey_bucket::BucketList;
use henyey_common::LedgerSeq;
use henyey_common::{Hash256, NetworkId};

use henyey_tx::TransactionFrame;
use stellar_xdr::curr::{
    Hash, LedgerHeader, ScpHistoryEntry, TransactionHistoryEntry, TransactionHistoryEntryExt,
    TransactionHistoryResultEntry, TransactionHistoryResultEntryExt, TransactionResultPair,
    TransactionResultSet, TransactionSet, WriteXdr,
};
use tracing::warn;

use henyey_ledger::TransactionSetVariant;

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
                    let header_xdr = data.header.to_xdr(stellar_xdr::curr::Limits::none())?;
                    conn.store_ledger_header(&data.header, &header_xdr)?;

                    let tx_history_entry =
                        data.tx_history_entry
                            .clone()
                            .unwrap_or_else(|| match &data.tx_set {
                                TransactionSetVariant::Classic(set) => TransactionHistoryEntry {
                                    ledger_seq: data.header.ledger_seq,
                                    tx_set: set.clone(),
                                    ext: TransactionHistoryEntryExt::V0,
                                },
                                TransactionSetVariant::Generalized(set) => {
                                    TransactionHistoryEntry {
                                        ledger_seq: data.header.ledger_seq,
                                        tx_set: TransactionSet {
                                            previous_ledger_hash: Hash([0u8; 32]),
                                            txs: Default::default(),
                                        },
                                        ext: TransactionHistoryEntryExt::V1(set.clone()),
                                    }
                                }
                            });
                    conn.store_tx_history_entry(data.header.ledger_seq.into(), &tx_history_entry)?;

                    let tx_result_entry = data.tx_result_entry.clone().unwrap_or_else(|| {
                        let results = data.tx_results.clone().try_into().unwrap_or_default();
                        TransactionHistoryResultEntry {
                            ledger_seq: data.header.ledger_seq,
                            tx_result_set: TransactionResultSet { results },
                            ext: TransactionHistoryResultEntryExt::default(),
                        }
                    });
                    conn.store_tx_result_entry(data.header.ledger_seq.into(), &tx_result_entry)?;

                    let tx_results: Vec<TransactionResultPair> = tx_result_entry
                        .tx_result_set
                        .results
                        .iter()
                        .cloned()
                        .collect();
                    let transactions = data
                        .tx_set
                        .transactions_with_base_fee()
                        .into_iter()
                        .map(|(tx, _)| tx)
                        .collect::<Vec<_>>();
                    let tx_count = transactions.len().min(tx_results.len());

                    for (idx, tx) in transactions.iter().take(tx_count).enumerate() {
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
                            ledger_seq: data.header.ledger_seq.into(),
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
            .map_err(|err| {
                HistoryError::CatchupFailed(format!("failed to persist history: {}", err))
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

                    conn.store_scp_history(ledger_seq.into(), &envelopes)?;

                    for qset in v0.quorum_sets.iter() {
                        let hash = Hash256::hash_xdr(qset)?;
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
        ledger_seq: LedgerSeq,
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
    pub(super) fn emit_meta(
        &self,
        ledger_seq: LedgerSeq,
        meta: stellar_xdr::curr::LedgerCloseMeta,
    ) {
        // Persist to SQLite first (to_xdr borrows &self on meta, no clone needed).
        match meta.to_xdr(stellar_xdr::curr::Limits::none()) {
            Ok(meta_xdr) => {
                if let Err(err) = self.db.store_ledger_close_meta(ledger_seq.get(), &meta_xdr) {
                    warn!(
                        error = %err,
                        ledger_seq = ledger_seq.get(),
                        "Failed to persist LedgerCloseMeta during catchup"
                    );
                }
            }
            Err(err) => {
                warn!(
                    error = %err,
                    ledger_seq = ledger_seq.get(),
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
