//! Persistent publish queue for history archive publishing.
//!
//! This module provides crash-safe checkpoint queuing for history publishing.
//! Checkpoints are persisted to the database before publishing, allowing
//! publication to resume after a crash or restart.
//!
//! # Overview
//!
//! When a validator closes a checkpoint ledger, it:
//!
//! 1. **Queues** the checkpoint with its HistoryArchiveState to the database
//! 2. **Publishes** the checkpoint files to history archives
//! 3. **Dequeues** the checkpoint after successful publication
//!
//! If the node crashes between queueing and dequeuing, the checkpoint
//! remains in the queue and will be published on restart.
//!
//! # Database Schema
//!
//! The publish queue uses the `publishqueue` table:
//!
//! ```sql
//! CREATE TABLE publishqueue (
//!     ledgerseq INTEGER PRIMARY KEY,
//!     state TEXT NOT NULL
//! );
//! ```
//!
//! Where `state` contains the serialized `HistoryArchiveState` JSON.
//!
//! # Usage
//!
//! ```ignore
//! use henyey_history::publish_queue::PublishQueue;
//! use henyey_db::Database;
//!
//! let db = Database::open("/var/stellar/state.db")?;
//! let queue = PublishQueue::new(db);
//!
//! // Queue a checkpoint for publishing
//! queue.enqueue(checkpoint_ledger, &has)?;
//!
//! // Get queue stats
//! let len = queue.len()?;
//! let (min, max) = queue.ledger_range()?;
//!
//! // After successful publication, dequeue
//! queue.dequeue(checkpoint_ledger)?;
//! ```

use henyey_common::LedgerSeq;
use std::collections::HashSet;
use std::sync::Arc;

use henyey_db::Database;

use crate::archive_state::HistoryArchiveState;
use crate::checkpoint::is_checkpoint_ledger;
use crate::{HistoryError, Result};
use tracing::{debug, info, warn};

/// Maximum publish queue depth before transaction replay is paused during
/// offline catchup. CATCHUP_SPEC §5.6.
pub const PUBLISH_QUEUE_MAX_SIZE: usize = 16;

/// Queue depth at which transaction replay resumes after being paused.
/// CATCHUP_SPEC §5.6.
pub const PUBLISH_QUEUE_UNBLOCK_APPLICATION: usize = 8;

/// Persistent queue for checkpoints pending publication.
///
/// This queue is backed by a SQLite database, ensuring checkpoints
/// are not lost if the node crashes during publishing.
pub struct PublishQueue {
    /// Database reference.
    db: Arc<Database>,
}

impl PublishQueue {
    /// Create a new publish queue backed by the given database.
    pub fn new(db: Arc<Database>) -> Self {
        Self { db }
    }

    /// Get the number of checkpoints in the queue.
    pub fn len(&self) -> Result<usize> {
        self.db
            .with_connection(|conn| {
                let count: i64 =
                    conn.query_row("SELECT COUNT(*) FROM publishqueue", [], |row| row.get(0))?;
                Ok(count as usize)
            })
            .map_err(Into::into)
    }

    /// Check if the queue is empty.
    pub fn is_empty(&self) -> Result<bool> {
        Ok(self.len()? == 0)
    }

    /// Get the minimum (oldest) ledger sequence in the queue.
    ///
    /// Returns `None` if the queue is empty.
    pub fn min_ledger(&self) -> Result<Option<u32>> {
        self.db
            .with_connection(|conn| {
                let result: Option<i64> = conn
                    .query_row("SELECT MIN(ledgerseq) FROM publishqueue", [], |row| {
                        row.get(0)
                    })
                    .ok();
                Ok(result.map(|v| v as u32))
            })
            .map_err(Into::into)
    }

    /// Get the maximum (newest) ledger sequence in the queue.
    ///
    /// Returns `None` if the queue is empty.
    pub fn max_ledger(&self) -> Result<Option<u32>> {
        self.db
            .with_connection(|conn| {
                let result: Option<i64> = conn
                    .query_row("SELECT MAX(ledgerseq) FROM publishqueue", [], |row| {
                        row.get(0)
                    })
                    .ok();
                Ok(result.map(|v| v as u32))
            })
            .map_err(Into::into)
    }

