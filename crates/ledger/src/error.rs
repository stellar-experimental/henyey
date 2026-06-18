//! Error types for ledger operations.
//!
//! This module defines [`LedgerError`], the unified error type for all
//! ledger-related operations. Errors are categorized by their source
//! and severity to aid in debugging and error handling.

use thiserror::Error;

/// Errors that can occur during ledger operations.
///
/// This enum covers all error conditions that can arise during ledger
/// management, including initialization, ledger close, state access,
/// and validation failures.
///
/// # Error Categories
///
/// - **State errors**: `NotInitialized`, `AlreadyInitialized`
/// - **Validation errors**: `InvalidSequence`, `HashMismatch`, `InvalidHeaderChain`
/// - **External errors**: `Bucket`, `Xdr`
/// - **Internal errors**: `Serialization`, `Snapshot`, `Internal`
#[derive(Debug, Error)]
pub enum LedgerError {
    /// Ledger sequence number doesn't match expected value.
    ///
    /// This typically indicates an attempt to close a ledger out of order,
    /// a mismatch between expected and actual ledger sequences, or a
    /// state update with non-consecutive sequence numbers.
    #[error("invalid ledger sequence: expected {expected}, got {actual}")]
    InvalidSequence {
        /// The expected ledger sequence.
        expected: u32,
        /// The actual ledger sequence received.
        actual: u32,
    },

    /// Cryptographic hash mismatch.
    ///
    /// Indicates data corruption or an attempt to apply changes to
    /// the wrong ledger state. This is a critical error.
    #[error("ledger hash mismatch: expected {expected}, got {actual}")]
    HashMismatch {
        /// The expected hash (hex-encoded).
        expected: String,
        /// The actual computed hash (hex-encoded).
        actual: String,
    },

    /// Invalid header chain linkage.
    ///
    /// The previous_ledger_hash or skip list entries don't match
    /// the expected values for chain continuity.
    #[error("invalid header chain: {0}")]
    InvalidHeaderChain(String),

    /// Error from bucket list operations.
    #[error("bucket error: {0}")]
    Bucket(#[from] henyey_bucket::BucketError),

    /// XDR encoding or decoding error.
    #[error("XDR error: {0}")]
    Xdr(#[from] stellar_xdr::curr::Error),

    /// Generic serialization error.
    #[error("serialization error: {0}")]
    Serialization(String),

    /// Operation attempted on uninitialized ledger manager.
    ///
    /// The ledger manager must be initialized via `initialize`
    /// before ledger close operations can begin.
    #[error("ledger not initialized")]
    NotInitialized,

    /// Attempted to initialize an already-initialized ledger manager.
    ///
    /// Use `reset` then `initialize` to reset state if needed.
    #[error("ledger already initialized")]
    AlreadyInitialized,

    /// Snapshot-related error.
    #[error("snapshot error: {0}")]
    Snapshot(String),

    /// Internal error (indicates a bug).
    ///
    /// These errors should not occur during normal operation and
    /// indicate a logic error in the implementation.
    #[error("internal error: {0}")]
    Internal(String),

    /// Invalid entry type or state.
    ///
    /// Indicates an entry has an unexpected type or is in an invalid state
    /// for the requested operation.
    #[error("invalid entry: {0}")]
    InvalidEntry(String),

    /// A ledger upgrade failed during application.
    ///
    /// This occurs when a config upgrade set cannot be loaded or validated
    /// at apply time. Parity: stellar-core throws in this case because
    /// the upgrade should have been validated during SCP consensus.
    #[error("upgrade error: {0}")]
    UpgradeError(String),
}
