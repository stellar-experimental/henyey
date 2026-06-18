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
    /// First header's `previous_ledger_hash` doesn't match the expected
    /// bottom-of-chain trust anchor.
    BottomAnchor,
    /// Computed header hash at the LCL sequence doesn't match the local LCL
    /// hash (local state corruption).
    Lcl,
    /// Highest checkpoint's last header hash doesn't match the trusted
    /// top-of-chain anchor (§9.2 reverse-walk trust establishment).
    TopAnchor,
}

impl std::fmt::Display for VerifyHashKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Bucket => write!(f, "bucket"),
            Self::BucketList => write!(f, "bucket list"),
            Self::LedgerHeaderEntry => write!(f, "ledger header entry"),
            Self::TxResultSet => write!(f, "tx result set"),
            Self::TrustedHeader => write!(f, "trusted header"),
            Self::BottomAnchor => write!(f, "bottom anchor"),
            Self::Lcl => write!(f, "LCL"),
            Self::TopAnchor => write!(f, "top anchor"),
        }
    }
}

/// Ledger-chain verification status taxonomy (CATCHUP §3.9-1).
///
/// Mirrors stellar-core's `HistoryManager::LedgerVerificationStatus`
/// (`stellar-core/src/history/HistoryManager.h:195-204`), the status code
/// returned from `LedgerManager::verifyCatchupCandidate` /
/// `VerifyLedgerChainWork`. henyey keeps its richer typed [`HistoryError`]
/// variants as the live error currency; this enum is a discoverability and
/// classification overlay reached via [`HistoryError::verify_status`]. It is
/// **not** a return type of [`crate::verify::verify_reverse_walk`] and
/// changes no control flow — adding it is behavior-neutral.
///
/// See also `CATCHUP_SPEC.md` §3.9 (status table) and §9.2 (per-step status
/// assignment).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LedgerVerifyStatus {
    /// Verification succeeded. Mirrors `VERIFY_STATUS_OK`.
    ///
    /// Note: this variant is **unreachable** through
    /// [`HistoryError::verify_status`] — an error is never "OK". It exists
    /// solely for spec-mirroring completeness (the success path is the
    /// *absence* of a `HistoryError`).
    Ok,
    /// A hash comparison failed (header/chain/bucket-list/tx-set/replay/LCL
    /// hash). Mirrors `VERIFY_STATUS_ERR_BAD_HASH`.
    ErrBadHash,
    /// A ledger header carried an unsupported protocol version. Mirrors
    /// `VERIFY_STATUS_ERR_BAD_LEDGER_VERSION`.
    ErrBadLedgerVersion,
    /// The chain advanced past the expected next sequence (got > expected).
    /// Mirrors `VERIFY_STATUS_ERR_OVERSHOT`.
    ErrOvershot,
    /// The chain fell short of the expected next sequence (got < expected).
    /// Mirrors `VERIFY_STATUS_ERR_UNDERSHOT`.
    ErrUndershot,
    /// A checkpoint range was missing required entries. Mirrors
    /// `VERIFY_STATUS_ERR_MISSING_ENTRIES`.
    ///
    /// Taxonomy-only in henyey: stellar-core surfaces this at the
    /// checkpoint-start and end-of-range completeness checks
    /// (`VerifyLedgerChainWork.cpp:276,339`), which henyey's
    /// checkpoint-grouped reverse walk does not currently emit as a dedicated
    /// error. No live [`HistoryError`] maps to it today; it is retained for
    /// spec parity and reached only by direct construction in tests.
    ErrMissingEntries,
    /// Ledger-header material from the archive failed to parse / was corrupt.
    /// Mirrors `VERIFY_STATUS_ERR_CORRUPT_HEADER`.
    ErrCorruptHeader,
}

impl std::fmt::Display for LedgerVerifyStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Self::Ok => "VERIFY_STATUS_OK",
            Self::ErrBadHash => "VERIFY_STATUS_ERR_BAD_HASH",
            Self::ErrBadLedgerVersion => "VERIFY_STATUS_ERR_BAD_LEDGER_VERSION",
            Self::ErrOvershot => "VERIFY_STATUS_ERR_OVERSHOT",
            Self::ErrUndershot => "VERIFY_STATUS_ERR_UNDERSHOT",
            Self::ErrMissingEntries => "VERIFY_STATUS_ERR_MISSING_ENTRIES",
            Self::ErrCorruptHeader => "VERIFY_STATUS_ERR_CORRUPT_HEADER",
        };
        f.write_str(s)
    }
}

/// Diagnostic info for a verification hash mismatch.
///
/// Boxed inside [`HistoryError::VerificationHashMismatch`] to keep the
/// `HistoryError` enum small, consistent with [`TxSetHashMismatchInfo`].
///
/// Fields are private to enforce construction through [`Self::log_and_new`]
/// (preferred, emits structured tracing) or [`Self::new_unlogged`]
/// (crate-internal only, for callsites that handle their own logging).
#[derive(Debug, Clone)]
pub struct VerifyHashMismatchInfo {
    kind: VerifyHashKind,
    ledger: Option<u32>,
    expected: Hash256,
    actual: Hash256,
}