    /// Get the ledger range in the queue as (min, max).
    ///
    /// Returns `(0, 0)` if the queue is empty.
    pub fn ledger_range(&self) -> Result<(u32, u32)> {
        let min = self.min_ledger()?.unwrap_or(0);
        let max = self.max_ledger()?.unwrap_or(0);
        Ok((min, max))
    }

    /// Enqueue a checkpoint for publishing.
    ///
    /// Stores the checkpoint ledger and its HistoryArchiveState in the database.
    /// This is a no-op if the checkpoint is already in the queue.
    ///
    /// # Arguments
    ///
    /// * `ledger_seq` - The checkpoint ledger sequence
    /// * `has` - The History Archive State for this checkpoint
    ///
    /// # Errors
    ///
    /// Returns an error if `ledger_seq` is not a valid checkpoint ledger.
    pub fn enqueue(&self, ledger_seq: LedgerSeq, has: &HistoryArchiveState) -> Result<()> {
        if !is_checkpoint_ledger(ledger_seq.get()) {
            return Err(HistoryError::NotCheckpointLedger(ledger_seq.get()));
        }

        let state_json = serde_json::to_string(has).map_err(|e| {
            HistoryError::VerificationFailed(format!("JSON serialization failed: {e}"))
        })?;

        self.db.with_connection(|conn| {
            conn.execute(
                "INSERT OR REPLACE INTO publishqueue (ledgerseq, state) VALUES (?1, ?2)",
                rusqlite::params![ledger_seq.get() as i64, state_json],
            )?;
            Ok(())
        })?;

        debug!(
            ledger_seq = ledger_seq.get(),
            "Enqueued checkpoint for publishing"
        );
        Ok(())
    }

    /// Dequeue a checkpoint after successful publication.
    ///
    /// Removes the checkpoint from the queue. This is a no-op if the
    /// checkpoint is not in the queue.
    pub fn dequeue(&self, ledger_seq: LedgerSeq) -> Result<()> {
        self.db.with_connection(|conn| {
            conn.execute(
                "DELETE FROM publishqueue WHERE ledgerseq = ?1",
                rusqlite::params![ledger_seq.get() as i64],
            )?;
            Ok(())
        })?;

        debug!(
            ledger_seq = ledger_seq.get(),
            "Dequeued published checkpoint"
        );
        Ok(())
    }

    /// Check if a checkpoint is in the queue.
    pub fn contains(&self, ledger_seq: LedgerSeq) -> Result<bool> {
        self.db
            .with_connection(|conn| {
                let count: i64 = conn.query_row(
                    "SELECT COUNT(*) FROM publishqueue WHERE ledgerseq = ?1",
                    rusqlite::params![ledger_seq.get() as i64],
                    |row| row.get(0),
                )?;
                Ok(count > 0)
            })
            .map_err(Into::into)
    }

    /// Get the HistoryArchiveState for a queued checkpoint.
    ///
    /// Returns `None` if the checkpoint is not in the queue.
    pub fn get_state(&self, ledger_seq: LedgerSeq) -> Result<Option<HistoryArchiveState>> {
        self.db
            .with_connection(|conn| {
                let result: std::result::Result<String, _> = conn.query_row(
                    "SELECT state FROM publishqueue WHERE ledgerseq = ?1",
                    rusqlite::params![ledger_seq.get() as i64],
                    |row| row.get(0),
                );

                match result {
                    Ok(json) => {
                        let has: HistoryArchiveState =
                            serde_json::from_str(&json).map_err(|e| {
                                henyey_db::DbError::Integrity(format!("JSON parse failed: {e}"))
                            })?;
                        Ok(Some(has))
                    }
                    Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
                    Err(e) => Err(e.into()),
                }
            })
            .map_err(Into::into)
    }

    /// Get all queued checkpoints in ascending ledger order.
    ///
    /// Returns a list of (ledger_seq, HistoryArchiveState) pairs.
    pub fn get_all(&self) -> Result<Vec<(u32, HistoryArchiveState)>> {
        self.db
            .with_connection(|conn| {
                let mut stmt = conn
                    .prepare("SELECT ledgerseq, state FROM publishqueue ORDER BY ledgerseq ASC")?;

                let rows = stmt.query_map([], |row| {
                    let ledger_seq: i64 = row.get(0)?;
                    let state_json: String = row.get(1)?;
                    Ok((ledger_seq as u32, state_json))
                })?;

                let mut results = Vec::new();
                for row in rows {
                    let (ledger_seq, json) = row?;
                    let has: HistoryArchiveState = serde_json::from_str(&json).map_err(|e| {
                        henyey_db::DbError::Integrity(format!("JSON parse failed: {e}"))
                    })?;
                    results.push((ledger_seq, has));
                }

                Ok(results)
            })
            .map_err(Into::into)
    }

