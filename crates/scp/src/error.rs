//! Error types for SCP operations.
//!
//! This module defines the error types that can occur during SCP consensus
//! operations, currently the envelope signature-verification failure mode.

use thiserror::Error;

/// Errors that can occur during SCP operations.
#[derive(Debug, Error)]
pub enum ScpError {
    /// The envelope signature failed verification.
    ///
    /// Each SCP envelope must be signed by the sending node.
    /// This error indicates the signature is missing, malformed,
    /// or doesn't match the envelope content.
    #[error("signature verification failed")]
    SignatureVerificationFailed,
}