impl VerifyHashMismatchInfo {
    /// Construct without emitting any tracing event.
    ///
    /// **Crate-internal only.** Callers MUST have already emitted a structured
    /// `tracing::error!` with at minimum `kind`, `ledger_seq` (when `Some`),
    /// `expected_hash`, and `actual_hash` fields before calling this.
    ///
    /// For production mismatch sites, prefer [`Self::log_and_new`] which
    /// handles the structured logging automatically.
    pub(crate) fn new_unlogged(
        kind: VerifyHashKind,
        ledger: Option<u32>,
        expected: Hash256,
        actual: Hash256,
    ) -> Self {
        Self {
            kind,
            ledger,
            expected,
            actual,
        }
    }

    /// Construct the info and emit a structured `tracing::error!` event.
    ///
    /// Preferred at production mismatch sites — ensures every hash
    /// verification failure produces a queryable log event with `kind`,
    /// `ledger_seq`, `expected_hash`, and `actual_hash` fields.
    pub fn log_and_new(
        kind: VerifyHashKind,
        ledger: Option<u32>,
        expected: Hash256,
        actual: Hash256,
    ) -> Self {
        if let Some(seq) = ledger {
            tracing::error!(
                kind = %kind,
                ledger_seq = seq,
                expected_hash = %expected,
                actual_hash = %actual,
                "verification hash mismatch"
            );
        } else {
            tracing::error!(
                kind = %kind,
                expected_hash = %expected,
                actual_hash = %actual,
                "verification hash mismatch"
            );
        }
        Self {
            kind,
            ledger,
            expected,
            actual,
        }
    }

    /// What kind of hash was being verified.
    pub fn kind(&self) -> VerifyHashKind {
        self.kind
    }

    /// Ledger sequence where the mismatch was detected (`None` for
    /// bucket-level checks with no ledger context).
    pub fn ledger(&self) -> Option<u32> {
        self.ledger
    }

    /// The expected hash value.
    pub fn expected(&self) -> Hash256 {
        self.expected
    }

    /// The actual (computed) hash value.
    pub fn actual(&self) -> Hash256 {
        self.actual
    }
}

impl From<VerifyHashMismatchInfo> for HistoryError {
    fn from(info: VerifyHashMismatchInfo) -> Self {
        HistoryError::VerificationHashMismatch(Box::new(info))
    }
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

impl TxSetHashMismatchInfo {
    /// Convenience constructor for `TxSetHashMismatchInfo`.
    ///
    /// All fields remain `pub`, so direct struct literal construction is still
    /// valid. This constructor exists purely for ergonomics — combine with
    /// [`into_error`](Self::into_error) to produce a
    /// [`HistoryError::InvalidTxSetHash`].
    pub fn new(
        expected: Hash256,
        actual: Hash256,
        header_ledger_version: u32,
        header_prev_hash: Hash256,
        tx_set_prev_hash: Hash256,
        tx_set_format: &'static str,
    ) -> Self {
        Self {
            expected,
            actual,
            header_ledger_version,
            header_prev_hash,
            tx_set_prev_hash,
            tx_set_format,
        }
    }

    /// Convert into a [`HistoryError::InvalidTxSetHash`], boxing `self`.
    pub fn into_error(self, ledger: u32) -> HistoryError {
        HistoryError::InvalidTxSetHash {
            ledger,
            info: Box::new(self),
        }
    }
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
    /// [`crate::verify::verify_ledger_header_history_entry`],
    /// [`crate::verify::verify_tx_result_set`],
    /// [`crate::verify::verify_chain_anchors`], and
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

