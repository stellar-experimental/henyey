//! Transaction history queries.
//!
//! This module provides database operations for transaction history, including:
//!
//! - Individual transaction records (`txhistory` table)
//! - Transaction sets per ledger (`txsets` table)
//! - Transaction results per ledger (`txresults` table)
//!
//! Transaction sets and results are used for history archive publishing
//! and catchup operations.

use henyey_common::xdr_stream::XdrOutputStream;
use rusqlite::{params, Connection, OptionalExtension};
use stellar_xdr::curr::{
    Limits, ReadXdr, TransactionHistoryEntry, TransactionHistoryResultEntry, WriteXdr,
};

use crate::error::DbError;

/// Transaction status values stored in the `status` column.
pub const TX_STATUS_SUCCESS: i32 = 0;
pub const TX_STATUS_FAILED: i32 = 1;

/// A stored transaction record.
///
/// Contains all the data needed to reconstruct a transaction's execution,
/// including the original transaction body, execution result, and metadata.
#[derive(Debug, Clone)]
pub struct TxRecord {
    /// The transaction ID (hash), hex-encoded.
    pub tx_id: String,
    /// The ledger sequence number where this transaction was included.
    pub ledger_seq: u32,
    /// The index of this transaction within the ledger's transaction set.
    pub tx_index: u32,
    /// The XDR-encoded transaction envelope.
    pub body: Vec<u8>,
    /// The XDR-encoded transaction result.
    pub result: Vec<u8>,
    /// The XDR-encoded transaction metadata (optional).
    ///
    /// Contains ledger entry changes and other execution details.
    pub meta: Option<Vec<u8>>,
    /// Transaction status: 0 = success, 1 = failed.
    pub status: i32,
}

/// Parameters for storing a transaction.
///
/// Groups the fields needed by [`HistoryQueries::store_transaction`] to avoid
/// a long parameter list.
pub struct StoreTxParams<'a> {
    pub ledger_seq: u32,
    pub tx_index: u32,
    pub tx_id: &'a str,
    pub body: &'a [u8],
    pub result: &'a [u8],
    pub meta: Option<&'a [u8]>,
    pub status: i32,
}

/// Query trait for transaction history operations.
///
/// Provides methods for storing and retrieving individual transactions
/// as well as per-ledger transaction sets and results.
pub trait HistoryQueries {
    /// Stores a transaction in the history table.
    ///
    /// If a transaction with the same ID already exists, it is replaced.
    /// `status` should be [`TX_STATUS_SUCCESS`] or [`TX_STATUS_FAILED`].
    fn store_transaction(&self, params: &StoreTxParams) -> Result<(), DbError>;

    /// Loads a transaction by its ID (hash).
    ///
    /// Returns `None` if the transaction is not found.
    fn load_transaction(&self, tx_id: &str) -> Result<Option<TxRecord>, DbError>;

    /// Stores a transaction history entry (transaction set) for a ledger.
    ///
    /// Used for history archive publishing. Contains all transactions
    /// that were applied in the ledger.
    fn store_tx_history_entry(
        &self,
        ledger_seq: u32,
        entry: &TransactionHistoryEntry,
    ) -> Result<(), DbError>;

    /// Loads a transaction history entry for a ledger.
    ///
    /// Returns `None` if no entry exists for the given ledger.
    fn load_tx_history_entry(
        &self,
        ledger_seq: u32,
    ) -> Result<Option<TransactionHistoryEntry>, DbError>;

    /// Stores transaction results for a ledger.
    ///
    /// Used for history archive publishing. Contains the execution
    /// results for all transactions in the ledger.
    fn store_tx_result_entry(
        &self,
        ledger_seq: u32,
        entry: &TransactionHistoryResultEntry,
    ) -> Result<(), DbError>;

    /// Loads transaction results for a ledger.
    ///
    /// Returns `None` if no entry exists for the given ledger.
    fn load_tx_result_entry(
        &self,
        ledger_seq: u32,
    ) -> Result<Option<TransactionHistoryResultEntry>, DbError>;

    /// Copy transaction history and results to XDR output streams.
    ///
    /// Writes `TransactionHistoryEntry` records to `tx_stream` and
    /// `TransactionHistoryResultEntry` records to `result_stream` for
    /// ledger sequences `[begin, begin + count)`.
    ///
    /// Returns `(tx_entries_written, result_entries_written)`.
    fn copy_tx_history_to_streams(
        &self,
        begin: u32,
        count: u32,
        tx_stream: &mut XdrOutputStream,
        result_stream: &mut XdrOutputStream,
    ) -> Result<(u32, u32), DbError>;

