//! Database schema migrations.
//!
//! This module provides a migration system for upgrading the database schema
//! between versions. Each migration is a SQL script that transforms the schema
//! from one version to the next.
//!
//! # Migration Strategy
//!
//! Migrations are applied sequentially, one version at a time. Each migration
//! is executed in a transaction to ensure atomicity. If a migration fails,
//! the database is left in its previous state.
//!
//! # Adding New Migrations
//!
//! To add a new migration:
//!
//! 1. Increment [`CURRENT_VERSION`]
//! 2. Add a new `Migration` entry to the `MIGRATIONS` array
//! 3. The `from_version` should be the previous `CURRENT_VERSION`
//! 4. Provide idempotent SQL (use `IF NOT EXISTS`, `IF EXISTS`, etc.)
//!
//! # Version Compatibility
//!
//! The migration system will refuse to open a database with a schema version
//! newer than [`CURRENT_VERSION`], preventing data corruption from version
//! mismatches.

use crate::{DbError, Result};
use rusqlite::Connection;
use tracing::info;

/// Current database schema version.
///
/// This should be incremented whenever a new migration is added.
/// The database initialization and migration system uses this to
/// determine if upgrades are needed.
pub(crate) const CURRENT_VERSION: i32 = 10;

/// Represents a single database migration.
///
/// Each migration transforms the schema from one version to the next.
/// Migrations should be designed to be idempotent where possible.
struct Migration {
    /// The schema version this migration upgrades FROM.
    from_version: i32,
    /// The schema version this migration upgrades TO.
    to_version: i32,
    /// SQL statements to execute for the upgrade.
    ///
    /// Should use `IF NOT EXISTS`/`IF EXISTS` for idempotency.
    upgrade_sql: &'static str,
    /// Human-readable description of what this migration does.
    description: &'static str,
}

