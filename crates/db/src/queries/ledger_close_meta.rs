//! Ledger close metadata queries.
//!
//! This module provides database operations for full `LedgerCloseMeta` blobs,
//! used by the `getTransactions` and `getLedgers` RPC endpoints.
//! The data is stored as raw XDR and cleaned up by the Maintainer using the
//! RPC retention window.

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

    /// Loads serialized `LedgerCloseMeta` blobs for a range of ledgers with a
    /// cumulative byte budget.
    ///
    /// Behaves like [`load_ledger_close_metas_in_range`] but stops loading once
    /// the cumulative raw XDR byte count would exceed `max_total_bytes`. The
    /// **first row is always included** regardless of its size so that
    /// pagination can always make forward progress.
    ///
    /// The byte check uses `length(meta)` from SQLite before reading the blob,
    /// so oversized rows beyond the budget are never materialized into memory.
    fn load_ledger_close_metas_in_range_bounded(
        &self,
        start_sequence: u32,
        end_sequence: u32,
        limit: u32,
        max_total_bytes: usize,
    ) -> Result<Vec<(u32, Vec<u8>)>, DbError>;

    /// Deletes old ledger close metadata entries with `sequence <= max_ledger`.
    ///
    /// Removes at most `count` entries to limit the amount of work per call.
    /// Returns the number of entries actually deleted.
    fn delete_old_ledger_close_meta(&self, max_ledger: u32, count: u32) -> Result<u32, DbError>;
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

    fn load_ledger_close_metas_in_range_bounded(
        &self,
        start_sequence: u32,
        end_sequence: u32,
        limit: u32,
        max_total_bytes: usize,
    ) -> Result<Vec<(u32, Vec<u8>)>, DbError> {
        let mut stmt = self.prepare(
            "SELECT sequence, length(meta), meta FROM ledger_close_meta \
             WHERE sequence >= ?1 AND sequence < ?2 \
             ORDER BY sequence ASC LIMIT ?3",
        )?;
        let mut rows = stmt.query(params![start_sequence, end_sequence, limit])?;
        let mut results = Vec::new();
        let mut cumulative_bytes: usize = 0;

        while let Some(row) = rows.next()? {
            let seq: u32 = row.get(0)?;
            let blob_len: u32 = row.get(1)?;
            let blob_len = blob_len as usize;

            if !results.is_empty() && cumulative_bytes + blob_len > max_total_bytes {
                break;
            }

            let meta: Vec<u8> = row.get(2)?;
            cumulative_bytes += blob_len;
            results.push((seq, meta));
        }

        Ok(results)
    }

    fn delete_old_ledger_close_meta(&self, max_ledger: u32, count: u32) -> Result<u32, DbError> {
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

    // -----------------------------------------------------------------------
    // Bounded range query tests
    // -----------------------------------------------------------------------

    /// Helper: store ledgers with blobs of a given size.
    fn store_blobs(conn: &Connection, seqs: std::ops::Range<u32>, blob_size: usize) {
        for seq in seqs {
            let blob = vec![seq as u8; blob_size];
            conn.store_ledger_close_meta(seq, &blob).unwrap();
        }
    }

    #[test]
    fn test_bounded_range_stops_at_budget() {
        let conn = setup_db();
        // 10 ledgers, each with 1000-byte blobs
        store_blobs(&conn, 100..110, 1000);

        // Budget of 3500 → 3 rows (3000 bytes); 4th would push to 4000.
        let results = conn
            .load_ledger_close_metas_in_range_bounded(100, 110, 10, 3500)
            .unwrap();
        assert_eq!(results.len(), 3);
        assert_eq!(results[0].0, 100);
        assert_eq!(results[2].0, 102);
    }

    #[test]
    fn test_bounded_range_always_returns_first_row() {
        let conn = setup_db();
        store_blobs(&conn, 100..101, 5000);

        // Budget of 100 → still returns the oversized first row.
        let results = conn
            .load_ledger_close_metas_in_range_bounded(100, 110, 10, 100)
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, 100);
        assert_eq!(results[0].1.len(), 5000);
    }

    #[test]
    fn test_bounded_range_excludes_oversized_second_row() {
        let conn = setup_db();
        conn.store_ledger_close_meta(100, &vec![0u8; 100]).unwrap();
        conn.store_ledger_close_meta(101, &vec![1u8; 5000]).unwrap();
        conn.store_ledger_close_meta(102, &vec![2u8; 100]).unwrap();

        // Budget of 200 → first row (100 B) included, second (5000 B) excluded.
        let results = conn
            .load_ledger_close_metas_in_range_bounded(100, 110, 10, 200)
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, 100);
    }

    #[test]
    fn test_bounded_range_exact_boundary() {
        let conn = setup_db();
        store_blobs(&conn, 100..103, 1000);

        // Budget exactly 2000 → two rows fit (2000 == budget), third excluded.
        let results = conn
            .load_ledger_close_metas_in_range_bounded(100, 110, 10, 2000)
            .unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].0, 100);
        assert_eq!(results[1].0, 101);
    }

    #[test]
    fn test_bounded_range_empty_result() {
        let conn = setup_db();
        let results = conn
            .load_ledger_close_metas_in_range_bounded(100, 110, 10, 10000)
            .unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn test_bounded_range_row_limit_takes_precedence() {
        let conn = setup_db();
        store_blobs(&conn, 100..110, 10);

        // Row limit of 3, large byte budget → row limit wins.
        let results = conn
            .load_ledger_close_metas_in_range_bounded(100, 110, 3, 1_000_000)
            .unwrap();
        assert_eq!(results.len(), 3);
    }

    #[test]
    fn test_bounded_range_large_budget_returns_all() {
        let conn = setup_db();
        store_blobs(&conn, 100..105, 1000);

        let results = conn
            .load_ledger_close_metas_in_range_bounded(100, 110, 10, 1_000_000)
            .unwrap();
        assert_eq!(results.len(), 5);
    }

    #[test]
    fn test_delete_old() {
        let conn = setup_db();
        for seq in 1..=10 {
            conn.store_ledger_close_meta(seq, b"data").unwrap();
        }

        // Delete up to seq 5, but only 3 at a time
        let deleted = conn.delete_old_ledger_close_meta(5, 3).unwrap();
        assert_eq!(deleted, 3);

        // Delete remaining
        let deleted = conn.delete_old_ledger_close_meta(5, 10).unwrap();
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
