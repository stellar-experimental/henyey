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

/// WAL auto-checkpoint threshold in pages.
///
/// **Documented divergence from stellar-core** (per docs/PARITY.md this layer
/// is divergeable): core keeps SQLite's inline auto-checkpoint at 10000 pages
/// (`Database.cpp:169`) and has no background checkpointer. henyey instead
/// checkpoints from a background app task ([`Database::wal_checkpoint_passive`]
/// every ~2 s) and sets the inline threshold very high (~1 GiB) so it acts
/// purely as a disaster floor.
///
/// Why not core's 10000: henyey's ledger-close persist writes 10-30 MB of WAL
/// per ledger (per-tx rows with meta XDR; core writes ~0.1 MB since it dropped
/// per-tx SQL storage in v21), so a 10000-page (~40 MB) threshold fired every
/// couple of closes INSIDE the persist commit — the committing writer copied
/// and fsynced tens of MB while holding the WAL write lock, stalling closes
/// 4-16 s under sustained load (maxtps forensics 2026-07-03). Under max load
/// the WAL legitimately exceeds 40 MB even with the background task draining,
/// so restoring 10000 would re-trigger inline stalls.
///
/// Why not 0: with inline checkpoints fully disabled, a dead or starved
/// background checkpointer would let the WAL grow without bound → disk
/// exhaustion → validator death (review of #3712; cf. the ENOSPC fatality
/// history in #3478). At this floor, if the background task dies the node
/// degrades to occasional large inline checkpoints (a stall, but bounded
/// disk) instead of unbounded growth. The floor is never expected to fire
/// while the background checkpointer (plus its TRUNCATE backstop) is healthy.
const WAL_AUTOCHECKPOINT_PAGES: u32 = 262_144; // ~1 GiB at 4 KiB pages

