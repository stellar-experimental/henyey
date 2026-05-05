//! Error types for bucket operations.
//!
//! This module defines the error types that can occur when working with
//! buckets, bucket lists, and related operations like merging and eviction.

use thiserror::Error;

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

    /// Database operation failed.
    ///
    /// Occurs when bucket operations interact with the ledger database,
    /// such as during eviction or state verification.
    #[error("database error: {0}")]
    Database(#[from] henyey_db::DbError),

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