    /// Get all bucket hashes referenced by queued checkpoints.
    ///
    /// This is used to determine which buckets must be retained
    /// until all referencing checkpoints are published.
    pub fn get_referenced_bucket_hashes(&self) -> Result<HashSet<String>> {
        let checkpoints = self.get_all()?;
        let mut hashes = HashSet::new();

        for (_, has) in checkpoints {
            for bucket_hash in has.all_bucket_hashes() {
                hashes.insert(bucket_hash.to_hex());
            }
        }

        Ok(hashes)
    }

    /// Clear all entries from the queue.
    ///
    /// Use with caution - this removes all pending checkpoints.
    pub fn clear(&self) -> Result<()> {
        self.db.with_connection(|conn| {
            conn.execute("DELETE FROM publishqueue", [])?;
            Ok(())
        })?;
        warn!("Cleared all entries from publish queue");
        Ok(())
    }

    /// Log publish queue status.
    pub fn log_status(&self) -> Result<()> {
        let len = self.len()?;
        if len == 0 {
            debug!("Publish queue empty");
        } else {
            let (min, max) = self.ledger_range()?;
            info!(
                queue_length = len,
                min_ledger = min,
                max_ledger = max,
                "Publish queue status: {} checkpoints [{}-{}]",
                len,
                min,
                max
            );
        }
        Ok(())
    }
}

/// Statistics about the publish queue.
#[derive(Debug, Clone, Default)]
pub struct PublishQueueStats {
    /// Number of checkpoints in the queue.
    pub queue_length: usize,
    /// Minimum (oldest) ledger in the queue.
    pub min_ledger: LedgerSeq,
    /// Maximum (newest) ledger in the queue.
    pub max_ledger: LedgerSeq,
    /// Number of unique buckets referenced by queued checkpoints.
    pub bucket_count: usize,
}

