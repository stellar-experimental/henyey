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

/// Classification of verification hash mismatches in the offline verification path.
///
/// Each variant corresponds to a specific hash comparison in
/// [`crate::verify`] that was previously reported as a stringly-typed
/// [`HistoryError::VerificationFailed`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerifyHashKind {
    /// SHA-256 hash of bucket content doesn't match expected hash.
    Bucket,
    /// Computed bucket list hash doesn't match `header.bucket_list_hash`.
    BucketList,
    /// Computed header hash doesn't match the advertised hash in
    /// `LedgerHeaderHistoryEntry`.
    LedgerHeaderEntry,
    /// Hash of tx result set XDR doesn't match `header.tx_set_result_hash`.
    TxResultSet,
    /// Downloaded header hash doesn't match the trusted (SCP-verified) header
    /// hash.
    TrustedHeader,
}

impl std::fmt::Display for VerifyHashKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Bucket => write!(f, "bucket"),
            Self::BucketList => write!(f, "bucket list"),
            Self::LedgerHeaderEntry => write!(f, "ledger header entry"),
            Self::TxResultSet => write!(f, "tx result set"),
            Self::TrustedHeader => write!(f, "trusted header"),
        }
    }
}

/// Diagnostic info for a verification hash mismatch.
///
/// Boxed inside [`HistoryError::VerificationHashMismatch`] to keep the
/// `HistoryError` enum small, consistent with [`TxSetHashMismatchInfo`].
#[derive(Debug, Clone)]
pub struct VerifyHashMismatchInfo {
    /// What kind of hash was being verified.
    pub kind: VerifyHashKind,
    /// Ledger sequence where the mismatch was detected (`None` for
    /// bucket-level checks with no ledger context).
    pub ledger: Option<u32>,
    /// The expected hash value.
    pub expected: Hash256,
    /// The actual (computed) hash value.
    pub actual: Hash256,
}

impl std::fmt::Display for VerifyHashMismatchInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.ledger {
            Some(seq) => write!(
                f,
                "{} hash mismatch at ledger {}: expected {}, actual {}",
                self.kind, seq, self.expected, self.actual
            ),
            None => write!(
                f,
                "{} hash mismatch: expected {}, actual {}",
                self.kind, self.expected, self.actual
            ),
        }
    }
}

/// Diagnostic context for a tx-set hash mismatch, boxed inside `InvalidTxSetHash`
/// to keep the `HistoryError` enum small.
#[derive(Debug, Clone)]
pub struct TxSetHashMismatchInfo {
    /// Expected hash from the header's scp_value.tx_set_hash.
    pub expected: Hash256,
    /// Actual hash computed from the transaction set.
    pub actual: Hash256,
    /// The current ledger's protocol version (header.ledger_version).
    pub header_ledger_version: u32,
    /// The previous_ledger_hash from the header.
    pub header_prev_hash: Hash256,
    /// The previous_ledger_hash embedded in the transaction set itself.
    pub tx_set_prev_hash: Hash256,
    /// Human-readable tx set format: "classic" or "generalized_v1".
    pub tx_set_format: &'static str,
}

