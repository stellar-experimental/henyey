//! Ledger close metadata queries.
//!
//! This module provides database operations for full `LedgerCloseMeta` blobs,
//! used by the `getTransactions` and `getLedgers` RPC endpoints.
//! The data is stored as raw XDR and cleaned up by the Maintainer using the
//! RPC retention window.

use henyey_common::LedgerSeq;
use rusqlite::{params, Connection};

use crate::error::DbError;

/// Query trait for ledger close metadata operations.
pub trait LedgerCloseMetaQueries {
    /// Stores a serialized `LedgerCloseMeta` for a ledger.
    ///
    /// If an entry for this sequence already exists, it is replaced.
    fn store_ledger_close_meta(&self, sequence: u32, meta: &[u8]) -> Result<(), DbError>;

    /// Loads the serialized `LedgerCloseMeta` for a single ledger.
    ///
    /// Returns `None` if no entry exists for the given sequence.
    fn load_ledger_close_meta(&self, sequence: u32) -> Result<Option<Vec<u8>>, DbError>;

    /// Loads serialized `LedgerCloseMeta` blobs for a range of ledgers.
    ///
    /// Returns `(sequence, meta_bytes)` pairs ordered by sequence ascending,
    /// for ledgers in `[start_sequence, end_sequence)`.
    fn load_ledger_close_metas_in_range(
        &self,
        start_sequence: u32,
        end_sequence: u32,
        limit: u32,
    ) -> Result<Vec<(u32, Vec<u8>)>, DbError>;

    /// Deletes old ledger close metadata entries with `sequence <= max_ledger`.
    ///
    /// Removes at most `count` entries to limit the amount of work per call.
    /// Returns the number of entries actually deleted.
    fn delete_old_ledger_close_meta(
        &self,
        max_ledger: LedgerSeq,
        count: u32,
    ) -> Result<u32, DbError>;
}

impl LedgerCloseMetaQueries for Connection {
    fn store_ledger_close_meta(&self, sequence: u32, meta: &[u8]) -> Result<(), DbError> {
        self.execute(
            "INSERT OR REPLACE INTO ledger_close_meta (sequence, meta) VALUES (?1, ?2)",
            params![sequence, meta],
        )?;
        Ok(())
    }

    fn load_ledger_close_meta(&self, sequence: u32) -> Result<Option<Vec<u8>>, DbError> {
        use rusqlite::OptionalExtension;
        let result = self
            .query_row(
                "SELECT meta FROM ledger_close_meta WHERE sequence = ?1",
                params![sequence],
                |row| row.get(0),
            )
            .optional()?;
        Ok(result)
    }

    fn load_ledger_close_metas_in_range(
        &self,
        start_sequence: u32,
        end_sequence: u32,
        limit: u32,
    ) -> Result<Vec<(u32, Vec<u8>)>, DbError> {
        let mut stmt = self.prepare(
            "SELECT sequence, meta FROM ledger_close_meta \
             WHERE sequence >= ?1 AND sequence < ?2 \
             ORDER BY sequence ASC LIMIT ?3",
        )?;
        let rows = stmt.query_map(params![start_sequence, end_sequence, limit], |row| {
            Ok((row.get::<_, u32>(0)?, row.get::<_, Vec<u8>>(1)?))
        })?;
        let results: Result<Vec<_>, _> = rows.collect();
        Ok(results?)
    }

    fn delete_old_ledger_close_meta(
        &self,
        max_ledger: LedgerSeq,
        count: u32,
    ) -> Result<u32, DbError> {
        let deleted = self.execute(
            "DELETE FROM ledger_close_meta WHERE sequence IN (\
                SELECT sequence FROM ledger_close_meta \
                WHERE sequence <= ?1 \
                ORDER BY sequence ASC LIMIT ?2\
            )",
            params![max_ledger, count],
        )?;
        Ok(deleted as u32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    fn setup_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE ledger_close_meta (
                sequence INTEGER PRIMARY KEY,
                meta BLOB NOT NULL
            );",
        )
        .unwrap();
        conn
    }

    #[test]
    fn test_store_and_load() {
        let conn = setup_db();
        let meta = b"test-lcm-data";
        conn.store_ledger_close_meta(100, meta).unwrap();

        let loaded = conn.load_ledger_close_meta(100).unwrap().unwrap();
        assert_eq!(loaded, meta.to_vec());
    }

    #[test]
    fn test_load_nonexistent() {
        let conn = setup_db();
        assert!(conn.load_ledger_close_meta(999).unwrap().is_none());
    }

    #[test]
    fn test_store_replace() {
        let conn = setup_db();
        conn.store_ledger_close_meta(100, b"old").unwrap();
        conn.store_ledger_close_meta(100, b"new").unwrap();

        let loaded = conn.load_ledger_close_meta(100).unwrap().unwrap();
        assert_eq!(loaded, b"new".to_vec());
    }

    #[test]
    fn test_load_range() {
        let conn = setup_db();
        for seq in 100..110 {
            conn.store_ledger_close_meta(seq, format!("meta-{}", seq).as_bytes())
                .unwrap();
        }

        // Load [102, 107) with limit 10
        let results = conn.load_ledger_close_metas_in_range(102, 107, 10).unwrap();
        assert_eq!(results.len(), 5);
        assert_eq!(results[0].0, 102);
        assert_eq!(results[4].0, 106);

        // Load with limit smaller than range
        let results = conn.load_ledger_close_metas_in_range(100, 110, 3).unwrap();
        assert_eq!(results.len(), 3);
        assert_eq!(results[0].0, 100);
        assert_eq!(results[2].0, 102);
    }

    #[test]
    fn test_delete_old() {
        let conn = setup_db();
        for seq in 1..=10 {
            conn.store_ledger_close_meta(seq, b"data").unwrap();
        }

        // Delete up to seq 5, but only 3 at a time
        let deleted = conn.delete_old_ledger_close_meta(5.into(), 3).unwrap();
        assert_eq!(deleted, 3);

        // Delete remaining
        let deleted = conn.delete_old_ledger_close_meta(5.into(), 10).unwrap();
        assert_eq!(deleted, 2);

        // Verify 6-10 remain
        for seq in 6..=10 {
            assert!(conn.load_ledger_close_meta(seq).unwrap().is_some());
        }
        for seq in 1..=5 {
            assert!(conn.load_ledger_close_meta(seq).unwrap().is_none());
        }
    }
}