/// Registry of all available migrations.
///
/// Migrations are ordered by version and applied sequentially.
/// Each migration must have `from_version` equal to the previous
/// migration's `to_version`.
const MIGRATIONS: &[Migration] = &[
    Migration {
        from_version: 1,
        to_version: 2,
        upgrade_sql: r#"
            CREATE TABLE IF NOT EXISTS txsets (
                ledgerseq INTEGER PRIMARY KEY,
                data BLOB NOT NULL
            );
            CREATE TABLE IF NOT EXISTS txresults (
                ledgerseq INTEGER PRIMARY KEY,
                data BLOB NOT NULL
            );
        "#,
        description: "Add txsets and txresults tables for history publishing",
    },
    Migration {
        from_version: 2,
        to_version: 3,
        upgrade_sql: r#"
            CREATE TABLE IF NOT EXISTS bucketlist (
                ledgerseq INTEGER NOT NULL,
                level INTEGER NOT NULL,
                currhash TEXT NOT NULL,
                snaphash TEXT NOT NULL,
                PRIMARY KEY (ledgerseq, level)
            );
            CREATE INDEX IF NOT EXISTS bucketlist_ledger ON bucketlist(ledgerseq);
        "#,
        description: "Add bucket list table for checkpoint snapshots",
    },
    Migration {
        from_version: 3,
        to_version: 4,
        upgrade_sql: r#"
            CREATE TABLE IF NOT EXISTS publishqueue (
                ledgerseq INTEGER PRIMARY KEY,
                state TEXT NOT NULL
            );
        "#,
        description: "Add publish queue table for history publishing",
    },
    Migration {
        from_version: 4,
        to_version: 5,
        upgrade_sql: r#"
            ALTER TABLE scphistory RENAME TO scphistory_old;
            CREATE TABLE IF NOT EXISTS scphistory (
                nodeid TEXT NOT NULL,
                ledgerseq INTEGER NOT NULL,
                envelope BLOB NOT NULL
            );
            CREATE INDEX IF NOT EXISTS scphistory_ledger ON scphistory(ledgerseq);
            INSERT INTO scphistory (nodeid, ledgerseq, envelope)
                SELECT nodeid, ledgerseq, envelope FROM scphistory_old;
            DROP TABLE scphistory_old;
        "#,
        description: "Allow multiple SCP envelopes per node and ledger",
    },
    Migration {
        from_version: 5,
        to_version: 6,
        upgrade_sql: r#"
            CREATE TABLE IF NOT EXISTS events (
                id TEXT PRIMARY KEY NOT NULL,
                ledgerseq INTEGER NOT NULL,
                tx_index INTEGER NOT NULL,
                op_index INTEGER NOT NULL,
                tx_hash TEXT NOT NULL,
                contract_id TEXT,
                event_type INTEGER NOT NULL,
                topic1 TEXT,
                topic2 TEXT,
                topic3 TEXT,
                topic4 TEXT,
                event_xdr TEXT NOT NULL,
                in_successful_contract_call INTEGER NOT NULL DEFAULT 1
            );
            CREATE INDEX IF NOT EXISTS events_ledger ON events(ledgerseq);
            CREATE INDEX IF NOT EXISTS events_contract ON events(contract_id, ledgerseq);
        "#,
        description: "Add events table for contract event indexing",
    },
    Migration {
        from_version: 6,
        to_version: 7,
        upgrade_sql: r#"
            CREATE TABLE IF NOT EXISTS ledger_close_meta (
                sequence INTEGER PRIMARY KEY,
                meta BLOB NOT NULL
            );
        "#,
        description: "Add ledger_close_meta table for full LedgerCloseMeta storage",
    },
    Migration {
        from_version: 7,
        to_version: 8,
        upgrade_sql: r#"
            ALTER TABLE txhistory ADD COLUMN status INTEGER NOT NULL DEFAULT 0;
        "#,
        description: "Add status column to txhistory for DB-level transaction status filtering",
    },
    Migration {
        from_version: 8,
        to_version: 9,
        upgrade_sql: r#"
            CREATE INDEX IF NOT EXISTS events_topic1_id ON events(topic1, id);
        "#,
        description: "Add topic1+id index for efficient topic-filtered event queries",
    },
    Migration {
        from_version: 9,
        to_version: 10,
        upgrade_sql: r#"
            CREATE INDEX IF NOT EXISTS txhistory_status_ledger ON txhistory(status, ledgerseq, txindex);
        "#,
        description:
            "Add status+ledger+txindex index for efficient status-filtered transaction queries",
    },
];

/// Retrieves the current schema version from the database.
///
/// Returns version 1 if no version is recorded (assumed to be the initial
/// schema before versioning was introduced).
///
/// # Errors
///
/// Returns an error if the stored version string cannot be parsed as an integer.
pub(crate) fn get_schema_version(conn: &Connection) -> Result<i32> {
    let result: std::result::Result<String, _> = conn.query_row(
        "SELECT state FROM storestate WHERE statename = 'databaseschema'",
        [],
        |row| row.get(0),
    );

    match result {
        Ok(version_str) => version_str
            .parse()
            .map_err(|_| DbError::Migration(format!("Invalid schema version: {}", version_str))),
        Err(rusqlite::Error::QueryReturnedNoRows) => {
            // No schema version recorded - assume version 1 (initial)
            Ok(1)
        }
        Err(e) => Err(e.into()),
    }
}

/// Records the schema version in the database.
///
/// This is called after each successful migration to update the version tracker.
pub(crate) fn set_schema_version(conn: &Connection, version: i32) -> Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO storestate (statename, state) VALUES ('databaseschema', ?)",
        [version.to_string()],
    )?;
    Ok(())
}

/// Checks if the database requires migration.
///
/// Returns `true` if the database schema version is older than [`CURRENT_VERSION`].
pub(crate) fn needs_migration(conn: &Connection) -> Result<bool> {
    let current = get_schema_version(conn)?;
    Ok(current < CURRENT_VERSION)
}