    /// Loads transactions in a ledger range, for cursor-based pagination.
    ///
    /// Returns transactions where `(ledgerseq, txindex) > (start_ledger, start_tx_index)`
    /// and `ledgerseq < end_ledger`, ordered by `(ledgerseq, txindex)` ascending,
    /// limited to `limit` rows.
    ///
    /// If `start_tx_index` is `None`, starts from the beginning of `start_ledger`.
    /// If `status_filter` is `Some`, only returns transactions with that status.
    fn load_transactions_in_range(
        &self,
        start_ledger: u32,
        start_tx_index: Option<u32>,
        end_ledger: u32,
        limit: u32,
        status_filter: Option<i32>,
    ) -> Result<Vec<TxRecord>, DbError>;

    /// Deletes old transaction history entries with `ledgerseq <= max_ledger`.
    ///
    /// Cleans `txhistory`, `txsets`, and `txresults` tables.
    /// Removes at most `count` entries from each table.
    /// Returns the total number of entries deleted across all tables.
    fn delete_old_tx_history(&self, max_ledger: u32, count: u32) -> Result<u32, DbError>;
}

impl HistoryQueries for Connection {
    fn store_transaction(&self, p: &StoreTxParams) -> Result<(), DbError> {
        self.execute(
            r#"
            INSERT OR REPLACE INTO txhistory
            (txid, ledgerseq, txindex, txbody, txresult, txmeta, status)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
            "#,
            params![
                p.tx_id,
                p.ledger_seq,
                p.tx_index,
                p.body,
                p.result,
                p.meta,
                p.status
            ],
        )?;
        Ok(())
    }

    fn load_transaction(&self, tx_id: &str) -> Result<Option<TxRecord>, DbError> {
        let result = self
            .query_row(
                r#"
                SELECT ledgerseq, txindex, txbody, txresult, txmeta, status
                FROM txhistory WHERE txid = ?1
                "#,
                params![tx_id],
                |row| {
                    Ok(TxRecord {
                        tx_id: tx_id.to_string(),
                        ledger_seq: row.get(0)?,
                        tx_index: row.get(1)?,
                        body: row.get(2)?,
                        result: row.get(3)?,
                        meta: row.get(4)?,
                        status: row.get(5)?,
                    })
                },
            )
            .optional()?;
        Ok(result)
    }

    fn store_tx_history_entry(
        &self,
        ledger_seq: u32,
        entry: &TransactionHistoryEntry,
    ) -> Result<(), DbError> {
        let data = entry.to_xdr(Limits::none())?;
        self.execute(
            "INSERT OR REPLACE INTO txsets (ledgerseq, data) VALUES (?1, ?2)",
            params![ledger_seq, data],
        )?;
        Ok(())
    }

    fn load_tx_history_entry(
        &self,
        ledger_seq: u32,
    ) -> Result<Option<TransactionHistoryEntry>, DbError> {
        let result: Option<Vec<u8>> = self
            .query_row(
                "SELECT data FROM txsets WHERE ledgerseq = ?1",
                params![ledger_seq],
                |row| row.get(0),
            )
            .optional()?;
        result
            .map(|data| {
                TransactionHistoryEntry::from_xdr(data.as_slice(), Limits::none())
                    .map_err(DbError::from)
            })
            .transpose()
    }

    fn store_tx_result_entry(
        &self,
        ledger_seq: u32,
        entry: &TransactionHistoryResultEntry,
    ) -> Result<(), DbError> {
        let data = entry.to_xdr(Limits::none())?;
        self.execute(
            "INSERT OR REPLACE INTO txresults (ledgerseq, data) VALUES (?1, ?2)",
            params![ledger_seq, data],
        )?;
        Ok(())
    }

    fn load_tx_result_entry(
        &self,
        ledger_seq: u32,
    ) -> Result<Option<TransactionHistoryResultEntry>, DbError> {
        let result: Option<Vec<u8>> = self
            .query_row(
                "SELECT data FROM txresults WHERE ledgerseq = ?1",
                params![ledger_seq],
                |row| row.get(0),
            )
            .optional()?;
        result
            .map(|data| {
                TransactionHistoryResultEntry::from_xdr(data.as_slice(), Limits::none())
                    .map_err(DbError::from)
            })
            .transpose()
    }

