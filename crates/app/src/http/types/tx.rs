//! Types for transaction submission endpoints.

use serde::{Deserialize, Serialize};

/// Request for submitting a transaction via POST.
#[derive(Deserialize)]
pub struct SubmitTxRequest {
    /// Base64-encoded XDR transaction envelope.
    pub tx: String,
}

/// Transaction submission status, matching stellar-core's status strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum TxStatus {
    /// Transaction was accepted into the pending queue.
    Pending,
    /// Transaction is a duplicate of one already in the queue.
    Duplicate,
    /// Transaction failed validation.
    Error,
    /// Queue is full or source account is busy; retry later.
    TryAgainLater,
    /// Transaction contains a filtered operation type.
    Filtered,
}

/// Response for transaction submission.
#[derive(Serialize)]
pub struct SubmitTxResponse {
    /// Transaction submission status.
    pub status: TxStatus,
    /// Transaction hash (hex-encoded).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hash: Option<String>,
    /// Error detail string. For `ERROR` status, this is the error code name
    /// (e.g. "txBadSeq"). In the compat layer (Part B), this will be base64
    /// XDR `TransactionResult`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}