/// Runs all necessary migrations to bring the database up to date.
///
/// Migrations are applied sequentially, each in its own transaction.
/// If the database is already at [`CURRENT_VERSION`], this is a no-op.
///
/// # Errors
///
/// Returns an error if:
/// - The database version is newer than [`CURRENT_VERSION`]
/// - A required migration is not found
/// - Any migration SQL fails to execute
pub(crate) fn run_migrations(conn: &Connection) -> Result<()> {
    let mut current_version = get_schema_version(conn)?;

    if current_version == CURRENT_VERSION {
        info!("Database is up to date at version {}", current_version);
        return Ok(());
    }

    if current_version > CURRENT_VERSION {
        return Err(DbError::Migration(format!(
            "Database version {} is newer than supported version {}",
            current_version, CURRENT_VERSION
        )));
    }

    info!(
        "Migrating database from version {} to {}",
        current_version, CURRENT_VERSION
    );

    while current_version < CURRENT_VERSION {
        let migration = MIGRATIONS
            .iter()
            .find(|m| m.from_version == current_version)
            .ok_or_else(|| {
                DbError::Migration(format!(
                    "No migration found from version {}",
                    current_version
                ))
            })?;

        info!(
            "Applying migration {} -> {}: {}",
            migration.from_version, migration.to_version, migration.description
        );

        // Execute the migration in a transaction
        let tx = conn.unchecked_transaction()?;
        tx.execute_batch(migration.upgrade_sql)?;
        set_schema_version(&tx, migration.to_version)?;
        tx.commit()?;

        current_version = migration.to_version;
        info!("Migration complete, now at version {}", current_version);
    }

    info!(
        "All migrations complete, database at version {}",
        CURRENT_VERSION
    );
    Ok(())
}

/// Verifies the database schema is compatible with this software version.
///
/// This is called during initialization to ensure the database can be safely used.
///
/// # Errors
///
/// Returns an error if the schema version is:
/// - Older than [`CURRENT_VERSION`] (migrations should be run first)
/// - Newer than [`CURRENT_VERSION`] (software upgrade required)
pub(crate) fn verify_schema(conn: &Connection) -> Result<()> {
    let version = get_schema_version(conn)?;

    if version < CURRENT_VERSION {
        return Err(DbError::Migration(format!(
            "Database schema version {} is too old, run migrations first",
            version
        )));
    }

    if version > CURRENT_VERSION {
        return Err(DbError::Migration(format!(
            "Database schema version {} is newer than this software supports ({})",
            version, CURRENT_VERSION
        )));
    }

    Ok(())
}

