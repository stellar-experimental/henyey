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
    Xdr(#[from] stellar_xdr::curr::Error),

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
