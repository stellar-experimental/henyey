//! Common error types for henyey.
//!
//! This module provides the unified error type [`enum@Error`] and the convenience
//! type alias [`Result`] used throughout the henyey crates.
//!
//! # Error Handling Philosophy
//!
//! The [`enum@Error`] enum provides broad categories of errors that can occur
//! during stellar-core operations. More specific error types can be wrapped
//! in the appropriate variant using the string message.
//!
//! # Example
//!
//! ```rust
//! use henyey_common::{Error, Result};
//!
//! fn validate_data(data: &[u8]) -> Result<()> {
//!     if data.is_empty() {
//!         return Err(Error::InvalidData("data cannot be empty".to_string()));
//!     }
//!     Ok(())
//! }
//! ```

use thiserror::Error;

/// A type alias for `Result<T, Error>`.
///
/// This is the standard result type used throughout henyey.
pub type Result<T> = std::result::Result<T, Error>;

/// Common error type for henyey operations.
///
/// This enum covers the major categories of errors that can occur:
/// - XDR serialization/deserialization failures
/// - I/O errors (file, network)
/// - Configuration parsing errors
/// - Data validation failures
/// - Resource lookup failures
/// - General operation failures
#[derive(Error, Debug)]
pub enum Error {
    /// XDR encoding/decoding error.
    ///
    /// Occurs when serializing or deserializing Stellar XDR types fails.
    #[error("XDR error: {0}")]
    Xdr(#[from] stellar_xdr::Error),

    /// I/O error.
    ///
    /// Wraps standard I/O errors from file or network operations.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// Configuration error.
    ///
    /// Occurs when configuration is invalid or cannot be parsed.
    #[error("Configuration error: {0}")]
    Config(String),

    /// Invalid data error.
    ///
    /// Occurs when data fails validation (wrong format, out of range, etc.).
    #[error("Invalid data: {0}")]
    InvalidData(String),

    /// Resource not found error.
    ///
    /// Occurs when a requested resource (ledger entry, transaction, etc.) does not exist.
    #[error("Not found: {0}")]
    NotFound(String),

    /// Operation failed error.
    ///
    /// A catch-all for operations that fail for reasons not covered by other variants.
    #[error("Operation failed: {0}")]
    OperationFailed(String),
}