/// Initializes a fresh database with the current schema.
///
/// This creates all tables from [`CREATE_SCHEMA`](crate::schema::CREATE_SCHEMA)
/// and sets the schema version to [`CURRENT_VERSION`].
///
/// Should only be called on empty databases. For existing databases,
/// use [`run_migrations`] instead.
pub(crate) fn initialize_schema(conn: &Connection) -> Result<()> {
    // Create the schema
    conn.execute_batch(crate::schema::CREATE_SCHEMA)?;

    // Set the schema version
    set_schema_version(conn, CURRENT_VERSION)?;

    info!(
        "Initialized database with schema version {}",
        CURRENT_VERSION
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    fn setup_test_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(crate::schema::CREATE_SCHEMA).unwrap();
        conn
    }

    #[test]
    fn test_get_schema_version_default() {
        let conn = setup_test_db();
        let version = get_schema_version(&conn).unwrap();
        assert_eq!(version, 1);
    }

    #[test]
    fn test_set_and_get_schema_version() {
        let conn = setup_test_db();
        set_schema_version(&conn, 5).unwrap();
        let version = get_schema_version(&conn).unwrap();
        assert_eq!(version, 5);
    }

    #[test]
    fn test_needs_migration() {
        let conn = setup_test_db();
        set_schema_version(&conn, CURRENT_VERSION).unwrap();
        assert!(!needs_migration(&conn).unwrap());
    }

    #[test]
    fn test_verify_schema_current() {
        let conn = setup_test_db();
        set_schema_version(&conn, CURRENT_VERSION).unwrap();
        assert!(verify_schema(&conn).is_ok());
    }

    #[test]
    fn test_verify_schema_too_old() {
        let conn = setup_test_db();
        set_schema_version(&conn, CURRENT_VERSION - 1).unwrap();
        // Only fails if CURRENT_VERSION > 1
        if CURRENT_VERSION > 1 {
            assert!(verify_schema(&conn).is_err());
        }
    }

    #[test]
    fn test_migration_8_to_9_adds_topic1_id_index() {
        let conn = Connection::open_in_memory().unwrap();
        // Set up a minimal v8 schema with the events and txhistory tables
        conn.execute_batch(
            r#"
            CREATE TABLE storestate (statename TEXT PRIMARY KEY, state TEXT);
            CREATE TABLE events (
                id TEXT PRIMARY KEY NOT NULL,
                ledgerseq INTEGER NOT NULL,
                tx_index INTEGER NOT NULL,
                op_index INTEGER NOT NULL,
                tx_hash TEXT NOT NULL,
                contract_id TEXT,
                event_type INTEGER NOT NULL,
                topic1 TEXT,
                topic2 TEXT,
                topic3 TEXT,
                topic4 TEXT,
                event_xdr TEXT NOT NULL,
                in_successful_contract_call INTEGER NOT NULL DEFAULT 1
            );
            CREATE INDEX events_ledger ON events(ledgerseq);
            CREATE INDEX events_contract ON events(contract_id, ledgerseq);
            CREATE TABLE txhistory (
                txid TEXT PRIMARY KEY,
                ledgerseq INTEGER NOT NULL,
                txindex INTEGER NOT NULL,
                txbody BLOB NOT NULL,
                txresult BLOB NOT NULL,
                txmeta BLOB,
                status INTEGER NOT NULL DEFAULT 0
            );
            CREATE INDEX txhistory_ledger ON txhistory(ledgerseq);
            "#,
        )
        .unwrap();
        set_schema_version(&conn, 8).unwrap();

        // Apply migration
        run_migrations(&conn).unwrap();

        // Verify index exists
        let index_exists: bool = conn
            .query_row(
                "SELECT COUNT(*) > 0 FROM pragma_index_list('events') WHERE name = 'events_topic1_id'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(
            index_exists,
            "events_topic1_id index should exist after migration"
        );
        assert_eq!(get_schema_version(&conn).unwrap(), CURRENT_VERSION);
    }

    #[test]
    fn test_migration_9_to_10_adds_status_ledger_index() {
        let conn = Connection::open_in_memory().unwrap();
        // Set up a minimal v9 schema with the txhistory table
        conn.execute_batch(
            r#"
            CREATE TABLE storestate (statename TEXT PRIMARY KEY, state TEXT);
            CREATE TABLE txhistory (
                txid TEXT PRIMARY KEY,
                ledgerseq INTEGER NOT NULL,
                txindex INTEGER NOT NULL,
                txbody BLOB NOT NULL,
                txresult BLOB NOT NULL,
                txmeta BLOB,
                status INTEGER NOT NULL DEFAULT 0
            );
            CREATE INDEX txhistory_ledger ON txhistory(ledgerseq);
            "#,
        )
        .unwrap();
        set_schema_version(&conn, 9).unwrap();

        // Apply migration
        run_migrations(&conn).unwrap();

        // Verify index exists
        let index_exists: bool = conn
            .query_row(
                "SELECT COUNT(*) > 0 FROM pragma_index_list('txhistory') WHERE name = 'txhistory_status_ledger'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(
            index_exists,
            "txhistory_status_ledger index should exist after migration"
        );

        // Verify index column order: status, ledgerseq, txindex
        let columns: Vec<String> = conn
            .prepare("SELECT name FROM pragma_index_info('txhistory_status_ledger') ORDER BY seqno")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<std::result::Result<_, _>>()
            .unwrap();
        assert_eq!(columns, vec!["status", "ledgerseq", "txindex"]);

        assert_eq!(get_schema_version(&conn).unwrap(), CURRENT_VERSION);
    }
}