    /// Catchup knit-to-LCL: archive entry at LCL-1 disagrees with LCL's
    /// `previousLedgerHash` (§11.2 case 2).
    ///
    /// Triggered when the catchup replay pipeline reads a header history
    /// entry whose sequence is `lcl.seq - 1` and whose own hash does not
    /// match `lcl.previousLedgerHash`. Mirrors stellar-core
    /// `ApplyCheckpointWork::getNextLedgerCloseData()` "replay failed to
    /// connect on hash of LCL predecessor". Classified as fatal.
    #[error(
        "knit-to-LCL failed at LCL predecessor (ledger {ledger}): expected {expected}, got {actual}"
    )]
    KnitLclPredecessorHashMismatch {
        /// The ledger sequence of the predecessor (lcl.seq - 1).
        ledger: u32,
        /// LCL's previousLedgerHash (hex-encoded).
        expected: String,
        /// Archive entry's own hash (hex-encoded).
        actual: String,
    },

    /// Catchup knit-to-LCL: archive entry at LCL has a hash that disagrees
    /// with the local LCL hash (§11.2 case 3).
    ///
    /// Mirrors stellar-core "replay at LCL X disagreed on hash". Classified
    /// as fatal.
    #[error("knit-to-LCL failed at LCL: expected {expected}, got {actual}")]
    KnitLclHashMismatch {
        /// LCL hash held locally (hex-encoded).
        expected: String,
        /// Archive entry's own hash at LCL (hex-encoded).
        actual: String,
    },

    /// Catchup knit-to-LCL: archive entry at LCL+1 disagrees with LCL's
    /// hash via its `previousLedgerHash` (§11.2 case 4 prev-hash check).
    ///
    /// Mirrors stellar-core "replay at current ledger X disagreed on LCL
    /// hash". Distinct from [`KnitLclHashMismatch`] because the comparison
    /// is on `entry.header.previousLedgerHash`, not `entry.hash`. Classified
    /// as fatal.
    #[error(
        "knit-to-LCL failed at LCL+1 (ledger {ledger}): \
         entry.previousLedgerHash {actual} != lcl.hash {expected}"
    )]
    KnitCurrentLedgerPrevHashMismatch {
        /// The ledger sequence at LCL+1.
        ledger: u32,
        /// LCL hash held locally (hex-encoded).
        expected: String,
        /// Entry's previousLedgerHash (hex-encoded).
        actual: String,
    },

    /// Catchup knit-to-LCL: archive entry's sequence is more than one past
    /// LCL (§11.2 case 5 overshoot).
    ///
    /// Mirrors stellar-core "replay overshot current ledger". Classified as
    /// fatal.
    #[error("knit-to-LCL overshot: entry seq {entry_seq} > lcl seq {lcl_seq} + 1")]
    KnitOvershot {
        /// The archive entry's ledger sequence.
        entry_seq: u32,
        /// Local Last Closed Ledger sequence.
        lcl_seq: u32,
    },

    /// Ledger hash mismatch during catchup replay.
    ///
    /// Returned only by the replay path (`replay_via_close_ledger`) when
    /// `close_ledger` produces a `LedgerError::HashMismatch`. This can
    /// originate from:
    /// - Header hash validation (expected vs computed ledger header hash)
    /// - Previous-hash chain checks
    /// - Bucket list hash verification
    ///
    /// Other callers may still produce `HistoryError::Ledger(LedgerError::HashMismatch { .. })`
    /// via the `From<LedgerError>` conversion or direct propagation.
    ///
    /// The variant captures replay-specific context (ledger sequence)
    /// alongside the raw hash strings from the underlying `LedgerError`.
    #[error("replay hash mismatch at ledger {ledger}: expected {expected}, got {actual}")]
    ReplayHashMismatch {
        /// The ledger sequence being replayed when the mismatch was detected.
        ledger: u32,
        /// The expected hash (hex-encoded).
        expected: String,
        /// The actual computed hash (hex-encoded).
        actual: String,
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

    /// Fatal failure: ledger chain disagrees with local state and trust came
    /// from SCP consensus (§9.5). The node MUST NOT retry catchup.
    #[error("fatal: ledger chain disagrees with local state (§9.5 fatal failure)")]
    FatalChainDisagreement,

    /// Transient: the catchup target's covering checkpoint has not yet been
    /// published to the history archive (the archive HAS `currentLedger` is
    /// still behind `checkpoint_containing(target)`).
    ///
    /// This restores the precondition stellar-core enforces via
    /// `GetHistoryArchiveStateWork` (retry-until-published) before knit/replay,
    /// which henyey's cloned-local `ReplayOnly` fast path optimized away. A
    /// knit-to-LCL boundary mismatch produced against an unpublished/stale
    /// archive checkpoint is an archive-not-ready condition that must be
    /// **retried**, not treated as local-state corruption. It is therefore
    /// **excluded** from [`is_fatal_catchup_failure`](HistoryError::is_fatal_catchup_failure)
    /// and [`is_hash_mismatch`](HistoryError::is_hash_mismatch). See #2931.
    ///
    /// The covering checkpoint (`checkpoint_containing(target)`) is *derived*
    /// from `target` rather than stored, so the variant's public shape stays
    /// minimal; read it via [`covering_checkpoint`](HistoryError::covering_checkpoint).
    #[error(
        "catchup target checkpoint not yet published: target ledger {target} \
         requires archive currentLedger >= its covering checkpoint \
         {covering}, but archive HAS currentLedger is {has_current}",
        covering = crate::checkpoint::checkpoint_containing(*target)
    )]
    CheckpointNotYetPublished {
        /// The catchup target ledger sequence.
        target: u32,
        /// The archive HAS `currentLedger` observed at gate time.
        has_current: u32,
    },

    /// Unsupported ledger version detected during chain verification (§9.3 step 2e).
    #[error("unsupported ledger version {version} at ledger {ledger} (supported: {min}..={max})")]
    UnsupportedLedgerVersion {
        /// The ledger sequence with the unsupported version.
        ledger: u32,
        /// The ledger version found.
        version: u32,
        /// Minimum supported version.
        min: u32,
        /// Maximum supported version.
        max: u32,
    },
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
    ///   `VerificationHashMismatch`, `ReplayHashMismatch`)
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
                | HistoryError::ReplayHashMismatch { .. }
                | HistoryError::KnitLclPredecessorHashMismatch { .. }
                | HistoryError::KnitLclHashMismatch { .. }
                | HistoryError::KnitCurrentLedgerPrevHashMismatch { .. }
                | HistoryError::KnitOvershot { .. }
                | HistoryError::FatalChainDisagreement
                | HistoryError::UnsupportedLedgerVersion { .. }
                | HistoryError::Ledger(henyey_ledger::LedgerError::HashMismatch { .. })
        )
    }

    /// Returns `true` if this error represents a **typed** hash mismatch
    /// (bucket, bucket list, ledger header, tx set, trusted header, bottom
    /// anchor, or LCL) that indicates state divergence.
    ///
    /// Recognized variants:
    /// - [`VerificationHashMismatch`](HistoryError::VerificationHashMismatch)
    ///   — verification and replay paths (bucket, bucket list, header entry,
    ///   tx result set, trusted header, bottom anchor, LCL)
    /// - [`ReplayHashMismatch`](HistoryError::ReplayHashMismatch) — replay
    ///   path hash mismatch with ledger sequence context
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
                | HistoryError::ReplayHashMismatch { .. }
        )
    }

    /// Returns `true` if this error represents a **local-vs-archive state
    /// divergence** — the local ledger state (LCL header, replayed header, or
    /// applied bucket-list) disagrees with the canonical history archive.
    ///
    /// This is the divergence class that a forced near-tip recovery catchup
    /// SEEDED FROM CLONED LOCAL STATE can hit when the local LCL is wrong
    /// relative to a *published, canonical* archive header (#3282). Such a
    /// divergence is **self-healable**: re-deriving canonical state from the
    /// archive (a `force_full` bucket-apply that ignores the local clone)
    /// rebuilds the correct state without any operator wipe — mirroring
    /// stellar-core's near-tip recovery via `CatchupWork::downloadApplyBuckets`
    /// (`stellar-core/src/catchup/CatchupWork.cpp:198`). It is distinct from a
    /// genuine bucket/verification corruption: detection is unchanged (all of
    /// these stay `is_fatal_catchup_failure()`); only the app-layer *response*
    /// branches on this classifier to attempt an archive rebuild BEFORE any
    /// terminal wipe.
    ///
    /// Recognized variants (the four knit/replay header-chain disagreements
    /// plus the apply-path ledger hash mismatch):
    /// - [`KnitLclHashMismatch`](HistoryError::KnitLclHashMismatch) — §11.2
    ///   case 3 (the #3282 observable).
    /// - [`KnitLclPredecessorHashMismatch`](HistoryError::KnitLclPredecessorHashMismatch)
    ///   — §11.2 case 2.
    /// - [`KnitCurrentLedgerPrevHashMismatch`](HistoryError::KnitCurrentLedgerPrevHashMismatch)
    ///   — §11.2 case 4.
    /// - [`ReplayHashMismatch`](HistoryError::ReplayHashMismatch) — replay
    ///   produced a header hash that disagrees with the archive.
    /// - [`Ledger(LedgerError::HashMismatch)`](HistoryError::Ledger) —
    ///   apply-path ledger-header hash mismatch.
    ///
    /// Deliberately EXCLUDED: bucket/bucket-list verification failures
    /// (`VerificationFailed`, `VerificationHashMismatch`) and chain-structure
    /// failures (`InvalidPreviousHash`, `InvalidSequence`, `CorruptHeader`,
    /// `KnitOvershot`, `FatalChainDisagreement`, `UnsupportedLedgerVersion`,
    /// `InvalidTxSetHash`) — those indicate problems an archive re-derivation
    /// would not (or could not) repair, so they remain terminal.
    pub fn is_local_vs_archive_divergence(&self) -> bool {
        matches!(
            self,
            HistoryError::KnitLclHashMismatch { .. }
                | HistoryError::KnitLclPredecessorHashMismatch { .. }
                | HistoryError::KnitCurrentLedgerPrevHashMismatch { .. }
                | HistoryError::ReplayHashMismatch { .. }
                | HistoryError::Ledger(henyey_ledger::LedgerError::HashMismatch { .. })
        )
    }

    /// Classify this error onto the ledger-chain verification taxonomy
    /// ([`LedgerVerifyStatus`], CATCHUP §3.9-1), or `None` if it is not a
    /// verification failure.
    ///
    /// This is a **read-only classifier** — it inspects an existing error and
    /// does not change any control flow or which error a path returns. The
    /// "verification-related" variant set it recognizes is:
    /// `UnsupportedLedgerVersion`, `InvalidPreviousHash`,
    /// `VerificationHashMismatch`, `FatalChainDisagreement`,
    /// `KnitLclHashMismatch`, `KnitLclPredecessorHashMismatch`,
    /// `KnitCurrentLedgerPrevHashMismatch`, `InvalidTxSetHash`,
    /// `ReplayHashMismatch`, `Ledger(LedgerError::HashMismatch)`,
    /// `KnitOvershot`, `CorruptHeader`, and `InvalidSequence`. All other
    /// variants — transient (network, IO, download, archive-not-ready) and
    /// non-verification — return `None`.
    ///
    /// Notes on faithfulness to stellar-core
    /// (`VerifyLedgerChainWork.cpp:296-311`):
    /// - [`LedgerVerifyStatus::Ok`] is **never** returned here — an error is
    ///   never "OK". The success path is the absence of a `HistoryError`.
    /// - [`LedgerVerifyStatus::ErrMissingEntries`] has no henyey producer and
    ///   is therefore never returned by this classifier (see its doc comment).
    /// - For [`HistoryError::InvalidSequence`] the overshot/undershot split is
    ///   a *field-based approximation*: `got > expected` ⇒ overshot,
    ///   `got < expected` ⇒ undershot (matching stellar-core, since
    ///   `verify.rs:318` constructs it as `expected = prev.seq + 1`,
    ///   `got = curr.seq`). The trust-anchor-range sites (`verify.rs:361,416`)
    ///   also construct `InvalidSequence` for what are semantically
    ///   range/missing-entries conditions; those are classified by the same
    ///   `got`-vs-`expected` rule, a known imprecision that is acceptable
    ///   because the live error returned is unchanged. `got == expected` is
    ///   unreachable (the variant is only built on mismatch); the equal arm
    ///   deterministically falls through to undershot rather than panicking.
    pub fn verify_status(&self) -> Option<LedgerVerifyStatus> {
        match self {
            HistoryError::UnsupportedLedgerVersion { .. } => {
                Some(LedgerVerifyStatus::ErrBadLedgerVersion)
            }
            HistoryError::InvalidPreviousHash { .. }
            | HistoryError::VerificationHashMismatch(_)
            | HistoryError::FatalChainDisagreement
            | HistoryError::KnitLclHashMismatch { .. }
            | HistoryError::KnitLclPredecessorHashMismatch { .. }
            | HistoryError::KnitCurrentLedgerPrevHashMismatch { .. }
            | HistoryError::InvalidTxSetHash { .. }
            | HistoryError::ReplayHashMismatch { .. }
            | HistoryError::Ledger(henyey_ledger::LedgerError::HashMismatch { .. }) => {
                Some(LedgerVerifyStatus::ErrBadHash)
            }
            HistoryError::KnitOvershot { .. } => Some(LedgerVerifyStatus::ErrOvershot),
            HistoryError::CorruptHeader { .. } => Some(LedgerVerifyStatus::ErrCorruptHeader),
            HistoryError::InvalidSequence { expected, got } => {
                if got > expected {
                    Some(LedgerVerifyStatus::ErrOvershot)
                } else {
                    // got < expected (undershot); got == expected is
                    // unreachable but deterministically classified here.
                    Some(LedgerVerifyStatus::ErrUndershot)
                }
            }
            _ => None,
        }
    }

    /// The covering checkpoint for a
    /// [`CheckpointNotYetPublished`](HistoryError::CheckpointNotYetPublished)
    /// error — `checkpoint_containing(target)`, i.e. the archive `currentLedger`
    /// value required for the target to be considered published — or `None` for
    /// any other variant.
    ///
    /// Derived from `target` rather than stored on the variant, keeping the
    /// crate-root-re-exported enum's public shape minimal (see #2950).
    pub fn covering_checkpoint(&self) -> Option<u32> {
        match self {
            HistoryError::CheckpointNotYetPublished { target, .. } => {
                Some(crate::checkpoint::checkpoint_containing(*target))
            }
            _ => None,
        }
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
        assert!(TxSetHashMismatchInfo::new(
            Hash256::ZERO,
            Hash256::ZERO,
            0,
            Hash256::ZERO,
            Hash256::ZERO,
            "classic",
        )
        .into_error(5)
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

        let err = HistoryError::ReplayHashMismatch {
            ledger: 42,
            expected: "abc".into(),
            actual: "def".into(),
        };
        assert!(
            err.is_fatal_catchup_failure(),
            "ReplayHashMismatch should be a fatal catchup failure"
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
        let err = TxSetHashMismatchInfo::new(
            Hash256::ZERO,
            Hash256::ZERO,
            0,
            Hash256::ZERO,
            Hash256::ZERO,
            "classic",
        )
        .into_error(5);
        assert!(err.is_hash_mismatch());

        // Positive: ReplayHashMismatch
        let err = HistoryError::ReplayHashMismatch {
            ledger: 100,
            expected: "abc".into(),
            actual: "def".into(),
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

    #[test]
    fn test_checkpoint_not_yet_published_is_transient() {
        // #2931: a knit/replay attempt against an archive checkpoint that has
        // not yet been published is a TRANSIENT archive-not-ready condition.
        // It must NOT be classified as a fatal catchup failure (which would
        // trigger a state-wipe) nor as a typed hash mismatch (which would
        // force a full bucket-apply reset).
        //
        // #2950: the covering checkpoint is now DERIVED from `target` via
        // `checkpoint_containing`, not stored as an independent field. The
        // #2962 "distinct value per field" premise (which set
        // `covering_checkpoint` independently of `target` to prove the message
        // rendered the stored field) is intentionally superseded: a covering
        // value distinct from `checkpoint_containing(target)` is no longer
        // representable. We instead use semantically valid distinct values for
        // the two intrinsic inputs — `target` mid-checkpoint and `has_current`
        // on a prior published frontier — so `target`, the derived covering
        // checkpoint, and `has_current` still render as three distinct numbers.
        // `target` sits mid-checkpoint; its covering checkpoint is 62845439.
        let target = 62845437;
        let covering = crate::checkpoint::checkpoint_containing(target);
        // Archive frontier still on the prior checkpoint (62845375), distinct
        // from both `target` and `covering`.
        let has_current = covering - crate::checkpoint::checkpoint_frequency();
        // Sanity: all three values are distinct so each assertion below
        // independently proves its own value is rendered.
        assert_ne!(target, covering);
        assert_ne!(target, has_current);
        assert_ne!(covering, has_current);

        let err = HistoryError::CheckpointNotYetPublished {
            target,
            has_current,
        };
        assert!(
            !err.is_fatal_catchup_failure(),
            "CheckpointNotYetPublished must be transient, not fatal"
        );
        assert!(
            !err.is_hash_mismatch(),
            "CheckpointNotYetPublished must not be treated as a hash mismatch"
        );
        // The accessor derives the covering checkpoint from `target`.
        assert_eq!(
            err.covering_checkpoint(),
            Some(covering),
            "covering_checkpoint() must derive checkpoint_containing(target)"
        );
        // A non-matching variant returns None.
        assert_eq!(
            HistoryError::FatalChainDisagreement.covering_checkpoint(),
            None,
            "covering_checkpoint() must be None for non-CheckpointNotYetPublished variants"
        );
        // Each distinct value must appear in the rendered message so on-call
        // debugging needn't recompute checkpoint math from the log line.
        let msg = err.to_string();
        assert!(
            msg.contains(&target.to_string()),
            "message must include the target ledger value: {msg}"
        );
        assert!(
            msg.contains(&covering.to_string()),
            "message must include the derived covering checkpoint value: {msg}"
        );
        assert!(
            msg.contains(&has_current.to_string()),
            "message must still include the archive HAS currentLedger: {msg}"
        );
    }

    #[test]
    fn test_verify_hash_mismatch_info_new_unlogged_and_into() {
        let expected = Hash256::ZERO;
        let actual = Hash256::from([0xAB; 32]);
        let info = VerifyHashMismatchInfo::new_unlogged(
            VerifyHashKind::Bucket,
            Some(42),
            expected,
            actual,
        );

        assert_eq!(info.kind(), VerifyHashKind::Bucket);
        assert_eq!(info.ledger(), Some(42));
        assert_eq!(info.expected(), expected);
        assert_eq!(info.actual(), actual);

        let err: HistoryError = info.into();
        match &err {
            HistoryError::VerificationHashMismatch(boxed) => {
                assert_eq!(boxed.kind(), VerifyHashKind::Bucket);
                assert_eq!(boxed.ledger(), Some(42));
                assert_eq!(boxed.expected(), expected);
                assert_eq!(boxed.actual(), actual);
            }
            other => panic!("expected VerificationHashMismatch, got: {other:?}"),
        }
    }

    #[test]
    fn test_verification_hash_mismatch_is_fatal() {
        for kind in [
            VerifyHashKind::Bucket,
            VerifyHashKind::BucketList,
            VerifyHashKind::LedgerHeaderEntry,
            VerifyHashKind::TxResultSet,
            VerifyHashKind::TrustedHeader,
            VerifyHashKind::BottomAnchor,
            VerifyHashKind::Lcl,
        ] {
            let err: HistoryError = VerifyHashMismatchInfo::new_unlogged(
                kind,
                Some(42),
                Hash256::ZERO,
                Hash256::from([0xAB; 32]),
            )
            .into();
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
            VerifyHashKind::BottomAnchor,
            VerifyHashKind::Lcl,
        ] {
            let err: HistoryError = VerifyHashMismatchInfo::new_unlogged(
                kind,
                Some(42),
                Hash256::ZERO,
                Hash256::from([0xAB; 32]),
            )
            .into();
            assert!(
                err.is_hash_mismatch(),
                "VerificationHashMismatch({kind}) should be recognized as a hash mismatch"
            );
        }
    }

    #[test]
    fn test_tx_set_hash_mismatch_info_helpers() {
        let expected = Hash256::from([1u8; 32]);
        let actual = Hash256::from([2u8; 32]);
        let prev = Hash256::from([3u8; 32]);
        let tx_prev = Hash256::from([4u8; 32]);

        let info =
            TxSetHashMismatchInfo::new(expected, actual, 21, prev, tx_prev, "generalized_v1");
        assert_eq!(info.expected, expected);
        assert_eq!(info.actual, actual);
        assert_eq!(info.header_ledger_version, 21);
        assert_eq!(info.header_prev_hash, prev);
        assert_eq!(info.tx_set_prev_hash, tx_prev);
        assert_eq!(info.tx_set_format, "generalized_v1");

        let err = info.into_error(42);
        match &err {
            HistoryError::InvalidTxSetHash { ledger, info } => {
                assert_eq!(*ledger, 42);
                assert_eq!(info.expected, expected);
                assert_eq!(info.actual, actual);
                assert_eq!(info.header_ledger_version, 21);
                assert_eq!(info.header_prev_hash, prev);
                assert_eq!(info.tx_set_prev_hash, tx_prev);
                assert_eq!(info.tx_set_format, "generalized_v1");
            }
            other => panic!("expected InvalidTxSetHash, got: {other:?}"),
        }
    }

    #[test]
    fn test_verify_hash_mismatch_display_with_ledger() {
        let info = VerifyHashMismatchInfo::new_unlogged(
            VerifyHashKind::BucketList,
            Some(42),
            Hash256::ZERO,
            Hash256::from([0xAB; 32]),
        );
        let msg = info.to_string();
        assert!(msg.contains("bucket list hash mismatch at ledger 42"));
        assert!(msg.contains("expected"));
        assert!(msg.contains("actual"));
    }

    #[test]
    fn test_verify_hash_mismatch_display_without_ledger() {
        let info = VerifyHashMismatchInfo::new_unlogged(
            VerifyHashKind::Bucket,
            None,
            Hash256::ZERO,
            Hash256::from([0xAB; 32]),
        );
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
        assert_eq!(VerifyHashKind::BottomAnchor.to_string(), "bottom anchor");
        assert_eq!(VerifyHashKind::Lcl.to_string(), "LCL");
    }

    #[test]
    fn test_replay_hash_mismatch_fields_and_display() {
        let err = HistoryError::ReplayHashMismatch {
            ledger: 42,
            expected: "abc123".into(),
            actual: "def456".into(),
        };

        // Verify field access via pattern matching
        if let HistoryError::ReplayHashMismatch {
            ledger,
            expected,
            actual,
        } = &err
        {
            assert_eq!(*ledger, 42);
            assert_eq!(expected, "abc123");
            assert_eq!(actual, "def456");
        } else {
            panic!("Expected ReplayHashMismatch variant");
        }

        // Verify Display includes all structured fields
        let display = err.to_string();
        assert!(
            display.contains("42"),
            "Display should include ledger sequence"
        );
        assert!(
            display.contains("abc123"),
            "Display should include expected hash"
        );
        assert!(
            display.contains("def456"),
            "Display should include actual hash"
        );
    }

    #[test]
    fn test_log_and_new_emits_structured_tracing() {
        use crate::tracing_test_support::capture_events;

        let expected = Hash256::ZERO;
        let actual = Hash256::from([0xAB; 32]);

        // log_and_new should emit a tracing event.
        let events = capture_events(|| {
            let _ = VerifyHashMismatchInfo::log_and_new(
                VerifyHashKind::Bucket,
                Some(7),
                expected,
                actual,
            );
        });

        assert_eq!(events.len(), 1, "log_and_new should emit exactly one event");
        let field_names: Vec<&str> = events[0].fields.iter().map(|(k, _)| k.as_str()).collect();
        assert!(
            field_names.contains(&"kind"),
            "event should contain 'kind' field"
        );
        assert!(
            field_names.contains(&"ledger_seq"),
            "event should contain 'ledger_seq' field"
        );
        assert!(
            field_names.contains(&"expected_hash"),
            "event should contain 'expected_hash' field"
        );
        assert!(
            field_names.contains(&"actual_hash"),
            "event should contain 'actual_hash' field"
        );

        // new_unlogged should NOT emit any tracing event.
        let events = capture_events(|| {
            let _ = VerifyHashMismatchInfo::new_unlogged(
                VerifyHashKind::Bucket,
                Some(7),
                expected,
                actual,
            );
        });

        assert!(
            events.is_empty(),
            "new_unlogged should not emit any tracing events"
        );
    }

    // ---- LedgerVerifyStatus taxonomy + classifier (CATCHUP §3.9-1, #3036) ----

    #[test]
    fn test_verify_status_maps_verification_variants() {
        // ErrBadLedgerVersion
        assert_eq!(
            HistoryError::UnsupportedLedgerVersion {
                ledger: 10,
                version: 99,
                min: 20,
                max: 23,
            }
            .verify_status(),
            Some(LedgerVerifyStatus::ErrBadLedgerVersion),
        );

        // ErrBadHash — a representative hash-class variant.
        assert_eq!(
            HistoryError::InvalidPreviousHash { ledger: 5 }.verify_status(),
            Some(LedgerVerifyStatus::ErrBadHash),
        );
        let mismatch: HistoryError = VerifyHashMismatchInfo::new_unlogged(
            VerifyHashKind::BucketList,
            Some(7),
            Hash256::ZERO,
            Hash256::from([0xAB; 32]),
        )
        .into();
        assert_eq!(
            mismatch.verify_status(),
            Some(LedgerVerifyStatus::ErrBadHash)
        );
        assert_eq!(
            HistoryError::Ledger(henyey_ledger::LedgerError::HashMismatch {
                expected: "abc".into(),
                actual: "def".into(),
            })
            .verify_status(),
            Some(LedgerVerifyStatus::ErrBadHash),
        );

        // ErrOvershot via KnitOvershot.
        assert_eq!(
            HistoryError::KnitOvershot {
                entry_seq: 12,
                lcl_seq: 10,
            }
            .verify_status(),
            Some(LedgerVerifyStatus::ErrOvershot),
        );

        // ErrCorruptHeader.
        assert_eq!(
            HistoryError::CorruptHeader {
                ledger: 100,
                detail: "bad XDR".into(),
            }
            .verify_status(),
            Some(LedgerVerifyStatus::ErrCorruptHeader),
        );
    }

    #[test]
    fn test_verify_status_invalid_sequence_overshot_vs_undershot() {
        // got > expected => overshot.
        assert_eq!(
            HistoryError::InvalidSequence {
                expected: 5,
                got: 7,
            }
            .verify_status(),
            Some(LedgerVerifyStatus::ErrOvershot),
        );
        // got < expected => undershot.
        assert_eq!(
            HistoryError::InvalidSequence {
                expected: 7,
                got: 5,
            }
            .verify_status(),
            Some(LedgerVerifyStatus::ErrUndershot),
        );
    }

    #[test]
    fn test_verify_status_none_for_transient() {
        assert_eq!(
            HistoryError::ArchiveUnreachable("timeout".into()).verify_status(),
            None,
        );
        assert_eq!(
            HistoryError::DownloadFailed("404".into()).verify_status(),
            None,
        );
        assert_eq!(
            HistoryError::Io(std::io::Error::other("disk")).verify_status(),
            None,
        );
        assert_eq!(HistoryError::NotFound("x".into()).verify_status(), None);
        assert_eq!(
            HistoryError::CheckpointNotYetPublished {
                target: 100,
                has_current: 50,
            }
            .verify_status(),
            None,
        );
    }

    #[test]
    fn test_ledger_verify_status_display() {
        assert_eq!(LedgerVerifyStatus::Ok.to_string(), "VERIFY_STATUS_OK");
        assert_eq!(
            LedgerVerifyStatus::ErrBadHash.to_string(),
            "VERIFY_STATUS_ERR_BAD_HASH"
        );
        assert_eq!(
            LedgerVerifyStatus::ErrBadLedgerVersion.to_string(),
            "VERIFY_STATUS_ERR_BAD_LEDGER_VERSION"
        );
        assert_eq!(
            LedgerVerifyStatus::ErrOvershot.to_string(),
            "VERIFY_STATUS_ERR_OVERSHOT"
        );
        assert_eq!(
            LedgerVerifyStatus::ErrUndershot.to_string(),
            "VERIFY_STATUS_ERR_UNDERSHOT"
        );
        assert_eq!(
            LedgerVerifyStatus::ErrMissingEntries.to_string(),
            "VERIFY_STATUS_ERR_MISSING_ENTRIES"
        );
        assert_eq!(
            LedgerVerifyStatus::ErrCorruptHeader.to_string(),
            "VERIFY_STATUS_ERR_CORRUPT_HEADER"
        );
    }

    #[test]
    fn test_verify_status_err_missing_entries_taxonomy() {
        // ErrMissingEntries is a taxonomy-only variant (no distinct henyey
        // producer today); assert it exists and renders correctly.
        let s = LedgerVerifyStatus::ErrMissingEntries;
        assert_eq!(s, LedgerVerifyStatus::ErrMissingEntries);
        assert_eq!(s.to_string(), "VERIFY_STATUS_ERR_MISSING_ENTRIES");
    }
}