    fn copy_tx_history_to_streams(
        &self,
        begin: u32,
        count: u32,
        tx_stream: &mut XdrOutputStream,
        result_stream: &mut XdrOutputStream,
    ) -> Result<(u32, u32), DbError> {
        let end = begin.saturating_add(count);
        let mut tx_written = 0u32;
        let mut result_written = 0u32;

        // Stream transaction history entries
        {
            let mut stmt = self.prepare(
                "SELECT data FROM txsets WHERE ledgerseq >= ?1 AND ledgerseq < ?2 ORDER BY ledgerseq ASC",
            )?;
            let rows = stmt.query_map(params![begin, end], |row| row.get::<_, Vec<u8>>(0))?;
            for row in rows {
                let data = row?;
                let entry = TransactionHistoryEntry::from_xdr(data.as_slice(), Limits::none())?;
                tx_stream
                    .write_one(&entry)
                    .map_err(|e| DbError::Integrity(format!("Failed to write tx entry: {}", e)))?;
                tx_written += 1;
            }
        }

        // Stream transaction result entries
        {
            let mut stmt = self.prepare(
                "SELECT data FROM txresults WHERE ledgerseq >= ?1 AND ledgerseq < ?2 ORDER BY ledgerseq ASC",
            )?;
            let rows = stmt.query_map(params![begin, end], |row| row.get::<_, Vec<u8>>(0))?;
            for row in rows {
                let data = row?;
                let entry =
                    TransactionHistoryResultEntry::from_xdr(data.as_slice(), Limits::none())?;
                result_stream.write_one(&entry).map_err(|e| {
                    DbError::Integrity(format!("Failed to write result entry: {}", e))
                })?;
                result_written += 1;
            }
        }

        Ok((tx_written, result_written))
    }

    fn load_transactions_in_range(
        &self,
        start_ledger: u32,
        start_tx_index: Option<u32>,
        end_ledger: u32,
        limit: u32,
        status_filter: Option<i32>,
    ) -> Result<Vec<TxRecord>, DbError> {
        let map_row = |row: &rusqlite::Row<'_>| {
            Ok(TxRecord {
                tx_id: row.get(0)?,
                ledger_seq: row.get(1)?,
                tx_index: row.get(2)?,
                body: row.get(3)?,
                result: row.get(4)?,
                meta: row.get(5)?,
                status: row.get(6)?,
            })
        };

