//! Database error types.
//!
//! This module defines the error types used throughout the database layer.
//! All errors are consolidated into the [`DbError`] enum which provides
//! automatic conversion from underlying error types.

use thiserror::Error;

/// Errors that can occur during database operations.
///
/// This enum consolidates all error types from the database layer, providing
/// a unified error type for callers. Most variants wrap underlying errors
/// from SQLite, the connection pool, or XDR serialization.
///
/// # Error Categories
///
/// - **Infrastructure errors**: [`Sqlite`](DbError::Sqlite), [`Pool`](DbError::Pool),
///   [`Io`](DbError::Io) - failures in the underlying systems
/// - **Data errors**: [`Xdr`](DbError::Xdr), [`Integrity`](DbError::Integrity),
///   [`NotFound`](DbError::NotFound) - problems with data format or existence
/// - **Schema errors**: [`Migration`](DbError::Migration) - schema version incompatibilities
#[derive(Error, Debug)]
pub enum DbError {
    /// SQLite database error.
    ///
    /// Wraps errors from rusqlite including query failures, constraint
    /// violations, and database corruption.
    #[error("SQLite error: {0}")]
    Sqlite(#[from] rusqlite::Error),

    /// Connection pool error.
    ///
    /// Occurs when a connection cannot be obtained from the pool,
    /// typically due to pool exhaustion or configuration issues.
    #[error("Pool error: {0}")]
    Pool(#[from] r2d2::Error),

    /// File system I/O error.
    ///
    /// Occurs during database file operations such as creating the
    /// database file or its parent directory.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// XDR serialization/deserialization error.
    ///
    /// Occurs when reading or writing Stellar XDR-encoded data to/from
    /// the database. This can indicate data corruption or version mismatch.
    #[error("XDR error: {0}")]
    Xdr(#[from] stellar_xdr::Error),

    /// Requested data was not found.
    ///
    /// Unlike [`Sqlite`](DbError::Sqlite) errors for missing rows, this is used when
    /// the absence of data is unexpected and indicates a problem.
    #[error("Not found: {0}")]
    NotFound(String),

    /// Data integrity violation.
    ///
    /// Indicates that data in the database is in an unexpected state,
    /// such as invalid hash formats, missing required fields, or
    /// inconsistent relationships between records.
    #[error("Integrity error: {0}")]
    Integrity(String),

    /// Schema migration error.
    ///
    /// Occurs during database initialization or upgrade when the schema
    /// version is incompatible or a migration fails to apply.
    #[error("Migration error: {0}")]
    Migration(String),

    /// Query exceeded its VM-step budget.
    ///
    /// Returned when a query's SQLite virtual-machine instruction count
    /// exceeds the configured limit. This is a defense-in-depth mechanism
    /// to prevent expensive unindexed scans from monopolizing DB workers.
    #[error("query exceeded computational budget")]
    QueryBudgetExceeded,
}

impl DbError {
    /// Returns `true` iff this is a transient SQLite busy/locked
    /// (`SQLITE_BUSY` / `SQLITE_LOCKED`, "database is locked"), #3497.
    ///
    /// SQLite write transactions are atomic: a `DatabaseBusy`/`DatabaseLocked`
    /// means the transaction NEVER committed, so the on-disk state is
    /// consistent and the write can be safely re-issued (or, at a
    /// log-and-continue site, safely abandoned as a *known* loss rather than
    /// treated as corruption). This is the recoverable, environmental class.
    ///
    /// The match is NARROW by design (the load-bearing consensus-safety
    /// guard): only the two busy/locked primary `ErrorCode`s are recoverable.
    /// Every other SQLite code (`DatabaseCorrupt`, `SystemIoFailure`, …) and
    /// every other [`DbError`] variant (`Integrity`, `Xdr`, …) stay on the
    /// fatal path — genuine corruption must NEVER be reclassified recoverable.
    /// Mirrors the `is_query_interrupted` shape in `crate::queries` (matching
    /// the structured `ErrorCode`, NOT the message string).
    ///
    /// Lives here, on the error type, rather than in a single consumer crate
    /// so that every caller shares ONE definition of "transient": `crates/app`
    /// (`is_transient_db_busy`, ledger-close/persist/maintenance) and
    /// `crates/history` (catchup `emit_meta`, #3801) both delegate to it.
    pub fn is_transient_busy(&self) -> bool {
        matches!(
            self,
            DbError::Sqlite(rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error {
                    code: rusqlite::ffi::ErrorCode::DatabaseBusy
                        | rusqlite::ffi::ErrorCode::DatabaseLocked,
                    ..
                },
                _,
            ))
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a `DbError::Sqlite(SqliteFailure(ffi::Error { code, .. }, msg))`
    /// for the given primary code, mirroring how rusqlite materializes a
    /// SQLite error on a contended write.
    fn sqlite_error(code: rusqlite::ffi::ErrorCode, msg: &str) -> DbError {
        DbError::Sqlite(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error {
                code,
                extended_code: 0,
            },
            Some(msg.to_string()),
        ))
    }

    /// Both busy/locked primary codes are the recoverable, environmental class.
    #[test]
    fn test_is_transient_busy_matches_busy_and_locked() {
        assert!(
            sqlite_error(rusqlite::ffi::ErrorCode::DatabaseBusy, "database is locked")
                .is_transient_busy(),
            "SQLITE_BUSY must classify as transient"
        );
        assert!(
            sqlite_error(
                rusqlite::ffi::ErrorCode::DatabaseLocked,
                "database table is locked"
            )
            .is_transient_busy(),
            "SQLITE_LOCKED must classify as transient"
        );
    }

    /// The narrow-by-design boundary: genuine corruption and every non-SQLite
    /// variant must NEVER be reclassified as recoverable.
    #[test]
    fn test_is_transient_busy_rejects_corrupt_and_non_sqlite() {
        assert!(
            !sqlite_error(
                rusqlite::ffi::ErrorCode::DatabaseCorrupt,
                "database disk image is malformed"
            )
            .is_transient_busy(),
            "SQLITE_CORRUPT must stay on the fatal path"
        );
        assert!(
            !sqlite_error(rusqlite::ffi::ErrorCode::SystemIoFailure, "disk I/O error")
                .is_transient_busy(),
            "SQLITE_IOERR must stay on the fatal path"
        );
        assert!(
            !DbError::Integrity("bad hash".to_string()).is_transient_busy(),
            "Integrity must stay on the fatal path"
        );
        assert!(
            !DbError::Xdr(stellar_xdr::Error::Invalid).is_transient_busy(),
            "Xdr must stay on the fatal path"
        );
        assert!(
            !DbError::NotFound("row".to_string()).is_transient_busy(),
            "NotFound must stay on the fatal path"
        );
        assert!(
            !DbError::QueryBudgetExceeded.is_transient_busy(),
            "QueryBudgetExceeded must stay on the fatal path"
        );
        assert!(
            !DbError::Sqlite(rusqlite::Error::QueryReturnedNoRows).is_transient_busy(),
            "a non-SqliteFailure rusqlite error must stay on the fatal path"
        );
    }
}
