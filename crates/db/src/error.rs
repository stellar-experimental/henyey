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
    /// Is this the transient, recoverable SQLite busy/locked class?
    ///
    /// A SQLite write transaction is atomic: a `DatabaseBusy`/`DatabaseLocked`
    /// means the transaction NEVER committed, so on-disk state is consistent
    /// and the operation can be retried or safely dropped-and-retried-later.
    /// This is the recoverable, environmental class — as opposed to genuine
    /// corruption or an integrity violation.
    ///
    /// The match is NARROW by design (a load-bearing consensus-safety guard):
    /// only the two busy/locked primary [`rusqlite::ffi::ErrorCode`]s are
    /// recoverable. Every other SQLite code (`DatabaseCorrupt`,
    /// `SystemIoFailure`, …) and every other [`DbError`] variant (`Integrity`,
    /// `Xdr`, `Pool`, …) is NON-transient — genuine corruption must NEVER be
    /// reclassified recoverable. Matches on the structured `ErrorCode`, NOT on
    /// the rendered message string.
    ///
    /// This is the single, canonical definition of the transient-busy
    /// predicate; callers in other crates (e.g. `henyey-app`'s
    /// `is_transient_db_busy`) delegate here rather than re-deriving the match.
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

    fn sqlite_error(
        code: rusqlite::ffi::ErrorCode,
        extended_code: std::os::raw::c_int,
        msg: &str,
    ) -> DbError {
        DbError::Sqlite(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error {
                code,
                extended_code,
            },
            Some(msg.to_string()),
        ))
    }

    /// The two transient busy/locked SQLite codes must classify as transient.
    #[test]
    fn test_is_transient_busy_matches_busy_and_locked() {
        assert!(
            sqlite_error(
                rusqlite::ffi::ErrorCode::DatabaseBusy,
                5,
                "database is locked"
            )
            .is_transient_busy(),
            "DatabaseBusy must be classified transient"
        );
        assert!(
            sqlite_error(
                rusqlite::ffi::ErrorCode::DatabaseLocked,
                6,
                "database table is locked"
            )
            .is_transient_busy(),
            "DatabaseLocked must be classified transient"
        );
    }

    /// Every other SQLite code and every non-`Sqlite` variant must stay
    /// non-transient — the narrow match is the load-bearing consensus-safety
    /// guard (genuine corruption must NEVER be reclassified recoverable).
    #[test]
    fn test_is_transient_busy_rejects_non_busy() {
        assert!(
            !sqlite_error(
                rusqlite::ffi::ErrorCode::DatabaseCorrupt,
                11,
                "database disk image is malformed"
            )
            .is_transient_busy(),
            "DatabaseCorrupt must NOT be classified transient"
        );
        assert!(
            !sqlite_error(
                rusqlite::ffi::ErrorCode::SystemIoFailure,
                266,
                "disk I/O error"
            )
            .is_transient_busy(),
            "SystemIoFailure must NOT be classified transient"
        );
        assert!(
            !DbError::Integrity("bucket list hash mismatch".to_string()).is_transient_busy(),
            "Integrity error must NOT be classified transient"
        );
        assert!(
            !DbError::NotFound("missing row".to_string()).is_transient_busy(),
            "NotFound error must NOT be classified transient"
        );
    }
}
