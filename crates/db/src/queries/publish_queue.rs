//! Publish queue queries.
//!
//! The publish queue tracks checkpoint ledgers that need to be published
//! to history archives. When a checkpoint is reached (every 64 ledgers),
//! the ledger is added to the queue. After successful publication, the
//! ledger is removed from the queue.
//!
//! This allows the node to resume publishing after a restart without
//! missing checkpoints.

use rusqlite::{params, Connection};

use crate::error::DbError;

/// Query trait for the history publish queue.
///
/// Provides methods for managing the queue of checkpoint ledgers
/// pending publication to history archives.
pub trait PublishQueueQueries {
    /// Adds a checkpoint ledger to the publish queue.
    ///
    /// Last-write-wins: re-enqueuing an existing `ledgerseq` overwrites the
    /// stored row with the new HAS JSON. This matches stellar-core v26.0.1's
    /// `writeCheckpointFile`, which `durableRename`s the HAS onto a fixed
    /// per-seq path (an unconditional overwrite), and henyey-history's
    /// `PublishQueue::enqueue`, which already uses `INSERT OR REPLACE`.
    /// The `has_json` parameter stores the History Archive State JSON
    /// captured at checkpoint time, ensuring the publish path uses the
    /// exact HAS (including hot archive bucket hashes) from the
    /// checkpoint ledger rather than rebuilding it later when the state
    /// may have advanced.
    fn enqueue_publish(&self, ledger_seq: u32, has_json: &str) -> Result<(), DbError>;

    /// Removes a checkpoint ledger from the publish queue.
    ///
    /// Called after successful publication. This is a no-op if the
    /// ledger is not in the queue.
    fn remove_publish(&self, ledger_seq: u32) -> Result<(), DbError>;

    /// Removes all queued checkpoint ledgers above the given LCL.
    ///
    /// This is called during startup recovery (restore_checkpoint) to clean
    /// up stale publish queue entries that refer to ledgers beyond what has
    /// been committed. Mirrors stellar-core's `restoreCheckpoint()` which
    /// iterates `.checkpoint.dirty` files and removes entries above LCL.
    fn remove_above_lcl(&self, lcl: u32) -> Result<u64, DbError>;

    /// Loads queued checkpoint ledgers in ascending order.
    ///
    /// Optionally limited to a maximum count.
    fn load_publish_queue(&self, limit: Option<usize>) -> Result<Vec<u32>, DbError>;

    /// Loads the HAS JSON for a specific queued checkpoint.
    ///
    /// Returns the History Archive State JSON that was stored at enqueue
    /// time, or `None` if the checkpoint is not in the queue.
    fn load_publish_has(&self, ledger_seq: u32) -> Result<Option<String>, DbError>;

    /// Loads all HAS JSON values from the publish queue.
    ///
    /// Used by bucket cleanup to determine which bucket files are still
    /// referenced by pending publish queue entries.
    fn load_all_publish_has(&self) -> Result<Vec<String>, DbError>;

    /// Removes all queued checkpoint ledgers below the given threshold.
    ///
    /// This permanently abandons those checkpoints — they will never be
    /// published. Used by the maintainer to evict stale entries that are
    /// too far behind the current ledger, preventing unbounded retention
    /// from persistently failing archive publishing.
    ///
    /// The boundary is strict `<`: the entry at exactly `threshold` is
    /// preserved.
    ///
    /// Returns the number of entries removed.
    fn remove_publish_entries_below(&self, threshold: u32) -> Result<u64, DbError>;
}

impl PublishQueueQueries for Connection {
    fn enqueue_publish(&self, ledger_seq: u32, has_json: &str) -> Result<(), DbError> {
        // `INSERT OR REPLACE` makes the last-write-wins contract explicit: on a
        // `ledgerseq` primary-key conflict the existing row is deleted and the
        // new row inserted, overwriting `state` with the new `has_json`. This
        // matches stellar-core's durable-rename overwrite and henyey-history's
        // `INSERT OR REPLACE`. The table has only `(ledgerseq PK, state)` with
        // no other columns, FKs, or triggers, so the replace cannot drop a
        // column or orphan data. All callers always pass a non-null `has_json`,
        // so the `state NOT NULL` constraint is never at risk.
        self.execute(
            "INSERT OR REPLACE INTO publishqueue (ledgerseq, state) VALUES (?1, ?2)",
            params![ledger_seq as i64, has_json],
        )?;
        Ok(())
    }

    fn remove_publish(&self, ledger_seq: u32) -> Result<(), DbError> {
        self.execute(
            "DELETE FROM publishqueue WHERE ledgerseq = ?1",
            params![ledger_seq as i64],
        )?;
        Ok(())
    }

    fn remove_above_lcl(&self, lcl: u32) -> Result<u64, DbError> {
        let count = self.execute(
            "DELETE FROM publishqueue WHERE ledgerseq > ?1",
            params![lcl as i64],
        )?;
        Ok(count as u64)
    }

    fn load_publish_queue(&self, limit: Option<usize>) -> Result<Vec<u32>, DbError> {
        let row_fn = |row: &rusqlite::Row<'_>| row.get::<_, i64>(0).map(|v| v as u32);
        if let Some(limit) = limit {
            let mut stmt =
                self.prepare("SELECT ledgerseq FROM publishqueue ORDER BY ledgerseq ASC LIMIT ?1")?;
            let rows = stmt.query_map(params![limit as i64], row_fn)?;
            rows.collect::<std::result::Result<Vec<_>, _>>()
                .map_err(DbError::from)
        } else {
            let mut stmt =
                self.prepare("SELECT ledgerseq FROM publishqueue ORDER BY ledgerseq ASC")?;
            let rows = stmt.query_map([], row_fn)?;
            rows.collect::<std::result::Result<Vec<_>, _>>()
                .map_err(DbError::from)
        }
    }

