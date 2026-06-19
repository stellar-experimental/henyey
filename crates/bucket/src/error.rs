//! Error types for bucket operations.
//!
//! This module defines the error types that can occur when working with
//! buckets, bucket lists, and related operations like merging and eviction.

use thiserror::Error;

/// Raw `errno` for `ENOSPC` ("No space left on device") on Linux.
pub const ENOSPC: i32 = 28;
/// Raw `errno` for `EDQUOT` ("Disk quota exceeded") on Linux.
pub const EDQUOT: i32 = 122;

/// Coarse classification of a bucket error for recovery decisions.
///
/// This is the `Clone`able classification threaded through the merge error
/// chain (`MergeResult` / `MergeRecvState`) so that recovery decisions key on
/// the structured class / raw `errno` rather than on string-matching the
/// rendered error message (#3478).
///
/// # Parity (stellar-core v26.0.1)
///
/// stellar-core does NOT inspect `errno`: a bucket merge/flush IO failure
/// throws a plain `runtime_error` tagged `POSSIBLY_CORRUPTED_LOCAL_FS`
/// ("ensure enough space") which propagates uncaught out of `closeLedger` →
/// **clean process exit** (see `FutureBucket.cpp:446-452`,
/// `BucketManager.cpp:79,89,95`). There is no `std::abort`/`terminate` on the
/// bucket path, and core auto-wipes on NEITHER ENOSPC nor corruption (wipe is
/// operator-driven). Corruption is the DISTINCT path
/// (`LedgerManagerImpl.cpp:1752`, `POSSIBLY_CORRUPTED_LOCAL_DATA`, "reset this
/// instance"). henyey narrows "transient" to the free-space class
/// (ENOSPC/EDQUOT) and keeps everything else (incl. EIO/EROFS) fatal — a
/// conservative SUPERSET of core's fatality that never masks corruption.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BucketErrorClass {
    /// Transient, environmental free-space IO failure (ENOSPC / EDQUOT).
    /// Recoverable: no partial state is committed; the operation can be
    /// retried after disk recovers. Carries the raw `errno`.
    TransientIo(i32),
    /// Genuine data corruption (hash mismatch / structural corruption).
    /// Fatal: must NOT be masked or silently retried.
    Corruption,
    /// Any other failure (non-free-space IO, serialization, merge logic, …).
    /// Treated as fatal (conservative).
    Other,
}

impl BucketErrorClass {
    /// True only for the transient free-space IO class.
    pub fn is_transient_io(self) -> bool {
        matches!(self, BucketErrorClass::TransientIo(_))
    }
}

/// Errors that can occur during bucket operations.
///
/// This enum covers all failure modes in the bucket subsystem, from I/O
/// errors to protocol violations like hash mismatches during verification.
#[derive(Debug, Error)]
pub enum BucketError {
    /// Bucket file not found on disk.
    ///
    /// This typically occurs when trying to load a bucket by hash that
    /// hasn't been downloaded or has been garbage collected.
    #[error("bucket not found: {0}")]
    NotFound(String),

    /// Bucket hash verification failed.
    ///
    /// This indicates data corruption or a bug in hash computation.
    /// The bucket's content hash doesn't match the expected hash,
    /// which is critical for bucket list integrity.
    #[error("bucket hash mismatch: expected {expected}, got {actual}")]
    HashMismatch {
        /// The expected hash (from bucket list or history archive).
        expected: String,
        /// The actual computed hash of the bucket contents.
        actual: String,
    },

    /// XDR serialization or deserialization failed.
    ///
    /// This can occur when:
    /// - Parsing a bucket file with invalid XDR format
    /// - Serializing entries that exceed XDR limits
    /// - Record marks are corrupted or indicate invalid lengths
    #[error("bucket serialization error: {0}")]
    Serialization(String),

    /// Bucket merge operation failed.
    ///
    /// This can occur when:
    /// - Protocol version constraints are violated
    /// - A merge is already in progress at a level
    /// - Ledger sequence is invalid (e.g., zero)
    #[error("bucket merge error: {0}")]
    Merge(String),

    /// File I/O operation failed.
    ///
    /// Covers disk read/write errors, permission issues, and
    /// filesystem problems when working with bucket files.
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    /// Bloom filter construction or lookup failed.
    ///
    /// This can occur when:
    /// - Not enough elements to build a filter (minimum 2)
    /// - Too many hash collisions during construction
    #[error("bloom filter error: {0}")]
    BloomFilter(String),

    /// Bucket data is structurally corrupt.
    ///
    /// Returned when loading a bucket from disk or raw bytes reveals that
    /// entries are not strictly sorted, contain duplicate keys, or violate
    /// the metadata-prefix invariant. Distinct from `Merge` (construction-
    /// time failures) — this variant covers load-time validation only.
    #[error("bucket corruption: {0}")]
    Corruption(String),
}

impl BucketError {
    /// Classify this error for recovery decisions (#3478).
    ///
    /// Keys on the structured `io::Error` raw `errno` for the `Io` arm — NOT on
    /// string-matching the rendered message. Only `ENOSPC`(28)/`EDQUOT`(122)
    /// are transient (free-space class). `HashMismatch`/`Corruption` are
    /// `Corruption`; everything else is `Other` (fatal, conservative).
    pub fn error_class(&self) -> BucketErrorClass {
        match self {
            BucketError::Io(e) => match e.raw_os_error() {
                Some(errno @ (ENOSPC | EDQUOT)) => BucketErrorClass::TransientIo(errno),
                _ => BucketErrorClass::Other,
            },
            BucketError::HashMismatch { .. } | BucketError::Corruption(_) => {
                BucketErrorClass::Corruption
            }
            _ => BucketErrorClass::Other,
        }
    }

    /// True only for a transient, environmental free-space IO failure
    /// (`ENOSPC`/`EDQUOT`). Such failures commit no partial state and are
    /// recoverable once disk recovers — distinct from genuine corruption,
    /// which stays fatal. See [`BucketErrorClass`] for the parity rationale.
    pub fn is_transient_io(&self) -> bool {
        self.error_class().is_transient_io()
    }
}
