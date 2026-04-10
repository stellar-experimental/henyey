//! Publish queue queries.
//!
//! The publish queue tracks checkpoint ledgers that need to be published
//! to history archives. When a checkpoint is reached (every 64 ledgers),
//! the ledger is added to the queue. After successful publication, the
//! ledger is removed from the queue.
//!
//! This allows the node to resume publishing after a restart without
//! missing checkpoints.

use henyey_common::LedgerSeq;
use rusqlite::{params, Connection};

use crate::error::DbError;

/// Query trait for the history publish queue.
///
/// Provides methods for managing the queue of checkpoint ledgers
/// pending publication to history archives.
pub trait PublishQueueQueries {
    /// Adds a checkpoint ledger to the publish queue.
    ///
    /// This is a no-op if the ledger is already in the queue.
    /// The `has_json` parameter stores the History Archive State JSON
    /// captured at checkpoint time, ensuring the publish path uses the
    /// exact HAS (including hot archive bucket hashes) from the
    /// checkpoint ledger rather than rebuilding it later when the state
    /// may have advanced.
    fn enqueue_publish(&self, ledger_seq: LedgerSeq, has_json: &str) -> Result<(), DbError>;

    /// Removes a checkpoint ledger from the publish queue.
    ///
    /// Called after successful publication. This is a no-op if the
    /// ledger is not in the queue.
    fn remove_publish(&self, ledger_seq: LedgerSeq) -> Result<(), DbError>;

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
    fn load_publish_has(&self, ledger_seq: LedgerSeq) -> Result<Option<String>, DbError>;

    /// Loads all HAS JSON values from the publish queue.
    ///
    /// Used by bucket cleanup to determine which bucket files are still
    /// referenced by pending publish queue entries.
    fn load_all_publish_has(&self) -> Result<Vec<String>, DbError>;
}

impl PublishQueueQueries for Connection {
    fn enqueue_publish(&self, ledger_seq: LedgerSeq, has_json: &str) -> Result<(), DbError> {
        self.execute(
            "INSERT INTO publishqueue (ledgerseq, state) VALUES (?1, ?2) \
             ON CONFLICT(ledgerseq) DO UPDATE SET state = excluded.state \
             WHERE publishqueue.state = 'pending'",
            params![ledger_seq, has_json],
        )?;
        Ok(())
    }

    fn remove_publish(&self, ledger_seq: LedgerSeq) -> Result<(), DbError> {
        self.execute(
            "DELETE FROM publishqueue WHERE ledgerseq = ?1",
            params![ledger_seq],
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
        let mut sql = String::from("SELECT ledgerseq FROM publishqueue ORDER BY ledgerseq ASC");
        if limit.is_some() {
            sql.push_str(" LIMIT ?1");
        }
        let mut stmt = self.prepare(&sql)?;
        let rows = if let Some(limit) = limit {
            stmt.query_map(params![limit as i64], row_fn)?
        } else {
            stmt.query_map([], row_fn)?
        };
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(DbError::from)
    }

    fn load_publish_has(&self, ledger_seq: LedgerSeq) -> Result<Option<String>, DbError> {
        use rusqlite::OptionalExtension;
        self.query_row(
            "SELECT state FROM publishqueue WHERE ledgerseq = ?1",
            params![ledger_seq],
            |row| row.get(0),
        )
        .optional()
        .map_err(DbError::from)
    }

    fn load_all_publish_has(&self) -> Result<Vec<String>, DbError> {
        let mut stmt = self.prepare(
            "SELECT state FROM publishqueue WHERE state != 'pending' ORDER BY ledgerseq ASC",
        )?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(DbError::from)
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
    fn test_enqueue_publish_overwrites_existing_legacy_state() {
        let conn = setup_db();

        conn.execute(
            "INSERT INTO publishqueue (ledgerseq, state) VALUES (?1, ?2)",
            params![63_i64, "pending"],
        )
        .unwrap();

        let has_json = r#"{"version":2,"currentLedger":63}"#;
        conn.enqueue_publish(63.into(), has_json).unwrap();

        let stored = conn.load_publish_has(63.into()).unwrap().unwrap();
        assert_eq!(stored, has_json);
    }

    #[test]
    fn test_enqueue_publish_keeps_existing_has_json() {
        let conn = setup_db();

        let first_has = r#"{"version":2,"currentLedger":63,"marker":"first"}"#;
        let second_has = r#"{"version":2,"currentLedger":63,"marker":"second"}"#;

        conn.enqueue_publish(63.into(), first_has).unwrap();
        conn.enqueue_publish(63.into(), second_has).unwrap();

        let stored = conn.load_publish_has(63.into()).unwrap().unwrap();
        assert_eq!(stored, first_has);
    }
}