/// Applies the per-connection SQLite PRAGMAs shared by the file-backed and
/// in-memory open paths.
///
/// These are set via the pool's `with_init` callback so every pooled
/// connection gets the same configuration. Note that `journal_mode` is
/// database-level (persistent) and is therefore set once in [`Database::initialize`]
/// rather than here.
fn init_connection_pragmas(conn: &mut rusqlite::Connection) -> rusqlite::Result<()> {
    // `synchronous = NORMAL` is a **documented divergence** from stellar-core,
    // not parity: core leaves SQLite at its default FULL for validators —
    // `Database.cpp:164-166` comments the NORMAL pragma out ("FULL is needed
    // as to ensure durability / NORMAL is enough for non validating nodes").
    // In WAL mode FULL fsyncs the WAL on every commit; henyey's ledger-close
    // persist writes thousands of tx rows per ledger (core writes ~0.1 MB),
    // and the resulting multi-MB fsync per close stalled commits for 4-16 s
    // on saturated NVMe under sustained load (WAL write-lock holder
    // forensics, maxtps 2026-07-03). NORMAL is determinism-safe: a power
    // loss can drop the newest commits but cannot corrupt or fork the DB —
    // restart recovers via catchup with no hash divergence vs peers.
    conn.execute_batch(&format!(
        "PRAGMA busy_timeout = {};\
         PRAGMA synchronous = NORMAL;\
         PRAGMA foreign_keys = ON;\
         PRAGMA cache_size = {};\
         PRAGMA wal_autocheckpoint = {};\
         PRAGMA temp_store = MEMORY;",
        BUSY_TIMEOUT_MS, CACHE_SIZE_KIB, WAL_AUTOCHECKPOINT_PAGES
    ))
}

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

        let manager =
            r2d2_sqlite::SqliteConnectionManager::file(path).with_init(init_connection_pragmas);
        let pool = r2d2::Pool::builder()
            .max_size(POOL_MAX_SIZE)
            .connection_timeout(std::time::Duration::from_secs(CONNECTION_TIMEOUT_SECS))
            .build(manager)?;

        let db = Self { pool };
        db.initialize()?;
        Ok(db)
    }

    /// Opens an in-memory database.
    ///
    /// Used for captive-core / `--in-memory` mode as well as tests.
    /// The database is initialized with the current schema but data is not
    /// persisted across restarts. The connection pool size is limited to 1
    /// since in-memory databases are connection-specific.
    pub fn open_in_memory() -> Result<Self> {
        let manager =
            r2d2_sqlite::SqliteConnectionManager::memory().with_init(init_connection_pragmas);
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

        // journal_mode is database-level (persistent), so it only needs to be set once.
        // Per-connection PRAGMAs (synchronous, foreign_keys, cache_size, temp_store,
        // busy_timeout) are applied via the pool's with_init callback to ensure every
        // pooled connection gets them.
        conn.execute_batch("PRAGMA journal_mode = WAL;")?;

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
    pub fn get_ledger_header(&self, seq: u32) -> Result<Option<stellar_xdr::LedgerHeader>> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_open_in_memory_applies_pragmas() {
        let db = Database::open_in_memory().unwrap();
        db.with_connection(|conn| {
            let fk: bool = conn
                .query_row("PRAGMA foreign_keys", [], |r| r.get(0))
                .unwrap();
            assert!(fk, "foreign_keys should be ON");

            let cache: i64 = conn
                .query_row("PRAGMA cache_size", [], |r| r.get(0))
                .unwrap();
            assert_eq!(cache, CACHE_SIZE_KIB as i64);

            let busy: u32 = conn
                .query_row("PRAGMA busy_timeout", [], |r| r.get(0))
                .unwrap();
            assert_eq!(busy, BUSY_TIMEOUT_MS);

            Ok(())
        })
        .unwrap();
    }

    /// #3640: every pooled connection must read back
    /// `PRAGMA wal_autocheckpoint = 10000` (parity with
    /// `stellar-core/src/database/Database.cpp:169`; the SQLite default is
    /// 1000). Setting it via the `with_init` callback proves the pragma
    /// reaches every connection drawn from the pool — including the
    /// ledger-close write connection — which is what reduces in-line
    /// WAL-checkpoint contention frequency. Asserting on a file-backed DB
    /// (the production path) because `wal_autocheckpoint` is a WAL-mode
    /// pragma.
    #[test]
    fn wal_autocheckpoint_pragma_applied_to_pooled_connection_3640() {
        let tmp = tempfile::tempdir().unwrap();
        let db = Database::open(tmp.path().join("test.db")).unwrap();
        db.with_connection(|conn| {
            let autockpt: i64 = conn
                .pragma_query_value(None, "wal_autocheckpoint", |r| r.get(0))
                .unwrap();
            assert_eq!(
                autockpt, WAL_AUTOCHECKPOINT_PAGES as i64,
                "wal_autocheckpoint must be 10000 (parity with stellar-core)"
            );
            Ok(())
        })
        .unwrap();
    }

    /// #3812: `cleanup_ahead_of_lcl` must anchor on the durable LCL
    /// (`lastclosedledger`) and truncate every history row strictly above it,
    /// returning the number of rows removed. FAILS on origin/main: no cleanup
    /// exists, so `MAX(ledgerseq)` would stay at 110 (the issue's divergence).
    #[test]
    fn test_cleanup_ahead_of_lcl_anchors_on_durable_lcl() {
        use crate::queries::StateQueries;

        let db = Database::open_in_memory().unwrap();
        db.with_connection(|conn| {
            conn.set_last_closed_ledger(100)?;
            for seq in 1..=110u32 {
                conn.execute(
                    "INSERT INTO ledgerheaders \
                     (ledgerhash, prevhash, bucketlisthash, ledgerseq, closetime, data) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    rusqlite::params![
                        format!("h{seq}"),
                        format!("p{seq}"),
                        format!("b{seq}"),
                        seq,
                        0i64,
                        vec![0u8]
                    ],
                )?;
            }
            Ok(())
        })
        .unwrap();

        // Only seq 101..=110 are ahead of the durable LCL → 10 rows removed.
        assert_eq!(db.cleanup_ahead_of_lcl().unwrap(), Some(10));
        assert_eq!(db.get_latest_ledger_seq().unwrap(), Some(100));
    }

    /// #3812: with no durable LCL yet (fresh/legacy DB), `cleanup_ahead_of_lcl`
    /// must be a no-op returning `None` — there is no authoritative anchor to
    /// truncate against. FAILS on origin/main: method does not exist.
    #[test]
    fn test_cleanup_ahead_of_lcl_noop_without_durable_lcl() {
        let db = Database::open_in_memory().unwrap();
        db.with_connection(|conn| {
            for seq in 1..=5u32 {
                conn.execute(
                    "INSERT INTO ledgerheaders \
                     (ledgerhash, prevhash, bucketlisthash, ledgerseq, closetime, data) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    rusqlite::params![
                        format!("h{seq}"),
                        format!("p{seq}"),
                        format!("b{seq}"),
                        seq,
                        0i64,
                        vec![0u8]
                    ],
                )?;
            }
            Ok(())
        })
        .unwrap();

        assert_eq!(db.cleanup_ahead_of_lcl().unwrap(), None);
        assert_eq!(db.get_latest_ledger_seq().unwrap(), Some(5));
    }

    #[test]
    fn test_open_in_memory_initializes_schema() {
        let db = Database::open_in_memory().unwrap();
        db.with_connection(|conn| {
            let tables: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='storestate'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(tables, 1);
            Ok(())
        })
        .unwrap();
    }
}
