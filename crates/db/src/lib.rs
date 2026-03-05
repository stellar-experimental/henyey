//! Database abstraction layer for rs-stellar-core.
//!
//! This crate provides SQLite-based persistence for the Stellar blockchain node,
//! handling storage and retrieval of:
//!
//! - **Ledger headers**: Block metadata including sequence numbers, hashes, and timestamps
//! - **Transaction history**: Transaction bodies, results, and metadata
//! - **SCP state**: Stellar Consensus Protocol envelopes and quorum sets
//! - **Bucket list snapshots**: Merkle tree state at checkpoint ledgers
//! - **Peer records**: Network peer discovery and connection tracking
//! - **Operational state**: Configuration and runtime state persistence
//!
//! # Architecture
//!
//! The crate is organized into several modules:
//!
//! - [`pool`]: Connection pool management using r2d2
//! - [`schema`]: Database schema definitions and table layouts
//! - [`migrations`]: Schema versioning and migration system
//! - [`queries`]: Typed query traits for each data domain
//! - [`error`]: Error types for database operations
//!
//! # Usage
//!
//! ```no_run
//! use henyey_db::Database;
//!
//! // Open a database (creates if it doesn't exist)
//! let db = Database::open("path/to/stellar.db")?;
//!
//! // Or use an in-memory database for testing
//! let test_db = Database::open_in_memory()?;
//!
//! // Query the latest ledger
//! if let Some(seq) = db.get_latest_ledger_seq()? {
//!     println!("Latest ledger: {}", seq);
//! }
//! # Ok::<(), henyey_db::DbError>(())
//! ```
//!
//! # Query Traits
//!
//! Query functionality is organized into domain-specific traits that extend
//! [`rusqlite::Connection`]. The [`Database`] type provides convenience methods
//! that wrap these traits for common operations.
//!
//! For advanced use cases, you can obtain a connection and use the traits directly:
//!
//! ```no_run
//! use henyey_db::{Database, queries::LedgerQueries};
//!
//! let db = Database::open_in_memory()?;
//! db.with_connection(|conn| {
//!     // Use trait methods directly on the connection
//!     let seq = conn.get_latest_ledger_seq()?;
//!     Ok(seq)
//! })?;
//! # Ok::<(), henyey_db::DbError>(())
//! ```

pub mod error;
pub mod migrations;
pub mod pool;
pub mod queries;
pub mod schema;
pub mod scp_persistence;

pub use error::DbError;
pub use migrations::{needs_migration, run_migrations, verify_schema, CURRENT_VERSION};
pub use pool::{Database, PooledConnection};
pub use queries::*;
pub use scp_persistence::SqliteScpPersistence;

use std::path::Path;
use stellar_xdr::curr::{TransactionHistoryEntry, TransactionHistoryResultEntry};
use tracing::info;

