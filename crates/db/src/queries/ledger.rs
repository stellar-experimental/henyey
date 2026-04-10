//! Ledger header queries.
//!
//! This module provides database operations for ledger headers, which contain
//! the metadata for each closed ledger including sequence numbers, timestamps,
//! hashes, and protocol version information.

use henyey_common::xdr_stream::XdrOutputStream;
use henyey_common::Hash256;
use henyey_common::LedgerSeq;
use rusqlite::{params, Connection, OptionalExtension};
use stellar_xdr::curr::{LedgerHeader, LedgerHeaderHistoryEntry, Limits, ReadXdr};

use crate::error::DbError;

/// Query trait for ledger header operations.
///
/// Provides methods for storing and retrieving ledger headers from the
/// `ledgerheaders` table. Headers are stored as XDR blobs along with
/// indexed columns for efficient querying.
pub trait LedgerQueries {
    /// Loads a ledger header by its sequence number.
    ///
    /// Returns `None` if no ledger with the given sequence exists.
    fn load_ledger_header(&self, seq: u32) -> Result<Option<LedgerHeader>, DbError>;

    /// Stores a ledger header in the database.
    ///
    /// The raw XDR data is stored along with extracted fields for indexing.
    /// If a header with the same sequence already exists, it is replaced.
    fn store_ledger_header(&self, header: &LedgerHeader, data: &[u8]) -> Result<(), DbError>;

    /// Returns the highest ledger sequence number in the database.
    ///
    /// Returns `None` if no ledgers have been stored.
    fn get_latest_ledger_seq(&self) -> Result<Option<u32>, DbError>;

    /// Returns the hash of a ledger by its sequence number.
    ///
    /// The hash is computed from the XDR-encoded header data.
    /// Returns `None` if the ledger is not found.
    fn get_ledger_hash(&self, seq: u32) -> Result<Option<Hash256>, DbError>;

    /// Loads a ledger header by its hash (hex-encoded).
    ///
    /// Returns `None` if no ledger with the given hash is found.
    fn load_ledger_header_by_hash(&self, hash: &str) -> Result<Option<LedgerHeader>, DbError>;

    /// Copy ledger headers from the database to an XDR output stream.
    ///
    /// Writes `LedgerHeaderHistoryEntry` records for ledger sequences
    /// `[begin, begin + count)` to the stream. Returns the number of
    /// headers actually written (may be less than `count` if some ledgers
    /// are missing from the database).
    fn copy_ledger_headers_to_stream(
        &self,
        begin: u32,
        count: u32,
        stream: &mut XdrOutputStream,
    ) -> Result<u32, DbError>;

    /// Deletes old ledger headers up to and including `max_ledger`.
    ///
    /// Removes at most `count` entries to limit the amount of work per call.
    /// Returns the number of entries actually deleted.
    ///
    /// This is used by the Maintainer to garbage collect old ledger history.
    fn delete_old_ledger_headers(&self, max_ledger: LedgerSeq, count: u32) -> Result<u32, DbError>;

    /// Returns the lowest ledger sequence number in the database.
    ///
    /// Returns `None` if no ledgers have been stored.
    fn get_oldest_ledger_seq(&self) -> Result<Option<u32>, DbError>;
}

impl LedgerQueries for Connection {
    fn load_ledger_header(&self, seq: u32) -> Result<Option<LedgerHeader>, DbError> {
        let result: Option<Vec<u8>> = self
            .query_row(
                "SELECT data FROM ledgerheaders WHERE ledgerseq = ?1",
                params![seq],
                |row| row.get(0),
            )
            .optional()?;

        result
            .map(|data| LedgerHeader::from_xdr(&data, Limits::none()).map_err(DbError::from))
            .transpose()
    }