impl PublishQueue {
    /// Get queue statistics.
    pub fn stats(&self) -> Result<PublishQueueStats> {
        let queue_length = self.len()?;
        let (min_ledger, max_ledger) = self.ledger_range()?;
        let bucket_count = self.get_referenced_bucket_hashes()?.len();

        Ok(PublishQueueStats {
            queue_length,
            min_ledger: min_ledger.into(),
            max_ledger: max_ledger.into(),
            bucket_count,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::archive_state::{HASBucketLevel, HASBucketNext};

    fn create_test_db() -> Arc<Database> {
        Arc::new(Database::open_in_memory().expect("Failed to create test database"))
    }

    fn create_test_has(ledger_seq: LedgerSeq) -> HistoryArchiveState {
        // Create valid 64-character hex hashes based on ledger_seq
        let curr_hash = format!("{:064x}", ledger_seq.get() as u128 * 2);
        let snap_hash = format!("{:064x}", ledger_seq.get() as u128 * 2 + 1);

        HistoryArchiveState {
            version: 2,
            server: Some("test".to_string()),
            current_ledger: ledger_seq,
            network_passphrase: Some("Test Network".to_string()),
            current_buckets: vec![HASBucketLevel {
                curr: curr_hash,
                snap: snap_hash,
                next: HASBucketNext::default(),
            }],
            hot_archive_buckets: None,
        }
    }

    #[test]
    fn test_empty_queue() {
        let db = create_test_db();
        let queue = PublishQueue::new(db);

        assert!(queue.is_empty().unwrap());
        assert_eq!(queue.len().unwrap(), 0);
        assert_eq!(queue.min_ledger().unwrap(), None);
        assert_eq!(queue.max_ledger().unwrap(), None);
    }

    #[test]
    fn test_enqueue_dequeue() {
        let db = create_test_db();
        let queue = PublishQueue::new(db);

        let has = create_test_has(63u32.into());
        queue.enqueue(63u32.into(), &has).unwrap();

        assert!(!queue.is_empty().unwrap());
        assert_eq!(queue.len().unwrap(), 1);
        assert!(queue.contains(63u32.into()).unwrap());
        assert_eq!(queue.min_ledger().unwrap(), Some(63));
        assert_eq!(queue.max_ledger().unwrap(), Some(63));

        queue.dequeue(63u32.into()).unwrap();
        assert!(queue.is_empty().unwrap());
    }

    #[test]
    fn test_multiple_checkpoints() {
        let db = create_test_db();
        let queue = PublishQueue::new(db);

        queue
            .enqueue(63u32.into(), &create_test_has(63u32.into()))
            .unwrap();
        queue
            .enqueue(127u32.into(), &create_test_has(127u32.into()))
            .unwrap();
        queue
            .enqueue(191u32.into(), &create_test_has(191u32.into()))
            .unwrap();

        assert_eq!(queue.len().unwrap(), 3);
        assert_eq!(queue.ledger_range().unwrap(), (63, 191));

        let all = queue.get_all().unwrap();
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].0, 63);
        assert_eq!(all[1].0, 127);
        assert_eq!(all[2].0, 191);
    }

    #[test]
    fn test_get_state() {
        let db = create_test_db();
        let queue = PublishQueue::new(db);

        let has = create_test_has(63u32.into());
        queue.enqueue(63u32.into(), &has).unwrap();

        let loaded = queue.get_state(63u32.into()).unwrap().unwrap();
        assert_eq!(loaded.current_ledger, 63);
        assert_eq!(loaded.network_passphrase, Some("Test Network".to_string()));

        assert!(queue.get_state(127u32.into()).unwrap().is_none());
    }

    #[test]
    fn test_referenced_buckets() {
        let db = create_test_db();
        let queue = PublishQueue::new(db);

        queue
            .enqueue(63u32.into(), &create_test_has(63u32.into()))
            .unwrap();
        queue
            .enqueue(127u32.into(), &create_test_has(127u32.into()))
            .unwrap();

        let buckets = queue.get_referenced_bucket_hashes().unwrap();
        assert_eq!(buckets.len(), 4); // 2 checkpoints × 2 buckets each
    }

    #[test]
    fn test_invalid_checkpoint() {
        let db = create_test_db();
        let queue = PublishQueue::new(db);

        let has = create_test_has(64u32.into()); // Not a checkpoint ledger
        let result = queue.enqueue(64u32.into(), &has);
        assert!(result.is_err());
    }

    #[test]
    fn test_stats() {
        let db = create_test_db();
        let queue = PublishQueue::new(db);

        queue
            .enqueue(63u32.into(), &create_test_has(63u32.into()))
            .unwrap();
        queue
            .enqueue(127u32.into(), &create_test_has(127u32.into()))
            .unwrap();

        let stats = queue.stats().unwrap();
        assert_eq!(stats.queue_length, 2);
        assert_eq!(stats.min_ledger, LedgerSeq::from(63));
        assert_eq!(stats.max_ledger, LedgerSeq::from(127));
        assert_eq!(stats.bucket_count, 4);
    }

    #[test]
    fn test_clear() {
        let db = create_test_db();
        let queue = PublishQueue::new(db);

        queue
            .enqueue(63u32.into(), &create_test_has(63u32.into()))
            .unwrap();
        queue
            .enqueue(127u32.into(), &create_test_has(127u32.into()))
            .unwrap();
        assert_eq!(queue.len().unwrap(), 2);

        queue.clear().unwrap();
        assert!(queue.is_empty().unwrap());
    }

    // ── CATCHUP_SPEC §5.6: Publish queue constants ──────────────────

    #[test]
    fn test_publish_queue_max_size_is_16() {
        assert_eq!(
            PUBLISH_QUEUE_MAX_SIZE, 16,
            "CATCHUP_SPEC §5.6: PUBLISH_QUEUE_MAX_SIZE must be 16"
        );
    }

    #[test]
    fn test_publish_queue_unblock_is_8() {
        assert_eq!(
            PUBLISH_QUEUE_UNBLOCK_APPLICATION, 8,
            "CATCHUP_SPEC §5.6: PUBLISH_QUEUE_UNBLOCK_APPLICATION must be 8"
        );
    }

    #[test]
    fn test_publish_queue_hysteresis_invariant() {
        // The unblock threshold must be strictly less than the max size
        // for hysteresis to work correctly.
        assert!(
            PUBLISH_QUEUE_UNBLOCK_APPLICATION < PUBLISH_QUEUE_MAX_SIZE,
            "unblock threshold must be less than max size for hysteresis"
        );
    }
}
