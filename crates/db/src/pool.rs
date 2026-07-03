//! Connection pool management.
//!
//! This module provides the [`Database`] struct which wraps an r2d2 connection
//! pool for SQLite. The pool allows multiple threads to access the database
//! concurrently while managing connection lifecycle.
//!
//! # Thread Safety
//!
//! The [`Database`] type is `Clone` and can be shared across threads. Each
//! call to [`connection`](Database::connection) returns a connection from
//! the pool, which is returned to the pool when dropped.
//!
//! # Transactions
//!
//! For operations that require atomicity, use [`transaction`](Database::transaction)
//! which ensures the closure runs within a database transaction.

use crate::error::DbError;
use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::Connection;

/// A pooled SQLite connection.
///
/// This is a wrapper around an r2d2 pooled connection that automatically
/// returns the connection to the pool when dropped.
pub(crate) type PooledConnection = r2d2::PooledConnection<SqliteConnectionManager>;

/// Database handle with connection pooling.
///
/// This is the primary entry point for database operations. It wraps an
/// r2d2 connection pool and provides methods for executing queries and
/// transactions.
///
/// # Example
///
/// ```no_run
/// use henyey_db::Database;
///
/// let db = Database::open("stellar.db")?;
///
/// // Execute a query with a connection
/// db.with_connection(|conn| {
///     // Use rusqlite connection methods here
///     Ok(())
/// })?;
///
/// // Execute multiple operations in a transaction
/// db.transaction(|tx| {
///     // Operations here are atomic
///     Ok(())
/// })?;
/// # Ok::<(), henyey_db::DbError>(())
/// ```
#[derive(Clone)]
pub struct Database {
    /// The underlying r2d2 connection pool.
    pub(crate) pool: Pool<SqliteConnectionManager>,
}

impl Database {
    /// Obtains a connection from the pool.
    ///
    /// The connection is automatically returned to the pool when the
    /// returned [`PooledConnection`] is dropped.
    ///
    /// # Errors
    ///
    /// Returns an error if the pool is exhausted or a connection cannot
    /// be established.
    pub(crate) fn connection(&self) -> Result<PooledConnection, DbError> {
        self.pool.get().map_err(DbError::from)
    }

    /// Runs a PASSIVE WAL checkpoint on a pooled connection.
    ///
    /// PASSIVE transfers as much WAL content into the main database file as
    /// possible without blocking concurrent readers or writers, then returns.
    /// Inline auto-checkpoints are disabled (`wal_autocheckpoint = 0`), so the
    /// app's background checkpointer calls this off the ledger-close critical
    /// path — the close-persist commit never pays checkpoint I/O itself.
    ///
    /// Returns `(busy, wal_pages, checkpointed_pages)` from
    /// `PRAGMA wal_checkpoint(PASSIVE)`: `busy` is 1 if the checkpoint could
    /// not complete (e.g. a long-running reader pins the WAL), `wal_pages` is
    /// the WAL size in pages, `checkpointed_pages` how many were transferred.
    pub fn wal_checkpoint_passive(&self) -> Result<(i64, i64, i64), DbError> {
        let conn = self.connection()?;
        conn.query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })
        .map_err(DbError::from)
    }

    /// Executes a closure within a database transaction.
    ///
    /// If the closure returns `Ok`, the transaction is committed.
    /// If it returns `Err`, the transaction is rolled back.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use henyey_db::Database;
    /// # let db = Database::open_in_memory()?;
    /// db.transaction(|tx| {
    ///     tx.execute("INSERT INTO storestate (statename, state) VALUES ('key', 'value')", [])?;
    ///     tx.execute("UPDATE storestate SET state = 'new_value' WHERE statename = 'key'", [])?;
    ///     Ok(())
    /// })?;
    /// # Ok::<(), henyey_db::DbError>(())
    /// ```
    pub fn transaction<T, F>(&self, f: F) -> Result<T, DbError>
    where
        F: FnOnce(&rusqlite::Transaction) -> Result<T, DbError>,
    {
        let mut conn = self.connection()?;
        let tx = conn.transaction()?;
        let result = f(&tx)?;
        tx.commit()?;
        Ok(result)
    }

    /// Executes a closure within a `BEGIN IMMEDIATE` SQLite transaction.
    ///
    /// Unlike [`transaction`](Self::transaction) which uses `BEGIN DEFERRED`
    /// (acquiring a write lock lazily on first write), this method acquires
    /// the RESERVED write lock up-front. This means concurrent writers on
    /// other pool connections are blocked for the entire duration of the
    /// closure — useful when read-then-write sequences must be serialized
    /// w.r.t. all other writers.
    ///
    /// Used by the atomic SCP-tx-set purge (#2770): without an immediate
    /// lock, a concurrent `save_tx_set` on another connection can interleave
    /// between the purge's reads and lead to the freshly-inserted tx-set
    /// being deleted as an orphan.
    pub fn transaction_immediate<T, F>(&self, f: F) -> Result<T, DbError>
    where
        F: FnOnce(&rusqlite::Transaction) -> Result<T, DbError>,
    {
        let mut conn = self.connection()?;
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let result = f(&tx)?;
        tx.commit()?;
        Ok(result)
    }

    /// Executes a closure with a database connection.
    ///
    /// This is useful for read operations or simple writes that don't
    /// require explicit transaction handling.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use henyey_db::Database;
    /// # let db = Database::open_in_memory()?;
    /// let count: i64 = db.with_connection(|conn| {
    ///     conn.query_row("SELECT COUNT(*) FROM storestate", [], |row| row.get(0))
    ///         .map_err(Into::into)
    /// })?;
    /// # Ok::<(), henyey_db::DbError>(())
    /// ```
    pub fn with_connection<T, F>(&self, f: F) -> Result<T, DbError>
    where
        F: FnOnce(&Connection) -> Result<T, DbError>,
    {
        let conn = self.connection()?;
        f(&conn)
    }
}