    fn store_ledger_header(&self, header: &LedgerHeader, data: &[u8]) -> Result<(), DbError> {
        let ledger_hash = Hash256::hash(data);
        let prev_hash = Hash256::from_bytes(header.previous_ledger_hash.0);
        let bucket_list_hash = Hash256::from_bytes(header.bucket_list_hash.0);

        self.execute(
            r#"
            INSERT OR REPLACE INTO ledgerheaders
            (ledgerhash, prevhash, bucketlisthash, ledgerseq, closetime, data)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6)
            "#,
            params![
                ledger_hash.to_hex(),
                prev_hash.to_hex(),
                bucket_list_hash.to_hex(),
                header.ledger_seq,
                header.scp_value.close_time.0,
                data,
            ],
        )?;
        Ok(())
    }

    fn get_latest_ledger_seq(&self) -> Result<Option<u32>, DbError> {
        // MAX() returns NULL when the table is empty, so we get the value optionally
        let result: Option<Option<u32>> = self
            .query_row("SELECT MAX(ledgerseq) FROM ledgerheaders", [], |row| {
                row.get::<_, Option<u32>>(0)
            })
            .optional()?;
        Ok(result.flatten())
    }

    fn get_ledger_hash(&self, seq: u32) -> Result<Option<Hash256>, DbError> {
        let result: Option<String> = self
            .query_row(
                "SELECT ledgerhash FROM ledgerheaders WHERE ledgerseq = ?1",
                params![seq],
                |row| row.get(0),
            )
            .optional()?;

        result
            .map(|hex| {
                Hash256::from_hex(&hex)
                    .map_err(|e| DbError::Integrity(format!("Invalid ledger hash: {}", e)))
            })
            .transpose()
    }

    fn load_ledger_header_by_hash(&self, hash: &str) -> Result<Option<LedgerHeader>, DbError> {
        let result: Option<Vec<u8>> = self
            .query_row(
                "SELECT data FROM ledgerheaders WHERE ledgerhash = ?1",
                params![hash],
                |row| row.get(0),
            )
            .optional()?;

        result
            .map(|data| LedgerHeader::from_xdr(&data, Limits::none()).map_err(DbError::from))
            .transpose()
    }

    fn copy_ledger_headers_to_stream(
        &self,
        begin: u32,
        count: u32,
        stream: &mut XdrOutputStream,
    ) -> Result<u32, DbError> {
        let end = begin.saturating_add(count);
        let mut stmt = self.prepare(
            "SELECT ledgerseq, ledgerhash, data FROM ledgerheaders WHERE ledgerseq >= ?1 AND ledgerseq < ?2 ORDER BY ledgerseq ASC",
        )?;
        let rows = stmt.query_map(params![begin, end], |row| {
            let seq: u32 = row.get(0)?;
            let hash_hex: String = row.get(1)?;
            let data: Vec<u8> = row.get(2)?;
            Ok((seq, hash_hex, data))
        })?;

        let mut written = 0u32;
        for row in rows {
            let (_seq, hash_hex, data) = row?;
            let header = LedgerHeader::from_xdr(&data, Limits::none())?;
            let hash_bytes = Hash256::from_hex(&hash_hex)
                .map_err(|e| DbError::Integrity(format!("Invalid ledger hash: {}", e)))?;
            let entry = LedgerHeaderHistoryEntry {
                hash: stellar_xdr::curr::Hash(hash_bytes.0),
                header,
                ext: stellar_xdr::curr::LedgerHeaderHistoryEntryExt::V0,
            };
            stream
                .write_one(&entry)
                .map_err(|e| DbError::Integrity(format!("Failed to write header: {}", e)))?;
            written += 1;
        }

        Ok(written)
    }

    fn delete_old_ledger_headers(&self, max_ledger: LedgerSeq, count: u32) -> Result<u32, DbError> {
        // Delete up to `count` entries with ledgerseq <= max_ledger
        // Use a subquery to find the entries to delete (SQLite doesn't support LIMIT in DELETE)
        let deleted = self.execute(
            r#"
            DELETE FROM ledgerheaders
            WHERE ledgerseq IN (
                SELECT ledgerseq FROM ledgerheaders
                WHERE ledgerseq <= ?1
                ORDER BY ledgerseq ASC
                LIMIT ?2
            )
            "#,
            params![max_ledger, count],
        )?;
        Ok(deleted as u32)
    }

    fn get_oldest_ledger_seq(&self) -> Result<Option<u32>, DbError> {
        let result: Option<Option<u32>> = self
            .query_row("SELECT MIN(ledgerseq) FROM ledgerheaders", [], |row| {
                row.get::<_, Option<u32>>(0)
            })
            .optional()?;
        Ok(result.flatten())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;
    use stellar_xdr::curr::{
        Hash, LedgerHeader, LedgerHeaderExt, StellarValue, StellarValueExt, TimePoint, WriteXdr,
    };

    fn setup_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            r#"
            CREATE TABLE ledgerheaders (
                ledgerhash TEXT PRIMARY KEY,
                prevhash TEXT NOT NULL,
                bucketlisthash TEXT NOT NULL,
                ledgerseq INTEGER UNIQUE NOT NULL,
                closetime INTEGER NOT NULL,
                data BLOB NOT NULL
            );
            CREATE INDEX ledgerheaders_seq ON ledgerheaders(ledgerseq);
            "#,
        )
        .unwrap();
        conn
    }

    fn create_test_header(seq: u32) -> LedgerHeader {
        LedgerHeader {
            ledger_version: 20,
            previous_ledger_hash: Hash([0u8; 32]),
            scp_value: StellarValue {
                tx_set_hash: Hash([0u8; 32]),
                close_time: TimePoint(1234567890),
                upgrades: vec![].try_into().unwrap(),
                ext: StellarValueExt::Basic,
            },
            tx_set_result_hash: Hash([0u8; 32]),
            bucket_list_hash: Hash([1u8; 32]),
            ledger_seq: seq,
            total_coins: 100_000_000_000_000_000,
            fee_pool: 0,
            inflation_seq: 0,
            id_pool: 0,
            base_fee: 100,
            base_reserve: 5_000_000,
            max_tx_set_size: 100,
            skip_list: std::array::from_fn(|_| Hash([0u8; 32])),
            ext: LedgerHeaderExt::V0,
        }
    }

    #[test]
    fn test_store_and_load_ledger_header() {
        let conn = setup_db();
        let header = create_test_header(100);
        let data = header.to_xdr(Limits::none()).unwrap();

        conn.store_ledger_header(&header, &data).unwrap();

        let loaded = conn.load_ledger_header(100).unwrap().unwrap();
        assert_eq!(loaded.ledger_seq, 100);
        assert_eq!(loaded.base_fee, 100);
    }

    #[test]
    fn test_get_latest_ledger_seq() {
        let conn = setup_db();

        // Initially no ledgers
        assert!(conn.get_latest_ledger_seq().unwrap().is_none());

        // Add some ledgers
        for seq in [10, 20, 15] {
            let header = create_test_header(seq);
            let data = header.to_xdr(Limits::none()).unwrap();
            conn.store_ledger_header(&header, &data).unwrap();
        }

        assert_eq!(conn.get_latest_ledger_seq().unwrap(), Some(20));
    }

    #[test]
    fn test_get_ledger_hash() {
        let conn = setup_db();
        let header = create_test_header(100);
        let data = header.to_xdr(Limits::none()).unwrap();

        conn.store_ledger_header(&header, &data).unwrap();

        let hash = conn.get_ledger_hash(100).unwrap().unwrap();
        assert!(!hash.is_zero());

        // Non-existent ledger
        assert!(conn.get_ledger_hash(999).unwrap().is_none());
    }

    #[test]
    fn test_copy_ledger_headers_to_stream() {
        let conn = setup_db();

        // Store 5 ledger headers (seq 10-14)
        for seq in 10..=14 {
            let header = create_test_header(seq);
            let data = header.to_xdr(Limits::none()).unwrap();
            conn.store_ledger_header(&header, &data).unwrap();
        }

        // Write to an in-memory XDR stream
        let buf = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
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

        let writer = SharedBuf(buf.clone());
        let mut stream = XdrOutputStream::from_writer(Box::new(writer));

        let written = conn
            .copy_ledger_headers_to_stream(10, 5, &mut stream)
            .unwrap();
        assert_eq!(written, 5);

        // Verify we can read them back
        let data = buf.lock().unwrap().clone();
        assert!(!data.is_empty());

        // Read back with XdrInputStream
        let cursor = std::io::Cursor::new(data);
        let mut input = henyey_common::xdr_stream::XdrInputStream::from_reader(Box::new(cursor));
        let entries: Vec<LedgerHeaderHistoryEntry> = input.read_all().unwrap();
        assert_eq!(entries.len(), 5);
        assert_eq!(entries[0].header.ledger_seq, 10);
        assert_eq!(entries[4].header.ledger_seq, 14);
    }

    #[test]
    fn test_copy_ledger_headers_to_stream_partial_range() {
        let conn = setup_db();

        // Store only ledgers 10 and 12 (skip 11)
        for seq in [10, 12] {
            let header = create_test_header(seq);
            let data = header.to_xdr(Limits::none()).unwrap();
            conn.store_ledger_header(&header, &data).unwrap();
        }

        let buf = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
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

        let writer = SharedBuf2(buf.clone());
        let mut stream = XdrOutputStream::from_writer(Box::new(writer));

        // Request 5 ledgers starting at 10, but only 2 exist
        let written = conn
            .copy_ledger_headers_to_stream(10, 5, &mut stream)
            .unwrap();
        assert_eq!(written, 2);
    }

    #[test]
    fn test_load_ledger_header_by_hash_found() {
        let conn = setup_db();
        let header = create_test_header(100);
        let data = header.to_xdr(Limits::none()).unwrap();
        let hash = Hash256::hash(&data);

        conn.store_ledger_header(&header, &data).unwrap();

        let loaded = conn
            .load_ledger_header_by_hash(&hash.to_hex())
            .unwrap()
            .unwrap();
        assert_eq!(loaded.ledger_seq, 100);
    }

    #[test]
    fn test_load_ledger_header_by_hash_not_found() {
        let conn = setup_db();
        let result = conn
            .load_ledger_header_by_hash(
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            )
            .unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_delete_old_ledger_headers() {
        let conn = setup_db();

        // Add ledgers 1-10
        for seq in 1..=10 {
            let header = create_test_header(seq);
            let data = header.to_xdr(Limits::none()).unwrap();
            conn.store_ledger_header(&header, &data).unwrap();
        }

        // Delete ledgers up to 5, but only 3 at a time
        let deleted = conn.delete_old_ledger_headers(5.into(), 3).unwrap();
        assert_eq!(deleted, 3);

        // Delete more
        let deleted = conn.delete_old_ledger_headers(5.into(), 10).unwrap();
        assert_eq!(deleted, 2); // Only 2 remaining under threshold

        // Verify ledgers 6-10 remain
        for seq in 6..=10 {
            assert!(conn.load_ledger_header(seq).unwrap().is_some());
        }

        // Verify ledgers 1-5 are gone
        for seq in 1..=5 {
            assert!(conn.load_ledger_header(seq).unwrap().is_none());
        }
    }
}