        match (start_tx_index, status_filter) {
            (Some(tx_index), Some(status)) => {
                let mut stmt = self.prepare(
                    "SELECT txid, ledgerseq, txindex, txbody, txresult, txmeta, status FROM txhistory \
                     WHERE (ledgerseq > ?1 OR (ledgerseq = ?2 AND txindex > ?3)) \
                     AND ledgerseq < ?4 AND status = ?5 \
                     ORDER BY ledgerseq ASC, txindex ASC LIMIT ?6",
                )?;
                let rows = stmt.query_map(
                    params![
                        start_ledger,
                        start_ledger,
                        tx_index,
                        end_ledger,
                        status,
                        limit
                    ],
                    map_row,
                )?;
                rows.collect::<std::result::Result<Vec<_>, _>>()
                    .map_err(DbError::from)
            }
            (Some(tx_index), None) => {
                let mut stmt = self.prepare(
                    "SELECT txid, ledgerseq, txindex, txbody, txresult, txmeta, status FROM txhistory \
                     WHERE (ledgerseq > ?1 OR (ledgerseq = ?2 AND txindex > ?3)) \
                     AND ledgerseq < ?4 \
                     ORDER BY ledgerseq ASC, txindex ASC LIMIT ?5",
                )?;
                let rows = stmt.query_map(
                    params![start_ledger, start_ledger, tx_index, end_ledger, limit],
                    map_row,
                )?;
                rows.collect::<std::result::Result<Vec<_>, _>>()
                    .map_err(DbError::from)
            }
            (None, Some(status)) => {
                let mut stmt = self.prepare(
                    "SELECT txid, ledgerseq, txindex, txbody, txresult, txmeta, status FROM txhistory \
                     WHERE ledgerseq >= ?1 AND ledgerseq < ?2 AND status = ?3 \
                     ORDER BY ledgerseq ASC, txindex ASC LIMIT ?4",
                )?;
                let rows =
                    stmt.query_map(params![start_ledger, end_ledger, status, limit], map_row)?;
                rows.collect::<std::result::Result<Vec<_>, _>>()
                    .map_err(DbError::from)
            }
            (None, None) => {
                let mut stmt = self.prepare(
                    "SELECT txid, ledgerseq, txindex, txbody, txresult, txmeta, status FROM txhistory \
                     WHERE ledgerseq >= ?1 AND ledgerseq < ?2 \
                     ORDER BY ledgerseq ASC, txindex ASC LIMIT ?3",
                )?;
                let rows = stmt.query_map(params![start_ledger, end_ledger, limit], map_row)?;
                rows.collect::<std::result::Result<Vec<_>, _>>()
                    .map_err(DbError::from)
            }
        }
    }

    fn delete_old_tx_history(&self, max_ledger: u32, count: u32) -> Result<u32, DbError> {
        let mut total = 0u32;

        // Delete from txhistory
        let deleted = self.execute(
            "DELETE FROM txhistory WHERE rowid IN (\
                SELECT rowid FROM txhistory WHERE ledgerseq <= ?1 \
                ORDER BY ledgerseq ASC LIMIT ?2\
            )",
            params![max_ledger, count],
        )?;
        total += deleted as u32;

        // Delete from txsets
        let deleted = self.execute(
            "DELETE FROM txsets WHERE ledgerseq IN (\
                SELECT ledgerseq FROM txsets WHERE ledgerseq <= ?1 \
                ORDER BY ledgerseq ASC LIMIT ?2\
            )",
            params![max_ledger, count],
        )?;
        total += deleted as u32;

        // Delete from txresults
        let deleted = self.execute(
            "DELETE FROM txresults WHERE ledgerseq IN (\
                SELECT ledgerseq FROM txresults WHERE ledgerseq <= ?1 \
                ORDER BY ledgerseq ASC LIMIT ?2\
            )",
            params![max_ledger, count],
        )?;
        total += deleted as u32;

        Ok(total)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;
    use stellar_xdr::curr::{
        Hash, TransactionHistoryEntryExt, TransactionHistoryResultEntryExt, TransactionResultSet,
        TransactionSet, VecM,
    };

    fn setup_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            r#"
            CREATE TABLE txhistory (
                txid TEXT PRIMARY KEY,
                ledgerseq INTEGER NOT NULL,
                txindex INTEGER NOT NULL,
                txbody BLOB NOT NULL,
                txresult BLOB NOT NULL,
                txmeta BLOB,
                status INTEGER NOT NULL DEFAULT 0
            );
            CREATE INDEX txhistory_ledger ON txhistory(ledgerseq);
            CREATE TABLE txsets (
                ledgerseq INTEGER PRIMARY KEY,
                data BLOB NOT NULL
            );
            CREATE TABLE txresults (
                ledgerseq INTEGER PRIMARY KEY,
                data BLOB NOT NULL
            );
            "#,
        )
        .unwrap();
        conn
    }

    #[test]
    fn test_store_and_load_transaction() {
        let conn = setup_db();
        let tx_id = "abc123def456";
        let body = b"transaction body";
        let result = b"transaction result";
        let meta = b"transaction meta";

        conn.store_transaction(&StoreTxParams {
            ledger_seq: 100,
            tx_index: 0,
            tx_id,
            body,
            result,
            meta: Some(meta),
            status: TX_STATUS_SUCCESS,
        })
        .unwrap();

        let loaded = conn.load_transaction(tx_id).unwrap().unwrap();
        assert_eq!(loaded.tx_id, tx_id);
        assert_eq!(loaded.ledger_seq, 100);
        assert_eq!(loaded.tx_index, 0);
        assert_eq!(loaded.body, body.to_vec());
        assert_eq!(loaded.result, result.to_vec());
        assert_eq!(loaded.meta, Some(meta.to_vec()));
        assert_eq!(loaded.status, TX_STATUS_SUCCESS);
    }

    #[test]
    fn test_store_transaction_without_meta() {
        let conn = setup_db();
        let tx_id = "xyz789";
        let body = b"body";
        let result = b"result";

        conn.store_transaction(&StoreTxParams {
            ledger_seq: 200,
            tx_index: 5,
            tx_id,
            body,
            result,
            meta: None,
            status: TX_STATUS_FAILED,
        })
        .unwrap();

        let loaded = conn.load_transaction(tx_id).unwrap().unwrap();
        assert_eq!(loaded.ledger_seq, 200);
        assert_eq!(loaded.tx_index, 5);
        assert!(loaded.meta.is_none());
        assert_eq!(loaded.status, TX_STATUS_FAILED);
    }

    #[test]
    fn test_load_nonexistent_transaction() {
        let conn = setup_db();
        assert!(conn.load_transaction("nonexistent").unwrap().is_none());
    }

    #[test]
    fn test_update_transaction() {
        let conn = setup_db();
        let tx_id = "update_test";

        // Store initial version
        conn.store_transaction(&StoreTxParams {
            ledger_seq: 100,
            tx_index: 0,
            tx_id,
            body: b"old_body",
            result: b"old_result",
            meta: None,
            status: TX_STATUS_SUCCESS,
        })
        .unwrap();

        // Update with new data
        conn.store_transaction(&StoreTxParams {
            ledger_seq: 100,
            tx_index: 0,
            tx_id,
            body: b"new_body",
            result: b"new_result",
            meta: Some(b"meta"),
            status: TX_STATUS_FAILED,
        })
        .unwrap();

        let loaded = conn.load_transaction(tx_id).unwrap().unwrap();
        assert_eq!(loaded.body, b"new_body".to_vec());
        assert_eq!(loaded.result, b"new_result".to_vec());
        assert_eq!(loaded.meta, Some(b"meta".to_vec()));
        assert_eq!(loaded.status, TX_STATUS_FAILED);
    }

    #[test]
    fn test_copy_tx_history_to_streams() {
        let conn = setup_db();

        // Store tx history and result entries for ledgers 100-102
        for seq in 100..=102 {
            let tx_entry = TransactionHistoryEntry {
                ledger_seq: seq,
                tx_set: TransactionSet {
                    previous_ledger_hash: Hash::default(),
                    txs: VecM::default(),
                },
                ext: TransactionHistoryEntryExt::V0,
            };
            conn.store_tx_history_entry(seq, &tx_entry).unwrap();

            let result_entry = TransactionHistoryResultEntry {
                ledger_seq: seq,
                tx_result_set: TransactionResultSet {
                    results: VecM::default(),
                },
                ext: TransactionHistoryResultEntryExt::V0,
            };
            conn.store_tx_result_entry(seq, &result_entry).unwrap();
        }

        // Create two streams
        struct SharedBuf(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
        impl std::io::Write for SharedBuf {
            fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(data);
                Ok(data.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let tx_buf = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
        let result_buf = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));

        let mut tx_stream = XdrOutputStream::from_writer(Box::new(SharedBuf(tx_buf.clone())));
        let mut result_stream =
            XdrOutputStream::from_writer(Box::new(SharedBuf(result_buf.clone())));

        let (tx_written, result_written) = conn
            .copy_tx_history_to_streams(100, 3, &mut tx_stream, &mut result_stream)
            .unwrap();

        assert_eq!(tx_written, 3);
        assert_eq!(result_written, 3);

        // Verify data was written
        assert!(!tx_buf.lock().unwrap().is_empty());
        assert!(!result_buf.lock().unwrap().is_empty());
    }

    #[test]
    fn test_copy_tx_history_to_streams_readback() {
        let conn = setup_db();

        // Store entries for ledgers 100-102
        for seq in 100..=102 {
            let tx_entry = TransactionHistoryEntry {
                ledger_seq: seq,
                tx_set: TransactionSet {
                    previous_ledger_hash: Hash::default(),
                    txs: VecM::default(),
                },
                ext: TransactionHistoryEntryExt::V0,
            };
            conn.store_tx_history_entry(seq, &tx_entry).unwrap();

            let result_entry = TransactionHistoryResultEntry {
                ledger_seq: seq,
                tx_result_set: TransactionResultSet {
                    results: VecM::default(),
                },
                ext: TransactionHistoryResultEntryExt::V0,
            };
            conn.store_tx_result_entry(seq, &result_entry).unwrap();
        }

        struct SharedBufRB(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
        impl std::io::Write for SharedBufRB {
            fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(data);
                Ok(data.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let tx_buf = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
        let result_buf = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));

        let mut tx_stream = XdrOutputStream::from_writer(Box::new(SharedBufRB(tx_buf.clone())));
        let mut result_stream =
            XdrOutputStream::from_writer(Box::new(SharedBufRB(result_buf.clone())));

        let (tx_written, result_written) = conn
            .copy_tx_history_to_streams(100, 3, &mut tx_stream, &mut result_stream)
            .unwrap();

        assert_eq!(tx_written, 3);
        assert_eq!(result_written, 3);

        // Read back tx entries
        let tx_data = tx_buf.lock().unwrap().clone();
        let cursor = std::io::Cursor::new(tx_data);
        let mut input = henyey_common::xdr_stream::XdrInputStream::from_reader(Box::new(cursor));
        let tx_entries: Vec<TransactionHistoryEntry> = input.read_all().unwrap();
        assert_eq!(tx_entries.len(), 3);
        assert_eq!(tx_entries[0].ledger_seq, 100);
        assert_eq!(tx_entries[1].ledger_seq, 101);
        assert_eq!(tx_entries[2].ledger_seq, 102);

        // Read back result entries
        let result_data = result_buf.lock().unwrap().clone();
        let cursor = std::io::Cursor::new(result_data);
        let mut input = henyey_common::xdr_stream::XdrInputStream::from_reader(Box::new(cursor));
        let result_entries: Vec<TransactionHistoryResultEntry> = input.read_all().unwrap();
        assert_eq!(result_entries.len(), 3);
        assert_eq!(result_entries[0].ledger_seq, 100);
        assert_eq!(result_entries[2].ledger_seq, 102);
    }

    #[test]
    fn test_copy_tx_history_to_streams_partial() {
        let conn = setup_db();

        // Store entries only for ledgers 100 and 102 (gap at 101)
        for seq in [100, 102] {
            let tx_entry = TransactionHistoryEntry {
                ledger_seq: seq,
                tx_set: TransactionSet {
                    previous_ledger_hash: Hash::default(),
                    txs: VecM::default(),
                },
                ext: TransactionHistoryEntryExt::V0,
            };
            conn.store_tx_history_entry(seq, &tx_entry).unwrap();

            let result_entry = TransactionHistoryResultEntry {
                ledger_seq: seq,
                tx_result_set: TransactionResultSet {
                    results: VecM::default(),
                },
                ext: TransactionHistoryResultEntryExt::V0,
            };
            conn.store_tx_result_entry(seq, &result_entry).unwrap();
        }

        struct SharedBufP(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
        impl std::io::Write for SharedBufP {
            fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(data);
                Ok(data.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let tx_buf = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
        let result_buf = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));

        let mut tx_stream = XdrOutputStream::from_writer(Box::new(SharedBufP(tx_buf.clone())));
        let mut result_stream =
            XdrOutputStream::from_writer(Box::new(SharedBufP(result_buf.clone())));

        let (tx_written, result_written) = conn
            .copy_tx_history_to_streams(100, 5, &mut tx_stream, &mut result_stream)
            .unwrap();

        // Only 2 entries exist even though we asked for 5
        assert_eq!(tx_written, 2);
        assert_eq!(result_written, 2);
    }

    #[test]
    fn test_copy_tx_history_to_streams_empty_range() {
        let conn = setup_db();

        struct SharedBuf2(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
        impl std::io::Write for SharedBuf2 {
            fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(data);
                Ok(data.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let tx_buf = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
        let result_buf = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));

        let mut tx_stream = XdrOutputStream::from_writer(Box::new(SharedBuf2(tx_buf.clone())));
        let mut result_stream =
            XdrOutputStream::from_writer(Box::new(SharedBuf2(result_buf.clone())));

        let (tx_written, result_written) = conn
            .copy_tx_history_to_streams(100, 3, &mut tx_stream, &mut result_stream)
            .unwrap();

        assert_eq!(tx_written, 0);
        assert_eq!(result_written, 0);
    }

    #[test]
    fn test_store_and_load_tx_history_entry() {
        let conn = setup_db();
        let entry = TransactionHistoryEntry {
            ledger_seq: 123,
            tx_set: TransactionSet {
                previous_ledger_hash: Hash::default(),
                txs: VecM::default(),
            },
            ext: TransactionHistoryEntryExt::V0,
        };

        conn.store_tx_history_entry(123, &entry).unwrap();
        let loaded = conn.load_tx_history_entry(123).unwrap().unwrap();
        assert_eq!(loaded.ledger_seq, 123);
        assert_eq!(loaded.tx_set, entry.tx_set);
        assert_eq!(loaded.ext, entry.ext);
    }

    #[test]
    fn test_store_and_load_tx_result_entry() {
        let conn = setup_db();
        let entry = TransactionHistoryResultEntry {
            ledger_seq: 456,
            tx_result_set: TransactionResultSet {
                results: VecM::default(),
            },
            ext: TransactionHistoryResultEntryExt::V0,
        };

        conn.store_tx_result_entry(456, &entry).unwrap();
        let loaded = conn.load_tx_result_entry(456).unwrap().unwrap();
        assert_eq!(loaded.ledger_seq, 456);
        assert_eq!(loaded.tx_result_set, entry.tx_result_set);
        assert_eq!(loaded.ext, entry.ext);
    }
}