    fn load_publish_has(&self, ledger_seq: u32) -> Result<Option<String>, DbError> {
        use rusqlite::OptionalExtension;
        self.query_row(
            "SELECT state FROM publishqueue WHERE ledgerseq = ?1",
            params![ledger_seq as i64],
            |row| row.get(0),
        )
        .optional()
        .map_err(DbError::from)
    }

    fn load_all_publish_has(&self) -> Result<Vec<String>, DbError> {
        let mut stmt = self.prepare("SELECT state FROM publishqueue ORDER BY ledgerseq ASC")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(DbError::from)
    }

    fn remove_publish_entries_below(&self, threshold: u32) -> Result<u64, DbError> {
        let count = self.execute(
            "DELETE FROM publishqueue WHERE ledgerseq < ?1",
            params![threshold as i64],
        )?;
        Ok(count as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE publishqueue (ledgerseq INTEGER PRIMARY KEY, state TEXT NOT NULL);",
        )
        .unwrap();
        conn
    }

    #[test]
    fn test_enqueue_publish_last_write_wins() {
        // Last-write-wins: re-enqueuing the same `ledgerseq` with a differing
        // HAS overwrites the stored row. This mirrors stellar-core v26.0.1's
        // `writeCheckpointFile` durable-rename onto a fixed per-seq path (an
        // unconditional overwrite) and henyey-history's `INSERT OR REPLACE`.
        let conn = setup_db();

        let has_a = r#"{"version":2,"currentLedger":63,"marker":"A"}"#;
        let has_b = r#"{"version":2,"currentLedger":63,"marker":"B"}"#;

        conn.enqueue_publish(63, has_a).unwrap();
        conn.enqueue_publish(63, has_b).unwrap();

        let stored = conn.load_publish_has(63).unwrap().unwrap();
        assert_eq!(stored, has_b);
    }

    #[test]
    fn test_enqueue_publish_overwrites_legacy_pending_row() {
        // Last-write-wins repairs legacy rows: a pre-existing `'pending'`
        // sentinel row (which this crate no longer writes) is overwritten by a
        // real HAS on re-enqueue. This matches the pre-#3390 `WHERE state =
        // 'pending'` UPDATE branch and stellar-core's unconditional rename.
        let conn = setup_db();

        conn.execute(
            "INSERT INTO publishqueue (ledgerseq, state) VALUES (?1, ?2)",
            params![63_i64, "pending"],
        )
        .unwrap();

        let has_json = r#"{"version":2,"currentLedger":63}"#;
        conn.enqueue_publish(63, has_json).unwrap();

        let stored = conn.load_publish_has(63).unwrap().unwrap();
        assert_eq!(stored, has_json);
    }

    #[test]
    fn test_load_all_publish_has_returns_all_rows() {
        // Pins the removal of the dead `WHERE state != 'pending'` filter:
        // every enqueued HAS row is returned, in ascending ledgerseq order.
        let conn = setup_db();

        let has_63 = r#"{"version":2,"currentLedger":63}"#;
        let has_127 = r#"{"version":2,"currentLedger":127}"#;

        // Insert out of order to confirm the ORDER BY is honored.
        conn.enqueue_publish(127, has_127).unwrap();
        conn.enqueue_publish(63, has_63).unwrap();

        let all = conn.load_all_publish_has().unwrap();
        assert_eq!(all, vec![has_63.to_string(), has_127.to_string()]);
    }

    #[test]
    fn test_remove_publish_entries_below_basic() {
        let conn = setup_db();
        let has = r#"{"version":2}"#;

        conn.enqueue_publish(63, has).unwrap();
        conn.enqueue_publish(127, has).unwrap();
        conn.enqueue_publish(191, has).unwrap();

        let removed = conn.remove_publish_entries_below(128).unwrap();
        assert_eq!(removed, 2); // 63 and 127 removed

        let remaining = conn.load_publish_queue(None).unwrap();
        assert_eq!(remaining, vec![191]);
    }

    #[test]
    fn test_remove_publish_entries_below_exact_boundary() {
        let conn = setup_db();
        let has = r#"{"version":2}"#;

        conn.enqueue_publish(63, has).unwrap();
        conn.enqueue_publish(127, has).unwrap();

        // Strict <: entry at exactly 127 is preserved
        let removed = conn.remove_publish_entries_below(127).unwrap();
        assert_eq!(removed, 1);

        let remaining = conn.load_publish_queue(None).unwrap();
        assert_eq!(remaining, vec![127]);
    }

    #[test]
    fn test_remove_publish_entries_below_empty_queue() {
        let conn = setup_db();
        let removed = conn.remove_publish_entries_below(1000).unwrap();
        assert_eq!(removed, 0);
    }

    #[test]
    fn test_remove_publish_entries_below_returns_count() {
        let conn = setup_db();
        let has = r#"{"version":2}"#;

        for seq in (63..=63 + 64 * 9).step_by(64) {
            conn.enqueue_publish(seq, has).unwrap();
        }

        // 10 entries: 63, 127, 191, 255, 319, 383, 447, 511, 575, 639
        let removed = conn.remove_publish_entries_below(400).unwrap();
        assert_eq!(removed, 6); // 63, 127, 191, 255, 319, 383
    }
}
