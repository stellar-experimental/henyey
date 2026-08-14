//! SQLite implementation of SCP state persistence.
//!
//! This module provides a SQLite-backed implementation of the `ScpStatePersistence`
//! trait from `stellar-core-herder`. It enables crash recovery by persisting SCP
//! state to the database.
//!
//! # Usage
//!
//! ```ignore
//! use henyey_db::{Database, SqliteScpPersistence};
//! use henyey_herder::ScpPersistenceManager;
//!
//! let db = Database::open("stellar.db")?;
//! let persistence = SqliteScpPersistence::new(db);
//! let manager = ScpPersistenceManager::new(Box::new(persistence));
//! ```

use stellar_xdr::Hash;
use tracing::debug;

use crate::error::DbError;
use crate::pool::Database;
use crate::queries::ScpStatePersistenceQueries;

/// SQLite implementation of SCP state persistence.
///
/// This implementation stores SCP state in the SQLite database using the
/// `storestate` table with appropriate prefixes for slot states and tx sets.
pub struct SqliteScpPersistence {
    db: Database,
}

impl SqliteScpPersistence {
    /// Create a new SQLite SCP persistence instance.
    pub fn new(db: Database) -> Self {
        Self { db }
    }

    fn with_connection<T>(
        &self,
        f: impl FnOnce(&rusqlite::Connection) -> Result<T, DbError>,
    ) -> Result<T, DbError> {
        self.db.with_connection(f)
    }

    /// Save SCP state for a slot.
    pub fn save_scp_state(&self, slot: u64, state_json: &str) -> Result<(), DbError> {
        self.with_connection(|conn| {
            conn.save_scp_slot_state(slot, state_json)?;
            Ok(())
        })
    }

    /// Load SCP state for a slot.
    pub fn load_scp_state(&self, slot: u64) -> Result<Option<String>, DbError> {
        self.with_connection(|conn| conn.load_scp_slot_state(slot))
    }

    /// Load SCP state for all slots.
    pub fn load_all_scp_states(&self) -> Result<Vec<(u64, String)>, DbError> {
        self.with_connection(|conn| conn.load_all_scp_slot_states())
    }

    /// Delete SCP state for slots below the given threshold.
    pub fn delete_scp_state_below(&self, slot: u64) -> Result<(), DbError> {
        self.with_connection(|conn| {
            conn.delete_scp_slot_states_below(slot)?;
            debug!("Deleted SCP state below slot {}", slot);
            Ok(())
        })
    }

    /// Save a transaction set.
    pub fn save_tx_set(&self, hash: &Hash, tx_set: &[u8]) -> Result<(), DbError> {
        self.with_connection(|conn| {
            conn.save_tx_set_data(hash, tx_set)?;
            Ok(())
        })
    }

    /// Load a transaction set.
    pub fn load_tx_set(&self, hash: &Hash) -> Result<Option<Vec<u8>>, DbError> {
        self.with_connection(|conn| conn.load_tx_set_data(hash))
    }

    /// Load all transaction sets.
    pub fn load_all_tx_sets(&self) -> Result<Vec<(Hash, Vec<u8>)>, DbError> {
        self.with_connection(|conn| conn.load_all_tx_set_data())
    }

    /// Check if a transaction set exists.
    pub fn has_tx_set(&self, hash: &Hash) -> Result<bool, DbError> {
        self.with_connection(|conn| conn.has_tx_set_data(hash))
    }

    /// Return the hashes of all persisted transaction sets.
    pub fn get_all_tx_set_hashes(&self) -> Result<Vec<Hash>, DbError> {
        self.with_connection(|conn| conn.get_all_tx_set_hashes())
    }

    /// Delete persisted transaction sets by their hashes.
    pub fn delete_tx_sets_by_hashes(&self, hashes: &[Hash]) -> Result<(), DbError> {
        self.with_connection(|conn| conn.delete_tx_sets_by_hashes(hashes))
    }

    /// Atomic purge of unreferenced persisted transaction sets.
    ///
    /// Wraps the three-step purge (read hashes, read SCP states, delete
    /// orphans) in a single SQLite transaction so a concurrent writer
    /// (e.g. `persist_scp_state`) cannot have its freshly-inserted tx-set
    /// deleted as an orphan between steps. See `#2770` for context.
    pub fn purge_unreferenced_tx_sets_atomic(&self) -> Result<(), DbError> {
        // BEGIN IMMEDIATE so the purge holds a RESERVED write lock for the
        // entire read-then-delete sequence — blocks concurrent `save_tx_set`
        // / `save_scp_state` on other pool connections so we cannot delete a
        // tx-set that a writer is in the middle of registering. See #2770.
        //
        // Instrument the hold: this BEGIN IMMEDIATE spans full-table reads of
        // `storestate`, so a long hold here is a prime `database is locked`
        // suspect during near-tip back-fill (#3702). Instrumentation only —
        // no change to the txn scope acquired above.
        let _write_ctx = henyey_common::WriteCtxGuard::new("scp-persist-purge");
        self.db
            .transaction_immediate(|tx| tx.purge_unreferenced_tx_sets_atomic())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sqlite_scp_persistence() {
        let db = Database::open_in_memory().unwrap();
        let persistence = SqliteScpPersistence::new(db);

        // Test slot state
        let state_json = r#"{"version":1,"envelopes":[],"quorum_sets":[]}"#;
        persistence.save_scp_state(100, state_json).unwrap();

        let loaded = persistence.load_scp_state(100).unwrap();
        assert_eq!(loaded, Some(state_json.to_string()));

        // Test all states
        persistence.save_scp_state(101, "state101").unwrap();
        let all = persistence.load_all_scp_states().unwrap();
        assert_eq!(all.len(), 2);

        // Test deletion
        persistence.delete_scp_state_below(101).unwrap();
        let remaining = persistence.load_all_scp_states().unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].0, 101);
    }

    #[test]
    fn test_sqlite_tx_set_persistence() {
        let db = Database::open_in_memory().unwrap();
        let persistence = SqliteScpPersistence::new(db);

        let hash = Hash([1u8; 32]);
        let data = vec![1, 2, 3, 4, 5];

        // Save and load
        persistence.save_tx_set(&hash, &data).unwrap();
        assert!(persistence.has_tx_set(&hash).unwrap());

        let loaded = persistence.load_tx_set(&hash).unwrap();
        assert_eq!(loaded, Some(data.clone()));

        // Load all
        let all = persistence.load_all_tx_sets().unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].0, hash);
        assert_eq!(all[0].1, data);
    }
}