impl std::fmt::Display for TxSetHashMismatchInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "expected={}, actual={}, header_ledger_version={}, \
             header_prev_hash={}, tx_set_prev_hash={}, format={}",
            self.expected,
            self.actual,
            self.header_ledger_version,
            self.header_prev_hash,
            self.tx_set_prev_hash,
            self.tx_set_format
        )
    }
}

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

    /// Typed verification hash mismatch.
    ///
    /// Replaces string-based [`VerificationFailed`](HistoryError::VerificationFailed)
    /// for hash comparison errors in [`crate::verify::verify_bucket_hash`],
    /// [`crate::verify::verify_ledger_hash`],
    /// [`crate::verify::verify_ledger_header_history_entry`],
    /// [`crate::verify::verify_tx_result_set`],
    /// [`crate::verify::verify_header_matches_trusted`], and
    /// [`crate::replay::execution::verify_bucket_list_hash`].
    #[error("verification hash mismatch: {0}")]
    VerificationHashMismatch(Box<VerifyHashMismatchInfo>),

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

    /// Invalid transaction set hash — includes full diagnostic context for debugging.
    #[error("invalid tx set hash at ledger {ledger}: {info}")]
    InvalidTxSetHash {
        /// The ledger with the invalid transaction set hash.
        ledger: u32,
        /// Boxed diagnostic info (expected/actual hashes, protocol version, format).
        info: Box<TxSetHashMismatchInfo>,
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

    /// Ledger error from the ledger crate.
    #[error("ledger error: {0}")]
    Ledger(#[from] henyey_ledger::LedgerError),

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
    /// - Bucket list / ledger hash mismatches (`VerificationFailed`,
    ///   `VerificationHashMismatch`)
    /// - Transaction set hash mismatches (`InvalidTxSetHash`)
    /// - Ledger-apply hash mismatches (`Ledger(LedgerError::HashMismatch)`)
    ///
    /// Transient errors (network, download, archive unreachable) are **not**
    /// fatal — the node should retry those.
    pub fn is_fatal_catchup_failure(&self) -> bool {
        matches!(
            self,
            HistoryError::VerificationFailed(_)
                | HistoryError::VerificationHashMismatch(_)
                | HistoryError::InvalidPreviousHash { .. }
                | HistoryError::InvalidTxSetHash { .. }
                | HistoryError::InvalidSequence { .. }
                | HistoryError::CorruptHeader { .. }
                | HistoryError::Ledger(henyey_ledger::LedgerError::HashMismatch { .. })
        )
    }

    /// Returns `true` if this error represents a **typed** hash mismatch
    /// (bucket, bucket list, ledger header, tx set, or trusted header) that
    /// indicates state divergence.
    ///
    /// Recognized variants:
    /// - [`VerificationHashMismatch`](HistoryError::VerificationHashMismatch)
    ///   — verification and replay paths (bucket, bucket list, header entry,
    ///   tx result set, trusted header)
    /// - [`InvalidTxSetHash`](HistoryError::InvalidTxSetHash) — tx set hash
    ///   mismatch with rich diagnostic context
    /// - [`Ledger(LedgerError::HashMismatch)`](HistoryError::Ledger) —
    ///   apply-path mismatch from `henyey-ledger`
    ///
    /// Note: [`VerificationFailed(String)`](HistoryError::VerificationFailed)
    /// is **not** recognized even if its text mentions "hash mismatch" — only
    /// typed variants count.
    pub fn is_hash_mismatch(&self) -> bool {
        matches!(
            self,
            HistoryError::VerificationHashMismatch(_)
                | HistoryError::Ledger(henyey_ledger::LedgerError::HashMismatch { .. })
                | HistoryError::InvalidTxSetHash { .. }
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
        assert!(HistoryError::InvalidTxSetHash {
            ledger: 5,
            info: Box::new(TxSetHashMismatchInfo {
                expected: Hash256::ZERO,
                actual: Hash256::ZERO,
                header_ledger_version: 0,
                header_prev_hash: Hash256::ZERO,
                tx_set_prev_hash: Hash256::ZERO,
                tx_set_format: "classic",
            }),
        }
        .is_fatal_catchup_failure());
        assert!(HistoryError::InvalidSequence {
            expected: 5,
            got: 6
        }
        .is_fatal_catchup_failure());
    }

    #[test]
    fn test_ledger_hash_mismatch_is_fatal() {
        let err = HistoryError::Ledger(henyey_ledger::LedgerError::HashMismatch {
            expected: "abc".into(),
            actual: "def".into(),
        });
        assert!(
            err.is_fatal_catchup_failure(),
            "Ledger(HashMismatch) should be a fatal catchup failure"
        );
    }

    #[test]
    fn test_is_hash_mismatch() {
        // Positive: LedgerError::HashMismatch via Ledger variant
        let err = HistoryError::Ledger(henyey_ledger::LedgerError::HashMismatch {
            expected: "abc".into(),
            actual: "def".into(),
        });
        assert!(err.is_hash_mismatch());

        // Positive: InvalidTxSetHash
        let err = HistoryError::InvalidTxSetHash {
            ledger: 5,
            info: Box::new(TxSetHashMismatchInfo {
                expected: Hash256::ZERO,
                actual: Hash256::ZERO,
                header_ledger_version: 0,
                header_prev_hash: Hash256::ZERO,
                tx_set_prev_hash: Hash256::ZERO,
                tx_set_format: "classic",
            }),
        };
        assert!(err.is_hash_mismatch());

        // Negative: CatchupFailed is NOT a hash mismatch
        let err = HistoryError::CatchupFailed("some other error".into());
        assert!(!err.is_hash_mismatch());

        // Negative: VerificationFailed is NOT a hash mismatch (even if text mentions it)
        let err = HistoryError::VerificationFailed("hash mismatch at ledger 5".into());
        assert!(!err.is_hash_mismatch());

        // Negative: Other LedgerError variants are NOT hash mismatches
        let err = HistoryError::Ledger(henyey_ledger::LedgerError::Internal("bug".into()));
        assert!(!err.is_hash_mismatch());
    }

    /// Helper to construct a `VerificationHashMismatch` error for tests.
    fn make_verify_hash_mismatch(kind: VerifyHashKind, ledger: Option<u32>) -> HistoryError {
        HistoryError::VerificationHashMismatch(Box::new(VerifyHashMismatchInfo {
            kind,
            ledger,
            expected: Hash256::ZERO,
            actual: Hash256::from([0xAB; 32]),
        }))
    }

    #[test]
    fn test_verification_hash_mismatch_is_fatal() {
        for kind in [
            VerifyHashKind::Bucket,
            VerifyHashKind::BucketList,
            VerifyHashKind::LedgerHeaderEntry,
            VerifyHashKind::TxResultSet,
            VerifyHashKind::TrustedHeader,
        ] {
            let err = make_verify_hash_mismatch(kind, Some(42));
            assert!(
                err.is_fatal_catchup_failure(),
                "VerificationHashMismatch({kind}) should be a fatal catchup failure"
            );
        }
    }

    #[test]
    fn test_verification_hash_mismatch_is_hash_mismatch() {
        for kind in [
            VerifyHashKind::Bucket,
            VerifyHashKind::BucketList,
            VerifyHashKind::LedgerHeaderEntry,
            VerifyHashKind::TxResultSet,
            VerifyHashKind::TrustedHeader,
        ] {
            let err = make_verify_hash_mismatch(kind, Some(42));
            assert!(
                err.is_hash_mismatch(),
                "VerificationHashMismatch({kind}) should be recognized as a hash mismatch"
            );
        }
    }

    #[test]
    fn test_verify_hash_mismatch_display_with_ledger() {
        let info = VerifyHashMismatchInfo {
            kind: VerifyHashKind::BucketList,
            ledger: Some(42),
            expected: Hash256::ZERO,
            actual: Hash256::from([0xAB; 32]),
        };
        let msg = info.to_string();
        assert!(msg.contains("bucket list hash mismatch at ledger 42"));
        assert!(msg.contains("expected"));
        assert!(msg.contains("actual"));
    }

    #[test]
    fn test_verify_hash_mismatch_display_without_ledger() {
        let info = VerifyHashMismatchInfo {
            kind: VerifyHashKind::Bucket,
            ledger: None,
            expected: Hash256::ZERO,
            actual: Hash256::from([0xAB; 32]),
        };
        let msg = info.to_string();
        assert!(msg.contains("bucket hash mismatch:"));
        assert!(!msg.contains("at ledger"));
    }

    #[test]
    fn test_verify_hash_kind_display() {
        assert_eq!(VerifyHashKind::Bucket.to_string(), "bucket");
        assert_eq!(VerifyHashKind::BucketList.to_string(), "bucket list");
        assert_eq!(
            VerifyHashKind::LedgerHeaderEntry.to_string(),
            "ledger header entry"
        );
        assert_eq!(VerifyHashKind::TxResultSet.to_string(), "tx result set");
        assert_eq!(VerifyHashKind::TrustedHeader.to_string(), "trusted header");
    }
}
