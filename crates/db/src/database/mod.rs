//! High-level database API built on top of query traits.

mod history;
mod network;
mod scp;

use std::path::Path;

use tracing::info;

use crate::{migrations, pool::Database, queries, schema, Result};

/// Maximum number of connections in the pool for file-backed databases.
const POOL_MAX_SIZE: u32 = 10;

/// Timeout in seconds for acquiring a connection from the pool.
const CONNECTION_TIMEOUT_SECS: u64 = 30;

/// SQLite busy timeout in milliseconds for lock contention handling.
const BUSY_TIMEOUT_MS: u32 = 30_000;

/// SQLite cache size in kibibytes (negative value = KiB for PRAGMA cache_size).
const CACHE_SIZE_KIB: i32 = -64_000;

impl Database {
    /// Opens a database at the given path, creating it if necessary.
    ///
    /// This method will:
    /// 1. Create the parent directory if it doesn't exist
    /// 2. Open or create the SQLite database file
    /// 3. Configure SQLite for optimal performance (WAL mode, cache settings)
    /// 4. Run any pending schema migrations
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The parent directory cannot be created
    /// - The database file cannot be opened
    /// - Schema migrations fail
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();

        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }

        let manager = r2d2_sqlite::SqliteConnectionManager::file(path).with_init(|conn| {
            conn.execute_batch(&format!("PRAGMA busy_timeout = {};", BUSY_TIMEOUT_MS))?;
            Ok(())
        });
        let pool = r2d2::Pool::builder()
            .max_size(POOL_MAX_SIZE)
            .connection_timeout(std::time::Duration::from_secs(CONNECTION_TIMEOUT_SECS))
            .build(manager)?;

        let db = Self { pool };
        db.initialize()?;
        Ok(db)
    }

    /// Opens an in-memory database, primarily for testing.
    ///
    /// The database is initialized with the current schema but data is not
    /// persisted across restarts. The connection pool size is limited to 1
    /// since in-memory databases are connection-specific.
    pub fn open_in_memory() -> Result<Self> {
        let manager = r2d2_sqlite::SqliteConnectionManager::memory();
        let pool = r2d2::Pool::builder().max_size(1).build(manager)?;

        let db = Self { pool };
        db.initialize()?;
        Ok(db)
    }

    /// Initializes the database, configuring SQLite and running migrations.
    ///
    /// This is called automatically by [`open`] and [`open_in_memory`].
    /// It configures SQLite pragmas for performance and either initializes
    /// a fresh database or migrates an existing one.
    fn initialize(&self) -> Result<()> {
        let conn = self.connection()?;

        conn.execute_batch(&format!(
            r#"
            PRAGMA journal_mode = WAL;
            PRAGMA synchronous = FULL;
            PRAGMA cache_size = {};
            PRAGMA foreign_keys = ON;
            PRAGMA temp_store = MEMORY;
            PRAGMA busy_timeout = {};
        "#,
            CACHE_SIZE_KIB, BUSY_TIMEOUT_MS
        ))?;

        let tables_exist: bool = conn
            .query_row(
                "SELECT COUNT(*) > 0 FROM sqlite_master WHERE type='table' AND name='storestate'",
                [],
                |row| row.get(0),
            )
            .unwrap_or(false);

        if tables_exist {
            if migrations::needs_migration(&conn)? {
                info!("Database requires migration");
                migrations::run_migrations(&conn)?;
            }
            migrations::verify_schema(&conn)?;
        } else {
            migrations::initialize_schema(&conn)?;
        }

        Ok(())
    }

    /// Returns the highest ledger sequence number stored in the database.
    ///
    /// Returns `None` if no ledgers have been stored yet.
    pub fn get_latest_ledger_seq(&self) -> Result<Option<u32>> {
        self.with_connection(|conn| {
            use queries::LedgerQueries;
            conn.get_latest_ledger_seq()
        })
    }

    /// Returns the lowest ledger sequence number stored in the database.
    ///
    /// Returns `None` if no ledgers have been stored yet.
    pub fn get_oldest_ledger_seq(&self) -> Result<Option<u32>> {
        self.with_connection(|conn| {
            use queries::LedgerQueries;
            conn.get_oldest_ledger_seq()
        })
    }

    /// Returns the ledger header for a given sequence number.
    ///
    /// Returns `None` if the ledger is not found.
    pub fn get_ledger_header(&self, seq: u32) -> Result<Option<stellar_xdr::curr::LedgerHeader>> {
        self.with_connection(|conn| {
            use queries::LedgerQueries;
            conn.load_ledger_header(seq)
        })
    }

    /// Returns the hash of a ledger by its sequence number.
    ///
    /// Returns `None` if the ledger is not found.
    pub fn get_ledger_hash(&self, seq: u32) -> Result<Option<henyey_common::Hash256>> {
        self.with_connection(|conn| {
            use queries::LedgerQueries;
            conn.get_ledger_hash(seq)
        })
    }

    /// Deletes old ledger headers up to and including `max_ledger`.
    ///
    /// Removes at most `count` entries. Used by the Maintainer for garbage
    /// collection of old ledger history.
    pub fn delete_old_ledger_headers(&self, max_ledger: u32, count: u32) -> Result<u32> {
        self.with_connection(|conn| {
            use queries::LedgerQueries;
            conn.delete_old_ledger_headers(max_ledger, count)
        })
    }

    /// Returns the stored network passphrase, if set.
    ///
    /// The network passphrase identifies the Stellar network (mainnet, testnet, etc.)
    /// and is used in transaction signing.
    pub fn get_network_passphrase(&self) -> Result<Option<String>> {
        self.with_connection(|conn| {
            use queries::StateQueries;
            conn.get_state(schema::state_keys::NETWORK_PASSPHRASE)
        })
    }

    /// Stores the network passphrase.
    ///
    /// This should be set once when the node is first initialized and should
    /// match the network the node is connecting to.
    pub fn set_network_passphrase(&self, passphrase: &str) -> Result<()> {
        self.with_connection(|conn| {
            use queries::StateQueries;
            conn.set_state(schema::state_keys::NETWORK_PASSPHRASE, passphrase)
        })
    }
}
