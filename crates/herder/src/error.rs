//! Error types for Herder operations.
//!
//! This module defines the error types that can occur during Herder operations,
//! including SCP consensus errors and internal processing errors.

use thiserror::Error;

/// Errors that can occur during Herder operations.
///
/// The Herder can fail for various reasons including transaction validation issues,
/// capacity limits, SCP consensus problems, or internal state errors.
#[derive(Debug, Error)]
pub enum HerderError {
    /// An error occurred in the SCP consensus layer.
    ///
    /// This wraps errors from the underlying SCP implementation, such as
    /// signature verification failures or invalid message handling.
    #[error("SCP error: {0}")]
    Scp(#[from] henyey_scp::ScpError),

    /// Operation requires validator mode but the node is not validating.
    ///
    /// Some operations (like triggering consensus) require the node to be
    /// configured as a validator with a secret key and quorum set.
    #[error("not in validating state")]
    NotValidating,

    /// An internal error occurred.
    ///
    /// This is a catch-all for unexpected internal errors that don't fit
    /// other categories.
    #[error("internal error: {0}")]
    Internal(String),

    /// A database error occurred on the SCP-persistence path.
    ///
    /// Preserves the structured [`henyey_db::DbError`] (instead of collapsing
    /// it into an `Internal(String)`) so callers can classify it — e.g. the
    /// tx-set GC purge distinguishes a transient `SQLITE_BUSY`/`LOCKED` from
    /// genuine corruption via [`henyey_db::DbError::is_transient_busy`] (#3806).
    #[error("database error: {0}")]
    Db(#[from] henyey_db::DbError),

    /// An invalid SCP envelope was received or processed.
    ///
    /// This can occur during serialization, deserialization, or validation
    /// of SCP envelopes and related data structures.
    #[error("invalid envelope: {0}")]
    InvalidEnvelope(String),
}
