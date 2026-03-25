//! Error types for history operations.
//!
//! This module defines the error types used throughout the history crate.
//! Errors are categorized by their source:
//!
//! - **Network errors**: HTTP failures, timeouts, unavailable archives
//! - **Parsing errors**: Malformed XDR, JSON, or URL data
//! - **Verification errors**: Hash mismatches, broken chains, invalid sequences
//! - **Catchup errors**: Process failures during synchronization

use henyey_common::Hash256;
use thiserror::Error;

/// Errors that can occur during history operations.
///
/// These errors cover the full range of failures that can occur when
/// interacting with history archives, from network issues to data
/// integrity problems.
#[derive(Debug, Error)]
pub enum HistoryError {
    /// Archive not reachable.
    #[error("archive not reachable: {0}")]
    ArchiveUnreachable(String),

    /// Checkpoint not found.
    #[error("checkpoint not found: {0}")]
    CheckpointNotFound(u32),

    /// History verification failed.
    #[error("history verification failed: {0}")]
    VerificationFailed(String),

    /// Catchup failed.
    #[error("catchup failed: {0}")]
    CatchupFailed(String),

    /// HTTP error from reqwest.
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),

    /// HTTP status error.
    #[error("HTTP status {status} for {url}")]
    HttpStatus {
        /// The URL that returned the error.
        url: String,
        /// The HTTP status code.
        status: u16,
    },

    /// Resource not found (404).
    #[error("not found: {0}")]
    NotFound(String),

    /// Download failed after retries.
    #[error("download failed: {0}")]
    DownloadFailed(String),

    /// Invalid response.
    #[error("invalid response: {0}")]
    InvalidResponse(String),

    /// URL parse error.
    #[error("URL parse error: {0}")]
    UrlParse(#[from] url::ParseError),

    /// JSON parse error.
    #[error("JSON parse error: {0}")]
    Json(#[from] serde_json::Error),

    /// XDR error.
    #[error("XDR error: {0}")]
    Xdr(#[from] stellar_xdr::curr::Error),

    /// XDR parsing error.
    #[error("XDR parsing error: {0}")]
    XdrParsing(String),

    /// Corrupt ledger header material downloaded from archive.
    ///
    /// Matches stellar-core `VERIFY_STATUS_ERR_CORRUPT_HEADER`. This is
    /// returned when ledger-header data fails to parse or produces runtime
    /// errors during verification, indicating the archive material itself is
    /// corrupted.
    #[error("corrupt header at ledger {ledger}: {detail}")]
    CorruptHeader {
        /// The ledger sequence where corruption was detected (0 if unknown).
        ledger: u32,
        /// Description of the corruption.
        detail: String,
    },

    /// IO error.
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    /// Bucket not found.
    #[error("bucket not found: {0}")]
    BucketNotFound(Hash256),

    /// No archive available.
    #[error("no archive available")]
    NoArchiveAvailable,

    /// Invalid ledger sequence.
    #[error("invalid sequence: expected {expected}, got {got}")]
    InvalidSequence {
        /// Expected ledger sequence.
        expected: u32,
        /// Actual ledger sequence.
        got: u32,
    },

    /// Invalid previous hash in ledger chain.
    #[error("invalid previous hash at ledger {ledger}")]
    InvalidPreviousHash {
        /// The ledger with the invalid previous hash.
        ledger: u32,
    },

    /// Invalid transaction set hash.
    #[error("invalid tx set hash at ledger {ledger}")]
    InvalidTxSetHash {
        /// The ledger with the invalid transaction set hash.
        ledger: u32,
    },

    /// Not a checkpoint ledger.
    #[error("not a checkpoint ledger: {0}")]
    NotCheckpointLedger(u32),

    /// Unsupported mode.
    #[error("unsupported mode: {0}")]
    UnsupportedMode(String),

    /// Bucket error from stellar-core-bucket crate.
    #[error("bucket error: {0}")]
    Bucket(#[from] henyey_bucket::BucketError),

    /// Database error from stellar-core-db crate.
    #[error("database error: {0}")]
    Database(#[from] henyey_db::DbError),

    /// Remote archive command not configured.
    #[error("remote archive not configured: {0}")]
    RemoteNotConfigured(String),

    /// Remote archive command failed.
    #[error("remote command failed: {command} (exit code: {exit_code:?})")]
    RemoteCommandFailed {
        /// The command that failed.
        command: String,
        /// The exit code, if any.
        exit_code: Option<i32>,
        /// Standard error output.
        stderr: String,
    },

    /// Archive already initialized.
    #[error("archive already initialized: {0}")]
    ArchiveAlreadyInitialized(String),

    /// Archive not writable (no put command configured).
    #[error("archive not writable: {0}")]
    ArchiveNotWritable(String),

    /// Archive not found by name.
    #[error("archive not found: {0}")]
    ArchiveNotFound(String),
}

impl HistoryError {
    /// Returns `true` if this error indicates a **fatal catchup failure** — the
    /// verified ledger chain from the archive disagrees with local state.
    ///
    /// Per the spec (§13.3), a fatal catchup failure occurs when a
    /// verification/integrity check fails in a way that implies the local
    /// ledger state is corrupt (not just stale or unreachable).  Specifically:
    ///
    /// - Hash chain verification failures (`InvalidPreviousHash`)
    /// - Bucket list / ledger hash mismatches (`VerificationFailed`)
    /// - Transaction set hash mismatches (`InvalidTxSetHash`)
    ///
    /// Transient errors (network, download, archive unreachable) are **not**
    /// fatal — the node should retry those.
    pub fn is_fatal_catchup_failure(&self) -> bool {
        matches!(
            self,
            HistoryError::VerificationFailed(_)
                | HistoryError::InvalidPreviousHash { .. }
                | HistoryError::InvalidTxSetHash { .. }
                | HistoryError::InvalidSequence { .. }
                | HistoryError::CorruptHeader { .. }
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_corrupt_header_is_fatal() {
        let err = HistoryError::CorruptHeader {
            ledger: 100,
            detail: "bad XDR".to_string(),
        };
        assert!(
            err.is_fatal_catchup_failure(),
            "CorruptHeader should be a fatal catchup failure"
        );
    }

    #[test]
    fn test_transient_errors_are_not_fatal() {
        let transient = HistoryError::ArchiveUnreachable("timeout".into());
        assert!(!transient.is_fatal_catchup_failure());

        let download = HistoryError::DownloadFailed("404".into());
        assert!(!download.is_fatal_catchup_failure());
    }

    #[test]
    fn test_verification_errors_are_fatal() {
        assert!(HistoryError::VerificationFailed("bad".into()).is_fatal_catchup_failure());
        assert!(HistoryError::InvalidPreviousHash { ledger: 5 }.is_fatal_catchup_failure());
        assert!(HistoryError::InvalidTxSetHash { ledger: 5 }.is_fatal_catchup_failure());
        assert!(HistoryError::InvalidSequence {
            expected: 5,
            got: 6
        }
        .is_fatal_catchup_failure());
    }
}