/// Result type for database operations.
pub type Result<T> = std::result::Result<T, DbError>;

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

        // Ensure parent directory exists
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }

        let manager = r2d2_sqlite::SqliteConnectionManager::file(path).with_init(|conn| {
            // Set busy_timeout on every new connection for lock contention handling
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

        // Configure SQLite for durability and performance:
        // - WAL mode for concurrent reads during writes
        // - FULL sync for durability (required for WAL mode to survive OS crashes;
        //   NORMAL only guarantees app-crash safety, not power-loss safety)
        // - 64MB cache for frequently accessed pages
        // - Foreign keys for referential integrity
        // - Memory-based temp storage for performance
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

        // Check if this is a fresh database by looking for the storestate table
        let tables_exist: bool = conn
            .query_row(
                "SELECT COUNT(*) > 0 FROM sqlite_master WHERE type='table' AND name='storestate'",
                [],
                |row| row.get(0),
            )
            .unwrap_or(false);

        if tables_exist {
            // Existing database - check version and run migrations if needed
            if migrations::needs_migration(&conn)? {
                info!("Database requires migration");
                migrations::run_migrations(&conn)?;
            }
            migrations::verify_schema(&conn)?;
        } else {
            // Fresh database - initialize with current schema
            migrations::initialize_schema(&conn)?;
        }

        Ok(())
    }

    /// Upgrades the database schema to the latest version.
    ///
    /// This is typically called by the "upgrade-db" CLI command. Normal database
    /// initialization already handles migrations automatically.
    pub fn upgrade(&self) -> Result<()> {
        let conn = self.connection()?;
        migrations::run_migrations(&conn)
    }

    /// Returns the current database schema version.
    ///
    /// This can be used to check compatibility or diagnose migration issues.
    pub fn schema_version(&self) -> Result<i32> {
        let conn = self.connection()?;
        migrations::get_schema_version(&conn)
    }

    // =========================================================================
    // Ledger Operations
    // =========================================================================

    /// Returns the highest ledger sequence number stored in the database.
    ///
    /// Returns `None` if no ledgers have been stored yet.
    pub fn get_latest_ledger_seq(&self) -> Result<Option<u32>> {
        self.with_connection(|conn| {
            use queries::LedgerQueries;
            conn.get_latest_ledger_seq()
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

    // =========================================================================
    // Network Configuration
    // =========================================================================

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

    // =========================================================================
    // Transaction History
    // =========================================================================

    /// Returns the transaction set for a ledger.
    ///
    /// The transaction history entry contains all transactions that were
    /// included in the specified ledger.
    pub fn get_tx_history_entry(&self, seq: u32) -> Result<Option<TransactionHistoryEntry>> {
        self.with_connection(|conn| {
            use queries::HistoryQueries;
            conn.load_tx_history_entry(seq)
        })
    }

    /// Returns the transaction results for a ledger.
    ///
    /// Contains the execution results of all transactions in the ledger.
    pub fn get_tx_result_entry(&self, seq: u32) -> Result<Option<TransactionHistoryResultEntry>> {
        self.with_connection(|conn| {
            use queries::HistoryQueries;
            conn.load_tx_result_entry(seq)
        })
    }

    // =========================================================================
    // SCP (Stellar Consensus Protocol) State
    // =========================================================================

    /// Stores SCP envelopes for a ledger.
    ///
    /// SCP envelopes contain the consensus messages from validators that
    /// were used to agree on this ledger's contents.
    pub fn store_scp_history(
        &self,
        seq: u32,
        envelopes: &[stellar_xdr::curr::ScpEnvelope],
    ) -> Result<()> {
        self.with_connection(|conn| {
            use queries::ScpQueries;
            conn.store_scp_history(seq, envelopes)
        })
    }

    // =========================================================================
    // Maintenance / Cleanup Operations
    // =========================================================================

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

    /// Deletes old SCP history entries up to and including `max_ledger`.
    ///
    /// Removes at most `count` entries from both scphistory and scpquorums
    /// tables. Used by the Maintainer for garbage collection.
    pub fn delete_old_scp_entries(&self, max_ledger: u32, count: u32) -> Result<u32> {
        self.with_connection(|conn| {
            use queries::ScpQueries;
            conn.delete_old_scp_entries(max_ledger, count)
        })
    }

    /// Loads SCP envelopes for a ledger.
    ///
    /// Returns the consensus messages that were recorded for the specified ledger.
    pub fn load_scp_history(&self, seq: u32) -> Result<Vec<stellar_xdr::curr::ScpEnvelope>> {
        self.with_connection(|conn| {
            use queries::ScpQueries;
            conn.load_scp_history(seq)
        })
    }

    /// Stores a quorum set by its hash.
    ///
    /// Quorum sets define the trust configuration for SCP consensus.
    /// They are stored by hash and associated with the last ledger where
    /// they were seen, allowing for garbage collection of old quorum sets.
    pub fn store_scp_quorum_set(
        &self,
        hash: &henyey_common::Hash256,
        last_ledger_seq: u32,
        quorum_set: &stellar_xdr::curr::ScpQuorumSet,
    ) -> Result<()> {
        self.with_connection(|conn| {
            use queries::ScpQueries;
            conn.store_scp_quorum_set(hash, last_ledger_seq, quorum_set)
        })
    }

    /// Loads a quorum set by its hash.
    ///
    /// Returns `None` if no quorum set with the given hash is stored.
    pub fn load_scp_quorum_set(
        &self,
        hash: &henyey_common::Hash256,
    ) -> Result<Option<stellar_xdr::curr::ScpQuorumSet>> {
        self.with_connection(|conn| {
            use queries::ScpQueries;
            conn.load_scp_quorum_set(hash)
        })
    }

    // =========================================================================
    // Bucket List Snapshots
    // =========================================================================

    /// Stores bucket list snapshot levels for a ledger.
    ///
    /// The bucket list is a Merkle tree structure that stores all ledger entries.
    /// At checkpoint ledgers (every 64 ledgers), the bucket hashes are stored
    /// to enable state reconstruction during catchup.
    ///
    /// Each level contains a pair of hashes: (current bucket hash, snap bucket hash).
    pub fn store_bucket_list(
        &self,
        seq: u32,
        levels: &[(henyey_common::Hash256, henyey_common::Hash256)],
    ) -> Result<()> {
        self.with_connection(|conn| {
            use queries::BucketListQueries;
            conn.store_bucket_list(seq, levels)
        })
    }

    /// Loads bucket list snapshot levels for a ledger.
    ///
    /// Returns `None` if no bucket list snapshot exists for the given ledger.
    pub fn load_bucket_list(
        &self,
        seq: u32,
    ) -> Result<Option<Vec<(henyey_common::Hash256, henyey_common::Hash256)>>> {
        self.with_connection(|conn| {
            use queries::BucketListQueries;
            conn.load_bucket_list(seq)
        })
    }

    // =========================================================================
    // Peer Management
    // =========================================================================

    /// Loads peer records from the database.
    ///
    /// Returns a list of (host, port, record) tuples. Optionally limited to
    /// the specified number of peers.
    pub fn load_peers(
        &self,
        limit: Option<usize>,
    ) -> Result<Vec<(String, u16, queries::PeerRecord)>> {
        self.with_connection(|conn| {
            use queries::PeerQueries;
            conn.load_peers(limit)
        })
    }

    // =========================================================================
    // History Publishing Queue
    // =========================================================================

    /// Adds a checkpoint ledger to the publish queue with its HAS JSON.
    ///
    /// Checkpoint ledgers (every 64 ledgers) need to be published to history
    /// archives. This queue tracks which checkpoints are pending publication.
    /// The HAS JSON is captured at checkpoint time to preserve the exact
    /// bucket list state (including hot archive hashes) for publishing.
    pub fn enqueue_publish(&self, ledger_seq: u32, has_json: &str) -> Result<()> {
        self.with_connection(|conn| {
            use queries::PublishQueueQueries;
            conn.enqueue_publish(ledger_seq, has_json)
        })
    }

    /// Removes a checkpoint ledger from the publish queue.
    ///
    /// Called after successful publication to a history archive.
    pub fn remove_publish(&self, ledger_seq: u32) -> Result<()> {
        self.with_connection(|conn| {
            use queries::PublishQueueQueries;
            conn.remove_publish(ledger_seq)
        })
    }

    /// Removes all publish queue entries above the given LCL.
    ///
    /// Called during startup recovery to clean up stale entries that
    /// refer to checkpoints beyond what has been committed to the database.
    /// Returns the number of entries removed.
    pub fn remove_publish_above_lcl(&self, lcl: u32) -> Result<u64> {
        self.with_connection(|conn| {
            use queries::PublishQueueQueries;
            conn.remove_above_lcl(lcl)
        })
    }

    /// Loads queued checkpoint ledgers pending publication.
    ///
    /// Returns ledger sequence numbers in ascending order.
    pub fn load_publish_queue(&self, limit: Option<usize>) -> Result<Vec<u32>> {
        self.with_connection(|conn| {
            use queries::PublishQueueQueries;
            conn.load_publish_queue(limit)
        })
    }

    /// Loads the HAS JSON for a specific queued checkpoint.
    ///
    /// Returns the History Archive State JSON that was stored at enqueue
    /// time, or `None` if the checkpoint is not in the queue.
    pub fn load_publish_has(&self, ledger_seq: u32) -> Result<Option<String>> {
        self.with_connection(|conn| {
            use queries::PublishQueueQueries;
            conn.load_publish_has(ledger_seq)
        })
    }

    // =========================================================================
    // Node Ban List
    // =========================================================================

    /// Adds a node ID to the ban list.
    ///
    /// Banned nodes are excluded from consensus and peer connections.
    pub fn ban_node(&self, node_id: &str) -> Result<()> {
        self.with_connection(|conn| {
            use queries::BanQueries;
            conn.ban_node(node_id)
        })
    }

    /// Removes a node ID from the ban list.
    pub fn unban_node(&self, node_id: &str) -> Result<()> {
        self.with_connection(|conn| {
            use queries::BanQueries;
            conn.unban_node(node_id)
        })
    }

    /// Checks if a node ID is banned.
    pub fn is_banned(&self, node_id: &str) -> Result<bool> {
        self.with_connection(|conn| {
            use queries::BanQueries;
            conn.is_banned(node_id)
        })
    }

    /// Loads all banned node IDs.
    pub fn load_bans(&self) -> Result<Vec<String>> {
        self.with_connection(|conn| {
            use queries::BanQueries;
            conn.load_bans()
        })
    }

    // =========================================================================
    // Additional Peer Operations
    // =========================================================================

    /// Stores or updates a peer record.
    ///
    /// The peer record tracks connection metadata including failure count,
    /// next retry time, and peer type (inbound/outbound).
    pub fn store_peer(&self, host: &str, port: u16, record: queries::PeerRecord) -> Result<()> {
        self.with_connection(|conn| {
            use queries::PeerQueries;
            conn.store_peer(host, port, record)
        })
    }

    /// Loads a peer record by host and port.
    ///
    /// Returns `None` if the peer is not in the database.
    pub fn load_peer(&self, host: &str, port: u16) -> Result<Option<queries::PeerRecord>> {
        self.with_connection(|conn| {
            use queries::PeerQueries;
            conn.load_peer(host, port)
        })
    }

    /// Removes peers that have exceeded the failure threshold.
    ///
    /// This is used to garbage collect peers that consistently fail to connect.
    pub fn remove_peers_with_failures(&self, min_failures: u32) -> Result<()> {
        self.with_connection(|conn| {
            use queries::PeerQueries;
            conn.remove_peers_with_failures(min_failures)
        })
    }

    /// Loads random peers matching the specified constraints.
    ///
    /// Filters by maximum failures, next attempt time, and optionally peer type.
    /// Results are randomized to distribute connection attempts.
    pub fn load_random_peers(
        &self,
        limit: usize,
        max_failures: u32,
        now: i64,
        peer_type: Option<i32>,
    ) -> Result<Vec<(String, u16, queries::PeerRecord)>> {
        self.with_connection(|conn| {
            use queries::PeerQueries;
            conn.load_random_peers(limit, max_failures, now, peer_type)
        })
    }

    /// Loads random outbound peers (excludes the specified inbound type).
    ///
    /// Filters by maximum failures and next attempt time.
    pub fn load_random_peers_any_outbound(
        &self,
        limit: usize,
        max_failures: u32,
        now: i64,
        inbound_type: i32,
    ) -> Result<Vec<(String, u16, queries::PeerRecord)>> {
        self.with_connection(|conn| {
            use queries::PeerQueries;
            conn.load_random_peers_any_outbound(limit, max_failures, now, inbound_type)
        })
    }

    /// Loads random outbound peers by failure count only.
    ///
    /// Similar to [`Self::load_random_peers_any_outbound`] but ignores the next
    /// attempt time, useful for aggressive peer discovery.
    pub fn load_random_peers_any_outbound_max_failures(
        &self,
        limit: usize,
        max_failures: u32,
        inbound_type: i32,
    ) -> Result<Vec<(String, u16, queries::PeerRecord)>> {
        self.with_connection(|conn| {
            use queries::PeerQueries;
            conn.load_random_peers_any_outbound_max_failures(limit, max_failures, inbound_type)
        })
    }

    /// Loads random peers of a specific type by failure count only.
    ///
    /// Ignores next attempt time, useful for targeted peer type queries.
    pub fn load_random_peers_by_type_max_failures(
        &self,
        limit: usize,
        max_failures: u32,
        peer_type: i32,
    ) -> Result<Vec<(String, u16, queries::PeerRecord)>> {
        self.with_connection(|conn| {
            use queries::PeerQueries;
            conn.load_random_peers_by_type_max_failures(limit, max_failures, peer_type)
        })
    }
}
